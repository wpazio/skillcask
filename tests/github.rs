//! GitHub source: the cooldown uses times GitHub sets, not commit dates the author chooses.
mod common;

use common::*;
use serde_json::json;

#[test]
fn a_backdated_tag_waits_for_its_release_time() {
    let s = Server::start();
    let (c1, c2) = ("1".repeat(40), "2".repeat(40));
    s.put(
        "/repos/acme/tagged/tags",
        json!([{ "name": "v1.1.0", "commit": { "sha": c2 } }, { "name": "v1.0.0", "commit": { "sha": c1 } }]).to_string(),
    );
    s.put(&format!("/repos/acme/tagged/commits/{c1}"), gh_commit(&c1, 300).to_string());
    s.put(&format!("/repos/acme/tagged/commits/{c2}"), gh_commit(&c2, 240).to_string());
    s.put("/repos/acme/tagged/releases", json!([{ "tag_name": "v1.1.0", "published_at": ago(1) }]).to_string());
    s.put(&format!("/repos/acme/tagged/zipball/{c1}"), zipball(&[("SKILL.md", &skill_md("tagged"))]));
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "");
    p.ok(&["add", "github:acme/tagged"]);
    assert!(p.read("skills.lock").contains("version = \"v1.0.0\""), "{}", p.read("skills.lock"));
    let tag_reads = s.log().iter().filter(|l| l.starts_with("/repos/acme/tagged/tags")).count();
    assert_eq!(tag_reads, 1, "`add` reads the tags once");
}

#[test]
fn a_backdated_commit_waits_for_its_push_time() {
    let s = Server::start();
    let (old, new) = ("a".repeat(40), "b".repeat(40));
    s.put("/repos/acme/tools/tags", "[]");
    s.put("/repos/acme/tools", json!({ "default_branch": "main" }).to_string());
    // The head claims to be 10 days old but was pushed an hour ago.
    s.put("/repos/acme/tools/commits/HEAD", gh_commit(&new, 240).to_string());
    s.put(&format!("/repos/acme/tools/commits/{old}"), gh_commit(&old, 300).to_string());
    s.put(
        "/repos/acme/tools/activity",
        json!([
            { "before": old, "after": new, "timestamp": ago(1) },
            { "before": "0".repeat(40), "after": old, "timestamp": ago(300) },
        ])
        .to_string(),
    );
    s.put(&format!("/repos/acme/tools/zipball/{old}"), zipball(&[("SKILL.md", &skill_md("tools"))]));
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "");
    let out = p.ok(&["add", "github:acme/tools"]);
    assert!(out.contains("newer pushes to \"HEAD\""), "{out}");
    assert!(p.read("skills.lock").contains(&format!("zipball/{old}")), "{}", p.read("skills.lock"));
    assert!(s.log().iter().any(|l| l.starts_with("/repos/acme/tools/activity?ref=main&")), "{:?}", s.log());

    // Only pushes inside the cooldown, the oldest creating the branch: nothing is eligible.
    s.put(
        "/repos/acme/tools/activity",
        json!([{ "before": old, "after": new, "timestamp": ago(1) }, { "before": "0".repeat(40), "after": old, "timestamp": ago(2) }])
            .to_string(),
    );
    let q = with_github(Project::new(&s), &s);
    q.manifest(&s, "");
    assert!(q.expect(&["add", "github:acme/tools"], 1).contains("every push to \"HEAD\" is newer"));
}
