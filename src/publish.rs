//! `skillcask publish`: a static Composer repository from every versioned SKILL.md in a monorepo,
//! and `skillcask validate`, which checks the same SKILL.md files against the Agent Skills spec.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::bail;
use crate::error::{Context, Error, MANIFEST, Result};
use crate::manifest::valid_package_name;
use crate::skill::{FileEntry, SKILL_MD, frontmatter, tree_hash, valid_name};
use crate::version::{Stability, Version};

/// Tree hash of the published files, so a version stays verifiable without its zip.
const TREE: &str = "ai-agent-skill-tree";

pub struct PublishArgs {
    pub vendor: String,
    pub base_url: Option<String>,
    /// Dist URL with `{vendor}`, `{name}` and `{version}`; default `<base-url>/dist/{vendor}/{name}/{version}.zip`.
    pub dist_url: Option<String>,
    /// Write every package into `packages.json` (Composer v1) instead of `metadata-url` + `p2/`.
    pub inline: bool,
    pub out: PathBuf,
    pub paths: Vec<PathBuf>,
}

struct Found {
    dir: PathBuf,
    version: String,
    normalized: Version,
    description: Option<String>,
    abandoned: Option<Value>,
    zip: Vec<u8>,
    files: Vec<FileEntry>,
}

pub fn publish(args: &PublishArgs) -> Result<()> {
    let base = args.base_url.as_deref().map(|b| b.trim_end_matches('/'));
    if base.is_some_and(|b| !b.starts_with("https://")) {
        bail!(MANIFEST, "--base-url must be an https:// URL");
    }
    if base.is_none() && !(args.inline && args.dist_url.is_some()) {
        bail!(MANIFEST, "--base-url is required unless both --inline and --dist-url are given");
    }
    let dist_url = match (&args.dist_url, base) {
        (Some(t), _) => t.clone(),
        (None, Some(b)) => format!("{b}/dist/{{vendor}}/{{name}}/{{version}}.zip"),
        (None, None) => unreachable!("checked above"),
    };
    if !dist_url.starts_with("https://") || !dist_url.contains("{name}") || !dist_url.contains("{version}") {
        bail!(MANIFEST, "--dist-url must be an https:// URL containing {{name}} and {{version}}");
    }
    std::fs::create_dir_all(&args.out).ctx(args.out.display())?;
    let out = args.out.canonicalize().ctx(args.out.display())?;

    let roots = if args.paths.is_empty() { vec![PathBuf::from(".")] } else { args.paths.clone() };
    let mut dirs = Vec::new();
    for r in &roots {
        find(r, Some(&out), &mut dirs)?;
    }

    let mut found: BTreeMap<String, Vec<Found>> = BTreeMap::new();
    let mut origin: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in dirs {
        let md = dir.join(SKILL_MD);
        let text = std::fs::read_to_string(&md).ctx(md.display())?;
        let fm = frontmatter(&text);
        let (Some(name), Some(version)) = (fm.get("name"), fm.get("version")) else {
            let missing = match (fm.contains_key("name"), fm.contains_key("version")) {
                (false, false) => "no name and no version",
                (false, true) => "no name",
                _ => "no version",
            };
            crate::ui::warn(format!("skipping {}/{SKILL_MD}: {missing} in the frontmatter", shown(&dir)));
            continue;
        };
        let at = dir.display();
        // Fails in the skill's own CI rather than at every consumer.
        for (k, v) in crate::skill::requires(&text).map_err(|e| Error::new(MANIFEST, format!("{at}: {e}")))? {
            crate::deps::check(&k, &v).map_err(|e| Error::new(MANIFEST, format!("{at}: requires: {e}")))?;
        }
        if !valid_name(name) {
            bail!(MANIFEST, "{at}: invalid skill name {name:?}");
        }
        let normalized = Version::parse(version)
            .filter(|v| v.stability >= Stability::Stable)
            .filter(|_| version.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c)))
            .ok_or_else(|| Error::new(MANIFEST, format!("{at}: version {version:?} is not a stable release version")))?;
        let package = fm.get("package").cloned().unwrap_or_else(|| format!("{}/{name}", args.vendor));
        if !valid_package_name(&package) {
            bail!(MANIFEST, "{at}: invalid package name {package:?}");
        }
        // Two skills with the same name in different namespaces would merge into one package.
        let first = origin.entry(package.clone()).or_insert_with(|| dir.clone());
        if *first != dir {
            bail!(MANIFEST, "{package} is defined in both {} and {at}; set `package:` in one SKILL.md", first.display());
        }
        let abandoned = match fm.get("deprecated").map(String::as_str) {
            None | Some("false") => None,
            Some("true") => Some(Value::Bool(true)),
            Some(r) if valid_package_name(r) => Some(r.into()),
            Some(r) => bail!(MANIFEST, "{at}: deprecated: {r:?} must be true, false or the replacement package (vendor/name)"),
        };
        let versions = found.entry(package.clone()).or_default();
        let (zip, files) = crate::archive::zip_dir(&dir).ctx(&at)?;
        versions.push(Found {
            dir: dir.clone(),
            zip,
            files,
            version: version.clone(),
            normalized,
            description: fm.get("description").cloned(),
            abandoned,
        });
    }

    let index_path = out.join("packages.json");
    let index: Value = match std::fs::read(&index_path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| Error::new(MANIFEST, format!("{}: {e}", index_path.display())))?,
        Err(_) => json!({}),
    };
    let mut available: BTreeSet<String> = index
        .get("available-packages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    available.extend(index.get("packages").and_then(Value::as_object).into_iter().flat_map(|m| m.keys().cloned()));
    // Names become paths under `out`.
    if let Some(p) = available.iter().find(|p| !valid_package_name(p)) {
        bail!(MANIFEST, "{}: invalid package name {p:?}", index_path.display());
    }
    // Earlier versions come from the inline index or the p2 file, so switching modes keeps them.
    let load = |package: &str| -> Result<Vec<Value>> {
        match index.get("packages").and_then(|p| p.get(package)) {
            Some(Value::Object(m)) => return Ok(m.values().cloned().collect()),
            Some(Value::Array(a)) => return Ok(a.clone()),
            _ => {}
        }
        let p2_path = out.join("p2").join(format!("{package}.json"));
        match std::fs::read(&p2_path) {
            Ok(b) => serde_json::from_slice::<Value>(&b)
                .ok()
                .and_then(|v| v.get("packages")?.get(package)?.as_array().cloned())
                .ok_or_else(|| Error::new(MANIFEST, format!("{}: not a Composer metadata file", p2_path.display()))),
            Err(_) => Ok(Vec::new()),
        }
    };
    let mut all: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for package in &available {
        all.insert(package.clone(), load(package)?);
    }

    // Validate everything before writing, so an immutability error leaves `out` untouched.
    let mut writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut report = Vec::new();
    for (package, versions) in found {
        let mut meta = match all.remove(&package) {
            Some(m) => m,
            None => load(&package)?,
        };
        let (vendor, name) = package.split_once('/').expect("valid package names contain /");
        for f in versions {
            let shasum = hex::encode(Sha1::digest(&f.zip));
            let normalized = f.normalized.to_string();
            let tree = tree_hash(&f.files);
            let existing = meta.iter_mut().find(|m| m.get("version_normalized").and_then(Value::as_str) == Some(normalized.as_str()));
            let zip_path = out.join("dist").join(&package).join(format!("{}.zip", f.version));
            let (state, zip) = match existing {
                Some(m) => {
                    let s = m["dist"]["shasum"].as_str().unwrap_or_default().to_string();
                    let zip = if s == shasum {
                        Some(f.zip)
                    } else if let Some(old) = published_zip_with_tree(&zip_path, &s, &f.files)? {
                        Some(old)
                    } else if let Some(old) = zip_0_1_2(&zip_path, &s, &f.dir) {
                        Some(old)
                    } else if zip_path.symlink_metadata().is_err() && m["extra"][TREE].as_str() == Some(tree.as_str()) {
                        // `dist/` is not kept (CI): the published zip stays where it was uploaded.
                        None
                    } else {
                        bail!(MANIFEST, "{package} {} is already published with different content; bump the version", f.version)
                    };
                    // Versions published before the tree was recorded get it while their zip still matches.
                    if let Some(extra) = m.get_mut("extra").and_then(Value::as_object_mut) {
                        extra.entry(TREE).or_insert_with(|| tree.into());
                    }
                    ("unchanged", zip)
                }
                None => {
                    let mut m = json!({
                        "name": package,
                        "version": f.version,
                        "version_normalized": normalized,
                        "type": "ai-agent-skill",
                        "extra": { "ai-agent-skill": SKILL_MD, TREE: tree },
                        "dist": {
                            "type": "zip",
                            "url": dist_url.replace("{vendor}", vendor).replace("{name}", name).replace("{version}", &f.version),
                            "shasum": shasum,
                            "reference": hex::encode(Sha256::digest(&f.zip)),
                        },
                        // Truncate: rounding up would date the release in the future and fail a zero cooldown.
                        "time": jiff::Timestamp::now()
                            .round(jiff::TimestampRound::new().smallest(jiff::Unit::Second).mode(jiff::RoundMode::Trunc))
                            .map_err(|e| Error::new(MANIFEST, e.to_string()))?
                            .to_string(),
                    });
                    if let Some(d) = f.description {
                        m["description"] = d.into();
                    }
                    if let Some(a) = f.abandoned {
                        m["abandoned"] = a;
                    }
                    meta.push(m);
                    ("published", Some(f.zip))
                }
            };
            if let Some(zip) = zip {
                writes.push((zip_path, zip));
            }
            report.push(format!("{state} {package} {}", f.version));
        }
        let key = |m: &Value| m.get("version_normalized").and_then(Value::as_str).and_then(Version::parse);
        meta.sort_by_key(|m| std::cmp::Reverse(key(m)));
        all.insert(package.clone(), meta);
        available.insert(package);
    }

    let index = match base.filter(|_| !args.inline) {
        Some(base) => {
            for (package, meta) in all.iter().filter(|(_, m)| !m.is_empty()) {
                writes.push((out.join("p2").join(format!("{package}.json")), pretty(&json!({ "packages": { package: meta } }))));
            }
            json!({
                "packages": {},
                "metadata-url": format!("{base}/p2/%package%.json"),
                "available-packages": available,
            })
        }
        None => {
            let by_version = |meta: Vec<Value>| -> Map<String, Value> {
                meta.into_iter().filter_map(|m| Some((m.get("version")?.as_str()?.to_string(), m))).collect()
            };
            json!({ "packages": all.into_iter().map(|(p, m)| (p, Value::Object(by_version(m)))).collect::<Map<_, _>>() })
        }
    };
    writes.push((index_path, pretty(&index)));
    for (path, bytes) in &writes {
        write_if_changed(path, bytes)?;
    }
    for line in report {
        println!("{line}");
    }
    Ok(())
}

/// The zip at `path` if it is the published one (`shasum`) and holds the same files as `files`.
/// Its bytes may differ from a fresh zip after a deflate change. Hidden entries, which older
/// versions zipped, and exec bits on systems without them are ignored.
fn published_zip_with_tree(path: &Path, shasum: &str, files: &[FileEntry]) -> Result<Option<Vec<u8>>> {
    let Ok(zip) = std::fs::read(path) else { return Ok(None) };
    if !hex::encode(Sha1::digest(&zip)).eq_ignore_ascii_case(shasum) {
        return Ok(None);
    }
    let tmp = tempfile::tempdir()?;
    let mut old = crate::archive::extract(&zip, tmp.path(), &crate::archive::Limits::default()).ctx(path.display())?.files;
    old.retain(|f| !f.path.split('/').any(|p| p.starts_with('.')));
    let mut new = files.to_vec();
    if !cfg!(unix) {
        old.iter_mut().chain(new.iter_mut()).for_each(|f| f.exec = false);
    }
    Ok((tree_hash(&old) == tree_hash(&new)).then_some(zip))
}

/// Without the published zip at `path`, the zip 0.1.2 builds from `dir` if it is the published one
/// (`shasum`): 0.1.2 zipped hidden files, so a fresh zip of a skill with `.gitkeep` differs.
fn zip_0_1_2(path: &Path, shasum: &str, dir: &Path) -> Option<Vec<u8>> {
    if path.symlink_metadata().is_ok() {
        return None;
    }
    let zip = crate::archive::zip_dir_0_1_2(dir).ok()?;
    hex::encode(Sha1::digest(&zip)).eq_ignore_ascii_case(shasum).then_some(zip)
}

/// `skillcask validate`: every skill under `paths` (default `.`) against the Agent Skills spec and
/// what `publish` requires of `requires:`; invisible Unicode in its files is a warning.
pub fn validate(paths: &[PathBuf]) -> Result<()> {
    let roots = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.to_vec() };
    let mut dirs = Vec::new();
    for r in &roots {
        let before = dirs.len();
        find(r, None, &mut dirs).ctx(shown(r))?;
        if dirs.len() == before {
            bail!(MANIFEST, "no {SKILL_MD} under {}", shown(r));
        }
    }
    let mut invalid = 0;
    for dir in &dirs {
        let at = shown(dir);
        let mut problems = Vec::new();
        let text = std::fs::read_to_string(dir.join(SKILL_MD)).ctx(format!("{at}/{SKILL_MD}"))?;
        let dir_name = dir.canonicalize().ctx(&at)?.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        problems.extend(crate::skill::spec_problems(&text, &dir_name));
        match crate::skill::requires(&text) {
            Ok(r) => problems.extend(r.iter().filter_map(|(k, v)| crate::deps::check(k, v).err().map(|e| format!("requires: {e}")))),
            Err(e) => problems.push(e),
        }
        match crate::skill::scan_publish(dir) {
            Ok(files) => {
                for f in files {
                    let path = format!("{at}/{}", f.path);
                    let bytes = std::fs::read(dir.join(&f.path)).unwrap_or_default();
                    crate::skill::invisible_warnings(&path, &bytes).into_iter().for_each(crate::ui::warn);
                }
            }
            Err(e) => problems.push(e.msg),
        }
        for p in &problems {
            crate::ui::error(format!("{at}: {p}"), None);
        }
        invalid += usize::from(!problems.is_empty());
    }
    let n = dirs.len();
    let skills = if n == 1 { "skill" } else { "skills" };
    if invalid > 0 {
        bail!(MANIFEST, "{invalid} of {n} {skills} are invalid");
    }
    if !crate::ui::agent() {
        crate::ui::done("checked", format!("{n} {skills}"));
    }
    Ok(())
}

/// A path for messages: without a leading `./`, with `/` on Windows too.
fn shown(p: &Path) -> String {
    let p = p.strip_prefix(".").ok().filter(|r| !r.as_os_str().is_empty()).unwrap_or(p);
    let s = p.display().to_string();
    if cfg!(windows) { s.replace('\\', "/") } else { s }
}

/// Skill directories under `dir`; `out` (the publish output) is not searched.
fn find(dir: &Path, out: Option<&Path>, found: &mut Vec<PathBuf>) -> Result<()> {
    if out.is_some_and(|out| dir.canonicalize().is_ok_and(|c| c == out)) {
        return Ok(());
    }
    if dir.join(SKILL_MD).is_file() {
        found.push(dir.to_path_buf());
    }
    let mut children: Vec<_> = std::fs::read_dir(dir).and_then(Iterator::collect).ctx(dir.display())?;
    children.sort_by_key(|e| e.file_name());
    for e in children {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if e.file_type().ctx(e.path().display())?.is_dir() && !name.starts_with('.') && name != "target" && name != "node_modules" {
            find(&e.path(), out, found)?;
        }
    }
    Ok(())
}

fn pretty(v: &Value) -> Vec<u8> {
    let mut s = serde_json::to_string_pretty(v).expect("json values serialise");
    s.push('\n');
    s.into_bytes()
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    if std::fs::read(path).is_ok_and(|b| b == bytes) {
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).ctx(p.display())?;
    }
    std::fs::write(path, bytes).ctx(path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(root: &Path, dir: &str, name: &str, version: &str, body: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(SKILL_MD), format!("---\nname: {name}\nversion: {version}\ndescription: d\n---\n{body}")).unwrap();
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() { stack.push(p) } else { out.push(p) }
            }
        }
        out
    }

    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
        files(dir).into_iter().map(|p| (p.clone(), (std::fs::read(&p).unwrap(), p.metadata().unwrap().modified().unwrap()))).collect()
    }

    #[test]
    fn publishes_immutably() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        skill(src.path(), "skills/a", "alpha", "1.0.0", "a");
        skill(src.path(), "skills/b", "beta", "2.1.0", "b");
        skill(src.path(), "nested/deep/c", "gamma", "0.3.0", "c");
        skill(src.path(), "skills/noversion", "nov", "", "x");
        std::fs::write(src.path().join("skills/noversion/SKILL.md"), "---\nname: nov\n---\n").unwrap();
        let args = PublishArgs {
            vendor: "acme".into(),
            base_url: Some("https://example.com/repo/".into()),
            dist_url: None,
            inline: false,
            out: out.path().into(),
            paths: vec![src.path().into()],
        };
        publish(&args).unwrap();

        let index: Value = serde_json::from_slice(&std::fs::read(out.path().join("packages.json")).unwrap()).unwrap();
        assert_eq!(index["metadata-url"], "https://example.com/repo/p2/%package%.json");
        assert_eq!(index["available-packages"], json!(["acme/alpha", "acme/beta", "acme/gamma"]));
        let p2: Value = serde_json::from_slice(&std::fs::read(out.path().join("p2/acme/beta.json")).unwrap()).unwrap();
        let v = &p2["packages"]["acme/beta"][0];
        assert_eq!(v["version_normalized"], "2.1.0.0");
        assert_eq!(v["dist"]["url"], "https://example.com/repo/dist/acme/beta/2.1.0.zip");
        let zip = std::fs::read(out.path().join("dist/acme/beta/2.1.0.zip")).unwrap();
        assert_eq!(v["dist"]["shasum"], hex::encode(Sha1::digest(&zip)));

        for p in files(out.path()) {
            crate::archive::backdate(&p);
        }
        let before = snapshot(out.path());
        publish(&args).unwrap();
        assert_eq!(snapshot(out.path()), before);

        skill(src.path(), "skills/a", "alpha", "1.0.0", "changed");
        assert!(publish(&args).unwrap_err().msg.contains("acme/alpha 1.0.0"));

        skill(src.path(), "skills/a", "alpha", "1.1.0", "changed");
        publish(&args).unwrap();
        let p2: Value = serde_json::from_slice(&std::fs::read(out.path().join("p2/acme/alpha.json")).unwrap()).unwrap();
        let versions: Vec<_> =
            p2["packages"]["acme/alpha"].as_array().unwrap().iter().map(|v| v["version"].as_str().unwrap().to_string()).collect();
        assert_eq!(versions, ["1.1.0", "1.0.0"]);
        assert!(out.path().join("dist/acme/alpha/1.0.0.zip").exists());
    }

    fn args(src: &Path, out: &Path) -> PublishArgs {
        PublishArgs {
            vendor: "acme".into(),
            base_url: Some("https://example.com".into()),
            dist_url: None,
            inline: false,
            out: out.into(),
            paths: vec![src.into()],
        }
    }

    #[test]
    fn keeps_a_published_zip_whose_tree_is_unchanged() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        skill(src.path(), "a", "alpha", "1.0.0", "a");
        publish(&args(src.path(), out.path())).unwrap();
        // An earlier deflate implementation: same tree, other bytes.
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::DEFAULT.compression_method(zip::CompressionMethod::Stored).unix_permissions(0o644);
        w.start_file(SKILL_MD, opts).unwrap();
        std::io::Write::write_all(&mut w, b"---\nname: alpha\nversion: 1.0.0\ndescription: d\n---\na").unwrap();
        let old = w.finish().unwrap().into_inner();
        std::fs::write(out.path().join("dist/acme/alpha/1.0.0.zip"), &old).unwrap();
        let p2 = out.path().join("p2/acme/alpha.json");
        let meta = std::fs::read_to_string(&p2).unwrap();
        let shasum =
            serde_json::from_str::<Value>(&meta).unwrap()["packages"]["acme/alpha"][0]["dist"]["shasum"].as_str().unwrap().to_string();
        std::fs::write(&p2, meta.replace(&shasum, &hex::encode(Sha1::digest(&old)))).unwrap();

        let before = snapshot(out.path());
        publish(&args(src.path(), out.path())).unwrap();
        assert_eq!(snapshot(out.path()), before);
        skill(src.path(), "a", "alpha", "1.0.0", "changed");
        assert!(publish(&args(src.path(), out.path())).unwrap_err().msg.contains("different content"));
    }

    #[test]
    fn compares_the_recorded_tree_when_dist_is_gone() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        skill(src.path(), "a", "alpha", "1.0.0", "a");
        publish(&args(src.path(), out.path())).unwrap();
        let p2 = out.path().join("p2/acme/alpha.json");
        let mut meta: Value = serde_json::from_slice(&std::fs::read(&p2).unwrap()).unwrap();
        let v = &mut meta["packages"]["acme/alpha"][0];
        let tree = v["extra"][TREE].as_str().unwrap().to_string();
        // Published before the tree was recorded: the first run with a matching zip backfills it.
        v["extra"].as_object_mut().unwrap().remove(TREE);
        std::fs::write(&p2, pretty(&meta)).unwrap();
        publish(&args(src.path(), out.path())).unwrap();
        let meta: Value = serde_json::from_slice(&std::fs::read(&p2).unwrap()).unwrap();
        assert_eq!(meta["packages"]["acme/alpha"][0]["extra"][TREE], tree.as_str());

        // CI without `dist/`, and a zip crate that now writes other bytes for the same tree.
        let mut meta = meta;
        meta["packages"]["acme/alpha"][0]["dist"]["shasum"] = "0".repeat(40).into();
        std::fs::write(&p2, pretty(&meta)).unwrap();
        std::fs::remove_dir_all(out.path().join("dist")).unwrap();
        let before = snapshot(out.path());
        publish(&args(src.path(), out.path())).unwrap();
        assert_eq!(snapshot(out.path()), before, "no zip is written for a version published elsewhere");
        skill(src.path(), "a", "alpha", "1.0.0", "changed");
        assert!(publish(&args(src.path(), out.path())).unwrap_err().msg.contains("different content"));
    }

    #[test]
    fn republishes_a_0_1_2_zip_with_hidden_files_after_dist_is_gone() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        skill(src.path(), "a", "alpha", "1.0.0", "a");
        std::fs::write(src.path().join("a/.gitkeep"), "").unwrap();
        publish(&args(src.path(), out.path())).unwrap();
        let legacy = crate::archive::zip_dir_0_1_2(&src.path().join("a")).unwrap();
        let p2 = out.path().join("p2/acme/alpha.json");
        let mut meta: Value = serde_json::from_slice(&std::fs::read(&p2).unwrap()).unwrap();
        let dist = &mut meta["packages"]["acme/alpha"][0]["dist"];
        dist["shasum"] = hex::encode(Sha1::digest(&legacy)).into();
        dist["reference"] = hex::encode(Sha256::digest(&legacy)).into();
        std::fs::write(&p2, pretty(&meta)).unwrap();
        std::fs::remove_dir_all(out.path().join("dist")).unwrap();

        let before = snapshot(out.path());
        publish(&args(src.path(), out.path())).unwrap();
        let mut after = snapshot(out.path());
        assert_eq!(after.remove(&out.path().join("dist/acme/alpha/1.0.0.zip")).unwrap().0, legacy, "the published zip is restored");
        assert_eq!(after, before);
    }

    #[test]
    fn never_writes_outside_out() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let out = tmp.path().join("x/out");
        std::fs::create_dir_all(&out).unwrap();
        skill(&src, "a", "alpha", "1.0.0", "a");
        let evil = json!({ "packages": { "../../evil": { "1.0.0": { "version": "1.0.0", "version_normalized": "1.0.0.0" } } } });
        std::fs::write(out.join("packages.json"), evil.to_string()).unwrap();
        assert!(publish(&args(&src, &out)).unwrap_err().msg.contains("../../evil"));
        let names: Vec<_> = std::fs::read_dir(tmp.path().join("x")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["out"]);
        assert!(!out.join("p2").exists() && !out.join("dist").exists());
    }

    #[test]
    fn reads_block_scalar_descriptions() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join(SKILL_MD), "---\nname: alpha\nversion: 1.0.0\ndescription: >-\n  Reviews\n  code\n---\n").unwrap();
        publish(&args(src.path(), out.path())).unwrap();
        let p2: Value = serde_json::from_slice(&std::fs::read(out.path().join("p2/acme/alpha.json")).unwrap()).unwrap();
        assert_eq!(p2["packages"]["acme/alpha"][0]["description"], "Reviews code");
    }

    #[test]
    fn maps_deprecated_to_abandoned() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let publish_with = |version: &str, deprecated: &str| {
            std::fs::write(src.path().join(SKILL_MD), format!("---\nname: alpha\nversion: {version}\n{deprecated}---\n")).unwrap();
            publish(&args(src.path(), out.path()))
        };
        let abandoned = |i: usize| {
            let p2: Value = serde_json::from_slice(&std::fs::read(out.path().join("p2/acme/alpha.json")).unwrap()).unwrap();
            p2["packages"]["acme/alpha"][i].get("abandoned").cloned()
        };
        publish_with("1.0.0", "deprecated: false\n").unwrap();
        assert_eq!(abandoned(0), None);
        publish_with("1.1.0", "deprecated: true\n").unwrap();
        assert_eq!(abandoned(0), Some(json!(true)));
        publish_with("1.2.0", "deprecated: \"acme/beta\"\n").unwrap();
        assert_eq!((abandoned(0), abandoned(1)), (Some(json!("acme/beta")), Some(json!(true))));
        let e = publish_with("1.3.0", "deprecated: use beta\n").unwrap_err().msg;
        assert!(e.contains("deprecated"), "{e}");
    }

    #[test]
    fn checks_requires() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let write = |req: &str| {
            std::fs::write(src.path().join(SKILL_MD), format!("---\nname: alpha\nversion: 1.0.0\nrequires:\n  {req}\n---\n")).unwrap()
        };
        write("acme/beta: ^1.0");
        publish(&args(src.path(), out.path())).unwrap();
        for bad in ["acme/beta: main", "beta: ^1.0", "- acme/beta"] {
            write(bad);
            let e = publish(&args(src.path(), out.path())).unwrap_err().msg;
            assert!(e.contains("requires"), "{bad}: {e}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let args = |base: &str| PublishArgs {
            vendor: "acme".into(),
            base_url: Some(base.into()),
            dist_url: None,
            inline: false,
            out: out.path().into(),
            paths: vec![src.path().into()],
        };
        assert!(publish(&args("http://insecure")).is_err());
        skill(src.path(), "a", "Bad_Name", "1.0.0", "");
        assert!(publish(&args("https://x")).is_err());
        skill(src.path(), "a", "ok", "1.0.0-beta1", "");
        assert!(publish(&args("https://x")).is_err());
        skill(src.path(), "a", "ok", "1.0.0", "");
        skill(src.path(), "b/nested", "ok", "2.0.0", "other");
        let e = publish(&args("https://x")).unwrap_err().msg;
        assert!(e.contains("acme/ok is defined in both") && e.contains("package:"), "{e}");
        let no_base = PublishArgs { base_url: None, ..args("https://x") };
        assert!(publish(&no_base).unwrap_err().msg.contains("--base-url is required"));
        let bad_tpl = PublishArgs { inline: true, dist_url: Some("https://x/{name}.zip".into()), ..no_base };
        assert!(publish(&bad_tpl).unwrap_err().msg.contains("{version}"));
    }

    #[test]
    fn publishes_inline_with_dist_template() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        skill(src.path(), "a", "alpha", "1.0.0", "a");
        let mut args = PublishArgs {
            vendor: "acme".into(),
            base_url: Some("https://example.com/repo".into()),
            dist_url: None,
            inline: false,
            out: out.path().into(),
            paths: vec![src.path().into()],
        };
        publish(&args).unwrap();
        // Switching to inline keeps versions published through p2.
        args.base_url = None;
        args.inline = true;
        args.dist_url = Some("https://git.example.com/generic/{vendor}-{name}/{version}/{name}-{version}.zip".into());
        skill(src.path(), "a", "alpha", "1.1.0", "a2");
        skill(src.path(), "b", "beta", "2.0.0", "b");
        publish(&args).unwrap();
        let index: Value = serde_json::from_slice(&std::fs::read(out.path().join("packages.json")).unwrap()).unwrap();
        assert!(index.get("metadata-url").is_none());
        let alpha = &index["packages"]["acme/alpha"];
        assert_eq!(alpha["1.0.0"]["dist"]["url"], "https://example.com/repo/dist/acme/alpha/1.0.0.zip", "published URLs are immutable");
        assert_eq!(alpha["1.1.0"]["dist"]["url"], "https://git.example.com/generic/acme-alpha/1.1.0/alpha-1.1.0.zip");
        assert_eq!(index["packages"]["acme/beta"]["2.0.0"]["version_normalized"], "2.0.0.0");
        assert!(out.path().join("dist/acme/beta/2.0.0.zip").exists());

        let before = std::fs::read(out.path().join("packages.json")).unwrap();
        publish(&args).unwrap();
        assert_eq!(std::fs::read(out.path().join("packages.json")).unwrap(), before);
        skill(src.path(), "b", "beta", "2.0.0", "changed");
        assert!(publish(&args).unwrap_err().msg.contains("acme/beta 2.0.0"));
    }
}
