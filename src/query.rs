#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryToken {
    Term(String),
    Phrase(String),
    And,
    Or,
    Not,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAtom {
    pub text: String,
    pub phrase: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryNode {
    Atom(QueryAtom),
    And(Box<QueryNode>, Box<QueryNode>),
    Or(Box<QueryNode>, Box<QueryNode>),
    Not(Box<QueryNode>),
}

pub fn lex_query(raw: &str) -> Vec<QueryToken> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        if chars[i] == '-' {
            out.push(QueryToken::Not);
            i += 1;
            continue;
        }

        if chars[i] == '"' {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            let phrase = chars[start..i]
                .iter()
                .collect::<String>()
                .trim()
                .to_string();
            if !phrase.is_empty() {
                out.push(QueryToken::Phrase(phrase));
            }
            if i < chars.len() && chars[i] == '"' {
                i += 1;
            }
            continue;
        }

        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let token = chars[start..i].iter().collect::<String>();
        match token.to_ascii_uppercase().as_str() {
            "AND" => out.push(QueryToken::And),
            "OR" => out.push(QueryToken::Or),
            "NOT" => out.push(QueryToken::Not),
            _ if !token.is_empty() => out.push(QueryToken::Term(token)),
            _ => {}
        }
    }

    out
}

pub fn parse_query(raw: &str) -> Option<QueryNode> {
    let raw_tokens = lex_query(raw);
    let mut tokens = Vec::new();
    let mut exclusions = Vec::new();
    let mut i = 0;

    while i < raw_tokens.len() {
        if matches!(raw_tokens[i], QueryToken::Not)
            && i + 1 < raw_tokens.len()
            && matches!(
                raw_tokens[i + 1],
                QueryToken::Term(_) | QueryToken::Phrase(_)
            )
        {
            exclusions.push(raw_tokens[i + 1].clone());
            i += 2;
            continue;
        }
        tokens.push(raw_tokens[i].clone());
        i += 1;
    }

    if tokens.is_empty() {
        return None;
    }
    let mut pos = 0;
    let mut node = parse_or(&tokens, &mut pos)?;
    for exclusion in exclusions {
        let atom = match exclusion {
            QueryToken::Term(text) => QueryNode::Atom(QueryAtom {
                text: text.to_lowercase(),
                phrase: false,
            }),
            QueryToken::Phrase(text) => QueryNode::Atom(QueryAtom {
                text: text.to_lowercase(),
                phrase: true,
            }),
            _ => continue,
        };
        node = QueryNode::And(Box::new(node), Box::new(QueryNode::Not(Box::new(atom))));
    }
    Some(node)
}

fn parse_or(tokens: &[QueryToken], pos: &mut usize) -> Option<QueryNode> {
    let mut node = parse_and(tokens, pos)?;
    while *pos < tokens.len() {
        if matches!(tokens[*pos], QueryToken::Or) {
            *pos += 1;
            let rhs = parse_and(tokens, pos)?;
            node = QueryNode::Or(Box::new(node), Box::new(rhs));
            continue;
        }
        if matches!(tokens[*pos], QueryToken::Term(_) | QueryToken::Phrase(_)) {
            let rhs = parse_and(tokens, pos)?;
            node = QueryNode::Or(Box::new(node), Box::new(rhs));
            continue;
        }
        break;
    }
    Some(node)
}

fn parse_and(tokens: &[QueryToken], pos: &mut usize) -> Option<QueryNode> {
    let mut node = parse_primary(tokens, pos)?;
    while *pos < tokens.len() {
        if !matches!(tokens[*pos], QueryToken::And) {
            break;
        }
        *pos += 1;
        let rhs = parse_primary(tokens, pos)?;
        node = QueryNode::And(Box::new(node), Box::new(rhs));
    }
    Some(node)
}

fn parse_primary(tokens: &[QueryToken], pos: &mut usize) -> Option<QueryNode> {
    if *pos >= tokens.len() {
        return None;
    }
    let node = match &tokens[*pos] {
        QueryToken::Term(text) => QueryNode::Atom(QueryAtom {
            text: text.to_lowercase(),
            phrase: false,
        }),
        QueryToken::Phrase(text) => QueryNode::Atom(QueryAtom {
            text: text.to_lowercase(),
            phrase: true,
        }),
        _ => return None,
    };
    *pos += 1;
    Some(node)
}

fn render_atom(atom: &QueryAtom) -> String {
    if atom.phrase {
        format!("\"{}\"", atom.text.replace('"', "\\\""))
    } else {
        atom.text.clone()
    }
}

fn render_query(node: &QueryNode) -> String {
    match node {
        QueryNode::Atom(atom) => render_atom(atom),
        QueryNode::And(lhs, rhs) => format!("({} AND {})", render_query(lhs), render_query(rhs)),
        QueryNode::Or(lhs, rhs) => format!("({} OR {})", render_query(lhs), render_query(rhs)),
        QueryNode::Not(inner) => format!("NOT {}", render_query(inner)),
    }
}

pub fn smart_query_to_tantivy(raw: &str) -> Option<String> {
    let ast = parse_query(raw)?;
    Some(render_query(&ast))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_query_defaults_adjacent_terms_to_or() {
        assert_eq!(
            smart_query_to_tantivy("disallow glob").as_deref(),
            Some("(disallow OR glob)")
        );
    }

    #[test]
    fn smart_query_preserves_and_and_exclusion() {
        assert_eq!(
            smart_query_to_tantivy("\"glob pattern\" AND disallow -legacy").as_deref(),
            Some("((\"glob pattern\" AND disallow) AND NOT legacy)")
        );
    }
}
