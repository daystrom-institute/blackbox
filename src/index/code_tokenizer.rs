use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

pub const CODE_TOKENIZER_NAME: &str = "code";

#[derive(Clone, Default)]
pub struct CodeTokenizer {
    token: Token,
}

pub struct CodeTokenStream {
    tokens: Vec<Token>,
    idx: usize,
    current: Token,
}

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = CodeTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        self.token.reset();
        CodeTokenStream {
            tokens: code_tokens(text),
            idx: 0,
            current: self.token.clone(),
        }
    }
}

impl TokenStream for CodeTokenStream {
    fn advance(&mut self) -> bool {
        let Some(token) = self.tokens.get(self.idx).cloned() else {
            return false;
        };
        self.idx += 1;
        self.current = token;
        true
    }

    fn token(&self) -> &Token {
        &self.current
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}

fn code_tokens(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    for (raw, start, end) in raw_code_terms(text) {
        push_unique_token(&mut tokens, raw.to_string(), start, end);
        for (part, part_start, part_end) in delimiter_parts(raw, start) {
            push_unique_token(&mut tokens, part.to_string(), part_start, part_end);
            for (camel, camel_start, camel_end) in camel_parts(part, part_start) {
                push_unique_token(&mut tokens, camel.to_string(), camel_start, camel_end);
            }
        }
    }
    for (position, token) in tokens.iter_mut().enumerate() {
        token.position = position;
    }
    tokens
}

fn raw_code_terms(text: &str) -> Vec<(&str, usize, usize)> {
    let mut terms = Vec::new();
    let mut start = None;
    for (idx, ch) in text.char_indices() {
        if is_code_char(ch) {
            start.get_or_insert(idx);
        } else if let Some(from) = start.take() {
            terms.push((&text[from..idx], from, idx));
        }
    }
    if let Some(from) = start {
        terms.push((&text[from..], from, text.len()));
    }
    terms
}

fn delimiter_parts(term: &str, offset: usize) -> Vec<(&str, usize, usize)> {
    let mut parts = Vec::new();
    let mut start = None;
    for (idx, ch) in term.char_indices() {
        if matches!(ch, '_' | ':' | '.' | '>') {
            if let Some(from) = start.take() {
                parts.push((&term[from..idx], offset + from, offset + idx));
            }
        } else {
            start.get_or_insert(idx);
        }
    }
    if let Some(from) = start {
        parts.push((&term[from..], offset + from, offset + term.len()));
    }
    parts
}

fn camel_parts(term: &str, offset: usize) -> Vec<(&str, usize, usize)> {
    let chars: Vec<(usize, char)> = term.char_indices().collect();
    if chars.len() < 2 {
        return Vec::new();
    }
    let mut starts = vec![0usize];
    for idx in 1..chars.len() {
        let prev = chars[idx - 1].1;
        let current = chars[idx].1;
        let next = chars.get(idx + 1).map(|(_, ch)| *ch);
        if current.is_ascii_uppercase()
            && (prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next.is_some_and(|ch| ch.is_ascii_lowercase())))
        {
            starts.push(chars[idx].0);
        }
    }
    if starts.len() == 1 {
        return Vec::new();
    }
    starts.push(term.len());
    starts
        .windows(2)
        .filter_map(|pair| {
            let from = pair[0];
            let to = pair[1];
            (from < to).then_some((&term[from..to], offset + from, offset + to))
        })
        .collect()
}

fn push_unique_token(tokens: &mut Vec<Token>, text: String, offset_from: usize, offset_to: usize) {
    if text.is_empty()
        || tokens
            .iter()
            .any(|token| token.text == text && token.offset_from == offset_from)
    {
        return;
    }
    tokens.push(Token {
        offset_from,
        offset_to,
        position: 0,
        text,
        position_length: 1,
    });
    let lower = tokens
        .last()
        .expect("just pushed token")
        .text
        .to_lowercase();
    if lower != tokens.last().expect("just pushed token").text
        && !tokens
            .iter()
            .any(|token| token.text == lower && token.offset_from == offset_from)
    {
        tokens.push(Token {
            offset_from,
            offset_to,
            position: 0,
            text: lower,
            position_length: 1,
        });
    }
}

fn is_code_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':' | '.' | '>')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(input: &str) -> Vec<String> {
        code_tokens(input)
            .into_iter()
            .map(|token| token.text)
            .collect()
    }

    #[test]
    fn splits_common_identifier_shapes_and_keeps_originals() {
        let tokens = texts("KnowledgeStore bbox_project_register std::collections::HashMap");
        for expected in [
            "KnowledgeStore",
            "Knowledge",
            "Store",
            "bbox_project_register",
            "bbox",
            "project",
            "register",
            "std::collections::HashMap",
            "std",
            "collections",
            "HashMap",
            "Hash",
            "Map",
        ] {
            assert!(tokens.iter().any(|token| token == expected), "{expected}");
        }
    }
}
