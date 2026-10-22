//! Agent presets in `[agents] targets` and the targets of `config.toml` (D29).
mod common;

use common::*;

fn config(p: &Project, text: &str) {
    let dir = p.home().join(".config/skillcask");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

/// A manifest without `[agents]`, so the targets come from `config.toml` or the default.
fn manifest(p: &Project, s: &Server, agents: &str) {
    p.write(
        "skills.toml",
        &format!("[repositories]\nfix = {{ url = \"{}\" }}\n{agents}\n[policy]\npackagist = false\n", s.url("/packages.json")),
    );
}

#[test]
fn config_targets_apply_and_dropped_targets_lose_their_links() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    config(&p, "[agents]\ntargets = [\"claude\", \"kiro\", \"cursor\"]\n");
    manifest(&p, &s, "");
    p.write(".skills/mine/SKILL.md", &skill_md("mine"));
    p.ok(&["add", "acme/one"]);
    for l in [".claude/skills/one", ".kiro/skills/one", ".kiro/skills/mine"] {
        assert!(!p.read(&format!("{l}/SKILL.md")).is_empty(), "{l}");
    }
    assert!(!p.exists(".junie") && !p.exists(".cursor"), "cursor reads .agents/skills");
    let st = p.ok(&["status"]);
    assert!(st.contains("agents.targets = [\"claude\", \"kiro\", \"cursor\"] (config.toml): .claude/skills, .kiro/skills"), "{st}");

    p.write(".kiro/skills/theirs/SKILL.md", "not ours");
    config(&p, "[agents]\ntargets = [\"claude\"]\n");
    let out = p.expect(&["doctor"], 1);
    assert!(out.contains("links in directories that are no longer targets") && out.contains(".kiro/skills/one"), "{out}");
    p.ok(&["install"]);
    assert!(!p.exists(".kiro/skills/one") && !p.exists(".kiro/skills/mine"), "managed and committed links both go");
    assert_eq!(p.read(".kiro/skills/theirs/SKILL.md"), "not ours");
    assert!(p.exists(".claude/skills/one") && p.exists(".claude/skills/mine"));
    assert!(!p.read(".gitignore").contains(".kiro"));
    p.ok(&["doctor"]);

    manifest(&p, &s, "[skills]\n\"acme/one\" = \"^1.0\"\n[agents]\ntargets = [\"copilot\"]\n");
    p.ok(&["install"]);
    assert!(!p.exists(".claude/skills/one") && !p.exists(".claude/skills/mine"), "the manifest replaces the config list");
    assert!(p.exists(".agents/skills/one/SKILL.md"));
    assert!(p.ok(&["status"]).contains("agents.targets = [\"copilot\"] (skills.toml): .agents/skills only"));
}

#[test]
fn presets_resolve_per_scope_and_unknown_names_fail() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.global_manifest(&s, "");
    let cfg = p.home().join(".config/skillcask/skills.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, text.replace("targets = [\".claude/skills\", \".junie/skills\"]", "targets = [\"junie\"]")).unwrap();
    p.global(&["add", "acme/one"], 0);
    assert!(p.home().join(".junie/skills/one/SKILL.md").is_file(), "Junie reads only ~/.junie/skills in home");
    assert!(!p.home().join(".claude").exists());

    config(&p, "[agents]\ntargets = [\"cusor\"]\n");
    manifest(&p, &s, "");
    let e = p.expect(&["install"], 1);
    assert!(e.contains("config.toml: agents.targets: unknown agent \"cusor\""), "{e}");
}

#[test]
fn init_leaves_targets_to_the_layers() {
    let s = Server::start();
    let p = Project::new(&s);
    p.ok(&["init"]);
    assert!(p.read("skills.toml").contains("# targets = [\"claude\"]"));
    config(&p, "[agents]\ntargets = [\"kiro\"]\n");
    assert!(p.ok(&["status"]).contains("agents.targets = [\"kiro\"] (config.toml): .kiro/skills"));
}
