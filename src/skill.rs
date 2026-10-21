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

/// Problems with a SKILL.md under the Agent Skills specification (agentskills.io) for a skill in
/// the directory `dir_name`. Stricter than `valid_name`, which still installs names such as `a--b`.
pub fn spec_problems(text: &str, dir_name: &str) -> Vec<String> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    if fields(&body.split_inclusive('\n').collect::<Vec<_>>()).is_none() {
        return vec![format!("{SKILL_MD} has no frontmatter (--- ... ---)")];
    }
    let fm = frontmatter(text);
    let mut out = Vec::new();
    match fm.get("name") {
        None => out.push("name is required".into()),
        Some(n) if n.len() > 64 || !valid_name(n) || n.ends_with('-') || n.contains("--") => {
            out.push(format!("name {n:?} must be 1-64 lowercase letters, digits and single hyphens, not starting or ending with a hyphen"));
        }
        Some(n) if n != dir_name => out.push(format!("name {n:?} does not match the directory name {dir_name:?}")),
        Some(_) => {}
    }
    let mut length = |key: &str, max: usize, required: bool| match fm.get(key).map(|v| v.chars().count()) {
        None if required => out.push(format!("{key} is required")),
        Some(n) if n > max => out.push(format!("{key} has {n} characters, at most {max}")),
        _ => {}
    };
    length("description", 1024, true);
    length("compatibility", 500, false);
    out
}

/// Most invisible characters reported per file; the rest are counted.
const INVISIBLE_SHOWN: usize = 5;

/// An invisible or reordering character: the agent reads text a reviewer does not see.
#[derive(Debug, PartialEq)]
pub struct Invisible {
    /// 1-based.
    pub line: usize,
    pub ch: char,
    pub name: &'static str,
}

fn invisible_name(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{061C}' => "ARABIC LETTER MARK",
        '\u{200B}' => "ZERO WIDTH SPACE",
        '\u{200C}' => "ZERO WIDTH NON-JOINER",
        '\u{200D}' => "ZERO WIDTH JOINER",
        '\u{200E}' => "LEFT-TO-RIGHT MARK",
        '\u{200F}' => "RIGHT-TO-LEFT MARK",
        '\u{202A}' => "LEFT-TO-RIGHT EMBEDDING",
        '\u{202B}' => "RIGHT-TO-LEFT EMBEDDING",
        '\u{202C}' => "POP DIRECTIONAL FORMATTING",
        '\u{202D}' => "LEFT-TO-RIGHT OVERRIDE",
        '\u{202E}' => "RIGHT-TO-LEFT OVERRIDE",
        '\u{2060}' => "WORD JOINER",
        '\u{2066}' => "LEFT-TO-RIGHT ISOLATE",
        '\u{2067}' => "RIGHT-TO-LEFT ISOLATE",
        '\u{2068}' => "FIRST STRONG ISOLATE",
        '\u{2069}' => "POP DIRECTIONAL ISOLATE",
        '\u{FEFF}' => "ZERO WIDTH NO-BREAK SPACE",
        '\u{E0001}' => "LANGUAGE TAG",
        '\u{E007F}' => "CANCEL TAG",
        '\u{E0000}'..='\u{E007E}' => "TAG CHARACTER",
        _ => return None,
    })
}

/// Bidi controls, zero-width characters and Unicode tags in `text`, except a leading byte order
/// mark and a zero width joiner between two emoji (`👩‍💻`).
pub fn invisible(text: &str) -> Vec<Invisible> {
    let emoji = |c: Option<char>| c.is_some_and(|c| matches!(c, '\u{2600}'..='\u{27BF}' | '\u{FE0F}' | '\u{1F000}'..='\u{1FAFF}'));
    let mut out = Vec::new();
    let mut line = 1;
    let mut prev = None;
    let mut chars = text.strip_prefix('\u{feff}').unwrap_or(text).chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            line += 1;
        } else if let Some(name) = invisible_name(c)
            && !(c == '\u{200D}' && emoji(prev) && emoji(chars.peek().copied()))
        {
            out.push(Invisible { line, ch: c, name });
        }
        prev = Some(c);
    }
    out
}

/// `<path>:<line>: U+XXXX NAME` per invisible character in a file's `bytes`, a few per file, then
/// a count of the rest. Not UTF-8 means binary: nothing.
pub fn invisible_warnings(path: &str, bytes: &[u8]) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(bytes) else { return Vec::new() };
    let found = invisible(text);
    let mut out: Vec<String> =
        found.iter().take(INVISIBLE_SHOWN).map(|i| format!("{}:{}: U+{:04X} {}", shown(path), i.line, u32::from(i.ch), i.name)).collect();
    if found.len() > INVISIBLE_SHOWN {
        out.push(format!("{}: {} more invisible characters", shown(path), found.len() - INVISIBLE_SHOWN));
    }
    out
}

/// Package text for the terminal: control characters (except tab) and the characters `invisible`
/// reports become `<U+XXXX>`, so they are seen instead of moving the cursor or reordering the line.
pub fn shown(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if (c.is_control() && c != '\t') || invisible_name(c).is_some() {
            let _ = write!(out, "<U+{:04X}>", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
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
    fn finds_invisible_characters() {
        let found = |t: &str| invisible(t).into_iter().map(|i| (i.line, u32::from(i.ch))).collect::<Vec<_>>();
        assert_eq!(found("a\r\nb\u{202E}c\r\n\u{200B}"), [(2, 0x202E), (3, 0x200B)], "CRLF counts one line");
        assert_eq!(found("\u{feff}x\u{feff}"), [(1, 0xFEFF)], "only a leading BOM is fine");
        assert_eq!(found("\u{1F469}\u{200D}\u{1F4BB} \u{2764}\u{FE0F}\u{200D}\u{1F525}"), [], "emoji ZWJ sequences");
        assert_eq!(found("a\u{200D}b \u{1F469}\u{200D}"), [(1, 0x200D), (1, 0x200D)]);
        assert_eq!(found("\u{E0001}\u{E0041}\u{E007F}\u{E0080}"), [(1, 0xE0001), (1, 0xE0041), (1, 0xE007F)]);
        for c in ['\u{061C}', '\u{200C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202D}', '\u{2060}', '\u{2066}', '\u{2069}'] {
            assert_eq!(found(&c.to_string()).len(), 1, "{c:?}");
        }
        assert_eq!(found("zażółć ‘quotes’ — dash\t\u{00A0}"), Vec::<(usize, u32)>::new());

        assert_eq!(invisible_warnings("s/SKILL.md", "x\n\u{202E}".as_bytes()), ["s/SKILL.md:2: U+202E RIGHT-TO-LEFT OVERRIDE"]);
        assert!(invisible_warnings("a.bin", b"\xe2\x80\xae\xff").is_empty(), "binary files are skipped");
        assert_eq!(invisible_warnings("a\u{202E}b.md", "\u{200B}".as_bytes()), ["a<U+202E>b.md:1: U+200B ZERO WIDTH SPACE"]);
        assert_eq!(shown("x\u{1b}]0;t\u{7}\ty\u{E0041}"), "x<U+001B>]0;t<U+0007>\ty<U+E0041>");
        let w = invisible_warnings("f", "\u{200B}".repeat(8).as_bytes());
        assert_eq!((w.len(), w[5].as_str()), (6, "f: 3 more invisible characters"));
    }

    #[test]
    fn checks_the_agent_skills_spec() {
        let md = |fm: &str| format!("---\n{fm}---\nbody\n");
        assert_eq!(spec_problems(&md("name: a-1\ndescription: d\ncompatibility: git\n"), "a-1"), Vec::<String>::new());
        assert_eq!(spec_problems(&format!("\u{feff}{}", md("name: a\ndescription: d\n")), "a"), Vec::<String>::new());
        assert_eq!(spec_problems("# none", "a"), ["SKILL.md has no frontmatter (--- ... ---)"]);
        assert_eq!(spec_problems(&md("x: y\n"), "a"), ["name is required", "description is required"]);
        for bad in ["A", "-a", "a-", "a--b", "a_b", &"a".repeat(65)] {
            let p = spec_problems(&md(&format!("name: {bad}\ndescription: d\n")), bad);
            assert!(p.len() == 1 && p[0].contains("must be 1-64"), "{bad}: {p:?}");
        }
        assert_eq!(spec_problems(&md(&format!("name: {}\ndescription: d\n", "a".repeat(64))), &"a".repeat(64)), Vec::<String>::new());
        assert_eq!(spec_problems(&md("name: a\ndescription: d\n"), "b"), ["name \"a\" does not match the directory name \"b\""]);
        let long = md(&format!("name: a\ndescription: {}\ncompatibility: {}\n", "ł".repeat(1025), "c".repeat(501)));
        assert_eq!(
            spec_problems(&long, "a"),
            ["description has 1025 characters, at most 1024", "compatibility has 501 characters, at most 500"]
        );
        assert!(spec_problems(&md(&format!("name: a\ndescription: {}\n", "ł".repeat(1024))), "a").is_empty(), "characters, not bytes");
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
