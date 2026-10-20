//! Layered policy and `allow-sources` (D18).
mod common;

use common::*;
use serde_json::json;

/// `acme/public` lives only in the second repository, `/pub/packages.json`.
fn public(s: &Server) {
    public_at(s, "/pub");
}

/// `acme/public` listed by a repository at `at`; the dist stays under `/pub/`.
fn public_at(s: &Server, at: &str) {
    let dist = "/pub/acme/public/1.0.0.zip";
    s.put(dist, zipball(&[("SKILL.md", &skill_md("public"))]));
    s.put(
        &format!("{at}/packages.json"),
        json!({ "packages": { "acme/public": { "1.0.0": {
            "name": "acme/public", "version": "1.0.0", "version_normalized": "1.0.0.0", "type": "ai-agent-skill",
            "time": ago(240), "dist": { "type": "zip", "url": s.url(dist) },
        } } } })
        .to_string(),
    );
}

fn two_repos(s: &Server, policy: &str) -> Project {
    s.simple("acme/private", "1.0.0", "private");
    public(s);
    let p = Project::new(s);
    p.write(
        "skills.toml",
        &format!(
            "[repositories]\nfix = {{ url = \"{}\" }}\npub = {{ url = \"{}\" }}\n[policy]\npackagist = false\n{policy}\n",
            s.url("/packages.json"),
            s.url("/pub/packages.json")
        ),
    );
    p
}

#[test]
fn blocked_repositories_get_no_request() {
    let s = Server::start();
    let p = two_repos(&s, "allow-sources = [\"fix\"]");
    p.ok(&["add", "acme/private"]);
    let e = p.expect(&["add", "acme/public"], 1);
    assert!(e.contains("acme/public not found in repositories fix"), "{e}");
    assert!(!s.log().iter().any(|l| l.contains("/pub/")), "{:?}", s.log());
    let e = p.expect(&["add", "acme/public", "--repository", "pub"], 1);
    assert!(e.contains("is not in policy.allow-sources (skills.toml)"), "{e}");
}

#[test]
fn a_lock_from_a_blocked_source_does_not_install() {
    let s = Server::start();
    let p = two_repos(&s, "");
    p.ok(&["add", "acme/public"]);
    p.write("skills.toml", &p.read("skills.toml").replace("packagist = false", "packagist = false\nallow-sources = [\"fix\"]"));
    let mut c = clone_of(&p, &s);
    c.env.push(("CI".into(), "true".into()));
    let e = c.expect(&["install"], 1);
    assert!(e.contains("acme/public: source") && e.contains("not in policy.allow-sources"), "{e}");
    assert!(!c.exists(".agents/skills/public"));
}

#[test]
fn enforced_config_beats_the_project_and_the_environment_beats_both() {
    let s = Server::start();
    let p = two_repos(&s, "allow-sources = [\"pub\"]");
    p.config(&format!("[policy]\nallow-sources = [\"{}\"]\nenforce = [\"allow-sources\"]\n", s.url("/packages.json")));
    let e = p.expect(&["add", "acme/public"], 1);
    assert!(e.contains("policy.allow-sources is enforced") && e.contains("config.toml"), "{e}");

    p.write("skills.toml", &p.read("skills.toml").replace("allow-sources = [\"pub\"]", ""));
    p.ok(&["add", "acme/private"]);
    assert!(p.expect(&["add", "acme/public"], 1).contains("not found in repositories fix"));
    let st = p.ok(&["status"]);
    assert!(st.contains("policy.allow-sources = [") && st.contains("(config.toml, enforced)"), "{st}");

    let mut env = two_repos(&s, "allow-sources = [\"pub\"]");
    env.config(&format!("[policy]\nallow-sources = [\"{}\"]\nenforce = [\"allow-sources\"]\n", s.url("/packages.json")));
    env.env.push(("SKILLCASK_ALLOW_SOURCES".into(), s.url("/pub/")));
    let out = env.ok(&["add", "acme/public"]);
    assert_eq!(out.matches("warning: SKILLCASK_ALLOW_SOURCES overrides policy.allow-sources").count(), 1, "{out}");
    assert!(out.contains("enforced by") && out.contains("config.toml"), "{out}");
    assert!(env.ok(&["status"]).contains("(environment)"));

    // The same value as the enforced one changes nothing, so it is not worth a warning.
    let mut same = two_repos(&s, "");
    same.config(&format!("[policy]\nallow-sources = [\"{}\"]\nenforce = [\"allow-sources\"]\n", s.url("/packages.json")));
    same.env.push(("SKILLCASK_ALLOW_SOURCES".into(), s.url("/packages.json")));
    assert!(!same.ok(&["add", "acme/private"]).contains("warning: SKILLCASK_ALLOW_SOURCES"));
}

#[test]
fn unenforced_config_is_a_default_the_project_overrides() {
    let s = Server::start();
    let p = two_repos(&s, "");
    p.config(&format!("[policy]\nallow-sources = [\"{}\"]\n", s.url("/packages.json")));
    assert!(p.expect(&["add", "acme/public"], 1).contains("not found in repositories fix"));
    p.write("skills.toml", &p.read("skills.toml").replace("packagist = false", "packagist = false\nallow-sources = [\"fix\", \"pub\"]"));
    p.ok(&["add", "acme/public"]);
}

#[test]
fn a_dependency_from_a_blocked_source_fails() {
    let s = Server::start();
    let md = "---\nname: mr\ndescription: d\nrequires:\n  acme/public: ^1.0\n---\n";
    s.package("acme/mr", vec![Ver { version: "1.0.0", time: Some(ago(240)), zip: zipball(&[("SKILL.md", md)]), extra: None }]);
    public(&s);
    let p = Project::new(&s);
    p.write(
        "skills.toml",
        &format!(
            "[repositories]\nfix = {{ url = \"{}\" }}\npub = {{ url = \"{}\" }}\n[policy]\npackagist = false\nallow-sources = [\"fix\"]\n",
            s.url("/packages.json"),
            s.url("/pub/packages.json")
        ),
    );
    let e = p.expect(&["add", "acme/mr"], 1);
    assert!(e.contains("acme/public") && e.contains("required by: acme/mr"), "{e}");
    assert!(!p.exists("skills.lock"));
}

#[test]
fn dot_segments_do_not_escape_an_enforced_prefix() {
    let s = Server::start();
    public(&s);
    let p = Project::new(&s);
    p.config(&format!("[policy]\nallow-sources = [\"{}\"]\nenforce = [\"allow-sources\"]\n", s.url("/fix/")));
    p.write(
        "skills.toml",
        &format!("[repositories]\nx = {{ url = \"{}\" }}\n[policy]\npackagist = false\n", s.url("/fix/../pub/packages.json")),
    );
    let e = p.expect(&["add", "acme/public", "--repository", "x"], 1);
    assert!(e.contains("not in policy.allow-sources"), "{e}");
    assert!(s.log().is_empty(), "{:?}", s.log());

    // A committed lock that records such a url is refused the same way.
    public_at(&s, "/fix");
    p.write("skills.toml", &p.read("skills.toml").replace("/fix/../pub/", "/fix/"));
    p.ok(&["add", "acme/public", "--repository", "x"]);
    for f in ["skills.toml", "skills.lock"] {
        p.write(f, &p.read(f).replace(&s.url("/fix/packages.json"), &s.url("/fix/../pub/packages.json")));
    }
    let e = p.expect(&["install", "--frozen"], 1);
    assert!(e.contains("not in policy.allow-sources"), "{e}");
    assert!(!s.log().iter().any(|l| l.starts_with("/pub/packages.json")), "{:?}", s.log());
}

#[test]
fn github_can_be_turned_off_or_limited_to_an_organisation() {
    let s = Server::start();
    let mut p = Project::new(&s);
    p.env.push(("SKILLCASK_GITHUB_API".into(), s.base.clone()));
    p.write("skills.toml", "[repositories]\ngithub = false\n");
    assert!(p.expect(&["add", "github:acme/tagged"], 1).contains("GitHub is turned off"));
    p.write("skills.toml", &format!("[policy]\nallow-sources = [\"{}/repos/other/\"]\n", s.base));
    assert!(p.expect(&["add", "github:acme/tagged"], 1).contains("not in policy.allow-sources"));
    assert!(s.log().is_empty(), "no request before the check: {:?}", s.log());
}

#[test]
fn init_lists_the_default_sources() {
    let s = Server::start();
    let p = Project::new(&s);
    p.ok(&["init"]);
    let t = p.read("skills.toml");
    assert!(t.contains("packagist = { url = \"https://repo.packagist.org\" }") && t.contains("github = { type = \"github\""), "{t}");
}
