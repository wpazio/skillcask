//! HTTPS-only client with Composer `auth.json` credentials.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;
use ureq::tls::{PemItem, RootCerts, TlsConfig};
use url::Url;

use crate::bail;
use crate::error::{AUTH, Error, IO, NETWORK, Result};

const MAX_REDIRECTS: usize = 5;
const JSON_LIMIT: u64 = 50 * 1024 * 1024;
const ATTEMPTS: u32 = 3;
/// A longer `Retry-After` (a rate limit window) fails at once instead of stalling the command.
const MAX_RETRY_AFTER: u64 = 60;

#[derive(Clone, Debug, PartialEq)]
enum Cred {
    Basic(String, String),
    Bearer(String),
    PrivateToken(String),
}

impl Cred {
    fn header(&self) -> (&'static str, String) {
        match self {
            Cred::Basic(u, p) => {
                ("Authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))))
            }
            Cred::Bearer(t) => ("Authorization", format!("Bearer {t}")),
            Cred::PrivateToken(t) => ("PRIVATE-TOKEN", t.clone()),
        }
    }
}

/// Entries in source priority order; the first entry for a host wins. With `git`, a host none of
/// them covers is asked from git's credential helper, once.
#[derive(Debug, Default)]
struct Auth(Vec<(String, Cred)>, bool, std::cell::RefCell<Vec<(String, Option<Cred>)>>);

impl Auth {
    fn load(config: &Path) -> Result<Auth> {
        let home = crate::scope::home();
        // Composer's Platform::isXdg(): any XDG_* variable, or a system XDG directory.
        let xdg = std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("XDG_")) || Path::new("/etc/xdg").is_dir();
        let global = global_auth_paths(|k| std::env::var_os(k), home.as_deref(), xdg).into_iter().find(|p| p.exists());
        let mut auth = Auth::default();
        if let Ok(env) = std::env::var("COMPOSER_AUTH") {
            auth.add(&env, "COMPOSER_AUTH")?;
        }
        for path in std::iter::once(config.join("auth.json")).chain(global) {
            match std::fs::read_to_string(&path) {
                Ok(text) => auth.add(&text, &path.display().to_string())?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let token = ["GITHUB_TOKEN", "GH_TOKEN"].iter().find_map(|k| std::env::var(k).ok().filter(|t| !t.is_empty()));
        if let Some(t) = token {
            for host in crate::github::trusted_apis().iter().filter_map(|a| Url::parse(a).ok().as_ref().and_then(auth_key)) {
                auth.0.push((host, Cred::Bearer(t.clone())));
            }
        }
        auth.1 = crate::credential::enabled();
        Ok(auth)
    }

    fn add(&mut self, json: &str, source: &str) -> Result<()> {
        let bad = || Error::new(AUTH, format!("{source}: invalid auth JSON"));
        let v: Value = serde_json::from_str(json).map_err(|_| bad())?;
        let section = |k: &str| v.get(k).and_then(Value::as_object).into_iter().flatten();
        let str_or = |v: &Value, k: &str| v.as_str().or_else(|| v.get(k).and_then(Value::as_str)).map(String::from);
        for (host, c) in section("http-basic") {
            let user = c.get("username").and_then(Value::as_str).ok_or_else(bad)?;
            let pass = c.get("password").and_then(Value::as_str).unwrap_or("");
            self.0.push((host.clone(), Cred::Basic(user.into(), pass.into())));
        }
        for (host, c) in section("bearer") {
            self.0.push((host.clone(), Cred::Bearer(c.as_str().ok_or_else(bad)?.into())));
        }
        for (host, c) in section("gitlab-token") {
            let token = str_or(c, "token").ok_or_else(bad)?;
            // As in Composer: with a username the token is a password for HTTP Basic.
            let cred = match c.get("username").and_then(Value::as_str) {
                Some(u) => Cred::Basic(u.into(), token),
                None => Cred::PrivateToken(token),
            };
            self.0.push((host.clone(), cred));
        }
        for (host, c) in section("gitlab-oauth") {
            self.0.push((host.clone(), Cred::Bearer(str_or(c, "token").ok_or_else(bad)?)));
        }
        for (host, c) in section("github-oauth") {
            let token = Cred::Bearer(c.as_str().ok_or_else(bad)?.into());
            // Composer keys the token by github.com; the API that skillcask calls lives on api.github.com.
            if host.eq_ignore_ascii_case("github.com") {
                self.0.push(("api.github.com".into(), token.clone()));
            }
            self.0.push((host.clone(), token));
        }
        Ok(())
    }

    /// `host:port` must match exactly; a bare host means port 443.
    fn lookup(&self, url: &Url) -> Option<Cred> {
        let key = auth_key(url)?;
        let keys = if url.port().is_some() { vec![key.clone()] } else { vec![format!("{key}:443"), key.clone()] };
        if let Some((_, c)) = self.0.iter().find(|(k, _)| keys.iter().any(|x| k.eq_ignore_ascii_case(x))) {
            return Some(c.clone());
        }
        if !self.1 {
            return None;
        }
        let mut seen = self.2.borrow_mut();
        if let Some((_, c)) = seen.iter().find(|(k, _)| *k == key) {
            return c.clone();
        }
        let c = crate::credential::fill(&key).map(|(user, pass)| match user.as_str() {
            crate::credential::PRIVATE_TOKEN => Cred::PrivateToken(pass),
            crate::credential::OAUTH2 => Cred::Bearer(pass),
            _ => Cred::Basic(user, pass),
        });
        seen.push((key, c.clone()));
        c
    }
}

/// `host`, or `host:port` for a port other than the scheme's default.
fn auth_key(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(url.port().map_or_else(|| host.to_string(), |p| format!("{host}:{p}")))
}

/// Candidates for the global `auth.json`, in Composer's home order: `COMPOSER_HOME`, else
/// `%APPDATA%\Composer` on Windows, else the XDG config dir (when the system uses XDG) before
/// `~/.composer`. `~/.config/composer` stays a fallback either way.
fn global_auth_paths(var: impl Fn(&str) -> Option<std::ffi::OsString>, home: Option<&Path>, xdg: bool) -> Vec<PathBuf> {
    let set = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(c) = set("COMPOSER_HOME") {
        return vec![c.join("auth.json")];
    }
    let mut dirs = Vec::new();
    if cfg!(windows) {
        dirs.extend(set("APPDATA").map(|a| a.join("Composer")));
    } else if xdg {
        dirs.extend(set("XDG_CONFIG_HOME").map(|x| x.join("composer")).or_else(|| home.map(|h| h.join(".config/composer"))));
    }
    if let Some(h) = home {
        for d in [h.join(".composer"), h.join(".config/composer")] {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    dirs.into_iter().map(|d| d.join("auth.json")).collect()
}

/// `redact` for a string that is not a valid URL: everything up to the last `@` may be userinfo.
fn redact_raw(s: &str) -> String {
    let start = s.find("://").map_or(0, |i| i + 3);
    let mut out = match s.rfind('@').filter(|&at| at >= start) {
        Some(at) => format!("{}…@{}", &s[..start], &s[at + 1..]),
        None => s.to_string(),
    };
    if let Some(q) = out.find(['?', '#']) {
        out.truncate(q);
        out.push_str("?…");
    }
    out
}

/// URL safe to print: no userinfo, no query (tokens sometimes travel there).
pub fn redact(url: &Url) -> String {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    if u.query().is_some() {
        u.set_query(Some("…"));
    }
    u.to_string()
}

pub struct Http {
    agent: ureq::Agent,
    auth: Auth,
    verbose: bool,
    /// First retry delay without `Retry-After`; doubles per retry.
    backoff: Duration,
}

/// Failures worth another attempt: the connection broke, not the request. ureq 3 reports TLS
/// failures (rustls: `InvalidData`) and OS timeouts as I/O errors too; those would fail again (D20).
fn transient(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(io) => !matches!(io.kind(), std::io::ErrorKind::InvalidData | std::io::ErrorKind::TimedOut),
        _ => false,
    }
}

/// GitHub's quota is spent until its reset, typically far beyond any retry.
fn rate_limited<B>(r: &ureq::http::Response<B>) -> bool {
    r.headers().get("x-ratelimit-remaining").is_some_and(|v| v.as_bytes() == b"0")
}

/// Wait before retry number `retry` (1-based), or `None` when `Retry-After` asks for longer than
/// we are willing to wait. Only the delay-seconds form is read; a date falls back to backoff.
fn retry_delay(retry: u32, retry_after: Option<&str>, backoff: Duration) -> Option<Duration> {
    match retry_after.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(s) if s > MAX_RETRY_AFTER => None,
        Some(s) => Some(Duration::from_secs(s)),
        None => Some(backoff * 2u32.pow(retry - 1)),
    }
}

impl Http {
    pub fn new(config: &Path, verbose: bool) -> Result<Http> {
        let roots = match std::env::var_os("SKILLCASK_CA_FILE") {
            None => RootCerts::PlatformVerifier,
            Some(path) => {
                let pem =
                    std::fs::read(&path).map_err(|e| Error::new(IO, format!("SKILLCASK_CA_FILE {}: {e}", Path::new(&path).display())))?;
                let certs: Vec<_> = ureq::tls::parse_pem(&pem)
                    .filter_map(|i| match i {
                        Ok(PemItem::Certificate(c)) => Some(c),
                        _ => None,
                    })
                    .collect();
                if certs.is_empty() {
                    bail!(IO, "SKILLCASK_CA_FILE {}: no certificates found", Path::new(&path).display());
                }
                RootCerts::from(certs)
            }
        };
        let agent = ureq::Agent::config_builder()
            .tls_config(TlsConfig::builder().root_certs(roots).build())
            .https_only(true)
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent(concat!("skillcask/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(Http { agent, auth: Auth::load(config)?, verbose, backoff: Duration::from_secs(1) })
    }

    /// `Ok(None)` on 404/410. Connection failures, 429 and 5xx are retried: every request is a GET.
    pub fn get(&self, url: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        let mut cur = Url::parse(url).map_err(|e| Error::new(NETWORK, format!("invalid URL {:?}: {e}", redact_raw(url))))?;
        for _ in 0..=MAX_REDIRECTS {
            if cur.scheme() != "https" {
                bail!(NETWORK, "refusing non-HTTPS URL {}", redact(&cur));
            }
            let shown = redact(&cur);
            let inline =
                (!cur.username().is_empty()).then(|| Cred::Basic(cur.username().to_string(), cur.password().unwrap_or("").to_string()));
            let _ = cur.set_username("");
            let _ = cur.set_password(None);
            let cred = inline.or_else(|| self.auth.lookup(&cur));
            let mut retry = 0;
            let mut resp = loop {
                if self.verbose {
                    eprintln!("GET {shown}");
                }
                let mut req = self.agent.get(cur.as_str());
                if let Some(c) = &cred {
                    let (k, v) = c.header();
                    req = req.header(k, v);
                }
                let (why, after) = match req.call() {
                    Ok(r) if (r.status() == 429 && !rate_limited(&r)) || r.status().is_server_error() => {
                        let after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(String::from);
                        (format!("HTTP {}", r.status().as_u16()), after)
                    }
                    Ok(r) => break r,
                    Err(e) if transient(&e) => (e.to_string(), None),
                    Err(e) => bail!(NETWORK, "{shown}: {e}"),
                };
                retry += 1;
                let wait = (retry < ATTEMPTS).then(|| retry_delay(retry, after.as_deref(), self.backoff)).flatten();
                let Some(wait) = wait else {
                    let after = after.map(|a| format!(" (Retry-After: {})", a.trim())).unwrap_or_default();
                    bail!(NETWORK, "{shown}: {why}{after}")
                };
                crate::ui::warn(format!("{shown}: {why}; retrying in {}s", wait.as_secs()));
                std::thread::sleep(wait);
            };
            let status = resp.status().as_u16();
            match status {
                200..=299 => {
                    return match resp.body_mut().with_config().limit(max_bytes).read_to_vec() {
                        Ok(b) => Ok(Some(b)),
                        Err(ureq::Error::BodyExceedsLimit(_)) => {
                            Err(Error::new(IO, format!("{shown}: response exceeds the limit of {max_bytes} bytes")))
                        }
                        Err(e) => Err(Error::new(NETWORK, format!("{shown}: {e}"))),
                    };
                }
                301 | 302 | 303 | 307 | 308 => {
                    let loc = resp
                        .headers()
                        .get("location")
                        .and_then(|v| v.to_str().ok())
                        .ok_or_else(|| Error::new(NETWORK, format!("{shown}: redirect without Location")))?;
                    cur = cur.join(loc).map_err(|e| Error::new(NETWORK, format!("{shown}: bad redirect: {e}")))?;
                }
                404 | 410 => return Ok(None),
                403 | 429 if rate_limited(&resp) => {
                    bail!(AUTH, "{shown}: HTTP {status}: GitHub API rate limit reached; set {}", crate::github::token_hint())
                }
                401 | 403 => {
                    let github = if crate::github::on_trusted_host(&cur) {
                        " (GitHub: GITHUB_TOKEN, also for the anonymous rate limit)"
                    } else {
                        ""
                    };
                    bail!(AUTH, "{shown}: HTTP {status}; check auth.json / COMPOSER_AUTH for {}{github}", cur.host_str().unwrap_or(""))
                }
                _ => bail!(NETWORK, "{shown}: HTTP {status}"),
            }
        }
        bail!(NETWORK, "{}: more than {MAX_REDIRECTS} redirects", redact(&cur))
    }

    pub fn has_credential(&self, url: &str) -> bool {
        Url::parse(url).is_ok_and(|u| !u.username().is_empty() || self.auth.lookup(&u).is_some())
    }

    pub fn get_json(&self, url: &str) -> Result<Option<Value>> {
        match self.get(url, JSON_LIMIT)? {
            None => Ok(None),
            Some(b) => serde_json::from_slice(&b).map(Some).map_err(|e| {
                Error::new(NETWORK, format!("{}: invalid JSON: {e}", Url::parse(url).map(|u| redact(&u)).unwrap_or_default()))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_broken_connections_are_transient() {
        use std::io::{Error as IoError, ErrorKind};
        let io = |e: IoError| transient(&ureq::Error::Io(e));
        assert!(io(IoError::from(ErrorKind::ConnectionReset)));
        assert!(io(IoError::new(ErrorKind::UnexpectedEof, "Peer disconnected")));
        assert!(transient(&ureq::Error::ConnectionFailed));
        assert!(!io(IoError::new(ErrorKind::InvalidData, rustls::Error::InvalidCertificate(rustls::CertificateError::Expired))));
        assert!(!io(IoError::from(ErrorKind::TimedOut)));
        assert!(!transient(&ureq::Error::HostNotFound));
    }

    #[test]
    fn auth_sources_and_host_matching() {
        let mut a = Auth::default();
        a.add(
            r#"{"http-basic":{"git.example.com":{"username":"gitlab-ci-token","password":"job"}},"bearer":{"repo.example.com:8443":"b1"}}"#,
            "env",
        )
        .unwrap();
        a.add(r#"{"http-basic":{"git.example.com":{"username":"other","password":"x"}},"gitlab-token":{"gl.example.com":{"username":"u","token":"pt"}},"gitlab-oauth":{"oa.example.com":"ot"},"bearer":{"repo.example.com":"b2"},"github-oauth":{"github.com":"gh"}}"#, "file").unwrap();
        let get = |u: &str| a.lookup(&Url::parse(u).unwrap());
        assert_eq!(get("https://git.example.com/x"), Some(Cred::Basic("gitlab-ci-token".into(), "job".into())));
        assert_eq!(get("https://repo.example.com:8443/x"), Some(Cred::Bearer("b1".into())));
        assert_eq!(get("https://repo.example.com/x"), Some(Cred::Bearer("b2".into())));
        assert_eq!(get("https://gl.example.com/x"), Some(Cred::Basic("u".into(), "pt".into())));
        assert_eq!(get("https://oa.example.com/x"), Some(Cred::Bearer("ot".into())));
        assert_eq!(get("https://api.github.com/repos/a/b"), Some(Cred::Bearer("gh".into())));
        assert_eq!(get("https://none.example.com/x"), None);
        assert_eq!(Cred::Basic("a".into(), "b".into()).header().1, "Basic YTpi");
        assert!(a.add("not json", "x").is_err());
        assert!(a.add(r#"{"bearer":{"h":5}}"#, "x").is_err());
    }

    #[test]
    fn credentials_match_host_and_port() {
        let mut a = Auth::default();
        a.add(r#"{"bearer":{"bare.example.com":"b","p.example.com:443":"p443","h.example.com:8443":"h8443"},"gitlab-token":{"gl.example.com":"pt"}}"#, "x").unwrap();
        let get = |u: &str| a.lookup(&Url::parse(u).unwrap());
        assert_eq!(get("https://bare.example.com/x"), Some(Cred::Bearer("b".into())));
        assert_eq!(get("https://bare.example.com:443/x"), Some(Cred::Bearer("b".into())));
        assert_eq!(get("https://bare.example.com:8443/x"), None, "a bare host is port 443 only");
        assert_eq!(get("https://p.example.com/x"), Some(Cred::Bearer("p443".into())));
        assert_eq!(get("https://h.example.com:8443/x"), Some(Cred::Bearer("h8443".into())));
        assert_eq!(get("https://h.example.com/x"), None);
        assert_eq!(get("https://127.0.0.1:9/x"), None);
        assert_eq!(get("https://gl.example.com/x"), Some(Cred::PrivateToken("pt".into())));
    }

    #[test]
    fn invalid_url_error_hides_userinfo_and_query() {
        let http = Http::new(Path::new("/nonexistent"), false).unwrap();
        let e = http.get("https://u:secret@bad host/x?token=abc", 10).unwrap_err();
        assert_eq!(e.code, NETWORK);
        assert!(e.msg.contains("invalid URL") && !e.msg.contains("secret") && !e.msg.contains("u:") && !e.msg.contains("abc"), "{}", e.msg);
        assert!(e.msg.contains("bad host/x"), "{}", e.msg);
    }

    #[cfg(not(windows))]
    #[test]
    fn global_auth_follows_composer_home_order() {
        let env = |vars: &'static [(&'static str, &'static str)]| move |k: &str| vars.iter().find(|v| v.0 == k).map(|v| v.1.into());
        let home = Path::new("/h");
        let paths = |vars, xdg| global_auth_paths(env(vars), Some(home), xdg);
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(paths(&[("COMPOSER_HOME", "/c"), ("XDG_CONFIG_HOME", "/x")], true), [p("/c/auth.json")]);
        assert_eq!(
            paths(&[("XDG_CONFIG_HOME", "/x")], true),
            [p("/x/composer/auth.json"), p("/h/.composer/auth.json"), p("/h/.config/composer/auth.json")]
        );
        assert_eq!(paths(&[], true), [p("/h/.config/composer/auth.json"), p("/h/.composer/auth.json")]);
        assert_eq!(paths(&[("COMPOSER_HOME", "")], false), [p("/h/.composer/auth.json"), p("/h/.config/composer/auth.json")]);
    }

    #[test]
    fn redacts_credentials() {
        let u = Url::parse("https://user:secret@h.example.com/p.json?private_token=abc").unwrap();
        let r = redact(&u);
        assert!(!r.contains("secret") && !r.contains("user") && !r.contains("abc"), "{r}");
        assert!(r.starts_with("https://h.example.com/p.json"));
    }

    #[test]
    fn retry_delay_backs_off_and_honours_retry_after() {
        let unit = Duration::from_millis(100);
        assert_eq!(retry_delay(1, None, unit), Some(unit));
        assert_eq!(retry_delay(2, None, unit), Some(unit * 2));
        assert_eq!(retry_delay(1, Some(" 7 "), unit), Some(Duration::from_secs(7)));
        assert_eq!(retry_delay(1, Some("0"), unit), Some(Duration::ZERO));
        assert_eq!(retry_delay(1, Some("60"), unit), Some(Duration::from_secs(60)));
        assert_eq!(retry_delay(1, Some("3600"), unit), None);
        assert_eq!(retry_delay(2, Some("Wed, 21 Oct 2026 07:28:00 GMT"), unit), Some(unit * 2));
    }

    #[test]
    fn retries_a_dropped_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count = accepted.clone();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(s);
            }
        });
        let mut http = Http::new(Path::new("/nonexistent"), false).unwrap();
        http.backoff = Duration::from_millis(1);
        let e = http.get(&format!("https://127.0.0.1:{port}/packages.json"), 10).unwrap_err();
        assert_eq!(e.code, NETWORK);
        let accepted = accepted.load(std::sync::atomic::Ordering::SeqCst);
        // Windows CI accepted 7 connections for 3 calls; the extra ones come from below this retry
        // loop, so only the lower bound holds there.
        if cfg!(windows) {
            assert!(accepted >= ATTEMPTS, "{accepted}");
        } else {
            assert_eq!(accepted, ATTEMPTS);
        }
    }

    #[test]
    fn refuses_plain_http_before_any_request() {
        let http = Http::new(Path::new("/nonexistent"), false).unwrap();
        let e = http.get("http://example.com/packages.json", 10).unwrap_err();
        assert_eq!(e.code, NETWORK);
        assert!(e.msg.contains("refusing non-HTTPS"));
    }
}
