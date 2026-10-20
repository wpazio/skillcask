//! Composer version normalisation and constraint matching (not npm semver).

use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Stability {
    Dev,
    Alpha,
    Beta,
    RC,
    Stable,
    Patch,
}

impl Stability {
    fn parse(s: &str) -> Option<Stability> {
        Some(match s.to_ascii_lowercase().as_str() {
            "dev" => Stability::Dev,
            "alpha" | "a" => Stability::Alpha,
            "beta" | "b" => Stability::Beta,
            "rc" => Stability::RC,
            "stable" => Stability::Stable,
            "patch" | "pl" | "p" => Stability::Patch,
            _ => return None,
        })
    }

    fn effective(self) -> Stability {
        if self == Stability::Patch { Stability::Stable } else { self }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Version {
    pub parts: [u64; 4],
    pub stability: Stability,
    /// Numbers after the stability word: `beta.1.2` is `[1, 2]`, `beta` is `[0]`.
    pub n: Vec<u64>,
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.parts, self.stability, &self.n).cmp(&(o.parts, o.stability, &o.n))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.parts;
        write!(f, "{a}.{b}.{c}.{d}")?;
        match self.stability {
            Stability::Stable => Ok(()),
            Stability::Dev => write!(f, "-dev"),
            s => {
                let n: Vec<String> = self.n.iter().map(u64::to_string).collect();
                write!(f, "-{s:?}{}", n.join("."))
            }
        }
    }
}

impl Version {
    /// Parses a release version (`v1.2.3`, `1.0.0.0-beta2`). `dev-*` branches and `-dev`
    /// versions return `None`: they are never resolved.
    pub fn parse(s: &str) -> Option<Version> {
        parse_partial(s).map(|p| p.0).filter(|v| v.stability != Stability::Dev)
    }

    /// `0.0.0`, the placeholder of a candidate that is a git ref, not a version: selection by
    /// constraint never sees it, so it is never compared.
    pub fn unversioned() -> Version {
        Version { parts: [0; 4], stability: Stability::Stable, n: vec![0] }
    }

    /// Default constraint written by `add`; `^0.0` would also take later `0.0.x` patches,
    /// which Composer treats as breaking.
    pub fn major_minor(&self) -> String {
        let [a, b, c, _] = self.parts;
        if a == 0 && b == 0 { format!("^0.0.{c}") } else { format!("^{a}.{b}") }
    }

    fn floor(mut self) -> Version {
        self.stability = Stability::Dev;
        self.n = vec![0];
        self
    }

    fn bump(mut self, idx: usize) -> Version {
        self.parts[idx] += 1;
        for p in &mut self.parts[idx + 1..] {
            *p = 0;
        }
        self.floor()
    }
}

/// Returns the version, how many numeric parts were written, and whether a stability suffix
/// was written.
fn parse_partial(s: &str) -> Option<(Version, usize, bool)> {
    // The original string is printed and locked, and build metadata after `+` is not compared.
    if s.contains(char::is_control) {
        return None;
    }
    let s = s.trim();
    // Build metadata does not take part in comparison, as in Composer.
    let s = s.split_once('+').map_or(s, |(v, _)| v);
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    if s.starts_with("dev-") {
        return None;
    }
    let mut parts = [0u64; 4];
    let mut count = 0;
    let mut rest = s;
    while count < 4 {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 18 {
            break;
        }
        parts[count] = rest[..digits].parse().ok()?;
        count += 1;
        rest = &rest[digits..];
        match rest.strip_prefix('.') {
            Some(r) if r.starts_with(|c: char| c.is_ascii_digit()) => rest = r,
            _ => break,
        }
    }
    if count == 0 {
        return None;
    }
    let rest = rest.trim_start_matches(['.', '_', '-']);
    if rest.is_empty() {
        return Some((Version { parts, stability: Stability::Stable, n: vec![0] }, count, false));
    }
    let lower = rest.to_ascii_lowercase();
    let (body, dev) = match lower.strip_suffix("dev") {
        Some(b) => (b.trim_end_matches(['.', '-']), true),
        None => (lower.as_str(), false),
    };
    if body.is_empty() {
        return dev.then_some((Version { parts, stability: Stability::Dev, n: vec![0] }, count, true));
    }
    let word = body.bytes().take_while(u8::is_ascii_alphabetic).count();
    let stability = Stability::parse(&body[..word])?;
    let tail = body[word..].trim_start_matches(['.', '-']);
    if !tail.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') {
        return None;
    }
    let mut n: Vec<u64> = tail.split(['.', '-']).filter(|x| !x.is_empty()).map(|x| x.parse().ok()).collect::<Option<_>>()?;
    if n.is_empty() {
        n.push(0);
    }
    let stability = if dev { Stability::Dev } else { stability };
    Some((Version { parts, stability, n }, count, true))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
    Ne,
}

#[derive(Clone, Debug)]
struct Cmp {
    op: Op,
    v: Version,
}

impl Cmp {
    fn matches(&self, v: &Version) -> bool {
        let o = v.cmp(&self.v);
        match self.op {
            Op::Ge => o != Ordering::Less,
            Op::Gt => o == Ordering::Greater,
            Op::Le => o != Ordering::Greater,
            Op::Lt => o == Ordering::Less,
            Op::Eq => o == Ordering::Equal,
            Op::Ne => o != Ordering::Equal,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Constraint {
    /// OR of ANDs; an empty AND matches everything.
    any: Vec<Vec<Cmp>>,
    min_stability: Stability,
}

impl Constraint {
    pub fn parse(s: &str) -> Result<Constraint, String> {
        let err = || format!("invalid version constraint {s:?}");
        let mut any = Vec::new();
        let mut min_stability = Stability::Stable;
        for group in s.split('|').map(str::trim).filter(|g| !g.is_empty()) {
            let mut all = Vec::new();
            if let Some((lo, hi)) = group.split_once(" - ") {
                let (lo, _, lo_explicit) = parse_partial(lo).ok_or_else(err)?;
                let (hi, hi_count, _) = parse_partial(hi).ok_or_else(err)?;
                if lo_explicit {
                    min_stability = min_stability.min(lo.stability.effective());
                }
                all.push(Cmp { op: Op::Ge, v: if lo_explicit { lo } else { lo.floor() } });
                all.push(if hi_count < 3 { Cmp { op: Op::Lt, v: hi.bump(hi_count - 1) } } else { Cmp { op: Op::Le, v: hi } });
            } else {
                for tok in tokens(group) {
                    let (base, flag) = match tok.split_once('@') {
                        Some((b, f)) => (b, Some(Stability::parse(f).ok_or_else(err)?)),
                        None => (tok.as_str(), None),
                    };
                    if let Some(f) = flag {
                        min_stability = min_stability.min(f.effective());
                    }
                    let explicit = atom(base, &mut all).ok_or_else(err)?;
                    if let Some(st) = explicit {
                        min_stability = min_stability.min(st.effective());
                    }
                }
            }
            any.push(all);
        }
        if any.is_empty() {
            return Err(err());
        }
        Ok(Constraint { any, min_stability })
    }

    pub fn matches(&self, v: &Version) -> bool {
        self.any.iter().any(|all| all.iter().all(|c| c.matches(v)))
    }

    /// Matches and passes the stability filter: stable only unless the constraint names one.
    pub fn allows(&self, v: &Version) -> bool {
        v.stability.effective() >= self.min_stability && self.matches(v)
    }
}

/// `x.y.z` with nothing else, as required by `allow-ranges = false`.
pub fn is_exact(s: &str) -> bool {
    matches!(parse_partial(s), Some((_, n, _)) if n >= 3)
        && s.trim_start_matches(['v', 'V']).starts_with(|c: char| c.is_ascii_digit())
        && !s.contains([' ', ',', '|', '*', '@'])
}

/// Splits an AND group on commas and whitespace, keeping `>= 1.0` together.
fn tokens(group: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut glue = false;
    for t in group.split([',', ' ']).filter(|t| !t.is_empty()) {
        if glue {
            out.last_mut().unwrap().push_str(t);
        } else {
            out.push(t.to_string());
        }
        glue = t.chars().all(|c| "<>=!^~".contains(c));
    }
    out
}

/// Pushes the comparisons for one atom; returns the stability it names explicitly, if any.
fn atom(s: &str, all: &mut Vec<Cmp>) -> Option<Option<Stability>> {
    if s.is_empty() || s == "*" || s.eq_ignore_ascii_case("x") {
        return Some(None);
    }
    for op in ["^", "~", ">=", "<=", "!=", "<>", "==", ">", "<", "="] {
        if let Some(rest) = s.strip_prefix(op) {
            if op == "~" && rest.starts_with('>') {
                continue;
            }
            let (v, count, explicit) = parse_partial(rest)?;
            let named = explicit.then_some(v.stability);
            let lo = if explicit { v.clone() } else { v.clone().floor() };
            match op {
                "^" => {
                    let pos = if v.parts[0] != 0 || count == 1 {
                        0
                    } else if v.parts[1] != 0 || count == 2 {
                        1
                    } else {
                        2
                    };
                    all.push(Cmp { op: Op::Ge, v: lo });
                    all.push(Cmp { op: Op::Lt, v: v.bump(pos) });
                }
                "~" => {
                    all.push(Cmp { op: Op::Ge, v: lo });
                    all.push(Cmp { op: Op::Lt, v: v.bump(count.saturating_sub(2)) });
                }
                ">=" => all.push(Cmp { op: Op::Ge, v: lo }),
                "<" => all.push(Cmp { op: Op::Lt, v: lo }),
                ">" => all.push(Cmp { op: Op::Gt, v }),
                "<=" => all.push(Cmp { op: Op::Le, v }),
                "!=" | "<>" => all.push(Cmp { op: Op::Ne, v }),
                _ => all.push(Cmp { op: Op::Eq, v }),
            }
            return Some(named);
        }
    }
    if let Some(prefix) = s.strip_suffix(".*").or_else(|| s.strip_suffix(".x")).or_else(|| s.strip_suffix(".X")) {
        let (v, count, explicit) = parse_partial(prefix)?;
        if explicit {
            return None;
        }
        all.push(Cmp { op: Op::Ge, v: v.clone().floor() });
        all.push(Cmp { op: Op::Lt, v: v.bump(count - 1) });
        return Some(None);
    }
    let (v, _, explicit) = parse_partial(s)?;
    let named = explicit.then_some(v.stability);
    all.push(Cmp { op: Op::Eq, v });
    Some(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        parse_partial(s).unwrap().0
    }

    /// Regression guard: build metadata is not compared, but the raw string is printed and locked.
    #[test]
    fn control_characters_are_not_a_version() {
        assert!(Version::parse("1.0.0+\x1b]52;c;ZXZpbA==\x07").is_none());
        assert!(Version::parse("1.0.0+build.5").is_some());
    }

    #[test]
    fn normalises_versions() {
        assert_eq!(v("v1.2.3").to_string(), "1.2.3.0");
        assert_eq!(v("1.0.0.0-beta2").to_string(), "1.0.0.0-Beta2");
        assert_eq!(v("1.0.0-RC1").to_string(), "1.0.0.0-RC1");
        assert_eq!(v("2.0.0-alpha.3").to_string(), "2.0.0.0-Alpha3");
        assert_eq!(v("1.0-dev").to_string(), "1.0.0.0-dev");
        assert!(Version::parse("dev-main").is_none());
        assert!(Version::parse("1.0.x-dev").is_none());
        assert!(Version::parse("9999999-dev").is_none());
        assert!(Version::parse("garbage").is_none());
        assert!(v("1.0.0-beta1") < v("1.0.0-RC1"));
        assert!(v("1.0.0-RC1") < v("1.0.0"));
        assert!(v("1.0.0") < v("1.0.0-patch1"));
        assert_eq!(Version::parse("1.2.3+build.5"), Version::parse("1.2.3"));
        assert_eq!(Version::parse("v2.0.0-beta1+sha.abc"), Version::parse("2.0.0-beta1"));
        assert!(v("1.0.0-beta.1.1") < v("1.0.0-beta.1.2"));
        assert!(v("1.0.0-beta1") < v("1.0.0-beta1.1"));
        assert_eq!(v("1.0.0-beta"), v("1.0.0-beta0"));
        assert_eq!(v("1.0.0-beta1-2"), v("1.0.0-beta.1.2"));
        assert_eq!(v("1.0.0-stable"), v("1.0.0"));
        assert!(Version::parse("1.2.3.4.5").is_none());
        // Date-like parts fit; a part that could overflow when bumped is not a version.
        assert!(Version::parse("1.123456789012345678").is_some());
        assert!(Version::parse("1.1234567890123456789").is_none());
    }

    // Cases follow Composer's VersionParserTest (parseConstraints) and the spec's examples.
    #[test]
    fn constraint_table() {
        let table: &[(&str, &[&str], &[&str])] = &[
            ("^1.2", &["1.2.0", "1.9.9"], &["1.1.9", "2.0.0", "2.0.0-beta1"]),
            ("^1.2.3", &["1.2.3", "1.99"], &["1.2.2", "2.0"]),
            ("^0.3", &["0.3.0", "0.3.9"], &["0.4.0", "0.2.9"]),
            ("^0.0.3", &["0.0.3"], &["0.0.4"]),
            ("^0", &["0.0.1", "0.9"], &["1.0"]),
            ("^0.0", &["0.0.9"], &["0.1.0"]),
            ("~1.2", &["1.2", "1.9"], &["2.0", "1.1"]),
            ("~1.2.3", &["1.2.3", "1.2.9"], &["1.3.0", "1.2.2"]),
            ("~1", &["1.0", "1.9"], &["2.0"]),
            ("1.2.*", &["1.2.0", "1.2.99"], &["1.3.0", "1.1.9"]),
            ("1.*", &["1.0", "1.9.9"], &["2.0"]),
            ("1.2.x", &["1.2.5"], &["1.3"]),
            ("*", &["0.1", "99.0"], &[]),
            (">=1.0", &["1.0", "5.0"], &["0.9"]),
            (">= 1.0 <2.0", &["1.5"], &["2.0", "0.9"]),
            (">=1.0,<1.1", &["1.0.9"], &["1.1"]),
            (">1.0", &["1.0.1"], &["1.0"]),
            ("<=1.2", &["1.2.0", "0.1"], &["1.2.0.1"]),
            ("!=1.0.1", &["1.0.0", "1.0.2"], &["1.0.1"]),
            ("1.0.0 - 2.0.0", &["1.0.0", "2.0.0"], &["2.0.0.1", "2.0.1"]),
            ("1.0 - 2.0", &["2.0.5"], &["2.1.0"]),
            ("^1.0 || ^3.0", &["1.5", "3.1"], &["2.0", "4.0"]),
            ("1.4.2", &["1.4.2", "v1.4.2"], &["1.4.3"]),
            ("1.0.0-beta2", &["1.0.0-beta2"], &["1.0.0-beta3", "1.0.0"]),
            ("^1.0@beta", &["1.1.0-beta1", "1.0.0"], &["1.1.0-alpha1", "2.0.0-beta1"]),
            ("@beta", &["3.0.0-beta1", "1.0"], &["3.0.0-alpha1"]),
            ("^1.0@dev", &["1.0.0-alpha1", "1.0.0"], &["2.0.0-alpha1"]),
            ("^1.0@stable", &["1.0.0"], &["1.1.0-RC1"]),
            ("^2.0", &["2.1.0"], &["2.1.0-RC1", "2.1.0-dev"]),
        ];
        for (c, ok, bad) in table {
            let parsed = Constraint::parse(c).unwrap_or_else(|e| panic!("{e}"));
            for s in *ok {
                assert!(parsed.allows(&v(s)), "{c} should allow {s}");
            }
            for s in *bad {
                assert!(!parsed.allows(&v(s)), "{c} should reject {s}");
            }
        }
    }

    #[test]
    fn rejects_garbage_and_detects_exact() {
        assert!(Constraint::parse("").is_err());
        assert!(Constraint::parse("^abc").is_err());
        assert!(Constraint::parse("1.0@nope").is_err());
        assert!(is_exact("1.4.2"));
        assert!(is_exact("v1.4.2"));
        assert!(!is_exact("^1.4.2"));
        assert!(!is_exact("1.4"));
        assert!(!is_exact("1.4.*"));
        assert_eq!(v("1.4.2").major_minor(), "^1.4");
        assert_eq!(v("0.3.1").major_minor(), "^0.3");
        assert_eq!(v("0.0.5").major_minor(), "^0.0.5");
    }
}
