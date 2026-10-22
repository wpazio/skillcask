//! `skillcask validate` against the Agent Skills SKILL.md rules, and `publish` naming skipped files.
mod common;

use common::*;

fn md(name: &str, rest: &str) -> String {
    format!("---\nname: {name}\ndescription: Does a thing.\n{rest}---\n# body\n")
}

#[test]
fn valid_skills_pass_and_invisible_unicode_is_a_warning() {
    let s = Server::start();
    let p = Project::new(&s);
    p.write("skills/alpha/SKILL.md", &md("alpha", "compatibility: Needs git\n"));
    p.write("skills/nested/beta/SKILL.md", &format!("{}line \u{2066}x\n", md("beta", "")));
    let out = p.ok(&["validate"]);
    assert!(out.contains("warning: skills/nested/beta/SKILL.md:6: U+2066 LEFT-TO-RIGHT ISOLATE"), "{out}");
    assert!(out.contains("checked 2 skills"), "{out}");
    let out = p.ok(&["--agent", "validate", "skills/alpha"]);
    assert_eq!(out, "", "agents get problems only");
}

#[test]
fn reports_every_problem_and_fails() {
    let s = Server::start();
    let p = Project::new(&s);
    p.write("ok/SKILL.md", &md("ok", ""));
    p.write("upper/SKILL.md", &md("Upper", ""));
    p.write("mismatch/SKILL.md", &md("other", ""));
    p.write("double--dash/SKILL.md", &md("double--dash", ""));
    p.write("nodesc/SKILL.md", "---\nname: nodesc\n---\n");
    p.write("long/SKILL.md", &format!("---\nname: long\ndescription: {}\ncompatibility: {}\n---\n", "d".repeat(1025), "c".repeat(501)));
    p.write("nofm/SKILL.md", "# just markdown\n");
    p.write("badreq/SKILL.md", &md("badreq", "requires:\n  - acme/x\n"));
    let out = p.expect(&["validate"], 1);
    for want in [
        "error: upper: name \"Upper\" must be 1-64 lowercase letters, digits and single hyphens, not starting or ending with a hyphen",
        "error: mismatch: name \"other\" does not match the directory name \"mismatch\"",
        "error: double--dash: name \"double--dash\" must be",
        "error: nodesc: description is required",
        "error: long: description has 1025 characters, at most 1024",
        "error: long: compatibility has 501 characters, at most 500",
        "error: nofm: SKILL.md has no frontmatter",
        "error: badreq: requires:",
        "error: 7 of 8 skills are invalid",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(!out.contains("error: ok:"), "{out}");
}

#[test]
fn a_path_without_skills_fails() {
    let s = Server::start();
    let p = Project::new(&s);
    p.write("empty/readme.md", "x");
    let out = p.expect(&["validate", "empty"], 1);
    assert!(out.contains("no SKILL.md under empty"), "{out}");
}

#[test]
fn publish_names_the_skill_md_files_it_skips() {
    let s = Server::start();
    let p = Project::new(&s);
    p.write("src/a/SKILL.md", "---\nname: a\nversion: 1.0.0\ndescription: d\n---\n");
    p.write("src/draft/SKILL.md", "---\nname: draft\ndescription: d\n---\n");
    p.write("src/plain/SKILL.md", "# no frontmatter\n");
    let out = p.path().join("out");
    let text = p.ok(&["publish", "--vendor", "acme", "--base-url", "https://x.example", "--out", out.to_str().unwrap(), "src"]);
    assert!(text.contains("warning: skipping src/draft/SKILL.md: no version in the frontmatter"), "{text}");
    assert!(text.contains("warning: skipping src/plain/SKILL.md: no name and no version in the frontmatter"), "{text}");
    assert!(text.contains("published acme/a 1.0.0"), "{text}");
}
