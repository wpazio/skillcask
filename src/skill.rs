//! SKILL.md frontmatter, skill names and the tree hash.

use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::bail;
use crate::error::{Context, IO, Result};

pub const SKILL_MD: &str = "SKILL.md";

/// Top-level scalar keys of the YAML frontmatter; nested values are skipped. No closing `---`:
/// no frontmatter.
pub fn frontmatter(text: &str) -> BTreeMap<String, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    fields(&lines).unwrap_or_default().into_iter().filter_map(|f| Some((f.key.to_string(), f.value?))).collect()
}

/// Sets the frontmatter `name:` value, keeping every other byte. No frontmatter or no `name:`
/// line: unchanged, since the directory name then names the skill. Picks the key `frontmatter`
/// reads (the last top-level `name`) and replaces all lines of its value.
pub fn rename_frontmatter(text: &str, to: &str) -> String {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let bom = &text[..text.len() - body.len()];
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let Some(f) = fields(&lines).and_then(|f| f.into_iter().rev().find(|f| f.key == "name")) else {
        return text.to_string();
    };
    let first = lines[f.lines.start];
    let eol = &first[first.trim_end_matches(['\n', '\r']).len()..];
    format!("{bom}{}name: {to}{eol}{}", lines[..f.lines.start].concat(), lines[f.lines.end..].concat())
}

/// The top-level `requires:` block mapping, key to value as written (quotes and comments
/// removed); empty without one. Values are checked by `deps::check`.
pub fn requires(text: &str) -> std::result::Result<BTreeMap<String, String>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(f) = fields(&lines).and_then(|f| f.into_iter().rev().find(|f| f.key == "requires")) else {
        return Ok(BTreeMap::new());
    };
    let line = |l: &'_ str| l.trim_end_matches(['\n', '\r']).to_string();
    let inline = line(lines[f.lines.start]).split_once(':').map_or(String::new(), |(_, v)| v.trim().to_string());
    if !inline.is_empty() && !inline.starts_with('#') {
        return Err("requires: must be a block mapping of package to constraint".into());
    }
    let mut out = BTreeMap::new();
    let mut indent = None;
    for l in &lines[f.lines.start + 1..f.lines.end] {
        let l = line(l);
        let t = l.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if *indent.get_or_insert(l.len() - t.len()) != l.len() - t.len() {
            return Err(format!("requires: unexpected nesting at {t:?}"));
        }
        let (k, v) = entry(t).ok_or_else(|| format!("requires: expected `package: constraint`, got {t:?}"))?;
        if out.insert(k.clone(), v).is_some() {
            return Err(format!("requires: {k} appears twice"));
        }
    }
    Ok(out)
}

/// `key: value` of one mapping line; either side may be quoted.
fn entry(t: &str) -> Option<(String, String)> {
    let unquote = |s: &str| -> Option<(String, usize)> {
        let q = s.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let n = closing_quote(&s[1..], q)?;
        Some((s[1..=n].to_string(), n + 2))
    };
    let (key, rest) = match unquote(t) {
        Some((k, n)) => (k, t[n..].strip_prefix(':')?),
        // `github:owner/repo` holds a colon; only `: ` or a final `:` ends a plain key.
        None => match t.find(": ") {
            Some(n) => (t[..n].to_string(), &t[n + 1..]),
            None => (t.strip_suffix(':')?.to_string(), ""),
        },
    };
    let rest = rest.trim();
    let value = match unquote(rest) {
        Some((v, _)) => v,
        None => {
            let cut = rest.char_indices().find(|&(n, c)| c == '#' && rest[..n].ends_with(char::is_whitespace));
            rest[..cut.map_or(rest.len(), |(n, _)| n)].trim().to_string()
        }
    };
    (!key.trim().is_empty() && !value.is_empty()).then(|| (key.trim().to_string(), value))
}

struct Field<'a> {
    key: &'a str,
    /// `None` when empty, nested or an unterminated quote.
    value: Option<String>,
    lines: std::ops::Range<usize>,
}

/// Top-level keys of the frontmatter in `lines` (with line ends); `None` unless it opens and
/// closes with `---`. Understands the YAML that decides which line is a key: quoted scalars
/// spanning lines, block scalars and comments.
fn fields<'a>(lines: &[&'a str]) -> Option<Vec<Field<'a>>> {
    let line = |i: usize| lines.get(i).map(|l| l.trim_end_matches(['\n', '\r']));
    let is_end = |l: &str| l.trim_end() == "---";
    let indented = |l: &str| l.starts_with([' ', '\t']) || l.trim().is_empty();
    if !line(0).is_some_and(is_end) {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    loop {
        let l = line(i)?;
        if is_end(l) {
            return Some(out);
        }
        let start = i;
        i += 1;
        let Some((key, v)) = l.split_once(':').filter(|_| !l.starts_with([' ', '\t', '#', '-'])) else { continue };
        let v = v.trim_start();
        let mut value = None;
        if let Some(q) = v.chars().next().filter(|c| matches!(c, '"' | '\'')) {
            let (mut parts, mut rest) = (Vec::new(), &v[1..]);
            loop {
                if let Some(n) = closing_quote(rest, q) {
                    parts.push(rest[..n].trim());
                    value = Some(parts.join(" "));
                    break;
                }
                parts.push(rest.trim());
                match line(i) {
                    Some(l) if !is_end(l) => (rest, i) = (l, i + 1),
                    _ => break,
                }
            }
        }
        let body = i;
        while line(i).is_some_and(indented) {
            i += 1;
        }
        while i > body && line(i - 1).is_some_and(|l| l.trim().is_empty()) {
            i -= 1;
        }
        if v.starts_with(['>', '|']) {
            let sep = if v.starts_with('|') { "\n" } else { " " };
            value = Some((body..i).filter_map(line).map(str::trim).collect::<Vec<_>>().join(sep));
        } else if !v.starts_with(['"', '\'']) {
            let plain = |v: &'a str| {
                let cut = v.char_indices().find(|&(n, c)| c == '#' && v[..n].chars().next_back().is_none_or(char::is_whitespace));
                v[..cut.map_or(v.len(), |(n, _)| n)].trim()
            };
            // Indented lines continue a plain scalar, folded with spaces, unless they hold a mapping,
            // a sequence or a comment: then the value is nested.
            let more: Vec<&str> = (body..i).filter_map(line).map(str::trim).filter(|l| !l.is_empty()).collect();
            let nested =
                more.iter().any(|l| l.starts_with('#') || l.starts_with("- ") || *l == "-" || l.contains(": ") || l.ends_with(':'));
            if !nested {
                value = Some(
                    std::iter::once(plain(v)).chain(more.into_iter().map(plain)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" "),
                );
            }
        }
        out.push(Field { key: key.trim(), value: value.filter(|v| !v.is_empty()), lines: start..i });
    }
}

fn closing_quote(s: &str, q: char) -> Option<usize> {
    let mut chars = s.char_indices().peekable();
    while let Some((n, c)) = chars.next() {
        if q == '"' && c == '\\' {
            chars.next();
        } else if c == q {
            if q == '\'' && chars.next_if(|&(_, c)| c == '\'').is_some() {
                continue;
            }
            return Some(n);
        }
    }
    None
}

/// `[a-z0-9][a-z0-9-]*`: also the directory name under `.agents/skills/`.
pub fn valid_name(n: &str) -> bool {
    n.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    /// Relative, `/`-separated.
    pub path: String,
    pub exec: bool,
    pub sha256: String,
}

impl FileEntry {
    pub fn new(path: String, exec: bool, content: &[u8]) -> FileEntry {
        FileEntry { path, exec, sha256: hex::encode(Sha256::digest(content)) }
    }
}

/// sha256 over `<path>\0<mode>\0<sha256 of content>\n` per regular file, sorted by path.
pub fn tree_hash(entries: &[FileEntry]) -> String {
    let mut sorted: Vec<&FileEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut h = Sha256::new();
    for e in sorted {
        h.update(format!("{}\0{}\0{}\n", e.path, if e.exec { "755" } else { "644" }, e.sha256));
    }
    hex::encode(h.finalize())
}

/// Lists regular files under `dir`; symlinks, special files and empty directories are errors.
pub fn scan_dir(dir: &Path) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    scan(dir, "", None, &mut out)?;
    Ok(out)
}

/// What `publish` zips: like `scan_dir`, without hidden entries (`.DS_Store`, `.env`, `.git/`),
/// and a relative symlink to a file inside the skill is a copy of that file, as at install.
pub fn scan_publish(dir: &Path) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    scan(dir, "", Some(&dir.canonicalize().ctx(dir.display())?), &mut out)?;
    Ok(out)
}

fn scan(dir: &Path, prefix: &str, publish_root: Option<&Path>, out: &mut Vec<FileEntry>) -> Result<()> {
    let mut any = false;
    for entry in std::fs::read_dir(dir).ctx(dir.display())? {
        let entry = entry.ctx(dir.display())?;
        any = true;
        #[expect(clippy::unnecessary_debug_formatting, reason = "Debug shows the non-UTF-8 bytes; the message text must not change")]
        let name = entry.file_name().into_string().map_err(|n| crate::error::Error::new(IO, format!("non-UTF-8 file name {n:?}")))?;
        if publish_root.is_some() && name.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let meta = entry.path().symlink_metadata().ctx(format!("{rel:?}"))?;
        let ft = meta.file_type();
        if ft.is_dir() {
            scan(&entry.path(), &rel, publish_root, out)?;
        } else if ft.is_file() {
            let bytes = std::fs::read(entry.path()).ctx(format!("{rel:?}"))?;
            out.push(FileEntry::new(rel, is_exec(&meta), &bytes));
        } else if let Some(root) = publish_root.filter(|_| ft.is_symlink()) {
            let target = std::fs::canonicalize(entry.path())
                .ok()
                .filter(|t| std::fs::read_link(entry.path()).is_ok_and(|l| l.is_relative()) && t.starts_with(root) && t.is_file());
            let Some(target) = target else {
                bail!(IO, "{rel:?}: a symlink in a skill must be relative and point to a file inside the skill");
            };
            let what = format!("{rel:?}");
            let meta = target.metadata().ctx(&what)?;
            out.push(FileEntry::new(rel, is_exec(&meta), &std::fs::read(&target).ctx(what)?));
        } else {
            bail!(IO, "{rel:?}: symlinks and special files are not allowed in a skill");
        }
    }
    if !any && !prefix.is_empty() {
        bail!(IO, "{prefix:?}: empty directories are not allowed in a skill");
    }
    Ok(())
}

#[cfg(unix)]
fn is_exec(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

// No exec bit here; installed modes come from the store's `.skillcask-modes` instead.
#[cfg(not(unix))]
fn is_exec(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_frontmatter() {
        let f = frontmatter("---\nname: agents-md\nversion: \"1.2.0\"\ndescription: >\n  multi\nmetadata:\n  a: b\n---\n# body\nname: no");
        assert_eq!(f.get("name").unwrap(), "agents-md");
        assert_eq!(f.get("version").unwrap(), "1.2.0");
        assert!(!f.contains_key("a"));
        assert!(frontmatter("# no frontmatter").is_empty());
        assert!(valid_name("agents-md") && valid_name("a1") && !valid_name("-a") && !valid_name("A") && !valid_name(""));
    }

    #[test]
    fn reads_requires() {
        let t = "---\nname: mr\nrequires:\n  acme/review: ^1.0 # pinned\n\n  # comment\n  \"github:o/r/pr\": 'main'\n  github:o/x: \">=1, <2\"\n  acme/b: >=1, <2\ndescription: d\n---\n";
        let r = requires(t).unwrap();
        assert_eq!(r.len(), 4, "{r:?}");
        assert_eq!(r["acme/review"], "^1.0");
        assert_eq!(r["github:o/r/pr"], "main");
        assert_eq!(r["github:o/x"], ">=1, <2");
        assert_eq!(r["acme/b"], ">=1, <2");
        assert!(requires("---\nname: a\n---\n").unwrap().is_empty());
        assert!(requires("---\nrequires:\n---\n").unwrap().is_empty());
        assert!(requires("# none").unwrap().is_empty());
        for bad in [
            "---\nrequires: acme/a\n---\n",
            "---\nrequires: {acme/a: ^1}\n---\n",
            "---\nrequires:\n  - acme/a\n---\n",
            "---\nrequires:\n  acme/a:\n    x: y\n---\n",
            "---\nrequires:\n  acme/a:\n---\n",
            "---\nrequires:\n  acme/a: ^1\n  acme/a: ^2\n---\n",
        ] {
            assert!(requires(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn renames_frontmatter_name_only() {
        let t = "---\nname: code-review\ndescription: name: x\n---\n# name: code-review\n";
        assert_eq!(rename_frontmatter(t, "mp-review"), "---\nname: mp-review\ndescription: name: x\n---\n# name: code-review\n");
        assert_eq!(rename_frontmatter("---\r\nname: \"a\"\r\n---\r\n", "b"), "---\r\nname: b\r\n---\r\n");
        assert_eq!(rename_frontmatter("\u{feff}---\nname: a\n---\n", "b"), "\u{feff}---\nname: b\n---\n");
        let no_name = "---\ndescription: d\n---\nname: a\n";
        assert_eq!(rename_frontmatter(no_name, "b"), no_name);
        let nested = "---\nmetadata:\n  name: x\n---\n";
        assert_eq!(rename_frontmatter(nested, "b"), nested);
        assert_eq!(rename_frontmatter("# no frontmatter", "b"), "# no frontmatter");
        assert_eq!(frontmatter(&rename_frontmatter(t, "mp-review"))["name"], "mp-review");
        assert_eq!(rename_frontmatter("---\nname : one\n---\n", "b"), "---\nname: b\n---\n");
        assert_eq!(rename_frontmatter("---\nname: one\nx: y\nname: two\n---\n", "b"), "---\nname: one\nx: y\nname: b\n---\n");
        assert_eq!(rename_frontmatter("---\nname: >\n  Multi Line\nx: y\n---\n", "b"), "---\nname: b\nx: y\n---\n");
        assert_eq!(rename_frontmatter("---\nname:\n  one\n---\n", "b"), "---\nname: b\n---\n");
        assert_eq!(rename_frontmatter("---\nname: \"open\n\n---\n", "b"), "---\nname: b\n---\n", "an open quote runs to the closing ---");
        for t in ["---\nname : one\n---\n", "---\nname: one\nname: two\n---\n", "---\nname: |-\n  x\n---\n"] {
            assert_eq!(frontmatter(&rename_frontmatter(t, "b"))["name"], "b", "{t:?}");
        }
    }

    #[test]
    fn frontmatter_agrees_with_yaml() {
        // Regression: a key-looking line inside a multi-line quoted scalar was read as a key.
        for q in ['"', '\''] {
            let t = format!("---\nname: code-review\ndescription: {q}x\nname: innocent\n  y{q}\n---\n");
            let f = frontmatter(&t);
            assert_eq!(f["name"], "code-review", "{t:?}");
            assert_eq!(f["description"], "x name: innocent y");
            let r = rename_frontmatter(&t, "b");
            assert_eq!(r, t.replace("name: code-review", "name: b"));
            assert_eq!(frontmatter(&r)["description"], "x name: innocent y");
        }
        let escaped = frontmatter("---\ndescription: \"a \\\" b\nname: c\"\nname: real\n---\n");
        assert_eq!((escaped["name"].as_str(), escaped["description"].as_str()), ("real", "a \\\" b name: c"));
        let doubled = frontmatter("---\ndescription: 'it''s\nname: c'\nname: real\n---\n");
        assert_eq!((doubled["name"].as_str(), doubled["description"].as_str()), ("real", "it''s name: c"));
        // A quote still open at the closing `---` ends there: the other keys are read, it is not.
        let open = frontmatter("---\nname: a\ndescription: \"open\n---\nname: body\n");
        assert_eq!(open["name"], "a");
        assert!(!open.contains_key("description"));

        let unclosed = "---\nname: a\ndescription: d\n";
        assert!(frontmatter(unclosed).is_empty());
        assert_eq!(rename_frontmatter(unclosed, "b"), unclosed);

        let f = frontmatter("---\nname: a # c\nversion: \"1.0.0\" # c\nurl: x#y\nempty: # c\n---\n");
        assert_eq!((f["name"].as_str(), f["version"].as_str(), f["url"].as_str()), ("a", "1.0.0", "x#y"));
        assert!(!f.contains_key("empty"));
        assert_eq!(rename_frontmatter("---\nname: a # c\n---\n", "b"), "---\nname: b\n---\n");

        let f = frontmatter("---\nd1: >-\n  folded\n  text\nd2: |+\n  lit\n  eral\n\nd3: >\nname: n\n---\n");
        assert_eq!((f["d1"].as_str(), f["d2"].as_str(), f["name"].as_str()), ("folded text", "lit\neral", "n"));
        assert!(!f.contains_key("d3"));

        let f =
            frontmatter("---\ndescription: Reviews code\n  for bugs # c\n\n  and style\nname:\n  one\nmeta:\n  a: b\nlist:\n  - x\n---\n");
        assert_eq!((f["description"].as_str(), f["name"].as_str()), ("Reviews code for bugs and style", "one"));
        assert!(!f.contains_key("meta") && !f.contains_key("list"));
    }

    #[test]
    fn tree_hash_matches_definition() {
        let e = vec![FileEntry::new("b/x.sh".into(), true, b"x"), FileEntry::new("SKILL.md".into(), false, b"s")];
        let mut h = Sha256::new();
        h.update(format!("SKILL.md\0644\0{}\n", hex::encode(Sha256::digest(b"s"))));
        h.update(format!("b/x.sh\0755\0{}\n", hex::encode(Sha256::digest(b"x"))));
        assert_eq!(tree_hash(&e), hex::encode(h.finalize()));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), b"s").unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        std::fs::write(dir.path().join("b/x.sh"), b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path().join("b/x.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(tree_hash(&scan_dir(dir.path()).unwrap()), tree_hash(&e));
            std::os::unix::fs::symlink("SKILL.md", dir.path().join("l")).unwrap();
            assert!(scan_dir(dir.path()).is_err());
            std::fs::remove_file(dir.path().join("l")).unwrap();
        }
        #[cfg(not(unix))]
        assert!(scan_dir(dir.path()).unwrap().iter().all(|e| !e.exec), "no exec bit without unix modes");
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        assert!(scan_dir(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn publish_copies_a_relative_symlink_to_a_file_inside_the_skill_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill");
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("SKILL.md"), b"s").unwrap();
        std::fs::write(dir.join("d/f.md"), b"f").unwrap();
        std::fs::write(tmp.path().join("outside.md"), b"o").unwrap();
        let link = |target: &str| {
            let _ = std::fs::remove_file(dir.join("l.md"));
            std::os::unix::fs::symlink(target, dir.join("l.md")).unwrap();
        };
        link("d/f.md");
        let mut got = scan_publish(&dir).unwrap();
        got.sort_by(|a, b| a.path.cmp(&b.path));
        let f = FileEntry::new(String::new(), false, b"f").sha256;
        // (path, has f.md's content)
        let seen: Vec<(&str, bool)> = got.iter().map(|e| (e.path.as_str(), e.sha256 == f)).collect();
        assert_eq!(seen, [("SKILL.md", false), ("d/f.md", true), ("l.md", true)]);
        for bad in ["../outside.md", "d", &dir.join("d/f.md").display().to_string()] {
            link(bad);
            assert!(scan_publish(&dir).unwrap_err().msg.contains("inside the skill"), "{bad}");
        }
    }
}
