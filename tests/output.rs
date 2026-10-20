//! Output for people and for agents (D19).
mod common;

use common::*;

fn installed(s: &Server) -> Project {
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(s);
    p.manifest(s, "");
    p.ok(&["add", "acme/one"]);
    p
}

#[test]
fn status_for_an_agent_lists_problems_only() {
    let s = Server::start();
    let mut p = installed(&s);
    let person = p.ok(&["status"]);
    assert!(person.contains("package") && person.contains("✓ 1 skill(s) match skills.lock"), "{person}");
    assert!(!person.contains('\x1b'), "no colour without a terminal: {person}");

    p.env.push(("SKILLCASK_AGENT".into(), "1".into()));
    assert_eq!(p.ok(&["status"]).trim(), "status: ok");

    p.write(".agents/skills/one/SKILL.md", "changed");
    let out = p.expect(&["status"], 1);
    assert!(out.contains("acme/one\t1.0.0\tone\tmodified"), "{out}");
    assert!(!out.contains("\tok"), "{out}");
    assert!(out.contains("error: 1 problem(s)\n  hint: run skillcask install"), "{out}");
}

#[test]
fn agents_are_detected_and_can_opt_out() {
    let s = Server::start();
    let mut p = installed(&s);
    p.env.push(("AI_AGENT".into(), "some-agent".into()));
    assert_eq!(p.ok(&["status"]).trim(), "status: ok");
    p.env.push(("SKILLCASK_AGENT".into(), "0".into()));
    assert!(p.ok(&["status"]).contains("✓ 1 skill(s)"));
    let mut q = installed(&s);
    q.env.push(("CLAUDECODE".into(), "1".into()));
    assert_eq!(q.ok(&["status"]).trim(), "status: ok");
    assert_eq!(installed(&s).ok(&["--agent", "status"]).trim(), "status: ok");
}

#[test]
fn doctor_groups_items_and_summarises() {
    let s = Server::start();
    let mut p = installed(&s);
    std::fs::remove_file(p.path().join(".claude/skills/one")).ok();
    std::fs::remove_dir_all(p.path().join(".claude/skills/one")).ok();
    let out = p.expect(&["doctor"], 1);
    assert!(out.contains("  ✗ .claude/skills/one: link missing"), "{out}");
    assert!(out.contains("hint: skillcask doctor --fix repairs links"), "{out}");
    let fixed = p.ok(&["doctor", "--fix"]);
    assert!(fixed.contains("  ✓ .claude/skills/one: link missing") && fixed.contains("✓ fixed 1 problem(s)"), "{fixed}");

    p.env.push(("SKILLCASK_AGENT".into(), "1".into()));
    assert_eq!(p.ok(&["doctor"]).trim(), "doctor: no problems found");
    std::fs::remove_file(p.path().join(".claude/skills/one")).ok();
    std::fs::remove_dir_all(p.path().join(".claude/skills/one")).ok();
    let out = p.expect(&["doctor"], 1);
    assert!(out.contains("problem: .claude/skills/one: link missing") && !out.contains('✗'), "{out}");
}

#[test]
fn doctor_lists_its_checks_for_a_person() {
    let s = Server::start();
    let mut p = installed(&s);
    let out = p.ok(&["doctor"]);
    for check in [
        "  ✓ links: ",
        "  ✓ git index: no managed paths committed",
        "  ✓ .gitignore: managed block up to date",
        "  ✓ installed: 1 skill(s) match skills.lock",
    ] {
        assert!(out.contains(check), "{check:?} in {out}");
    }
    p.write(".agents/skills/one/SKILL.md", "changed");
    let out = p.expect(&["doctor"], 1);
    assert!(out.contains("✗ .agents/skills/one: modified") && !out.contains("✓ installed:"), "{out}");
    assert!(out.contains("  ✓ links: "), "passed checks still show next to a failed one: {out}");

    p.env.push(("SKILLCASK_AGENT".into(), "1".into()));
    p.ok(&["install"]);
    assert_eq!(p.ok(&["doctor"]).trim(), "doctor: no problems found");
}

#[test]
fn colour_follows_clicolor_force_and_no_color() {
    let s = Server::start();
    let mut p = Project::new(&s);
    p.env.push(("CLICOLOR_FORCE".into(), "1".into()));
    let out = p.expect(&["status"], 1);
    assert!(out.contains("\x1b[1;31merror\x1b[0m: skills.toml not found"), "{out:?}");
    assert!(out.contains("\x1b[1;36mhint\x1b[0m: run skillcask init"), "{out:?}");
    p.env.push(("NO_COLOR".into(), "1".into()));
    let out = p.expect(&["status"], 1);
    assert!(!out.contains('\x1b') && out.contains("error: skills.toml not found\n  hint: run skillcask init"), "{out:?}");
}

#[test]
fn an_environment_override_is_warned_about_once_per_run() {
    let s = Server::start();
    let mut p = installed(&s);
    let dir = p.home().join(".config/skillcask");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "[policy]\nallow-ranges = false\nenforce = [\"allow-ranges\"]\n").unwrap();
    p.env.push(("SKILLCASK_ALLOW_RANGES".into(), "true".into()));
    let out = p.ok(&["status"]);
    assert_eq!(out.matches("SKILLCASK_ALLOW_RANGES overrides").count(), 1, "{out}");
}

#[test]
fn the_home_directory_error_names_the_way_out() {
    let s = Server::start();
    let mut p = Project::new(&s);
    p.cwd = Some(p.home().to_path_buf());
    let out = p.expect(&["install"], 1);
    assert!(out.contains("error: the home directory is the global scope, not a project"), "{out}");
    assert!(out.contains("  hint: manage personal skills with --global, e.g. skillcask --global install"), "{out}");
}

#[test]
fn ui_refuses_to_start_without_a_terminal() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_skillcask")).arg("ui").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("skillcask ui needs a terminal"), "{out:?}");
}
