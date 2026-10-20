use std::collections::{BTreeSet, HashMap};

use jiff::Timestamp;

use crate::bail;
use crate::deps;
use crate::error::{Context, Error, MANIFEST, Result, USAGE};
use crate::github;
use crate::lock::Lock;
use crate::manifest::{self, GitHub, Manifest, Repository, SkillReq};
use crate::registry::{self, Candidate, Registry};
use crate::skill;
use crate::version::{self, Constraint};

use super::drift::drifted;
use super::resolve::{find, required_by, settle_deps};
use super::{Ctx, say};

/// The constraint `add` writes when none is given.
fn default_constraint(c: &Candidate, allow_ranges: bool) -> String {
    let v = c.version.trim_start_matches(['v', 'V']);
    if allow_ranges {
        c.parsed.major_minor()
    } else if version::is_exact(v) {
        v.to_string()
    } else {
        let [a, b, p, _] = c.parsed.parts;
        format!("{a}.{b}.{p}")
    }
}

pub fn init(ctx: &Ctx) -> Result<()> {
    let path = ctx.s.config.join(manifest::FILE);
    let shown = if ctx.s.global { path.display().to_string() } else { manifest::FILE.into() };
    if path.exists() {
        bail!(MANIFEST, "{shown} already exists");
    }
    std::fs::create_dir_all(&ctx.s.config).ctx(ctx.s.config.display())?;
    std::fs::write(&path, manifest::template(ctx.s.global)).ctx(&shown)?;
    crate::ui::done("created", &shown);
    Ok(())
}

pub fn add(ctx: &Ctx, spec: &str, repository: Option<String>) -> Result<()> {
    if let Some(rest) = spec.strip_prefix("github:") {
        if repository.is_some() {
            bail!(USAGE, "--repository does not apply to github: sources");
        }
        return add_github(ctx, rest);
    }
    let (name, constraint) = match spec.split_once('@') {
        Some((n, c)) => (n.to_string(), Some(c.to_string())),
        None => (spec.to_string(), None),
    };
    if !manifest::valid_package_name(&name) {
        bail!(MANIFEST, "{name:?} is not a valid package name (vendor/name, lowercase)");
    }
    let m = ctx.manifest_or_default()?;
    if let Some(r) = &repository
        && m.repository(r).is_none()
    {
        bail!(MANIFEST, "unknown repository {r:?}; declare it under [repositories] in {}", manifest::FILE);
    }
    let existing = m.skill(&name).cloned();
    // Before the constraint check: `@main` is a valid ref for a GitHub entry.
    if existing.as_ref().is_some_and(|e| e.github.is_some()) && repository.is_none() {
        return add_github(ctx, spec);
    }
    if let Some(c) = &constraint {
        Constraint::parse(c).map_err(|e| Error::new(MANIFEST, e))?;
        if !m.policy.allow_ranges && !version::is_exact(c) {
            bail!(MANIFEST, "policy.allow-ranges = false requires an exact version, got {c:?}");
        }
    }
    let repository = repository.or_else(|| existing.as_ref().and_then(|e| e.repository.clone()));
    let constraint = constraint.or_else(|| existing.as_ref().map(|e| e.constraint.clone()));

    let lock = Lock::load(&ctx.s.config)?;
    let probe = SkillReq { name: name.clone(), repository: repository.clone(), ..Default::default() };
    let (repo, cands) = find(&Registry::new(ctx.http()?), &m, &probe, lock.get(&name))?;
    let c = registry::select(&cands, &name, constraint.as_deref().unwrap_or("*"), m.min_release_age(&repo), Timestamp::now())?;
    let constraint = constraint.unwrap_or_else(|| default_constraint(&c, m.policy.allow_ranges));
    let (skills, rename) = existing.map(|e| (e.skills, e.rename)).unwrap_or_default();
    let req = SkillReq { name, constraint, repository, skills, rename, ..Default::default() };
    finish_add(ctx, &m, &req, &repo, &c, None)
}

/// `owner/repo[/skill][@constraint-or-ref]`, the form skills.sh shows as `owner/repo/skill`.
fn add_github(ctx: &Ctx, spec: &str) -> Result<()> {
    let (path, at) = match spec.split_once('@') {
        Some((p, a)) => (p, Some(a.to_string())),
        None => (spec, None),
    };
    let mut parts = path.splitn(3, '/');
    let name = format!("{}/{}", parts.next().unwrap_or(""), parts.next().unwrap_or("")).to_lowercase();
    if !manifest::valid_package_name(&name) {
        bail!(MANIFEST, "{spec:?}: expected github:owner/repo[/skill][@version-or-ref]");
    }
    let skill = parts.next().map(String::from);
    let must_have = skill.clone();
    if let Some(s) = &skill
        && !skill::valid_name(s)
    {
        bail!(MANIFEST, "{s:?} is not a valid skill name ([a-z0-9][a-z0-9-]*)");
    }
    let m = ctx.manifest_or_default()?;
    m.check_github(&name)?;
    let existing = m.skill(&name).filter(|e| e.github.is_some()).cloned();
    let skills = match (skill, existing.as_ref().map(|e| e.skills.clone())) {
        (None, have) => have.flatten(),
        // Already installs every skill of the repository.
        (Some(_), Some(None)) => None,
        (Some(s), Some(Some(mut have))) => {
            if !have.contains(&s) {
                have.push(s);
            }
            Some(have)
        }
        (Some(s), None) => Some(vec![s]),
    };
    let (given, mode) = match (at, &existing) {
        (Some(a), _) => {
            let mode = if Constraint::parse(&a).is_ok() { GitHub::Tags } else { GitHub::Ref };
            (Some(a), mode)
        }
        (None, Some(e)) => (Some(e.constraint.clone()), e.github.expect("filtered to GitHub entries")),
        (None, None) if github::tags(ctx.http()?, &name)?.is_empty() => (None, GitHub::Ref),
        (None, None) => (None, GitHub::Tags),
    };
    let any = if mode == GitHub::Tags { "*" } else { manifest::DEFAULT_REF };
    let rename = existing.map(|e| e.rename).unwrap_or_default();
    let mut req = SkillReq {
        name,
        constraint: given.clone().unwrap_or_else(|| any.into()),
        skills,
        github: Some(mode),
        rename,
        ..Default::default()
    };
    if given.is_some() {
        manifest::check_constraint(&req, &m.policy).map_err(|e| Error::new(MANIFEST, format!("{}: {e}", req.name)))?;
    }
    let c = github::resolve(ctx.http()?, &req, m.policy.min_release_age, Timestamp::now())?;
    if given.is_none() {
        req.constraint = match (mode, m.policy.allow_ranges) {
            (GitHub::Tags, allow_ranges) => default_constraint(&c, allow_ranges),
            (GitHub::Ref, true) => manifest::DEFAULT_REF.into(),
            (GitHub::Ref, false) => c.dist.reference.clone().expect("GitHub candidates carry their sha"),
        };
    }
    finish_add(ctx, &m, &req, &github::repository(&req.name), &c, must_have.as_deref())
}

/// `must_have`: a skill named on the command line, checked also when the entry selects all.
fn finish_add(ctx: &Ctx, m: &Manifest, req: &SkillReq, repo: &Repository, c: &Candidate, must_have: Option<&str>) -> Result<()> {
    let mut prepared = HashMap::new();
    let pkg = ctx.lock_candidate(req, repo, c, None, &mut prepared)?;
    if let Some(s) = must_have {
        let have: Vec<&str> = pkg.skills.iter().map(|k| k.from.as_deref().unwrap_or(&k.name)).collect();
        if !have.contains(&s) {
            bail!(MANIFEST, "{}: no skill {s:?} in {} (it has: {})", req.name, c.version, have.join(", "));
        }
    }
    let mut lock = Lock::load(&ctx.s.config)?;
    let old = lock.clone();
    lock.upsert(pkg);
    // skills.toml is written only after the checks pass; the prompt still needs to see the entry.
    let mut view = m.clone();
    view.skills.retain(|r| r.name != req.name);
    view.skills.push(req.clone());
    let mut skip = drifted(m, &old);
    skip.remove(&req.name);
    let mut newcomers = BTreeSet::from([req.name.clone()]);
    let report = settle_deps(ctx, &view, &mut lock, &mut prepared, &mut newcomers, &skip)?;
    let changed = ctx.settle_conflicts(&view, &mut lock, &old, &prepared, &newcomers, true)?;
    let req = &changed.into_iter().find(|r| r.name == req.name).unwrap_or_else(|| req.clone());
    ctx.preflight(&view, &lock, &old)?;

    manifest::write_skill(&ctx.s.config, ctx.s.global, req)?;
    lock.save(&ctx.s.config)?;
    crate::ui::done("added", format!("{} = {:?} ({})", req.name, req.constraint, c.version));
    for r in report {
        say(&r);
    }
    ctx.materialize(&Manifest::load(&ctx.s.config, ctx.s.global)?, &lock, &old, prepared)
}

pub fn remove(ctx: &Ctx, names: &[String]) -> Result<()> {
    let m = Manifest::load(&ctx.s.config, ctx.s.global)?;
    let mut lock = Lock::load(&ctx.s.config)?;
    let old = lock.clone();
    let w = deps::wanted(&m, &lock)?;
    for n in names {
        if m.skill(n).is_none() && lock.get(n).is_none() {
            bail!(MANIFEST, "{n} is not in {} or {}", manifest::FILE, crate::lock::FILE);
        }
        if m.skill(n).is_none() && w.by.contains_key(n) {
            bail!(MANIFEST, "{n} is a dependency; remove what requires it{}", required_by(&w, n));
        }
    }
    let mut view = m.clone();
    view.skills.retain(|r| !names.contains(&r.name));
    let mut prepared = HashMap::new();
    let mut skip = drifted(&m, &old);
    skip.retain(|n| !names.contains(n));
    settle_deps(ctx, &view, &mut lock, &mut prepared, &mut BTreeSet::new(), &skip)?;
    let still = deps::wanted(&view, &lock)?;
    for n in names.iter().filter(|n| lock.get(n).is_some()) {
        crate::ui::done("kept", format!("{n} as a dependency{}", required_by(&still, n)));
    }
    ctx.preflight(&view, &lock, &old)?;
    manifest::remove_skills(&ctx.s.config, ctx.s.global, names)?;
    lock.save(&ctx.s.config)?;
    ctx.materialize(&Manifest::load(&ctx.s.config, ctx.s.global)?, &lock, &old, prepared)
}
