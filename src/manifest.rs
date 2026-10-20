//! `skills.toml`: read with declaration order preserved, edited in place so comments survive.

use std::collections::BTreeMap;
use std::path::Path;

use jiff::SignedDuration;
use toml_edit::{DocumentMut, InlineTable, Item, TableLike, Value};

use crate::bail;
use crate::error::{Context, MANIFEST, Result};

pub const FILE: &str = "skills.toml";
pub const PACKAGIST_URL: &str = "https://repo.packagist.org";
pub const DEFAULT_TARGETS: [&str; 2] = [".claude/skills", ".junie/skills"];
pub const DEFAULT_LOCAL: &str = ".skills";

pub const TEMPLATE: &str = r#"# skillcask manifest. Lock: skills.lock (generated, commit both).

[repositories]
# name = { type = "composer", url = "https://example.com/packages.json" }
# Default sources; `packagist = false` turns one off. Packagist is always searched last.
packagist = { url = "https://repo.packagist.org" }
github = { type = "github", url = "https://api.github.com" }

[skills]
# "vendor/name" = "^1.0"
# "owner/repo" = { github = true, skills = ["name"] }   # GitHub, default branch

[agents]
targets = [".claude/skills", ".junie/skills"]
local = ".skills"   # project skills, linked into .agents/skills

[policy]
min-release-age = "3d"
allow-ranges = true
# allow-sources = ["packagist", "github"]   # only these: repository names, packagist, github, https:// prefixes
"#;

pub const GLOBAL_TEMPLATE: &str = r#"# skillcask global manifest: personal skills in ~/.agents/skills. Lock: skills.lock.
# Policy for every project goes into config.toml next to this file.

[repositories]
# name = { type = "composer", url = "https://example.com/packages.json" }
# Default sources; `packagist = false` turns one off. Packagist is always searched last.
packagist = { url = "https://repo.packagist.org" }
github = { type = "github", url = "https://api.github.com" }

[skills]
# "vendor/name" = "^1.0"
# "owner/repo" = { github = true, skills = ["name"] }   # GitHub, default branch

[agents]
targets = [".claude/skills", ".junie/skills"]

[policy]
min-release-age = "3d"
allow-ranges = true
# allow-sources = ["packagist", "github"]   # only these: repository names, packagist, github, https:// prefixes
"#;

pub fn template(global: bool) -> &'static str {
    if global { GLOBAL_TEMPLATE } else { TEMPLATE }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Repository {
    pub name: String,
    pub url: String,
    /// Overrides `policy.min-release-age` for packages resolved here.
    pub min_release_age: Option<SignedDuration>,
}

/// A GitHub source: `constraint` is a version constraint over tags, or a branch, tag or commit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GitHub {
    Tags,
    Ref,
}

/// The `ref` of a GitHub entry without one: the default branch.
pub const DEFAULT_REF: &str = "HEAD";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillReq {
    pub name: String,
    pub constraint: String,
    pub repository: Option<String>,
    pub skills: Option<Vec<String>>,
    pub github: Option<GitHub>,
    /// Upstream skill name to install name.
    pub rename: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub min_release_age: SignedDuration,
    pub allow_ranges: bool,
    pub packagist: bool,
    /// `requires:` may name GitHub sources (D16).
    pub github_requires: bool,
    /// The only sources allowed; `None`: all (D18).
    pub allow_sources: Option<Vec<String>>,
    /// Keys set by a layer, with that layer, for `status`.
    pub effective: crate::policy::Effective,
}

#[derive(Clone, Debug)]
pub struct Manifest {
    /// Declared order, Packagist last; it is implicit unless turned off.
    pub repositories: Vec<Repository>,
    /// The GitHub API base; `None` with `github = false`.
    pub github: Option<String>,
    pub skills: Vec<SkillReq>,
    pub targets: Vec<String>,
    /// Project skills, linked into the store (`[agents] local`, default `.skills`).
    pub local: String,
    pub policy: Policy,
}

impl Manifest {
    pub fn load(dir: &Path, global: bool) -> Result<Manifest> {
        let path = dir.join(FILE);
        if !path.exists() {
            let init = if global { "skillcask --global init" } else { "skillcask init" };
            return Err(crate::error::Error::new(MANIFEST, format!("{FILE} not found")).hint(format!("run {init}")));
        }
        layered(&std::fs::read_to_string(&path)?, global)
    }

    /// Whether `url` (a repository URL as the lock records it) is an allowed source.
    pub fn allows(&self, url: &str) -> bool {
        let Some(list) = &self.policy.allow_sources else { return true };
        list.iter().any(|e| match e.as_str() {
            "packagist" => crate::policy::under(url, PACKAGIST_URL),
            "github" => self.github_apis().iter().any(|a| crate::policy::under(url, &format!("{a}/repos/"))),
            e if e.starts_with("https://") => crate::policy::under(url, e),
            name => self.repository(name).is_some_and(|r| r.url == url),
        })
    }

    /// The APIs `"github"` in `allow-sources` stands for: the one in use, unless the list comes from
    /// the environment or an enforced key, which a project's `github` url must not widen (D29).
    fn github_apis(&self) -> Vec<String> {
        if crate::policy::fixed(&self.policy.effective, "allow-sources") {
            crate::github::trusted_apis()
        } else {
            vec![crate::github::api()]
        }
    }

    /// Why packages from `url` are refused, or `None` when they are allowed.
    pub fn refuses(&self, url: &str) -> Option<String> {
        if self.allows(url) {
            return None;
        }
        let from = self.policy.effective.get("allow-sources").map_or("", |(_, f)| f);
        let shown = url::Url::parse(url).map_or_else(|_| url.to_string(), |u| crate::http::redact(&u));
        let github = self.policy.allow_sources.as_ref().is_some_and(|l| l.iter().any(|e| e == "github"))
            && crate::policy::under(url, &format!("{}/repos/", crate::github::api()));
        let why = if github {
            format!("; there \"github\" covers {} only (SKILLCASK_GITHUB_API names one more)", crate::github::trusted_apis().join(", "))
        } else {
            String::new()
        };
        Some(format!("source {shown} is not in policy.allow-sources ({from}){why}"))
    }

    /// Repositories lookups may contact.
    pub fn allowed_repositories(&self) -> Vec<Repository> {
        self.repositories.iter().filter(|r| self.allows(&r.url)).cloned().collect()
    }

    /// Errors unless GitHub repository `name` (`owner/repo`) may be used.
    pub fn check_github(&self, name: &str) -> Result<()> {
        if self.github.is_none() {
            bail!(MANIFEST, "{name}: GitHub is turned off (github = false in [repositories] of {FILE})");
        }
        if let Some(why) = self.refuses(&format!("{}/repos/{name}", crate::github::api())) {
            bail!(MANIFEST, "{name}: {why}");
        }
        Ok(())
    }

    pub fn repository(&self, name: &str) -> Option<&Repository> {
        self.repositories.iter().find(|r| r.name == name)
    }

    pub fn skill(&self, name: &str) -> Option<&SkillReq> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// The cooldown for packages from `repo`.
    pub fn min_release_age(&self, repo: &Repository) -> SignedDuration {
        repo.min_release_age.unwrap_or(self.policy.min_release_age)
    }
}

fn bad(what: impl std::fmt::Display) -> crate::error::Error {
    crate::error::Error::new(MANIFEST, format!("{FILE}: {what}"))
}

/// The manifest alone, without the user config and the environment.
pub fn parse(text: &str, global: bool) -> Result<Manifest> {
    parse_with(text, global, &crate::policy::Outer::default())
}

/// The manifest with the policy layers of `config.toml` and the environment (D18).
pub fn layered(text: &str, global: bool) -> Result<Manifest> {
    let m = parse_with(text, global, &crate::policy::outer()?)?;
    if let Some(api) = &m.github {
        crate::github::set_api(api);
    }
    Ok(m)
}

pub fn parse_with(text: &str, global: bool, outer: &crate::policy::Outer) -> Result<Manifest> {
    use crate::policy::Value as V;
    let doc: DocumentMut = text.parse().map_err(bad)?;

    let mut repositories = Vec::new();
    let mut github = Some(crate::github::API.to_string());
    let mut packagist_off = false;
    if let Some(t) = doc.get("repositories").and_then(Item::as_table_like) {
        for (name, item) in t.iter() {
            if matches!(name, "packagist" | "github") && item.as_bool().is_some() {
                if item.as_bool() == Some(false) {
                    if name == "github" {
                        github = None;
                    } else {
                        packagist_off = true;
                    }
                }
                continue;
            }
            let r = item.as_table_like().ok_or_else(|| bad(format!("repository {name} must be a table")))?;
            let kind = r.get("type").and_then(Item::as_str).unwrap_or("composer");
            let url = r.get("url").and_then(Item::as_str).ok_or_else(|| bad(format!("repository {name}: missing url")))?;
            if !url.starts_with("https://") {
                return Err(bad(format!("repository {name}: only https:// URLs are allowed")));
            }
            match (kind, name) {
                ("github", "github") => {
                    if r.contains_key("min-release-age") {
                        return Err(bad("repository github: min-release-age is not supported; GitHub uses policy.min-release-age"));
                    }
                    github = Some(url.trim_end_matches('/').to_string());
                    continue;
                }
                ("github", _) => return Err(bad(format!("repository {name}: type \"github\" is only accepted on the key github"))),
                ("composer", _) => {}
                _ => return Err(bad(format!("repository {name}: type {kind:?} is not supported, only \"composer\" or \"github\""))),
            }
            let min_release_age = r.get("min-release-age").map(|i| age(i, &format!("repository {name}: min-release-age"))).transpose()?;
            repositories.push(Repository { name: name.to_string(), url: url.to_string(), min_release_age });
        }
    }

    let mut project = crate::policy::layer(doc.get("policy").and_then(Item::as_table_like), FILE, false)?;
    if packagist_off {
        project.values.insert("packagist", V::Bool(false));
    }
    let effective = crate::policy::resolve(&project, outer)?;
    let flag = |k: &str, default: bool| match effective.get(k) {
        Some((V::Bool(b), _)) => *b,
        _ => default,
    };
    let policy = Policy {
        min_release_age: match effective.get("min-release-age") {
            Some((V::Age(a), _)) => *a,
            _ => SignedDuration::from_hours(72),
        },
        allow_ranges: flag("allow-ranges", true),
        packagist: flag("packagist", true),
        github_requires: flag("github-requires", false),
        allow_sources: match effective.get("allow-sources") {
            Some((V::List(l), _)) => Some(l.clone()),
            _ => None,
        },
        effective,
    };
    if let Some(list) = &policy.allow_sources
        && let Some(n) = list.iter().find(|e| !matches!(e.as_str(), "packagist" | "github") && !e.starts_with("https://"))
        && !repositories.iter().any(|r| &r.name == n)
    {
        return Err(bad(format!("policy.allow-sources: unknown repository {n:?}")));
    }
    let is_packagist = |r: &Repository| r.name == "packagist" || r.url.trim_end_matches('/') == PACKAGIST_URL;
    if !policy.packagist {
        repositories.retain(|r| !is_packagist(r));
    } else if !repositories.iter().any(is_packagist) {
        repositories.push(Repository { name: "packagist".into(), url: PACKAGIST_URL.into(), ..Default::default() });
    }
    // A public package must never shadow a private one with the same name.
    repositories.sort_by_key(is_packagist);
    if let Some((V::Age(enforced), "config.toml, enforced")) = policy.effective.get("min-release-age") {
        for r in &mut repositories {
            let floor = crate::policy::repository_floor(outer, &r.url, *enforced);
            match r.min_release_age {
                Some(a) if a < floor => {
                    return Err(bad(format!(
                        "repository {}: min-release-age \"{a:#}\" is shorter than \"{floor:#}\" enforced by {}; remove it or ask to lift it",
                        r.name, outer.config_path
                    )));
                }
                Some(_) => {}
                None => r.min_release_age = Some(floor),
            }
        }
    }

    let mut skills = Vec::new();
    if let Some(t) = doc.get("skills").and_then(Item::as_table_like) {
        for (name, item) in t.iter() {
            if !valid_package_name(name) {
                return Err(bad(format!("{name:?} is not a valid package name (vendor/name, lowercase)")));
            }
            let req = if let Some(c) = item.as_str() {
                SkillReq { name: name.into(), constraint: c.into(), ..Default::default() }
            } else if let Some(t) = item.as_table_like() {
                let github = match t.get("github") {
                    None => false,
                    Some(i) => i.as_bool().ok_or_else(|| bad(format!("{name}: github must be true or false")))?,
                };
                let version = t.get("version").and_then(Item::as_str);
                let git_ref = t.get("ref").and_then(Item::as_str);
                let repository = t.get("repository").and_then(Item::as_str).map(String::from);
                let (constraint, github) = match (github, version, git_ref) {
                    (_, Some(_), Some(_)) => return Err(bad(format!("{name}: set either version or ref, not both"))),
                    (false, _, Some(_)) => return Err(bad(format!("{name}: ref needs github = true"))),
                    (false, None, None) => return Err(bad(format!("{name}: missing version"))),
                    (false, Some(v), None) => (v, None),
                    (true, Some(v), None) => (v, Some(GitHub::Tags)),
                    (true, None, r) => (r.unwrap_or(DEFAULT_REF), Some(GitHub::Ref)),
                };
                if let Some(r) = &repository {
                    if github.is_some() {
                        return Err(bad(format!("{name}: repository and github = true cannot be combined")));
                    }
                    if !repositories.iter().any(|x| &x.name == r) {
                        return Err(bad(format!("{name}: unknown repository {r:?}")));
                    }
                }
                let skills = match t.get("skills") {
                    None => None,
                    Some(i) => Some(string_array(i).ok_or_else(|| bad(format!("{name}: skills must be a list of names")))?),
                };
                let mut rename = BTreeMap::new();
                if let Some(i) = t.get("rename") {
                    let bad_table = || bad(format!("{name}: rename must be a table of skill = \"new-name\""));
                    for (k, v) in i.as_table_like().ok_or_else(bad_table)?.iter() {
                        let v = v.as_str().ok_or_else(bad_table)?;
                        if !crate::skill::valid_name(v) {
                            return Err(bad(format!("{name}: rename: {v:?} is not a valid skill name ([a-z0-9][a-z0-9-]*)")));
                        }
                        if rename.values().any(|x: &String| x == v) {
                            return Err(bad(format!("{name}: rename: two skills renamed to {v:?}")));
                        }
                        rename.insert(k.to_string(), v.to_string());
                    }
                }
                rename.retain(|k, v| k != v);
                SkillReq { name: name.into(), constraint: constraint.into(), repository, skills, github, rename }
            } else {
                return Err(bad(format!("{name}: expected a version string or a table")));
            };
            check_constraint(&req, &policy).map_err(|e| bad(format!("{name}: {e}")))?;
            skills.push(req);
        }
    }

    let agents = doc.get("agents").and_then(Item::as_table_like);
    let local = match agents.and_then(|a| a.get("local")) {
        Some(_) if global => {
            return Err(bad("agents.local is not supported with --global; personal skills go straight into ~/.agents/skills"));
        }
        None => DEFAULT_LOCAL.to_string(),
        Some(i) => i.as_str().ok_or_else(|| bad("agents.local must be a path"))?.trim_end_matches('/').to_string(),
    };
    let targets = match agents.and_then(|a| a.get("targets")) {
        None => DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect(),
        Some(i) => string_array(i).ok_or_else(|| bad("agents.targets must be a list of paths"))?,
    };
    for t in targets.iter().chain([&local]) {
        let p = Path::new(t);
        if p.is_absolute() || p.components().any(|c| !matches!(c, std::path::Component::Normal(_))) || t.is_empty() || t.contains('\\') {
            return Err(bad(format!("agents: {t:?} must be a relative path inside the project")));
        }
        if p == Path::new(crate::project::STORE) {
            return Err(bad(format!("agents: {t:?} is the store itself")));
        }
    }

    if !global && targets.contains(&local) {
        return Err(bad(format!("agents.local {local:?} cannot also be a target")));
    }

    Ok(Manifest { repositories, github, skills, targets, local, policy })
}

/// The version constraint, or for a GitHub `ref` its syntax; `allow-ranges = false` also forbids
/// branches, since they move like ranges do.
pub fn check_constraint(req: &SkillReq, policy: &Policy) -> std::result::Result<(), String> {
    let c = &req.constraint;
    if req.github == Some(GitHub::Ref) {
        // The ref ends up in a GitHub API URL path, so no escaping is needed for these characters.
        let ok = !c.is_empty()
            && !c.starts_with(['-', '/'])
            && !c.ends_with('/')
            && !c.contains("..")
            && !c.contains("//")
            && c.chars().all(|ch| ch.is_ascii_alphanumeric() || "._/-".contains(ch));
        if !ok {
            return Err(format!("invalid ref {c:?}"));
        }
        if !policy.allow_ranges && !crate::github::is_sha(c) {
            return Err(format!("policy.allow-ranges = false requires ref to be a full commit sha, got {c:?}"));
        }
        return Ok(());
    }
    crate::version::Constraint::parse(c)?;
    if !policy.allow_ranges && !crate::version::is_exact(c) {
        return Err(format!("policy.allow-ranges = false requires an exact version, got {c:?}"));
    }
    Ok(())
}

fn string_array(i: &Item) -> Option<Vec<String>> {
    i.as_array()?.iter().map(|v| v.as_str().map(String::from)).collect()
}

/// Composer's package name rule.
pub fn valid_package_name(n: &str) -> bool {
    let Some((v, p)) = n.split_once('/') else { return false };
    let part = |s: &str| {
        !s.is_empty()
            && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && s.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
            && !s.contains("..")
    };
    part(v) && part(p)
}

fn age(i: &Item, what: &str) -> Result<SignedDuration> {
    match (i.as_str(), i.as_integer()) {
        (Some(s), _) => parse_age(s),
        (_, Some(0)) => Ok(SignedDuration::ZERO),
        _ => Err(bad(format!("{what} must be a string such as \"3d\""))),
    }
}

/// `"3d"`, `"12h"`, `"30m"`, `"1w"`, `"90s"`, `"0"`.
pub fn parse_age(s: &str) -> Result<SignedDuration> {
    let s = s.trim();
    if s == "0" {
        return Ok(SignedDuration::ZERO);
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse().map_err(|_| bad(format!("invalid duration {s:?}")))?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        _ => return Err(bad(format!("invalid duration {s:?}, use s, m, h, d or w"))),
    };
    // Past this the cutoff arithmetic in resolution could overflow and disable the cooldown.
    const MAX: i64 = 36525 * 86400;
    match n.checked_mul(secs) {
        Some(total) if total <= MAX => Ok(SignedDuration::from_secs(total)),
        _ => Err(bad(format!("duration {s:?} is longer than 100 years"))),
    }
}

fn edit(root: &Path, global: bool, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let path = root.join(FILE);
    // A missing manifest starts as the template and is created only if the edit succeeds.
    let text = if path.exists() { std::fs::read_to_string(&path).ctx(FILE)? } else { template(global).to_string() };
    let mut doc: DocumentMut = text.parse().map_err(bad)?;
    f(&mut doc)?;
    parse(&doc.to_string(), global)?;
    crate::project::write_atomic(&path, &doc.to_string())
}

/// Adds or updates the entry of `req`, keeping its comments and its form (string, inline table,
/// dotted keys or a `[skills."vendor/name"]` table).
pub fn write_skill(root: &Path, global: bool, req: &SkillReq) -> Result<()> {
    edit(root, global, |doc| {
        let skills = doc.entry("skills").or_insert(toml_edit::table()).as_table_like_mut().ok_or_else(|| bad("skills must be a table"))?;
        let simple = req.repository.is_none() && req.skills.is_none() && req.github.is_none() && req.rename.is_empty();
        let fresh = || {
            if simple {
                Value::from(req.constraint.as_str())
            } else {
                let mut t = InlineTable::new();
                set_keys(&mut t, req);
                Value::InlineTable(t)
            }
        };
        match skills.get_mut(&req.name) {
            Some(item) if item.is_table_like() => match item.as_inline_table_mut() {
                Some(t) if multiline(t) => edit_lines(t, req),
                Some(t) => {
                    set_keys(t, req);
                    // Spacing around removed and appended keys; the comment after `}` is the value's.
                    t.fmt();
                }
                None => set_keys(item.as_table_like_mut().expect("checked"), req),
            },
            Some(Item::Value(old)) => {
                let mut new = fresh();
                *new.decor_mut() = old.decor().clone();
                *old = new;
            }
            _ => {
                skills.insert(&req.name, Item::Value(fresh()));
            }
        }
        Ok(())
    })
}

fn set_keys(t: &mut dyn TableLike, req: &SkillReq) {
    let c = req.constraint.as_str();
    let (github, version, git_ref) = match req.github {
        None => (None, Some(c), None),
        Some(GitHub::Tags) => (Some(true), Some(c), None),
        Some(GitHub::Ref) => (Some(true), None, Some(c).filter(|c| *c != DEFAULT_REF)),
    };
    set(t, "github", github.map(Value::from));
    set(t, "version", version.map(Value::from));
    set(t, "ref", git_ref.map(Value::from));
    set(t, "repository", req.repository.as_deref().map(Value::from));
    set(t, "skills", req.skills.as_ref().map(|s| Value::Array(s.iter().map(String::as_str).collect())));
    let rename = (!req.rename.is_empty()).then(|| req.rename.iter().map(|(k, v)| (k.as_str(), Value::from(v.as_str()))).collect());
    set(t, "rename", rename.map(Value::InlineTable));
}

fn prefix(t: &InlineTable, k: &str) -> String {
    t.key(k).and_then(|k| k.leaf_decor().prefix()).and_then(|p| p.as_str()).unwrap_or("").to_string()
}

/// TOML 1.1 inline table over several lines.
fn multiline(t: &InlineTable) -> bool {
    t.iter().any(|(k, _)| prefix(t, k).contains('\n')) || t.trailing().as_str().is_some_and(|s| s.contains('\n'))
}

/// Edits a multi-line inline table line by line. toml_edit keeps each comment in the prefix of
/// the next key (or the table's trailing text): up to the first newline it ends the line above,
/// after it it is a line of its own above that key.
fn edit_lines(t: &mut InlineTable, req: &SkillReq) {
    let mut want = InlineTable::new();
    set_keys(&mut want, req);
    let head = |s: &str| s.find('\n').unwrap_or(s.len());
    let keys: Vec<String> = t.iter().map(|(k, _)| k.to_string()).collect();
    let indent = keys.first().map(|k| prefix(t, k)).map(|p| p[p.rfind('\n').map_or(0, |n| n + 1)..].to_string()).unwrap_or_default();
    // Without a trailing comma the last line's comment is the last value's suffix: move it to
    // the trailing text, where it is with a comma.
    if let Some(v) = keys.last().and_then(|k| t.get_mut(k)) {
        let suffix = v.decor().suffix().and_then(|s| s.as_str()).unwrap_or("").to_string();
        if suffix.contains('\n') {
            v.decor_mut().set_suffix("");
            let trailing = format!("{suffix}{}", t.trailing().as_str().unwrap_or(""));
            t.set_trailing(trailing);
        }
    }
    for k in keys.iter().filter(|k| !want.contains_key(k)) {
        // The comment ending the line above stays; the removed key's own lines and comment go.
        let mine = prefix(t, k);
        let keep = &mine[..head(&mine)];
        let next = t.iter().map(|(n, _)| n.to_string()).skip_while(|n| n != k).nth(1);
        t.remove(k);
        match next {
            Some(n) => {
                let p = prefix(t, &n);
                t.key_mut(&n).expect("listed").leaf_decor_mut().set_prefix(format!("{keep}{}", &p[head(&p)..]));
            }
            None => {
                let p = t.trailing().as_str().unwrap_or("").to_string();
                t.set_trailing(format!("{keep}{}", &p[head(&p)..]));
            }
        }
    }
    for (k, v) in want.iter() {
        if t.contains_key(k) {
            set(t, k, Some(v.clone()));
            continue;
        }
        // A new last line: the comment ending the previous last line stays there.
        let trailing = t.trailing().as_str().unwrap_or("").to_string();
        let mut v = v.clone();
        v.decor_mut().set_prefix(" ");
        v.decor_mut().set_suffix("");
        t.insert(k, v);
        t.key_mut(k).expect("inserted").leaf_decor_mut().set_prefix(format!("{}\n{indent}", &trailing[..head(&trailing)]));
        t.set_trailing(&trailing[head(&trailing)..]);
    }
}

/// Sets `k` keeping the old value's comments, or removes it for `None`.
fn set(t: &mut dyn TableLike, k: &str, v: Option<Value>) {
    let Some(mut v) = v else {
        t.remove(k);
        return;
    };
    if let Some(Item::Value(old)) = t.get_mut(k) {
        *v.decor_mut() = old.decor().clone();
        *old = v;
        return;
    }
    t.insert(k, Item::Value(v));
}

pub fn remove_skills(root: &Path, global: bool, names: &[String]) -> Result<()> {
    edit(root, global, |doc| {
        if let Some(t) = doc.get_mut("skills").and_then(Item::as_table_like_mut) {
            for n in names {
                t.remove(n);
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_example() {
        let m = parse(
            r#"
[repositories]
acme = { type = "composer", url = "https://git.example.com/packages.json" }

[skills]
"netresearch/agent-jira-skill" = "^2.0"
"acme/agents-md" = { version = "^1.0", repository = "acme" }
"acme/review" = { version = "1.4.2", repository = "acme", skills = ["review"] }

[agents]
targets = [".claude/skills"]

[policy]
min-release-age = "12h"
"#,
            false,
        )
        .unwrap();
        assert_eq!(m.repositories.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["acme", "packagist"]);
        assert_eq!(m.skills.len(), 3);
        assert_eq!(m.skills[2].skills.as_deref(), Some(&["review".to_string()][..]));
        assert_eq!(m.targets, [".claude/skills"]);
        assert_eq!(m.policy.min_release_age, SignedDuration::from_hours(12));
    }

    #[test]
    fn default_sources() {
        let names = |m: &Manifest| m.repositories.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        let m = parse(&TEMPLATE.replace("[repositories]\n", "[repositories]\nzz = { url = \"https://z\" }\n"), false).unwrap();
        assert_eq!(names(&m), ["zz", "packagist"]);
        let listed_first =
            parse("[repositories]\npackagist = { url = \"https://repo.packagist.org\" }\nacme = { url = \"https://a\" }", false).unwrap();
        assert_eq!(names(&listed_first), ["acme", "packagist"], "Packagist is searched last");
        assert_eq!(m.github.as_deref(), Some(crate::github::API));
        let off = parse("[repositories]\npackagist = false\ngithub = false", false).unwrap();
        assert!(off.repositories.is_empty() && off.github.is_none());
        assert!(!off.policy.packagist);
        let ghe = parse("[repositories]\ngithub = { type = \"github\", url = \"https://ghe.example.com/api/v3/\" }", false).unwrap();
        assert_eq!(ghe.github.as_deref(), Some("https://ghe.example.com/api/v3"));
        assert!(parse("[repositories]\nmine = { type = \"github\", url = \"https://x\" }", false).is_err());
        assert!(parse("[repositories]\ngithub = { type = \"github\", url = \"https://x\", min-release-age = \"0\" }", false).is_err());
        assert!(parse("[policy]\nallow-sources = [\"nope\"]", false).unwrap_err().msg.contains("unknown repository"));
        let allow = parse("[repositories]\nacme = { url = \"https://a.example/packages.json\" }\n[policy]\nallow-sources = [\"acme\", \"https://b.example/\"]", false).unwrap();
        assert!(allow.allows("https://a.example/packages.json") && allow.allows("https://b.example/x/packages.json"));
        assert!(!allow.allows(PACKAGIST_URL) && !allow.allows("https://a.example/other.json"));
        assert_eq!(names(&allow), ["acme", "packagist"]);
        assert_eq!(allow.allowed_repositories().len(), 1);
    }

    #[test]
    fn defaults_and_rejections() {
        let m = parse(TEMPLATE, false).unwrap();
        assert!(parse(GLOBAL_TEMPLATE, true).is_ok());
        assert!(parse("[agents]\nlocal = \".skills\"", true).is_err());
        assert_eq!(m.policy.min_release_age, SignedDuration::from_hours(72));
        assert_eq!(m.targets, DEFAULT_TARGETS);
        assert!(parse("[repositories]\nx = { url = \"http://a\" }", false).is_err());
        assert!(parse("[skills]\n\"Acme/X\" = \"1.0\"", false).is_err());
        assert!(parse("[skills]\n\"a/b\" = { version = \"1\", repository = \"nope\" }", false).is_err());
        assert!(parse("[policy]\nallow-ranges = false\n[skills]\n\"a/b\" = \"^1.0\"", false).is_err());
        assert!(parse("[policy]\nallow-ranges = false\n[skills]\n\"a/b\" = \"1.0.3\"", false).is_ok());
        assert!(parse("[agents]\ntargets = [\"../x\"]", false).is_err());
        assert!(parse("[agents]\ntargets = [\"a\\\\b\"]", false).is_err());
        assert!(parse("[agents]\nlocal = \"..\\\\x\"", false).is_err());
        assert!(parse("[policy]\npackagist = false", false).unwrap().repositories.is_empty());
        let gh = |entry: &str| parse(&format!("[repositories]\nr = {{ url = \"https://r\" }}\n[skills]\n\"a/b\" = {entry}"), false);
        assert!(gh("{ github = true, ref = \"main\" }").is_ok());
        assert!(gh("{ github = true, version = \"^1.0\", ref = \"main\" }").is_err());
        assert!(gh("{ ref = \"main\" }").is_err());
        assert!(gh("{ github = true, repository = \"r\" }").is_err());
        for evil in ["../x", "a b", "-x", "a?b", "a//b", "x/"] {
            assert!(gh(&format!("{{ github = true, ref = {evil:?} }}")).is_err(), "{evil}");
        }
        let strict = |entry: &str| parse(&format!("[policy]\nallow-ranges = false\n[skills]\n\"a/b\" = {entry}"), false);
        assert!(strict("{ github = true }").is_err());
        assert!(strict(&format!("{{ github = true, ref = \"{}\" }}", "ab".repeat(20))).is_ok());
        let r = |entry: &str| parse(&format!("[skills]\n\"a/b\" = {entry}"), false);
        let ok = r(r#"{ version = "^1.0", rename = { code-review = "mp-review" } }"#).unwrap();
        assert_eq!(ok.skills[0].rename, BTreeMap::from([("code-review".to_string(), "mp-review".to_string())]));
        assert!(r(r#"{ version = "^1.0", rename = { a = "Bad Name" } }"#).is_err());
        assert!(r(r#"{ version = "^1.0", rename = { a = "x", b = "x" } }"#).is_err());
        assert!(r(r#"{ version = "^1.0", rename = ["x"] }"#).is_err());
        assert!(r(r#"{ version = "^1.0", rename = { a = "a" } }"#).unwrap().skills[0].rename.is_empty(), "identity renames are dropped");
        assert_eq!(parse_age("0").unwrap(), SignedDuration::ZERO);
        assert!(parse_age("3y").is_err());
    }

    #[test]
    fn edits_keep_comments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), TEMPLATE).unwrap();
        let req = SkillReq { name: "a/b".into(), constraint: "^1.0".into(), skills: Some(vec!["x".into()]), ..Default::default() };
        write_skill(dir.path(), false, &req).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.contains("# skillcask manifest"));
        assert_eq!(parse(&text, false).unwrap().skills, std::slice::from_ref(&req));
        for (constraint, github, line) in [
            (DEFAULT_REF, GitHub::Ref, r#""a/b" = { github = true }"#),
            ("feature/x", GitHub::Ref, r#""a/b" = { github = true, ref = "feature/x" }"#),
            ("^1.3", GitHub::Tags, r#""a/b" = { github = true, version = "^1.3" }"#),
        ] {
            let gh = SkillReq { constraint: constraint.into(), skills: None, github: Some(github), ..req.clone() };
            write_skill(dir.path(), false, &gh).unwrap();
            let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
            assert!(text.contains(line), "{text}");
            assert_eq!(parse(&text, false).unwrap().skills, [gh]);
        }
        let renamed = SkillReq { rename: BTreeMap::from([("x".into(), "y".into())]), ..req.clone() };
        write_skill(dir.path(), false, &renamed).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.contains(r#"rename = { x = "y" }"#), "{text}");
        assert_eq!(parse(&text, false).unwrap().skills, [renamed]);
        remove_skills(dir.path(), false, &["a/b".into()]).unwrap();
        assert!(parse(&std::fs::read_to_string(dir.path().join(FILE)).unwrap(), false).unwrap().skills.is_empty());
    }

    #[test]
    fn edits_keep_the_entry_form_and_drop_keys_that_no_longer_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let gh = SkillReq { name: "a/b".into(), constraint: "main".into(), github: Some(GitHub::Ref), ..Default::default() };
        for (before, after) in [
            (
                "[skills.\"a/b\"]\n# inner\ngithub = true\nversion = \"^1.0\"   # tags\nskills = [\"x\"]\n",
                "[skills.\"a/b\"]\n# inner\ngithub = true\nref = \"main\"\n",
            ),
            ("[skills]\n\"a/b\".github = true\n\"a/b\".version = \"^1.0\"\n", "[skills]\n\"a/b\".github = true\n\"a/b\".ref = \"main\"\n"),
            (
                "[skills]\n\"a/b\" = { github = true, version = \"^1.0\" }   # why\n",
                "[skills]\n\"a/b\" = { github = true, ref = \"main\" }   # why\n",
            ),
        ] {
            std::fs::write(&path, before).unwrap();
            write_skill(dir.path(), false, &gh).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
        }
        // Multi-line inline tables (TOML 1.1): comments stay on their lines, removed keys take
        // their own comments along.
        for (before, after) in [
            (
                "[skills]\n\"a/b\" = {\n  # source\n  github = true, # gh\n  # tags only\n  version = \"^1.0\", # pinned\n  skills = [\"x\"], # sel\n}\n",
                "[skills]\n\"a/b\" = {\n  # source\n  github = true, # gh\n  skills = [\"x\"], # sel\n  ref = \"main\",\n}\n",
            ),
            (
                "[skills]\n\"a/b\" = {\n  github = true, # gh\n  version = \"^1.0\" # last\n}\n",
                "[skills]\n\"a/b\" = {\n  github = true, # gh\n  ref = \"main\"\n}\n",
            ),
        ] {
            let sel = SkillReq { skills: before.contains("skills =").then(|| vec!["x".into()]), ..gh.clone() };
            std::fs::write(&path, before).unwrap();
            write_skill(dir.path(), false, &sel).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
        }
        std::fs::write(&path, "[skills]\n\"a/b\" = \"^1.0\"   # why\n").unwrap();
        write_skill(dir.path(), false, &gh).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[skills]\n\"a/b\" = { github = true, ref = \"main\" }   # why\n");
    }

    #[test]
    fn a_failed_first_edit_creates_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let bad = SkillReq { name: "a/b".into(), constraint: "^^1".into(), ..Default::default() };
        assert!(write_skill(dir.path(), false, &bad).is_err());
        assert!(!dir.path().join(FILE).exists());
        let ok = SkillReq { constraint: "^1.0".into(), ..bad };
        write_skill(dir.path(), true, &ok).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.starts_with("# skillcask global manifest") && text.contains("\"a/b\" = \"^1.0\""), "{text}");
    }

    #[test]
    fn enforced_age_binds_repositories() {
        let doc: DocumentMut = "[policy]\nmin-release-age = \"3d\"\nenforce = [\"min-release-age\"]\n[policy.repository-min-release-age]\n\"https://git.example.com/\" = \"0\"\n\"https://git.example.com/slow/\" = \"5d\"".parse().unwrap();
        let config = crate::policy::layer(doc.get("policy").and_then(Item::as_table_like), "config.toml", true).unwrap();
        let o = crate::policy::Outer { config, config_path: "config.toml".into(), env: Default::default() };
        let m = |repo: &str| parse_with(&format!("[repositories]\nr = {{ url = {repo} }}"), false, &o);
        let age = |m: Manifest| m.min_release_age(&m.repositories[0]);

        // Regression: a project's min-release-age = "0" used to bypass an enforced cooldown.
        let e = m(r#""https://other.example/packages.json", min-release-age = "0""#).unwrap_err();
        assert!(e.msg.contains("repository r: min-release-age \"0s\" is shorter than \"72h\" enforced by config.toml"), "{}", e.msg);
        assert_eq!(age(m(r#""https://other.example/packages.json""#).unwrap()), SignedDuration::from_hours(72));
        assert_eq!(age(m(r#""https://other.example/packages.json", min-release-age = "7d""#).unwrap()), SignedDuration::from_hours(168));
        assert_eq!(age(m(r#""https://git.example.com/packages.json", min-release-age = "0""#).unwrap()), SignedDuration::ZERO);
        assert_eq!(age(m(r#""https://git.example.com/packages.json""#).unwrap()), SignedDuration::ZERO);
        assert!(m(r#""https://git.example.com/slow/packages.json", min-release-age = "1d""#).is_err(), "longest prefix wins");
        let packagist = m(r#""https://other.example/packages.json", min-release-age = "4d""#).unwrap();
        assert_eq!(packagist.min_release_age(&packagist.repositories[1]), SignedDuration::from_hours(72));

        let env = crate::policy::env_layer(|k| (k == "SKILLCASK_MIN_RELEASE_AGE").then(|| "1h".to_string())).unwrap();
        let lifted = parse_with(
            "[repositories]\nr = { url = \"https://other.example/p.json\", min-release-age = \"0\" }",
            false,
            &crate::policy::Outer { env, ..o },
        );
        assert_eq!(age(lifted.unwrap()), SignedDuration::ZERO, "the environment lifts enforcement");
    }
}
