//! GitHub repositories as a skill source: semver tags as versions, or a branch, tag or commit
//! pinned to its sha. Skills are found by their `SKILL.md`, see `discover`.

use std::sync::PoisonError;

use jiff::{RoundMode, SignedDuration, Timestamp, TimestampRound, Unit};
use serde_json::Value;

use crate::bail;
use crate::error::{Error, MANIFEST, NETWORK, Result};
use crate::http::Http;
use crate::lock::Dist;
use crate::manifest::{GitHub, Repository, SkillReq};
use crate::registry::{self, Candidate};
use crate::skill::{FileEntry, SKILL_MD};
use crate::version::{Constraint, Version};

pub const API: &str = "https://api.github.com";
/// The `url` of the manifest's `github` repository entry.
static CONFIGURED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
const TAG_PAGES: usize = 10;
/// Pushes read from the Activity API, one page.
const ACTIVITY: usize = 100;

/// `SKILLCASK_GITHUB_API`, else the manifest's `github` entry, points at GitHub Enterprise
/// (`https://ghe.example.com/api/v3`) or a test server.
pub fn api() -> String {
    let set = || CONFIGURED.get().cloned().unwrap_or_else(|| API.into());
    std::env::var("SKILLCASK_GITHUB_API").ok().filter(|v| !v.is_empty()).unwrap_or_else(set).trim_end_matches('/').to_string()
}

/// A run reads one manifest, so the first one sets the API base.
pub fn set_api(url: &str) {
    let _ = CONFIGURED.set(url.to_string());
}

pub fn repository(name: &str) -> Repository {
    Repository { name: "github".into(), url: format!("{}/repos/{name}", api()), ..Default::default() }
}

pub fn is_sha(r: &str) -> bool {
    r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit())
}

fn get(http: &Http, name: &str, path: &str) -> Result<Value> {
    let url = format!("{}{path}", repository(name).url);
    http.get_json(&url)?.ok_or_else(|| {
        // GitHub answers 404, not 401, for a private repository read without a token.
        let hint = if http.has_credential(&url) { "" } else { "; private repositories need GITHUB_TOKEN" };
        Error::new(MANIFEST, format!("GitHub: {name}: {path:?} not found (no such repository, ref or tag?{hint})"))
    })
}

fn commit_of(v: &Value, name: &str) -> Result<(String, Timestamp)> {
    let sha = v.get("sha").and_then(Value::as_str).filter(|s| is_sha(s));
    let date = v.pointer("/commit/committer/date").and_then(Value::as_str).and_then(|d| d.parse().ok());
    match (sha, date) {
        (Some(s), Some(d)) => Ok((s.to_ascii_lowercase(), d)),
        _ => bail!(NETWORK, "GitHub: {name}: unexpected commit response"),
    }
}

fn commit(http: &Http, name: &str, r: &str) -> Result<(String, Timestamp)> {
    commit_of(&get(http, name, &format!("/commits/{r}"))?, name)
}

fn candidate(name: &str, version: String, parsed: Version, sha: &str, time: Option<Timestamp>) -> Candidate {
    Candidate {
        name: name.to_string(),
        version,
        parsed,
        time,
        dist: Dist { url: format!("{}/zipball/{sha}", repository(name).url), reference: Some(sha.to_string()) },
        dist_shasum: None,
        // Empty: the skills are discovered in the archive.
        skill_paths: Vec::new(),
        abandoned: None,
        description: None,
        license: Vec::new(),
    }
}

/// Tags that parse as versions, newest first, without release times. Read once per process:
/// `add` asks before resolving, and the anonymous API allows 60 requests an hour.
pub fn tags(http: &Http, name: &str) -> Result<Vec<Candidate>> {
    static SEEN: std::sync::Mutex<Vec<(String, Vec<Candidate>)>> = std::sync::Mutex::new(Vec::new());
    if let Some((_, c)) = SEEN.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(n, _)| n == name) {
        return Ok(c.clone());
    }
    let out = read_tags(http, name)?;
    SEEN.lock().unwrap_or_else(PoisonError::into_inner).push((name.to_string(), out.clone()));
    Ok(out)
}

fn read_tags(http: &Http, name: &str) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    let mut full = false;
    for page in 1..=TAG_PAGES {
        let v = get(http, name, &format!("/tags?per_page=100&page={page}"))?;
        let list = v.as_array().ok_or_else(|| Error::new(NETWORK, format!("GitHub: {name}: unexpected tags response")))?;
        for t in list {
            let (Some(tag), Some(sha)) = (t.get("name").and_then(Value::as_str), t.pointer("/commit/sha").and_then(Value::as_str)) else {
                continue;
            };
            // Keep the first of tags naming the same version: release times are fetched for one only.
            if let Some(parsed) = Version::parse(tag).filter(|v| is_sha(sha) && !out.iter().any(|c: &Candidate| &c.parsed == v)) {
                out.push(candidate(name, tag.to_string(), parsed, &sha.to_ascii_lowercase(), None));
            }
        }
        full = list.len() >= 100;
        if !full {
            break;
        }
    }
    if full {
        crate::ui::warn(format!("{name}: more than {} tags; only the first {} were read", TAG_PAGES * 100, TAG_PAGES * 100));
    }
    out.sort_by(|a, b| b.parsed.cmp(&a.parsed));
    Ok(out)
}

/// Publication times of releases by tag. Unlike commit dates, which the author sets, GitHub sets
/// them, so a backdated commit cannot skip the cooldown. Missing endpoint: none.
fn releases(http: &Http, name: &str) -> Result<Vec<(String, Timestamp)>> {
    let url = format!("{}/releases?per_page=100", repository(name).url);
    let list = http.get_json(&url)?.and_then(|v| v.as_array().cloned()).unwrap_or_default();
    Ok(list
        .iter()
        .filter_map(|r| Some((r.get("tag_name")?.as_str()?.to_string(), r.get("published_at")?.as_str()?.parse().ok()?)))
        .collect())
}

/// Pushes to branch `r`, newest first, as (before, after, time), with times GitHub sets. `None`
/// when the API has nothing (no such endpoint, a tag, or no activity kept).
fn pushes(http: &Http, name: &str, r: &str) -> Result<Option<Vec<(String, String, Timestamp)>>> {
    // The API knows branches, not `HEAD`.
    let branch = match r {
        "HEAD" => match http.get_json(&repository(name).url)?.as_ref().and_then(|v| v.get("default_branch")?.as_str()) {
            Some(b) => b.to_string(),
            None => return Ok(None),
        },
        _ => r.to_string(),
    };
    let per_page = ACTIVITY.to_string();
    let url =
        url::Url::parse_with_params(&format!("{}/activity", repository(name).url), [("ref", branch.as_str()), ("per_page", &per_page)])
            .map_err(|e| Error::new(MANIFEST, format!("GitHub: {name}: {e}")))?;
    let list = http.get_json(url.as_str())?.and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let out: Vec<_> = list
        .iter()
        .filter_map(|a| {
            let sha = |k: &str| a.get(k).and_then(Value::as_str).filter(|s| is_sha(s)).map(str::to_ascii_lowercase);
            Some((sha("before")?, sha("after")?, a.get("timestamp")?.as_str()?.parse().ok()?))
        })
        .collect();
    Ok((!out.is_empty()).then_some(out))
}

pub fn resolve(http: &Http, req: &SkillReq, min_age: SignedDuration, now: Timestamp) -> Result<Candidate> {
    let name = &req.name;
    let cutoff = now.checked_sub(min_age).unwrap_or(Timestamp::MIN);
    match req.github {
        Some(GitHub::Tags) => {
            let mut cands = tags(http, name)?;
            if cands.is_empty() {
                bail!(MANIFEST, "{name}: the repository has no version tags; use `ref` instead of `version`");
            }
            let c = Constraint::parse(&req.constraint).map_err(|e| Error::new(MANIFEST, format!("{name}: {e}")))?;
            // Release time is one request per tag: newest first, stop at the first one past the cooldown.
            // Older matching tags keep no time, but `select` never reaches them.
            let published = releases(http, name)?;
            for x in cands.iter_mut().filter(|x| c.allows(&x.parsed)) {
                let t = commit(http, name, x.dist.reference.as_deref().expect("set by candidate"))?.1;
                let t = published.iter().find(|(tag, _)| *tag == x.version).map_or(t, |(_, p)| t.max(*p));
                x.time = Some(t);
                if t <= cutoff {
                    break;
                }
            }
            registry::select(&cands, name, &req.constraint, min_age, now)
        }
        Some(GitHub::Ref) => {
            let r = &req.constraint;
            let (mut sha, mut time) = commit(http, name, r)?;
            let pushed = if is_sha(r) { None } else { pushes(http, name, r)? };
            if let Some(list) = &pushed {
                // The head when the cooldown began: after the newest push before it, or before the
                // oldest push GitHub still lists. Commit dates play no part.
                let eligible = list.iter().find(|p| p.2 <= cutoff).map(|p| (p.1.clone(), Some(p.2)));
                let eligible = eligible.or_else(|| (list.len() < ACTIVITY).then(|| (list[list.len() - 1].0.clone(), None)));
                let Some((at, pushed_at)) = eligible.filter(|(sha, _)| sha.bytes().any(|b| b != b'0')) else {
                    let newest = list[0].2.checked_add(min_age).unwrap_or(Timestamp::MAX).round(jiff::Unit::Second).unwrap_or(list[0].2);
                    bail!(
                        MANIFEST,
                        "{name}: every push to {r:?} is newer than policy.min-release-age ({min_age:#}); the newest becomes eligible at {newest}"
                    );
                };
                if at != sha {
                    let t = commit(http, name, &at)?.1;
                    crate::ui::note(format!("{name}: newer pushes to {r:?} are within policy.min-release-age; using {}", &at[..12]));
                    (sha, time) = (at, t);
                }
                // Before the oldest listed push only the commit date is known, at most that push.
                time = pushed_at.unwrap_or_else(|| time.min(list[list.len() - 1].2));
            }
            if time > cutoff && pushed.is_none() {
                let eligible = time.checked_add(min_age).unwrap_or(Timestamp::MAX).round(jiff::Unit::Second).unwrap_or(time);
                if is_sha(r) {
                    bail!(
                        MANIFEST,
                        "{name}: commit {r} is newer than policy.min-release-age ({min_age:#}); it becomes eligible at {eligible}"
                    );
                }
                let until = cutoff.round(TimestampRound::new().smallest(Unit::Second).mode(RoundMode::Trunc)).unwrap_or(cutoff);
                let older = get(http, name, &format!("/commits?sha={r}&per_page=1&until={until}"))?;
                let none = || {
                    Error::new(
                        MANIFEST,
                        format!(
                            "{name}: every commit on {r:?} is newer than policy.min-release-age ({min_age:#}); the newest becomes eligible at {eligible}"
                        ),
                    )
                };
                let first = older.as_array().and_then(|a| a.first()).ok_or_else(none)?;
                (sha, time) = commit_of(first, name)?;
                // Do not trust `until` alone: the API (or a proxy) may ignore it.
                if time > cutoff {
                    return Err(none());
                }
                crate::ui::note(format!("{name}: newer commits on {r:?} are within policy.min-release-age; using {}", &sha[..12]));
            }
            Ok(candidate(name, sha[..12].to_string(), Version::unversioned(), &sha, Some(time)))
        }
        None => unreachable!("only GitHub entries are resolved here"),
    }
}

/// (newest within the constraint, newest overall) for `outdated`; cooldown is not applied.
pub fn newest(http: &Http, req: &SkillReq) -> Result<(Option<String>, Option<String>)> {
    if req.github == Some(GitHub::Ref) {
        let head = commit(http, &req.name, &req.constraint)?.0[..12].to_string();
        return Ok((Some(head.clone()), Some(head)));
    }
    let cands = tags(http, &req.name)?;
    let c = Constraint::parse(&req.constraint).map_err(|e| Error::new(MANIFEST, format!("{}: {e}", req.name)))?;
    let v = |x: Option<&Candidate>| x.map(|x| x.version.clone());
    Ok((v(registry::newest(&cands, Some(&c))), v(registry::newest(&cands, None))))
}

/// Paths of `SKILL.md` files in a repository archive, skipping those nested in another skill.
pub fn discover(entries: &[FileEntry]) -> Vec<String> {
    let mut dirs: Vec<&str> =
        entries.iter().filter_map(|e| if e.path == SKILL_MD { Some("") } else { e.path.strip_suffix(&format!("/{SKILL_MD}")) }).collect();
    dirs.sort_unstable();
    let mut out: Vec<&str> = Vec::new();
    for d in dirs {
        if !out.iter().any(|o| o.is_empty() || d.starts_with(&format!("{o}/"))) {
            out.push(d);
        }
    }
    out.into_iter().map(|d| if d.is_empty() { SKILL_MD.to_string() } else { format!("{d}/{SKILL_MD}") }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_outermost_skills() {
        let e = |p: &str| FileEntry::new(p.into(), false, b"");
        let found = discover(&[
            e("README.md"),
            e("skills/b/SKILL.md"),
            e("skills/b/examples/x/SKILL.md"),
            e("skills/a/SKILL.md"),
            e("a/SKILL.mdx"),
        ]);
        assert_eq!(found, ["skills/a/SKILL.md", "skills/b/SKILL.md"]);
        assert_eq!(discover(&[e("SKILL.md"), e("sub/SKILL.md")]), ["SKILL.md"]);
        assert!(is_sha(&"a".repeat(40)) && !is_sha("main") && !is_sha(&"g".repeat(40)));
    }
}
