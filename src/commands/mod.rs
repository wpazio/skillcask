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

pub use edit::{add, init, remove};
pub use report::{doctor, outdated, search, status};
pub use sync::{install, update};

pub struct Ctx {
    pub s: Scope,
    pub verbose: bool,
    pub limits: Limits,
    /// stdin and stderr are terminals and CI is unset: conflicts may prompt for a rename.
    pub interactive: bool,
    http: OnceCell<Http>,
}

impl Ctx {
    pub fn new(s: Scope, verbose: bool, limits: Limits, interactive: bool) -> Ctx {
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
