//! Credentials from git's credential helper, so tokens can live in the macOS Keychain, the Windows
//! Credential Manager or libsecret instead of a plain-text `auth.json`. Asked per host after the
//! Composer sources; `SKILLCASK_GIT_CREDENTIALS=0` turns it off.

use std::fmt::Write as _;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::bail;
use crate::error::{AUTH, Result};

/// Username that marks a token for the `PRIVATE-TOKEN` header (GitLab).
pub const PRIVATE_TOKEN: &str = "PRIVATE-TOKEN";
/// Username git uses for OAuth tokens; sent as `Authorization: Bearer`.
pub const OAUTH2: &str = "oauth2";

pub fn enabled() -> bool {
    std::env::var("SKILLCASK_GIT_CREDENTIALS").map_or(true, |v| v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// `git credential <action>` with `input` on stdin; `None` when git is missing or fails. Without
/// `prompt` git must not ask: no terminal prompt, no askpass program, no credential manager UI.
fn git(action: &str, input: &str, prompt: bool) -> Option<String> {
    let mut c = Command::new("git");
    if !prompt {
        // An empty core.askPass with GIT_ASKPASS unset also skips SSH_ASKPASS.
        c.args(["-c", "core.askPass=", "-c", "credential.interactive=never"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .stderr(Stdio::null());
    }
    let mut child = c.args(["credential", action]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `host` is `host` or `host:port`, from a parsed URL, so it holds no newline.
fn describe(host: &str, username: Option<&str>, password: Option<&str>) -> String {
    let mut s = format!("protocol=https\nhost={host}\n");
    for (k, v) in [("username", username), ("password", password)] {
        if let Some(v) = v {
            let _ = writeln!(s, "{k}={v}");
        }
    }
    s + "\n"
}

fn field(out: &str, key: &str) -> Option<String> {
    out.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).filter(|v| !v.is_empty()).map(String::from)
}

/// (username, password) git's helper holds for `https://<host>`.
pub fn fill(host: &str) -> Option<(String, String)> {
    let out = git("fill", &describe(host, None, None), false)?;
    Some((field(&out, "username")?, field(&out, "password")?))
}

/// Stores a token for `host` with git's helper. `token`: from a pipe; `None` lets git prompt
/// without echo. Returns the token's length in characters and the helpers that keep it, so the
/// user can tell the paste arrived without the token being shown.
pub fn store(host: &str, username: &str, token: Option<&str>) -> Result<(usize, String)> {
    // A newline would add fields to git's credential protocol.
    if username.is_empty()
        || token == Some("")
        || [Some(username), token].into_iter().flatten().any(|v| v.contains(|c: char| c.is_control()))
    {
        bail!(AUTH, "the username and token must be one line without control characters");
    }
    let helpers = Command::new("git").args(["config", "--get-all", "credential.helper"]).output();
    let helpers = helpers.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    if helpers.trim().is_empty() {
        bail!(AUTH, "git has no credential helper; set credential.helper (osxkeychain, manager or libsecret) first");
    }
    if helpers.lines().any(|h| h.trim() == "store") {
        crate::ui::warn("git's credential helper `store` keeps the token in plain text in ~/.git-credentials");
    }
    // A stored token would be returned instead of the prompt.
    git("reject", &describe(host, Some(username), None), false);
    let token = match token {
        Some(t) => t.to_string(),
        None => {
            let out = git("fill", &describe(host, Some(username), None), true)
                .ok_or_else(|| crate::error::Error::new(AUTH, "no token entered"))?;
            field(&out, "password").ok_or_else(|| crate::error::Error::new(AUTH, "no token entered"))?
        }
    };
    if git("approve", &describe(host, Some(username), Some(&token)), false).is_none() {
        bail!(AUTH, "git credential approve failed for {host}");
    }
    let helpers: Vec<&str> = helpers.lines().map(str::trim).filter(|h| !h.is_empty()).collect();
    Ok((token.chars().count(), helpers.join(", ")))
}

/// Only the entry of `username`: without one git would erase every credential for the host,
/// including the user's own for git itself.
pub fn remove(host: &str, username: &str) -> Result<()> {
    if username.is_empty() || username.contains(|c: char| c.is_control()) {
        bail!(AUTH, "invalid username {username:?}");
    }
    if git("reject", &describe(host, Some(username), None), false).is_none() {
        bail!(AUTH, "git credential reject failed for {host}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_and_reads_the_protocol() {
        assert_eq!(describe("h:8443", Some("u"), None), "protocol=https\nhost=h:8443\nusername=u\n\n");
        let out = "protocol=https\nhost=h\nusername=oauth2\npassword=a=b\n";
        assert_eq!((field(out, "username").as_deref(), field(out, "password").as_deref()), (Some("oauth2"), Some("a=b")));
        assert_eq!(field("password=\n", "password"), None);
    }
}
