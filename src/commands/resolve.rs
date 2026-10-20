//! Pick versions and build lock entries; settle dependencies.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

use jiff::Timestamp;

use crate::bail;
use crate::deps;
use crate::error::{Error, MANIFEST, Result};
use crate::github;
use crate::lock::{Lock, LockedSkill, Package};
use crate::manifest::{self, GitHub, Manifest, Repository, SkillReq};
use crate::registry::{self, Candidate, Registry};
use crate::skill;
use crate::ui;
use crate::version::{self, Constraint};

use super::Ctx;
use super::drift::{self, Reconciled};
use super::fetch::{self, Fetched};

impl Ctx {
    /// `locked`: the entry's lock entry before this command, if any.
    pub(super) fn resolve(
        &self,
        m: &Manifest,
        req: &SkillReq,
        locked: Option<&Package>,
        prepared: &mut HashMap<String, Fetched>,
    ) -> Result<Package> {
        if req.github.is_some() {
            m.check_github(&req.name)?;
            let c = github::resolve(self.http()?, req, m.policy.min_release_age, Timestamp::now())?;
            return self.lock_candidate(req, &github::repository(&req.name), &c, None, prepared);
        }
        let (repo, cands) = find(&Registry::new(self.http()?), m, req, locked)?;
        let c = registry::select(&cands, &req.name, &req.constraint, m.min_release_age(&repo), Timestamp::now())?;
        self.lock_candidate(req, &repo, &c, None, prepared)
    }

    /// The locked version again, with the entry's current selection and rename, when it still
    /// satisfies the constraint and the source and GitHub mode are unchanged; else `None`.
    fn relock(
        &self,
        m: &Manifest,
        req: &SkillReq,
        p: Option<&Package>,
        prepared: &mut HashMap<String, Fetched>,
    ) -> Result<Option<Package>> {
        let Some(p) = p.filter(|p| !drift::source_changed(m, req, p)) else { return Ok(None) };
        if let Some(why) = m.refuses(&p.repository) {
            bail!(MANIFEST, "{}: {why}", p.name);
        }
        let fits = || {
            let c = Constraint::parse(&req.constraint).ok();
            c.zip(version::Version::parse(&p.version)).is_some_and(|(c, v)| c.allows(&v))
        };
        let c = match req.github {
            Some(g) => {
                let same_ref = g == GitHub::Ref && p.constraint == req.constraint;
                // A lock without `ref` (0.1.2) kept the mode as long as the constraint is unchanged, as in `drift`.
                let same_mode = p.github.map_or(p.constraint == req.constraint, |l| l == g);
                if !same_mode || !(same_ref || g == GitHub::Tags && fits()) {
                    return Ok(None);
                }
                Candidate {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    parsed: version::Version::parse(&p.version).unwrap_or_else(version::Version::unversioned),
                    time: p.released,
                    dist: p.dist.clone(),
                    dist_shasum: None,
                    skill_paths: Vec::new(),
                }
            }
            None => {
                let Some(repo) = drift::locked_repository(m, req, Some(p)).filter(|_| fits()) else { return Ok(None) };
                let found =
                    Registry::new(self.http()?).versions(repo, &req.name)?.and_then(|cs| cs.into_iter().find(|c| c.version == p.version));
                let Some(c) = found else { return Ok(None) };
                Candidate { dist: p.dist.clone(), time: p.released, dist_shasum: None, ..c }
            }
        };
        let repo = Repository { name: String::new(), url: p.repository.clone(), ..Default::default() };
        self.lock_candidate(req, &repo, &c, Some(&p.dist_sha256), prepared).map(Some)
    }

    pub(super) fn lock_candidate(
        &self,
        req: &SkillReq,
        repo: &Repository,
        c: &Candidate,
        expected: Option<&str>,
        prepared: &mut HashMap<String, Fetched>,
    ) -> Result<Package> {
        let (sha, f) = self.fetch(&c.name, &c.dist, expected, c.dist_shasum.as_deref())?;
        // GitHub repositories declare no skill paths; any SKILL.md counts.
        let discovered = c.skill_paths.is_empty();
        let paths = if discovered { github::discover(&f.entries) } else { c.skill_paths.clone() };
        let mut found: Vec<(String, String, String)> = Vec::new();
        for p in &paths {
            let p = p.trim_start_matches("./").trim_end_matches('/');
            let rel = Path::new(p);
            if p.is_empty() || p.contains('\\') || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
                bail!(MANIFEST, "{}: invalid skill path {p:?} in extra.ai-agent-skill", c.name);
            }
            let (dir, md) = if p.ends_with(".md") {
                (p.rsplit_once('/').map_or("", |(d, _)| d).to_string(), p.to_string())
            } else {
                (p.to_string(), format!("{p}/{}", skill::SKILL_MD))
            };
            let text = std::fs::read_to_string(f.dir.path().join(&md)).map_err(|_| match f.skipped.iter().find(|x| x.path == md) {
                Some(x) => Error::new(crate::error::IO, format!("{}: {}", c.name, x.why)),
                None => Error::new(MANIFEST, format!("{} {}: {md} is missing from the package", c.name, c.version)),
            })?;
            let name = skill::frontmatter(&text).remove("name").filter(|n| skill::valid_name(n));
            // Repositories written for other tools often use a display name; their directory is the id.
            let name = if discovered {
                let base = dir.rsplit('/').next().filter(|d| !d.is_empty()).unwrap_or_else(|| c.name.rsplit('/').next().unwrap_or(""));
                match name.or_else(|| skill::valid_name(base).then(|| base.to_string())) {
                    Some(n) => n,
                    None => {
                        ui::warn(format!("{}: skipping {md}: no valid skill name ([a-z0-9][a-z0-9-]*)", c.name));
                        continue;
                    }
                }
            } else {
                name.ok_or_else(|| {
                    Error::new(MANIFEST, format!("{} {}: {md} has no frontmatter name matching [a-z0-9][a-z0-9-]*", c.name, c.version))
                })?
            };
            found.push((name, dir, md));
        }
        for k in req.rename.keys() {
            if !found.iter().any(|(n, ..)| n == k) {
                let have: Vec<&str> = found.iter().map(|(n, ..)| n.as_str()).collect();
                bail!(MANIFEST, "{}: rename: no skill {k:?} in {} (it has: {})", req.name, c.version, have.join(", "));
            }
            if req.skills.as_ref().is_some_and(|sel| !sel.contains(k)) {
                bail!(MANIFEST, "{}: rename: skill {k:?} is not in skills = [...]", req.name);
            }
        }
        if let Some(sel) = &req.skills {
            for s in sel {
                if !found.iter().any(|(n, ..)| n == s) {
                    let have: Vec<&str> = found.iter().map(|(n, ..)| n.as_str()).collect();
                    bail!(MANIFEST, "{}: no skill {s:?} in {} (it has: {})", req.name, c.version, have.join(", "));
                }
            }
            found.retain(|(n, ..)| sel.contains(n));
        }
        if found.is_empty() {
            bail!(MANIFEST, "{} {}: no skills found", c.name, c.version);
        }
        let mut skills: Vec<LockedSkill> = Vec::new();
        let mut requires: BTreeMap<String, String> = BTreeMap::new();
        for (from, dir, md) in found {
            deps::collect(&mut requires, &fetch::skill_md_of(&f.dir.path().join(&dir)))
                .map_err(|e| Error::new(MANIFEST, format!("{} {}: {md}: {e}", c.name, c.version)))?;
            let to = req.rename.get(&from);
            let name = to.unwrap_or(&from).clone();
            if let Some(other) = skills.iter().find(|s| s.name == name) {
                bail!(MANIFEST, "{}: skill name {name:?} appears twice in the package ({} and {dir})", c.name, other.path);
            }
            let mut files = f.skill_files(&c.name, &from, &dir)?;
            if to.is_some() {
                files = f.renamed_files(files, &dir, &name)?.0;
            }
            let tree_sha256 = skill::tree_hash(&files);
            skills.push(LockedSkill { name, from: to.map(|_| from), path: dir, tree_sha256 });
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        prepared.insert(c.name.clone(), f);
        Ok(Package {
            name: c.name.clone(),
            constraint: req.constraint.clone(),
            github: req.github,
            version: c.version.clone(),
            repository: repo.url.clone(),
            dist: c.dist.clone(),
            dist_sha256: sha,
            released: c.time,
            selected: req.skills.clone().map(|mut v| {
                v.sort();
                v
            }),
            requires,
            skills,
        })
    }
}

pub(super) fn required_by(w: &deps::Wanted, name: &str) -> String {
    let Some(by) = w.by.get(name) else { return String::new() };
    format!("\n  required by: {}", by.iter().map(|(r, c)| format!("{r} ({c})")).collect::<Vec<_>>().join(", "))
}

/// A requirer's new version can require more, so resolving repeats until nothing drifts.
const MAX_ROUNDS: usize = 50;

/// Resolves until the lock matches the manifest plus every dependency, and drops what nobody
/// wants. `skip`: drifted manifest entries this command leaves alone. Returns a line per new
/// dependency, printed once the command has saved.
pub(super) fn settle_deps(
    ctx: &Ctx,
    m: &Manifest,
    lock: &mut Lock,
    prepared: &mut HashMap<String, Fetched>,
    newcomers: &mut BTreeSet<String>,
    skip: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut report = Vec::new();
    for _ in 0..MAX_ROUNDS {
        let w = deps::wanted(m, lock)?;
        let Reconciled { needs, stale, .. } = drift::reconcile(m, &w.reqs, lock);
        lock.packages.retain(|p| !stale.contains(&p.name) || skip.contains(&p.name));
        let needs: Vec<SkillReq> = needs.into_iter().filter(|r| !skip.contains(&r.name)).collect();
        if needs.is_empty() {
            return Ok(report);
        }
        for req in &needs {
            let current = lock.get(&req.name).cloned();
            let p = match ctx.relock(m, req, current.as_ref(), prepared)? {
                Some(p) => p,
                None => ctx.resolve(m, req, current.as_ref(), prepared).map_err(|e| e.map_msg(|m| m + &required_by(&w, &req.name)))?,
            };
            if current.is_none() && m.skill(&req.name).is_none() {
                let by: Vec<&str> = w.by.get(&req.name).into_iter().flatten().map(|(r, _)| r.as_str()).collect();
                report.push(format!("added {} {} (dependency of {})", p.name, p.version, by.join(", ")));
            }
            newcomers.insert(req.name.clone());
            lock.upsert(p);
        }
    }
    bail!(MANIFEST, "dependencies do not settle after {MAX_ROUNDS} rounds; check for requirements that keep changing each other")
}

pub(super) fn find(reg: &Registry, m: &Manifest, req: &SkillReq, locked: Option<&Package>) -> Result<(Repository, Vec<Candidate>)> {
    // A blocked repository fails here; searching the others could pick a namesake elsewhere.
    let blocked = |r: &Repository| m.refuses(&r.url).map(|why| Error::new(MANIFEST, format!("{}: {why}", req.name)));
    let Some(repo) = drift::locked_repository(m, req, locked) else {
        if let Some(e) = req.repository.as_deref().and_then(|n| m.repository(n)).and_then(blocked) {
            return Err(e);
        }
        return reg.find(&m.allowed_repositories(), &req.name, req.repository.as_deref());
    };
    if let Some(e) = blocked(repo) {
        return Err(e);
    }
    match reg.versions(repo, &req.name)? {
        Some(c) if !c.is_empty() => Ok((repo.clone(), c)),
        Some(_) => bail!(MANIFEST, "{} in repository {} has no installable ai-agent-skill versions", req.name, repo.name),
        None => {
            let url = url::Url::parse(&repo.url).map_or_else(|_| repo.name.clone(), |u| crate::http::redact(&u));
            bail!(
                MANIFEST,
                "{}: not found in its locked repository {url}; if it moved on purpose, set repository = \"…\" in {}",
                req.name,
                manifest::FILE
            )
        }
    }
}
