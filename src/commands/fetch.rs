use std::path::Path;

use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::archive;
use crate::bail;
use crate::error::{Context, Error, HASH, NETWORK, Result};
use crate::lock::Dist;
use crate::project;
use crate::skill::{self, FileEntry};

use super::Ctx;

/// A downloaded, verified and unpacked dist, kept so `add`/`update` do not download twice.
pub(super) struct Fetched {
    pub(super) dir: tempfile::TempDir,
    pub(super) entries: Vec<FileEntry>,
    pub(super) links: Vec<archive::Link>,
    pub(super) skipped: Vec<archive::Skipped>,
}

impl Fetched {
    pub(super) fn skill_files(&self, package: &str, skill: &str, dir: &str) -> Result<Vec<FileEntry>> {
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
    pub(super) fn renamed_files(&self, mut files: Vec<FileEntry>, dir: &str, to: &str) -> Result<(Vec<FileEntry>, Option<String>)> {
        let Some(e) = files.iter_mut().find(|e| e.path == skill::SKILL_MD) else {
            return Ok((files, None));
        };
        let new = skill::rename_frontmatter(&std::fs::read_to_string(self.dir.path().join(dir).join(skill::SKILL_MD))?, to);
        *e = FileEntry::new(e.path.clone(), e.exec, new.as_bytes());
        Ok((files, Some(new)))
    }
}

impl Ctx {
    pub(super) fn fetch(&self, name: &str, dist: &Dist, expected: Option<&str>, shasum: Option<&str>) -> Result<(String, Fetched)> {
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
        let dir = project::temp_dir(&self.s)?;
        let x = archive::extract(&bytes, dir.path(), &self.limits).ctx(name)?;
        Ok((sha, Fetched { dir, entries: x.files, links: x.links, skipped: x.skipped }))
    }
}

/// `<dir>/SKILL.md`, where `requires:` is read from both at lock and at install time; empty when
/// the skill names another file.
pub(super) fn skill_md_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(skill::SKILL_MD)).unwrap_or_default()
}
