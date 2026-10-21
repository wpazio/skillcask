//! Composer `abandoned` in registry metadata, and `deprecated:` in publish.
mod common;

use common::*;
use serde_json::json;

fn abandoned(s: &Server, name: &str, value: &serde_json::Value) {
    s.simple(name, "1.0.0", "old");
    let v = json!({ "name": name, "version": "1.0.0", "version_normalized": "1.0.0.0", "type": "ai-agent-skill", "time": ago(240),
        "dist": { "type": "zip", "url": s.url(&format!("/dist/{name}/1.0.0.zip")) }, "abandoned": value });
    s.put(&format!("/p2/{name}.json"), json!({ "packages": { name: [v] } }).to_string());
}

#[test]
fn add_update_and_outdated_warn_about_an_abandoned_package() {
    let s = Server::start();
    abandoned(&s, "acme/old", &json!("acme/new\u{1b}]0;x"));
    let p = Project::new(&s);
    p.manifest(&s, "");
    for args in [&["add", "acme/old"][..], &["update"], &["outdated"]] {
        let out = p.ok(args);
        // Not a package name: the replacement could carry terminal escapes or fake lines.
        assert_eq!(out.matches("warning: acme/old is abandoned\n").count(), 1, "{args:?}: {out}");
        assert!(!out.contains("]0;x"), "{out}");
    }
    assert!(p.exists(".agents/skills/old/SKILL.md"));
    // Installing from the lock does not ask the registry.
    assert!(!clone_of(&p, &s).ok(&["install"]).contains("abandoned"));
}

#[test]
fn abandoned_without_replacement() {
    let s = Server::start();
    abandoned(&s, "acme/old", &json!(true));
    let p = Project::new(&s);
    p.manifest(&s, "");
    let out = p.ok(&["add", "acme/old"]);
    assert!(out.contains("warning: acme/old is abandoned\n"), "{out}");
}

#[test]
fn published_deprecation_reaches_consumers() {
    let s = Server::start();
    let p = Project::new(&s);
    p.write("src/old/SKILL.md", "---\nname: old\nversion: 1.0.0\ndescription: d\ndeprecated: acme/new\n---\n");
    let out = p.path().join("out");
    p.ok(&["publish", "--vendor", "acme", "--base-url", &s.base, "--out", out.to_str().unwrap(), "src"]);
    s.serve_dir(&out);
    let mut c = Project::new(&s);
    c.env.push(("SKILLCASK_MIN_RELEASE_AGE".into(), "0".into()));
    c.manifest(&s, "");
    let text = c.ok(&["add", "acme/old"]);
    assert!(text.contains("acme/old is abandoned; use acme/new instead"), "{text}");
}
