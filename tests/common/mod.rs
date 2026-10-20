//! Test harness: a local HTTPS server (test CA) serving a fixture Composer repository, and a
//! wrapper that runs the skillcask binary in an isolated project directory.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SanType};
use serde_json::{Value, json};

#[derive(Default)]
struct State {
    files: HashMap<String, Vec<u8>>,
    dir: Option<PathBuf>,
    /// Required value of the Authorization header, if any.
    auth: Option<String>,
    log: Vec<String>,
    requests: Vec<Request>,
    /// Canned responses by path, with or without the query string.
    responses: HashMap<String, Canned>,
    /// One-shot responses by path, served in order before anything else.
    queued: HashMap<String, std::collections::VecDeque<Canned>>,
}

type Canned = (u16, Vec<(String, String)>, Vec<u8>);

#[derive(Clone, Debug)]
pub struct Request {
    pub path: String,
    /// Header names in lower case.
    pub headers: HashMap<String, String>,
}

pub struct Server {
    pub base: String,
    pub ca_file: PathBuf,
    state: Arc<Mutex<State>>,
    _tmp: tempfile::TempDir,
}

impl Server {
    pub fn start() -> Server {
        let tmp = tempfile::tempdir().unwrap();
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf = CertificateParams::new(Vec::<String>::new()).unwrap();
        leaf.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
        let leaf_cert = leaf.signed_by(&leaf_key, &issuer).unwrap();
        let ca_file = tmp.path().join("ca.pem");
        std::fs::write(&ca_file, ca_cert.pem()).unwrap();

        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![leaf_cert.der().clone()], rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()))
            .unwrap();
        let config = Arc::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("https://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = Arc::new(Mutex::new(State::default()));
        let st = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let config = config.clone();
                let st = st.clone();
                std::thread::spawn(move || {
                    let Ok(conn) = rustls::ServerConnection::new(config) else { return };
                    let mut tls = rustls::StreamOwned::new(conn, stream);
                    let _ = handle(&mut tls, &st);
                });
            }
        });
        Server { base, ca_file, state, _tmp: tmp }
    }

    pub fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.state.lock().unwrap().files.insert(path.to_string(), body.into());
    }

    /// Serves files from `dir` for paths not registered with `put`.
    pub fn serve_dir(&self, dir: &Path) {
        self.state.lock().unwrap().dir = Some(dir.to_path_buf());
    }

    pub fn require_auth(&self, header: Option<&str>) {
        self.state.lock().unwrap().auth = header.map(String::from);
    }

    pub fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Answers `path` (matched with its query string first, then without) with a fixed response.
    pub fn respond(&self, path: &str, status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.state.lock().unwrap().responses.insert(path.to_string(), (status, headers, body.into()));
    }

    /// Answers the next request for `path` (without query) once with this response.
    pub fn respond_once(&self, path: &str, status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.state.lock().unwrap().queued.entry(path.to_string()).or_default().push_back((status, headers, body.into()));
    }

    pub fn redirect(&self, path: &str, location: &str) {
        self.respond(path, 302, &[("Location", location)], "");
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

fn handle(s: &mut (impl Read + Write), st: &Mutex<State>) -> std::io::Result<()> {
    let mut reader = BufReader::new(&mut *s);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let auth = headers.get("authorization").cloned();
    let (status, extra, body) = {
        let mut st = st.lock().unwrap();
        st.log.push(format!("{path} auth={}", auth.is_some()));
        st.requests.push(Request { path: path.clone(), headers });
        let path_only = path.split('?').next().unwrap_or("");
        if st.auth.is_some() && st.auth != auth {
            (401, Vec::new(), Vec::new())
        } else if let Some(r) = st.queued.get_mut(path_only).and_then(|q| q.pop_front()) {
            r
        } else if let Some(r) = st.responses.get(&path).or_else(|| st.responses.get(path_only)) {
            r.clone()
        } else if let Some(b) = st.files.get(path_only) {
            (200, Vec::new(), b.clone())
        } else if let Some(b) = st.dir.as_ref().and_then(|d| std::fs::read(d.join(path_only.trim_start_matches('/'))).ok()) {
            (200, Vec::new(), b)
        } else {
            (404, Vec::new(), Vec::new())
        }
    };
    let extra: String = extra.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let head = format!("HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    s.write_all(head.as_bytes())?;
    s.write_all(&body)?;
    s.flush()
}

/// A zip like a GitHub zipball: everything under one top-level directory.
pub fn zipball(files: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, content) in files {
        let mode = if path.ends_with(".sh") { 0o755 } else { 0o644 };
        w.start_file(
            format!("acme-pkg-1234abc/{path}"),
            zip::write::SimpleFileOptions::default().system(zip::System::Unix).unix_permissions(mode),
        )
        .unwrap();
        w.write_all(content.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

pub fn skill_md(name: &str) -> String {
    format!("---\nname: {name}\ndescription: test skill {name}\n---\n# {name}\n")
}

pub fn ago(hours: i64) -> String {
    (jiff::Timestamp::now() - jiff::SignedDuration::from_hours(hours)).round(jiff::Unit::Second).unwrap().to_string()
}

pub struct Ver<'a> {
    pub version: &'a str,
    pub time: Option<String>,
    pub zip: Vec<u8>,
    pub extra: Option<Value>,
}

impl Server {
    /// Registers a package with the given versions (newest first, minified like Packagist).
    pub fn package(&self, name: &str, versions: Vec<Ver>) {
        let mut list = Vec::new();
        for (i, v) in versions.into_iter().enumerate() {
            let dist = format!("/dist/{name}/{}.zip", v.version);
            let mut m = json!({
                "version": v.version,
                "version_normalized": format!("{}.0", v.version.trim_start_matches('v')),
                "dist": { "type": "zip", "url": self.url(&dist), "reference": format!("ref{i}"), "shasum": "" },
            });
            if i == 0 {
                m["name"] = name.into();
                m["type"] = "ai-agent-skill".into();
            }
            if let Some(t) = v.time {
                m["time"] = t.into();
            }
            if let Some(e) = v.extra {
                m["extra"] = json!({ "ai-agent-skill": e });
            }
            self.put(&dist, v.zip);
            list.push(m);
        }
        self.put(
            &format!("/p2/{name}.json"),
            serde_json::to_vec(&json!({ "minified": "composer/2.0", "packages": { name: list } })).unwrap(),
        );
        self.put("/packages.json", serde_json::to_vec(&json!({ "packages": [], "metadata-url": "/p2/%package%.json" })).unwrap());
    }

    /// One-version package with a single root skill, released 10 days ago.
    pub fn simple(&self, name: &str, version: &str, skill: &str) {
        self.package(
            name,
            vec![Ver {
                version,
                time: Some(ago(240)),
                zip: zipball(&[("SKILL.md", &skill_md(skill)), ("scripts/run.sh", "#!/bin/sh\necho hi\n")]),
                extra: None,
            }],
        );
    }
}

pub struct Project {
    pub dir: tempfile::TempDir,
    home: tempfile::TempDir,
    ca: PathBuf,
    pub env: Vec<(String, String)>,
    /// Working directory in place of the project dir.
    pub cwd: Option<PathBuf>,
}

impl Project {
    pub fn new(server: &Server) -> Project {
        let p = Project {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
            ca: server.ca_file.clone(),
            env: Vec::new(),
            cwd: None,
        };
        git(p.path(), &["init", "-q"]);
        p
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// `skillcask --global <args>` with the config dir at `~/.config/skillcask` of the fake home.
    pub fn global(&self, args: &[&str], code: i32) -> String {
        let mut a = vec!["--global"];
        a.extend_from_slice(args);
        self.expect(&a, code)
    }

    /// Writes the user `config.toml` (D18).
    pub fn config(&self, text: &str) {
        let dir = self.home().join(".config/skillcask");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    pub fn global_manifest(&self, server: &Server, skills: &str) {
        let dir = self.home().join(".config/skillcask");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("skills.toml"),
            format!(
                "[repositories]\nfix = {{ type = \"composer\", url = \"{}\" }}\n\n[skills]\n{skills}\n[policy]\npackagist = false\n",
                server.url("/packages.json")
            ),
        )
        .unwrap();
    }

    pub fn manifest(&self, server: &Server, skills: &str) {
        self.write(
            "skills.toml",
            &format!(
                "[repositories]\nfix = {{ type = \"composer\", url = \"{}\" }}\n\n[skills]\n{skills}\n[policy]\npackagist = false\n",
                server.url("/packages.json")
            ),
        );
    }

    pub fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path().join(rel)).unwrap_or_default()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path().join(rel).symlink_metadata().is_ok()
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs with `input` piped to stdin.
    pub fn run_input(&self, args: &[&str], input: &str) -> Output {
        let mut child = self.command(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_skillcask"));
        c.args(args)
            .current_dir(self.cwd.as_deref().unwrap_or(self.path()))
            .env("SKILLCASK_CA_FILE", &self.ca)
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("COMPOSER_HOME", self.home.path())
            .env_remove("COMPOSER_AUTH")
            .env_remove("CI")
            .env_remove("SKILLCASK_LINK_MODE")
            // The tests run inside coding agents and coloured terminals too; output must not depend on it.
            .env_remove("AI_AGENT")
            .env_remove("CLAUDECODE")
            .env_remove("SKILLCASK_AGENT")
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("SKILLCASK_HOME")
            .env_remove("XDG_CONFIG_HOME")
            // Set by GitHub Actions and by developers; they decide which host gets a token.
            .env_remove("GITHUB_API_URL")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            // HOME holds the only git config, so no system credential helper (Keychain) is asked.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("SKILLCASK_GIT_CREDENTIALS");
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }

    /// Runs and asserts the exit code; returns stdout + stderr.
    pub fn ok(&self, args: &[&str]) -> String {
        self.expect(args, 0)
    }

    pub fn expect(&self, args: &[&str], code: i32) -> String {
        let o = self.run(args);
        let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert_eq!(o.status.code(), Some(code), "skillcask {args:?} output:\n{text}");
        text
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Copies committed project files (manifest and lock) into a fresh project, like a clone.
pub fn clone_of(p: &Project, server: &Server) -> Project {
    let c = Project::new(server);
    for f in ["skills.toml", "skills.lock", ".gitignore"] {
        if p.exists(f) {
            c.write(f, &p.read(f));
        }
    }
    c
}
