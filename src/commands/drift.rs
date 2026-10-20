use std::collections::{BTreeMap, BTreeSet};

use crate::deps;
use crate::lock::{Lock, Package};
use crate::manifest::{GitHub, Manifest, Repository, SkillReq};

/// Manifest entries that need resolving, lock entries to drop, and why.
/// `reqs`: the effective requirements, `deps::wanted`.
pub(super) fn reconcile(m: &Manifest, reqs: &[SkillReq], lock: &Lock) -> (Vec<SkillReq>, Vec<String>, Vec<String>) {
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
pub(super) fn drifted(m: &Manifest, lock: &Lock) -> BTreeSet<String> {
    let Ok(w) = deps::wanted(m, lock) else { return BTreeSet::new() };
    let (needs, stale, _) = reconcile(m, &w.reqs, lock);
    needs.into_iter().map(|r| r.name).filter(|n| m.skill(n).is_some()).chain(stale).collect()
}

/// Why the lock entry does not match the manifest entry. Keys a lock may lack (written by an
/// older version) count as matching: absent `selected` is all skills, absent `ref` any mode.
pub(super) fn drift(m: &Manifest, req: &SkillReq, p: Option<&Package>) -> Option<String> {
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
pub(super) fn locked_repository<'a>(m: &'a Manifest, req: &SkillReq, locked: Option<&Package>) -> Option<&'a Repository> {
    let p = locked.filter(|p| req.github.is_none() && !source_changed(m, req, p))?;
    m.repositories.iter().find(|r| r.url == p.repository)
}

pub(super) fn source_changed(m: &Manifest, req: &SkillReq, p: &Package) -> bool {
    match (req.github, &req.repository) {
        // Only the path: the API base may differ between machines (GitHub Enterprise, a proxy).
        (Some(_), _) => !p.repository.ends_with(&format!("/repos/{}", req.name)),
        (None, Some(r)) => m.repository(r).is_some_and(|r| r.url != p.repository),
        (None, None) => !m.repositories.iter().any(|r| r.url == p.repository),
    }
}

/// Adds `selected` and `ref` to entries of a lock written before those keys existed.
pub(super) fn fill_new_keys(m: &Manifest, lock: &mut Lock) -> bool {
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
