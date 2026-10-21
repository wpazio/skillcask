//! Safe zip extraction for installs and deterministic zips for `publish`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, System, ZipArchive, ZipWriter};

use crate::bail;
use crate::error::{Context, IO, Result};
use crate::skill::{FileEntry, scan_dir, scan_publish};

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub download: u64,
    pub unpacked: u64,
    pub files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { download: 50 << 20, unpacked: 100 << 20, files: 5000 }
    }
}

struct Entry {
    index: usize,
    raw: String,
    parts: Vec<String>,
    dir: bool,
    link: bool,
    exec: bool,
}

pub struct Extracted {
    pub files: Vec<FileEntry>,
    pub links: Vec<Link>,
    /// Not written: names some OS cannot hold or would fold into one. Fatal only to a skill
    /// that contains one, so a stray file elsewhere in a repository does not block its skills.
    pub skipped: Vec<Skipped>,
}

pub struct Skipped {
    pub path: String,
    /// The error a skill containing it fails with.
    pub why: String,
}

/// A symlink in the archive. Never written as a link: when it points to a regular file in the
/// archive it is written as a copy of that file (`CLAUDE.md -> AGENTS.md` is common in skills).
pub struct Link {
    pub path: String,
    /// Normalised target, set only when it is a regular file in the archive.
    pub target: Option<String>,
}

/// Extracts into `dest` (an existing empty directory); paths are relative to the stripped root.
pub fn extract(zip: &[u8], dest: &Path, limits: &Limits) -> Result<Extracted> {
    let mut archive = ZipArchive::new(Cursor::new(zip))?;
    // Duplicate names (the last one wins) and local names that differ from the central
    // directory make other tools show other files than the ones installed.
    archive.validate_header_consistency()?;
    let mut entries = Vec::new();
    let (mut files, mut declared) = (0usize, 0u64);
    for index in 0..archive.len() {
        let f = archive.by_index(index)?;
        let raw = f.name()?.to_string();
        if raw.starts_with('/') || raw.contains(['\\', ':', '\0']) {
            bail!(IO, "archive: unsafe entry name {raw:?}");
        }
        let parts: Vec<String> = raw.split('/').filter(|p| !p.is_empty() && *p != ".").map(String::from).collect();
        if parts.iter().any(|p| p == "..") {
            bail!(IO, "archive: unsafe entry name {raw:?}");
        }
        let mode = f.unix_mode().unwrap_or(0);
        let kind = mode & S_IFMT;
        let dir = f.is_dir();
        let link = f.is_symlink() && !dir;
        if !(link || kind == 0 || kind == S_IFREG || dir && kind == S_IFDIR) {
            bail!(IO, "archive: {raw:?} is not a regular file or directory");
        }
        if parts.is_empty() {
            continue;
        }
        if !dir {
            files += 1;
            declared = declared.saturating_add(f.size());
            if files > limits.files {
                bail!(IO, "archive: more than {} files", limits.files);
            }
            if declared > limits.unpacked {
                bail!(IO, "archive: unpacked size exceeds {} bytes", limits.unpacked);
            }
        }
        entries.push(Entry { index, raw, parts, dir, link, exec: mode & 0o111 != 0 });
    }

    let top = entries.first().map(|e| e.parts[0].clone());
    let strip = top.is_some_and(|t| entries.iter().all(|e| e.parts[0] == t && (e.parts.len() > 1 || e.dir)));
    if strip {
        for e in &mut entries {
            e.parts.remove(0);
        }
        entries.retain(|e| !e.parts.is_empty());
    }
    let mut why: HashMap<usize, String> = HashMap::new();
    for e in entries.iter().filter(|e| !e.dir) {
        if let Some(p) = e.parts.iter().find(|p| !portable(p)) {
            why.insert(e.index, format!("archive: {:?}: {p:?} is not a valid file name on every OS", e.raw));
        }
    }
    why.extend(collisions(&entries, &why)?);

    let mut ancestors = BTreeSet::new();
    for e in entries.iter().filter(|e| !e.dir) {
        for i in 1..e.parts.len() {
            ancestors.insert(e.parts[..i].join("/"));
        }
    }
    if let Some(e) = entries.iter().find(|e| e.dir && !ancestors.contains(&e.parts.join("/"))) {
        bail!(IO, "archive: empty directory {:?}", e.parts.join("/"));
    }

    let (bad, entries): (Vec<&Entry>, Vec<&Entry>) = entries.iter().partition(|e| why.contains_key(&e.index));
    let skipped = bad.iter().filter_map(|e| why.remove(&e.index).map(|why| Skipped { path: e.parts.join("/"), why })).collect();
    let mut out = Vec::new();
    let mut written = 0u64;
    for e in entries.iter().filter(|e| !e.dir && !e.link) {
        let rel = e.parts.join("/");
        let mut content = Vec::new();
        archive.by_index(e.index)?.take(limits.unpacked - written + 1).read_to_end(&mut content)?;
        charge(&mut written, content.len(), limits)?;
        write_new(dest, &rel, &content, e.exec)?;
        out.push(FileEntry::new(rel, e.exec, &content));
    }

    let mut pending = Vec::new();
    for e in entries.iter().filter(|e| e.link) {
        let mut raw = String::new();
        archive.by_index(e.index)?.take(4096).read_to_string(&mut raw)?;
        pending.push((e.parts.join("/"), resolve(&e.parts[..e.parts.len() - 1], &raw)));
    }
    // A link may point to another link, in any entry order: copy whatever resolves until a
    // round makes no progress.
    let mut index: HashMap<String, usize> = out.iter().enumerate().map(|(i, f)| (f.path.clone(), i)).collect();
    let mut links = Vec::new();
    loop {
        let before = pending.len();
        let mut rest = Vec::new();
        for (rel, target) in pending {
            let Some(&i) = target.as_ref().and_then(|t| index.get(t)) else {
                rest.push((rel, target));
                continue;
            };
            let src = out[i].clone();
            let content = std::fs::read(dest.join(&src.path)).ctx(format!("archive: {:?}", src.path))?;
            charge(&mut written, content.len(), limits)?;
            write_new(dest, &rel, &content, src.exec)?;
            index.insert(rel.clone(), out.len());
            out.push(FileEntry { path: rel.clone(), ..src });
            links.push(Link { path: rel, target });
        }
        pending = rest;
        if pending.len() == before {
            break;
        }
    }
    links.extend(pending.into_iter().map(|(path, _)| Link { path, target: None }));

    // Whatever the name checks miss (a file system folding names in some other way) shows up
    // here as fewer or different paths than the hashes cover. HFS+ stores names decomposed (NFD),
    // so both sides compare in NFC; `collisions` already refused names equal in NFC.
    let nfc = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    let on_disk: BTreeSet<String> = scan_dir(dest)?.into_iter().map(|f| nfc.normalize(&f.path).into_owned()).collect();
    if on_disk != out.iter().map(|f| nfc.normalize(&f.path).into_owned()).collect() {
        bail!(IO, "archive: the extracted files differ from the archive entries");
    }
    Ok(Extracted { files: out, links, skipped })
}

fn charge(written: &mut u64, n: usize, limits: &Limits) -> Result<()> {
    *written += n as u64;
    if *written > limits.unpacked {
        bail!(IO, "archive: unpacked size exceeds {} bytes", limits.unpacked);
    }
    Ok(())
}

fn write_new(dest: &Path, rel: &str, content: &[u8], exec: bool) -> Result<()> {
    let path = dest.join(rel);
    let io = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::File::create_new(&path)?.write_all(content)?;
        set_mode(&path, exec)
    };
    io().ctx(format!("archive: {rel:?}"))
}

/// Not a name Windows refuses or rewrites: reserved device names, a trailing dot or space,
/// `<>:"|?*\` and control characters.
fn portable(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ').to_ascii_uppercase();
    let port = stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT"));
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || port.is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}"));
    !(reserved || name.ends_with(['.', ' ']) || name.contains(|c: char| c.is_control() || "<>:\"|?*\\".contains(c)))
}

/// Entries (by archive index) distinct here but one file on a case-insensitive or normalising
/// file system (macOS, Windows), so they are skipped the same way on every OS. `skip`: entries
/// already skipped, which are never written and so collide with nothing.
fn collisions(entries: &[Entry], skip: &HashMap<usize, String>) -> Result<HashMap<usize, String>> {
    let nfc = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    // Upper then lower also folds letters whose lowercase alone stays distinct (`ſ`, `K`).
    let fold = |p: &String| nfc.normalize(&p.to_uppercase().to_lowercase()).into_owned();
    let files: Vec<(&Entry, Vec<String>)> =
        entries.iter().filter(|e| !e.dir && !skip.contains_key(&e.index)).map(|e| (e, e.parts.iter().map(fold).collect())).collect();
    // The first spelling of each folded path prefix, and the error of the first entry spelling it
    // otherwise: every entry of the group gets that one, the text a whole-archive check gave.
    let mut first: HashMap<String, String> = HashMap::new();
    let mut group: HashMap<String, String> = HashMap::new();
    let mut seen = HashSet::new();
    for (e, folded) in &files {
        if !seen.insert(e.parts.join("/")) {
            bail!(IO, "archive: duplicate entry {:?}", e.parts.join("/"));
        }
        for n in 1..=e.parts.len() {
            let key = folded[..n].join("/");
            let other = first.entry(key.clone()).or_insert_with(|| e.parts[..n].join("/"));
            if *other != e.parts[..n].join("/") {
                let msg = format!("archive: {:?} and {other:?} are one name on case-insensitive file systems", e.parts.join("/"));
                group.entry(key).or_insert(msg);
                break;
            }
        }
    }
    let mut out = HashMap::new();
    for (e, folded) in &files {
        if let Some(msg) = (1..=e.parts.len()).find_map(|n| group.get(&folded[..n].join("/"))) {
            out.insert(e.index, msg.clone());
        }
    }
    Ok(out)
}

/// Resolves a relative link target against the link's directory; `None` if it leaves the archive.
fn resolve(dir: &[String], target: &str) -> Option<String> {
    if target.starts_with('/') || target.contains(['\\', ':', '\0']) {
        return None;
    }
    let mut parts: Vec<&str> = dir.iter().map(String::as_str).collect();
    for p in target.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// A symlink inside `dir` (`""` = the whole package) that is not a copy of a file in that same
/// directory: those are rejected.
pub fn bad_link_in<'a>(links: &'a [Link], dir: &str) -> Option<&'a Link> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{}/", dir.trim_end_matches('/')) };
    links.iter().filter(|l| l.path.starts_with(&prefix)).find(|l| !l.target.as_ref().is_some_and(|t| t.starts_with(&prefix)))
}

#[cfg(unix)]
fn set_mode(path: &Path, exec: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))
}

#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps, reason = "mirrors the unix signature; the modes file carries exec bits there")]
fn set_mode(_: &Path, _: bool) -> std::io::Result<()> {
    Ok(())
}

/// Entries under `dir` (`""` = all) with the prefix removed.
pub fn subtree(entries: &[FileEntry], dir: &str) -> Vec<FileEntry> {
    if dir.is_empty() {
        return entries.to_vec();
    }
    let prefix = format!("{}/", dir.trim_end_matches('/'));
    entries.iter().filter_map(|e| e.path.strip_prefix(&prefix).map(|p| FileEntry { path: p.to_string(), ..e.clone() })).collect()
}

/// Same tree, same bytes: files only, sorted, fixed mtime and permissions. Also returns the
/// zipped files.
pub fn zip_dir(dir: &Path) -> Result<(Vec<u8>, Vec<FileEntry>)> {
    zip_entries(dir, scan_publish(dir)?)
}

/// The zip skillcask 0.1.2 published: hidden files included, symlinks refused.
pub fn zip_dir_0_1_2(dir: &Path) -> Result<Vec<u8>> {
    Ok(zip_entries(dir, scan_dir(dir)?)?.0)
}

fn zip_entries(dir: &Path, mut entries: Vec<FileEntry>) -> Result<(Vec<u8>, Vec<FileEntry>)> {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    for e in &entries {
        let opts = SimpleFileOptions::DEFAULT
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(6))
            .system(System::Unix)
            .unix_permissions(if e.exec { 0o755 } else { 0o644 });
        w.start_file(e.path.as_str(), opts)?;
        w.write_all(&std::fs::read(dir.join(&e.path)).ctx(format!("{:?}", e.path))?)?;
    }
    Ok((w.finish()?.into_inner(), entries))
}

/// Sets a file's mtime to a fixed past time, so tests see a later write (or two trees with
/// different mtimes) without sleeping past the file system's timestamp resolution. Files only:
/// the directory-capable twin lives in `tests/common`, which unit tests cannot see.
#[cfg(test)]
pub(crate) fn backdate(path: &Path) {
    let past = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    // Windows needs write access to set times.
    std::fs::File::options().read(true).write(cfg!(windows)).open(path).unwrap().set_modified(past).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::tree_hash;

    enum Item<'a> {
        File(&'a str, u32, &'a [u8]),
        Dir(&'a str),
        Link(&'a str, &'a str),
    }

    fn build(items: &[Item]) -> Vec<u8> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        for it in items {
            match it {
                Item::File(n, mode, c) => {
                    w.start_file(*n, SimpleFileOptions::DEFAULT.system(System::Unix).unix_permissions(*mode)).unwrap();
                    w.write_all(c).unwrap();
                }
                Item::Dir(n) => w.add_directory(*n, SimpleFileOptions::DEFAULT.system(System::Unix)).unwrap(),
                Item::Link(n, t) => w.add_symlink(*n, *t, SimpleFileOptions::DEFAULT.system(System::Unix)).unwrap(),
            }
        }
        w.finish().unwrap().into_inner()
    }

    fn try_extract(items: &[Item], limits: Limits) -> Result<Vec<FileEntry>> {
        let dir = tempfile::tempdir().unwrap();
        extract(&build(items), dir.path(), &limits).map(|e| e.files)
    }

    #[test]
    fn rejects_attacks() {
        for name in ["../x", "/abs", "a/../../x", "a/..", "c:/x", "a\\..\\x"] {
            let r = try_extract(&[Item::File("ok", 0o644, b"1"), Item::File(name, 0o644, b"x")], Limits::default());
            assert!(r.is_err(), "{name} accepted");
        }
        let dir = tempfile::tempdir().unwrap();
        let items = [
            Item::File("s/SKILL.md", 0o644, b"x"),
            Item::File("s/AGENTS.md", 0o755, b"agents"),
            Item::Link("s/CLAUDE.md", "AGENTS.md"),
            Item::Link("s/evil", "/etc/passwd"),
            Item::Link("t/out", "../s/AGENTS.md"),
            Item::Link("u/esc", "../../x"),
        ];
        let e = extract(&build(&items), dir.path(), &Limits::default()).unwrap();
        let claude = dir.path().join("s/CLAUDE.md");
        assert!(!claude.symlink_metadata().unwrap().file_type().is_symlink(), "links are written as copies");
        assert_eq!(std::fs::read(&claude).unwrap(), b"agents");
        assert!(e.files.iter().any(|f| f.path == "s/CLAUDE.md" && f.exec));
        assert!(!dir.path().join("s/evil").exists() && !dir.path().join("u/esc").exists());
        assert_eq!(bad_link_in(&e.links, "s").map(|l| l.path.as_str()), Some("s/evil"));
        assert_eq!(bad_link_in(&e.links, "t").map(|l| l.path.as_str()), Some("t/out"), "target outside the skill");
        assert!(bad_link_in(&e.links, "other").is_none());
        assert!(bad_link_in(&e.links[..1], "s").is_none());
        let many = [Item::File("a", 0o644, b"1"), Item::File("b", 0o644, b"2"), Item::File("c", 0o644, b"3")];
        assert!(try_extract(&many, Limits { files: 2, ..Limits::default() }).is_err());
        assert!(try_extract(&many, Limits { files: 3, ..Limits::default() }).is_ok());
        let big = vec![0u8; 1000];
        assert!(try_extract(&[Item::File("a", 0o644, &big)], Limits { unpacked: 999, ..Limits::default() }).is_err());
        assert!(try_extract(&[Item::File("a", 0o644, b"x"), Item::Dir("empty/")], Limits::default()).is_err());
        assert!(try_extract(&[Item::File("a", 0o644, b"x"), Item::File("./a", 0o644, b"y")], Limits::default()).is_err());
    }

    /// The skipped entries as (path, why); checks they are not on disk and the rest is.
    fn skipped(items: &[Item]) -> Vec<(String, String)> {
        let dir = tempfile::tempdir().unwrap();
        let x = extract(&build(items), dir.path(), &Limits::default()).unwrap();
        let on_disk: Vec<String> = scan_dir(dir.path()).unwrap().into_iter().map(|f| f.path).collect();
        assert_eq!(on_disk.len(), x.files.len());
        assert!(x.skipped.iter().all(|s| !on_disk.contains(&s.path)));
        x.skipped.into_iter().map(|s| (s.path, s.why)).collect()
    }

    fn paths(skipped: &[(String, String)]) -> Vec<&str> {
        skipped.iter().map(|(p, _)| p.as_str()).collect()
    }

    /// Patches every occurrence (`all`) or only the first one (the local header) of `from`.
    fn patch(mut zip: Vec<u8>, from: &[u8], to: &[u8], all: bool) -> Vec<u8> {
        assert_eq!(from.len(), to.len());
        let mut i = 0;
        while let Some(at) = zip[i..].windows(from.len()).position(|w| w == from) {
            zip[i + at..i + at + to.len()].copy_from_slice(to);
            i += at + to.len();
            if !all {
                break;
            }
        }
        zip
    }

    fn central(zip: &mut [u8], n: usize) -> &mut [u8] {
        let at = zip.windows(4).enumerate().filter(|(_, w)| *w == b"PK\x01\x02").nth(n).unwrap().0;
        &mut zip[at..]
    }

    #[test]
    fn rejects_duplicate_and_mismatched_names() {
        let zip = build(&[Item::File("s/SKILL.md", 0o644, b"shown"), Item::File("s/SKILL.mX", 0o644, b"installed")]);
        let dir = tempfile::tempdir().unwrap();
        assert!(extract(&patch(zip.clone(), b"SKILL.mX", b"SKILL.md", true), dir.path(), &Limits::default()).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(extract(&patch(zip, b"SKILL.mX", b"SKILL.md", false), dir.path(), &Limits::default()).is_err());
    }

    #[test]
    fn skips_names_that_fold_together() {
        let s = skipped(&[Item::File("skills/a/SKILL.md", 0o644, b"s"), Item::File("Skills/a/evil.sh", 0o755, b"x")]);
        assert_eq!(paths(&s), ["skills/a/SKILL.md", "Skills/a/evil.sh"]);
        assert!(s[1].1.contains("\"Skills/a/evil.sh\" and \"skills\" are one name"), "{s:?}");
        assert_eq!(skipped(&[Item::File("caf\u{e9}/SKILL.md", 0o644, b"s"), Item::File("cafe\u{301}/x", 0o644, b"x")]).len(), 2);
        assert_eq!(skipped(&[Item::File("a/SKILL.md", 0o644, b"s"), Item::File("A/SKILL.md", 0o644, b"x")]).len(), 2);
        assert_eq!(
            paths(&skipped(&[Item::File("a/x", 0o644, b"s"), Item::File("a/x.", 0o644, b"x")])),
            ["x."],
            "the single top dir is stripped"
        );
        assert_eq!(skipped(&[Item::File("a/x", 0o644, b"s"), Item::Link("a/X", "x")]).len(), 2);
        let s = skipped(&[
            Item::File("src/README.md", 0o644, b"1"),
            Item::File("src/readme.md", 0o644, b"2"),
            Item::File("src/x", 0o644, b"3"),
            Item::File("SKILL.md", 0o644, b"4"),
        ]);
        assert_eq!(paths(&s), ["src/README.md", "src/readme.md"], "only the colliding entries");
        let ok = [Item::File("a/b", 0o644, b"1"), Item::File("a/c", 0o644, b"2"), Item::File("caf\u{e9}", 0o644, b"3")];
        assert_eq!(try_extract(&ok, Limits::default()).unwrap().len(), 3);
    }

    #[test]
    fn skips_names_windows_cannot_hold() {
        for name in
            ["CON", "con.txt", "a/Nul", "LPT1.md", "com9", "aux.tar.gz", "x ", "x.", "a<b", "a>b", "a|b", "a?b", "a*b", "a\"b", "a\tb"]
        {
            let s = skipped(&[Item::File("ok", 0o644, b"1"), Item::File(name, 0o644, b"x")]);
            assert_eq!(paths(&s), [name]);
            assert!(s[0].1.contains(&format!("{name:?}")), "{name:?}: {s:?}");
        }
        let s = skipped(&[Item::File("ok", 0o644, b"1"), Item::File("\x1b[31mred", 0o644, b"x")]);
        assert!(!s[0].1.contains('\x1b'), "{s:?}");
        let ok = [Item::File("console.md", 0o644, b"1"), Item::File("com10", 0o644, b"2"), Item::File("a.b/c", 0o644, b"3")];
        assert!(try_extract(&ok, Limits::default()).is_ok());
    }

    #[test]
    fn rejects_special_modes_and_size_lies() {
        for mode in [0o060644u32, 0o010644, 0o020644] {
            let mut zip = build(&[Item::File("a", 0o644, b"x")]);
            central(&mut zip, 0)[38..42].copy_from_slice(&(mode << 16).to_le_bytes());
            let dir = tempfile::tempdir().unwrap();
            assert!(extract(&zip, dir.path(), &Limits::default()).is_err(), "{mode:o}");
        }
        let mut zip = build(&[Item::File("a", 0o644, &[0u8; 2000])]);
        central(&mut zip, 0)[24..28].copy_from_slice(&1u32.to_le_bytes());
        zip[22..26].copy_from_slice(&1u32.to_le_bytes());
        let dir = tempfile::tempdir().unwrap();
        // The zip crate stops at the declared size, so the lie is an error before our own budget check.
        assert!(extract(&zip, dir.path(), &Limits { unpacked: 1000, ..Limits::default() }).is_err());
        assert!(!dir.path().join("a").exists());
    }

    #[test]
    fn links_count_toward_limits() {
        let items = [Item::File("a", 0o644, &[0u8; 600]), Item::Link("b", "a")];
        assert!(try_extract(&items, Limits::default()).is_ok());
        assert!(try_extract(&items, Limits { files: 1, ..Limits::default() }).is_err());
        let e = try_extract(&items, Limits { unpacked: 1000, ..Limits::default() }).err().unwrap().msg;
        assert!(e.contains("unpacked size exceeds"), "{e}");
    }

    #[test]
    fn resolves_links_to_links_in_any_order() {
        for reverse in [false, true] {
            let mut items = vec![Item::Link("CLAUDE.md", "GEMINI.md"), Item::Link("GEMINI.md", "AGENTS.md")];
            if reverse {
                items.reverse();
            }
            items.insert(1, Item::File("AGENTS.md", 0o644, b"agents"));
            let dir = tempfile::tempdir().unwrap();
            let x = extract(&build(&items), dir.path(), &Limits::default()).unwrap();
            assert_eq!(std::fs::read(dir.path().join("CLAUDE.md")).unwrap(), b"agents", "{reverse}");
            assert!(bad_link_in(&x.links, "").is_none(), "{reverse}");
            assert_eq!(x.files.len(), 3);
        }
        let cycle = [Item::File("x", 0o644, b"x"), Item::Link("a", "b"), Item::Link("b", "a")];
        let dir = tempfile::tempdir().unwrap();
        let x = extract(&build(&cycle), dir.path(), &Limits::default()).unwrap();
        assert!(bad_link_in(&x.links, "").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn owner_exec_only_installs_as_755() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let x = extract(&build(&[Item::File("run", 0o100100, b"x")]), dir.path(), &Limits::default()).unwrap();
        assert!(x.files[0].exec);
        assert_eq!(std::fs::metadata(dir.path().join("run")).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn strips_single_top_dir_only() {
        let e = try_extract(
            &[Item::Dir("repo-abc/"), Item::File("repo-abc/SKILL.md", 0o644, b"s"), Item::File("repo-abc/bin/x", 0o755, b"x")],
            Limits::default(),
        )
        .unwrap();
        let mut paths: Vec<_> = e.iter().map(|e| (e.path.as_str(), e.exec)).collect();
        paths.sort_unstable();
        assert_eq!(paths, [("SKILL.md", false), ("bin/x", true)]);
        let e = try_extract(&[Item::File("top/a", 0o644, b"1"), Item::File("b", 0o644, b"2")], Limits::default()).unwrap();
        assert!(e.iter().any(|e| e.path == "top/a"));
        assert_eq!(subtree(&e, "top")[0].path, "a");
        assert_eq!(subtree(&e, "").len(), 2);
    }

    #[test]
    fn zip_dir_skips_hidden_entries() {
        let a = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("SKILL.md"), b"s").unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("SKILL.md"), b"s").unwrap();
        std::fs::write(b.path().join(".DS_Store"), b"x").unwrap();
        std::fs::write(b.path().join(".env"), b"SECRET=1").unwrap();
        std::fs::create_dir_all(b.path().join("sub/.git")).unwrap();
        std::fs::write(b.path().join("sub/.git/HEAD"), b"x").unwrap();
        assert_eq!(zip_dir(a.path()).unwrap().0, zip_dir(b.path()).unwrap().0);
    }

    #[cfg(unix)]
    #[test]
    fn zip_dir_copies_links_inside_the_skill() {
        let copy = tempfile::tempdir().unwrap();
        let linked = tempfile::tempdir().unwrap();
        for d in [copy.path(), linked.path()] {
            std::fs::write(d.join("SKILL.md"), b"s").unwrap();
            std::fs::write(d.join("AGENTS.md"), b"agents").unwrap();
        }
        std::fs::write(copy.path().join("CLAUDE.md"), b"agents").unwrap();
        std::os::unix::fs::symlink("AGENTS.md", linked.path().join("CLAUDE.md")).unwrap();
        assert_eq!(zip_dir(copy.path()).unwrap().0, zip_dir(linked.path()).unwrap().0);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("x"), linked.path().join("abs")).unwrap();
        let e = zip_dir(linked.path()).err().unwrap().msg;
        assert!(e.contains("\"abs\"") && e.contains("inside the skill"), "{e}");
    }

    #[test]
    fn zip_dir_is_deterministic_and_round_trips() {
        let make = || {
            let d = tempfile::tempdir().unwrap();
            std::fs::write(d.path().join("SKILL.md"), b"---\nname: x\n---\n").unwrap();
            std::fs::create_dir(d.path().join("scripts")).unwrap();
            std::fs::write(d.path().join("scripts/run.sh"), b"#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(d.path().join("scripts/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            d
        };
        let a = make();
        backdate(&a.path().join("SKILL.md"));
        backdate(&a.path().join("scripts/run.sh"));
        let b = make();
        let za = zip_dir(a.path()).unwrap().0;
        assert_eq!(za, zip_dir(b.path()).unwrap().0);
        let out = tempfile::tempdir().unwrap();
        let entries = extract(&za, out.path(), &Limits::default()).unwrap().files;
        assert_eq!(tree_hash(&entries), tree_hash(&scan_dir(a.path()).unwrap()));
        assert_eq!(tree_hash(&scan_dir(out.path()).unwrap()), tree_hash(&entries));
        #[cfg(unix)]
        assert!(entries.iter().any(|e| e.path == "scripts/run.sh" && e.exec));
    }
}
