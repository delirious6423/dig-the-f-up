//! Query router: a pure classifier that says whether a query looks like an exact lookup (an identifier, path, glob,
//! code, number, quoted phrase) or like natural language. Callers can use it to de-emphasise semantic search for
//! lookups. It never runs a search and never skips one; it only advises.
//!
//! Input is the query text after any `code:` prefix has been stripped. The engine strips that prefix in its private
//! `code_query_text` (digup-engine), and this module does not know about the prefix, so a leading `code:` passed here
//! would be read as part of the text.
//!
//! Heuristics, applied in order (the first match wins; `Routed::reason` names it):
//! 1. Empty or whitespace-only: Ambiguous ("empty").
//! 2. Ends with `?` or `？`: Semantic ("question").
//! 3. Wrapped in matching quotes (`"…"`, `'…'`, `“…”`, `` `…` ``): Exact ("quoted phrase").
//! 4. One token (no whitespace; trailing `. , ; : !` ignored), checked in this order:
//!    - regex-looking (starts with `^`, ends with `$`, or contains `|`, `.*`, `.+`, `(?`, `\d`, `\w`, `\s`, `\b`):
//!      Exact ("regex"). This runs before the glob and path checks, so `.*` is a regex and `*.pdf` is a glob.
//!    - glob (contains `*` or `?`, or `[` with a later `]`): Exact ("glob").
//!    - path (contains `/` or `\`): Exact ("path").
//!    - UUID (five hex groups of 8-4-4-4-12): Exact ("uuid").
//!    - hex literal (`0x` plus hex digits): Exact ("hex literal").
//!    - hash (7+ hex characters with both a digit and a letter a-f): Exact ("hash").
//!    - number or date (digits with optional sign and `. , _ -`, e.g. `404`, `3.14`, `2024-03-15`): Exact.
//!    - error code (1-10 letters, then digits, with optional `-`/`_` separators, e.g. `E0382`, `ERR_1234`,
//!      `CVE-2024-1234`, `mp4`): Exact ("error code").
//!    - identifier: `::`, a `_` next to letters or digits (`parse_query`), a dot between name characters (`README.md`,
//!      `foo.bar`, `v1.2.3`), or camelCase/PascalCase with only letters and digits (`parseQuery`, `iPhone`,
//!      `HTTPServer`). Exact ("identifier" variants).
//!    - Otherwise the token falls through to the rules below. A plain word such as `Paris`, `TODO` or `well-known`
//!      is Ambiguous ("single word").
//! 5. Any non-Latin letter (code point U+0370 or above: CJK, Arabic, Bengali, Thai and so on): Semantic ("non-Latin
//!    script"). These scripts often have no spaces and FTS tokenises them poorly, so they are never Exact just
//!    because they lack spaces. The step-4 markers above still win for tokens such as `报告.pdf` or `2024`.
//! 6. Several words: 3 or more whitespace-separated words are Semantic ("three or more words"). Two words are
//!    Ambiguous ("two words").
//!
//! Known limits: ALL-CAPS words are Ambiguous, not Exact. Kebab-case is Ambiguous. `McDonald` is Exact because it
//! looks like PascalCase. The classifier is a heuristic and makes no claim about the corpus.
//!
//! Weights: `suggested_word_weight(Exact)` returns the lookup words weight. `searcher.rs` gives lookups
//! (`is_lookup` or a code identifier) the words weight `word_weight * 2.0`, so with the default 2.0 it is 4.0. The
//! router is advisory and does not duplicate that logic. `searcher` already switches to lookup weights for many of
//! the same tokens (digits with letters, identifiers in the code index), so callers should not add the weight on top
//! of that without measuring.

use crate::searcher::DEFAULT_WORD_WEIGHT;

/// The lookup multiplier `searcher.rs` applies to the words weight in lookup mode (`word_weight * 2.0`).
const LOOKUP_WORDS_FACTOR: f64 = 2.0;

/// What a query looks like. See the module docs for the heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryShape {
    /// Looks like a name, path, pattern, code or number: an exact lookup.
    Exact,
    /// Looks like a natural-language question or phrase.
    Semantic,
    /// Too short to tell (one or two plain words).
    Ambiguous,
}

/// A routing decision with the rule that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routed {
    pub shape: QueryShape,
    /// Short name of the rule that matched, for logs and tests (never empty).
    pub reason: &'static str,
}

/// The shape of `query` (see the module docs).
pub fn classify(query: &str) -> QueryShape {
    route(query).shape
}

/// The shape of `query` and the rule that decided it (see the module docs).
pub fn route(query: &str) -> Routed {
    let t = query.trim();
    if t.is_empty() {
        return Routed { shape: QueryShape::Ambiguous, reason: "empty" };
    }
    if t.ends_with(['?', '？']) {
        return Routed { shape: QueryShape::Semantic, reason: "question" };
    }
    if is_quoted(t) {
        return Routed { shape: QueryShape::Exact, reason: "quoted phrase" };
    }
    let words = t.split_whitespace().count();
    if words == 1 {
        let core = t.trim_end_matches(['.', ',', ';', ':', '!']);
        let core = if core.is_empty() { t } else { core };
        if let Some(reason) = exact_token_reason(core) {
            return Routed { shape: QueryShape::Exact, reason };
        }
    }
    if has_non_latin(t) {
        return Routed { shape: QueryShape::Semantic, reason: "non-Latin script" };
    }
    if words >= 3 {
        Routed { shape: QueryShape::Semantic, reason: "three or more words" }
    } else if words == 2 {
        Routed { shape: QueryShape::Ambiguous, reason: "two words" }
    } else {
        Routed { shape: QueryShape::Ambiguous, reason: "single word" }
    }
}

/// Suggested `SearchOptions::word_weight` for a query of this shape, or `None` to keep the index default.
///
/// `Exact` returns `DEFAULT_WORD_WEIGHT * 2.0` (4.0 with the default 2.0). That is the words weight the lookup
/// mode uses in `searcher.rs`. For the code index the lookup value would be `CODE_WORD_WEIGHT * 2.0`, and the router
/// does not know which index it serves, so callers on the code index should compute that value themselves.
/// `Semantic` and `Ambiguous` return `None`.
pub fn suggested_word_weight(shape: QueryShape) -> Option<f64> {
    match shape {
        QueryShape::Exact => Some(DEFAULT_WORD_WEIGHT * LOOKUP_WORDS_FACTOR),
        QueryShape::Semantic | QueryShape::Ambiguous => None,
    }
}

/// The first exact-looking rule that matches a single token, or `None`.
fn exact_token_reason(s: &str) -> Option<&'static str> {
    if is_regex_like(s) {
        Some("regex")
    } else if s.contains(['*', '?']) || (s.contains('[') && s.contains(']')) {
        Some("glob")
    } else if s.contains(['/', '\\']) {
        Some("path")
    } else if is_uuid(s) {
        Some("uuid")
    } else if is_hex_literal(s) {
        Some("hex literal")
    } else if is_hash(s) {
        Some("hash")
    } else if is_number(s) {
        Some("number or date")
    } else if is_error_code(s) {
        Some("error code")
    } else {
        identifier_reason(s)
    }
}

fn is_quoted(s: &str) -> bool {
    let mut it = s.chars();
    match (it.next(), it.last()) {
        (Some(f), Some(l)) => matches!((f, l), ('"', '"') | ('\'', '\'') | ('“', '”') | ('`', '`')),
        _ => false,
    }
}

fn is_regex_like(s: &str) -> bool {
    s.starts_with('^')
        || s.ends_with('$')
        || s.contains('|')
        || s.contains(".*")
        || s.contains(".+")
        || s.contains("(?")
        || ["\\d", "\\w", "\\s", "\\b"].iter().any(|p| s.contains(p))
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(&n, p)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_hex_literal(s: &str) -> bool {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 7+ hex characters with at least one digit and one letter a-f (`3f2a9c1e`, `a1b2c3d`). All-digit runs are numbers.
fn is_hash(s: &str) -> bool {
    s.len() >= 7
        && s.chars().all(|c| c.is_ascii_hexdigit())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

fn is_number(s: &str) -> bool {
    let body = s.strip_prefix('+').or_else(|| s.strip_prefix('-')).unwrap_or(s);
    !body.is_empty()
        && body.chars().any(|c| c.is_ascii_digit())
        && body.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '_' | '-'))
}

/// Letters (optionally with `-`/`_`), then digits (optionally with `-`/`_`), ending in a digit.
fn is_error_code(s: &str) -> bool {
    let Some(i) = s.find(|c: char| c.is_ascii_digit()) else {
        return false;
    };
    let (head, tail) = s.split_at(i);
    head.len() <= 10
        && head.chars().any(|c| c.is_ascii_alphabetic())
        && head.chars().all(|c| c.is_ascii_alphabetic() || c == '-' || c == '_')
        && tail.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '_')
        && tail.ends_with(|c: char| c.is_ascii_digit())
}

fn identifier_reason(s: &str) -> Option<&'static str> {
    if s.contains("::") {
        return Some("identifier path");
    }
    if s.contains('_') && s.chars().any(char::is_alphanumeric) {
        return Some("snake_case identifier");
    }
    let chars: Vec<char> = s.chars().collect();
    for i in 1..chars.len().saturating_sub(1) {
        if chars[i] == '.' && chars[i - 1].is_alphanumeric() && chars[i + 1].is_alphanumeric() {
            let ext = s.rsplit('.').next().unwrap_or("");
            let is_ext = (1..=8).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric());
            return Some(if is_ext { "file name with extension" } else { "dotted identifier" });
        }
    }
    if chars.iter().all(|c| c.is_alphanumeric()) && has_camel_transition(&chars) {
        return Some("camelCase or PascalCase identifier");
    }
    None
}

/// `aB` (lower then upper) or `ABc` (upper, upper, then lower): camelCase, PascalCase or an acronym before a word.
fn has_camel_transition(c: &[char]) -> bool {
    c.windows(2).any(|w| w[0].is_lowercase() && w[1].is_uppercase())
        || c.windows(3).any(|w| w[0].is_uppercase() && w[1].is_uppercase() && w[2].is_lowercase())
}

/// Any letter at U+0370 or above (Greek, Cyrillic, Arabic, Indic, Thai, CJK and so on).
fn has_non_latin(s: &str) -> bool {
    s.chars().any(|c| c.is_alphabetic() && u32::from(c) >= 0x0370)
}

#[cfg(test)]
mod tests {
    use super::*;
    use QueryShape::{Ambiguous, Exact, Semantic};

    /// (query, expected shape). Grouped by kind; the comment on each group names the rule under test.
    const CASES: &[(&str, QueryShape)] = &[
        // English natural language (3+ words, or a question)
        ("how do I reset my password", Semantic),
        ("find my tax documents from last year", Semantic),
        ("meeting notes about the budget", Semantic),
        ("photos of my dog", Semantic),
        ("grocery list for next week", Semantic),
        ("What is in this folder?", Semantic),
        ("parse_query returns an error", Semantic),
        ("error code E0382 in build", Semantic),
        ("invoice 2024 pdf scan", Semantic),
        // English single and two words (Ambiguous)
        ("budget", Ambiguous),
        ("invoice", Ambiguous),
        ("vacation", Ambiguous),
        ("sunset", Ambiguous),
        ("tax return", Ambiguous),
        ("code review", Ambiguous),
        ("the", Ambiguous),
        ("ok", Ambiguous),
        ("Paris", Ambiguous),
        ("TODO", Ambiguous),
        ("well-known", Ambiguous),
        ("use-effect", Ambiguous),
        ("h2o", Ambiguous),
        ("fn parse_query", Ambiguous),
        ("invoice 2024", Ambiguous),
        ("my notes.txt", Ambiguous),
        ("photo IMG_1234.jpg", Ambiguous),
        ("a", Ambiguous),
        ("", Ambiguous),
        ("   ", Ambiguous),
        // identifiers (snake_case, camelCase, PascalCase, dotted, paths with ::)
        ("parse_query", Exact),
        ("parseQuery", Exact),
        ("ParseQuery", Exact),
        ("iPhone", Exact),
        ("HTTPServer", Exact),
        ("MAX_BUF_SIZE", Exact),
        ("__init__", Exact),
        ("std::io::Read", Exact),
        ("Foo::new()", Exact),
        ("self.field", Exact),
        ("foo.bar", Exact),
        ("config.toml", Exact),
        ("README.md", Exact),
        ("v1.2.3", Exact),
        ("parse_query.", Exact),
        // paths
        ("/Users/me/Documents", Exact),
        ("~/Desktop/notes.txt", Exact),
        ("./src/main.rs", Exact),
        ("../../x", Exact),
        ("src/lib", Exact),
        ("C:\\Users\\me\\file.docx", Exact),
        // globs
        ("*.pdf", Exact),
        ("IMG_????.jpg", Exact),
        ("report_[0-9].csv", Exact),
        ("data*", Exact),
        // quoted strings (even with spaces)
        ("\"quarterly report\"", Exact),
        ("'exact phrase'", Exact),
        ("“smart quotes”", Exact),
        ("`code span`", Exact),
        // error codes, numbers, dates, hashes, UUIDs
        ("E0382", Exact),
        ("ERR_1234", Exact),
        ("CVE-2024-1234", Exact),
        ("mp4", Exact),
        ("ab12", Exact),
        ("404", Exact),
        ("3.14", Exact),
        ("1,000", Exact),
        ("2024", Exact),
        ("2024-03-15", Exact),
        ("3f2a9c1e8b7d", Exact),
        ("a1b2c3d", Exact),
        ("0xDEADBEEF", Exact),
        ("550e8400-e29b-41d4-a716-446655440000", Exact),
        // regex-looking
        ("^foo", Exact),
        ("bar$", Exact),
        ("\\d{3}-\\d{4}", Exact),
        ("a|b", Exact),
        ("x.*y", Exact),
        // CJK (never Exact because of missing spaces; ASCII markers still work)
        ("猫", Semantic),
        ("咖啡", Semantic),
        ("東京 旅行", Semantic),
        ("猫 咖啡 照片", Semantic),
        ("我想找去年的旅行照片", Semantic),
        ("这是什么文件？", Semantic),
        ("2024年报告", Semantic),
        ("报告.pdf", Exact),
        // Arabic
        ("فاتورة", Semantic),
        ("فاتورة الكهرباء", Semantic),
        ("invoice فاتورة", Semantic),
        ("فاتورة.pdf", Exact),
        // Bengali
        ("বাংলা", Semantic),
        ("বাংলা ফাইল", Semantic),
        // Thai
        ("ภาษาไทย", Semantic),
        ("ใบเสร็จ", Semantic),
        // mixed
        ("报告 foo_bar", Semantic),
        ("invoice.pdf 2024", Ambiguous),
    ];

    #[test]
    fn classify_table() {
        let mut failures = Vec::new();
        for (q, want) in CASES {
            let got = classify(q);
            if got != *want {
                failures.push(format!("{q:?}: want {want:?}, got {got:?} ({})", route(q).reason));
            }
        }
        assert!(failures.is_empty(), "{} misrouted:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn table_has_at_least_sixty_queries() {
        assert!(CASES.len() >= 60, "only {} cases", CASES.len());
    }

    #[test]
    fn every_route_has_a_reason() {
        for (q, _) in CASES {
            assert!(!route(q).reason.is_empty(), "{q:?} has no reason");
        }
    }

    #[test]
    fn code_prefix_is_not_seen_here() {
        // The engine strips `code:` before calling the router. Given the stripped text, it routes like any other.
        assert_eq!(classify("parse_query"), Exact);
        assert_eq!(route("parse_query").reason, "snake_case identifier");
    }

    #[test]
    fn suggested_weight_matches_lookup_words_weight() {
        assert_eq!(suggested_word_weight(Exact), Some(4.0));
        assert_eq!(suggested_word_weight(Semantic), None);
        assert_eq!(suggested_word_weight(Ambiguous), None);
    }
}
