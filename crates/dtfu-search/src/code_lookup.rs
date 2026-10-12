//! Names in code (port of `enum CodeLookup`, CodeExtractor.swift:250-378): a query that's a name finds it exactly,
//! and the place that defines it rather than every place that uses it; a result opens at the line that matched.
//!
//! Swift `Character` is a grapheme cluster; here a `char` stands in for it (letters, digits, `_`, `$` and the
//! whitespace around names are single scalars in code). Only the length check in `first_line` counts graphemes.

use std::collections::HashSet;
use std::sync::OnceLock;

use unicode_segmentation::UnicodeSegmentation;

/// Words that start a definition.
const DEFINERS: &[&str] = &[
    "def", "func", "fn", "fun", "function", "class", "struct", "enum", "protocol", "interface", "trait", "type",
    "typealias", "actor", "extension", "impl", "module", "namespace", "object", "record", "macro", "const", "let",
    "var", "val", "union", "mod", "sub", "@interface", "@implementation", "@protocol", "#define", "function*",
];
/// Words before a name that mean it's being used, not defined ("await name(...)", "return name(...)").
const USERS: &[&str] = &[
    "return", "await", "yield", "throw", "new", "else", "case", "print", "echo", "assert", "not", "and", "or", "in",
    "is", "do", "go", "defer", "try", "catch", "if", "while", "for", "typeof", "delete", "raise", "import", "from",
];
/// How comments and docstrings start.
const COMMENT_STARTS: &[&str] = &["//", "#", "/*", "*", "--", "\"\"\"", "'''", "<!--", ";"];

fn definers() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| DEFINERS.iter().copied().collect())
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// `^[A-Za-z_$][A-Za-z0-9_$]*(?:(?:\.|::|->|#)[A-Za-z_$][A-Za-z0-9_$]*)*$`
fn is_name_shape(s: &str) -> bool {
    let mut rest = s;
    loop {
        let mut chars = rest.chars();
        match chars.next() {
            Some(c) if is_ident_start(c) => {}
            _ => return false,
        }
        let end = rest.find(|c: char| !is_ident_char(c)).unwrap_or(rest.len());
        rest = &rest[end..];
        if rest.is_empty() {
            return true;
        }
        rest = if let Some(r) = rest.strip_prefix("::").or_else(|| rest.strip_prefix("->")) {
            r
        } else if let Some(r) = rest.strip_prefix('.').or_else(|| rest.strip_prefix('#')) {
            r
        } else {
            return false;
        };
    }
}

/// The words keyword search keeps a name from code as ("quantize_row_q8_0_ref" becomes quantize row q8 0 ref, as one
/// phrase), when the query is one: snake_case, camelCase, dotted, or with digits. `None` for words anyone would say
/// ("retry", "Renamer"), which are searched as words and by meaning.
pub fn identifier(query: &str) -> Option<Vec<String>> {
    let mut name = query.trim();
    if let Some(stripped) = name.strip_suffix("()") {
        name = stripped;
    }
    if !is_name_shape(name) {
        return None;
    }
    let camel = name.chars().skip(1).any(char::is_uppercase) && name.chars().any(char::is_lowercase);
    if !(camel || name.chars().any(|c| "_.:#$".contains(c) || c.is_numeric())) {
        return None;
    }
    Some(
        name.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// The keyword search pattern for an identifier's words: one phrase, so they match only side by side, as written.
pub fn phrase(words: &[String]) -> String {
    format!("\"{}\"", words.join(" "))
}

/// Swift's `trimmingCharacters(in: .whitespaces)`: blanks, not line breaks.
fn trim_blanks(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() && c != '\n' && c != '\r')
}

/// Where `name` is defined in `text` (a stretch whose first line is `first`): the line, and how surely. 2 after a
/// word that defines something (def, func, class, function, struct, #define, @implementation...) or as a C-style
/// definition (a type before it, its parameters after it, no "=" or call around it); 1 for a declaration (the
/// same, ending in ";"). `None` when the stretch only uses it.
pub fn definition(name: &str, text: &str, first: i64) -> Option<(i64, i32)> {
    let mut best: Option<(i64, i32)> = None;
    for (offset, line) in lines(text).into_iter().enumerate() {
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        for (from, to) in occurrences(name, &chars) {
            let lo = chars.get(from).map_or(line.len(), |c| c.0);
            let hi = chars.get(to).map_or(line.len(), |c| c.0);
            let before = trim_blanks(&line[..lo]);
            let after = trim_blanks(&line[hi..]);
            let strength = definition_strength(before, after, line);
            if strength > best.map_or(0, |b| b.1) {
                best = Some((first + offset as i64, strength));
            }
            if strength == 2 {
                return best;
            }
        }
    }
    best
}

/// A word that starts a definition ("def", "func", "class", "@implementation"...). Not the ones that only declare a
/// value (let, var, const, val).
pub fn defines_something(word: &str) -> bool {
    definers().contains(word) && !matches!(word, "let" | "var" | "const" | "val")
}

fn definition_strength(before: &str, after: &str, line: &str) -> i32 {
    let previous = before.split(char::is_whitespace).rfind(|s| !s.is_empty()).unwrap_or("");
    if definers().contains(previous) {
        return 2;
    }
    if before.starts_with("func (") && before.ends_with(')') {
        return 2; // a Go method
    }
    if before.starts_with("- (") || before.starts_with("+ (") {
        // an Objective-C method
        return if after.starts_with(':') || after.is_empty() || after.starts_with('{') || after.starts_with(';') {
            2
        } else {
            0
        };
    }
    // C-style: "static void name(...) {", "struct gguf_context * name(...)", not a call or an assignment.
    if !after.starts_with('(')
        || before.is_empty()
        || USERS.contains(&previous)
        || before.chars().any(|c| "=(,.!?".contains(c))
        || before.contains("->")
    {
        return 0;
    }
    match before.chars().next_back() {
        Some(last) if last.is_alphabetic() || last.is_numeric() || "*&>_]".contains(last) => {}
        _ => return 0,
    }
    if trim_blanks(line).ends_with(';') {
        1
    } else {
        2
    }
}

/// The line (from `first`) to open at for `words`: the first line with one of them that defines something ("def
/// retry_with_backoff"), else the first such line of code (not a comment), else the first such line. A word counts
/// as itself or as a part of a name (the "retry" of retryWithBackoff and retry_with_backoff). `None` when none is there.
pub fn first_line(words: &[String], text: &str, first: i64) -> Option<i64> {
    let words: Vec<Vec<char>> = words
        .iter()
        .filter(|w| w.graphemes(true).count() >= 2)
        .map(|w| w.to_lowercase().chars().collect())
        .collect();
    if words.is_empty() {
        return None;
    }
    let (mut first_code, mut first_any): (Option<i64>, Option<i64>) = (None, None);
    for (offset, line) in lines(text).into_iter().enumerate() {
        let offset = offset as i64;
        let line_chars: Vec<char> = line.chars().collect();
        let lower: Vec<char> = line.to_lowercase().chars().collect();
        if !words.iter().any(|w| has(w, &line_chars, &lower)) {
            continue;
        }
        let trimmed = trim_blanks(line);
        if trimmed.split(char::is_whitespace).filter(|s| !s.is_empty()).take(4).any(defines_something) {
            return Some(first + offset);
        }
        if first_code.is_none() && !COMMENT_STARTS.iter().any(|c| trimmed.starts_with(c)) {
            first_code = Some(offset);
        }
        if first_any.is_none() {
            first_any = Some(offset);
        }
    }
    first_code.or(first_any).map(|o| first + o)
}

/// Whether the line has `word` (lowercased) as a word or as the start of a name's part: at the line's start, after
/// something that isn't a letter or a digit, or at a capital letter (camelCase).
fn has(word: &[char], line: &[char], lower: &[char]) -> bool {
    let n = word.len();
    if n == 0 || lower.len() < n {
        return false;
    }
    let mut i = 0;
    while i + n <= lower.len() {
        if lower[i..i + n] == *word {
            if i == 0
                || (!lower[i - 1].is_alphabetic() && !lower[i - 1].is_numeric())
                || line.get(i).is_some_and(|c| c.is_uppercase())
            {
                return true;
            }
            i += n;
        } else {
            i += 1;
        }
    }
    false
}

/// Lines split on "\n" and "\r\n", empty ones kept.
fn lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

fn is_name_character(c: char) -> bool {
    c.is_alphabetic() || c.is_numeric() || c == '_' || c == '$'
}

/// Where `name` is in `line` as a whole name (not a part of a longer one), whatever its case: char index ranges.
fn occurrences(name: &str, line: &[(usize, char)]) -> Vec<(usize, usize)> {
    let name: Vec<char> = name.chars().collect();
    let n = name.len();
    let mut found = Vec::new();
    if n == 0 {
        return found;
    }
    let mut start = 0;
    while start + n <= line.len() {
        let same = (0..n).all(|j| line[start + j].1.to_lowercase().eq(name[j].to_lowercase()));
        if !same {
            start += 1;
            continue;
        }
        let end = start + n;
        let clear_before = start == 0 || !is_name_character(line[start - 1].1);
        let clear_after = end == line.len() || !is_name_character(line[end].1);
        if clear_before && clear_after {
            found.push((start, end));
        }
        start = end;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(v: &[&str]) -> Option<Vec<String>> {
        Some(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn names_from_code() {
        assert_eq!(identifier("quantize_row_q8_0_ref"), words(&["quantize", "row", "q8", "0", "ref"]));
        assert_eq!(identifier("refreshAccessToken"), words(&["refreshaccesstoken"]));
        assert_eq!(identifier("DuplicateFinder"), words(&["duplicatefinder"]));
        assert_eq!(identifier("os.path.join"), words(&["os", "path", "join"]));
        assert_eq!(identifier("llama_sampler_top_k_impl()"), words(&["llama", "sampler", "top", "k", "impl"]));
        assert_eq!(identifier("retry"), None);
        assert_eq!(identifier("Renamer"), None);
        assert_eq!(identifier("retry with backoff"), None);
        assert_eq!(identifier("hamming distance"), None);
        assert_eq!(phrase(&["a".into(), "b".into()]), "\"a b\"");
    }

    #[test]
    fn definitions_over_uses() {
        let defined = |name: &str, text: &str| definition(name, text, 10).map(|d| d.0);
        assert_eq!(defined("retry_with_backoff", "x = retry_with_backoff(f)\n\ndef retry_with_backoff(attempts=5):"), Some(12));
        assert_eq!(defined("isMPEGTransportStream", "    static func isMPEGTransportStream(_ url: URL) -> Bool {"), Some(10));
        assert_eq!(
            defined("refreshAccessToken", "  token = await refreshAccessToken();\nexport async function refreshAccessToken() {"),
            Some(11)
        );
        assert_eq!(
            defined(
                "gguf_init_from_reader",
                "struct gguf_context * result = gguf_init_from_reader(gr, params);\n\
                 static struct gguf_context * gguf_init_from_reader(const struct gguf_reader & gr, struct gguf_init_params p) {"
            ),
            Some(11)
        );
        assert_eq!(defined("llama_sampler_top_k_impl", "    llama_sampler_top_k_impl(cur_p, ctx->k);\n    return;"), None);
        assert_eq!(defined("SUStandardVersionComparator", "@implementation SUStandardVersionComparator"), Some(10));
        assert_eq!(defined("Name", "func (s *Server) Name() string {"), Some(10));
        // A declaration counts less than a definition, more than a use.
        let header = "GGML_API void quantize_row_q8_0_ref(const float * x, block_q8_0 * y, int64_t k);";
        assert_eq!(definition("quantize_row_q8_0_ref", header, 1).map(|d| d.1), Some(1));
    }

    #[test]
    fn the_line_to_open_at() {
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let text = "import time\n\n\ndef retry_with_backoff(attempts=5):\n    pass\n";
        assert_eq!(first_line(&w(&["retry"]), text, 40), Some(43));
        assert_eq!(first_line(&w(&["backoff"]), "let a = 1\nfunc retryWithBackoff() {}", 1), Some(2));
        assert_eq!(first_line(&w(&["zebra"]), text, 1), None);
        // A definition with the words beats a docstring or a comment that has them first.
        let module = "\"\"\"Retrying calls to flaky services.\"\"\"\n\nimport random\n\ndef retry_with_backoff(f):\n    pass\n";
        assert_eq!(first_line(&w(&["retry", "backoff"]), module, 1), Some(5));
        assert_eq!(first_line(&w(&["calls"]), &format!("{module}x = calls()\n"), 1), Some(7));
    }
}
