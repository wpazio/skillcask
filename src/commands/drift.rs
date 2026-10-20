use std::collections::{BTreeMap, BTreeSet};

use crate::deps;
use crate::lock::{Lock, Package};
use crate::manifest::{GitHub, Manifest, Repository, SkillReq};

/// Manifest entries that need resolving, lock entries to drop, and why.
pub(super) struct Reconciled {
    pub(super) needs: Vec<SkillReq>,
    pub(super) stale: Vec<String>,
    /// A line per entry of `needs`, then per entry of `stale`.
    pub(super) why: Vec<String>,
}

/// `reqs`: the effective requirements, `deps::wanted`.
pub(super) fn reconcile(m: &Manifest, reqs: &[SkillReq], lock: &Lock) -> Reconciled {
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
    Reconciled { needs, stale, why }
}

/// Manifest entries that drift from `lock`, and lock entries nobody wants: commands that do not
/// own them leave them alone.
pub(super) fn drifted(m: &Manifest, lock: &Lock) -> BTreeSet<String> {
    let Ok(w) = deps::wanted(m, lock) else { return BTreeSet::new() };
    let Reconciled { needs, stale, .. } = reconcile(m, &w.reqs, lock);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fixtures::pkg;
    use crate::lock::LockedSkill;

    const ACME: &str = "https://acme.example/packages.json";
    const OTHER: &str = "https://other.example/packages.json";
    const GH: &str = "https://api.github.com/repos/acme/a";

    fn manifest(skills: &str) -> Manifest {
        crate::manifest::parse(&format!("[repositories]\nacme = {{ url = \"{ACME}\" }}\n[skills]\n{skills}\n"), false).unwrap()
    }

    fn lock(packages: Vec<Package>) -> Lock {
        Lock { packages }
    }

    #[test]
    fn drift_names_the_first_difference() {
        let m = manifest("\"acme/a\" = { version = \"^1.0\", repository = \"acme\", rename = { a = \"x\" } }");
        let req = &m.skills[0];
        assert_eq!(drift(&m, req, None).as_deref(), Some("not in skills.lock"), "missing entry");
        // The constraint is reported before the repository, and both before the rename, when all differ.
        let p = pkg("acme/a", "^2.0", OTHER, &["a"]);
        assert_eq!(drift(&m, req, Some(&p)).as_deref(), Some("constraint changed from \"^2.0\" to \"^1.0\""), "constraint first");
        let p = pkg("acme/a", "^1.0", OTHER, &["a"]);
        assert_eq!(drift(&m, req, Some(&p)).as_deref(), Some("repository changed"), "repository before rename");
        let mut p = pkg("acme/a", "^1.0", ACME, &["a"]);
        assert_eq!(drift(&m, req, Some(&p)).as_deref(), Some("rename changed"), "rename last");
        p.skills[0] = LockedSkill { name: "x".into(), from: Some("a".into()), ..p.skills[0].clone() };
        assert_eq!(drift(&m, req, Some(&p)), None, "matching rename");
    }

    #[test]
    fn drift_in_github_entries() {
        let m = manifest("\"acme/a\" = { github = true, version = \"^1.0\" }");
        let req = &m.skills[0];
        // Only the path counts: the API base may differ between machines.
        let mut p = pkg("acme/a", "^1.0", "https://ghe.example/api/v3/repos/acme/a", &["a"]);
        p.github = Some(GitHub::Tags);
        assert_eq!(drift(&m, req, Some(&p)), None, "same path, other API base");
        p.github = Some(GitHub::Ref);
        assert_eq!(drift(&m, req, Some(&p)).as_deref(), Some("changed from ref to version"), "mode differs");
        // A lock written before `ref` existed matches either mode.
        p.github = None;
        assert_eq!(drift(&m, req, Some(&p)), None, "old lock without ref");
    }

    #[test]
    fn drift_in_the_skill_selection() {
        let m = manifest("\"acme/a\" = { version = \"^1.0\", skills = [\"a\"] }");
        let p = pkg("acme/a", "^1.0", ACME, &["a", "b"]);
        assert_eq!(drift(&m, &m.skills[0], Some(&p)).as_deref(), Some("skill selection changed"), "narrowed to one skill");
        let all = manifest("\"acme/a\" = \"^1.0\"");
        let mut p = pkg("acme/a", "^1.0", ACME, &["a"]);
        p.selected = Some(vec!["a".into()]);
        assert_eq!(drift(&all, &all.skills[0], Some(&p)).as_deref(), Some("skill selection changed"), "selection dropped");
        // The selection names skills as the package calls them, before any rename.
        let m = manifest("\"acme/a\" = { version = \"^1.0\", skills = [\"a\"], rename = { a = \"x\" } }");
        let mut p = pkg("acme/a", "^1.0", ACME, &["x"]);
        p.skills[0].from = Some("a".into());
        assert_eq!(drift(&m, &m.skills[0], Some(&p)), None, "selection by upstream name");
    }

    #[test]
    fn source_changed_compares_what_the_entry_names() {
        let gh = "\"acme/a\" = { github = true, version = \"^1.0\" }";
        let named = "\"acme/a\" = { version = \"^1.0\", repository = \"acme\" }";
        // Without a repository any declared one keeps the entry; an undeclared one is a change.
        let any = "\"acme/a\" = \"^1.0\"";
        let cases = [
            (gh, "https://ghe.example/api/v3/repos/acme/a", false),
            (gh, "https://api.github.com/repos/acme/b", true),
            (named, ACME, false),
            (named, OTHER, true),
            (any, ACME, false),
            (any, OTHER, true),
        ];
        for (entry, repo, want) in cases {
            let m = manifest(entry);
            assert_eq!(source_changed(&m, &m.skills[0], &pkg("acme/a", "^1.0", repo, &[])), want, "{entry} locked at {repo}");
        }
    }

    #[test]
    fn reconcile_lists_needs_then_stale() {
        let m = manifest("\"acme/a\" = \"^1.0\"");
        let Reconciled { needs, stale, why } = reconcile(&m, &m.skills, &Lock::default());
        assert_eq!(needs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["acme/a"], "empty lock: needs");
        assert_eq!(stale, Vec::<String>::new(), "empty lock: stale");
        assert_eq!(why, ["acme/a: not in skills.lock"], "empty lock: why");

        let orphan = lock(vec![pkg("acme/b", "^1.0", ACME, &["b"])]);
        let Reconciled { needs, stale, why } = reconcile(&m, &m.skills, &orphan);
        assert_eq!(needs.len(), 1, "orphan: needs");
        assert_eq!(stale, ["acme/b"], "orphan: stale");
        assert_eq!(why, ["acme/a: not in skills.lock", "acme/b: in skills.lock but neither in skills.toml nor required"], "orphan: why");

        let matching = lock(vec![pkg("acme/a", "^1.0", ACME, &["a"])]);
        let Reconciled { needs, stale, why } = reconcile(&m, &m.skills, &matching);
        assert!(needs.is_empty(), "{needs:?}");
        assert!(stale.is_empty(), "{stale:?}");
        assert!(why.is_empty(), "{why:?}");
    }

    #[test]
    fn drifted_skips_dependencies_and_keeps_orphans() {
        let m = manifest("\"acme/a\" = \"^1.0\"");
        let mut a = pkg("acme/a", "^1.0", ACME, &["a"]);
        a.requires.insert("acme/dep".into(), "^1.0".into());
        // The dependency drifts (locked at ^2.0) but has no manifest entry to leave alone; the orphan is stale.
        let l = lock(vec![a, pkg("acme/dep", "^2.0", ACME, &["dep"]), pkg("acme/c", "^1.0", ACME, &["c"])]);
        assert_eq!(drifted(&m, &l), BTreeSet::from(["acme/c".to_string()]));
    }

    #[test]
    fn locked_repository_only_for_an_unchanged_composer_source() {
        let gh = manifest("\"acme/a\" = { github = true, version = \"^1.0\" }");
        assert!(locked_repository(&gh, &gh.skills[0], Some(&pkg("acme/a", "^1.0", GH, &[]))).is_none());
        let m = manifest("\"acme/a\" = \"^1.0\"");
        assert!(locked_repository(&m, &m.skills[0], None).is_none());
        assert!(locked_repository(&m, &m.skills[0], Some(&pkg("acme/a", "^1.0", OTHER, &[]))).is_none());
        assert_eq!(locked_repository(&m, &m.skills[0], Some(&pkg("acme/a", "^1.0", ACME, &[]))).map(|r| r.name.as_str()), Some("acme"));
    }

    #[test]
    fn fill_new_keys_completes_matching_entries() {
        let m =
            manifest("\"acme/a\" = { version = \"^1.0\", skills = [\"b\", \"a\"] }\n\"acme/g\" = { github = true, version = \"^1.0\" }");
        let mut l =
            lock(vec![pkg("acme/a", "^1.0", ACME, &["a", "b"]), pkg("acme/g", "^1.0", "https://api.github.com/repos/acme/g", &["g"])]);
        assert!(fill_new_keys(&m, &mut l));
        assert_eq!(l.packages[0].selected.as_deref(), Some(&["a".to_string(), "b".to_string()][..]));
        assert_eq!(l.packages[1].github, Some(GitHub::Tags));
        assert!(!fill_new_keys(&m, &mut l), "nothing left to fill");
        // A drifted entry is re-locked by `install`, not patched.
        let mut l = lock(vec![pkg("acme/a", "^2.0", ACME, &["a", "b"])]);
        assert!(!fill_new_keys(&m, &mut l));
        assert_eq!(l.packages[0].selected, None);
    }
}
