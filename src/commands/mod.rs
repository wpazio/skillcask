//! The nine commands, the `Ctx` they share, and the pipeline split into fetch, resolve, conflicts, materialize.

mod conflicts;
mod disk;
mod drift;
mod edit;
mod fetch;
mod materialize;
mod report;
mod resolve;
mod sync;

use std::cell::OnceCell;

use crate::archive::Limits;
use crate::error::Result;
use crate::http::Http;
use crate::manifest::{self, Manifest};
use crate::scope::Scope;
use crate::ui;

pub(crate) use edit::{add, init, remove};
pub(crate) use report::{doctor, outdated, search, status};
pub(crate) use sync::{install, update};

pub(crate) struct Ctx {
    s: Scope,
    verbose: bool,
    limits: Limits,
    /// stdin and stderr are terminals and CI is unset: conflicts may prompt for a rename.
    interactive: bool,
    http: OnceCell<Http>,
}

impl Ctx {
    pub(crate) fn new(s: Scope, verbose: bool, limits: Limits, interactive: bool) -> Ctx {
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

/// A report line such as `updated a/b 1.0.0 -> 1.1.0`, its first word as the verb.
fn say(line: &str) {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    ui::done(verb, rest);
}

#[cfg(test)]
mod fixtures {
    use std::collections::BTreeMap;

    use crate::lock::{Dist, LockedSkill, Package};

    /// A lock entry at 1.0.0 whose skills keep their upstream names.
    pub(super) fn pkg(name: &str, constraint: &str, repo: &str, skills: &[&str]) -> Package {
        let skill = |s: &&str| LockedSkill { name: (*s).into(), from: None, path: (*s).into(), tree_sha256: String::new() };
        Package {
            name: name.into(),
            constraint: constraint.into(),
            github: None,
            version: "1.0.0".into(),
            repository: repo.into(),
            dist: Dist { url: String::new(), reference: None },
            dist_sha256: String::new(),
            released: None,
            selected: None,
            requires: BTreeMap::new(),
            skills: skills.iter().map(skill).collect(),
        }
    }
}
