//! Tokens carry their column: TLA+'s `/\` and `\/` bullet lists are
//! layout-sensitive, and the parser ends a list item at any token whose
//! column is at or left of the bullet.

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Num(i64),
    Str(String),
    Op(&'static str),
    /// A `----` line (four or more dashes).
    Sep,
    /// A `====` line (four or more equals signs).
    End,
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    pub col: u32,
}

/// Symbolic operators, longest first so a prefix never wins.
const SYMBOLS: &[&str] = &[
    "<=>", "|->", "...", "==", "=>", "=<", "<=", ">=", "/=", "/\\", "<<", ">>", "->", "<-", "[]",
    // user-definable infix operators
    "^^", "++", "**", "//", "&&", "%%", "##", "$$",
    "<>", "~>", "..", "::", ":>", "@@", "=", "#", "<", ">", ":", "@", "'", "(", ")", "[", "]", "{",
    "}", ",", ".", "+", "-", "*", "^", "%", "~", "!", "|", "&", "$", "?",
];

/// `\word` operators, mapped to their canonical spelling.
fn backslash_word(w: &str) -> Option<&'static str> {
    Some(match w {
        "in" => "\\in",
        "notin" => "\\notin",
        "cup" | "union" => "\\cup",
        "cap" | "intersect" => "\\cap",
        "subseteq" => "\\subseteq",
        "subset" => "\\subset",
        "supseteq" => "\\supseteq",
        "A" => "\\A",
        "E" => "\\E",
        "AA" => "\\AA",
        "EE" => "\\EE",
        "X" | "times" => "\\X",
        "o" | "circ" => "\\o",
        "div" => "\\div",
        "land" => "/\\",
        "lor" => "\\/",
        "lnot" | "neg" => "~",
        "equiv" => "<=>",
        "leq" => "<=",
        "geq" => ">=",
        // user-definable infix operators
        "prec" => "\\prec",
        "preceq" => "\\preceq",
        "succ" => "\\succ",
        "succeq" => "\\succeq",
        "ll" => "\\ll",
        "gg" => "\\gg",
        "sqsubset" => "\\sqsubset",
        "sqsubseteq" => "\\sqsubseteq",
        "sqsupset" => "\\sqsupset",
        "sqsupseteq" => "\\sqsupseteq",
        "sim" => "\\sim",
        "simeq" => "\\simeq",
        "approx" => "\\approx",
        "cong" => "\\cong",
        "doteq" => "\\doteq",
        "oplus" => "\\oplus",
        "ominus" => "\\ominus",
        "otimes" => "\\otimes",
        "odot" => "\\odot",
        "oslash" => "\\oslash",
        "uplus" => "\\uplus",
        "sqcap" => "\\sqcap",
        "sqcup" => "\\sqcup",
        "star" => "\\star",
        "bullet" => "\\bullet",
        _ => return None,
    })
}

/// A module file: only the text from the `---- MODULE` line to the first
/// `====` line after it is TLA+; prose before and after is ignored, as by
/// SANY. The rest is blanked so line numbers stay true.
pub fn lex_module(src: &str) -> Result<Vec<Token>, String> {
    let lines: Vec<&str> = src.split('\n').collect();
    let is_head = |l: &str| {
        let t = l.trim_start();
        t.starts_with("----") && t.trim_start_matches('-').trim_start().starts_with("MODULE")
    };
    let Some(start) = lines.iter().position(|l| is_head(l)) else { return lex(src) };
    let end = lines.iter().skip(start + 1).position(|l| l.trim_start().starts_with("====")).map(|k| k + start + 1);
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| if i < start || end.is_some_and(|e| i > e) { "" } else { *l })
        .collect();
    lex(&kept.join("\n"))
}

pub fn lex(src: &str) -> Result<Vec<Token>, String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut line, mut line_start) = (0usize, 1u32, 0usize);
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            i += 1;
            line += 1;
            line_start = i;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let col = (i - line_start) as u32;
        let push = |out: &mut Vec<Token>, tok| out.push(Token { tok, line, col });
        // Comments.
        if c == b'\\' && b.get(i + 1) == Some(&b'*') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'(' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0;
            while i < b.len() {
                if b[i] == b'(' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b')') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if b[i] == b'\n' {
                        line += 1;
                        line_start = i + 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        // Separator and end lines.
        if c == b'-' || c == b'=' {
            let mut j = i;
            while j < b.len() && b[j] == c {
                j += 1;
            }
            if j - i >= 4 {
                push(&mut out, if c == b'-' { Tok::Sep } else { Tok::End });
                i = j;
                continue;
            }
        }
        if c.is_ascii_digit() {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            // An identifier may start with digits (e.g. `1st`): take it whole.
            if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                push(&mut out, Tok::Ident(src[i..j].to_string()));
            } else {
                let n = src[i..j].parse().map_err(|e| format!("line {line}: {e}"))?;
                push(&mut out, Tok::Num(n));
            }
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            push(&mut out, Tok::Ident(src[i..j].to_string()));
            i = j;
            continue;
        }
        if c == b'"' {
            let mut j = i + 1;
            let mut s = String::new();
            while j < b.len() && b[j] != b'"' {
                if b[j] == b'\\' && j + 1 < b.len() {
                    j += 1;
                    s.push(match b[j] {
                        b'n' => '\n',
                        b't' => '\t',
                        o => o as char,
                    });
                } else {
                    s.push(b[j] as char);
                }
                j += 1;
            }
            push(&mut out, Tok::Str(s));
            i = j + 1;
            continue;
        }
        if c == b'\\' {
            if b.get(i + 1) == Some(&b'/') {
                push(&mut out, Tok::Op("\\/"));
                i += 2;
                continue;
            }
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j > i + 1 {
                let w = &src[i + 1..j];
                match backslash_word(w) {
                    Some(op) => push(&mut out, Tok::Op(op)),
                    None => return Err(format!("line {line}: unsupported operator \\{w}")),
                }
                i = j;
            } else {
                push(&mut out, Tok::Op("\\"));
                i += 1;
            }
            continue;
        }
        match SYMBOLS.iter().find(|s| src[i..].starts_with(**s)) {
            Some(s) => {
                push(&mut out, Tok::Op(s));
                i += s.len();
            }
            None => return Err(format!("line {line}: unexpected character {:?}", c as char)),
        }
    }
    out.push(Token { tok: Tok::Eof, line: line + 1, col: 0 });
    Ok(out)
}
