//! HTTP retries on 429 and 5xx (connection failures: unit test in src/http.rs).
mod common;

use common::*;

fn count(s: &Server, path: &str) -> usize {
    s.requests().iter().filter(|r| r.path.split('?').next() == Some(path)).count()
}

#[test]
fn retries_429_and_5xx_and_honours_retry_after() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.respond_once("/p2/acme/one.json", 503, &[("Retry-After", "0")], "");
    s.respond_once("/p2/acme/one.json", 429, &[("Retry-After", "0")], "");
    s.respond_once("/dist/acme/one/1.0.0.zip", 502, &[("Retry-After", "0")], "");
    let p = Project::new(&s);
    p.manifest(&s, "");
    let out = p.ok(&["add", "acme/one"]);
    assert!(out.contains("HTTP 503; retrying in 0s") && out.contains("HTTP 429; retrying"), "{out}");
    assert_eq!(count(&s, "/p2/acme/one.json"), 3);
    assert_eq!(count(&s, "/dist/acme/one/1.0.0.zip"), 2);
    assert!(p.exists(".agents/skills/one/SKILL.md"));
}

#[test]
fn gives_up_after_three_attempts() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.respond("/p2/acme/one.json", 500, &[("Retry-After", "0")], "");
    let p = Project::new(&s);
    p.manifest(&s, "");
    let out = p.expect(&["add", "acme/one"], 3);
    assert!(out.contains("one.json: HTTP 500"), "{out}");
    assert_eq!(count(&s, "/p2/acme/one.json"), 3);
}

#[test]
fn does_not_wait_for_a_long_retry_after_or_retry_client_errors() {
    let s = Server::start();
    s.simple("acme/one", "1.0.0", "one");
    s.respond("/p2/acme/one.json", 429, &[("Retry-After", "3600")], "");
    let p = Project::new(&s);
    p.manifest(&s, "");
    let out = p.expect(&["add", "acme/one"], 3);
    assert!(out.contains("HTTP 429 (Retry-After: 3600)"), "{out}");
    assert_eq!(count(&s, "/p2/acme/one.json"), 1);

    s.respond("/p2/acme/one.json", 400, &[], "");
    p.expect(&["add", "acme/one"], 3);
    assert_eq!(count(&s, "/p2/acme/one.json"), 2);
}

#[test]
fn github_rate_limit_is_not_retried() {
    let s = Server::start();
    s.respond("/repos/acme/rl/tags", 429, &[("x-ratelimit-remaining", "0")], "");
    let mut p = Project::new(&s);
    p.env.push(("SKILLCASK_GITHUB_API".into(), s.base.clone()));
    p.manifest(&s, "");
    let out = p.expect(&["add", "github:acme/rl"], 2);
    assert!(out.contains("HTTP 429: GitHub API rate limit"), "{out}");
    assert_eq!(count(&s, "/repos/acme/rl/tags"), 1);
}
