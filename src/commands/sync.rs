use std::collections::{BTreeSet, HashMap};

use crate::bail;
use crate::deps;
use crate::error::{Error, MANIFEST, Result};
use crate::lock::Lock;
use crate::manifest::{self, Manifest};

use super::drift::{drift, drifted, fill_new_keys, reconcile};
use super::fetch::Fetched;
use super::resolve::settle_deps;
use super::{Ctx, say};

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
