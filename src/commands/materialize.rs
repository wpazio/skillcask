use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::bail;
use crate::deps;
use crate::error::{Context, Error, HASH, MANIFEST, Result};
use crate::lock::{Lock, LockedSkill};
use crate::manifest::Manifest;
use crate::project::{self, STORE};
use crate::skill;

use super::Ctx;
use super::disk::{Link, hand_written, installed_ok, kept, link_plan, locked_names, managed_paths, set_modes, shadows};
use super::fetch::{Fetched, skill_md_of};

impl Ctx {
    /// Makes disk match the lock: store, links, gitignore block, git index.
    pub(super) fn materialize(&self, m: &Manifest, lock: &Lock, old: &Lock, mut prepared: HashMap<String, Fetched>) -> Result<()> {
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
                    let shown = format!("{}/{}", project::store_shown(s, &sk.name), skill::SKILL_MD);
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
                crate::ui::warn(format!("kept {}: modified or not installed by skillcask", project::store_shown(s, n)));
                continue;
            }
            let dir = project::store_dir(s, n);
            if dir.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
                std::fs::remove_file(&dir).ctx(project::store_shown(s, n))?;
            } else if dir.is_dir() {
                std::fs::remove_dir_all(dir).ctx(project::store_shown(s, n))?;
            }
            crate::ui::done("removed", n);
        }
        project::write_modes(s, &modes)?;

        let plan = link_plan(m, &locked, &hand, &locals);
        if !s.global {
            // In home skillcask links only locked skills; other links there are the user's.
            let names: BTreeSet<&String> = plan.iter().map(|l| &l.name).collect();
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
        for Link { at, source, name: n } in plan {
            if project::link(s, at, source, &n)? && !locked.contains_key(&n) {
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
