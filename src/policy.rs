//! Policy layers (D18): environment, enforced keys of the user `config.toml`, the manifest's
//! `[policy]`, the rest of `config.toml`, built-in defaults.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use jiff::SignedDuration;
use toml_edit::{DocumentMut, Item, TableLike};

use crate::bail;
use crate::error::{Context, Error, MANIFEST, Result};

pub const CONFIG: &str = "config.toml";
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
    if matches!(s, "packagist" | "github") || s.starts_with("https://") && s.len() > "https://".len() || names && name {
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
    let shown = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Layer::default(), String::new())),
        Err(e) => return Err(e).ctx(&shown),
    };
    let doc: DocumentMut = text.parse().map_err(|e| Error::new(MANIFEST, format!("{shown}: {e}")))?;
    if let Some((k, _)) = doc.iter().find(|(k, _)| *k != "policy") {
        bail!(MANIFEST, "{shown}: only [policy] is read here, found {k}");
    }
    Ok((layer(doc.get("policy").and_then(Item::as_table_like), &shown, true)?, shown))
}

/// Set once the environment-over-`enforce` warnings are printed: a run loads the manifest more than once.
static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            Some((v, "environment"))
        } else if let Some(v) = enforced {
            Some((v, "config.toml, enforced"))
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

/// `url` is `prefix` or below it on a `/` (or query) boundary.
pub fn under(url: &str, prefix: &str) -> bool {
    url == prefix || url.starts_with(prefix) && (prefix.ends_with('/') || url[prefix.len()..].starts_with(['/', '?']))
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
        assert!(cfg("[policy]\nmin-release-age = 3").is_err());
        assert!(cfg("[policy]\ntypo = true").unwrap_err().msg.contains("unknown key"));
        assert!(cfg("[policy]\nenforce = [\"packagist\"]").unwrap_err().msg.contains("does not set"));
        assert!(cfg("[policy]\nenforce = [\"nope\"]").is_err());
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
    }
}
