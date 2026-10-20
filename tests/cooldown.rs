//! `min-release-age` per repository.
mod common;

use common::*;
use serde_json::json;

#[test]
fn a_repository_can_set_its_own_min_release_age() {
    let s = Server::start();
    let z = || zipball(&[("SKILL.md", &skill_md("one"))]);
    s.package(
        "acme/one",
        vec![
            Ver { version: "1.1.0", time: Some(ago(1)), zip: z(), extra: None },
            Ver { version: "1.0.0", time: Some(ago(240)), zip: z(), extra: None },
        ],
    );
    let dist = "/pub/acme/two/2.0.0.zip";
    s.put(dist, zipball(&[("SKILL.md", &skill_md("two"))]));
    s.put(
        "/pub/packages.json",
        json!({ "packages": { "acme/two": { "2.0.0": {
            "name": "acme/two", "version": "2.0.0", "version_normalized": "2.0.0.0", "type": "ai-agent-skill",
            "time": ago(1), "dist": { "type": "zip", "url": s.url(dist) },
        } } } })
        .to_string(),
    );
    let p = Project::new(&s);
    p.write(
        "skills.toml",
        &format!(
            "[repositories]\ninternal = {{ url = \"{}\", min-release-age = \"0\" }}\npub = {{ url = \"{}\" }}\n[policy]\npackagist = false\n",
            s.url("/packages.json"),
            s.url("/pub/packages.json")
        ),
    );
    p.ok(&["add", "acme/one"]);
    assert!(p.read("skills.lock").contains("version = \"1.1.0\""), "{}", p.read("skills.lock"));
    assert!(p.expect(&["add", "acme/two"], 1).contains("newer than policy.min-release-age"));

    p.write("skills.toml", &p.read("skills.toml").replace("min-release-age = \"0\"", "min-release-age = 3"));
    assert!(p.expect(&["install"], 1).contains("repository internal: min-release-age must be a string"));
}

#[test]
fn a_repository_cannot_go_below_a_fixed_min_release_age() {
    let s = Server::start();
    let z = || zipball(&[("SKILL.md", &skill_md("one"))]);
    s.package(
        "acme/one",
        vec![
            Ver { version: "1.1.0", time: Some(ago(1)), zip: z(), extra: None },
            Ver { version: "1.0.0", time: Some(ago(240)), zip: z(), extra: None },
        ],
    );
    let p = Project::new(&s);
    p.write(
        "skills.toml",
        &format!(
            "[repositories]\ninternal = {{ url = \"{}\", min-release-age = \"0\" }}\n[policy]\npackagist = false\n",
            s.url("/packages.json")
        ),
    );
    p.config("[policy]\nmin-release-age = \"3d\"\nenforce = [\"min-release-age\"]\n");
    p.ok(&["add", "acme/one"]);
    assert!(p.read("skills.lock").contains("version = \"1.0.0\""), "{}", p.read("skills.lock"));
}
