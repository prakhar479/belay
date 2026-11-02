//! Function and test items in a text, with their spans and bodies: what moved between
//! files, which test owns a skip, and whether a skip runs only under a condition.
//!
//! Items are found on the raw text, so a span can be cut out of it; a definition counts
//! only when it is still there once comments and strings are gone.

use std::collections::BTreeSet;
use std::ops::Range;

use super::code_only;
use crate::text::sha256_hex;

/// A function or test definition: its name, a hash of its code (whitespace collapsed,
/// comments left out), and its byte span in the text, attributes and decorators included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub hash: String,
    pub span: Range<usize>,
}

impl Item {
    /// The same item in another file: same name, same body.
    pub fn key(&self) -> (String, String) {
        (self.name.clone(), self.hash.clone())
    }
}

/// Lines with their byte offsets.
fn lines(text: &str) -> Vec<(usize, &str)> {
    let mut at = 0;
    text.split_inclusive('\n')
        .map(|l| {
            let start = at;
            at += l.len();
            (start, l)
        })
        .collect()
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Every definition in `text`, outermost and nested alike.
pub fn items(text: &str) -> Vec<Item> {
    let ls = lines(text);
    let mut out = Vec::new();
    for (i, &(start, line)) in ls.iter().enumerate() {
        let code = code_only(line);
        // Python: `def name(`; the body is what is indented deeper.
        if let Some(c) = re!(r"^(\s*)(?:async\s+)?def\s+(\w+)\s*\(").captures(&code) {
            let ind = indent(line);
            let end = ls[i + 1..]
                .iter()
                .find(|(_, l)| !l.trim().is_empty() && indent(l) <= ind)
                .map_or(text.len(), |(s, _)| *s);
            let first = decorated_from(&ls, i, |l| l.trim_start().starts_with('@'));
            out.push(item(text, c[2].to_string(), first..end));
            continue;
        }
        // JS: `it('name', …)`, `test(…)`, `describe(…)`: the call is the item.
        if let Some(c) =
            re!(r#"^\s*(?:it|test|describe)(?:\.\w+)*\s*\(\s*(['"`])([^'"`]*)['"`]"#).captures(line)
        {
            if code.trim_start().starts_with(|ch: char| ch.is_alphabetic()) {
                let open = start + c.get(1).map_or(0, |m| m.start()) - 1;
                let open = text[..=open].rfind('(').unwrap_or(open);
                if let Some(close) = closing(text, open, '(', ')', true) {
                    out.push(item(text, c[2].to_string(), start..line_end(text, close)));
                }
            }
            continue;
        }
        // Brace languages: Rust `fn`, Go `func`, JS `function`, Java methods after
        // `@Test`. Attributes and annotations above belong to the item.
        let named = re!(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|func|function)\s+(?:\([^)]*\)\s*)?(\w+)")
            .captures(&code)
            .map(|c| c[1].to_string())
            .or_else(|| {
                let prev = ls[..i].iter().rev().find(|(_, l)| !l.trim().is_empty())?;
                prev.1.trim().starts_with("@Test").then(|| {
                    re!(r"(\w+)\s*\(")
                        .captures(&code)
                        .map(|c| c[1].to_string())
                })?
            });
        let Some(name) = named else { continue };
        let Some(open) = text[start..].find('{').map(|o| start + o) else {
            continue;
        };
        // A declaration without a body (`fn f();`) ends before any brace.
        if text[start..open].contains(';') {
            continue;
        }
        let quotes = !re!(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\b").is_match(&code);
        if let Some(close) = closing(text, open, '{', '}', quotes) {
            let first = decorated_from(&ls, i, |l| {
                let t = l.trim_start();
                t.starts_with("#[") || t.starts_with('@')
            });
            out.push(item(text, name, first..line_end(text, close)));
        }
    }
    out
}

fn item(text: &str, name: String, span: Range<usize>) -> Item {
    let body = code_only(&text[span.clone()]);
    let norm = body.split_whitespace().collect::<Vec<_>>().join(" ");
    Item {
        name,
        hash: sha256_hex(&norm)[..16].to_string(),
        span,
    }
}

/// The offset of the first of the lines just above line `i` that `is_attr` accepts
/// (decorators, attributes), or of line `i` itself.
fn decorated_from(ls: &[(usize, &str)], i: usize, is_attr: impl Fn(&str) -> bool) -> usize {
    let mut first = i;
    while first > 0 && is_attr(ls[first - 1].1) {
        first -= 1;
    }
    ls[first].0
}

/// The end of the line holding byte `at` (after its newline).
fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |n| at + n + 1)
}

/// The byte offset of the bracket closing the one at `open`. Double-quoted and
/// backtick strings and `//` comments are skipped; single quotes too where `quotes`
/// (Rust uses `'` for lifetimes).
fn closing(text: &str, open: usize, o: char, c: char, quotes: bool) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        let ch = b[i] as char;
        match ch {
            '"' | '`' | '\'' if ch != '\'' || quotes => {
                i += 1;
                while i < b.len() && b[i] as char != ch {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            _ if ch == o => depth += 1,
            _ if ch == c => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `text` without the spans of `items` whose key is in `keys`.
pub fn strip(text: &str, keys: &BTreeSet<(String, String)>) -> String {
    let mut spans: Vec<Range<usize>> = items(text)
        .into_iter()
        .filter(|it| keys.contains(&it.key()))
        .map(|it| it.span)
        .collect();
    spans.sort_by_key(|s| s.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for s in spans {
        if s.start >= at {
            out.push_str(&text[at..s.start]);
        }
        at = at.max(s.end);
    }
    out.push_str(&text[at.min(text.len())..]);
    out
}

/// The skip at line `i` of `code` runs only under a condition: it sits in an `if`,
/// `else`, `except`, `try` or `catch` block (by indentation in Python, by braces
/// elsewhere), or its own line starts with one.
pub fn conditional(code_lines: &[&str], i: usize, col: usize) -> bool {
    let branch = re!(r"^\s*(?:\}\s*)?(if|elif|else|except|try|catch|switch|case)\b");
    let line = code_lines[i];
    if branch.is_match(line) || re!(r"\b(if|else|catch)\b").is_match(&line[..col.min(line.len())]) {
        return true;
    }
    let stop = re!(r"^\s*(?:async\s+)?(def|class|fn|func|function)\b|^\s*(?:pub\s+)?fn\b");
    // Python: the enclosing blocks are the lines above with less indentation.
    let mut ind = indent(line);
    for l in code_lines[..i].iter().rev() {
        if l.trim().is_empty() || indent(l) >= ind {
            continue;
        }
        ind = indent(l);
        if stop.is_match(l) {
            break;
        }
        if branch.is_match(l) && l.trim_end().ends_with(':') {
            return true;
        }
        if ind == 0 {
            break;
        }
    }
    // Braces: the lines that opened a block still open at the skip.
    let mut depth = 0i32;
    for l in code_lines[..i].iter().rev() {
        for ch in l.chars().rev() {
            match ch {
                '}' => depth += 1,
                '{' if depth > 0 => depth -= 1,
                '{' => {
                    if stop.is_match(l) || re!(r"=>\s*\{\s*$|\b(it|test|describe)\s*\(").is_match(l)
                    {
                        return false;
                    }
                    if re!(r"\b(if|else|catch|switch|case)\b").is_match(l) {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}
