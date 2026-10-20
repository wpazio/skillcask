use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;

use crate::deps;
use crate::error::{Error, MANIFEST, Result};
use crate::github;
use crate::lock::Lock;
use crate::manifest::Manifest;
use crate::project::{self, LinkState, STORE};
use crate::registry::{self, Candidate, Registry};
use crate::ui;
use crate::version::Constraint;

use super::Ctx;
use super::disk::{hand_written, link_plan, locked_names, managed_paths, not_ours, shadows, tree_on_disk};
use super::drift::drift;

pub(crate) fn outdated(ctx: &Ctx) -> Result<()> {
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

pub(crate) fn status(ctx: &Ctx) -> Result<()> {
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

pub(crate) fn doctor(ctx: &Ctx, fix: bool) -> Result<()> {
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

pub(crate) fn search(ctx: &Ctx, query: &str) -> Result<()> {
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

/// `status` states that are not problems.
fn healthy(state: &str) -> bool {
    matches!(state, "ok" | "hand-written" | "project")
}
