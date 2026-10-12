//! Port of `SearchText.swift` (and the `Landmark` and `PageTextProblem` helpers that live beside it): text the way
//! keyword search keeps it and matches it, so a word is found however it was typed or extracted.

use std::ops::Range;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
use unicode_segmentation::UnicodeSegmentation;

/// NFKC (presentation forms and ligatures become plain letters, full-width becomes ASCII, Bengali's two spellings of
/// "ো" become one), invisible formatting characters dropped (joiners, direction marks), and Arabic the way people type
/// it: no tatweel, no short-vowel marks, one alef. A zero-width space becomes a word break.
pub fn normalized(text: &str) -> String {
    let compatible: String = text.nfkc().collect();
    let mut out = String::with_capacity(compatible.len());
    for c in compatible.chars() {
        match c as u32 {
            0x200B => out.push(' '),
            0x00AD | 0x061C | 0x200C..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xFEFF => {}
            0x0640 | 0x064B..=0x065F | 0x0670 => {}
            0x0622 | 0x0623 | 0x0625 | 0x0671 => out.push('\u{0627}'),
            _ => out.push(c),
        }
    }
    out
}

fn in_indic(v: u32) -> bool {
    (0x0900..=0x0DFF).contains(&v)
}

fn in_arabic(v: u32) -> bool {
    (0x0600..=0x06FF).contains(&v)
}

/// True for words of scripts that attach endings to words (Bengali, Hindi, Arabic): keyword search matches them as
/// prefixes.
pub fn inflects(word: &str) -> bool {
    word.chars().any(|c| in_indic(c as u32) || in_arabic(c as u32))
}

/// A character of a word the way Swift's `CharacterSet.alphanumerics` sees it: letters, numbers and *all* marks.
/// Rust's `is_alphanumeric` leaves out marks that are not `Alphabetic`, such as the Bengali and Devanagari virama
/// (U+09CD, U+094D), which would split "চুক্তি" in two.
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c)
}

/// Whether text says something in words: at least a fifth of what isn't whitespace is letters (Swift's
/// `CharacterSet.letters` includes marks, such as Bengali vowel signs and the virama).
pub fn has_words(text: &str) -> bool {
    let (mut letters, mut others) = (0usize, 0usize);
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if c.is_alphabetic() || is_combining_mark(c) {
            letters += 1;
        } else {
            others += 1;
        }
    }
    letters > 0 && letters * 4 >= others
}

/// A grapheme cluster counts as a word character when any of its scalars is alphanumeric (Swift's `Character` rule).
fn is_word_grapheme(g: &str) -> bool {
    g.chars().any(is_word_char)
}

fn grapheme_before(text: &str, at: usize) -> &str {
    text[..at].graphemes(true).next_back().unwrap_or("")
}

fn grapheme_at(text: &str, at: usize) -> &str {
    text[at..].graphemes(true).next().unwrap_or("")
}

/// `text` normalized one grapheme cluster at a time, remembering where each byte came from.
struct Folded {
    text: String,
    /// For each byte of `text`, the byte span of the source grapheme it came from.
    origin: Vec<(usize, usize)>,
}

impl Folded {
    fn new(source: &str) -> Self {
        let mut text = String::with_capacity(source.len());
        let mut origin = Vec::with_capacity(source.len());
        for (start, g) in source.grapheme_indices(true) {
            let piece = normalized(g).to_lowercase();
            text.push_str(&piece);
            origin.extend(std::iter::repeat_n((start, start + g.len()), piece.len()));
        }
        Folded { text, origin }
    }

    /// The source bytes that the folded bytes `start..end` came from.
    fn original(&self, start: usize, end: usize) -> Option<Range<usize>> {
        if start >= end || end > self.origin.len() {
            return None;
        }
        Some(self.origin[start].0..self.origin[end - 1].1)
    }
}

/// Where `word` is in `text` as keyword search would find it (normalized, ignoring case and accents), as byte ranges
/// of the text as it's written: whole words, and for a word of a language that attaches endings, the whole word it
/// starts ("কুমির" marks all of "কুমিরের").
pub fn matches(word: &str, text: &str) -> Vec<Range<usize>> {
    let needle = normalized(word).to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let folded = Folded::new(text);
    let prefix = inflects(word);
    let mut found: Vec<Range<usize>> = Vec::new();
    for (hit, _) in folded.text.match_indices(&needle) {
        if found.len() >= 500 {
            break;
        }
        let Some(mut range) = folded.original(hit, hit + needle.len()) else {
            continue;
        };
        if range.start > 0 && is_word_grapheme(grapheme_before(text, range.start)) {
            continue;
        }
        if range.end < text.len() && is_word_grapheme(grapheme_at(text, range.end)) {
            if !prefix {
                continue;
            }
            let mut end = range.end;
            for g in text[range.end..].graphemes(true) {
                if !is_word_grapheme(g) {
                    break;
                }
                end += g.len();
            }
            range = range.start..end;
        }
        if let Some(last) = found.last() {
            if last.start < range.end && range.start < last.end {
                continue;
            }
        }
        found.push(range);
    }
    found
}

/// One line from `text` around the first of `terms` it has, the terms marked « » as SQLite's snippet() marks them.
/// Nil (None) when none of the terms is in it.
pub fn snippet(text: &str, terms: &[&str], words: usize) -> Option<String> {
    let mut marks: Vec<Range<usize>> = terms.iter().flat_map(|t| matches(t, text)).collect();
    marks.sort_by_key(|r| r.start);
    let first = marks.first()?.clone();

    // Grapheme-index arithmetic, as Swift's Character indices.
    let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let n = graphemes.len();
    let byte_at = |i: usize| if i < n { graphemes[i].0 } else { text.len() };
    let index_of = |b: usize| graphemes.partition_point(|(s, _)| *s < b);
    let is_ws = |i: usize| graphemes[i].1.chars().next().is_some_and(char::is_whitespace);

    let first_lower = index_of(first.start);
    let first_upper = index_of(first.end);
    // About `words` words, starting a few before the first match.
    let mut start = first_lower;
    let mut before = 0usize;
    while start > 0 && before < words / 3 {
        start -= 1;
        if is_ws(start) && start < first_lower && start + 1 < n && !is_ws(start + 1) {
            before += 1;
        }
    }
    if start > 0 || is_ws(start) {
        start += 1;
    }
    let mut end = first_upper;
    let mut after = 0usize;
    while end < n && after < words.saturating_sub(before) {
        if is_ws(end) {
            after += 1;
        }
        end += 1;
    }

    let (start_b, end_b) = (byte_at(start), byte_at(end));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut cursor = start_b;
    for mark in &marks {
        if mark.start >= start_b && mark.end <= end_b && mark.start >= cursor {
            out.push_str(&text[cursor..mark.start]);
            out.push('«');
            out.push_str(&text[mark.clone()]);
            out.push('»');
            cursor = mark.end;
        }
    }
    out.push_str(&text[cursor..end_b]);
    if end < n {
        out.push('…');
    }

    // Collapse whitespace runs to one space, then trim the spaces at the ends.
    let mut collapsed = String::with_capacity(out.len());
    for c in out.chars() {
        if c.is_whitespace() {
            if !collapsed.ends_with(' ') {
                collapsed.push(' ');
            }
        } else {
            collapsed.push(c);
        }
    }
    Some(collapsed.trim_matches(' ').to_string())
}

/// Words to search for in a PDF so its page is among the matches. `texts`: the PDF's own text of pages 1 through
/// `page` (1-based). Returns the first of `words` on that page (spelled as the page spells it), preferring one that
/// isn't on an earlier page. None for page 1, or when none of the words is on it.
pub fn landmark_find(page: usize, texts: &[String], words: &[String]) -> Option<String> {
    if page <= 1 || texts.len() < page {
        return None;
    }
    let target = &texts[page - 1];
    let before: Vec<String> = texts[..page - 1].iter().map(|t| normalized(t).to_lowercase()).collect();
    let on_page: Vec<String> = words
        .iter()
        .filter(|w| w.graphemes(true).count() >= 3)
        .filter_map(|w| matches(w, target).first().map(|r| target[r.clone()].to_string()))
        .collect();
    on_page
        .iter()
        .find(|w| {
            let needle = normalized(w).to_lowercase();
            !before.iter().any(|b| b.contains(&needle))
        })
        .or(on_page.first())
        .cloned()
}

/// What's wrong with the text PDFKit read from a page, when the page's text layer can't be used as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageTextProblem {
    /// Bengali or Hindi read in glyph order: vowel signs ahead of their consonants, conjuncts lost.
    GlyphOrder,
    /// Arabic letters lost where the font drew a lam ligature: a bare alef before a "?" or "."
    LostLetters,
}

impl PageTextProblem {
    pub fn check(text: &str) -> Option<PageTextProblem> {
        let text: String = text.nfc().collect();
        let (mut vowel_signs, mut misplaced, mut lost_letters) = (0usize, 0usize, 0usize);
        let mut previous = ' ';
        let mut before_previous = ' ';
        for c in text.chars() {
            let v = c as u32;
            if is_indic_vowel_sign(v) {
                vowel_signs += 1;
                if !is_indic_consonant_or_nukta(previous as u32) {
                    misplaced += 1;
                }
            }
            if before_previous == '\u{0627}'
                && "?.\u{FFFD}".contains(previous)
                && in_arabic(v)
                && c.is_alphabetic()
            {
                lost_letters += 1;
            }
            before_previous = previous;
            previous = c;
        }
        if vowel_signs >= 10 && misplaced * 20 >= vowel_signs {
            return Some(PageTextProblem::GlyphOrder);
        }
        if lost_letters >= 2 {
            return Some(PageTextProblem::LostLetters);
        }
        None
    }
}

/// Dependent vowel signs of Bengali and Devanagari: in a real word, each follows a consonant (or its nukta).
fn is_indic_vowel_sign(v: u32) -> bool {
    matches!(v,
        0x09BE..=0x09C4 | 0x09C7 | 0x09C8 | 0x09CB | 0x09CC | 0x09D7 | 0x09E2 | 0x09E3
        | 0x093A | 0x093B | 0x093E..=0x094C | 0x094E | 0x094F | 0x0955..=0x0957 | 0x0962 | 0x0963)
}

fn is_indic_consonant_or_nukta(v: u32) -> bool {
    matches!(v,
        0x0995..=0x09B9 | 0x09BC | 0x09CE | 0x09DC | 0x09DD | 0x09DF | 0x09F0 | 0x09F1
        | 0x0915..=0x0939 | 0x093C | 0x0958..=0x095F | 0x0979..=0x097F)
}

#[cfg(test)]
mod tests {
    #[test]
    fn virama_keeps_conjuncts_in_one_word() {
        let w = "চুক্তি";
        assert!(w.chars().all(super::is_word_char));
        assert!(super::has_words("ক্ত ক্ত ক্ত"));
        assert!(!super::is_word_char(' ') && !super::is_word_char('-'));
    }

    use super::*;

    #[test]
    fn normalized_folds_what_is_typed_differently() {
        assert_eq!(normalized("الْمَكْتَبَةَ"), "المكتبة"); // harakat
        assert_eq!(normalized("إلى الأردن"), "الى الاردن"); // alef forms
        assert_eq!(normalized("مـــرحبا"), "مرحبا"); // tatweel
        assert_eq!(normalized("\u{FEFB}"), "\u{0644}\u{0627}"); // the lam-alef ligature → lam, alef
        assert_eq!(normalized("\u{FB01}le"), "file"); // ﬁ
        assert_eq!(normalized("র\u{200D}্যাব"), "র্যাব"); // zero-width joiner
        assert_eq!(normalized("\u{09C7}\u{09BE}"), "\u{09CB}"); // ে + া → ো
        assert_eq!(normalized("a\u{200B}b"), "a b"); // zero-width space breaks words
    }

    #[test]
    fn inflects_detects_indic_and_arabic() {
        assert!(inflects("কুমির"));
        assert!(inflects("مدرسة"));
        assert!(!inflects("payment"));
    }

    #[test]
    fn has_words_needs_a_fifth_letters() {
        assert!(has_words("Revenue grew in Japan"));
        assert!(!has_words("1234 5678 9012 3456 7890 1 2 3 4 5 6 7"));
        assert!(!has_words("   \n  "));
    }

    #[test]
    fn garbled_page_text_is_noticed() {
        let glyph_order = "বািড় ভাড়ার চিপ ধানমি ৭ নর রােডর িতন বডরেমর াট মািসক ২৫,০০০ টাকা ভাড়ায় দওয়া হেলা। ভাড়ায়া দুই মােসর";
        let bengali = "বাড়ি ভাড়ার চুক্তিপত্র। ধানমন্ডি ৭ নম্বর রোডের তিন বেডরুমের ফ্ল্যাটটি মাসিক ২৫,০০০ টাকা ভাড়ায় দেওয়া হলো।";
        let lost = "ارتفعت ا.بيعات بنسبة خمسة عشر با.ئة في الربع الثالث. زرنا مدينة البتراء ا?ثرية في جنوب ا?ردن.";
        let arabic = "ارتفعت المبيعات بنسبة خمسة عشر بالمئة في الربع الثالث. زرنا مدينة البتراء الأثرية في جنوب الأردن.";
        assert_eq!(PageTextProblem::check(glyph_order), Some(PageTextProblem::GlyphOrder));
        assert_eq!(PageTextProblem::check(bengali), None);
        assert_eq!(PageTextProblem::check(lost), Some(PageTextProblem::LostLetters));
        assert_eq!(PageTextProblem::check(arabic), None);
        assert_eq!(PageTextProblem::check("Revenue grew 20%. See p.4?No. Done."), None);
    }

    #[test]
    fn real_arabic_abbreviations_are_not_lost_letters() {
        assert_eq!(PageTextProblem::check("أ.د.محمد علي، أستاذ في الجامعة. د.خالد و د.سارة من شركة ذ.م.م للتجارة"), None);
        let bullets = "\u{F0B7} Item one of the slide\n".repeat(6);
        assert_eq!(PageTextProblem::check(&bullets), None);
    }

    #[test]
    fn landmark_finds_the_page_in_preview() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let pages = s(&[
            "Quarterly business review\nRevenue grew in Japan and Europe.",
            "Hiking trip to Mount Fuji\nWe took the Yoshida trail from the fifth station in Japan.",
        ]);
        assert_eq!(landmark_find(2, &pages, &s(&["japan", "Yoshida"])).as_deref(), Some("Yoshida"));
        assert_eq!(landmark_find(2, &pages, &s(&["japan", "bread"])).as_deref(), Some("Japan"));
        assert_eq!(landmark_find(2, &pages, &s(&["bread"])), None);
        assert_eq!(landmark_find(1, &pages, &s(&["japan"])), None);
        let arabic = s(&["التقرير الربعي", "رحلة إلى البتراء\nزرنا مدينة البتراء الأثرية في جنوب الأردن"]);
        assert_eq!(landmark_find(2, &arabic, &s(&["الاردن"])).as_deref(), Some("الأردن"));
        let garbled = s(&["التقرير الربعي", "زرنا مدينة البتراء ا?ثرية في جنوب ا?ردن"]);
        assert_eq!(landmark_find(2, &garbled, &s(&["الاردن"])), None);
    }

    #[test]
    fn matches_are_found_as_keyword_search_finds_them_but_shown_as_written() {
        let arabic = "زرنا مدينة البتراء الأثرية في جنوب الأردن.";
        let found: Vec<&str> = matches("الاردن", arabic).into_iter().map(|r| &arabic[r]).collect();
        assert_eq!(found, ["الأردن"]);
        let voweled = "إِنَّ الْمَكْتَبَةَ تُغْلَقُ";
        let got: Vec<&str> = matches("المكتبة", voweled).into_iter().map(|r| &voweled[r]).collect();
        assert_eq!(got, ["الْمَكْتَبَةَ"]);
        let bengali = "একটি বড় কুমিরের ছবি";
        let got: Vec<&str> = matches("কুমির", bengali).into_iter().map(|r| &bengali[r]).collect();
        assert_eq!(got, ["কুমিরের"]);
        let english = "Sunday trip: day two";
        let got: Vec<&str> = matches("day", english).into_iter().map(|r| &english[r]).collect();
        assert_eq!(got, ["day"]);
        let turkish = "İ".repeat(60) + " son kelime";
        let got: Vec<&str> = matches("kelime", &turkish).into_iter().map(|r| &turkish[r]).collect();
        assert_eq!(got, ["kelime"]);
    }

    #[test]
    fn snippets_quote_the_text_as_written() {
        let text = "قَالَ مُحَمَّدٌ إِنَّ الْمَكْتَبَةَ الْعَامَّةَ تُغْلَقُ فِي السَّاعَةِ الثَّامِنَةِ مَسَاءً يَوْمَ الْخَمِيسِ";
        let snip = snippet(text, &["المكتبة"], 12).unwrap();
        assert!(snip.contains("«الْمَكْتَبَةَ»"), "{snip}");
        assert_eq!(snippet(text, &["كلمة"], 12), None);
    }
}
