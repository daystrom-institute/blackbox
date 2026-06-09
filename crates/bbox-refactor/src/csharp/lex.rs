//! Small C# byte-level lex utilities shared across csharp_* plan kinds.
//!
//! These are deliberately minimal — enough to skip strings / chars /
//! verbatim strings / comments while walking braces, identifiers, and
//! keywords. Tree-sitter parsing happens via the validation step
//! (`ValidationStep::TreeSitterNoErrors`); these helpers exist so the
//! plan-time scans don't need a full parser dependency just to find a
//! `class` keyword boundary.

pub(crate) fn is_word_boundary(bytes: &[u8], i: usize) -> bool {
    i == 0 || !is_ident_char(bytes[i - 1])
}

pub(crate) fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub(crate) fn match_keyword(bytes: &[u8], at: usize, kw: &[u8]) -> Option<usize> {
    if at + kw.len() > bytes.len() {
        return None;
    }
    if &bytes[at..at + kw.len()] != kw {
        return None;
    }
    let after = at + kw.len();
    if after < bytes.len() && is_ident_char(bytes[after]) {
        return None;
    }
    Some(after)
}

pub(crate) fn skip_whitespace(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

pub(crate) fn read_ident(bytes: &[u8], from: usize) -> (String, usize) {
    let mut i = from;
    while i < bytes.len() && is_ident_char(bytes[i]) {
        i += 1;
    }
    (
        std::str::from_utf8(&bytes[from..i])
            .unwrap_or("")
            .to_string(),
        i,
    )
}

pub(crate) fn skip_balanced(bytes: &[u8], from: usize, open: u8, close: u8) -> Option<usize> {
    if bytes.get(from) != Some(&open) {
        return None;
    }
    let mut depth: i32 = 1;
    let mut i = from + 1;
    while i < bytes.len() {
        if bytes[i] == open {
            depth += 1;
        } else if bytes[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn find_matching_close_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut i = open + 1;
    let mut depth: i32 = 1;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
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

/// If `bytes[i]` begins a C# "lex atom" (string, char, verbatim
/// string, line comment, block comment), return the byte offset
/// immediately past its end. Otherwise return `None`.
pub(crate) fn skip_lex_atom(bytes: &[u8], i: usize) -> Option<usize> {
    let b = bytes.get(i).copied()?;
    match b {
        b'"' => {
            let mut j = i + 1;
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' => j += 2,
                    b'"' => return Some(j + 1),
                    b'\n' => return Some(j + 1),
                    _ => j += 1,
                }
            }
            Some(bytes.len())
        }
        b'\'' => {
            let mut j = i + 1;
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' => j += 2,
                    b'\'' => return Some(j + 1),
                    b'\n' => return Some(j + 1),
                    _ => j += 1,
                }
            }
            Some(bytes.len())
        }
        b'@' if bytes.get(i + 1) == Some(&b'"') => {
            let mut j = i + 2;
            while j < bytes.len() {
                if bytes[j] == b'"' {
                    if bytes.get(j + 1) == Some(&b'"') {
                        j += 2;
                        continue;
                    }
                    return Some(j + 1);
                }
                j += 1;
            }
            Some(bytes.len())
        }
        b'/' => match bytes.get(i + 1).copied() {
            Some(b'/') => {
                let mut j = i + 2;
                while j < bytes.len() && bytes[j] != b'\n' {
                    j += 1;
                }
                Some(j)
            }
            Some(b'*') => {
                let mut j = i + 2;
                while j + 1 < bytes.len() {
                    if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                        return Some(j + 2);
                    }
                    j += 1;
                }
                Some(bytes.len())
            }
            _ => None,
        },
        _ => None,
    }
}
