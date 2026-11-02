//! Test guard: rules that detect edits which weaken tests.
//! Rules are language-agnostic and cheap. They give false positives; keep the guard advisory.
//!
//! Every rule reads code only: comments are removed and string literals are replaced by
//! a placeholder first, so `# assert x` and `"assertion"` count for nothing (I2, G1).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::broker::Answer;
use crate::text::sha256_hex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardDecision {
    Warn,
    Ask,
    Deny,
}

pub fn is_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    re!(r"(^|/)(tests?|__tests__|spec|specs)/|(^|/)test_[^/]*\.py$|_test\.(py|go|rs)$|\.(test|spec)\.[cm]?[jt]sx?$|(^|/)conftest\.py$|(^|/)tests?\.(py|rs)$|Tests?\.(cs|java|kt)$|_spec\.rb$")
        .is_match(&p)
}

/// A file that is a test, or source code that may hold tests (Rust unit tests in `src/`).
pub fn may_hold_tests(path: &str) -> bool {
    is_test_path(path) || re!(r"\.(rs|py|go|[cm]?[jt]sx?|java|kt|cs|rb)$").is_match(path)
}

/// The text of an edit holds test code, wherever the file is: Rust unit tests live in
/// `src/`, Go and Python tests can too (G2).
pub fn has_test_code(text: &str) -> bool {
    re!(r"#\[(tokio::)?test\b|#\[cfg\(test\)\]|\bdef\s+test_\w*\s*\(|\bfunc\s+Test\w*\s*\(\s*\w+\s+\*testing\.T\b|@Test\b|\bunittest\.TestCase\b")
        .is_match(text)
}

/// Judge an edit of `path`: a test file, or test code anywhere.
pub fn judges(path: &str, old: &str, new: &str) -> bool {
    is_test_path(path) || has_test_code(old) || has_test_code(new)
}

/// Code without comments, with each string literal replaced by `"s<hash>"`. Distinct
/// strings stay distinct (for the same-operand rule); their contents do not count.
/// Line structure is kept, except inside multi-line strings and comments.
pub fn code_only(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let at = |i: usize, s: &str| {
        s.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    while i < chars.len() {
        let c = chars[i];
        // Python triple-quoted strings (docstrings).
        if (at(i, "\"\"\"") || at(i, "'''")) && i + 2 < chars.len() {
            let q: String = chars[i..i + 3].iter().collect();
            let mut j = i + 3;
            while j < chars.len() && !at(j, &q) {
                j += 1;
            }
            let body: String = chars[i + 3..j.min(chars.len())].iter().collect();
            out.push_str(&placeholder(&body));
            i = (j + 3).min(chars.len());
            continue;
        }
        match c {
            '\'' if is_lifetime(&chars, i) => {
                out.push(c);
                i += 1;
            }
            '"' | '`' | '\'' => {
                // A quote with no closing quote on its line (a Rust lifetime, an apostrophe
                // in prose) is not a string.
                let mut j = i + 1;
                let mut body = String::new();
                let mut closed = false;
                while j < chars.len() {
                    let d = chars[j];
                    if d == '\\' && j + 1 < chars.len() {
                        body.push(chars[j + 1]);
                        j += 2;
                        continue;
                    }
                    if d == c {
                        closed = true;
                        break;
                    }
                    if d == '\n' && c != '`' {
                        break;
                    }
                    body.push(d);
                    j += 1;
                }
                if closed {
                    out.push_str(&placeholder(&body));
                    i = j + 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            '/' if at(i, "//") => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if at(i, "/*") => {
                i += 2;
                while i < chars.len() && !at(i, "*/") {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
            }
            // `#` starts a comment, except Rust attributes `#[` and `#![` and C's `#include`.
            '#' if !at(i, "#[") && !at(i, "#![") && !at(i, "#include") => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// A Rust lifetime (`&'a`, `<'a`, `, 'b>`) at `i`, which a closing quote later on the
/// line must not turn into a string. A char literal (`'a'`) is not one.
fn is_lifetime(chars: &[char], i: usize) -> bool {
    let mut j = i + 1;
    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if j == i + 1 || !(chars[i + 1].is_alphabetic() || chars[i + 1] == '_') {
        return false;
    }
    let next = chars.get(j).copied();
    if next == Some('\'') {
        return false;
    }
    let prev = chars[..i]
        .iter()
        .rev()
        .find(|c| !c.is_whitespace())
        .copied();
    match i.checked_sub(1).map(|k| chars[k]) {
        Some('&') | Some('<') => true,
        _ => (prev == Some(',') && next == Some(':')) || next == Some('>'),
    }
}

fn placeholder(body: &str) -> String {
    format!("\"s{}\"", &sha256_hex(body)[..8])
}

fn count(re: &regex::Regex, s: &str) -> usize {
    re.find_iter(s).count()
}

/// Assertion calls, not mentions: `assert x`, `assert(`, `assertEqual(`, `assert_eq!(`,
/// `assert.Equal(`, `expect(`, `.should`, Go's `t.Error`, testify's `require.X(`.
fn assert_pattern() -> &'static regex::Regex {
    re!(
        r"\bassert\s|\bassert\w*\s*[(!]|\bassert\.\w+\s*\(|\bexpect\s*\(|\.should\b|\bt\.(Error|Errorf|Fatal|Fatalf)\(|\brequire\.\w+\(|\bself\.fail\("
    )
}

/// Test definitions: `def test_…(`, `#[test]`, `func Test…(`, `@Test`, `it(`, `test(`.
fn test_def_pattern() -> &'static regex::Regex {
    re!(
        r"\bdef\s+test\w*\s*\(|#\[(tokio::)?test\b|\bfunc\s+Test\w*\s*\(|@Test\b|(^|[^.\w])(it|test)(\.\w+)*\s*\("
    )
}

/// Assertions and test definitions in a text, comments and strings left out.
pub fn counts(text: &str) -> (usize, usize) {
    let code = code_only(text);
    (
        count(assert_pattern(), &code),
        count(test_def_pattern(), &code),
    )
}

/// The language of a file, for the rules that differ by test framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Python,
    Js,
    Rust,
    Go,
    Other,
}

pub fn lang_of(path: &str) -> Lang {
    let ext = path.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "py" | "pyi" => Lang::Python,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts" => Lang::Js,
        "rs" => Lang::Rust,
        "go" => Lang::Go,
        _ => Lang::Other,
    }
}

/// Compare the old and new text of an edit to a test file, language unknown.
pub fn assess(old: &str, new: &str) -> Answer {
    assess_in(Lang::Other, old, new)
}

/// Compare the old and new text of an edit to the test file `path`.
pub fn assess_path(path: &str, old: &str, new: &str) -> Answer {
    assess_in(lang_of(path), old, new)
}

fn assess_in(lang: Lang, raw_old: &str, raw_new: &str) -> Answer {
    let (old, new) = (code_only(raw_old), code_only(raw_new));
    let (old, new) = (old.as_str(), new.as_str());
    let assert_re = assert_pattern();
    let (a_old, a_new) = (count(assert_re, old), count(assert_re, new));
    // A file that did not exist, or was empty, held no test to weaken.
    if old.trim().is_empty() {
        return if a_new > 0 {
            Answer::Strengthened
        } else {
            Answer::Neutral
        };
    }
    let panic_re = re!(r"#\[should_panic\b");
    let true_re = re!(r"\|\|\s*true\b");
    let def_re = test_def_pattern();
    // A handler that turns a failed assertion into a pass.
    let swallow_re = re!(r"\bexcept\s*(\(\s*)?AssertionError\b|\bexcept\s*:");

    let mut reasons = Vec::new();
    if a_new < a_old {
        reasons.push(format!("assertions removed ({a_old} → {a_new})"));
    }
    let (d_old, d_new) = (count(def_re, old), count(def_re, new));
    if d_new < d_old {
        reasons.push(format!("tests removed or renamed ({d_old} → {d_new})"));
    }
    let existing = test_names(old);
    if skips(new, Some(&existing)) > skips(old, None) {
        reasons.push("skip marker added".to_string());
    }
    if focus(lang, new) > focus(lang, old) {
        reasons.push("focus marker added (the other tests stop running)".to_string());
    }
    if count(panic_re, new) > count(panic_re, old) {
        reasons.push("`#[should_panic]` added".to_string());
    }
    if count(true_re, new) > count(true_re, old) {
        reasons.push("`|| true` added".to_string());
    }
    if count(swallow_re, new) > count(swallow_re, old) {
        reasons.push("handler that swallows a failed assertion added".to_string());
    }
    if early_returns(new) > early_returns(old) {
        reasons.push("`return` before the assertions added".to_string());
    }
    let always = |s: &str| s.lines().filter(|l| always_passes(l.trim())).count();
    if always(new) > always(old) {
        reasons.push("assertion that always passes added".to_string());
    }
    if let Some(r) = looser_tolerance(old, new, &justified_lines(raw_new)) {
        reasons.push(r);
    }

    if !reasons.is_empty() {
        Answer::Weakened { reasons }
    } else if a_new > a_old {
        Answer::Strengthened
    } else {
        Answer::Neutral
    }
}

/// Names a code text defines: functions and named test calls (`it("a", …)` by its
/// string's placeholder).
fn test_names(code: &str) -> BTreeSet<String> {
    let def = re!(r"\b(?:def|fn|func|void|function)\s+(\w+)");
    let call = re!(r#"\b(?:it|test|describe)(?:\.\w+)*\s*\(\s*("s[0-9a-f]+")"#);
    def.captures_iter(code)
        .chain(call.captures_iter(code))
        .map(|c| c[1].to_string())
        .collect()
}

/// Unconditional skips in `code`. A skip under a runtime condition (`skipif`, `skipIf`,
/// `pytest.skip` in an `if` or `except` block) says the test cannot run here; it does
/// not disable it. With `existing` (the test names of the old text), a skip on a test
/// the edit adds does not count: it takes no running test away.
fn skips(code: &str, existing: Option<&BTreeSet<String>>) -> usize {
    let skip_re = re!(
        r"\bmark\.(skip|xfail)\b|\bpytest\.(skip|xfail)\(|@(unittest\.)?skip\b|\bexpectedFailure\b|\bskipTest\(|\bSkipTest\b|(^|[^.\w$])(it|test|describe)\.(skip|todo|fails)\b|(^|[^.\w$])x(it|describe|test)\(|#\[ignore\b|\bt\.Skip(Now|f)?\(|@(Disabled|Ignore)\b"
    );
    let lines: Vec<&str> = code.lines().collect();
    let mut n = 0;
    for (i, line) in lines.iter().enumerate() {
        for m in skip_re.find_iter(line) {
            if items::conditional(&lines, i, m.start()) {
                continue;
            }
            let owner = existing.and_then(|_| skip_owner(&lines, i, &line[m.start()..]));
            match (owner, existing) {
                (Some(o), Some(names)) if !names.contains(&o) => {}
                _ => n += 1,
            }
        }
    }
    n
}

/// The test a skip at line `i` applies to, when the text shows it: the definition
/// under a decorator or attribute, the named call (`it.skip("name", …)`), or the
/// definition the skip sits in.
fn skip_owner(lines: &[&str], i: usize, at: &str) -> Option<String> {
    let def = re!(r"\b(?:def|fn|func|void|function)\s+(\w+)");
    let line = lines[i];
    if let Some(c) = re!(r#"^(?:it|test|describe)\.(?:skip|todo|fails)\s*\(\s*("s[0-9a-f]+")|^x(?:it|describe|test)\(\s*("s[0-9a-f]+")"#)
        .captures(at.trim_start_matches(|c: char| !c.is_alphanumeric()))
    {
        return c.get(1).or(c.get(2)).map(|m| m.as_str().to_string());
    }
    let t = line.trim_start();
    if t.starts_with('@') || t.starts_with("#[") {
        return lines[i + 1..]
            .iter()
            .take(6)
            .find_map(|l| def.captures(l).map(|c| c[1].to_string()));
    }
    let ind = line.len() - t.len();
    lines[..i].iter().rev().find_map(|l| {
        let li = l.len() - l.trim_start().len();
        (li < ind && !l.trim().is_empty())
            .then(|| def.captures(l).map(|c| c[1].to_string()))
            .flatten()
    })
}

/// Focus markers: only the marked tests run, so the others stop running. Exact syntax
/// per framework: pytest has none but the pytest-only plugin's `@pytest.mark.only`;
/// jest, vitest, mocha and jasmine have `.only(` and `fit(` / `fdescribe(`; Rust and Go
/// have none.
fn focus(lang: Lang, code: &str) -> usize {
    let py = re!(r"(^|[^.\w])pytest\.mark\.only\b");
    let js =
        re!(r"(^|[^.\w$])((it|test|describe|context|suite)\.only\b|(fit|fdescribe|ftest)\s*\()");
    let defines = re!(r"\b(def|fn|func|function|void)\s+$");
    // `def fit(` defines a function; it focuses nothing.
    let js_count = || {
        js.find_iter(code)
            .filter(|m| {
                let s = &code[m.start()..];
                let skip = s.chars().next().filter(|c| !c.is_alphabetic());
                let at = m.start() + skip.map_or(0, char::len_utf8);
                !defines.is_match(&code[..at])
            })
            .count()
    };
    match lang {
        Lang::Python => count(py, code),
        Lang::Js => js_count(),
        Lang::Rust | Lang::Go => 0,
        Lang::Other => count(py, code) + js_count(),
    }
}

/// (actual, expected) values a failure message shows: pytest `assert 0 == 1`, unittest
/// `5 != 6`, jest `Expected: 6` / `Received: 5`, Go `got 5, want 6`, Rust `left: 5` /
/// `right: 6` (both orders: Rust does not say which side is expected).
pub fn value_pairs(message: &str) -> Vec<(String, String)> {
    macro_rules! lit {
        () => {
            r#"(-?[0-9][0-9._eE+-]*|'[^']{0,40}'|"[^"]{0,40}"|True|False|None|true|false|null)"#
        };
    }
    let mut out = Vec::new();
    for re in [
        re!(concat!(r"\bassert ", lit!(), " == ", lit!())),
        re!(concat!(r"(?:^|\s)", lit!(), " != ", lit!())),
        re!(concat!(r"\bgot:? ", lit!(), ",? want:? ", lit!())),
    ] {
        if let Some(c) = re.captures(message) {
            out.push((c[1].to_string(), c[2].to_string()));
        }
    }
    let field = |re: &regex::Regex| re.captures(message).map(|c| c[1].to_string());
    let (expected, received) = (
        field(re!(concat!(r"\bExpected:\s*", lit!()))),
        field(re!(concat!(r"\bReceived:\s*", lit!()))),
    );
    if let (Some(e), Some(r)) = (expected, received) {
        out.push((r, e));
    }
    let (left, right) = (
        field(re!(concat!(r"\bleft:\s*", lit!()))),
        field(re!(concat!(r"\bright:\s*", lit!()))),
    );
    if let (Some(l), Some(r)) = (left, right) {
        out.push((l.clone(), r.clone()));
        out.push((r, l));
    }
    out.retain(|(a, e)| a != e);
    out
}

/// The edit changes an expected value to the actual value of a recent failure, so the
/// test now agrees with the bug (reference Phase 3; I6 E4). `pairs`: (actual, expected).
pub fn expected_to_actual(old: &str, new: &str, pairs: &[(String, String)]) -> Option<String> {
    let (old, new) = (code_only_keep_strings(old), code_only_keep_strings(new));
    let count = |text: &str, lit: &str| {
        regex::Regex::new(&format!(r"(^|[^\w.]){}($|[^\w.])", regex::escape(lit)))
            .map(|r| r.find_iter(text).count())
            .unwrap_or(0)
    };
    pairs.iter().find_map(|(actual, expected)| {
        let fewer_expected = count(&new, expected) < count(&old, expected);
        let more_actual = count(&new, actual) > count(&old, actual);
        (fewer_expected && more_actual).then(|| {
            format!(
                "expected value changed to the failing run's actual value ({expected} → {actual})"
            )
        })
    })
}

/// Code without comments, strings kept as written (literals are what E4 compares).
fn code_only_keep_strings(text: &str) -> String {
    text.lines()
        .map(|l| {
            let l = l.split(" # ").next().unwrap_or(l);
            l.split(" // ").next().unwrap_or(l)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An assertion that cannot fail: `assert True`, `assert True, "msg"`, `assert 1 == 1`,
/// `self.assertTrue(True)`, `assert!(true)`, `assert_eq!(1, 1)`, `expect(1).toBe(1)`.
fn always_passes(line: &str) -> bool {
    let literal_true = re!(
        r"^assert\s*\(?\s*(True|true|1|not\s+False)\s*\)?\s*(,.*)?;?$|\bassert(True|_true)?\s*\(\s*(True|true|1)\s*(,[^)]*)?\)|\bassert!\(\s*true\s*(,[^)]*)?\)|\bexpect\(\s*true\s*\)\.toBe(Truthy)?\(\s*(true)?\s*\)"
    );
    if literal_true.is_match(line) {
        return true;
    }
    // The same expression on both sides. The regex crate has no back-references.
    let pairs = [
        re!(r"^assert\s+\(?(.+?)\s*==\s*(.+?)\)?\s*(,.*)?;?$"),
        re!(
            r"\bassert(?:_eq!|Equals?|\.Equal)\s*\(\s*(?:t\s*,\s*)?([^,]+?)\s*,\s*([^,)]+?)\s*[,)]"
        ),
        re!(r"\bexpect\(\s*(.+?)\s*\)\.(?:toBe|toEqual|toStrictEqual)\(\s*(.+?)\s*\)"),
    ];
    pairs.iter().any(|re| {
        re.captures(line)
            .is_some_and(|c| c[1].trim() == c[2].trim() && !c[1].trim().is_empty())
    })
}

/// Bare `return`s followed by an assertion within the next five code lines: the
/// assertions after it no longer run.
fn early_returns(code: &str) -> usize {
    let lines: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let assert_line = re!(r"\bassert|\bexpect\s*\(|\bt\.(Error|Fatal)");
    lines
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            re!(r"^return\s*;?$").is_match(l)
                && lines[i + 1..]
                    .iter()
                    .take(5)
                    .take_while(|n| !re!(r"^(def|fn|func|it|test)\b|^\}").is_match(n))
                    .any(|n| assert_line.is_match(n))
        })
        .count()
}

/// An assertion whose tolerance grew (`rel=1e-6` → `rel=0.1`) or whose digits argument
/// shrank (`toBeCloseTo(x, 5)` → `toBeCloseTo(x, 1)`, `places=7` → `2`). Assertions are
/// paired by their text with the numbers taken out, so a new assertion with a wide
/// tolerance is not a loosened old one (whole files, as Write and Bash writes give).
/// `a` without one copy of each value in `b`.
fn minus(a: &[f64], b: &[f64]) -> Vec<f64> {
    let mut rest: Vec<f64> = b.to_vec();
    a.iter()
        .filter(|x| match rest.iter().position(|y| y == *x) {
            Some(i) => {
                rest.swap_remove(i);
                false
            }
            None => true,
        })
        .copied()
        .collect()
}

/// A loosened tolerance or time limit counts only when its line, or the comment line
/// just above it, says nothing about why (the audit treats a suppression with a reason
/// the same way). A tolerance must also grow at least tenfold, or become vacuous (a
/// relative tolerance of 1 or more): a 2× tweak is tuning, not weakening.
fn looser_tolerance(old: &str, new: &str, justified: &BTreeSet<String>) -> Option<String> {
    let tol = re!(
        r"\b(rel|abs|rel_tol|abs_tol|atol|rtol|delta|epsilon|tol|tolerance)\s*=\s*([0-9][0-9._eE+-]*)"
    );
    let digits = re!(r"\b(toBeCloseTo)\([^,()]+,\s*(\d+)\s*\)|\b(places)\s*=\s*(\d+)");
    let num = |s: &str| s.replace('_', "").parse::<f64>().ok();
    // (line with numbers masked, keyword) → value. A digits argument is stored negated:
    // fewer digits is looser, so "grew" means looser for both.
    let values = |text: &str| {
        let mut m: std::collections::BTreeMap<(String, String), Vec<f64>> = Default::default();
        for line in text.lines() {
            let masked = mask_numbers(line.trim());
            for c in tol.captures_iter(line) {
                if let Some(v) = num(&c[2]) {
                    m.entry((masked.clone(), c[1].to_string()))
                        .or_default()
                        .push(v);
                }
            }
            // A time limit on a timed assertion (`elapsed < 0.2`): raising it loosens the
            // test the same way (found by the benchmark's `perf` task).
            if re!(r"perf_counter|monotonic|time\(\)|elapsed|duration|took|seconds|timeout")
                .is_match(line)
            {
                if let Some(c) = re!(r"<=?\s*([0-9][0-9._eE+-]*)").captures(line) {
                    if let Some(v) = num(&c[1]) {
                        m.entry((masked.clone(), "time limit".to_string()))
                            .or_default()
                            .push(v);
                    }
                }
            }
            for c in digits.captures_iter(line) {
                let (k, v) = match (c.get(1), c.get(2), c.get(3), c.get(4)) {
                    (Some(k), Some(v), _, _) | (_, _, Some(k), Some(v)) => (k, v),
                    _ => continue,
                };
                if let Some(v) = num(v.as_str()) {
                    m.entry((masked.clone(), k.as_str().to_string()))
                        .or_default()
                        .push(-v);
                }
            }
        }
        m
    };
    // Loosened: a value of the old text is gone and a larger one took its place. An old
    // assertion that stays, next to a new one with a wider tolerance, is not loosened.
    let (o, n) = (values(old), values(new));
    n.iter().find_map(|(key, nvs)| {
        if justified.contains(&key.0) {
            return None;
        }
        let ovs = o.get(key)?;
        let gone: Vec<f64> = minus(ovs, nvs);
        let came: Vec<f64> = minus(nvs, ovs);
        let ov = gone.iter().copied().fold(f64::NAN, f64::max);
        let nv = came.iter().copied().fold(f64::NAN, f64::max);
        let tolerance = nv > 0.0 && key.1 != "time limit";
        let relative = matches!(key.1.as_str(), "rel" | "rtol" | "rel_tol");
        let enough =
            !tolerance || ov <= 0.0 || nv >= 10.0 * ov * (1.0 - 1e-9) || (relative && nv >= 1.0);
        (nv > ov && enough).then(|| {
            if nv <= 0.0 {
                format!("precision lowered ({} → {} digits)", -ov, -nv)
            } else {
                if key.1 == "time limit" {
                    format!("time limit raised ({ov} → {nv})")
                } else {
                    format!("tolerance raised ({}: {ov} → {nv})", key.1)
                }
            }
        })
    })
}

/// Code lines (numbers masked, as `looser_tolerance` keys them) that carry a comment,
/// or follow a comment-only line: a change there comes with its reason.
fn justified_lines(raw: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut prev_comment = false;
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let only = (t.starts_with('#') && !t.starts_with("#[")) || t.starts_with("//");
        if !only && (prev_comment || has_comment(line)) {
            let code = code_only(line);
            out.insert(mask_numbers(code.trim()));
        }
        prev_comment = only;
    }
    out
}

fn mask_numbers(line: &str) -> String {
    re!(r"[0-9][0-9._eE+-]*").replace_all(line, "#").to_string()
}

/// A `#` or `//` comment outside quotes on this line.
fn has_comment(line: &str) -> bool {
    let b = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(_) if c == b'\\' => i += 1,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' || c == b'`' => quote = Some(c),
            None if c == b'#' && b.get(i + 1) != Some(&b'[') => return true,
            None if c == b'/' && b.get(i + 1) == Some(&b'/') => return true,
            None => {}
        }
        i += 1;
    }
    false
}

/// The rule behind a guard reason, as a stable name: the user's allows count per rule
/// (U6.3), and `guard:<rule>` names it in the ledger. Reasons come from `assess`,
/// `expected_to_actual`, and the protected-path check.
pub fn rule_of(reason: &str) -> &'static str {
    const RULES: [(&str, &str); 14] = [
        ("assertions removed", "assertions_removed"),
        ("tests removed", "tests_removed"),
        ("skip marker", "skip"),
        ("focus marker", "focus"),
        ("`#[should_panic]`", "should_panic"),
        ("`|| true`", "or_true"),
        ("handler that swallows", "swallow"),
        ("`return` before", "early_return"),
        ("assertion that always passes", "always_passes"),
        ("precision lowered", "tolerance"),
        ("time limit raised", "tolerance"),
        ("tolerance raised", "tolerance"),
        ("expected value changed", "expected_to_actual"),
        ("the path is protected", "protected"),
    ];
    RULES
        .iter()
        .find(|(prefix, _)| reason.starts_with(prefix))
        .map_or("other", |(_, rule)| rule)
}

pub mod items;

#[cfg(test)]
mod tests;
