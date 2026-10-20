//! Composer repository client (v2 `metadata-url`, or v1 inline `packages`): metadata, skill
//! filtering, version selection.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value};
use url::Url;

use crate::bail;
use crate::error::{Error, MANIFEST, NETWORK, Result};
use crate::http::Http;
use crate::lock::Dist;
use crate::manifest::Repository;
use crate::version::{Constraint, Stability, Version};

#[derive(Clone, Debug)]
pub struct Candidate {
    pub name: String,
    pub version: String,
    pub parsed: Version,
    pub time: Option<Timestamp>,
    pub dist: Dist,
    pub dist_shasum: Option<String>,
    pub skill_paths: Vec<String>,
}

const SEARCH_LIMIT: usize = 30;

type Versions = Vec<Map<String, Value>>;

/// A repository's `packages.json`, read once per run.
struct Root {
    url: Url,
    json: Value,
}

pub struct Registry<'a> {
    http: &'a Http,
    roots: RefCell<HashMap<String, Rc<Root>>>,
}

fn packages_url(repo: &Repository) -> Result<Url> {
    // A query string means an API endpoint (e.g. GitLab raw files `.../raw?ref=main`), used as given.
    let raw = if repo.url.ends_with(".json") || repo.url.contains('?') {
        repo.url.clone()
    } else {
        format!("{}/packages.json", repo.url.trim_end_matches('/'))
    };
    Url::parse(&raw).map_err(|e| Error::new(MANIFEST, format!("repository {}: invalid url: {e}", repo.name)))
}

fn is_packagist(repo: &Repository) -> bool {
    Url::parse(&repo.url).ok().and_then(|u| u.host_str().map(|h| h == "packagist.org" || h.ends_with(".packagist.org"))).unwrap_or(false)
}

/// `None` when the root lists its packages and `package` is not among them.
fn metadata_url(root: &Value, base: &Url, package: &str) -> Result<Option<Url>> {
    let names = root.get("available-packages").and_then(Value::as_array);
    let patterns = root.get("available-package-patterns").and_then(Value::as_array);
    if names.is_some() || patterns.is_some() {
        let listed = names.into_iter().flatten().any(|n| n.as_str() == Some(package))
            || patterns.into_iter().flatten().filter_map(Value::as_str).any(|p| glob(p, package));
        if !listed {
            return Ok(None);
        }
    }
    let Some(tpl) = root.get("metadata-url").and_then(Value::as_str) else {
        if root.get("packages").is_some_and(Value::is_object) {
            return Ok(None);
        }
        bail!(MANIFEST, "{}: no metadata-url and no inline packages", crate::http::redact(base));
    };
    base.join(&tpl.replace("%package%", package)).map(Some).map_err(|e| Error::new(NETWORK, format!("bad metadata-url {tpl:?}: {e}")))
}

/// Composer v1 inline metadata: `packages.<name>` maps version to metadata (Satis); a list is
/// accepted too.
fn inline(root: &Value, package: &str) -> Option<Vec<Value>> {
    match root.get("packages")?.get(package)? {
        Value::Object(m) => Some(m.values().cloned().collect()),
        Value::Array(a) => Some(a.clone()),
        _ => None,
    }
}

fn glob(pattern: &str, s: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = s.strip_prefix(first) else { return false };
    let parts: Vec<&str> = parts.collect();
    for (i, p) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            return rest.ends_with(p);
        }
        match rest.find(p) {
            Some(at) => rest = &rest[at + p.len()..],
            None => return false,
        }
    }
    rest.is_empty()
}

/// Expands Composer's minified format: each version inherits fields from the previous one.
fn expand(versions: &[Value], minified: bool) -> Vec<Map<String, Value>> {
    let mut acc = Map::new();
    let mut out = Vec::new();
    for v in versions {
        let Some(obj) = v.as_object() else { continue };
        if !minified {
            out.push(obj.clone());
            continue;
        }
        for (k, val) in obj {
            if val.as_str() == Some("__unset") {
                acc.remove(k);
            } else {
                acc.insert(k.clone(), val.clone());
            }
        }
        out.push(acc.clone());
    }
    out
}

/// A skill package: `type` is `ai-agent-skill`, or `extra.ai-agent-skill` names its skills.
fn is_skill(m: &Map<String, Value>) -> bool {
    m.get("type").and_then(Value::as_str) == Some("ai-agent-skill") || m.get("extra").and_then(|e| e.get("ai-agent-skill")).is_some()
}

/// Composer writes `time` with or without an offset; without one it is UTC.
fn release_time(t: &str) -> Option<Timestamp> {
    t.parse().ok().or_else(|| t.parse::<jiff::civil::DateTime>().ok()?.to_zoned(jiff::tz::TimeZone::UTC).ok().map(|z| z.timestamp()))
}

fn candidates(package: &str, versions: Vec<Map<String, Value>>, base: &Url) -> Vec<Candidate> {
    let s = |m: &Map<String, Value>, path: &[&str]| -> Option<String> {
        let mut v = m.get(path[0])?;
        for k in &path[1..] {
            v = v.get(k)?;
        }
        v.as_str().filter(|s| !s.is_empty()).map(String::from)
    };
    let mut out = Vec::new();
    for m in versions {
        if !is_skill(&m) {
            continue;
        }
        let extra = m.get("extra").and_then(|e| e.get("ai-agent-skill"));
        let Some(version) = s(&m, &["version"]) else { continue };
        let Some(parsed) = s(&m, &["version_normalized"]).and_then(|n| Version::parse(&n)).or_else(|| Version::parse(&version)) else {
            continue;
        };
        // Userinfo of the repository URL must not end up in the lock.
        let dist_url = s(&m, &["dist", "url"]).and_then(|u| base.join(&u).ok()).map(|mut u| {
            crate::http::strip_userinfo(&mut u);
            u
        });
        let (Some("zip"), Some(url)) = (s(&m, &["dist", "type"]).as_deref(), dist_url) else {
            crate::ui::warn(format!("skipping {package} {version}: no zip dist"));
            continue;
        };
        let skill_paths = match extra {
            Some(Value::String(p)) => vec![p.clone()],
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
            _ => vec![crate::skill::SKILL_MD.to_string()],
        };
        let time = s(&m, &["time"]).and_then(|t| {
            let parsed = release_time(&t);
            if parsed.is_none() {
                crate::ui::warn(format!("{package} {version}: unrecognised release time {t:?}"));
            }
            parsed
        });
        out.push(Candidate {
            name: package.to_string(),
            parsed,
            time,
            dist: Dist { url: url.to_string(), reference: s(&m, &["dist", "reference"]).or_else(|| s(&m, &["source", "reference"])) },
            dist_shasum: s(&m, &["dist", "shasum"]),
            skill_paths,
            version,
        });
    }
    out.sort_by(|a, b| b.parsed.cmp(&a.parsed));
    out
}

impl<'a> Registry<'a> {
    pub fn new(http: &'a Http) -> Self {
        Registry { http, roots: RefCell::new(HashMap::new()) }
    }

    fn root(&self, repo: &Repository) -> Result<Rc<Root>> {
        if let Some(r) = self.roots.borrow().get(&repo.url) {
            return Ok(r.clone());
        }
        let url = packages_url(repo)?;
        let json = self
            .http
            .get_json(url.as_str())?
            .ok_or_else(|| Error::new(NETWORK, format!("repository {}: {} not found", repo.name, crate::http::redact(&url))))?;
        let r = Rc::new(Root { url, json });
        self.roots.borrow_mut().insert(repo.url.clone(), r.clone());
        Ok(r)
    }

    pub fn versions(&self, repo: &Repository, package: &str) -> Result<Option<Vec<Candidate>>> {
        Ok(self.metadata(repo, package)?.map(|(list, base)| candidates(package, list, &base)))
    }

    /// Every version's metadata, expanded, and the URL relative dist URLs resolve against.
    fn metadata(&self, repo: &Repository, package: &str) -> Result<Option<(Versions, Url)>> {
        let root = self.root(repo)?;
        if let Some(list) = inline(&root.json, package) {
            return Ok(Some((expand(&list, false), root.url.clone())));
        }
        let Some(meta) = metadata_url(&root.json, &root.url, package)? else { return Ok(None) };
        let Some(p2) = self.http.get_json(meta.as_str())? else { return Ok(None) };
        let Some(list) = p2.get("packages").and_then(|p| p.get(package)).and_then(Value::as_array) else { return Ok(None) };
        let minified = p2.get("minified").and_then(Value::as_str) == Some("composer/2.0");
        Ok(Some((expand(list, minified), meta)))
    }

    pub fn find(&self, repos: &[Repository], package: &str, only: Option<&str>) -> Result<(Repository, Vec<Candidate>)> {
        let searched: Vec<&Repository> = match only {
            Some(n) => vec![repos.iter().find(|r| r.name == n).ok_or_else(|| Error::new(MANIFEST, format!("unknown repository {n:?}")))?],
            None => repos.iter().collect(),
        };
        for repo in &searched {
            if let Some(c) = self.versions(repo, package)? {
                if c.is_empty() {
                    bail!(MANIFEST, "{package} in repository {} has no installable ai-agent-skill versions", repo.name);
                }
                return Ok(((*repo).clone(), c));
            }
        }
        let names: Vec<&str> = searched.iter().map(|r| r.name.as_str()).collect();
        bail!(MANIFEST, "package {package} not found in repositories {}", names.join(", "))
    }

    /// (name, description) of skill packages whose name contains `query`, or that Packagist's
    /// search finds. A repository that fails is skipped with a warning; all failing is an error.
    pub fn search(&self, repos: &[Repository], query: &str) -> Result<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        let (mut failed, mut errors) = (None, 0);
        for repo in repos {
            let found = if is_packagist(repo) { self.search_packagist(query) } else { self.search_names(repo, query) };
            match found {
                Ok(f) => out.extend(f),
                Err(e) => {
                    crate::ui::warn(format!("repository {}: {}", repo.name, e.msg));
                    (failed, errors) = (Some(e), errors + 1);
                }
            }
        }
        if let Some(e) = failed.filter(|_| errors == repos.len()) {
            return Err(e);
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|(n, _)| seen.insert(n.clone()));
        Ok(out)
    }

    fn search_packagist(&self, query: &str) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut url = Url::parse_with_params("https://packagist.org/search.json", [("q", query), ("type", "ai-agent-skill")])
            .map_err(|e| Error::new(NETWORK, e.to_string()))?
            .to_string();
        for _ in 0..3 {
            let Some(page) = self.http.get_json(&url)? else { break };
            for r in page.get("results").and_then(Value::as_array).into_iter().flatten() {
                if let Some(n) = r.get("name").and_then(Value::as_str) {
                    out.push((n.to_string(), r.get("description").and_then(Value::as_str).unwrap_or("").to_string()));
                }
            }
            match page.get("next").and_then(Value::as_str) {
                Some(n) => url = n.to_string(),
                None => break,
            }
        }
        Ok(out)
    }

    /// Names come from `available-packages`, inline `packages` or, on GitLab, `provider-includes`;
    /// each match's metadata is read to keep skill packages only.
    fn search_names(&self, repo: &Repository, query: &str) -> Result<Vec<(String, String)>> {
        let root = self.root(repo)?;
        let q = query.to_lowercase();
        let mut names: Vec<String> = root
            .json
            .get("available-packages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        names.extend(root.json.get("packages").and_then(Value::as_object).into_iter().flat_map(|m| m.keys().cloned()));
        for (tpl, h) in root.json.get("provider-includes").and_then(Value::as_object).into_iter().flatten() {
            let Some(hash) = h.get("sha256").and_then(Value::as_str) else { continue };
            let url = root
                .url
                .join(&tpl.replace("%hash%", hash))
                .map_err(|e| Error::new(NETWORK, format!("bad provider-includes {tpl:?}: {e}")))?;
            let providers = self.http.get_json(url.as_str())?;
            names.extend(providers.as_ref().and_then(|p| p.get("providers")?.as_object()).into_iter().flat_map(|m| m.keys().cloned()));
        }
        names.sort();
        names.dedup();
        names.retain(|n| n.contains(&q));
        // ponytail: one metadata request per match, capped; a server-side search would lift the cap.
        if names.len() > SEARCH_LIMIT {
            crate::ui::warn(format!("repository {}: {} packages match; only the first {SEARCH_LIMIT} are checked", repo.name, names.len()));
            names.truncate(SEARCH_LIMIT);
        }
        let mut out = Vec::new();
        for n in names {
            let Some((list, _)) = self.metadata(repo, &n)? else { continue };
            if let Some(m) = list.iter().find(|m| is_skill(m)) {
                out.push((n, m.get("description").and_then(Value::as_str).unwrap_or("").to_string()));
            }
        }
        Ok(out)
    }
}

pub fn select(cands: &[Candidate], name: &str, constraint: &str, min_age: SignedDuration, now: Timestamp) -> Result<Candidate> {
    let c = Constraint::parse(constraint).map_err(|e| Error::new(MANIFEST, format!("{name}: {e}")))?;
    let Some(newest) = newest(cands, Some(&c)) else {
        let latest = cands.iter().max_by(|a, b| a.parsed.cmp(&b.parsed)).map(|c| format!(" (newest: {})", c.version)).unwrap_or_default();
        bail!(MANIFEST, "no version of {name} matches {constraint:?}{latest}");
    };
    let cutoff = now.checked_sub(min_age).unwrap_or(Timestamp::MIN);
    // Equal versions (`v1.0.0` and `1.0.0`): prefer the timed, older one, so an untimed duplicate cannot skip the cooldown.
    let age = |x: &Candidate| x.time.unwrap_or(Timestamp::MAX);
    let pick = cands
        .iter()
        .filter(|x| c.allows(&x.parsed) && x.time.is_none_or(|t| t <= cutoff))
        .max_by(|a, b| a.parsed.cmp(&b.parsed).then_with(|| age(b).cmp(&age(a))));
    match pick {
        Some(p) => {
            if p.time.is_none() {
                crate::ui::warn(format!("{name} {} has no release time; cooldown not applied", p.version));
            }
            Ok(p.clone())
        }
        None => {
            let t = newest.time.expect("only timed versions are filtered out");
            let eligible = t.checked_add(min_age).unwrap_or(Timestamp::MAX);
            bail!(
                MANIFEST,
                "{name}: every version matching {constraint:?} is newer than policy.min-release-age ({min_age:#}); newest {} was released {} and becomes eligible at {}",
                newest.version,
                t.round(jiff::Unit::Second).unwrap_or(t),
                eligible.round(jiff::Unit::Second).unwrap_or(eligible)
            )
        }
    }
}

pub fn newest<'c>(cands: &'c [Candidate], constraint: Option<&Constraint>) -> Option<&'c Candidate> {
    cands
        .iter()
        .filter(|x| match constraint {
            Some(c) => c.allows(&x.parsed),
            None => x.parsed.stability >= Stability::Stable,
        })
        .max_by(|a, b| a.parsed.cmp(&b.parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p2() -> Value {
        json!({"minified": "composer/2.0", "packages": {"acme/x": [
            {"name": "acme/x", "description": "d", "version": "v2.0.0", "version_normalized": "2.0.0.0", "type": "ai-agent-skill",
             "time": "2026-10-19T11:00:00+00:00",
             "dist": {"type": "zip", "url": "/dist/x-2.0.0.zip", "reference": "r2", "shasum": ""},
             "extra": {"ai-agent-skill": ["skills/a/SKILL.md", "skills/b/SKILL.md"]}},
            {"version": "v1.1.0", "version_normalized": "1.1.0.0", "time": "2026-10-01T00:00:00+00:00",
             "dist": {"type": "zip", "url": "https://cdn.example.com/x-1.1.0.zip", "shasum": "abc"},
             "source": {"type": "git", "url": "https://g", "reference": "s11"}, "extra": "__unset"},
            {"version": "v1.0.0", "version_normalized": "1.0.0.0", "time": "__unset"},
            {"version": "dev-main", "version_normalized": "dev-main"},
            {"version": "v0.9.0", "version_normalized": "0.9.0.0", "type": "library"},
            {"version": "v0.8.0", "version_normalized": "0.8.0.0", "type": "ai-agent-skill", "dist": {"type": "tar", "url": "https://a/b.tar"}}
        ]}})
    }

    fn cands() -> Vec<Candidate> {
        let v = p2();
        let list = v["packages"]["acme/x"].as_array().unwrap();
        candidates("acme/x", expand(list, true), &Url::parse("https://repo.example.com/p2/acme/x.json").unwrap())
    }

    #[test]
    fn expands_minified_and_filters() {
        let c = cands();
        assert_eq!(c.iter().map(|c| c.version.as_str()).collect::<Vec<_>>(), ["v2.0.0", "v1.1.0", "v1.0.0"]);
        assert_eq!(c[0].dist.url, "https://repo.example.com/dist/x-2.0.0.zip");
        assert_eq!(c[0].skill_paths, ["skills/a/SKILL.md", "skills/b/SKILL.md"]);
        assert_eq!(c[0].dist_shasum, None);
        assert_eq!(c[1].skill_paths, ["SKILL.md"], "__unset removes extra; type still inherited");
        assert_eq!(c[1].dist.reference.as_deref(), Some("s11"));
        assert_eq!(c[1].dist_shasum.as_deref(), Some("abc"));
        assert_eq!(c[2].time, None);
        let plain = expand(&[json!({"version": "1"}), json!({"x": 1})], false);
        assert!(plain[1].get("version").is_none());
    }

    #[test]
    fn selects_with_cooldown() {
        let c = cands();
        let now: Timestamp = "2026-10-19T12:00:00Z".parse().unwrap();
        let three_days = SignedDuration::from_hours(72);
        assert_eq!(select(&c, "acme/x", "^1.0", three_days, now).unwrap().version, "v1.1.0");
        assert_eq!(select(&c, "acme/x", "*", SignedDuration::ZERO, now).unwrap().version, "v2.0.0");
        assert_eq!(select(&c, "acme/x", "*", three_days, now).unwrap().version, "v1.1.0");
        let e = select(&c, "acme/x", "^2.0", three_days, now).unwrap_err();
        assert_eq!(e.code, MANIFEST);
        assert!(e.msg.contains("v2.0.0") && e.msg.contains("2026-10-22T11:00:00Z") && e.msg.contains("min-release-age"), "{}", e.msg);
        let e = select(&c, "acme/x", "^3.0", three_days, now).unwrap_err();
        assert!(e.msg.contains("no version of acme/x matches") && e.msg.contains("newest: v2.0.0"), "{}", e.msg);
        // 1.0.0 has no time: kept despite cooldown
        assert_eq!(select(&c, "acme/x", "1.0.0", SignedDuration::from_hours(100_000), now).unwrap().version, "v1.0.0");
        assert_eq!(newest(&c, None).unwrap().version, "v2.0.0");
        assert_eq!(newest(&c, Some(&Constraint::parse("~1.0").unwrap())).unwrap().version, "v1.1.0");
    }

    #[test]
    fn equal_versions_prefer_the_timed_one() {
        let now: Timestamp = "2026-10-19T12:00:00Z".parse().unwrap();
        let mut c = cands();
        c.truncate(2);
        let mut untimed = c[1].clone();
        untimed.version = "1.1.0".into();
        untimed.time = None;
        c.push(untimed);
        let picked = select(&c, "acme/x", "^1.0", SignedDuration::from_hours(72), now).unwrap();
        assert_eq!((picked.version.as_str(), picked.time.is_some()), ("v1.1.0", true));
    }

    #[test]
    fn times_without_offset_are_utc() {
        let v = |t: &str| json!({"version": "1.0.0", "type": "ai-agent-skill", "time": t, "dist": {"type": "zip", "url": "/x.zip"}});
        let base = Url::parse("https://repo.example.com/p2/acme/x.json").unwrap();
        let utc: Timestamp = "2026-10-19T11:00:00Z".parse().unwrap();
        for t in ["2026-10-19 11:00:00", "2026-10-19T11:00:00", "2026-10-19T11:00:00+00:00"] {
            let c = candidates("acme/x", expand(&[v(t)], false), &base);
            assert_eq!(c[0].time, Some(utc), "{t}");
            let now: Timestamp = "2026-10-19T12:00:00Z".parse().unwrap();
            assert!(select(&c, "acme/x", "*", SignedDuration::from_hours(72), now).is_err(), "cooldown applies to {t}");
        }
        assert_eq!(candidates("acme/x", expand(&[v("yesterday")], false), &base)[0].time, None);
    }

    #[test]
    fn userinfo_is_not_copied_into_dist_urls() {
        let v = json!({"version": "1.0.0", "type": "ai-agent-skill", "dist": {"type": "zip", "url": "/dist/x.zip"}});
        let base = Url::parse("https://u:secret@repo.example.com/p2/acme/x.json").unwrap();
        assert_eq!(candidates("acme/x", vec![v.as_object().unwrap().clone()], &base)[0].dist.url, "https://repo.example.com/dist/x.zip");
        let abs =
            json!({"version": "1.0.0", "type": "ai-agent-skill", "dist": {"type": "zip", "url": "https://u:p@cdn.example.com/x.zip"}});
        assert_eq!(candidates("acme/x", vec![abs.as_object().unwrap().clone()], &base)[0].dist.url, "https://cdn.example.com/x.zip");
    }

    #[test]
    fn huge_min_release_age_fails_closed() {
        use crate::manifest::parse_age;
        assert_eq!(parse_age("36525d").unwrap(), SignedDuration::from_hours(36525 * 24));
        assert!(parse_age("36526d").is_err(), "over 100 years");
        assert!(parse_age("9223372036854775807w").is_err(), "overflow");
        let c = cands();
        let now: Timestamp = "2026-10-19T12:00:00Z".parse().unwrap();
        let e = select(&c, "acme/x", "^1.1", SignedDuration::MAX, now).unwrap_err();
        assert!(e.msg.contains("min-release-age"), "{}", e.msg);
    }

    #[test]
    fn skill_packages_by_type_or_extra() {
        for (meta, skill) in [
            (json!({"type": "ai-agent-skill"}), true),
            (json!({"type": "library", "extra": {"ai-agent-skill": "skills/a/SKILL.md"}}), true),
            (json!({"extra": {"ai-agent-skill": ["a/SKILL.md"]}}), true),
            (json!({"type": "library"}), false),
            (json!({"type": "ai-agent-skill-x", "extra": {"other": 1}}), false),
            (json!({"type": ["ai-agent-skill"]}), false),
            (json!({}), false),
        ] {
            assert_eq!(is_skill(meta.as_object().unwrap()), skill, "{meta}");
        }
    }

    #[test]
    fn reads_inline_packages() {
        let v = json!({"version": "1.0.0", "type": "ai-agent-skill", "dist": {"type": "zip", "url": "https://h/x.zip"}});
        let root = json!({"packages": {"acme/x": {"1.0.0": v}, "acme/y": [v]}});
        assert_eq!(inline(&root, "acme/x").unwrap(), std::slice::from_ref(&v));
        assert_eq!(inline(&root, "acme/y").unwrap(), [v]);
        assert!(inline(&root, "acme/z").is_none());
        assert!(inline(&json!({"packages": []}), "acme/x").is_none());
    }

    #[test]
    fn resolves_metadata_url() {
        let base = Url::parse("https://git.example.com/api/v4/group/42/-/packages/composer/packages.json").unwrap();
        let root = json!({"metadata-url": "/api/v4/group/42/-/packages/composer/p2/%package%.json"});
        assert_eq!(
            metadata_url(&root, &base, "acme/x").unwrap().unwrap().as_str(),
            "https://git.example.com/api/v4/group/42/-/packages/composer/p2/acme/x.json"
        );
        let root =
            json!({"metadata-url": "p2/%package%.json", "available-packages": ["acme/x"], "available-package-patterns": ["other/*"]});
        let base = Url::parse("https://static.example.com/skills/packages.json").unwrap();
        assert_eq!(metadata_url(&root, &base, "acme/x").unwrap().unwrap().as_str(), "https://static.example.com/skills/p2/acme/x.json");
        assert!(metadata_url(&root, &base, "other/y").unwrap().is_some());
        assert!(metadata_url(&root, &base, "acme/nope").unwrap().is_none());
        assert!(metadata_url(&json!({}), &base, "acme/x").is_err());
        assert!(metadata_url(&json!({"packages": {"acme/y": {}}}), &base, "acme/x").unwrap().is_none());
        let packagist = Repository { name: "packagist".into(), url: crate::manifest::PACKAGIST_URL.into(), ..Default::default() };
        assert_eq!(packages_url(&packagist).unwrap().as_str(), "https://repo.packagist.org/packages.json");
        assert!(is_packagist(&packagist));
        let raw = "https://git.example.com/api/v4/projects/7/repository/files/composer%2Fpackages.json/raw?ref=main";
        assert_eq!(packages_url(&Repository { name: "r".into(), url: raw.into(), ..Default::default() }).unwrap().as_str(), raw);
        assert!(glob("a*c", "abbc") && glob("*", "x") && !glob("a*c", "abd") && glob("v/*", "v/x"));
    }
}
