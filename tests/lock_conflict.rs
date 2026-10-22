//! A skills.lock left with git conflict markers.
mod common;

use common::*;

#[test]
fn conflict_markers_fail_with_a_hint_and_install_completes_either_side() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.simple("acme/two", "1.0.0", "two");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    let ours = p.read("skills.lock");

    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n\"acme/two\" = \"^1.0\"\n");
    let at = ours.find("\n[[package]]").unwrap();
    p.write("skills.lock", &format!("{}\n<<<<<<< HEAD\n{}=======\n>>>>>>> feature\n", &ours[..at], &ours[at..]));
    let out = p.expect(&["install"], 1);
    assert!(out.contains("skills.lock: unresolved merge conflict") && out.contains("hint:") && out.contains("skillcask install"), "{out}");

    p.write("skills.lock", &ours);
    p.ok(&["install"]);
    let lock = p.read("skills.lock");
    assert!(lock.contains("name = \"acme/one\"") && lock.contains("name = \"acme/two\""), "{lock}");
    assert!(p.exists(".agents/skills/two/SKILL.md"));
}
