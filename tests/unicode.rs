//! Warnings for invisible Unicode in skill files at `add` and `update`.
mod common;

use common::*;
use serde_json::json;

fn package(s: &Server, version: &str, skill_md: &str) {
    let zip = zipball(&[("skills/foo/SKILL.md", skill_md), ("skills/foo/notes.txt", "a\u{200B}b\n")]);
    s.package("acme/x", vec![Ver { version, time: Some(ago(240)), zip, extra: Some(json!(["skills/foo"])) }]);
}

#[test]
fn add_and_update_warn_with_file_line_and_codepoint() {
    let s = Server::start();
    // CRLF line ends, a leading BOM (not reported) and an emoji ZWJ sequence (not reported).
    package(&s, "1.0.0", "\u{feff}---\r\nname: foo\r\ndescription: d\r\n---\r\n\u{1F469}\u{200D}\u{1F4BB} ok\r\nrun \u{202E}txt.exe\r\n");
    let p = Project::new(&s);
    p.manifest(&s, "");
    let out = p.ok(&["add", "acme/x"]);
    assert!(out.contains("warning: acme/x: skills/foo/SKILL.md:6: U+202E RIGHT-TO-LEFT OVERRIDE\n"), "{out}");
    assert!(out.contains("warning: acme/x: skills/foo/notes.txt:1: U+200B ZERO WIDTH SPACE\n"), "{out}");
    assert_eq!(out.matches("warning:").count(), 2, "BOM and emoji are not reported: {out}");
    assert!(p.exists(".agents/skills/foo/SKILL.md"), "warnings do not stop the install");

    assert!(!clone_of(&p, &s).ok(&["install"]).contains("warning"), "installing the reviewed lock does not warn again");

    let many = format!("---\nname: foo\ndescription: d\n---\n{}", "\u{E0041}".repeat(7));
    package(&s, "1.0.1", &many);
    let out = p.ok(&["update"]);
    assert_eq!(out.matches("skills/foo/SKILL.md:5: U+E0041 TAG CHARACTER").count(), 5, "{out}");
    assert!(out.contains("warning: acme/x: skills/foo/SKILL.md: 2 more invisible characters"), "{out}");

    // The same dist again was scanned when it was locked.
    let out = p.ok(&["update"]);
    assert!(!out.contains("warning"), "{out}");
}
