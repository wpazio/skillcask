use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

use jiff::{SignedDuration, Timestamp};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::archive::{self, Limits};
use crate::bail;
use crate::deps;
use crate::error::{Context, Error, HASH, MANIFEST, NETWORK, Result};
use crate::github;
use crate::http::Http;
use crate::lock::{Dist, Lock, LockedSkill, Package};
use crate::manifest::{self, GitHub, Manifest, Repository, SkillReq};
use crate::project::{self, LinkState, STORE};
use crate::registry::{self, Candidate, Registry};
use crate::scope::Scope;
use crate::skill::{self, FileEntry};
use crate::ui;
use crate::version::{self, Constraint};

pub struct Ctx {
    pub s: Scope,
    pub verbose: bool,
    pub limits: Limits,
    /// stdin and stderr are terminals and CI is unset: conflicts may prompt for a rename.
    pub interactive: bool,
    /// The command installs nothing: dists unpack in the system temp dir, not in the store.
    pub read_only: bool,
    http: OnceCell<Http>,
}

/// A downloaded, verified and unpacked dist, kept so `add`/`update` do not download twice.
struct Fetched {
    dir: tempfile::TempDir,
    entries: Vec<FileEntry>,
    links: Vec<archive::Link>,
    skipped: Vec<archive::Skipped>,
}

impl Fetched {
    fn skill_files(&self, package: &str, skill: &str, dir: &str) -> Result<Vec<FileEntry>> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{}/", dir.trim_end_matches('/')) };
        if let Some(x) = self.skipped.iter().find(|x| x.path.starts_with(&prefix)) {
            bail!(crate::error::IO, "{package}: {}", x.why);
        }
        if let Some(l) = archive::bad_link_in(&self.links, dir) {
            bail!(
                crate::error::IO,
                "archive: {package}: skill {skill} contains a symlink {} that does not point to a file inside the skill",
                l.path
            );
        }
        Ok(archive::subtree(&self.entries, dir))
    }

    /// The skill's files as installed under `to`, and the renamed SKILL.md if it has one. The
    /// extracted dir is left alone: nested skills and skills sharing a dir read the same files.
    fn renamed_files(&self, mut files: Vec<FileEntry>, dir: &str, to: &str) -> Result<(Vec<FileEntry>, Option<String>)> {
        let Some(e) = files.iter_mut().find(|e| e.path == skill::SKILL_MD) else {
            return Ok((files, None));
        };
        let new = skill::rename_frontmatter(&std::fs::read_to_string(self.dir.path().join(dir).join(skill::SKILL_MD))?, to);
        *e = FileEntry::new(e.path.clone(), e.exec, new.as_bytes());
        Ok((files, Some(new)))
    }
}

impl Ctx {
    pub fn new(s: Scope, verbose: bool, limits: Limits, interactive: bool) -> Ctx {
        Ctx { s, verbose, limits, interactive, read_only: false, http: OnceCell::new() }
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

    fn fetch(&self, name: &str, dist: &Dist, expected: Option<&str>, shasum: Option<&str>) -> Result<(String, Fetched)> {
        let bytes = self
            .http()?
            .get(&dist.url, self.limits.download)?
            .ok_or_else(|| Error::new(NETWORK, format!("{name}: dist archive not found (404)")))?;
        let sha = hex::encode(Sha256::digest(&bytes));
        if let Some(exp) = expected
            && exp != sha
        {
            bail!(HASH, "{name}: downloaded archive has sha256 {sha}, skills.lock expects {exp}");
        }
        if let Some(s) = shasum {
            let sha1 = hex::encode(Sha1::digest(&bytes));
            if !s.eq_ignore_ascii_case(&sha1) {
                bail!(HASH, "{name}: downloaded archive has sha1 {sha1}, the repository metadata says {s}");
            }
        }
        let dir = if self.read_only { tempfile::tempdir()? } else { project::temp_dir(&self.s)? };
        let x = archive::extract(&bytes, dir.path(), &self.limits).ctx(name)?;
        Ok((sha, Fetched { dir, entries: x.files, links: x.links, skipped: x.skipped }))
    }

    /// `locked`: the entry's lock entry before this command, if any.
    fn resolve(&self, m: &Manifest, req: &SkillReq, locked: Option<&Package>, prepared: &mut HashMap<String, Fetched>) -> Result<Package> {
        let (repo, c) = self.candidate(m, req, locked)?;
        let p = self.lock_candidate(req, &repo, &c, None, prepared)?;
        // The same dist as the lock's was scanned when it was locked.
        if locked.is_none_or(|l| l.dist_sha256 != p.dist_sha256) {
            warn_invisible(&prepared[&p.name], &p)?;
        }
        Ok(p)
    }

    /// The version `update` locks for `req`: same repository, policy and cooldown.
    fn candidate(&self, m: &Manifest, req: &SkillReq, locked: Option<&Package>) -> Result<(Repository, Candidate)> {
        if req.github.is_some() {
            m.check_github(&req.name)?;
            let c = github::resolve(self.http()?, req, m.policy.min_release_age, Timestamp::now())?;
            return Ok((github::repository(&req.name), c));
        }
        let (repo, cands) = find(&Registry::new(self.http()?), m, req, locked)?;
        let c = registry::select(&cands, &req.name, &req.constraint, m.min_release_age(&repo), Timestamp::now())?;
        Ok((repo, c))
    }

    /// The newest version of `req`'s package matching `v`, without the cooldown: the user named it.
    fn candidate_at(&self, m: &Manifest, req: &SkillReq, locked: Option<&Package>, v: &str) -> Result<(Repository, Candidate)> {
        if req.github.is_some() {
            m.check_github(&req.name)?;
            let mode = if Constraint::parse(v).is_ok() { GitHub::Tags } else { GitHub::Ref };
            let r = SkillReq { constraint: v.to_string(), github: Some(mode), ..req.clone() };
            return Ok((github::repository(&req.name), github::resolve(self.http()?, &r, SignedDuration::ZERO, Timestamp::now())?));
        }
        let (repo, cands) = find(&Registry::new(self.http()?), m, req, locked)?;
        let c = registry::select(&cands, &req.name, v, SignedDuration::ZERO, Timestamp::now())?;
        Ok((repo, c))
    }

    /// Prints a unified diff from `old`'s skills, as installed (none for a package new to the
    /// lock), to `new`'s in `f`. `scan`: warn about invisible Unicode in the files that changed.
    fn show_diff(&self, old: Option<&Package>, new: &Package, f: &Fetched, scan: bool) -> Result<()> {
        let title = match old {
            Some(o) => format!("{} {} -> {}", new.name, o.version, new.version),
            None => format!("{} {} (new)", new.name, new.version),
        };
        println!("{}", ui::out(ui::Color::Cyan, skill::shown(&title)));
        let mut old_dist: Option<Fetched> = None;
        let mut before: BTreeMap<&str, Tree> = BTreeMap::new();
        for (old, sk) in old.iter().flat_map(|o| o.skills.iter().map(move |sk| (*o, sk))) {
            let tree = if installed_ok(&self.s, sk) {
                let dir = project::store_dir(&self.s, &sk.name);
                let mut files = skill::scan_dir(&dir)?;
                if !cfg!(unix) {
                    let modes = project::read_modes(&self.s);
                    for e in &mut files {
                        e.exec = modes.contains(&format!("{}/{}", sk.name, e.path));
                    }
                }
                read_tree(&dir, files, None)?
            } else {
                if old_dist.is_none() {
                    old_dist = Some(self.fetch(&old.name, &old.dist, Some(&old.dist_sha256), None)?.1);
                }
                installed_tree(old_dist.as_ref().expect("fetched above"), old, sk)?
            };
            before.insert(&sk.name, tree);
        }
        let mut after: BTreeMap<&str, Tree> = BTreeMap::new();
        for sk in &new.skills {
            after.insert(&sk.name, installed_tree(f, new, sk)?);
        }
        let none = Tree::new();
        let mut warnings = Vec::new();
        for n in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
            let (a, b) = (before.get(n).unwrap_or(&none), after.get(n).unwrap_or(&none));
            for path in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
                let (x, y) = (a.get(path), b.get(path));
                if x == y {
                    continue;
                }
                let label = format!("{n}/{path}");
                print_file_diff(&label, x, y);
                if let Some((_, bytes)) = y.filter(|_| scan) {
                    warnings.extend(skill::invisible_warnings(&label, bytes));
                }
            }
        }
        for w in warnings {
            ui::warn(format!("{}: {w}", new.name));
        }
        Ok(())
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
        let Some(p) = p.filter(|p| !source_changed(m, req, p)) else { return Ok(None) };
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
                locked_candidate(p)
            }
            None => {
                let Some(repo) = locked_repository(m, req, Some(p)).filter(|_| fits()) else { return Ok(None) };
                let found =
                    Registry::new(self.http()?).versions(repo, &req.name)?.and_then(|cs| cs.into_iter().find(|c| c.version == p.version));
                let Some(c) = found else { return Ok(None) };
                Candidate { dist: p.dist.clone(), time: p.released, dist_shasum: None, ..c }
            }
        };
        let repo = Repository { name: String::new(), url: p.repository.clone(), ..Default::default() };
        self.lock_candidate(req, &repo, &c, Some(&p.dist_sha256), prepared).map(Some)
    }

    fn lock_candidate(
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
                        crate::ui::warn(format!("{}: skipping {md}: no valid skill name ([a-z0-9][a-z0-9-]*)", c.name));
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
            deps::collect(&mut requires, &skill_md_of(&f.dir.path().join(&dir)))
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

    /// Conflicts that would stop `materialize`, checked before `add` writes skills.toml and skills.lock.
    /// `old`: the lock as it was before this command.
    fn preflight(&self, m: &Manifest, lock: &Lock, old: &Lock) -> Result<()> {
        for p in m.targets.iter().map(String::as_str).chain([STORE, ".gitignore"]) {
            project::no_symlink(&self.s, p)?;
        }
        // A committed lock must not bring in what the policy refuses.
        for p in &lock.packages {
            if let Some(why) = m.refuses(&p.repository) {
                bail!(MANIFEST, "{}: {why}", p.name);
            }
        }
        if let Some(c) = self.conflicts(m, lock, old, &BTreeSet::new())?.first() {
            let renames = m.skill(&c.package).map(|r| &r.rename);
            bail!(MANIFEST, "{}", hint(c, &self.taken_names(m, lock)?, renames));
        }
        // Hand-written and project skills have no package to rename; without this check they
        // would fail in the middle of `materialize`.
        let s = &self.s;
        let old_managed = project::block_managed(s)?;
        let locked = locked_names(lock)?;
        let locals = project::local_names(s, &m.local)?;
        let kept = kept(s, lock, old, &old_managed);
        let hand = hand_written(s, lock, &old_managed.difference(&kept).cloned().collect(), &locals)?;
        for (at, source, n) in link_plan(m, &locked, &hand, &locals) {
            let removed = old_managed.contains(&n) && !kept.contains(&n) && !locked.contains_key(&n);
            if !(at == STORE && removed) && not_ours(s, m, &at, &n, &project::link_state(s, &at, source, &n), &old_managed) {
                bail!(MANIFEST, "{} exists and was not created by skillcask; move it away", s.show(&format!("{at}/{n}")));
            }
        }
        Ok(())
    }

    /// Skills that cannot be installed under their name. In a clash between packages the one in
    /// `newcomers` is reported, so it is the one offered a rename.
    fn conflicts(&self, m: &Manifest, lock: &Lock, old: &Lock, newcomers: &BTreeSet<String>) -> Result<Vec<Conflict>> {
        let s = &self.s;
        let mut out = Vec::new();
        let conflict = |p: &Package, sk: &LockedSkill, msg: String| Conflict {
            package: p.name.clone(),
            upstream: sk.from.clone().unwrap_or_else(|| sk.name.clone()),
            msg,
            rename: true,
        };
        let mut all: Vec<(&Package, &LockedSkill)> = lock.skills().collect();
        all.sort_by_key(|(p, _)| newcomers.contains(&p.name));
        let mut locked: BTreeMap<&str, (&Package, &LockedSkill)> = BTreeMap::new();
        for (p, sk) in all {
            match locked.get(sk.name.as_str()) {
                Some((first, _)) => {
                    out.push(conflict(p, sk, format!("skill {:?} is provided by both {} and {}", sk.name, first.name, p.name)))
                }
                None => {
                    locked.insert(&sk.name, (p, sk));
                }
            }
        }
        let locals = project::local_names(s, &m.local)?;
        for (n, (p, sk)) in &locked {
            if locals.contains(*n) {
                out.push(conflict(p, sk, format!("skill {n:?} is both a project skill in {} and provided by {}", m.local, p.name)));
            }
        }
        let old_managed = project::block_managed(s)?;
        let stale = project::stale_local_links(s, &m.local, &locals)?;
        let owned = match project::installed_here(s)? {
            Some(names) => Owned::Names(names),
            None => {
                let mut hashes = project::lock_history_hashes(s);
                hashes.extend(lock.skills().chain(old.skills()).map(|(_, sk)| sk.tree_sha256.clone()));
                Owned::Hashes(hashes)
            }
        };
        for (n, (p, sk)) in &locked {
            let present = project::store_dir(s, n).symlink_metadata().is_ok();
            if present
                && !stale.iter().any(|x| x == n)
                && let Some((msg, rename)) = store_conflict(s, p, sk, old, &owned)
            {
                out.push(Conflict { rename, ..conflict(p, sk, msg) });
            }
        }
        for (n, (p, sk)) in &locked {
            for t in &m.targets {
                if not_ours(s, m, t, n, &project::link_state(s, t, STORE, n), &old_managed) {
                    out.push(conflict(
                        p,
                        sk,
                        format!("{} exists and was not created by skillcask; move it away", s.show(&format!("{t}/{n}"))),
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Names a rename must avoid: locked, project, in the store, or present in any target.
    fn taken_names(&self, m: &Manifest, lock: &Lock) -> Result<BTreeSet<String>> {
        let s = &self.s;
        let mut out: BTreeSet<String> = lock.skills().map(|(_, sk)| sk.name.clone()).collect();
        out.extend(project::local_names(s, &m.local)?);
        out.extend(project::store_names(s)?);
        for t in &m.targets {
            if let Ok(rd) = std::fs::read_dir(s.root.join(t)) {
                out.extend(rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()));
            }
        }
        Ok(out)
    }

    /// Installs `upstream` of `pkg` as `to`: rewrites its lock entry from the prepared download.
    fn rename_locked(&self, lock: &mut Lock, prepared: &HashMap<String, Fetched>, pkg: &str, upstream: &str, to: &str) -> Result<()> {
        let f = prepared.get(pkg).expect("newcomers are prepared");
        let p = lock.packages.iter_mut().find(|p| p.name == pkg).expect("conflicts name locked packages");
        let s = p.skills.iter_mut().find(|s| s.from.as_deref().unwrap_or(&s.name) == upstream).expect("conflicts name locked skills");
        let (files, _) = f.renamed_files(f.skill_files(pkg, upstream, &s.path)?, &s.path, to)?;
        s.tree_sha256 = skill::tree_hash(&files);
        s.from = Some(upstream.to_string());
        s.name = to.to_string();
        p.skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    /// Offers a rename for each conflict of a newly resolved package; returns the manifest entries
    /// that changed. Without a terminal, does nothing: `preflight` then fails with the hint.
    fn settle_conflicts(
        &self,
        m: &Manifest,
        lock: &mut Lock,
        old: &Lock,
        prepared: &HashMap<String, Fetched>,
        newcomers: &BTreeSet<String>,
        shadows_too: bool,
    ) -> Result<Vec<SkillReq>> {
        if !self.interactive {
            return Ok(Vec::new());
        }
        let mut changed: BTreeMap<String, SkillReq> = BTreeMap::new();
        let mut rename = |lock: &mut Lock, pkg: &str, upstream: &str, to: String| -> Result<()> {
            self.rename_locked(lock, prepared, pkg, upstream, &to)?;
            let req = changed.entry(pkg.to_string()).or_insert_with(|| m.skill(pkg).cloned().expect("newcomers are in the manifest"));
            req.rename.insert(upstream.to_string(), to);
            Ok(())
        };
        let mut last: Option<(String, String)> = None;
        loop {
            let taken = self.taken_names(m, lock)?;
            // A dependency has no manifest entry to hold `rename`; `preflight` names the fix.
            let offer = |c: &Conflict| c.rename && newcomers.contains(&c.package) && m.skill(&c.package).is_some();
            let Some(c) = self.conflicts(m, lock, old, newcomers)?.into_iter().find(offer) else {
                break;
            };
            // A rename to an untaken name clears the conflict; if it did not, asking again would loop.
            if last.as_ref().is_some_and(|(p, u)| *p == c.package && *u == c.upstream) {
                break;
            }
            let Some(to) = ask(&question(&c.msg, &c.package), suggest(&c.package, &c.upstream, &taken).as_deref(), &taken)? else {
                break;
            };
            rename(lock, &c.package, &c.upstream, to)?;
            last = Some((c.package, c.upstream));
        }
        if shadows_too {
            let news: Vec<(String, LockedSkill)> = lock
                .skills()
                .filter(|(p, _)| newcomers.contains(&p.name) && m.skill(&p.name).is_some())
                .map(|(p, s)| (p.name.clone(), s.clone()))
                .collect();
            for (p, s) in news {
                let Some(w) = shadows(&self.s, m, [&s.name]).into_iter().next() else { continue };
                let taken = self.taken_names(m, lock)?;
                let upstream = s.from.clone().unwrap_or(s.name.clone());
                let mut suggestion = suggest(&p, &upstream, &taken);
                while let Some(to) = ask(&question(&w, &p), suggestion.as_deref(), &taken)? {
                    if let Some(why) = shadows(&self.s, m, [&to]).into_iter().next() {
                        eprintln!("{why}");
                        suggestion = None;
                        continue;
                    }
                    rename(lock, &p, &upstream, to)?;
                    break;
                }
            }
        }
        Ok(changed.into_values().collect())
    }

    /// Makes disk match the lock: store, links, gitignore block, git index.
    fn materialize(&self, m: &Manifest, lock: &Lock, old: &Lock, mut prepared: HashMap<String, Fetched>) -> Result<()> {
        self.preflight(m, lock, old)?;
        let s = &self.s;
        project::clean_leftovers(s, &prepared.values().map(|f| f.dir.path()).collect::<Vec<_>>())?;
        let old_managed = project::block_managed(s)?;
        let old_block = project::read_block(s)?;
        let locked = locked_names(lock)?;
        let locals = project::local_names(s, &m.local)?;
        for n in project::stale_local_links(s, &m.local, &locals)? {
            project::unlink(s, STORE, &n)?;
        }
        let kept = kept(s, lock, old, &old_managed);
        let hand = hand_written(s, lock, &old_managed.difference(&kept).cloned().collect(), &locals)?;

        // `requires` is not covered by any hash; checked before anything is placed, it cannot make
        // a lock-only edit install a package skills.toml never asked for.
        for p in &lock.packages {
            let mut declared = BTreeMap::new();
            for sk in &p.skills {
                let text = if installed_ok(s, sk) {
                    skill_md_of(&project::store_dir(s, &sk.name))
                } else {
                    if !prepared.contains_key(&p.name) {
                        let f = self.fetch(&p.name, &p.dist, Some(&p.dist_sha256), None)?.1;
                        prepared.insert(p.name.clone(), f);
                    }
                    skill_md_of(&prepared[&p.name].dir.path().join(&sk.path))
                };
                deps::collect(&mut declared, &text).map_err(|e| Error::new(MANIFEST, format!("{} {}: {e}", p.name, p.version)))?;
            }
            if declared != p.requires {
                bail!(
                    HASH,
                    "{} {}: skills.lock lists requires {:?}, but its SKILL.md files declare {:?}",
                    p.name,
                    p.version,
                    p.requires,
                    declared
                );
            }
        }
        let mut modes = project::read_modes(s);
        for p in &lock.packages {
            let need: Vec<&LockedSkill> = p.skills.iter().filter(|sk| !installed_ok(s, sk)).collect();
            if need.is_empty() {
                continue;
            }
            let f = match prepared.remove(&p.name) {
                Some(f) => f,
                None => self.fetch(&p.name, &p.dist, Some(&p.dist_sha256), None)?.1,
            };
            for sk in need {
                let mut files = f.skill_files(&p.name, sk.from.as_ref().unwrap_or(&sk.name), &sk.path)?;
                let mut skill_md = None;
                if sk.from.is_some() {
                    (files, skill_md) = f.renamed_files(files, &sk.path, &sk.name)?;
                }
                let got = skill::tree_hash(&files);
                if got != sk.tree_sha256 {
                    bail!(
                        HASH,
                        "{} {}: skill {} has tree-sha256 {got}, skills.lock expects {}",
                        p.name,
                        p.version,
                        sk.name,
                        sk.tree_sha256
                    );
                }
                project::place(s, &sk.name, &f.dir.path().join(&sk.path))?;
                if let Some(md) = skill_md {
                    // Truncates in place, so the copied mode stays.
                    let shown = s.show(&format!("{STORE}/{}/{}", sk.name, skill::SKILL_MD));
                    std::fs::write(project::store_dir(s, &sk.name).join(skill::SKILL_MD), md).ctx(shown)?;
                }
                set_modes(&mut modes, &sk.name, &files);
                crate::ui::done("installed", format!("{} ({} {})", sk.name, p.name, p.version));
            }
        }

        for n in old_managed.iter().filter(|n| !locked.contains_key(*n)) {
            for t in &m.targets {
                project::unlink(s, t, n)?;
            }
            set_modes(&mut modes, n, &[]);
            if kept.contains(n) {
                crate::ui::warn(format!("kept {}: modified or not installed by skillcask", s.show(&format!("{STORE}/{n}"))));
                continue;
            }
            let dir = project::store_dir(s, n);
            if dir.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
                std::fs::remove_file(&dir).ctx(s.show(&format!("{STORE}/{n}")))?;
            } else if dir.is_dir() {
                std::fs::remove_dir_all(dir).ctx(s.show(&format!("{STORE}/{n}")))?;
            }
            crate::ui::done("removed", n);
        }
        project::write_modes(s, &modes)?;

        let plan = link_plan(m, &locked, &hand, &locals);
        if !s.global {
            // In home skillcask links only locked skills; other links there are the user's.
            let names: BTreeSet<&String> = plan.iter().map(|(_, _, n)| n).collect();
            for t in &m.targets {
                for e in std::fs::read_dir(s.root.join(t)).into_iter().flatten().flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if !names.contains(&n) && project::made_by_skillcask(s, t, STORE, &n, &old_block) {
                        project::unlink(s, t, &n)?;
                    }
                }
            }
        }
        let mut copies = BTreeSet::new();
        for (at, source, n) in plan {
            if project::link(s, &at, source, &n)? && !locked.contains_key(&n) {
                copies.insert(format!("/{at}/{n}"));
            }
        }
        let managed: BTreeSet<String> = locked.keys().cloned().collect();
        project::write_block(s, &project::block_lines(&managed, &m.targets, &copies))?;
        // Every locked skill now matches the lock: placed now, or found intact and adopted.
        project::write_installed_here(s, &managed)?;

        let tracked = project::git_tracked(s, &managed_paths(&managed, &m.targets));
        if !tracked.is_empty() {
            project::git_untrack(s, &tracked)?;
            crate::ui::warn(format!("removed managed paths from the git index (files kept): {}", tracked.join(", ")));
        }
        for w in shadows(s, m, locked.keys().chain(&hand).chain(&locals)) {
            crate::ui::warn(w);
        }
        Ok(())
    }
}

/// Warns about invisible Unicode in `p`'s skill files (D23).
fn warn_invisible(f: &Fetched, p: &Package) -> Result<()> {
    for sk in &p.skills {
        for e in f.skill_files(&p.name, sk.from.as_ref().unwrap_or(&sk.name), &sk.path)? {
            let path = if sk.path.is_empty() { e.path } else { format!("{}/{}", sk.path, e.path) };
            for w in skill::invisible_warnings(&path, &std::fs::read(f.dir.path().join(&path))?) {
                ui::warn(format!("{}: {w}", p.name));
            }
        }
    }
    Ok(())
}

/// A skill's files by NFC path: exec bit and content.
type Tree = BTreeMap<String, (bool, Vec<u8>)>;

fn read_tree(base: &Path, files: Vec<FileEntry>, rename: Option<&str>) -> Result<Tree> {
    let nfc = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    let mut out = Tree::new();
    for e in files {
        let mut bytes = std::fs::read(base.join(&e.path))?;
        if let Some(to) = rename.filter(|_| e.path == skill::SKILL_MD) {
            bytes = skill::rename_frontmatter(&String::from_utf8_lossy(&bytes), to).into_bytes();
        }
        out.insert(nfc.normalize(&e.path).into_owned(), (e.exec, bytes));
    }
    Ok(out)
}

/// `sk` of `p` from its unpacked dist, as `install` would place it.
fn installed_tree(f: &Fetched, p: &Package, sk: &LockedSkill) -> Result<Tree> {
    let files = f.skill_files(&p.name, sk.from.as_ref().unwrap_or(&sk.name), &sk.path)?;
    read_tree(&f.dir.path().join(&sk.path), files, sk.from.as_ref().map(|_| sk.name.as_str()))
}

/// `None` for binary content; a missing file is empty.
fn text(f: Option<&(bool, Vec<u8>)>) -> Option<&str> {
    match f {
        None => Some(""),
        Some((_, b)) => std::str::from_utf8(b).ok().filter(|t| !t.contains('\0')),
    }
}

/// One file of a unified diff; package text is shown escaped (`skill::shown`).
fn print_file_diff(label: &str, old: Option<&(bool, Vec<u8>)>, new: Option<&(bool, Vec<u8>)>) {
    let label = skill::shown(label);
    println!("--- {}", old.map_or("/dev/null".into(), |_| format!("a/{label}")));
    println!("+++ {}", new.map_or("/dev/null".into(), |_| format!("b/{label}")));
    let mode = |x: bool| if x { "100755" } else { "100644" };
    match (old, new) {
        (Some((x, _)), Some((y, _))) if x != y => println!("old mode {}\nnew mode {}", mode(*x), mode(*y)),
        (None, Some((true, _))) => println!("new mode {}", mode(true)),
        (Some((true, _)), None) => println!("old mode {}", mode(true)),
        _ => {}
    }
    let (Some(a), Some(b)) = (text(old), text(new)) else {
        if old.map(|f| &f.1) != new.map(|f| &f.1) {
            println!("Binary files differ");
        }
        return;
    };
    let Some(lines) = crate::diff::unified(a, b, 3) else {
        println!("Files differ (too large to show)");
        return;
    };
    // CRLF files would show <U+000D> on every line; only a change of line ends needs it.
    let crlf = |t: &str| t.matches('\n').count() == t.matches("\r\n").count();
    let strip_cr = crlf(a) && crlf(b);
    for l in lines {
        let shown = skill::shown(if strip_cr { l.strip_suffix('\r').unwrap_or(&l) } else { &l });
        match l.as_bytes().first() {
            Some(b'+') => println!("{}", ui::out(ui::Color::Green, shown)),
            Some(b'-') => println!("{}", ui::out(ui::Color::Red, shown)),
            Some(b'@') => println!("{}", ui::out(ui::Color::Cyan, shown)),
            _ => println!("{shown}"),
        }
    }
}

/// The locked version of a GitHub entry, as a candidate: its skills are discovered in the archive.
fn locked_candidate(p: &Package) -> Candidate {
    Candidate {
        name: p.name.clone(),
        version: p.version.clone(),
        parsed: version::Version::parse(&p.version).or_else(|| version::Version::parse("0.0.0")).expect("valid"),
        time: p.released,
        dist: p.dist.clone(),
        dist_shasum: None,
        skill_paths: Vec::new(),
        abandoned: None,
        description: None,
        license: Vec::new(),
    }
}

/// A skill that cannot be installed under its name.
struct Conflict {
    package: String,
    /// Name inside the package (`from`, else the install name): the key for `rename`.
    upstream: String,
    msg: String,
    /// Whether installing under another name solves it; not when the dir is a local edit.
    rename: bool,
}

/// `<vendor>-<skill>`, unless taken or not a valid name.
fn suggest(package: &str, upstream: &str, taken: &BTreeSet<String>) -> Option<String> {
    let vendor: String = package.split('/').next().unwrap_or("").chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let name = format!("{vendor}-{upstream}");
    (skill::valid_name(&name) && !taken.contains(&name)).then_some(name)
}

/// `renames`: the entry's current `rename`, shown merged so the line can replace it; `None` for a
/// dependency, which has no entry yet.
fn hint(c: &Conflict, taken: &BTreeSet<String>, renames: Option<&BTreeMap<String, String>>) -> String {
    if !c.rename {
        return c.msg.clone();
    }
    let to = suggest(&c.package, &c.upstream, taken).unwrap_or_else(|| "<new-name>".into());
    let mut all = renames.cloned().unwrap_or_default();
    all.insert(c.upstream.clone(), to);
    let pairs: Vec<String> = all.iter().map(|(k, v)| format!("{k} = \"{v}\"")).collect();
    let place = if renames.is_some() { "set in its entry in" } else { "it is a dependency; add an entry for it to" };
    format!("{}\n  to install it under another name, {place} {}:\n  rename = {{ {} }}", c.msg, manifest::FILE, pairs.join(", "))
}

fn question(msg: &str, package: &str) -> String {
    if msg.contains(package) { msg.to_string() } else { format!("{msg} ({package})") }
}

#[derive(Debug, PartialEq)]
enum Answer {
    Use(String),
    Decline,
    Retry(String),
}

fn answer(input: &str, suggestion: Option<&str>, taken: &BTreeSet<String>) -> Answer {
    match input.trim() {
        "" | "y" | "Y" | "yes" => match suggestion {
            Some(s) => Answer::Use(s.into()),
            None => Answer::Retry("type a name".into()),
        },
        "n" | "N" | "no" => Answer::Decline,
        n if !skill::valid_name(n) => Answer::Retry(format!("{n:?} is not a valid skill name ([a-z0-9][a-z0-9-]*)")),
        n if taken.contains(n) => Answer::Retry(format!("{n:?} is taken too")),
        n => Answer::Use(n.into()),
    }
}

/// Asks on stderr/stdin for an install name; None when declined or stdin is closed.
fn ask(question: &str, suggestion: Option<&str>, taken: &BTreeSet<String>) -> Result<Option<String>> {
    loop {
        match suggestion {
            Some(s) => eprint!("{question}\nInstall it as {s}? [Y/n/<other name>] "),
            None => eprint!("{question}\nInstall it under which name? (empty line: no) "),
        }
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            eprintln!();
            return Ok(None);
        }
        if suggestion.is_none() && line.trim().is_empty() {
            return Ok(None);
        }
        match answer(&line, suggestion, taken) {
            Answer::Use(n) => return Ok(Some(n)),
            Answer::Decline => return Ok(None),
            Answer::Retry(why) => eprintln!("{why}"),
        }
    }
}

/// Personal skills with the same name as project ones. Claude Code runs the personal one;
/// Codex lists both. Empty for the global scope.
fn shadows<'a>(s: &Scope, m: &Manifest, names: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let Some(home) = crate::scope::home().filter(|_| !s.global) else { return Vec::new() };
    let mut out = Vec::new();
    for n in names {
        for d in std::iter::once(STORE).chain(m.targets.iter().map(String::as_str)) {
            if home.join(d).join(n).symlink_metadata().is_ok() {
                out.push(if d == ".claude/skills" {
                    format!("{n}: ~/{d}/{n} shadows the project skill in Claude Code")
                } else {
                    format!("{n}: ~/{d}/{n} also exists; the agent may show both")
                });
            }
        }
    }
    out
}

fn locked_names(lock: &Lock) -> Result<BTreeMap<String, (&Package, &LockedSkill)>> {
    let mut out: BTreeMap<String, (&Package, &LockedSkill)> = BTreeMap::new();
    for (p, s) in lock.skills() {
        if let Some((other, _)) = out.insert(s.name.clone(), (p, s)) {
            bail!(MANIFEST, "skill {:?} is provided by both {} and {}", s.name, other.name, p.name);
        }
    }
    Ok(out)
}

/// A target entry skillcask must neither replace nor repair. A symlink pointing elsewhere is the
/// user's or another tool's, unless the block manages the name or it leads into the store or the
/// project skills.
fn not_ours(s: &Scope, m: &Manifest, at: &str, n: &str, state: &LinkState, old_managed: &BTreeSet<String>) -> bool {
    match state {
        LinkState::Foreign => true,
        LinkState::Wrong => {
            let path = s.root.join(at).join(n);
            path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink())
                && !old_managed.contains(n)
                && !project::points_into(s, &path, STORE)
                && (s.global || !project::points_into(s, &path, &m.local))
        }
        _ => false,
    }
}

fn hand_written(s: &Scope, lock: &Lock, old_managed: &BTreeSet<String>, locals: &BTreeSet<String>) -> Result<BTreeSet<String>> {
    if s.global {
        return Ok(BTreeSet::new());
    }
    let locked: BTreeSet<&str> = lock.skills().map(|(_, sk)| sk.name.as_str()).collect();
    Ok(project::store_names(s)?
        .into_iter()
        .filter(|n| !locked.contains(n.as_str()) && !old_managed.contains(n) && !locals.contains(n))
        .collect())
}

/// Every link as (directory, source directory, name): project skills into the store, then all
/// skills from the store into each agent target.
fn link_plan<'a>(
    m: &'a Manifest,
    locked: &BTreeMap<String, (&Package, &LockedSkill)>,
    hand: &BTreeSet<String>,
    locals: &BTreeSet<String>,
) -> Vec<(String, &'a str, String)> {
    let mut plan: Vec<(String, &str, String)> = locals.iter().map(|n| (STORE.to_string(), m.local.as_str(), n.clone())).collect();
    for n in locked.keys().chain(hand).chain(locals) {
        plan.extend(m.targets.iter().map(|t| (t.clone(), STORE, n.clone())));
    }
    plan
}

fn locked_skill<'a>(lock: &'a Lock, name: &str) -> Option<(&'a Package, &'a LockedSkill)> {
    lock.skills().find(|(_, sk)| sk.name == name)
}

/// What proves a store dir is skillcask's.
enum Owned {
    /// Names in the machine-local record (the block is committed, so a clone has it too).
    Names(BTreeSet<String>),
    /// No record yet (0.1.2 wrote none): a tree skills.lock ever locked, so nothing is lost.
    Hashes(BTreeSet<String>),
}

/// Why installing `sk` over its existing store dir would lose something skillcask did not install,
/// and whether a rename helps. A dir matching the new or the old lock entry is skillcask's. Any
/// other dir is a local edit, restored only when skillcask installed it on this machine and its
/// lock entry is unchanged.
fn store_conflict(s: &Scope, p: &Package, sk: &LockedSkill, old: &Lock, owned: &Owned) -> Option<(String, bool)> {
    let shown = s.show(&format!("{STORE}/{}", sk.name));
    let mut expect = vec![sk.tree_sha256.as_str()];
    let old_skill = locked_skill(old, &sk.name);
    expect.extend(old_skill.map(|(_, os)| os.tree_sha256.as_str()));
    if let Owned::Hashes(hs) = owned {
        expect.extend(hs.iter().map(String::as_str));
    }
    let h = tree_on_disk(s, &sk.name, &expect);
    if h.as_ref().is_some_and(|h| *h == sk.tree_sha256 || matches!(owned, Owned::Hashes(hs) if hs.contains(h))) {
        return None;
    }
    let (Some(h), Some((op, os))) = (h, old_skill) else {
        return Some((format!("{shown} exists and is not managed by skillcask, but {} provides a skill with that name", p.name), true));
    };
    let away = "move it away (or delete it), then run `skillcask install`";
    if h == os.tree_sha256 {
        None
    } else if !matches!(owned, Owned::Names(n) if n.contains(&sk.name)) {
        Some((format!("{shown} exists and was not installed by skillcask on this machine; {away}"), false))
    } else if op.version == p.version && os.tree_sha256 == sk.tree_sha256 {
        None
    } else {
        Some((format!("{shown} was modified after skillcask installed it; {away}"), false))
    }
}

/// Managed names no longer locked whose store dir is not what the old lock installed: modified,
/// or never installed by skillcask (the block is committed and shared). They are not deleted.
fn kept(s: &Scope, lock: &Lock, old: &Lock, old_managed: &BTreeSet<String>) -> BTreeSet<String> {
    old_managed
        .iter()
        .filter(|n| locked_skill(lock, n).is_none())
        .filter(|n| project::store_dir(s, n).symlink_metadata().is_ok_and(|m| !m.file_type().is_symlink()))
        .filter(|n| {
            let want = locked_skill(old, n).map(|(_, sk)| sk.tree_sha256.as_str());
            tree_on_disk(s, n, want.as_slice()).as_deref() != want
        })
        .cloned()
        .collect()
}

/// `<dir>/SKILL.md`, where `requires:` is read from both at lock and at install time; empty when
/// the skill names another file.
fn skill_md_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(skill::SKILL_MD)).unwrap_or_default()
}

fn installed_ok(s: &Scope, sk: &LockedSkill) -> bool {
    tree_on_disk(s, &sk.name, &[&sk.tree_sha256]).is_some_and(|h| h == sk.tree_sha256)
}

/// `expect`: hashes the caller compares with. HFS+ stores names decomposed, so with a non-ASCII
/// path the NFC and NFD spellings are tried too, and the one matching `expect` wins.
fn tree_on_disk(s: &Scope, name: &str, expect: &[&str]) -> Option<String> {
    let dir = project::store_dir(s, name);
    let mut entries = skill::scan_dir(&dir).ok().filter(|_| dir.is_dir())?;
    if !cfg!(unix) {
        let modes = project::read_modes(s);
        for e in &mut entries {
            e.exec = modes.contains(&format!("{name}/{}", e.path));
        }
    }
    let raw = skill::tree_hash(&entries);
    if expect.contains(&raw.as_str()) || entries.iter().all(|e| e.path.is_ascii()) {
        return Some(raw);
    }
    let nfc = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    let nfd = icu_normalizer::DecomposingNormalizerBorrowed::new_nfd();
    let spelled = |f: &dyn Fn(&str) -> String| {
        skill::tree_hash(&entries.iter().map(|e| FileEntry { path: f(&e.path), ..e.clone() }).collect::<Vec<_>>())
    };
    [spelled(&|p| nfc.normalize(p).into_owned()), spelled(&|p| nfd.normalize(p).into_owned())]
        .into_iter()
        .find(|h| expect.contains(&h.as_str()))
        .or(Some(raw))
}

fn set_modes(modes: &mut BTreeSet<String>, name: &str, files: &[FileEntry]) {
    let prefix = format!("{name}/");
    modes.retain(|l| !l.starts_with(&prefix));
    modes.extend(files.iter().filter(|f| f.exec).map(|f| format!("{prefix}{}", f.path)));
}

fn managed_paths(managed: &BTreeSet<String>, targets: &[String]) -> Vec<String> {
    managed.iter().flat_map(|n| std::iter::once(format!("{STORE}/{n}")).chain(targets.iter().map(move |t| format!("{t}/{n}")))).collect()
}

/// Manifest entries that need resolving, lock entries to drop, and why.
/// `reqs`: the effective requirements, `deps::wanted`.
fn reconcile(m: &Manifest, reqs: &[SkillReq], lock: &Lock) -> (Vec<SkillReq>, Vec<String>, Vec<String>) {
    let (mut needs, mut stale, mut why) = (Vec::new(), Vec::new(), Vec::new());
    for req in reqs {
        if let Some(r) = drift(m, req, lock.get(&req.name)) {
            why.push(format!("{}: {r}", req.name));
            needs.push(req.clone());
        }
    }
    for p in &lock.packages {
        if !reqs.iter().any(|r| r.name == p.name) {
            why.push(format!("{}: in skills.lock but neither in skills.toml nor required", p.name));
            stale.push(p.name.clone());
        }
    }
    (needs, stale, why)
}

/// Manifest entries that drift from `lock`, and lock entries nobody wants: commands that do not
/// own them leave them alone.
fn drifted(m: &Manifest, lock: &Lock) -> BTreeSet<String> {
    let Ok(w) = deps::wanted(m, lock) else { return BTreeSet::new() };
    let (needs, stale, _) = reconcile(m, &w.reqs, lock);
    needs.into_iter().map(|r| r.name).filter(|n| m.skill(n).is_some()).chain(stale).collect()
}

fn required_by(w: &deps::Wanted, name: &str) -> String {
    let Some(by) = w.by.get(name) else { return String::new() };
    format!("\n  required by: {}", by.iter().map(|(r, c)| format!("{r} ({c})")).collect::<Vec<_>>().join(", "))
}

/// A requirer's new version can require more, so resolving repeats until nothing drifts.
const MAX_ROUNDS: usize = 50;

/// Resolves until the lock matches the manifest plus every dependency, and drops what nobody
/// wants. `skip`: drifted manifest entries this command leaves alone. Returns a line per new
/// dependency, printed once the command has saved.
fn settle_deps(
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
        let (needs, stale, _) = reconcile(m, &w.reqs, lock);
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

/// Why the lock entry does not match the manifest entry. Keys a lock may lack (written by an
/// older version) count as matching: absent `selected` is all skills, absent `ref` any mode.
fn drift(m: &Manifest, req: &SkillReq, p: Option<&Package>) -> Option<String> {
    let Some(p) = p else { return Some("not in skills.lock".into()) };
    if p.constraint != req.constraint {
        return Some(format!("constraint changed from {:?} to {:?}", p.constraint, req.constraint));
    }
    if source_changed(m, req, p) {
        return Some("repository changed".into());
    }
    let mode = |g: GitHub| if g == GitHub::Ref { "ref" } else { "version" };
    if let (Some(g), Some(l)) = (req.github, p.github)
        && l != g
    {
        return Some(format!("changed from {} to {}", mode(l), mode(g)));
    }
    let upstream: BTreeSet<&String> = p.skills.iter().map(|s| s.from.as_ref().unwrap_or(&s.name)).collect();
    let selection_changed = match &req.skills {
        Some(sel) => sel.iter().collect::<BTreeSet<_>>() != upstream,
        None => p.selected.is_some(),
    };
    if selection_changed {
        return Some("skill selection changed".into());
    }
    let renamed: BTreeMap<&String, &String> = p.skills.iter().filter_map(|s| s.from.as_ref().map(|f| (f, &s.name))).collect();
    if renamed != req.rename.iter().collect() {
        return Some("rename changed".into());
    }
    None
}

/// The repository a locked entry must be resolved in, unless its source changed. Searching the
/// others would let a public package with the same name replace a private one.
fn locked_repository<'a>(m: &'a Manifest, req: &SkillReq, locked: Option<&Package>) -> Option<&'a Repository> {
    let p = locked.filter(|p| req.github.is_none() && !source_changed(m, req, p))?;
    m.repositories.iter().find(|r| r.url == p.repository)
}

fn find(reg: &Registry, m: &Manifest, req: &SkillReq, locked: Option<&Package>) -> Result<(Repository, Vec<Candidate>)> {
    // A blocked repository fails here; searching the others could pick a namesake elsewhere.
    let blocked = |r: &Repository| m.refuses(&r.url).map(|why| Error::new(MANIFEST, format!("{}: {why}", req.name)));
    let Some(repo) = locked_repository(m, req, locked) else {
        if let Some(e) = req.repository.as_deref().and_then(|n| m.repository(n)).and_then(blocked) {
            return Err(e);
        }
        let found = reg.find(&m.allowed_repositories(), &req.name, req.repository.as_deref())?;
        registry::warn_abandoned(&found.1);
        return Ok(found);
    };
    if let Some(e) = blocked(repo) {
        return Err(e);
    }
    match reg.versions(repo, &req.name)? {
        Some(c) if !c.is_empty() => {
            registry::warn_abandoned(&c);
            Ok((repo.clone(), c))
        }
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

fn source_changed(m: &Manifest, req: &SkillReq, p: &Package) -> bool {
    match (req.github, &req.repository) {
        // Only the path: the API base may differ between machines (GitHub Enterprise, a proxy).
        (Some(_), _) => !p.repository.ends_with(&format!("/repos/{}", req.name)),
        (None, Some(r)) => m.repository(r).is_some_and(|r| r.url != p.repository),
        (None, None) => !m.repositories.iter().any(|r| r.url == p.repository),
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
            bail!(MANIFEST, "--repository does not apply to github: sources");
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
    warn_invisible(&prepared[&pkg.name], &pkg)?;
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

/// Adds `selected` and `ref` to entries of a lock written before those keys existed.
fn fill_new_keys(m: &Manifest, lock: &mut Lock) -> bool {
    let mut filled = false;
    for p in &mut lock.packages {
        let Some(req) = m.skill(&p.name).filter(|r| drift(m, r, Some(p)).is_none()) else { continue };
        if p.selected.is_none()
            && let Some(sel) = &req.skills
        {
            let mut sel = sel.clone();
            sel.sort();
            p.selected = Some(sel);
            filled = true;
        }
        if p.github.is_none()
            && let Some(g) = req.github
        {
            p.github = Some(g);
            filled = true;
        }
    }
    filled
}

/// `diff`: print what changes in each updated or added package before writing the lock.
pub fn update(ctx: &Ctx, names: &[String], diff: bool) -> Result<()> {
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
    // Before the lock is written, so a diff that cannot be shown changes nothing.
    for n in newcomers.iter().filter(|_| diff) {
        let o = old.get(n);
        if let (Some(p), Some(f)) = (lock.get(n), prepared.get(n))
            && o.is_none_or(|o| o.dist_sha256 != p.dist_sha256)
        {
            // Resolving already warned about invisible Unicode in every file.
            ctx.show_diff(o, p, f, false)?;
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

/// `strict`: exit 1 when `update` would lock a newer version of a manifest entry.
pub fn outdated(ctx: &Ctx, strict: bool) -> Result<()> {
    let m = Manifest::load(&ctx.s.config, ctx.s.global)?;
    let lock = Lock::load(&ctx.s.config)?;
    // Only --strict needs what `update` resolves, and fails as `update` would on a conflict.
    let w = if strict { deps::wanted(&m, &lock)?.reqs } else { Vec::new() };
    let reg = Registry::new(ctx.http()?);
    let now = Timestamp::now();
    let mut rows = vec![["package", "locked", "within constraint", "latest", "note"].map(String::from)];
    let mut behind = Vec::new();
    for req in &m.skills {
        let p = lock.get(&req.name);
        let locked = p.map_or("-".into(), |p| p.version.clone());
        // What `update` resolves: the constraint with those of requirers added.
        let eff = w.iter().find(|r| r.name == req.name).unwrap_or(req);
        if req.github.is_some() {
            m.check_github(&req.name)?;
            let (within, latest) = github::newest(ctx.http()?, req)?;
            let dash = |v: Option<String>| v.unwrap_or_else(|| "-".into());
            rows.push([req.name.clone(), locked, dash(within), dash(latest), String::new()]);
            if let Some(p) = p.filter(|_| strict) {
                match ctx.candidate(&m, eff, Some(p)) {
                    Ok((_, c)) if newer(eff, p, &c) => behind.push(format!("{} {} -> {}", req.name, p.version, c.version)),
                    Ok(_) => {}
                    Err(e) => ui::warn(e.msg),
                }
            }
            continue;
        }
        let (repo, cands) = find(&reg, &m, req, p)?;
        let age = m.min_release_age(&repo);
        if let Some(p) = p.filter(|_| strict)
            && let Ok(c) = registry::select(&cands, &req.name, &eff.constraint, age, now)
            && newer(eff, p, &c)
        {
            behind.push(format!("{} {} -> {}", req.name, p.version, c.version));
        }
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
    if !behind.is_empty() {
        return Err(Error::new(
            MANIFEST,
            format!("{} package(s) have a newer version within their constraint: {}", behind.len(), behind.join(", ")),
        )
        .hint("run skillcask update"));
    }
    Ok(())
}

/// Whether `c` is newer than the locked `p`; a branch or commit entry compares commits.
fn newer(req: &SkillReq, p: &Package, c: &Candidate) -> bool {
    if req.github == Some(GitHub::Ref) {
        return c.dist.reference != p.dist.reference;
    }
    version::Version::parse(&p.version).is_none_or(|v| c.parsed > v)
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
    let mut passed = Vec::new();

    let mut copies = BTreeSet::new();
    let (mut links, before) = (0, open.len());
    for (at, source, n) in link_plan(&m, &locked, &hand, &locals) {
        links += 1;
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
    if open.len() == before {
        passed.push(format!("links: {links} ok"));
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
    } else {
        passed.push("git index: no managed paths committed".to_string());
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
    } else {
        passed.push(format!("{block_file}: managed block up to date"));
    }

    let before = open.len();
    for (n, (p, sk)) in &locked {
        match tree_on_disk(s, n, &[&sk.tree_sha256]) {
            None => open.push(format!("{}: not installed ({}); run `skillcask install`", s.show(&format!("{STORE}/{n}")), p.name)),
            Some(h) if h != sk.tree_sha256 => {
                open.push(format!("{}: modified ({}); run `skillcask install` to restore", s.show(&format!("{STORE}/{n}")), p.name))
            }
            Some(_) => {}
        }
    }
    if open.len() == before {
        passed.push(format!("installed: {} skill(s) match skills.lock", locked.len()));
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
    // An agent pays for every `ok` line in tokens (D19), so only a person sees what passed.
    if !ui::agent() {
        section(&passed, "✓", "", ui::Color::Green);
    }
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

/// `package`: one locked package, else every manifest entry `update` would move. `to`: the newest
/// version matching it instead, with no cooldown.
pub fn diff(ctx: &Ctx, package: Option<&str>, to: Option<&str>) -> Result<()> {
    if to.is_some() && package.is_none() {
        bail!(MANIFEST, "--to needs a package");
    }
    let m = Manifest::load(&ctx.s.config, ctx.s.global)?;
    let lock = Lock::load(&ctx.s.config)?;
    let w = deps::wanted(&m, &lock)?;
    let mut queue: std::collections::VecDeque<SkillReq> = match package {
        Some(n) => [w
            .reqs
            .iter()
            .find(|r| r.name == n)
            .cloned()
            .ok_or_else(|| Error::new(MANIFEST, format!("{n} is not in {} and no locked package requires it", manifest::FILE)))?]
        .into(),
        None => w.reqs.iter().filter(|r| m.skill(&r.name).is_some()).cloned().collect(),
    };
    let mut seen: BTreeSet<String> = queue.iter().map(|r| r.name.clone()).collect();
    // The lock as `update` would leave it, to find the dependencies new versions add.
    let mut after = lock.clone();
    let mut shown = 0;
    while let Some(req) = queue.pop_front() {
        let old = lock.get(&req.name);
        let named = package == Some(req.name.as_str());
        let found = match to.filter(|_| named) {
            Some(v) => ctx.candidate_at(&m, &req, old, v),
            None => ctx.candidate(&m, &req, old),
        };
        let (repo, c) = match found {
            Ok(x) => x,
            Err(e) if !named && e.code == MANIFEST => {
                ui::warn(e.msg);
                continue;
            }
            Err(e) => return Err(e),
        };
        if !(named && to.is_some()) && old.is_some_and(|o| !newer(&req, o, &c)) {
            continue;
        }
        let mut prepared = HashMap::new();
        let new = ctx.lock_candidate(&req, &repo, &c, None, &mut prepared)?;
        ctx.show_diff(old, &new, &prepared[&new.name], true)?;
        shown += 1;
        after.upsert(new);
        for r in deps::wanted(&m, &after)?.reqs {
            if after.get(&r.name).is_none() && seen.insert(r.name.clone()) {
                queue.push_back(r);
            }
        }
    }
    if shown == 0 {
        println!("no newer versions within the constraints");
    }
    Ok(())
}

/// A package's skills, versions and registry metadata (`vendor/name[@constraint]` or
/// `github:owner/repo[@version-or-ref]`), or which package installs a skill (its name).
pub fn info(ctx: &Ctx, target: &str) -> Result<()> {
    let m = ctx.manifest_or_default()?;
    let lock = Lock::load(&ctx.s.config)?;
    let (spec, gh) = target.strip_prefix("github:").map_or((target, false), |r| (r, true));
    let (name, at) = spec.split_once('@').map_or((spec, None), |(n, a)| (n, Some(a)));
    if !gh && !name.contains('/') {
        return why_skill(ctx, &m, &lock, name);
    }
    let name = if gh { name.to_lowercase() } else { name.to_string() };
    if !manifest::valid_package_name(&name) {
        bail!(MANIFEST, "{name:?} is not a valid package name (vendor/name, lowercase)");
    }
    let existing = m.skill(&name);
    let gh = gh || existing.is_some_and(|e| e.github.is_some());
    let locked = lock.get(&name).filter(|_| at.is_none());
    let (repo, c, versions, abandoned) = if gh {
        m.check_github(&name)?;
        let tags = github::tags(ctx.http()?, &name)?;
        let versions: Vec<String> = tags.iter().map(|t| t.version.clone()).collect();
        let req = match (at, existing) {
            (Some(_), _) => SkillReq { name: name.clone(), github: Some(GitHub::Tags), ..Default::default() },
            (None, Some(e)) => e.clone(),
            (None, None) if tags.is_empty() => {
                SkillReq { name: name.clone(), constraint: manifest::DEFAULT_REF.into(), github: Some(GitHub::Ref), ..Default::default() }
            }
            (None, None) => SkillReq { name: name.clone(), constraint: "*".into(), github: Some(GitHub::Tags), ..Default::default() },
        };
        let (repo, c) = match (at, locked) {
            (Some(a), _) => ctx.candidate_at(&m, &req, None, a)?,
            (None, Some(p)) => (github::repository(&name), locked_candidate(p)),
            (None, None) => ctx.candidate(&m, &req, None)?,
        };
        (repo, c, versions, None)
    } else {
        let probe = existing.cloned().unwrap_or_else(|| SkillReq { name: name.clone(), ..Default::default() });
        let (repo, cands) = find(&Registry::new(ctx.http()?), &m, &probe, locked)?;
        let now = Timestamp::now();
        let c = match (at, locked) {
            (Some(a), _) => registry::select(&cands, &name, a, SignedDuration::ZERO, now)?,
            (None, Some(p)) => match cands.iter().find(|c| c.version == p.version) {
                Some(c) => Candidate { dist: p.dist.clone(), dist_shasum: None, ..c.clone() },
                None => locked_candidate(p),
            },
            (None, None) => registry::select(&cands, &name, existing.map_or("*", |e| &e.constraint), m.min_release_age(&repo), now)?,
        };
        let versions = cands.iter().map(|c| c.version.clone()).collect();
        (repo, c, versions, cands.first().and_then(|c| c.abandoned.clone()))
    };
    let req = SkillReq { name: name.clone(), github: gh.then_some(GitHub::Tags), ..Default::default() };
    // The locked version's skills are the lock's, whatever its metadata says now.
    let (pkg, f) = match locked {
        Some(p) => (p.clone(), ctx.fetch(&name, &p.dist, Some(&p.dist_sha256), None)?.1),
        None => {
            let mut prepared = HashMap::new();
            let pkg = ctx.lock_candidate(&req, &repo, &c, None, &mut prepared)?;
            let f = prepared.remove(&name).expect("lock_candidate prepares it");
            warn_invisible(&f, &pkg)?;
            (pkg, f)
        }
    };

    let line = |s: String| println!("{}", skill::shown(&s));
    let title = format!("{}{name} {}", if gh { "github:" } else { "" }, c.version);
    line(if locked.is_some() { format!("{title} (locked)") } else { title });
    if let Some(d) = &c.description {
        line(format!("description: {d}"));
    }
    if !c.license.is_empty() {
        line(format!("license: {}", c.license.join(", ")));
    }
    match abandoned {
        Some(Some(r)) if manifest::valid_package_name(&r) => line(format!("abandoned: use {r} instead")),
        Some(_) => line("abandoned: yes".into()),
        None => {}
    }
    const SHOWN: usize = 20;
    if !versions.is_empty() {
        let more = if versions.len() > SHOWN { format!(" and {} more", versions.len() - SHOWN) } else { String::new() };
        line(format!("versions: {}{more}", versions.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ")));
    }
    if !pkg.requires.is_empty() {
        line(format!("requires: {}", pkg.requires.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")));
    }
    line("skills:".into());
    for sk in &pkg.skills {
        match skill::frontmatter(&skill_md_of(&f.dir.path().join(&sk.path))).remove("description") {
            Some(d) => line(format!("  {}: {d}", sk.name)),
            None => line(format!("  {}", sk.name)),
        }
    }
    for l in why(&m, &deps::wanted(&m, &lock)?, &lock, &name) {
        line(l);
    }
    Ok(())
}

/// Which package installs skill `name`, and the manifest entries that bring that package in.
fn why_skill(ctx: &Ctx, m: &Manifest, lock: &Lock, name: &str) -> Result<()> {
    let line = |s: String| println!("{}", skill::shown(&s));
    if let Some((p, sk)) = lock.skills().find(|(_, sk)| sk.name == name) {
        line(match &sk.from {
            Some(from) => format!("{name}: skill {from} of {} {}, renamed", p.name, p.version),
            None => format!("{name}: skill of {} {}", p.name, p.version),
        });
        for l in why(m, &deps::wanted(m, lock)?, lock, &p.name) {
            line(l);
        }
        return Ok(());
    }
    let locals = project::local_names(&ctx.s, &m.local)?;
    if locals.contains(name) {
        line(format!("{name}: project skill in {}/{name}", m.local));
    } else if hand_written(&ctx.s, lock, &project::block_managed(&ctx.s)?, &locals)?.contains(name) {
        line(format!("{name}: in {}, not installed by skillcask", ctx.s.show(&format!("{STORE}/{name}"))));
    } else {
        bail!(MANIFEST, "no installed skill {name:?}; info takes an installed skill name, vendor/name or github:owner/repo");
    }
    Ok(())
}

/// Why `pkg` is locked: its manifest entry, or the packages requiring it, up to their entries.
fn why(m: &Manifest, w: &deps::Wanted, lock: &Lock, pkg: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([pkg.to_string()]);
    let mut seen = BTreeSet::new();
    while let Some(n) = queue.pop_front() {
        if !seen.insert(n.clone()) {
            continue;
        }
        if let Some(r) = m.skill(&n) {
            out.push(format!("{n}: in {} as {:?}", manifest::FILE, r.constraint));
        }
        match w.by.get(&n) {
            Some(by) => {
                out.push(format!("{n}: required by {}", by.iter().map(|(r, c)| format!("{r} ({c})")).collect::<Vec<_>>().join(", ")));
                queue.extend(by.iter().map(|(r, _)| r.clone()));
            }
            None if m.skill(&n).is_none() && lock.get(&n).is_some() => {
                out.push(format!("{n}: in {} only; nothing in {} requires it", crate::lock::FILE, manifest::FILE))
            }
            None => {}
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_and_answers() {
        let taken: BTreeSet<String> = ["code-review".into(), "mattpocock-taken".into()].into();
        assert_eq!(suggest("mattpocock/skills", "code-review", &taken).as_deref(), Some("mattpocock-code-review"));
        assert_eq!(suggest("my_org.x/skills", "a", &taken).as_deref(), Some("my-org-x-a"));
        assert_eq!(suggest("mattpocock/skills", "taken", &taken), None);
        let s = Some("mp-x");
        assert_eq!(answer("", s, &taken), Answer::Use("mp-x".into()));
        assert_eq!(answer("Y", s, &taken), Answer::Use("mp-x".into()));
        assert_eq!(answer("n", s, &taken), Answer::Decline);
        assert_eq!(answer("mine", s, &taken), Answer::Use("mine".into()));
        assert!(matches!(answer("", None, &taken), Answer::Retry(_)));
        assert!(matches!(answer("Bad Name", s, &taken), Answer::Retry(_)));
        assert!(matches!(answer("code-review", s, &taken), Answer::Retry(_)));

        let c = Conflict {
            package: "acme/a".into(),
            upstream: "one".into(),
            msg: "skill \"one\" is provided by both x and acme/a".into(),
            rename: true,
        };
        assert_eq!(question(&c.msg, &c.package), c.msg);
        assert_eq!(question("one: ~/.claude/skills/one shadows", "acme/a"), "one: ~/.claude/skills/one shadows (acme/a)");
        let renames = BTreeMap::from([("a".to_string(), "x".to_string())]);
        assert!(
            hint(&c, &taken, Some(&renames)).ends_with("rename = { a = \"x\", one = \"acme-one\" }"),
            "{}",
            hint(&c, &taken, Some(&renames))
        );
    }
}
