//! Policy layers (D18): environment, enforced keys of the user `config.toml`, the manifest's
//! `[policy]`, the rest of `config.toml`, built-in defaults.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use jiff::SignedDuration;
use toml_edit::{DocumentMut, Item, TableLike};

use crate::bail;
use crate::error::{Error, MANIFEST, Result};

pub const CONFIG: &str = "config.toml";
pub const REPOSITORY_AGES: &str = "repository-min-release-age";
pub const KEYS: [&str; 5] = ["min-release-age", "allow-ranges", "packagist", "github-requires", "allow-sources"];

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Age(SignedDuration),
    Bool(bool),
    List(Vec<String>),
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Age(a) => write!(f, "\"{a:#}\""),
            Value::Bool(b) => write!(f, "{b}"),
            Value::List(l) => {
                // Shown in `status` and warnings; a URL prefix may carry a token.
                let shown = |s: &String| url::Url::parse(s).map_or_else(|_| s.clone(), |u| crate::http::redact(&u));
                write!(f, "[{}]", l.iter().map(|s| format!("{:?}", shown(s))).collect::<Vec<_>>().join(", "))
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layer {
    pub values: BTreeMap<&'static str, Value>,
    pub enforce: BTreeSet<&'static str>,
    /// `config.toml` only: URL prefix to the cooldown an enforced `min-release-age` allows for
    /// repositories under it (D28).
    pub repository_ages: BTreeMap<String, SignedDuration>,
}

/// The layers around a manifest: the user config and the environment.
#[derive(Clone, Debug, Default)]
pub struct Outer {
    pub config: Layer,
    /// Shown in messages; empty when there is no config file.
    pub config_path: String,
    pub env: Layer,
}

/// Each key that is set, its value and the layer it came from.
pub type Effective = BTreeMap<&'static str, (Value, &'static str)>;

fn key(k: &str) -> Option<&'static str> {
    KEYS.iter().copied().find(|x| *x == k)
}

/// One `allow-sources` entry: `packagist`, `github`, an `https://` prefix, or (with `names`) a
/// repository name, checked against `[repositories]` by the manifest.
fn check_source(s: &str, names: bool) -> std::result::Result<(), String> {
    let name = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if matches!(s, "packagist" | "github") || s.starts_with("https://") && url::Url::parse(s).is_ok() || names && name {
        return Ok(());
    }
    Err(if name {
        format!("{s:?}: repository names are only accepted in skills.toml; use packagist, github or an https:// URL prefix")
    } else {
        format!("{s:?} is not packagist, github, an https:// URL prefix or a repository name")
    })
}

fn value(k: &'static str, raw: Raw, names: bool) -> std::result::Result<Value, String> {
    match (k, raw) {
        ("min-release-age", Raw::Str(s)) => crate::manifest::parse_age(&s).map(Value::Age).map_err(|e| e.msg),
        ("min-release-age", _) => Err("must be a string such as \"3d\"".into()),
        ("allow-sources", Raw::List(l)) => {
            l.iter().try_for_each(|s| check_source(s, names))?;
            Ok(Value::List(l))
        }
        ("allow-sources", _) => Err("must be a list".into()),
        (_, Raw::Bool(b)) => Ok(Value::Bool(b)),
        (_, Raw::Str(s)) if s == "true" || s == "false" => Ok(Value::Bool(s == "true")),
        _ => Err("must be true or false".into()),
    }
}

enum Raw {
    Str(String),
    Bool(bool),
    List(Vec<String>),
}

fn raw(i: &Item) -> Option<Raw> {
    if let Some(s) = i.as_str() {
        return Some(Raw::Str(s.into()));
    }
    if let Some(b) = i.as_bool() {
        return Some(Raw::Bool(b));
    }
    i.as_array()?.iter().map(|v| v.as_str().map(String::from)).collect::<Option<Vec<_>>>().map(Raw::List)
}

/// `[policy]` of a manifest (`config = false`: unknown keys are left to other versions) or of
/// `config.toml` (`config = true`: `enforce` allowed, unknown keys and repository names refused).
pub fn layer(t: Option<&dyn TableLike>, file: &str, config: bool) -> Result<Layer> {
    let bad = |k: &str, e: String| Error::new(MANIFEST, format!("{file}: policy.{k} {e}"));
    let mut out = Layer::default();
    let Some(t) = t else { return Ok(out) };
    for (k, item) in t.iter() {
        if config && k == "enforce" {
            for e in
                raw(item).and_then(|r| if let Raw::List(l) = r { Some(l) } else { None }).ok_or_else(|| bad(k, "must be a list".into()))?
            {
                out.enforce.insert(key(&e).ok_or_else(|| bad(k, format!("names unknown key {e:?}")))?);
            }
            continue;
        }
        if config && k == REPOSITORY_AGES {
            let t = item.as_table_like().ok_or_else(|| bad(k, "must be a table of URL prefix = age".into()))?;
            for (prefix, age) in t.iter() {
                if !prefix.starts_with("https://") || prefix.len() == "https://".len() {
                    return Err(bad(k, format!("{prefix:?} is not an https:// URL prefix")));
                }
                let age = age.as_str().ok_or_else(|| bad(k, format!("{prefix:?} must be a string such as \"3d\"")))?;
                let age = crate::manifest::parse_age(age).map_err(|e| bad(k, format!("{prefix:?}: {}", e.msg)))?;
                out.repository_ages.insert(prefix.to_string(), age);
            }
            continue;
        }
        let Some(k) = key(k) else {
            if config {
                bail!(MANIFEST, "{file}: unknown key policy.{k}");
            }
            continue;
        };
        let v = raw(item).ok_or_else(|| bad(k, "has an unsupported type".into()))?;
        out.values.insert(k, value(k, v, !config).map_err(|e| bad(k, e))?);
    }
    if let Some(k) = out.enforce.iter().find(|k| !out.values.contains_key(*k)) {
        bail!(MANIFEST, "{file}: policy.enforce lists {k}, which policy does not set");
    }
    if !out.repository_ages.is_empty() && !out.enforce.contains("min-release-age") {
        bail!(MANIFEST, "{file}: policy.{REPOSITORY_AGES} needs min-release-age in policy.enforce");
    }
    Ok(out)
}

/// `SKILLCASK_MIN_RELEASE_AGE` etc.; empty counts as unset, lists are comma-separated.
pub fn env_layer(var: impl Fn(&str) -> Option<String>) -> Result<Layer> {
    let mut out = Layer::default();
    for k in KEYS {
        let name = format!("SKILLCASK_{}", k.to_uppercase().replace('-', "_"));
        let Some(s) = var(&name).filter(|s| !s.trim().is_empty()) else { continue };
        let r = if k == "allow-sources" {
            Raw::List(s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect())
        } else {
            Raw::Str(s.trim().to_string())
        };
        out.values.insert(k, value(k, r, false).map_err(|e| Error::new(MANIFEST, format!("{name} {e}")))?);
    }
    Ok(out)
}

/// The user config next to the global manifest, and the environment.
pub fn outer() -> Result<Outer> {
    let env = env_layer(|k| std::env::var(k).ok())?;
    let Some(path) = crate::scope::config_dir().map(|d| d.join(CONFIG)) else {
        return Ok(Outer { env, ..Default::default() });
    };
    let (config, config_path) = read_config(&path)?;
    Ok(Outer { config, config_path, env })
}

fn read_config(path: &Path) -> Result<(Layer, String)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Layer::default(), String::new())),
        Err(e) => return Err(e.into()),
    };
    let shown = path.display().to_string();
    let doc: DocumentMut = text.parse().map_err(|e| Error::new(MANIFEST, format!("{shown}: {e}")))?;
    if let Some((k, _)) = doc.iter().find(|(k, _)| *k != "policy") {
        bail!(MANIFEST, "{shown}: only [policy] is read here, found {k}");
    }
    Ok((layer(doc.get("policy").and_then(Item::as_table_like), &shown, true)?, shown))
}

/// Set once the environment-over-`enforce` warnings are printed: a run loads the manifest more than once.
static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const ENVIRONMENT: &str = "environment";
const ENFORCED: &str = "config.toml, enforced";

/// Merges the layers; a manifest value that differs from an enforced one is an error.
pub fn resolve(project: &Layer, o: &Outer) -> Result<Effective> {
    let mut out = Effective::new();
    let mut overrides = Vec::new();
    for k in KEYS {
        let enforced = o.config.enforce.contains(k).then(|| o.config.values.get(k)).flatten();
        if let (Some(e), Some(v)) = (enforced, o.env.values.get(k))
            && e != v
        {
            let var = format!("SKILLCASK_{}", k.to_uppercase().replace('-', "_"));
            overrides.push(format!("{var} overrides policy.{k} = {e} enforced by {}; using {v}", o.config_path));
        }
        // With the key in the environment the project's value is unused, so it cannot clash.
        if let (Some(e), Some(p), None) = (enforced, project.values.get(k), o.env.values.get(k))
            && e != p
        {
            return Err(Error::new(MANIFEST, format!("policy.{k} is enforced as {e} by {}", o.config_path)).hint(format!(
                "remove policy.{k} from {}, or ask whoever manages {} to lift it",
                crate::manifest::FILE,
                o.config_path
            )));
        }
        let pick = if let Some(v) = o.env.values.get(k) {
            Some((v, ENVIRONMENT))
        } else if let Some(v) = enforced {
            Some((v, ENFORCED))
        } else if let Some(v) = project.values.get(k) {
            Some((v, "skills.toml"))
        } else {
            o.config.values.get(k).map(|v| (v, "config.toml"))
        };
        if let Some((v, from)) = pick {
            out.insert(k, (v.clone(), from));
        }
    }
    if !overrides.is_empty() && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        for w in overrides {
            crate::ui::warn(w);
        }
    }
    Ok(out)
}

/// Whether `k` comes from a layer the project cannot loosen: the environment or an enforced key.
pub fn fixed(e: &Effective, k: &str) -> bool {
    matches!(e.get(k), Some((_, ENVIRONMENT | ENFORCED)))
}

/// With `min-release-age` enforced, the shortest cooldown a project may give the repository at
/// `url`: the longest matching prefix of `repository-min-release-age`, else the enforced value.
pub fn repository_floor(o: &Outer, url: &str, enforced: SignedDuration) -> SignedDuration {
    o.config.repository_ages.iter().filter(|(p, _)| under(url, p)).max_by_key(|(p, _)| p.len()).map_or(enforced, |(_, a)| *a)
}

/// `url` is `prefix` or below it on a `/` (or query) boundary, both compared as parsed URLs, as the
/// request is made: scheme, host and port equal, `..`, `%2e%2e` and `\` already resolved. Below the
/// prefix a `..` behind `%2F` or `%5C`, or with a path parameter (`..;v=1`), is refused too, for
/// servers that decode or strip parameters before resolving. A prefix with a query matches only
/// itself; a side that does not parse matches nothing.
pub fn under(url: &str, prefix: &str) -> bool {
    let (Ok(u), Ok(p)) = (url::Url::parse(url), url::Url::parse(prefix)) else { return false };
    if u.origin() != p.origin() {
        return false;
    }
    if p.query().is_some() {
        return u.as_str() == p.as_str();
    }
    let (path, base) = (u.path(), p.path());
    let inside = path == base || path.starts_with(base) && (base.ends_with('/') || path[base.len()..].starts_with('/'));
    let dotdot = |s: &str| s.split(';').next() == Some("..");
    inside && !percent_encoding::percent_decode_str(&path[base.len()..]).decode_utf8_lossy().split(['/', '\\']).any(dotdot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Result<Layer> {
        let doc: DocumentMut = text.parse().unwrap();
        layer(doc.get("policy").and_then(Item::as_table_like), "config.toml", true)
    }

    fn proj(text: &str) -> Layer {
        let doc: DocumentMut = text.parse().unwrap();
        layer(doc.get("policy").and_then(Item::as_table_like), "skills.toml", false).unwrap()
    }

    #[test]
    fn layers_in_order() {
        let config = cfg("[policy]\nallow-sources = [\"https://git.example.com/\"]\nmin-release-age = \"1d\"\nallow-ranges = false\nenforce = [\"allow-sources\"]").unwrap();
        let o = Outer { config, config_path: "config.toml".into(), env: Layer::default() };
        let p = proj("[policy]\nmin-release-age = \"2d\"");
        let e = resolve(&p, &o).unwrap();
        assert_eq!(e["min-release-age"], (Value::Age(SignedDuration::from_hours(48)), "skills.toml"));
        assert_eq!(e["allow-ranges"], (Value::Bool(false), "config.toml"));
        assert_eq!(e["allow-sources"].1, "config.toml, enforced");
        assert!(!e.contains_key("packagist"));

        let clash = proj("[policy]\nallow-sources = [\"acme\"]");
        assert!(resolve(&clash, &o).unwrap_err().msg.contains("policy.allow-sources is enforced"));
        let same = proj("[policy]\nallow-sources = [\"https://git.example.com/\"]");
        assert!(resolve(&same, &o).is_ok());

        let env = env_layer(|k| (k == "SKILLCASK_ALLOW_SOURCES").then(|| "github, packagist".to_string())).unwrap();
        let e = resolve(&clash, &Outer { env, ..o }).unwrap();
        assert_eq!(e["allow-sources"], (Value::List(vec!["github".into(), "packagist".into()]), "environment"));
    }

    #[test]
    fn rejects_bad_config() {
        assert!(cfg("[policy]\nallow-sources = [\"acme\"]").unwrap_err().msg.contains("only accepted in skills.toml"));
        assert!(cfg("[policy]\nallow-ranges = 1").is_err());
        assert!(cfg("[policy]\nallow-sources = [\"https://exa mple.com/\"]").is_err());
        assert!(cfg("[policy]\nmin-release-age = 3").is_err());
        assert!(cfg("[policy]\ntypo = true").unwrap_err().msg.contains("unknown key"));
        assert!(cfg("[policy]\nenforce = [\"packagist\"]").unwrap_err().msg.contains("does not set"));
        assert!(cfg("[policy]\nenforce = [\"nope\"]").is_err());
        let ages = "[policy]\nmin-release-age = \"1d\"\n[policy.repository-min-release-age]\n";
        assert!(cfg(&format!("{ages}\"https://a/\" = \"0\"")).unwrap_err().msg.contains("needs min-release-age in policy.enforce"));
        let ages = ages.replace("\n[", "\nenforce = [\"min-release-age\"]\n[");
        assert!(cfg(&format!("{ages}\"https://a/\" = \"0\"")).is_ok());
        assert!(cfg(&format!("{ages}acme = \"0\"")).unwrap_err().msg.contains("not an https:// URL prefix"));
        assert!(cfg(&format!("{ages}\"https://a/\" = 0")).is_err());
        assert!(proj("[policy]\ntypo = true").values.is_empty());
        assert!(env_layer(|k| (k == "SKILLCASK_ALLOW_RANGES").then(|| "yes".to_string())).is_err());
        assert!(env_layer(|_| Some(" ".into())).unwrap().values.is_empty());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG), "[repositories]\nx = 1\n").unwrap();
        assert!(read_config(&dir.path().join(CONFIG)).unwrap_err().msg.contains("only [policy]"));
        assert_eq!(read_config(&dir.path().join("missing.toml")).unwrap().0, Layer::default());
    }

    #[test]
    fn shown_values_hide_credentials() {
        let v = Value::List(vec!["github".into(), "https://user:s3cret@git.example.com/x?private_token=t0k".into()]);
        let shown = v.to_string();
        assert!(!shown.contains("s3cret") && !shown.contains("t0k") && !shown.contains("user"), "{shown}");
        assert!(shown.contains("\"github\"") && shown.contains("git.example.com/x"), "{shown}");
    }

    #[test]
    fn prefix_boundary() {
        assert!(under("https://git.example.com/a/packages.json", "https://git.example.com"));
        assert!(under("https://git.example.com/a", "https://git.example.com/"));
        assert!(!under("https://git.example.com.evil/a", "https://git.example.com"));
        assert!(!under("https://api.github.com/repos/acmex/r", "https://api.github.com/repos/acme"));
        assert!(under("https://api.github.com/repos/acme/r", "https://api.github.com/repos/acme"));
        assert!(under("https://git.example.com/raw?ref=main", "https://git.example.com/raw"));
        assert!(under("https://GIT.example.com:443/a", "https://git.example.com/"));
        assert!(!under("https://git.example.com:8443/a", "https://git.example.com/"));
        assert!(!under("not a url", "https://git.example.com/"));
    }

    #[test]
    fn dot_segments_do_not_escape_a_prefix() {
        let allowed = "https://git.example.com/api/v4/group/42/";
        for escape in [
            "../../projects/666/packages.json",
            "%2e%2e/%2E%2E/projects/666/",
            ".%2e/x",
            "a/..\\..\\..\\x",
            "..%2F..%2Fx",
            "..%5cx",
            "%2e%2e%2fx",
            "..;/x",
            "a/..;v=1/..;/x",
            "%2e%2e;%2fx",
        ] {
            assert!(!under(&format!("{allowed}{escape}"), allowed), "{escape}");
        }
        assert!(under(&format!("{allowed}a/../b/packages.json"), allowed));
        assert!(under("https://git.example.com/api/v4/projects/group%2Fproj/packages.json", "https://git.example.com/api/v4/"));
    }
}
