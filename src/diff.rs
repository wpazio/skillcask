//! Line-based unified diff (Myers).

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Same,
    Del,
    Ins,
}

/// Edit distance past which the diff replaces the whole file: the trace grows with its square.
const MAX_EDITS: usize = 1000;
/// Past these, per side, `unified` gives up: each Myers step walks the whole file.
const MAX_LINES: usize = 10_000;
const MAX_BYTES: usize = 1 << 20;

/// Shortest edit script from `a` to `b` (Myers 1986), or `None` past `MAX_EDITS`.
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "Myers indexes v by a signed diagonal k; sides are capped at MAX_LINES"
)]
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let off = (n + m).min(MAX_EDITS as isize) + 1;
    let mut v = vec![0isize; 2 * off as usize + 1];
    // trace[d]: v for k in -d-1..=d+1 before step d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=(n + m).min(MAX_EDITS as isize) {
        trace.push(v[(off - d - 1) as usize..=(off + d + 1) as usize].to_vec());
        for k in (-d..=d).step_by(2) {
            let at = |k: isize| v[(off + k) as usize];
            let mut x = if k == -d || (k != d && at(k - 1) < at(k + 1)) { at(k + 1) } else { at(k - 1) + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(off + k) as usize] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
        }
    }
    None
}

#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "Myers indexes v by a signed diagonal k; sides are capped at MAX_LINES"
)]
fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let (mut x, mut y) = (n, m);
    let mut ops = Vec::new();
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let at = |k: isize| v[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Same);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            ops.push(if x == prev_x { Op::Ins } else { Op::Del });
        }
        (x, y) = (prev_x, prev_y);
    }
    ops.reverse();
    ops
}

/// Hunks between `a` and `b` with `context` lines around each change: `@@ -l,n +l,n @@`, then
/// lines led by ` `, `-` or `+` without their line end, and `\ No newline at end of file` after
/// a last line that has none. Empty when equal; `None` when a side is too large to compare.
pub fn unified(a: &str, b: &str, context: usize) -> Option<Vec<String>> {
    if a.len().max(b.len()) > MAX_BYTES {
        return None;
    }
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split_inclusive('\n').collect(), b.split_inclusive('\n').collect());
    if a.len().max(b.len()) > MAX_LINES {
        return None;
    }
    // ponytail: past MAX_EDITS the whole file is replaced; a linear-space Myers would keep it minimal.
    let ops = myers(&a, &b).unwrap_or_else(|| [vec![Op::Del; a.len()], vec![Op::Ins; b.len()]].concat());
    // Each op with the index of its line in `a` and in `b` (the next line, for the other side).
    let mut at = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0, 0);
    for op in &ops {
        at.push((*op, i, j));
        match op {
            Op::Same => (i, j) = (i + 1, j + 1),
            Op::Del => i += 1,
            Op::Ins => j += 1,
        }
    }
    let changes: Vec<usize> = (0..at.len()).filter(|&x| at[x].0 != Op::Same).collect();
    let mut out = Vec::new();
    let mut c = 0;
    while c < changes.len() {
        let mut last = c;
        while last + 1 < changes.len() && changes[last + 1] - changes[last] <= 2 * context + 1 {
            last += 1;
        }
        let (from, to) = (changes[c].saturating_sub(context), (changes[last] + context).min(at.len() - 1));
        let hunk = &at[from..=to];
        let count = |skip: Op| hunk.iter().filter(|h| h.0 != skip).count();
        let start = |n: usize, line: usize| if n == 0 { line } else { line + 1 };
        let (na, nb) = (count(Op::Ins), count(Op::Del));
        out.push(format!("@@ -{},{na} +{},{nb} @@", start(na, hunk[0].1), start(nb, hunk[0].2)));
        for &(op, i, j) in hunk {
            let (mark, line) = match op {
                Op::Same => (' ', a[i]),
                Op::Del => ('-', a[i]),
                Op::Ins => ('+', b[j]),
            };
            out.push(format!("{mark}{}", line.strip_suffix('\n').unwrap_or(line)));
            if !line.ends_with('\n') {
                out.push("\\ No newline at end of file".into());
            }
        }
        c = last + 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<&str> {
        s.split_inclusive('\n').collect()
    }

    #[test]
    fn edit_script_is_minimal() {
        let (a, b) = (lines("a\nb\nc\na\nb\nb\na\n"), lines("c\nb\na\nb\na\nc\n"));
        let ops = myers(&a, &b).unwrap();
        assert_eq!(ops.iter().filter(|o| **o != Op::Same).count(), 5);
        assert_eq!(myers(&[], &[]), Some(vec![]));
        assert_eq!(myers(&["x\n"], &[]), Some(vec![Op::Del]));
    }

    fn unified(a: &str, b: &str, context: usize) -> Vec<String> {
        super::unified(a, b, context).unwrap()
    }

    #[test]
    fn too_large_files_are_not_compared() {
        let lines = |n: usize| "x\n".repeat(n);
        assert!(super::unified(&lines(MAX_LINES), &lines(MAX_LINES), 3).is_some());
        assert!(super::unified(&lines(MAX_LINES + 1), "", 3).is_none());
        assert!(super::unified("", &"x".repeat(MAX_BYTES + 1), 3).is_none());
    }

    #[test]
    fn hunks_with_context() {
        assert_eq!(unified("a\nb\n", "a\nb\n", 3), Vec::<String>::new());
        assert_eq!(unified("a\nb\nc\n", "a\nB\nc\n", 3), ["@@ -1,3 +1,3 @@", " a", "-b", "+B", " c"]);
        assert_eq!(unified("", "x\n", 3), ["@@ -0,0 +1,1 @@", "+x"]);
        assert_eq!(unified("x\n", "", 3), ["@@ -1,1 +0,0 @@", "-x"]);
        assert_eq!(unified("a\n", "a", 3), ["@@ -1,1 +1,1 @@", "-a", "+a", "\\ No newline at end of file"]);

        let text = |changed: &[usize]| -> String {
            (1..=20).map(|i| if changed.contains(&i) { format!("new {i}\n") } else { format!("{i}\n") }).collect()
        };
        let h = unified(&text(&[]), &text(&[2, 19]), 3);
        assert_eq!(h.iter().filter(|l| l.starts_with("@@")).count(), 2, "{h:?}");
        assert_eq!(h[0], "@@ -1,5 +1,5 @@");
        assert_eq!(h[7], "@@ -16,5 +16,5 @@");
        let hunks = |a: &[usize], b: &[usize]| unified(&text(a), &text(b), 3).iter().filter(|l| l.starts_with("@@")).count();
        assert_eq!(hunks(&[], &[2, 9]), 1, "6 unchanged lines between changes join");
        assert_eq!(hunks(&[], &[2, 10]), 2);
    }

    #[test]
    fn huge_edits_replace_the_file() {
        let old: String = (0..MAX_EDITS + 10).map(|i| format!("{i}\n")).collect();
        let new: String = (0..MAX_EDITS + 10).map(|i| format!("n{i}\n")).collect();
        let h = unified(&old, &new, 3);
        assert_eq!(h[0], format!("@@ -1,{0} +1,{0} @@", MAX_EDITS + 10));
        assert_eq!(h.len(), 1 + 2 * (MAX_EDITS + 10));
    }
}
