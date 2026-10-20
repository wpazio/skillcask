use std::collections::{BTreeMap, BTreeSet};

use crate::bail;
use crate::error::{MANIFEST, Result};
use crate::lock::{Lock, LockedSkill, Package};
use crate::manifest::Manifest;
use crate::project::{self, LinkState, STORE};
use crate::scope::Scope;
use crate::skill::{self, FileEntry};

/// Personal skills with the same name as project ones. Claude Code runs the personal one;
/// Codex lists both. Empty for the global scope.
pub(super) fn shadows<'a>(s: &Scope, m: &Manifest, names: impl IntoIterator<Item = &'a String>) -> Vec<String> {
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

pub(super) fn locked_names(lock: &Lock) -> Result<BTreeMap<String, (&Package, &LockedSkill)>> {
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
pub(super) fn not_ours(s: &Scope, m: &Manifest, at: &str, n: &str, state: &LinkState, old_managed: &BTreeSet<String>) -> bool {
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

pub(super) fn hand_written(s: &Scope, lock: &Lock, old_managed: &BTreeSet<String>, locals: &BTreeSet<String>) -> Result<BTreeSet<String>> {
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
pub(super) fn link_plan<'a>(
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

pub(super) fn locked_skill<'a>(lock: &'a Lock, name: &str) -> Option<(&'a Package, &'a LockedSkill)> {
    lock.skills().find(|(_, sk)| sk.name == name)
}

/// Managed names no longer locked whose store dir is not what the old lock installed: modified,
/// or never installed by skillcask (the block is committed and shared). They are not deleted.
pub(super) fn kept(s: &Scope, lock: &Lock, old: &Lock, old_managed: &BTreeSet<String>) -> BTreeSet<String> {
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

pub(super) fn installed_ok(s: &Scope, sk: &LockedSkill) -> bool {
    tree_on_disk(s, &sk.name, &[&sk.tree_sha256]).is_some_and(|h| h == sk.tree_sha256)
}

/// `expect`: hashes the caller compares with. HFS+ stores names decomposed, so with a non-ASCII
/// path the NFC and NFD spellings are tried too, and the one matching `expect` wins.
pub(super) fn tree_on_disk(s: &Scope, name: &str, expect: &[&str]) -> Option<String> {
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

pub(super) fn set_modes(modes: &mut BTreeSet<String>, name: &str, files: &[FileEntry]) {
    let prefix = format!("{name}/");
    modes.retain(|l| !l.starts_with(&prefix));
    modes.extend(files.iter().filter(|f| f.exec).map(|f| format!("{prefix}{}", f.path)));
}

pub(super) fn managed_paths(managed: &BTreeSet<String>, targets: &[String]) -> Vec<String> {
    managed.iter().flat_map(|n| std::iter::once(format!("{STORE}/{n}")).chain(targets.iter().map(move |t| format!("{t}/{n}")))).collect()
}
