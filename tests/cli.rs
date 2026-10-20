mod common;

use common::*;
use serde_json::json;

fn store_hash(p: &Project, name: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let root = p.path().join(".agents/skills").join(name);
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push((e.path().strip_prefix(&root).unwrap().display().to_string(), std::fs::read(e.path()).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn add_creates_everything_and_frozen_clone_reproduces_it() {
    let s = Server::start();
    s.simple("acme/one", "v1.2.3", "one");
    let p = Project::new(&s);
    p.write(
        "skills.toml",
        &format!("[repositories]\nfix = {{ type = \"composer\", url = \"{}\" }}\n[policy]\npackagist = false\n", s.url("/packages.json")),
    );
    p.ok(&["add", "acme/one"]);

    assert!(p.read("skills.toml").contains("\"acme/one\" = \"^1.2\""));
    let lock = p.read("skills.lock");
    assert!(lock.contains("version = \"v1.2.3\"") && lock.contains("tree-sha256") && lock.contains("reference = \"ref0\""), "{lock}");
    assert_eq!(p.read(".agents/skills/one/SKILL.md"), skill_md("one"));
    assert!(!p.exists(".junie"), "the default targets are [\"claude\"] (D29)");
    let t = ".claude/skills/one";
    let meta = std::fs::symlink_metadata(p.path().join(t)).unwrap();
    if cfg!(unix) {
        assert!(meta.file_type().is_symlink(), "{t}");
        assert_eq!(std::fs::read_link(p.path().join(t)).unwrap(), std::path::Path::new("../../.agents/skills/one"));
    }
    assert_eq!(p.read(&format!("{t}/SKILL.md")), skill_md("one"));
    let gi = p.read(".gitignore");
    assert!(gi.contains("/.agents/skills/one/") && gi.contains("/.claude/skills/one"), "{gi}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(p.path().join(".agents/skills/one/scripts/run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    p.ok(&["status"]);

    let c = clone_of(&p, &s);
    let mut env = c;
    env.env.push(("CI".into(), "true".into()));
    env.ok(&["install"]);
    assert_eq!(store_hash(&env, "one"), store_hash(&p, "one"));
    env.ok(&["status"]);
    assert_eq!(env.read("skills.lock"), p.read("skills.lock"));

    let again = p.ok(&["install"]);
    assert!(!again.contains("installed"), "already-installed skills are skipped: {again}");
}

#[test]
fn modified_skill_fails_status_and_tampered_zip_fails_install() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    p.ok(&["status"]);

    p.write(".agents/skills/one/SKILL.md", &(skill_md("one") + "x"));
    let out = p.expect(&["status"], 1);
    assert!(out.contains("modified"), "{out}");
    p.expect(&["doctor"], 1);
    p.ok(&["install"]);
    p.ok(&["status"]);

    s.put("/dist/acme/one/1.0.0.zip", zipball(&[("SKILL.md", &skill_md("evil"))]));
    let c = clone_of(&p, &s);
    let out = c.expect(&["install", "--frozen"], 4);
    assert!(out.contains("sha256"), "{out}");
    assert!(!c.exists(".agents/skills/one"));
}

#[test]
fn ci_refuses_unlocked_entries_without_writing() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.simple("acme/two", "1.0.0", "two");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    let lock = p.read("skills.lock");
    let gi = p.read(".gitignore");
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n\"acme/two\" = \"^1.0\"\n");

    let mut ci = clone_of(&p, &s);
    ci.env.push(("CI".into(), "true".into()));
    let out = ci.expect(&["install"], 1);
    assert!(out.contains("acme/two: not in skills.lock"), "{out}");
    assert!(!ci.exists(".agents"));

    let out = p.expect(&["install", "--frozen"], 1);
    assert!(out.contains("out of date"), "{out}");
    assert_eq!(p.read("skills.lock"), lock);
    assert_eq!(p.read(".gitignore"), gi);
    assert!(!p.exists(".agents/skills/two"));

    p.manifest(&s, "\"acme/one\" = \"~1.0.0\"\n");
    assert!(p.expect(&["install", "--frozen"], 1).contains("constraint changed"));
}

#[test]
fn cooldown_skips_fresh_versions_and_names_eligible_date() {
    let s = Server::start();
    let z = || zipball(&[("SKILL.md", &skill_md("cool"))]);
    s.package(
        "acme/cool",
        vec![
            Ver { version: "1.1.0", time: Some(ago(1)), zip: z(), extra: None },
            Ver { version: "1.0.0", time: Some(ago(240)), zip: z(), extra: None },
        ],
    );
    s.package("acme/fresh", vec![Ver { version: "2.0.0", time: Some(ago(1)), zip: z(), extra: None }]);
    let p = Project::new(&s);
    p.manifest(&s, "");
    p.ok(&["add", "acme/cool"]);
    assert!(p.read("skills.lock").contains("version = \"1.0.0\""));
    let out = p.ok(&["outdated"]);
    assert!(out.contains("1.1.0 eligible"), "{out}");

    let before = p.read("skills.toml");
    let out = p.expect(&["add", "acme/fresh"], 1);
    assert!(out.contains("becomes eligible at") && out.contains("2.0.0"), "{out}");
    assert_eq!(p.read("skills.toml"), before);

    p.write("skills.toml", &before.replace("packagist = false", "packagist = false\nmin-release-age = \"0\""));
    p.ok(&["update"]);
    assert!(p.read("skills.lock").contains("version = \"1.1.0\""));
}

#[test]
fn version_without_time_is_kept_with_warning() {
    let s = Server::start();
    s.package("acme/notime", vec![Ver { version: "1.0.0", time: None, zip: zipball(&[("SKILL.md", &skill_md("notime"))]), extra: None }]);
    let p = Project::new(&s);
    p.manifest(&s, "");
    let out = p.ok(&["add", "acme/notime@1.0.0"]);
    assert!(out.contains("no release time"), "{out}");
    assert!(p.read("skills.toml").contains("\"acme/notime\" = \"1.0.0\""));
}

#[test]
fn typo_leaves_files_untouched() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "");
    let before = p.read("skills.toml");
    assert!(p.expect(&["add", "acme/onee"], 1).contains("not found"));
    assert!(p.expect(&["add", "acme/one@^9.0"], 1).contains("no version"));
    assert_eq!(p.read("skills.toml"), before);
    assert!(!p.exists("skills.lock"));
}

#[test]
fn http_basic_and_bearer_auth() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.require_auth(Some("Basic Z2l0bGFiLWNpLXRva2VuOmpvYnRva2Vu"));
    let mut p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    let out = p.expect(&["install", "--verbose"], 2);
    assert!(out.contains("401"), "{out}");

    let host = s.base.trim_start_matches("https://").to_string();
    p.env.push((
        "COMPOSER_AUTH".into(),
        json!({ "http-basic": { host.clone(): { "username": "gitlab-ci-token", "password": "jobtoken" } } }).to_string(),
    ));
    let seen = s.log().len();
    let out = p.ok(&["install", "--verbose"]);
    assert!(!out.contains("jobtoken") && out.contains("GET "), "token printed or no request log: {out}");
    assert!(s.log()[seen..].iter().all(|l| l.ends_with("auth=true")));

    s.require_auth(Some("Bearer t0ken"));
    let c = clone_of(&p, &s);
    c.write("auth.json", &json!({ "bearer": { host: "t0ken" } }).to_string());
    c.ok(&["install"]);
}

fn evil(entries: &[(&str, Option<u32>)]) -> Vec<u8> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, mode) in entries {
        let mut o = zip::write::SimpleFileOptions::default().system(zip::System::Unix);
        if let Some(m) = mode {
            o = o.unix_permissions(*m);
        }
        if mode.is_some_and(|m| m & 0o170000 == 0o120000) {
            w.add_symlink(*name, "/etc/passwd", o).unwrap();
        } else {
            w.start_file(*name, o).unwrap();
            w.write_all(b"---\nname: evil\n---\n").unwrap();
        }
    }
    w.finish().unwrap().into_inner()
}

#[test]
fn archive_attacks_are_rejected() {
    let s = Server::start();
    let cases = [
        ("acme/slip", evil(&[("SKILL.md", None), ("../../escape.txt", None)])),
        ("acme/link", evil(&[("SKILL.md", None), ("l", Some(0o120777))])),
        ("acme/abs", evil(&[("SKILL.md", None), ("/tmp/abs.txt", None)])),
    ];
    for (name, zip) in cases {
        s.package(name, vec![Ver { version: "1.0.0", time: Some(ago(240)), zip, extra: None }]);
    }
    s.simple("acme/big", "1.0.0", "big");
    let p = Project::new(&s);
    p.manifest(&s, "");
    for name in ["acme/slip", "acme/link", "acme/abs"] {
        let out = p.expect(&["add", name], 5);
        assert!(out.contains("archive"), "{name}: {out}");
    }
    assert!(!p.path().parent().unwrap().join("escape.txt").exists());
    assert!(p.expect(&["add", "acme/big", "--max-unpacked", "10"], 5).contains("archive"));
    assert!(p.expect(&["add", "acme/big", "--max-files", "1"], 5).contains("archive"));
    assert!(p.expect(&["add", "acme/big", "--max-download", "10"], 5).contains("limit"));
    assert!(!p.exists("skills.lock"));
    p.ok(&["add", "acme/big"]);
}

#[test]
fn collisions_are_errors() {
    let s = Server::start();
    s.simple("acme/a", "1.0.0", "same");
    s.simple("other/b", "1.0.0", "same");
    s.simple("acme/c", "1.0.0", "mine");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/a\" = \"^1.0\"\n\"other/b\" = \"^1.0\"\n");
    let out = p.expect(&["install"], 1);
    assert!(out.contains("acme/a") && out.contains("other/b"), "{out}");
    assert!(
        out.contains("to install it under another name, set in its entry in skills.toml:\n  rename = { same = \"other-same\" }"),
        "{out}"
    );

    p.manifest(&s, "");
    p.write(".agents/skills/mine/SKILL.md", &skill_md("mine-by-hand"));
    let before = p.read("skills.toml");
    let out = p.expect(&["add", "acme/c"], 1);
    assert!(out.contains("not managed by skillcask") && out.contains("rename = { mine = \"acme-mine\" }"), "{out}");
    assert_eq!(p.read(".agents/skills/mine/SKILL.md"), skill_md("mine-by-hand"));
    // A failed add used to leave acme/c in skills.toml and skills.lock, breaking every later install.
    assert_eq!(p.read("skills.toml"), before);
    assert!(!p.exists("skills.lock"));

    std::fs::remove_dir_all(p.path().join(".agents/skills/mine")).unwrap();
    p.write(".claude/skills/mine/SKILL.md", &skill_md("mine-by-hand"));
    let out = p.expect(&["add", "acme/c"], 1);
    assert!(out.contains(".claude/skills/mine exists"), "{out}");
    assert_eq!(p.read("skills.toml"), before);
    assert!(!p.exists("skills.lock"));
}

const BLOCK_BEGIN: &str = "# >>> skillcask managed block, regenerated by skillcask";

#[cfg(unix)]
#[test]
fn symlinked_project_dirs_are_refused() {
    let s = Server::start();
    let p = Project::new(&s);
    p.manifest(&s, "");
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join("gh")).unwrap();
    std::fs::write(outside.path().join("gh/SKILL.md"), "keep").unwrap();
    std::fs::create_dir_all(p.path().join(".agents")).unwrap();
    std::os::unix::fs::symlink(outside.path(), p.path().join(".agents/skills")).unwrap();
    p.write(".gitignore", &format!("{BLOCK_BEGIN}\n/.agents/skills/gh/\n# <<< skillcask managed block\n"));
    let out = p.expect(&["install"], 1);
    assert!(out.contains(".agents/skills is a symlink"), "{out}");
    assert!(outside.path().join("gh/SKILL.md").exists(), "install deleted a directory outside the project");
    p.expect(&["doctor", "--fix"], 1);

    std::fs::remove_file(p.path().join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink(outside.path(), p.path().join(".claude")).unwrap();
    assert!(p.expect(&["install"], 1).contains(".claude is a symlink"));
}

#[test]
fn gitignore_block_without_end_is_left_alone() {
    let s = Server::start();
    let p = Project::new(&s);
    p.manifest(&s, "");
    let gi = format!("/vendor\n{BLOCK_BEGIN}\n/.agents/skills/x/\n/my-own-line\n");
    p.write(".gitignore", &gi);
    assert!(p.expect(&["install"], 1).contains("no end line"));
    assert_eq!(p.read(".gitignore"), gi);
}

#[test]
fn hand_written_skills_are_linked_but_not_ignored() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.write(".agents/skills/local/SKILL.md", &skill_md("local"));
    p.ok(&["install"]);
    assert_eq!(p.read(".claude/skills/local/SKILL.md"), skill_md("local"));
    assert!(!p.read(".gitignore").contains("local"));
    let out = p.ok(&["status"]);
    assert!(out.contains("hand-written"), "{out}");
}

#[test]
fn remove_cleans_store_links_and_block() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.simple("acme/two", "1.0.0", "two");
    let p = Project::new(&s);
    p.write(".gitignore", "/vendor\n");
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n\"acme/two\" = \"^1.0\"\n");
    p.ok(&["install"]);
    p.ok(&["remove", "acme/one"]);
    assert!(!p.exists(".agents/skills/one") && !p.exists(".claude/skills/one") && !p.exists(".junie/skills/one"));
    assert!(p.exists(".agents/skills/two"));
    assert!(!p.read("skills.toml").contains("acme/one") && !p.read("skills.lock").contains("acme/one"));
    let gi = p.read(".gitignore");
    assert!(gi.starts_with("/vendor\n") && !gi.contains("/one") && gi.contains("/two"), "{gi}");
    p.ok(&["remove", "acme/two"]);
    assert_eq!(p.read(".gitignore"), "/vendor\n");
    p.expect(&["remove", "acme/nope"], 1);
}

#[test]
fn multi_skill_package_with_selection() {
    let s = Server::start();
    let zip = zipball(&[
        ("composer.json", "{}"),
        ("skills/alpha/SKILL.md", &skill_md("alpha")),
        ("skills/beta/SKILL.md", &skill_md("beta")),
        ("skills/beta/ref/x.md", "x"),
    ]);
    s.package(
        "acme/multi",
        vec![Ver { version: "1.0.0", time: Some(ago(240)), zip, extra: Some(json!(["skills/alpha/SKILL.md", "skills/beta/SKILL.md"])) }],
    );
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"] }\n");
    p.ok(&["install"]);
    assert!(p.exists(".agents/skills/beta/ref/x.md") && !p.exists(".agents/skills/alpha"));
    assert!(!p.exists(".agents/skills/beta/composer.json"));
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"alpha\", \"beta\"] }\n");
    p.ok(&["install"]);
    assert!(p.exists(".agents/skills/alpha/SKILL.md"));
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"gamma\"] }\n");
    assert!(p.expect(&["install"], 1).contains("gamma"));
}

#[test]
fn doctor_repairs_links_index_and_block() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.ok(&["install"]);
    p.ok(&["doctor"]);

    std::fs::remove_file(p.path().join(".claude/skills/one"))
        .or_else(|_| std::fs::remove_dir_all(p.path().join(".claude/skills/one")))
        .unwrap();
    p.write(".gitignore", "");
    git(p.path(), &["add", "-f", ".junie/skills/one"]);
    let out = p.expect(&["doctor"], 1);
    assert!(out.contains(".claude/skills/one") && out.contains("committed") && out.contains(".gitignore"), "{out}");
    p.ok(&["doctor", "--fix"]);
    p.ok(&["doctor"]);
    assert!(p.exists(".claude/skills/one/SKILL.md"));
    assert!(git(p.path(), &["ls-files"]).is_empty());
    assert!(p.exists(".junie/skills/one"), "git rm --cached keeps the working tree");
}

#[test]
fn copy_mode_when_symlinks_are_unavailable() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let mut p = Project::new(&s);
    p.env.push(("SKILLCASK_LINK_MODE".into(), "copy".into()));
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.write(".agents/skills/local/SKILL.md", &skill_md("local"));
    p.write(".skills/proj/SKILL.md", &skill_md("proj"));
    p.ok(&["install"]);
    assert_eq!(p.read(".claude/skills/proj/SKILL.md"), skill_md("proj"));
    assert!(p.read(".gitignore").contains("/.agents/skills/proj"), "store copy of a project skill is ignored");
    let meta = std::fs::symlink_metadata(p.path().join(".claude/skills/one")).unwrap();
    assert!(meta.is_dir() && !meta.file_type().is_symlink());
    assert_eq!(p.read(".claude/skills/one/SKILL.md"), skill_md("one"));
    assert!(p.read(".gitignore").contains("/.claude/skills/local"), "copies of hand-written skills are ignored");
    p.ok(&["doctor"]);
    p.ok(&["status"]);
    p.ok(&["remove", "acme/one"]);
    assert!(!p.exists(".claude/skills/one"));
    std::fs::remove_dir_all(p.path().join(".skills/proj")).unwrap();
    p.ok(&["install"]);
    assert!(!p.exists(".agents/skills/proj") && !p.exists(".claude/skills/proj"));
}

#[test]
fn publish_then_install_round_trip() {
    let s = Server::start();
    let mono = tempfile::tempdir().unwrap();
    for (n, v) in [("alpha", "1.0.0"), ("beta", "2.1.0"), ("gamma", "0.3.0")] {
        let d = mono.path().join("skills").join(n);
        std::fs::create_dir_all(d.join("scripts")).unwrap();
        std::fs::write(d.join("SKILL.md"), format!("---\nname: {n}\nversion: {v}\ndescription: {n} skill\n---\nbody\n")).unwrap();
        std::fs::write(d.join("scripts/x.sh"), "echo\n").unwrap();
    }
    std::fs::write(mono.path().join("skills/README.md"), "no frontmatter").unwrap();
    let out = mono.path().join("public");
    let publish = |p: &Project| {
        p.ok(&[
            "publish",
            "--vendor",
            "acme",
            "--base-url",
            &s.base,
            "--out",
            out.to_str().unwrap(),
            mono.path().join("skills").to_str().unwrap(),
        ])
    };
    let p = Project::new(&s);
    let text = publish(&p);
    assert!(text.contains("published acme/beta 2.1.0"), "{text}");
    s.serve_dir(&out);

    let files = ["packages.json", "p2/acme/alpha.json", "p2/acme/beta.json", "dist/acme/gamma/0.3.0.zip"];
    let snapshot = || {
        let mut v = Vec::new();
        for f in files {
            let m = std::fs::metadata(out.join(f)).unwrap();
            v.push((f, m.modified().unwrap(), std::fs::read(out.join(f)).unwrap()));
        }
        v
    };
    for f in files {
        backdate(&out.join(f));
    }
    let first = snapshot();
    assert!(publish(&p).contains("unchanged acme/alpha 1.0.0"));
    assert_eq!(snapshot(), first, "second publish of unchanged skills changes no file");

    p.write(
        "skills.toml",
        &format!(
            "[repositories]\nmono = {{ type = \"composer\", url = \"{}\" }}\n[skills]\n\"acme/alpha\" = \"^1.0\"\n\"acme/beta\" = \"^2.0\"\n\"acme/gamma\" = \"^0.3\"\n[policy]\npackagist = false\nmin-release-age = \"0\"\n",
            s.url("/packages.json")
        ),
    );
    p.ok(&["install"]);
    for n in ["alpha", "beta", "gamma"] {
        assert!(p.exists(&format!(".agents/skills/{n}/scripts/x.sh")));
    }
    p.ok(&["status"]);
    let found = p.ok(&["search", "bet"]);
    assert!(found.contains("acme/beta") && !found.contains("acme/alpha"), "{found}");

    std::fs::write(mono.path().join("skills/alpha/SKILL.md"), "---\nname: alpha\nversion: 1.0.0\n---\nchanged\n").unwrap();
    let o =
        p.expect(&["publish", "--vendor", "acme", "--base-url", &s.base, "--out", out.to_str().unwrap(), mono.path().to_str().unwrap()], 1);
    assert!(o.contains("different content"), "{o}");
}

#[cfg(unix)]
#[test]
fn unknown_commands_run_plugins_from_path() {
    use std::os::unix::fs::PermissionsExt;
    let s = Server::start();
    let mut p = Project::new(&s);
    let bin = tempfile::tempdir().unwrap();
    let plugin = bin.path().join("skillcask-hello");
    std::fs::write(&plugin, "#!/bin/sh\necho \"hello $*\"\nexit 7\n").unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());
    p.env.push(("PATH".into(), path));
    assert!(p.expect(&["hello", "a", "--b"], 7).contains("hello a --b"));
    assert!(p.expect(&["nope"], 1).contains("no skillcask-nope on PATH"));
    std::fs::create_dir_all(p.path().join("skillcask-x")).unwrap();
    std::fs::copy(&plugin, p.path().join("skillcask-x/run")).unwrap();
    let out = p.expect(&["x/run"], 1);
    assert!(out.contains("unknown command") && !out.contains("hello"), "a path in the name ran a file from the cwd: {out}");
}

#[test]
fn version_prints_commit() {
    let s = Server::start();
    let p = Project::new(&s);
    let out = p.ok(&["--version"]);
    assert!(out.starts_with("skillcask 0.") && out.contains('('), "{out}");
    p.ok(&["--help"]);
    // clap's own usage-error code 2 means authentication here.
    p.expect(&["--bogus"], 1);
    p.ok(&["init"]);
    p.expect(&["init"], 1);
    p.ok(&["install"]);
}

#[test]
#[ignore = "live Packagist: cargo test --test cli -- --ignored"]
fn packagist_netresearch_jira() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_skillcask");
    let run = |args: &[&str], d: &std::path::Path| {
        let o = std::process::Command::new(bin).args(args).current_dir(d).env_remove("CI").output().unwrap();
        assert!(o.status.success(), "{args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    };
    git(dir.path(), &["init", "-q"]);
    run(&["add", "netresearch/agent-jira-skill"], dir.path());
    assert!(dir.path().join(".claude/skills").read_dir().unwrap().count() > 0);
    let clone = tempfile::tempdir().unwrap();
    for f in ["skills.toml", "skills.lock"] {
        std::fs::copy(dir.path().join(f), clone.path().join(f)).unwrap();
    }
    run(&["install", "--frozen"], clone.path());
    run(&["status"], clone.path());
}

#[test]
fn project_skills_in_dot_skills_are_linked_and_committable() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.write(".skills/mine/SKILL.md", &skill_md("mine"));
    p.ok(&["install"]);
    if cfg!(unix) {
        assert_eq!(std::fs::read_link(p.path().join(".agents/skills/mine")).unwrap(), std::path::Path::new("../../.skills/mine"));
    }
    assert_eq!(p.read(".claude/skills/mine/SKILL.md"), skill_md("mine"));
    assert_eq!(p.read(".junie/skills/mine/SKILL.md"), skill_md("mine"));
    assert!(!p.read(".gitignore").contains("mine"), "project skill links are committed");
    assert!(p.ok(&["status"]).contains("project"));
    p.ok(&["doctor"]);

    std::fs::remove_dir_all(p.path().join(".skills/mine")).unwrap();
    p.ok(&["install"]);
    assert!(!p.exists(".agents/skills/mine") && !p.exists(".claude/skills/mine"));
    assert!(p.exists(".agents/skills/one/SKILL.md"));

    p.write(".skills/one/SKILL.md", &skill_md("one"));
    let out = p.expect(&["install"], 1);
    assert!(out.contains("both a project skill") && out.contains("rename = { one = \"acme-one\" }"), "{out}");
}

#[test]
fn github_branch_respects_cooldown_and_selects_skills() {
    let s = Server::start();
    let (old, new) = ("a".repeat(40), "b".repeat(40));
    let zip = |v: &str| {
        zipball(&[
            ("README.md", "repo"),
            ("skills/one/SKILL.md", &skill_md("one")),
            ("skills/two/SKILL.md", &format!("---\nname: Two Display Name\n---\n{v}")),
            ("skills/two/ref/x.md", "x"),
        ])
    };
    s.put("/repos/acme/tools/tags", "[]");
    s.put("/repos/acme/tools/commits/HEAD", gh_commit(&new, 1).to_string());
    s.put("/repos/acme/tools/commits", json!([gh_commit(&old, 240)]).to_string());
    s.put(&format!("/repos/acme/tools/zipball/{old}"), zip("old"));
    s.put(&format!("/repos/acme/tools/zipball/{new}"), zip("new"));
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "");

    let out = p.ok(&["add", "github:Acme/tools/two"]);
    assert!(out.contains("newer commits on \"HEAD\""), "{out}");
    assert!(p.read("skills.toml").contains(r#""acme/tools" = { github = true, skills = ["two"] }"#), "{}", p.read("skills.toml"));
    let lock = p.read("skills.lock");
    assert!(lock.contains(&format!("zipball/{old}")) && lock.contains(&format!("version = \"{}\"", &old[..12])), "{lock}");
    assert!(p.read(".agents/skills/two/SKILL.md").ends_with("old"), "display name falls back to the directory name");
    assert!(p.exists(".agents/skills/two/ref/x.md") && !p.exists(".agents/skills/one") && !p.exists(".agents/skills/README.md"));

    p.ok(&["add", "github:acme/tools/one"]);
    assert!(p.read("skills.toml").contains(r#"skills = ["two", "one"]"#));
    assert!(p.exists(".claude/skills/one/SKILL.md"));
    p.ok(&["status"]);

    let mut ci = with_github(clone_of(&p, &s), &s);
    ci.env.push(("CI".into(), "true".into()));
    ci.ok(&["install"]);
    assert_eq!(store_hash(&ci, "two"), store_hash(&p, "two"));

    p.write("skills.toml", &p.read("skills.toml").replace("github = true,", "github = true, ref = \"main\","));
    assert!(p.expect(&["install", "--frozen"], 1).contains("constraint changed"));
    s.put("/repos/acme/tools/commits/main", gh_commit(&new, 1).to_string());
    p.write("skills.toml", &p.read("skills.toml").replace("packagist = false", "packagist = false\nmin-release-age = \"0\""));
    p.ok(&["install"]);
    assert!(p.read("skills.lock").contains(&format!("zipball/{new}")));
    assert!(p.read(".agents/skills/two/SKILL.md").ends_with("new"));
    assert!(p.ok(&["outdated"]).contains(&new[..12]));

    p.write("skills.toml", &p.read("skills.toml").replace("min-release-age = \"0\"", "allow-ranges = false"));
    assert!(p.expect(&["install"], 1).contains("full commit sha"));
}

#[test]
fn github_tags_are_versions() {
    let s = Server::start();
    let (c1, c2) = ("1".repeat(40), "2".repeat(40));
    s.put(
        "/repos/acme/tagged/tags",
        json!([
            { "name": "v2.0.0", "commit": { "sha": c2 } },
            { "name": "latest", "commit": { "sha": c2 } },
            { "name": "v1.1.0", "commit": { "sha": c1 } },
        ])
        .to_string(),
    );
    s.put(&format!("/repos/acme/tagged/commits/{c2}"), gh_commit(&c2, 1).to_string());
    s.put(&format!("/repos/acme/tagged/commits/{c1}"), gh_commit(&c1, 240).to_string());
    s.put(&format!("/repos/acme/tagged/zipball/{c1}"), zipball(&[("SKILL.md", &skill_md("tagged"))]));
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "");
    p.ok(&["add", "github:acme/tagged"]);
    assert!(p.read("skills.toml").contains(r#""acme/tagged" = { github = true, version = "^1.1" }"#), "{}", p.read("skills.toml"));
    assert!(p.read("skills.lock").contains("version = \"v1.1.0\""));
    assert_eq!(p.read(".agents/skills/tagged/SKILL.md"), skill_md("tagged"));
    let out = p.ok(&["outdated"]);
    assert!(out.contains("v1.1.0") && out.contains("v2.0.0"), "{out}");
    assert!(p.expect(&["add", "github:acme/tagged@^2.0"], 1).contains("becomes eligible at"));

    s.require_auth(Some("Bearer ght0ken"));
    let mut c = with_github(clone_of(&p, &s), &s);
    c.expect(&["install"], 2);
    c.env.push(("GITHUB_TOKEN".into(), "ght0ken".into()));
    c.ok(&["install"]);
}

#[test]
fn inline_repository_behind_raw_files_api() {
    let s = Server::start();
    let mono = tempfile::tempdir().unwrap();
    for (n, v) in [("alpha", "1.0.0"), ("beta", "2.1.0")] {
        let d = mono.path().join("skills").join(n);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), format!("---\nname: {n}\nversion: {v}\ndescription: {n} skill\n---\nbody\n")).unwrap();
    }
    let out = mono.path().join("composer");
    let p = Project::new(&s);
    let tpl = s.url("/generic/{vendor}-{name}/{version}/{name}-{version}.zip");
    let text = p.ok(&[
        "publish",
        "--inline",
        "--vendor",
        "acme",
        "--dist-url",
        &tpl,
        "--out",
        out.to_str().unwrap(),
        mono.path().to_str().unwrap(),
    ]);
    assert!(text.contains("published acme/alpha 1.0.0") && text.contains("published acme/beta 2.1.0"), "{text}");
    assert!(!out.join("p2").exists());

    // Paths as GitLab serves them: the raw files API with an encoded slash, flat generic packages.
    let raw = "/api/v4/projects/7/repository/files/composer%2Fpackages.json/raw";
    s.put(raw, std::fs::read(out.join("packages.json")).unwrap());
    for (n, v) in [("alpha", "1.0.0"), ("beta", "2.1.0")] {
        s.put(&format!("/generic/acme-{n}/{v}/{n}-{v}.zip"), std::fs::read(out.join(format!("dist/acme/{n}/{v}.zip"))).unwrap());
    }
    s.require_auth(Some("Bearer t0ken"));
    let host = s.base.trim_start_matches("https://").to_string();
    p.write("auth.json", &json!({ "bearer": { host: "t0ken" } }).to_string());
    p.write(
        "skills.toml",
        &format!(
            "[repositories]\nmono = {{ type = \"composer\", url = \"{}\" }}\n[skills]\n\"acme/alpha\" = \"^1.0\"\n\"acme/beta\" = \"^2.0\"\n[policy]\npackagist = false\nmin-release-age = \"0\"\n",
            s.url(&format!("{raw}?ref=main"))
        ),
    );
    p.ok(&["install"]);
    assert!(p.exists(".agents/skills/alpha/SKILL.md") && p.exists(".agents/skills/beta/SKILL.md"));
    assert!(s.log().iter().any(|l| l.starts_with(&format!("{raw}?ref=main auth=true"))), "{:?}", s.log());
    let found = p.ok(&["search", "bet"]);
    assert!(found.contains("acme/beta") && !found.contains("acme/alpha"), "{found}");
}

#[test]
fn global_add_install_remove() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.global_manifest(&s, "");
    let h = p.home().to_path_buf();
    std::fs::create_dir_all(h.join(".agents/skills/mine")).unwrap();
    std::fs::write(h.join(".agents/skills/mine/SKILL.md"), skill_md("mine")).unwrap();

    p.global(&["add", "acme/one"], 0);
    let cfg = h.join(".config/skillcask");
    assert!(std::fs::read_to_string(cfg.join("skills.toml")).unwrap().contains("\"acme/one\""));
    assert!(cfg.join("skills.lock").is_file());
    assert_eq!(std::fs::read_to_string(h.join(".agents/skills/one/SKILL.md")).unwrap(), skill_md("one"));
    if cfg!(unix) {
        assert_eq!(std::fs::read_link(h.join(".claude/skills/one")).unwrap(), h.join(".agents/skills/one"));
    }
    assert!(std::fs::read_to_string(h.join(".agents/skills/.skillcask-installed")).unwrap().contains("/.claude/skills/one"));
    assert!(!cfg.join("installed").exists());
    assert!(
        !h.join(".gitignore").exists() && !p.exists("skills.toml") && !p.exists(".agents"),
        "nothing written to the project or ~/.gitignore"
    );
    assert!(!h.join(".claude/skills/mine").exists(), "hand-written personal skills are not linked");

    let out = p.global(&["status"], 0);
    assert!(out.lines().any(|l| l.starts_with("acme/one") && l.contains(" one ") && l.trim_end().ends_with("ok")), "{out}");
    p.global(&["doctor"], 0);
    std::fs::remove_dir_all(h.join(".agents/skills/one")).unwrap();
    let mut ci = p;
    ci.env.push(("CI".into(), "true".into()));
    ci.global(&["install"], 0);
    assert!(h.join(".agents/skills/one/SKILL.md").is_file());

    ci.global(&["remove", "acme/one"], 0);
    assert!(!h.join(".agents/skills/one").exists() && !h.join(".claude/skills/one").exists());
    assert!(h.join(".agents/skills/mine/SKILL.md").is_file());
}

#[cfg(unix)]
#[test]
fn global_refuses_to_replace_foreign_symlinks() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.global_manifest(&s, "");
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("SKILL.md"), skill_md("one")).unwrap();
    let link = p.home().join(".claude/skills/one");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(other.path(), &link).unwrap();

    let out = p.global(&["add", "acme/one"], 1);
    assert!(out.contains("~/.claude/skills/one exists and was not created by skillcask"), "{out}");
    assert_eq!(std::fs::read_link(&link).unwrap(), other.path());

    std::fs::remove_file(&link).unwrap();
    p.global(&["add", "acme/one"], 0);
    std::fs::remove_file(p.home().join(".agents/skills/.skillcask-installed")).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(other.path(), &link).unwrap();
    let out = p.global(&["doctor"], 1);
    assert!(out.contains("~/.agents/skills/.skillcask-installed: stale managed block"), "{out}");
    assert!(out.contains("repairs links and ~/.agents/skills/.skillcask-installed"), "{out}");
    let out = p.global(&["doctor", "--fix"], 1);
    assert!(out.contains("~/.claude/skills/one exists and was not created by skillcask; move it away"), "{out}");
    assert_eq!(std::fs::read_link(&link).unwrap(), other.path());
}

#[cfg(unix)]
#[test]
fn global_follows_symlinked_dotfiles() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.global_manifest(&s, "");
    let dotfiles = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(dotfiles.path(), p.home().join(".claude")).unwrap();
    p.global(&["add", "acme/one"], 0);
    assert_eq!(std::fs::read_to_string(dotfiles.path().join("skills/one/SKILL.md")).unwrap(), skill_md("one"));
}

#[test]
fn global_flag_rules() {
    let s = Server::start();
    let p = Project::new(&s);
    p.global(&["init"], 0);
    let text = std::fs::read_to_string(p.home().join(".config/skillcask/skills.toml")).unwrap();
    assert!(!text.contains("local"), "{text}");
    p.global(&["init"], 1);
    std::fs::write(p.home().join(".config/skillcask/skills.toml"), "[agents]\nlocal = \".skills\"\n").unwrap();
    assert!(p.global(&["install"], 1).contains("agents.local"));
    assert!(p.global(&["publish", "--vendor", "v", "--base-url", "https://x", "--out", "o"], 1).contains("--global"));
    assert!(p.global(&["some-plugin"], 1).contains("--global"));
    let mut custom = p;
    let dir = tempfile::tempdir().unwrap();
    custom.env.push(("SKILLCASK_HOME".into(), dir.path().display().to_string()));
    custom.global(&["init"], 0);
    assert!(dir.path().join("skills.toml").is_file());
}

#[test]
fn global_synced_config_does_not_claim_hand_written_skills() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let a = Project::new(&s);
    a.global_manifest(&s, "");
    a.global(&["add", "acme/one"], 0);

    let b = Project::new(&s);
    let (from, to) = (a.home().join(".config/skillcask"), b.home().join(".config/skillcask"));
    std::fs::create_dir_all(&to).unwrap();
    for e in std::fs::read_dir(&from).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
    let mine = b.home().join(".agents/skills/one");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::write(mine.join("SKILL.md"), "mine").unwrap();
    let out = b.global(&["install"], 1);
    assert!(out.contains("~/.agents/skills/one exists and was not installed by skillcask on this machine"), "{out}");
    assert_eq!(std::fs::read_to_string(mine.join("SKILL.md")).unwrap(), "mine");
}

#[cfg(unix)]
#[test]
fn global_target_dir_symlinked_to_the_store() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.global_manifest(&s, "");
    let h = p.home().to_path_buf();
    std::fs::create_dir_all(h.join(".agents/skills")).unwrap();
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    std::os::unix::fs::symlink("../.agents/skills", h.join(".claude/skills")).unwrap();

    p.global(&["add", "acme/one"], 0);
    assert_eq!(std::fs::read_to_string(h.join(".claude/skills/one/SKILL.md")).unwrap(), skill_md("one"));
    p.global(&["status"], 0);
    p.global(&["doctor"], 0);
    p.global(&["remove", "acme/one"], 0);
    assert!(!h.join(".agents/skills/one").exists());
    assert!(std::fs::symlink_metadata(h.join(".claude/skills")).unwrap().file_type().is_symlink());
}

#[test]
fn global_targets_may_be_named_like_the_project_local_dir() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.global_manifest(&s, "");
    let cfg = p.home().join(".config/skillcask/skills.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, text.replace("targets = [\".claude/skills\", \".junie/skills\"]", "targets = [\"./.skills\"]")).unwrap();
    p.global(&["add", "acme/one"], 0);
    assert!(p.home().join(".skills/one/SKILL.md").is_file());
    p.global(&["remove", "acme/one"], 0);
}

#[test]
fn global_init_names_the_config_path() {
    let s = Server::start();
    let p = Project::new(&s);
    let path = p.home().join(".config").join("skillcask").join("skills.toml");
    assert!(p.global(&["init"], 0).contains(&format!("created {}", path.display())));
    assert!(p.global(&["init"], 1).contains(&format!("{} already exists", path.display())));
    assert!(p.ok(&["init"]).contains("created skills.toml\n"));
    assert!(p.expect(&["init"], 1).contains("error: skills.toml already exists\n"));
}

#[test]
fn project_commands_do_not_treat_the_global_config_dir_as_a_project() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let mut p = Project::new(&s);
    p.global_manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    let cfg = p.home().join(".config/skillcask");
    p.cwd = Some(cfg.clone());
    let out = p.expect(&["install"], 1);
    assert!(out.contains("--global"), "{out}");
    assert!(!cfg.join(".agents").exists());
}

#[test]
fn global_reads_auth_json_from_the_config_dir() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.require_auth(Some("Bearer t0ken"));
    let p = Project::new(&s);
    p.global_manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.global(&["install"], 2);
    let host = s.base.trim_start_matches("https://").to_string();
    p.write("auth.json", &json!({ "bearer": { host.clone(): "wrong" } }).to_string());
    std::fs::write(p.home().join(".config/skillcask/auth.json"), json!({ "bearer": { host: "t0ken" } }).to_string()).unwrap();
    p.global(&["install"], 0);
}

#[test]
fn global_needs_a_home() {
    let s = Server::start();
    let mut p = Project::new(&s);
    p.env.push(("HOME".into(), String::new()));
    p.env.push(("USERPROFILE".into(), String::new()));
    assert!(p.global(&["status"], 1).contains("--global needs HOME"));
}

#[test]
fn the_environment_applies_without_a_home() {
    let s = Server::start();
    let mut p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.env.push(("HOME".into(), String::new()));
    p.env.push(("USERPROFILE".into(), String::new()));
    p.env.push(("SKILLCASK_ALLOW_RANGES".into(), "false".into()));
    let out = p.expect(&["status"], 1);
    assert!(out.contains("allow-ranges = false requires an exact version"), "{out}");
}

#[test]
fn global_uses_xdg_config_home() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let mut p = Project::new(&s);
    let xdg = tempfile::tempdir().unwrap();
    p.env.push(("XDG_CONFIG_HOME".into(), xdg.path().display().to_string()));
    p.global(&["init"], 0);
    let cfg = xdg.path().join("skillcask/skills.toml");
    std::fs::write(
        &cfg,
        format!("[repositories]\nfix = {{ type = \"composer\", url = \"{}\" }}\n[policy]\npackagist = false\n", s.url("/packages.json")),
    )
    .unwrap();
    p.global(&["add", "acme/one"], 0);
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("\"acme/one\""));
    assert!(xdg.path().join("skillcask/skills.lock").is_file());
    assert!(!p.home().join(".config").exists());
    assert!(p.home().join(".agents/skills/one/SKILL.md").is_file());
    p.global(&["status"], 0);
    p.global(&["remove", "acme/one"], 0);
    assert!(!p.home().join(".agents/skills/one").exists());
}

#[test]
fn personal_skills_shadowing_project_ones_are_warned_about() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    p.write(".skills/mine/SKILL.md", &skill_md("mine"));
    for d in [".claude/skills/one", ".agents/skills/mine"] {
        std::fs::create_dir_all(p.home().join(d)).unwrap();
        std::fs::write(p.home().join(d).join("SKILL.md"), "personal").unwrap();
    }
    let out = p.ok(&["install"]);
    assert!(out.contains("warning: one: ~/.claude/skills/one shadows the project skill in Claude Code"), "{out}");
    assert!(out.contains("warning: mine: ~/.agents/skills/mine also exists; the agent may show both"), "{out}");
    let out = p.ok(&["doctor"]);
    assert!(out.contains("  ! one: ~/.claude/skills/one shadows") && out.contains("no problems found"), "{out}");

    // Global installs land in the home dirs the check looks at; they must not warn about themselves.
    p.global_manifest(&s, "\"acme/one\" = \"^1.0\"\n");
    std::fs::remove_dir_all(p.home().join(".claude/skills/one")).unwrap();
    let out = p.global(&["install"], 0);
    assert!(p.home().join(".claude/skills/one").exists(), "{out}");
    assert!(!out.contains("warning"), "{out}");
}

#[test]
fn renamed_skill_installs_under_its_new_name() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/one\" = { version = \"^1.0\", rename = { one = \"acme-one\" } }\n");
    p.ok(&["install"]);
    assert_eq!(p.read(".agents/skills/acme-one/SKILL.md"), skill_md("one").replacen("name: one", "name: acme-one", 1));
    assert!(p.exists(".claude/skills/acme-one/SKILL.md") && !p.exists(".agents/skills/one"));
    assert!(p.read("skills.lock").contains("{ name = \"acme-one\", from = \"one\", path = \"\""), "{}", p.read("skills.lock"));
    p.ok(&["status"]);
    p.ok(&["doctor"]);

    let mut ci = clone_of(&p, &s);
    ci.env.push(("CI".into(), "true".into()));
    ci.ok(&["install"]);
    assert_eq!(ci.read(".agents/skills/acme-one/SKILL.md"), p.read(".agents/skills/acme-one/SKILL.md"));

    p.manifest(&s, "\"acme/one\" = { version = \"^1.0\", rename = { one = \"other\" } }\n");
    assert!(p.expect(&["install", "--frozen"], 1).contains("rename changed"));
    p.ok(&["install"]);
    assert!(p.exists(".agents/skills/other") && !p.exists(".agents/skills/acme-one"));

    p.manifest(&s, "\"acme/one\" = { version = \"^1.0\", rename = { nope = \"x\" } }\n");
    assert!(p.expect(&["install"], 1).contains("rename: no skill \"nope\""));
}

#[test]
fn renamed_nested_skill_leaves_the_parent_tree_alone() {
    let s = Server::start();
    let renamed = skill_md("sub").replacen("name: sub", "name: acme-sub", 1);
    for (pkg, paths, files) in [
        ("acme/nest", json!(["a", "a/sub"]), vec![("a/SKILL.md", skill_md("a")), ("a/sub/SKILL.md", skill_md("sub"))]),
        ("acme/flat", json!(["a/SKILL.md", "a/sub.md"]), vec![("a/SKILL.md", skill_md("a")), ("a/sub.md", skill_md("sub"))]),
    ] {
        let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
        s.package(pkg, vec![Ver { version: "1.0.0", time: Some(ago(240)), zip: zipball(&files), extra: Some(paths) }]);
        let p = Project::new(&s);
        p.manifest(&s, &format!("\"{pkg}\" = {{ version = \"^1.0\", rename = {{ sub = \"acme-sub\" }} }}\n"));
        p.ok(&["install"]);
        assert_eq!(p.read(".agents/skills/a/SKILL.md"), skill_md("a"), "{pkg}");
        assert!(p.exists(".agents/skills/acme-sub") && !p.exists(".agents/skills/sub"), "{pkg}");
        if pkg == "acme/nest" {
            assert_eq!(p.read(".agents/skills/a/sub/SKILL.md"), skill_md("sub"));
            assert_eq!(p.read(".agents/skills/acme-sub/SKILL.md"), renamed);
        }
        p.ok(&["status"]);

        let mut ci = clone_of(&p, &s);
        ci.env.push(("CI".into(), "true".into()));
        ci.ok(&["install"]);
        assert_eq!(store_hash(&ci, "a"), store_hash(&p, "a"), "{pkg}");
        assert_eq!(store_hash(&ci, "acme-sub"), store_hash(&p, "acme-sub"), "{pkg}");
        ci.ok(&["status"]);
    }
}

#[test]
fn rename_with_selection_and_a_tampered_lock() {
    let s = Server::start();
    let zip = zipball(&[("skills/alpha/SKILL.md", &skill_md("alpha")), ("skills/beta/SKILL.md", &skill_md("beta"))]);
    s.package("acme/multi", vec![Ver { version: "1.0.0", time: Some(ago(240)), zip, extra: Some(json!(["skills/alpha", "skills/beta"])) }]);
    let p = Project::new(&s);
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"], rename = { alpha = \"x\" } }\n");
    let out = p.expect(&["install"], 1);
    assert!(out.contains("acme/multi: rename: skill \"alpha\" is not in skills = [...]"), "{out}");

    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"], rename = { beta = \"acme-beta\" } }\n");
    p.ok(&["install"]);
    assert!(p.exists(".agents/skills/acme-beta") && !p.exists(".agents/skills/alpha"));
    let mut ci = clone_of(&p, &s);
    ci.env.push(("CI".into(), "true".into()));
    ci.ok(&["install", "--frozen"]);
    ci.ok(&["status"]);

    let mut ci = clone_of(&p, &s);
    ci.env.push(("CI".into(), "true".into()));
    ci.write("skills.lock", &p.read("skills.lock").replace("from = \"beta\", ", ""));
    assert!(ci.expect(&["install", "--frozen"], 1).contains("out of date"));
    ci.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"beta\"] }\n");
    let out = ci.expect(&["install", "--frozen"], 1);
    assert!(out.contains("skill selection changed"), "{out}");
    ci.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", skills = [\"acme-beta\"] }\n");
    let out = ci.expect(&["install", "--frozen"], 4);
    assert!(out.contains("tree-sha256"), "{out}");

    p.write(".skills/alpha/SKILL.md", &skill_md("alpha"));
    p.manifest(&s, "\"acme/multi\" = { version = \"^1.0\", rename = { beta = \"acme-beta\" } }\n");
    let out = p.expect(&["update"], 1);
    assert!(out.contains("set in its entry in skills.toml:\n  rename = { alpha = \"acme-alpha\", beta = \"acme-beta\" }"), "{out}");
}

#[test]
fn rename_in_global_scope_and_kept_by_add() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    let p = Project::new(&s);
    p.global_manifest(&s, "\"acme/one\" = { version = \"^1.0\", rename = { one = \"acme-one\" } }\n");
    p.global(&["install"], 0);
    assert!(p.home().join(".agents/skills/acme-one/SKILL.md").is_file());
    let out = p.global(&["status"], 0);
    assert!(out.lines().any(|l| l.contains(" acme-one ") && l.trim_end().ends_with("ok")), "{out}");

    p.manifest(&s, "\"acme/one\" = { version = \"^1.0\", rename = { one = \"acme-one\" } }\n");
    p.ok(&["add", "acme/one"]);
    assert!(p.read("skills.toml").contains("rename = { one = \"acme-one\" }"), "{}", p.read("skills.toml"));
    assert!(p.exists(".agents/skills/acme-one") && !p.exists(".agents/skills/one"));
}

#[test]
fn rename_of_a_skill_without_a_name_line() {
    let s = Server::start();
    let sha = "c".repeat(40);
    let md = "---\ndescription: no name\n---\n# plain\n";
    s.put("/repos/acme/tools/tags", "[]");
    s.put("/repos/acme/tools/commits/HEAD", gh_commit(&sha, 240).to_string());
    s.put(&format!("/repos/acme/tools/zipball/{sha}"), zipball(&[("skills/plain/SKILL.md", md)]));
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "\"acme/tools\" = { github = true, rename = { plain = \"acme-plain\" } }\n");
    p.ok(&["install"]);
    assert_eq!(p.read(".agents/skills/acme-plain/SKILL.md"), md);
    assert!(!p.exists(".agents/skills/plain"));
    p.ok(&["status"]);
}

#[test]
fn bad_names_outside_the_selected_skill_are_skipped() {
    let s = Server::start();
    let sha = "a".repeat(40);
    let serve = |extra: &[(&str, &str)]| {
        let mut files = vec![("skills/two/SKILL.md", skill_md("two")), ("skills/two/ref/x.md", "x".into())];
        files.extend(extra.iter().map(|(p, c)| (*p, c.to_string())));
        let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
        s.put(&format!("/repos/acme/tools/zipball/{sha}"), zipball(&files));
    };
    s.put("/repos/acme/tools/tags", "[]");
    s.put("/repos/acme/tools/commits/HEAD", gh_commit(&sha, 240).to_string());
    serve(&[("docs/aux.md", "a"), ("src/README.md", "r"), ("src/readme.md", "r"), ("bad/AUX/SKILL.md", &skill_md("bad"))]);
    let p = with_github(Project::new(&s), &s);
    p.manifest(&s, "");
    p.ok(&["add", "github:acme/tools/two"]);
    assert!(p.exists(".agents/skills/two/ref/x.md") && p.exists(".claude/skills/two"));
    assert!(!p.exists(".agents/skills/bad"), "a skipped SKILL.md is not discovered");

    for (bad, err) in [
        (("skills/two/aux.md", "a"), r#""acme-pkg-1234abc/skills/two/aux.md": "aux.md" is not a valid file name on every OS"#),
        (("skills/two/ref/X.md", "x"), r#""skills/two/ref/X.md" and "skills/two/ref/x.md" are one name on case-insensitive file systems"#),
    ] {
        serve(&[bad]);
        let q = with_github(Project::new(&s), &s);
        q.manifest(&s, "");
        let out = q.expect(&["add", "github:acme/tools/two"], 5);
        assert!(out.contains(&format!("acme/tools: archive: {err}")), "{out}");
        assert!(!q.exists("skills.lock"));
    }
}

#[cfg(unix)]
#[test]
fn global_lists_unmanaged_skills_and_adopts_one() {
    let s = Server::start();
    let sha = "a".repeat(40);
    s.put("/repos/acme/tools/tags", "[]");
    s.put("/repos/acme/tools/commits/HEAD", gh_commit(&sha, 240).to_string());
    // Regression guard (D37): a plugin copy of a skill used to fail the whole package.
    s.put(
        &format!("/repos/acme/tools/zipball/{sha}"),
        zipball(&[("skills/two/SKILL.md", &skill_md("two")), ("plugins/p/skills/two/SKILL.md", "---\nname: two\n---\nplugin")]),
    );
    let p = with_github(Project::new(&s), &s);
    p.global_manifest(&s, "");
    let h = p.home().to_path_buf();
    for (dir, body) in [(".agents/skills/two", "old"), (".agents/skills/mine", "mine"), (".claude/skills/other", "other")] {
        std::fs::create_dir_all(h.join(dir)).unwrap();
        std::fs::write(h.join(dir).join("SKILL.md"), body).unwrap();
    }
    std::os::unix::fs::symlink(h.join(".agents/skills/two"), h.join(".claude/skills/two")).unwrap();
    let npx = json!({ "version": 3, "skills": { "two": { "source": "Acme/tools", "sourceType": "github" } } });
    std::fs::write(h.join(".agents/.skill-lock.json"), npx.to_string()).unwrap();

    let doc: serde_json::Value = serde_json::from_str(&p.global(&["status", "--json"], 0)).unwrap();
    let entries = doc["entries"].as_array().unwrap();
    let state = |n: &str| entries.iter().find(|e| e["skill"] == n).map(|e| (e["state"].clone(), e["adopt"].clone()));
    assert_eq!(state("two"), Some((json!("not managed"), json!("github:Acme/tools/two"))), "{doc}");
    assert_eq!(state("mine"), Some((json!("not managed"), json!(null))), "{doc}");
    assert_eq!(state("other"), Some((json!("not managed"), json!(null))), "skills only in a target count too: {doc}");

    let out = p.global(&["add", "github:acme/tools/two"], 1);
    assert!(out.contains("`add --adopt` replaces it"), "{out}");
    assert!(p.expect(&["add", "--adopt", "github:acme/tools/two"], 1).contains("--adopt applies only with --global"));

    let out = p.global(&["add", "--adopt", "github:acme/tools/two"], 0);
    assert!(out.contains("is in skills/two and plugins/p/skills/two; using skills/two"), "{out}");
    let moved: Vec<_> = std::fs::read_dir(h.join(".config/skillcask/adopted")).unwrap().flatten().collect();
    assert_eq!(std::fs::read_to_string(moved[0].path().join(".agents/skills/two/SKILL.md")).unwrap(), "old");
    assert_eq!(std::fs::read_to_string(h.join(".agents/skills/two/SKILL.md")).unwrap(), skill_md("two"));
    assert_eq!(std::fs::read_link(h.join(".claude/skills/two")).unwrap(), h.join(".agents/skills/two"));
    let doc: serde_json::Value = serde_json::from_str(&p.global(&["status", "--json"], 0)).unwrap();
    assert!(doc["entries"].as_array().unwrap().iter().any(|e| e["skill"] == "two" && e["state"] == "ok"), "{doc}");
}
