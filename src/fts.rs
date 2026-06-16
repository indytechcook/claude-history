//! Turn user-typed search input into a safe FTS5 MATCH expression.
//!
//! FTS5's query grammar treats characters like `.`, `/`, `:` and `-` as syntax,
//! so a natural query such as `vector.yaml` raises "fts5: syntax error near .".
//! For plain queries we wrap each whitespace-separated term in double quotes,
//! making it an FTS5 *phrase* — punctuation inside is then tokenized like the
//! indexed text (`vector.yaml` -> the adjacent tokens `vector` `yaml`) and
//! matched literally.
//!
//! Power users can still pass real FTS5: if the input already uses quotes,
//! grouping, a column filter, or the boolean/NEAR operators, it is sent through
//! unchanged.

/// Build the string to pass to `... MATCH ?`.
pub fn build_match(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if is_advanced(trimmed) {
        return trimmed.to_string();
    }
    trimmed
        .split_whitespace()
        .filter_map(quote_term)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Does the query already use explicit FTS5 syntax we should not touch?
fn is_advanced(q: &str) -> bool {
    if q.contains('"') || q.contains('(') || q.contains(')') || q.contains(':') {
        return true;
    }
    // FTS5 boolean/NEAR operators are recognised only in uppercase.
    q.split_whitespace()
        .any(|t| matches!(t, "AND" | "OR" | "NOT") || t.starts_with("NEAR"))
}

/// Quote one term as a phrase, preserving a trailing `*` as a prefix match.
/// Returns `None` for terms that have nothing left to match (e.g. a lone `*`).
fn quote_term(term: &str) -> Option<String> {
    let (core, star) = match term.strip_suffix('*') {
        Some(c) => (c, "*"),
        None => (term, ""),
    };
    if core.is_empty() {
        return None;
    }
    let escaped = core.replace('"', "\"\"");
    Some(format!("\"{}\"{}", escaped, star))
}

#[cfg(test)]
mod tests {
    use super::build_match;

    #[test]
    fn plain_term_with_dot_becomes_phrase() {
        assert_eq!(build_match("vector.yaml"), "\"vector.yaml\"");
    }

    #[test]
    fn multiple_terms_are_each_quoted() {
        assert_eq!(build_match("vector.yaml config"), "\"vector.yaml\" \"config\"");
    }

    #[test]
    fn trailing_star_is_preserved_as_prefix() {
        assert_eq!(build_match("metric*"), "\"metric\"*");
    }

    #[test]
    fn advanced_queries_pass_through() {
        assert_eq!(build_match("sidekiq AND retry"), "sidekiq AND retry");
        assert_eq!(build_match("\"already quoted\""), "\"already quoted\"");
        assert_eq!(build_match("col:foo"), "col:foo");
    }

    #[test]
    fn lone_star_is_dropped() {
        assert_eq!(build_match("*"), "");
    }
}
