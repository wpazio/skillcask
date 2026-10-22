mod common;

use common::*;
use serde_json::json;

fn multi(s: &Server) {
    let zip = zipball(&[("skills/alpha/SKILL.md", &skill_md("alpha")), ("skills/beta/SKILL.md", &skill_md("beta"))]);
    s.package("acme/multi", vec![Ver { version: "1.0.0", time: Some(ago(240)), zip, extra: Some(json!(["skills/alpha", "skills/beta"])) }]);
}

/// `acme/tagged`: tag 1.0.0 at an old commit, also reachable as ref "1.0.0".
fn tagged(s: &Server) -> String {
    let sha = "1".repeat(40);
    s.put("/repos/acme/tagged/tags", json!([{ "name": "1.0.0", "commit": { "sha": sha } }]).to_string());
    s.put(&format!("/repos/acme/tagged/commits/{sha}"), gh_commit(&sha, 240).to_string());
    s.put("/repos/acme/tagged/commits/1.0.0", gh_commit(&sha, 240).to_string());
    s.put("/repos/acme/tagged/commits/main", gh_commit(&sha, 240).to_string());
    s.put(&format!("/repos/acme/tagged/zipball/{sha}"), zipball(&[("SKILL.md", &skill_md("tagged"))]));
    sha
}

#[test]
fn dropping_the_skill_selection_is_drift() {
    let s = Server::start();
    multi(&s);
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"] }\n");
    p.ok(&["install"]);
    let lock = p.read("skills.lock");
    assert!(lock.contains("selected = [\"beta\"]\n"), "{lock}");

    p.manifest(&s, "\"acme/multi\" = \"^1.0\"\n");
    let out = p.expect(&["install", "--frozen"], 1);
    assert!(out.contains("acme/multi: skill selection changed"), "{out}");
    assert!(p.expect(&["status"], 1).contains("skill selection changed"));
    p.ok(&["install"]);
    assert!(p.exists(".agents/skills/alpha") && p.exists(".agents/skills/beta"));
    assert!(!p.read("skills.lock").contains("selected"));
    p.ok(&["install", "--frozen"]);

    // A lock written before `selected` existed: absent means all, unless skills.toml selects.
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"] }\n");
    p.ok(&["install"]);
    let old = lock.replace("selected = [\"beta\"]\n", "");
    p.write("skills.lock", &old);
    p.ok(&["install", "--frozen"]);
    p.ok(&["status"]);
    assert_eq!(p.read("skills.lock"), old, "a frozen install writes nothing");
    p.ok(&["install"]);
    assert_eq!(p.read("skills.lock"), lock, "the first install adds the new keys");
}

#[test]
fn a_changed_repository_url_is_drift() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    p.write("skills.toml", &p.read("skills.toml").replace("/packages.json", "/moved/packages.json"));
    let out = p.expect(&["install", "--frozen"], 1);
    assert!(out.contains("acme/one: repository changed"), "{out}");
    assert!(p.expect(&["status"], 1).contains("repository changed"));
}

/// A second repository at `/pub/packages.json` with inline metadata.
fn public(s: &Server, name: &str, version: &str, skill: &str) {
    let dist = format!("/pub/{name}/{version}.zip");
    s.put(&dist, zipball(&[("SKILL.md", &skill_md(skill))]));
    s.put(
        "/pub/packages.json",
        json!({ "packages": { name: { version: {
            "name": name, "version": version, "version_normalized": format!("{version}.0"), "type": "ai-agent-skill",
            "time": ago(240), "dist": { "type": "zip", "url": s.url(&dist) },
        } } } })
        .to_string(),
    );
}

fn two_repos(p: &Project, s: &Server, skills: &str) {
    p.write(
        "skills.toml",
        &format!(
            "[repositories]\nfix = {{ url = \"{}\" }}\npub = {{ url = \"{}\" }}\n\n[skills]\n{skills}\n[policy]\npackagist = false\n",
            s.url("/packages.json"),
            s.url("/pub/packages.json")
        ),
    );
}

#[test]
fn a_locked_package_is_resolved_only_in_its_locked_repository() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    public(&s, "acme/one", "9.0.0", "one");
    let p = Project::new(&s);
    two_repos(&p, &s, "\"acme/one\" = \"*\"\n");
    p.ok(&["install"]);
    assert!(p.read("skills.lock").contains("version = \"1.0.0\""));

    s.respond("/p2/acme/one.json", 404, &[], "");
    let (toml, lock) = (p.read("skills.toml"), p.read("skills.lock"));
    let out = p.expect(&["update"], 1);
    assert!(out.contains("acme/one: not found in its locked repository") && !out.contains("9.0.0"), "{out}");
    assert_eq!((p.read("skills.toml"), p.read("skills.lock")), (toml.clone(), lock.clone()));
    p.write("skills.toml", &toml.replace("\"*\"", "\">=1.0\""));
    assert!(p.expect(&["install"], 1).contains("not found in its locked repository"));
    assert_eq!(p.read("skills.lock"), lock);

    // Moving it on purpose is allowed.
    p.write("skills.toml", &toml.replace("\"*\"", "{ version = \"*\", repository = \"pub\" }"));
    p.ok(&["install"]);
    assert!(p.read("skills.lock").contains("version = \"9.0.0\""));

    // Unlocked packages are still searched in order.
    let q = Project::new(&s);
    public(&s, "acme/two", "1.0.0", "two");
    two_repos(&q, &s, "");
    q.ok(&["add", "acme/two"]);
    assert!(q.read("skills.lock").contains("/pub/packages.json"));
}

#[test]
fn github_version_to_ref_with_the_same_string_is_drift() {
    let s = Server::start();
    tagged(&s);
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "\"acme/tagged\" = { github = true, version = \"1.0.0\" }\n");
    p.ok(&["install"]);
    let lock = p.read("skills.lock");
    assert!(lock.contains("constraint = \"1.0.0\"\nref = false\n"), "{lock}");

    p.manifest(&s, "\"acme/tagged\" = { github = true, ref = \"1.0.0\" }\n");
    let out = p.expect(&["install", "--frozen"], 1);
    assert!(out.contains("acme/tagged: changed from version to ref"), "{out}");
    assert!(p.expect(&["status"], 1).contains("changed from version to ref"));
    p.ok(&["install"]);
    assert!(p.read("skills.lock").contains("ref = true\n"));
    p.ok(&["install", "--frozen"]);

    // A lock without the key does not fail a frozen install.
    p.write("skills.lock", &p.read("skills.lock").replace("ref = true\n", ""));
    p.ok(&["install", "--frozen"]);
}

#[test]
fn status_reports_every_drift_reason() {
    let s = Server::start();
    multi(&s);
    s.simple("acme/one", "1.0.0", "one");
    s.simple("acme/two", "1.0.0", "two");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"] }\n\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    p.ok(&["status"]);

    p.manifest(&s, "\"acme/multi\" = \"^1.0\"\n\"acme/one\" = { version = \"^1.0\", rename = { one = \"x\" } }\n");
    let out = p.expect(&["status"], 1);
    assert!(out.contains("skill selection changed") && out.contains("rename changed") && out.contains("2 problem(s)"), "{out}");

    let q = Project::new(&s);
    q.manifest(&s, "\"acme/one\" = \"^1.0\"\n\"acme/two\" = \"^1.0\"\n");
    q.ok(&["install"]);
    q.write("skills.lock", &q.read("skills.lock").replace("name = \"two\"", "name = \"one\""));
    let out = q.expect(&["status"], 1);
    assert!(out.contains("skill \"one\" is provided by both acme/one and acme/two"), "{out}");
}

#[test]
fn install_keeps_the_locked_version_when_only_rename_or_selection_changed() {
    let s = Server::start();
    let z = |n: &str| zipball(&[("skills/alpha/SKILL.md", &skill_md("alpha")), ("skills/beta/SKILL.md", &skill_md(n))]);
    let paths = || Some(json!(["skills/alpha", "skills/beta"]));
    s.package("acme/multi", vec![Ver { version: "1.0.0", time: Some(ago(240)), zip: z("beta"), extra: paths() }]);
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"] }\n");
    p.ok(&["install"]);
    s.package(
        "acme/multi",
        vec![
            Ver { version: "1.1.0", time: Some(ago(100)), zip: z("beta"), extra: paths() },
            Ver { version: "1.0.0", time: Some(ago(240)), zip: z("beta"), extra: paths() },
        ],
    );

    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"], rename = { beta = \"acme-beta\" } }\n");
    p.ok(&["install"]);
    let lock = p.read("skills.lock");
    assert!(lock.contains("version = \"1.0.0\"") && lock.contains("from = \"beta\""), "{lock}");
    assert!(p.exists(".agents/skills/acme-beta"));

    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", rename = { beta = \"acme-beta\" } }\n");
    p.ok(&["install"]);
    assert!(p.read("skills.lock").contains("version = \"1.0.0\""));
    assert!(p.exists(".agents/skills/alpha"));
    p.ok(&["install", "--frozen"]);

    p.manifest(&s, "\"acme/multi\" = { version = \"~1.1\", rename = { beta = \"acme-beta\" } }\n");
    p.ok(&["install"]);
    assert!(p.read("skills.lock").contains("version = \"1.1.0\""), "a constraint the locked version fails re-resolves");
}

#[test]
fn a_rename_on_a_0_1_2_github_lock_keeps_the_locked_tag() {
    let s = Server::start();
    tagged(&s);
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "\"acme/tagged\" = { github = true, version = \"^1.0\" }\n");
    p.ok(&["install"]);
    p.write("skills.lock", &p.read("skills.lock").replace("ref = false\n", ""));
    let sha = "2".repeat(40);
    s.put(
        "/repos/acme/tagged/tags",
        json!([{ "name": "1.1.0", "commit": { "sha": sha } }, { "name": "1.0.0", "commit": { "sha": "1".repeat(40) } }]).to_string(),
    );
    s.put(&format!("/repos/acme/tagged/commits/{sha}"), gh_commit(&sha, 240).to_string());
    s.put(&format!("/repos/acme/tagged/zipball/{sha}"), zipball(&[("SKILL.md", &skill_md("tagged"))]));

    p.manifest(&s, "\"acme/tagged\" = { github = true, version = \"^1.0\", rename = { tagged = \"acme-tagged\" } }\n");
    p.ok(&["install"]);
    let lock = p.read("skills.lock");
    assert!(lock.contains("version = \"1.0.0\"") && lock.contains("ref = false\n"), "{lock}");
}

#[test]
fn add_without_constraint_keeps_the_existing_one() {
    let s = Server::start();
    let z = || zipball(&[("SKILL.md", &skill_md("one"))]);
    s.package(
        "acme/one",
        vec![
            Ver { version: "2.0.0", time: Some(ago(240)), zip: z(), extra: None },
            Ver { version: "1.0.5", time: Some(ago(240)), zip: z(), extra: None },
        ],
    );
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"~1.0\"   # pinned: 2.x drops X\n");
    p.ok(&["add", "acme/one"]);
    assert!(p.read("skills.toml").contains("\"acme/one\" = \"~1.0\"   # pinned: 2.x drops X\n"), "{}", p.read("skills.toml"));
    assert!(p.read("skills.lock").contains("version = \"1.0.5\""));
}

#[test]
fn manifest_edits_keep_comments_and_table_forms() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.simple("acme/two", "1.0.0", "two");
    s.simple("acme/three", "1.0.0", "three");
    let p = Project::new(&s);
    p.manifest(&s, "# above one\n\"acme/one\" = \"^1.0\"   # why one\n# above two\n\"acme/two\".version = \"^1.0\"   # why two\n\n# above three\n[skills.\"acme/three\"]\n# inner\nversion = \"^1.0\"   # why three\n\n");
    p.ok(&["install"]);
    for n in ["one", "two", "three"] {
        p.ok(&["add", &format!("acme/{n}@~1.0")]);
    }
    let t = p.read("skills.toml");
    assert!(t.contains("# above one\n\"acme/one\" = \"~1.0\"   # why one\n"), "{t}");
    assert!(t.contains("# above two\n\"acme/two\".version = \"~1.0\"   # why two\n"), "{t}");
    assert!(t.contains("# above three\n[skills.\"acme/three\"]\n# inner\nversion = \"~1.0\"   # why three\n"), "{t}");
    p.ok(&["install", "--frozen"]);

    p.ok(&["remove", "acme/two"]);
    let t = p.read("skills.toml");
    assert!(!t.contains("acme/two") && !t.contains("why two"), "{t}");
    assert!(t.contains("# why one") && t.contains("# above three") && t.contains("# inner"), "{t}");
    p.ok(&["remove", "acme/three"]);
    let t = p.read("skills.toml");
    assert!(!t.contains("acme/three") && t.contains("# why one") && t.contains("packagist = false"), "{t}");
    p.ok(&["remove", "acme/one"]);
    let t = p.read("skills.toml");
    assert!(!t.contains("acme/one") && t.contains("[policy]"), "{t}");
}

#[cfg(unix)]
#[test]
fn remove_writes_nothing_when_preflight_fails() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"   # keep\n");
    p.ok(&["install"]);
    let (toml, lock) = (p.read("skills.toml"), p.read("skills.lock"));
    let outside = tempfile::tempdir().unwrap();
    std::fs::rename(p.path().join(".junie"), outside.path().join("junie")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("junie"), p.path().join(".junie")).unwrap();
    assert!(p.expect(&["remove", "acme/one"], 1).contains(".junie is a symlink"));
    assert_eq!(p.read("skills.toml"), toml);
    assert_eq!(p.read("skills.lock"), lock);
}

#[test]
fn strict_add_writes_a_full_exact_version() {
    let s = Server::start();
    s.package("acme/two", vec![Ver { version: "1.2", time: Some(ago(240)), zip: zipball(&[("SKILL.md", &skill_md("two"))]), extra: None }]);
    let p = Project::new(&s);
    p.manifest(&s, "");
    p.write("skills.toml", &p.read("skills.toml").replace("packagist = false", "packagist = false\nallow-ranges = false"));
    p.ok(&["add", "acme/two"]);
    assert!(p.read("skills.toml").contains("\"acme/two\" = \"1.2.0\""), "{}", p.read("skills.toml"));
    p.ok(&["install", "--frozen"]);
}

#[test]
fn update_of_one_package_also_locks_new_entries() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.simple("acme/two", "1.0.0", "two");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n\"acme/two\" = \"^1.0\"\n");
    let out = p.ok(&["update", "acme/one"]);
    assert!(out.contains("locked acme/two 1.0.0"), "{out}");
    p.ok(&["install", "--frozen"]);
}

#[cfg(unix)]
#[test]
fn update_reports_nothing_when_it_fails() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    s.simple("acme/one", "1.1.0", "one");
    let outside = tempfile::tempdir().unwrap();
    std::fs::rename(p.path().join(".junie"), outside.path().join("junie")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("junie"), p.path().join(".junie")).unwrap();
    let out = p.expect(&["update"], 1);
    assert!(!out.contains("updated"), "{out}");
    assert!(p.read("skills.lock").contains("version = \"1.0.0\""));
}

#[test]
fn add_of_a_ref_for_an_existing_github_entry() {
    let s = Server::start();
    tagged(&s);
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "\"acme/tagged\" = { github = true, version = \"^1.0\" }   # tags\n");
    p.ok(&["add", "acme/tagged@main"]);
    let t = p.read("skills.toml");
    assert!(t.contains("\"acme/tagged\" = { github = true, ref = \"main\" }   # tags\n"), "{t}");
    p.ok(&["install", "--frozen"]);
}

#[test]
fn add_of_a_missing_skill_fails_when_all_are_selected() {
    let s = Server::start();
    tagged(&s);
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "\"acme/tagged\" = { github = true, version = \"^1.0\" }\n");
    p.ok(&["install"]);
    let (toml, lock) = (p.read("skills.toml"), p.read("skills.lock"));
    let out = p.expect(&["add", "github:acme/tagged/nope"], 1);
    assert!(out.contains("no skill \"nope\""), "{out}");
    assert_eq!((p.read("skills.toml"), p.read("skills.lock")), (toml, lock));
    p.ok(&["add", "github:acme/tagged/tagged"]);
}

#[test]
fn another_github_api_base_is_not_drift() {
    let s = Server::start();
    tagged(&s);
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "\"acme/tagged\" = { github = true, version = \"^1.0\" }\n");
    p.ok(&["install"]);
    let mut ci = clone_of(&p, &s);
    ci.env.push(("SKILLCASK_GITHUB_API".into(), format!("{}/api/v3", s.base)));
    ci.ok(&["install", "--frozen"]);
    ci.ok(&["status"]);
}
