//! `diff` and `update --diff`: what the next version changes in a package's skill files.
mod common;

use common::*;

const NOTES: &str = "a\nb\nc\n";

fn v1() -> Vec<u8> {
    zipball(&[("SKILL.md", &skill_md("x")), ("notes.txt", NOTES), ("logo.png", "\0png1"), ("old.md", "gone\n")])
}

fn v2() -> Vec<u8> {
    zipball(&[
        ("SKILL.md", &skill_md("x")),
        ("notes.txt", "a\nB\nc\n"),
        ("logo.png", "\0png2"),
        ("new.md", "hi\u{202E}x\n\u{1b}]52;c;ZXZp\u{7}\n"),
        ("run.sh", "#!/bin/sh\n"),
    ])
}

fn project(s: &Server) -> Project {
    s.package("acme/x", vec![Ver { version: "1.0.0", time: Some(ago(240)), zip: v1(), extra: None }]);
    let p = Project::new(s);
    p.manifest(s, "");
    p.ok(&["add", "acme/x"]);
    p
}

fn release(s: &Server, hours: i64) {
    s.package(
        "acme/x",
        vec![
            Ver { version: "1.1.0", time: Some(ago(hours)), zip: v2(), extra: None },
            Ver { version: "1.0.0", time: Some(ago(240)), zip: v1(), extra: None },
        ],
    );
}

#[test]
fn diff_shows_what_update_would_lock() {
    let s = Server::start();
    let p = project(&s);
    release(&s, 1);
    let out = p.ok(&["diff"]);
    assert!(out.contains("no newer versions"), "1.1.0 is within the cooldown: {out}");

    release(&s, 240);
    let o = p.run(&["diff"]);
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(o.status.success(), "{out}{err}");
    assert!(out.starts_with("acme/x 1.0.0 -> 1.1.0\n"), "{out}");
    assert!(out.contains("--- a/x/notes.txt\n+++ b/x/notes.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"), "{out}");
    assert!(out.contains("--- a/x/logo.png\n+++ b/x/logo.png\nBinary files differ\n"), "{out}");
    assert!(out.contains("--- a/x/old.md\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-gone\n"), "{out}");
    assert!(out.contains("--- /dev/null\n+++ b/x/new.md\n@@ -0,0 +1,2 @@\n+hi<U+202E>x\n+<U+001B>]52;c;ZXZp<U+0007>\n"), "{out}");
    assert!(out.contains("--- /dev/null\n+++ b/x/run.sh\nnew mode 100755\n@@ -0,0 +1,1 @@\n+#!/bin/sh\n"), "{out}");
    assert!(!out.contains("SKILL.md") && !out.contains('\u{1b}') && !out.contains('\u{202E}'), "{out}");
    assert!(err.contains("warning: acme/x: x/new.md:1: U+202E RIGHT-TO-LEFT OVERRIDE\n"), "{err}");
    assert_eq!(err.matches("warning: acme/x: x/").count(), 1, "only changed files are scanned: {err}");
    assert!(p.read("skills.lock").contains("version = \"1.0.0\""), "diff changes nothing");

    // A modified install is not the locked version: the locked archive is compared instead.
    p.write(".agents/skills/x/notes.txt", "local edit\n");
    let out = p.ok(&["diff", "acme/x"]);
    assert!(out.contains("\n-b\n+B\n") && !out.contains("local edit"), "{out}");

    assert_eq!(p.ok(&["diff", "acme/x", "--to", "1.0.0"]).lines().count(), 1, "same version, nothing to show");
    assert!(p.expect(&["diff", "acme/x", "--to", "3.0.0"], 1).contains("no version of acme/x matches \"3.0.0\""));
    assert!(p.expect(&["diff", "--to", "1.1.0"], 1).contains("--to needs a package"));
    assert!(p.expect(&["diff", "acme/nope"], 1).contains("acme/nope is not in skills.toml"));
}

#[test]
fn update_diff_prints_the_changes_it_locks() {
    let s = Server::start();
    let p = project(&s);
    release(&s, 240);
    let out = p.ok(&["update", "--diff"]);
    assert!(out.contains("acme/x 1.0.0 -> 1.1.0\n") && out.contains("\n-b\n+B\n"), "{out}");
    assert_eq!(out.matches("U+202E RIGHT-TO-LEFT OVERRIDE").count(), 1, "update scans once: {out}");
    assert!(p.read("skills.lock").contains("version = \"1.1.0\""));
    assert_eq!(p.read(".agents/skills/x/notes.txt"), "a\nB\nc\n");
}

#[test]
fn update_diff_writes_nothing_when_the_old_side_cannot_be_read() {
    let s = Server::start();
    let p = project(&s);
    release(&s, 240);
    // A missing install sends the old side to the locked dist, which is gone.
    std::fs::remove_dir_all(p.path().join(".agents/skills/x")).unwrap();
    s.respond("/dist/acme/x/1.0.0.zip", 404, &[], "");
    p.expect(&["update", "--diff"], 3);
    assert!(p.read("skills.lock").contains("version = \"1.0.0\""), "the lock is not written");
    assert!(!p.exists(".agents/skills/x"), "nothing is installed");
}

#[test]
fn a_new_dependency_is_diffed_against_nothing() {
    let s = Server::start();
    let p = project(&s);
    s.simple("acme/dep", "1.0.0", "dep");
    let md = "---\nname: x\ndescription: d\nrequires:\n  acme/dep: \"^1.0\"\n---\n";
    s.package(
        "acme/x",
        vec![
            Ver { version: "1.1.0", time: Some(ago(240)), zip: zipball(&[("SKILL.md", md), ("notes.txt", NOTES)]), extra: None },
            Ver { version: "1.0.0", time: Some(ago(240)), zip: v1(), extra: None },
        ],
    );
    let added = "acme/dep 1.0.0 (new)\n--- /dev/null\n+++ b/dep/SKILL.md\n";
    let out = p.ok(&["diff"]);
    assert!(out.contains(added), "{out}");
    let out = p.ok(&["update", "--diff"]);
    assert!(out.contains(added) && out.contains("acme/x 1.0.0 -> 1.1.0\n"), "{out}");
}

#[test]
fn a_line_end_change_is_visible() {
    let s = Server::start();
    let p = project(&s);
    s.package(
        "acme/x",
        vec![
            Ver {
                version: "1.1.0",
                time: Some(ago(240)),
                zip: zipball(&[("SKILL.md", &skill_md("x")), ("notes.txt", "a\r\nb\r\nc\r\n")]),
                extra: None,
            },
            Ver { version: "1.0.0", time: Some(ago(240)), zip: v1(), extra: None },
        ],
    );
    let out = p.ok(&["diff"]);
    assert!(out.contains("-a\n-b\n-c\n+a<U+000D>\n+b<U+000D>\n+c<U+000D>\n"), "{out}");
}
