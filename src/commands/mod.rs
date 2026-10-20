mod conflicts;
mod disk;
mod drift;
mod fetch;
mod materialize;
mod resolve;

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use jiff::Timestamp;

use crate::archive::Limits;
use crate::bail;
use crate::deps;
use crate::error::{Context, Error, MANIFEST, Result, USAGE};
use crate::github;
use crate::http::Http;
use crate::lock::Lock;
use crate::manifest::{self, GitHub, Manifest, Repository, SkillReq};
use crate::project::{self, LinkState, STORE};
use crate::registry::{self, Candidate, Registry};
use crate::scope::Scope;
use crate::skill;
use crate::ui;
use crate::version::{self, Constraint};

use disk::{hand_written, link_plan, locked_names, managed_paths, not_ours, shadows, tree_on_disk};
use drift::{drift, drifted, fill_new_keys, reconcile};
use fetch::Fetched;
use resolve::{find, required_by, settle_deps};

pub struct Ctx {
    pub s: Scope,
    pub verbose: bool,
    pub limits: Limits,
    /// stdin and stderr are terminals and CI is unset: conflicts may prompt for a rename.
    pub interactive: bool,
    http: OnceCell<Http>,
}

impl Ctx {
    pub fn new(s: Scope, verbose: bool, limits: Limits, interactive: bool) -> Ctx {
        Ctx { s, verbose, limits, interactive, http: OnceCell::new() }
    }

    fn http(&self) -> Result<&Http> {
        if let Some(h) = self.http.get() {
            return Ok(h);
        }
        let h = Http::new(&self.s.config, self.verbose)?;
        Ok(self.http.get_or_init(|| h))
    }

    fn manifest_or_default(&self) -> Result<Manifest> {
        if self.s.config.join(manifest::FILE).exists() {
            Manifest::load(&self.s.config, self.s.global)
        } else {
            manifest::layered(manifest::template(self.s.global), self.s.global)
        }
    }
}

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

pub fn install(ctx: &Ctx, frozen: bool) -> Result<()> {
    let mut m = Manifest::load(&ctx.s.config, ctx.s.global)?;
    let mut lock = Lock::load(&ctx.s.config)?;
    let old = lock.clone();
    let (_, _, why) = reconcile(&m, &deps::wanted(&m, &lock)?.reqs, &lock);
    let filled = !frozen && fill_new_keys(&m, &mut lock);
    let mut prepared = HashMap::new();
    if !why.is_empty() {
        if frozen {
            return Err(Error::new(
                MANIFEST,
                format!("skills.lock is out of date with skills.toml (frozen install):\n  {}", why.join("\n  ")),
            )
            .hint("run skillcask install locally and commit skills.lock"));
        }
        let mut newcomers = BTreeSet::new();
        let report = settle_deps(ctx, &m, &mut lock, &mut prepared, &mut newcomers, &BTreeSet::new())?;
        m = settle_and_save(ctx, m, &mut lock, &old, &prepared, &newcomers)?;
        for r in report {
            say(&r);
        }
    }
    ctx.materialize(&m, &lock, &old, prepared)?;
    if filled && why.is_empty() {
        lock.save(&ctx.s.config)?;
    }
    Ok(())
}

pub fn update(ctx: &Ctx, names: &[String]) -> Result<()> {
    let m = Manifest::load(&ctx.s.config, ctx.s.global)?;
    let mut lock = Lock::load(&ctx.s.config)?;
    let old = lock.clone();
    let w = deps::wanted(&m, &old)?;
    for n in names {
        if !w.reqs.iter().any(|r| r.name == *n) {
            bail!(MANIFEST, "{n} is not in {} and no locked package requires it", manifest::FILE);
        }
    }
    let mut prepared = HashMap::new();
    let mut newcomers = BTreeSet::new();
    let mut report = Vec::new();
    let skip: BTreeSet<String> =
        drifted(&m, &old).into_iter().filter(|n| m.skill(n).is_some() && !names.contains(n) && old.get(n).is_some()).collect();
    // Entries not locked yet are resolved too, so the lock is complete afterwards.
    for req in w.reqs.iter().filter(|r| names.is_empty() || names.contains(&r.name) || old.get(&r.name).is_none()) {
        newcomers.insert(req.name.clone());
        let new = ctx.resolve(&m, req, old.get(&req.name), &mut prepared)?;
        match old.get(&req.name) {
            Some(o) if o.version == new.version && o.dist_sha256 == new.dist_sha256 => {}
            Some(o) => report.push(format!("updated {} {} -> {}", new.name, o.version, new.version)),
            None => report.push(format!("locked {} {}", new.name, new.version)),
        }
        lock.upsert(new);
    }
    report.extend(settle_deps(ctx, &m, &mut lock, &mut prepared, &mut newcomers, &skip)?);
    for n in &skip {
        let req = deps::wanted(&m, &lock)?.reqs.into_iter().find(|r| r.name == *n).expect("skip holds manifest entries");
        if let Some(why) = drift(&m, &req, lock.get(n)) {
            crate::ui::warn(format!("{n}: {why}; `skillcask install` updates its lock entry"));
        }
    }
    let m = settle_and_save(ctx, m, &mut lock, &old, &prepared, &newcomers)?;
    for r in report {
        say(&r);
    }
    ctx.materialize(&m, &lock, &old, prepared)
}

/// Prompts for renames, then saves skills.toml and skills.lock only if no conflict is left.
fn settle_and_save(
    ctx: &Ctx,
    m: Manifest,
    lock: &mut Lock,
    old: &Lock,
    prepared: &HashMap<String, Fetched>,
    newcomers: &BTreeSet<String>,
) -> Result<Manifest> {
    let changed = ctx.settle_conflicts(&m, lock, old, prepared, newcomers, false)?;
    ctx.preflight(&m, lock, old)?;
    for r in &changed {
        manifest::write_skill(&ctx.s.config, ctx.s.global, r)?;
    }
    lock.save(&ctx.s.config)?;
    if changed.is_empty() { Ok(m) } else { Manifest::load(&ctx.s.config, ctx.s.global) }
}

pub fn outdated(ctx: &Ctx) -> Result<()> {
    let m = Manifest::load(&ctx.s.config, ctx.s.global)?;
    let lock = Lock::load(&ctx.s.config)?;
    let reg = Registry::new(ctx.http()?);
    let now = Timestamp::now();
    let mut rows = vec![["package", "locked", "within constraint", "latest", "note"].map(String::from)];
    for req in &m.skills {
        let locked = lock.get(&req.name).map_or("-".into(), |p| p.version.clone());
        if req.github.is_some() {
            m.check_github(&req.name)?;
            let (within, latest) = github::newest(ctx.http()?, req)?;
            let dash = |v: Option<String>| v.unwrap_or_else(|| "-".into());
            rows.push([req.name.clone(), locked, dash(within), dash(latest), String::new()]);
            continue;
        }
        let (repo, cands) = reg.find(&m.allowed_repositories(), &req.name, req.repository.as_deref())?;
        let age = m.min_release_age(&repo);
        let c = Constraint::parse(&req.constraint).map_err(|e| Error::new(MANIFEST, e))?;
        let within = registry::newest(&cands, Some(&c));
        let note = match within.and_then(|w| w.time.map(|t| (w, t))) {
            Some((w, t)) if now.checked_sub(age).ok().is_none_or(|c| t > c) => {
                let eligible = t.checked_add(age).unwrap_or(Timestamp::MAX);
                format!("{} eligible {}", w.version, eligible.round(jiff::Unit::Second).unwrap_or(eligible))
            }
            _ => String::new(),
        };
        let v = |c: Option<&Candidate>| c.map_or("-".to_string(), |c| c.version.clone());
        rows.push([req.name.clone(), locked, v(within), v(registry::newest(&cands, None)), note]);
    }
    ui::table(&rows, |_, _| None);
    Ok(())
}

pub fn status(ctx: &Ctx) -> Result<()> {
    let m = Manifest::load(&ctx.s.config, ctx.s.global)?;
    let lock = Lock::load(&ctx.s.config)?;
    let mut rows = vec![["package", "version", "skill", "state"].map(String::from)];
    let mut bad = 0;
    let mut row = |rows: &mut Vec<[String; 4]>, p: &str, v: &str, s: &str, state: &str| {
        if !healthy(state) {
            bad += 1;
        }
        // An agent pays for every row; it only needs the ones to act on.
        if !(ui::agent() && healthy(state)) {
            rows.push([p, v, s, state].map(String::from));
        }
    };
    let w = deps::wanted(&m, &lock)?;
    for req in &w.reqs {
        let label = match w.by.get(&req.name).filter(|_| m.skill(&req.name).is_none()) {
            Some(by) => format!("{} (via {})", req.name, by.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(", ")),
            None => req.name.clone(),
        };
        let p = lock.get(&req.name);
        if let Some(why) = drift(&m, req, p) {
            row(&mut rows, &label, p.map_or("-", |p| &p.version), "-", &why);
            continue;
        }
        let Some(p) = p else { continue };
        for sk in &p.skills {
            let state = match tree_on_disk(&ctx.s, &sk.name, &[&sk.tree_sha256]) {
                None => "not installed",
                Some(h) if h != sk.tree_sha256 => "modified",
                Some(_) => "ok",
            };
            row(&mut rows, &label, &p.version, &sk.name, state);
        }
    }
    let mut providers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (p, sk) in lock.skills() {
        providers.entry(&sk.name).or_default().push(&p.name);
    }
    for (n, ps) in providers.iter().filter(|(_, ps)| ps.len() > 1) {
        row(&mut rows, "-", "-", n, &format!("skill {n:?} is provided by both {}", ps.join(" and ")));
    }
    for p in lock.packages.iter().filter(|p| !w.reqs.iter().any(|r| r.name == p.name)) {
        row(&mut rows, &p.name, &p.version, "-", "not in skills.toml");
    }
    let locals = project::local_names(&ctx.s, &m.local)?;
    for h in hand_written(&ctx.s, &lock, &project::block_managed(&ctx.s)?, &locals)? {
        row(&mut rows, "-", "-", &h, "hand-written");
    }
    for l in &locals {
        let state = if project::link_state(&ctx.s, STORE, &m.local, l) == LinkState::Ok { "project" } else { "not linked" };
        row(&mut rows, "-", "-", l, state);
    }
    let skills = rows.iter().skip(1).filter(|r| r[3] == "ok").count();
    if rows.len() > 1 {
        ui::table(&rows, |i, c| match (i, healthy(c)) {
            (3, true) => Some(ui::Color::Green),
            (3, false) => Some(ui::Color::Red),
            _ => None,
        });
    }
    // An agent learns the policy's origin from the error a blocked source raises.
    for (k, (v, from)) in m.policy.effective.iter().filter(|_| !ui::agent()) {
        println!("{}", ui::out(ui::Color::Dim, format!("policy.{k} = {v} ({from})")));
    }
    if bad > 0 {
        return Err(Error::new(MANIFEST, format!("{bad} problem(s)")).hint("run skillcask install"));
    }
    if ui::agent() {
        println!("status: ok");
    } else {
        println!("{} {skills} skill(s) match skills.lock", ui::out(ui::Color::Green, "✓"));
    }
    Ok(())
}

pub fn doctor(ctx: &Ctx, fix: bool) -> Result<()> {
    let s = &ctx.s;
    let m = Manifest::load(&s.config, s.global)?;
    let lock = Lock::load(&s.config)?;
    for p in m.targets.iter().map(String::as_str).chain([STORE, ".gitignore"]) {
        project::no_symlink(s, p)?;
    }
    let locked = locked_names(&lock)?;
    let locals = project::local_names(s, &m.local)?;
    let old_managed = project::block_managed(s)?;
    let hand = hand_written(s, &lock, &old_managed, &locals)?;
    let mut open = Vec::new();
    let mut fixed = Vec::new();

    let mut copies = BTreeSet::new();
    for (at, source, n) in link_plan(&m, &locked, &hand, &locals) {
        let state = project::link_state(s, &at, source, &n);
        let what = format!("{}: link {}", s.show(&format!("{at}/{n}")), format!("{state:?}").to_lowercase());
        if state != LinkState::Foreign && not_ours(s, &m, &at, &n, &state, &old_managed) {
            open.push(format!("{} exists and was not created by skillcask; move it away", s.show(&format!("{at}/{n}"))));
        } else if state != LinkState::Ok {
            if fix && s.root.join(source).join(&n).is_dir() && state != LinkState::Foreign {
                project::link(s, &at, source, &n)?;
                fixed.push(what);
            } else {
                open.push(what);
            }
        }
        let p = s.root.join(&at).join(&n);
        let is_copy = p.symlink_metadata().is_ok_and(|m| m.is_dir()) && p.join(project::COPY_MARKER).is_file();
        if !locked.contains_key(&n) && project::link_state(s, &at, source, &n) == LinkState::Ok && is_copy {
            copies.insert(format!("/{at}/{n}"));
        }
    }

    let managed: BTreeSet<String> = locked.keys().cloned().collect();
    let tracked = project::git_tracked(s, &managed_paths(&managed, &m.targets));
    if !tracked.is_empty() {
        let what = format!("managed paths committed to git: {}", tracked.join(", "));
        if fix {
            project::git_untrack(s, &tracked)?;
            fixed.push(what);
        } else {
            open.push(what);
        }
    }

    let block_file = project::block_file(s);
    let lines = project::block_lines(&managed, &m.targets, &copies);
    if project::read_block(s)? != lines {
        let what = format!("{block_file}: stale managed block");
        if fix {
            project::write_block(s, &lines)?;
            fixed.push(what);
        } else {
            open.push(what);
        }
    }

    for (n, (p, sk)) in &locked {
        match tree_on_disk(s, n, &[&sk.tree_sha256]) {
            None => open.push(format!("{}: not installed ({}); run `skillcask install`", s.show(&format!("{STORE}/{n}")), p.name)),
            Some(h) if h != sk.tree_sha256 => {
                open.push(format!("{}: modified ({}); run `skillcask install` to restore", s.show(&format!("{STORE}/{n}")), p.name));
            }
            Some(_) => {}
        }
    }

    let warnings = shadows(s, &m, locked.keys().chain(&hand).chain(&locals));
    // An agent gets the same items with a word instead of a symbol and no blank lines.
    let section = |items: &[String], mark: &str, word: &str, c: ui::Color| {
        for i in items {
            if ui::agent() {
                println!("{word}: {i}");
            } else {
                println!("  {} {i}", ui::out(c, mark));
            }
        }
    };
    section(&open, "✗", "problem", ui::Color::Red);
    section(&fixed, "✓", "fixed", ui::Color::Green);
    section(&warnings, "!", "warning", ui::Color::Yellow);
    if !open.is_empty() {
        let e = Error::new(MANIFEST, format!("{} problem(s)", open.len()));
        return Err(match (fix, s.global) {
            (true, _) => e,
            (false, true) => e.hint(format!("skillcask --global doctor --fix repairs links and {block_file}")),
            (false, false) => e.hint("skillcask doctor --fix repairs links and the gitignore block"),
        });
    }
    let summary = if fixed.is_empty() { "no problems found".to_string() } else { format!("fixed {} problem(s)", fixed.len()) };
    if ui::agent() {
        println!("doctor: {summary}");
    } else {
        println!("{} {summary}", ui::out(ui::Color::Green, "✓"));
    }
    Ok(())
}

pub fn search(ctx: &Ctx, query: &str) -> Result<()> {
    let m = ctx.manifest_or_default()?;
    let results = Registry::new(ctx.http()?).search(&m.allowed_repositories(), query)?;
    if results.is_empty() {
        println!("no skill packages match {query:?}");
    }
    let results: Vec<(String, String)> = results.iter().map(|(n, d)| (ui::printable(n), ui::printable(d))).collect();
    let width = results.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0);
    for (n, d) in results {
        if ui::agent() {
            println!("{n}\t{d}");
        } else {
            println!("{n:width$}  {}", ui::out(ui::Color::Dim, d));
        }
    }
    Ok(())
}

/// A report line such as `updated a/b 1.0.0 -> 1.1.0`, its first word as the verb.
fn say(line: &str) {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    ui::done(verb, rest);
}

/// `status` states that are not problems.
fn healthy(state: &str) -> bool {
    matches!(state, "ok" | "hand-written" | "project")
}
