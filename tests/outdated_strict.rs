//! `outdated --strict`: exit 1 when `update` would lock a newer version of a manifest entry.
mod common;

use common::*;

fn md(name: &str, requires: &str) -> String {
    format!("---\nname: {name}\ndescription: d\n{requires}---\n")
}

fn one(s: &Server, newest_hours: i64) {
    let z = |v: &str| zipball(&[("SKILL.md", &md("one", "requires:\n  acme/dep: \"^1.0\"\n")), ("v.txt", v)]);
    s.package(
        "acme/one",
        vec![
            Ver { version: "2.0.0", time: Some(ago(240)), zip: z("2"), extra: None },
            Ver { version: "1.1.0", time: Some(ago(newest_hours)), zip: z("1.1"), extra: None },
            Ver { version: "1.0.0", time: Some(ago(240)), zip: z("1.0"), extra: None },
        ],
    );
}

fn dep(s: &Server, versions: &[&str]) {
    let vs = versions.iter().map(|v| Ver { version: v, time: Some(ago(240)), zip: zipball(&[("SKILL.md", &md("dep", ""))]), extra: None });
    s.package("acme/dep", vs.collect());
}

#[test]
fn strict_fails_only_for_a_newer_version_within_the_constraint_past_the_cooldown() {
    let s = Server::start();
    one(&s, 1);
    dep(&s, &["1.0.0"]);
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    assert!(p.read("skills.lock").contains("version = \"1.0.0\""));

    // 2.0.0 is outside the constraint, 1.1.0 is within the cooldown, a newer dependency does not count.
    dep(&s, &["1.1.0", "1.0.0"]);
    let out = p.ok(&["outdated", "--strict"]);
    assert!(out.contains("1.1.0 eligible"), "{out}");

    one(&s, 240);
    let out = p.expect(&["outdated", "--strict"], 1);
    assert!(out.contains("error: 1 package(s) have a newer version within their constraint: acme/one 1.0.0 -> 1.1.0"), "{out}");
    assert!(out.contains("hint: run skillcask update"), "{out}");
    assert!(out.contains("acme/one"), "the table is printed too: {out}");
    p.ok(&["outdated"]);

    p.ok(&["update", "acme/one"]);
    p.ok(&["outdated", "--strict"]);
}

#[test]
fn strict_uses_the_constraint_a_requirer_adds() {
    let s = Server::start();
    let top = zipball(&[("SKILL.md", &md("top", "requires:\n  acme/dep: \"~1.0.0\"\n"))]);
    s.package("acme/top", vec![Ver { version: "1.0.0", time: Some(ago(240)), zip: top, extra: None }]);
    dep(&s, &["1.0.0"]);
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/top\" = \"^1.0\"\n\"acme/dep\" = \"^1.0\"\n");
    p.ok(&["install"]);
    dep(&s, &["1.1.0", "1.0.0"]);
    let out = p.ok(&["outdated", "--strict"]);
    assert!(out.contains("1.1.0"), "the table still shows 1.1.0 within ^1.0: {out}");
}

#[test]
fn plain_outdated_survives_conflicting_requirements() {
    let s = Server::start();
    let top = zipball(&[("SKILL.md", &md("top", "requires:\n  acme/dep: \"~1.0.0\"\n"))]);
    s.package("acme/top", vec![Ver { version: "1.0.0", time: Some(ago(240)), zip: top, extra: None }]);
    dep(&s, &["1.0.0"]);
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/top\" = \"^1.0\"\n\"acme/dep\" = \"^1.0\"\n");
    p.ok(&["install"]);
    // A hyphen range cannot be combined with the manifest's constraint.
    let lock = p.read("skills.lock");
    assert!(lock.contains("\"~1.0.0\""), "{lock}");
    p.write("skills.lock", &lock.replace("\"~1.0.0\"", "\"1.0.0 - 2.0.0\""));
    assert!(p.expect(&["outdated", "--strict"], 1).contains("hyphen range"));
    let out = p.ok(&["outdated"]);
    assert!(out.contains("acme/dep"), "{out}");
}

#[test]
fn strict_on_a_branch_entry_compares_commits() {
    let s = Server::start();
    let (old, new) = ("a".repeat(40), "b".repeat(40));
    let commit = |sha: &str| serde_json::json!({ "sha": sha, "commit": { "committer": { "date": ago(240) } } }).to_string();
    s.put("/repos/acme/tools/tags", "[]");
    s.put("/repos/acme/tools", serde_json::json!({ "default_branch": "main" }).to_string());
    s.put("/repos/acme/tools/commits/HEAD", commit(&old));
    s.put(&format!("/repos/acme/tools/zipball/{old}"), zipball(&[("SKILL.md", &md("tools", ""))]));
    let mut p = Project::new(&s);
    p.env.push(("SKILLCASK_GITHUB_API".into(), s.base.clone()));
    p.manifest(&s, "");
    p.ok(&["add", "github:acme/tools"]);
    p.ok(&["outdated", "--strict"]);

    s.put("/repos/acme/tools/commits/HEAD", commit(&new));
    s.put(&format!("/repos/acme/tools/commits/{new}"), commit(&new));
    s.put(&format!("/repos/acme/tools/zipball/{new}"), zipball(&[("SKILL.md", &md("tools", "")), ("v.txt", "2")]));
    let out = p.expect(&["outdated", "--strict"], 1);
    assert!(out.contains("1 package(s) have a newer version within their constraint: acme/tools"), "{out}");
}
