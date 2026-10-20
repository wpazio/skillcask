//! Name clashes, rename prompts and the preflight that runs before anything is saved.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::bail;
use crate::error::{MANIFEST, Result};
use crate::lock::{Lock, LockedSkill, Package};
use crate::manifest::{self, Manifest, SkillReq};
use crate::project::{self, STORE};
use crate::scope::Scope;
use crate::skill;

use super::Ctx;
use super::disk::{Link, hand_written, kept, link_plan, locked_names, locked_skill, not_ours, shadows, tree_on_disk};
use super::fetch::Fetched;

impl Ctx {
    /// Conflicts that would stop `materialize`, checked before `add` writes skills.toml and skills.lock.
    /// `old`: the lock as it was before this command.
    pub(super) fn preflight(&self, m: &Manifest, lock: &Lock, old: &Lock) -> Result<()> {
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
        for Link { at, source, name: n } in link_plan(m, &locked, &hand, &locals) {
            let removed = old_managed.contains(&n) && !kept.contains(&n) && !locked.contains_key(&n);
            if !(at == STORE && removed) && not_ours(s, m, at, &n, &project::link_state(s, at, source, &n), &old_managed) {
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
                    out.push(conflict(p, sk, format!("skill {:?} is provided by both {} and {}", sk.name, first.name, p.name)));
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

    /// Offers a rename for each conflict of a newly resolved package; returns the manifest entries
    /// that changed. Without a terminal, does nothing: `preflight` then fails with the hint.
    pub(super) fn settle_conflicts(
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
            rename_locked(lock, prepared, pkg, upstream, &to)?;
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
}

/// Installs `upstream` of `pkg` as `to`: rewrites its lock entry from the prepared download.
fn rename_locked(lock: &mut Lock, prepared: &HashMap<String, Fetched>, pkg: &str, upstream: &str, to: &str) -> Result<()> {
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
    let shown = project::store_shown(s, &sk.name);
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
