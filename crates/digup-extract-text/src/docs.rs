//! Document readers and 1800-char chunker (roadmap C3).
//!
//! Port of `DocExtractor` (reference/digup-swift/Sources/DigUpKit/Extractors.swift:161-246) and the
//! `SearchText.hasWords` gate (SearchText.swift:37-43).
//!
//! Character counting: every size, index and overlap in the chunker counts grapheme clusters
//! (`unicode-segmentation`, extended grapheme clusters), as Swift's `String.count` and `String.Index`
//! do. Byte offsets are only used internally to slice the text.
//!
//! Pure functions only: no database, no embedder. Chunk indices are 0-based; the caller adds 1 when it
//! stores the Swift-style 1-based `loc`.

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::OnceLock;

use ego_tree::NodeRef;
use quick_xml::events::Event;
use quick_xml::Reader;
use regex::Regex;
use scraper::{Html, Node};
use unicode_segmentation::UnicodeSegmentation;

/// Characters per chunk (Swift `DocExtractor.chunks` default `size`).
pub const CHUNK_SIZE: usize = 1800;
/// Characters shared by consecutive chunks (Swift default `overlap`).
pub const CHUNK_OVERLAP: usize = 200;
/// Hard cap on chunks per document. `chunks_range` never returns more than this many.
pub const MAX_CHUNKS: usize = 5000;

/// One chunk of a document. `index` is 0-based and global within the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocChunk {
    pub index: usize,
    pub text: String,
}

/// Errors from document extraction.
#[derive(Debug)]
pub enum DocError {
    /// Format we do not read (legacy binary `.doc`, or an unknown extension).
    Unsupported(String),
    /// File could not be read.
    Io(io::Error),
    /// File claims to be a format we read, but its contents are malformed (bad zip, bad XML).
    Format(String),
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocError::Unsupported(what) => write!(f, "unsupported document: {what}"),
            DocError::Io(e) => write!(f, "read failed: {e}"),
            DocError::Format(what) => write!(f, "malformed document: {what}"),
        }
    }
}

impl std::error::Error for DocError {}

impl From<io::Error> for DocError {
    fn from(e: io::Error) -> Self {
        DocError::Io(e)
    }
}

fn xml_err(e: impl fmt::Display) -> DocError {
    DocError::Format(e.to_string())
}

/// Plain text of a document, dispatched on the file extension (case-insensitive).
///
/// - `txt`, `text`, `md`, `markdown`: UTF-8, falling back to Latin-1 (Swift `readText`).
/// - `html`, `htm`: [`plain_text_from_html`].
/// - `rtf`: [`rtf_to_text`].
/// - `docx`, `odt`: zip plus XML, see [`docx_text`] and [`odt_text`].
/// - `doc` (legacy binary Word): `DocError::Unsupported`.
pub fn extract_text(path: &Path) -> Result<String, DocError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "text" | "md" | "markdown" => read_plain(path),
        "html" | "htm" => Ok(plain_text_from_html(&read_plain(path)?)),
        "rtf" => Ok(rtf_to_text(&fs::read(path)?)),
        "docx" => docx_text(path),
        "odt" => odt_text(path),
        "doc" => Err(DocError::Unsupported(
            "legacy binary .doc is not supported (convert to .docx)".into(),
        )),
        other => Err(DocError::Unsupported(format!("extension .{other}"))),
    }
}

fn read_plain(path: &Path) -> Result<String, DocError> {
    let bytes = fs::read(path)?;
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        // Latin-1 fallback: every byte is one code point.
        Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
    };
    Ok(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

// ---------------------------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------------------------

/// Tags whose end adds a line break (Swift `plainText(fromHTML:)` block-end regex).
const BLOCK_END_TAGS: &[&str] = &[
    "p", "div", "li", "h1", "h2", "h3", "h4", "h5", "h6", "tr", "section", "article",
];

/// Plain text of an HTML document, matching Swift `DocExtractor.plainText(fromHTML:)`.
///
/// The DOM is walked (html5ever via `scraper`), so `<script>`, `<style>`, `<head>` and comments are dropped,
/// block ends (and `<br>`) become `\n`, every other element boundary becomes a space, and entities are
/// decoded by the parser (all named and numeric entities, not just Swift's list). Whitespace is then
/// collapsed: runs of space/tab/NBSP become one space, whitespace around newlines becomes one newline,
/// and the result is trimmed.
pub fn plain_text_from_html(html: &str) -> String {
    let doc = Html::parse_document(html);
    let mut raw = String::with_capacity(html.len() / 2);
    walk_html(doc.tree.root(), &mut raw);
    collapse_whitespace(&raw)
}

fn walk_html(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(text) => out.push_str(&text.text),
        Node::Element(element) => {
            let name = element.name();
            if matches!(name, "script" | "style" | "head" | "template") {
                return;
            }
            for child in node.children() {
                walk_html(child, out);
            }
            if name == "br" || BLOCK_END_TAGS.contains(&name) {
                out.push('\n');
            } else {
                out.push(' ');
            }
        }
        Node::Document | Node::Fragment => {
            for child in node.children() {
                walk_html(child, out);
            }
        }
        // Comments, doctype and processing instructions produce no text.
        _ => {}
    }
}

/// Swift's `[ \t]+` -> " ", `\s*\n\s*` -> "\n", then trim (NBSP treated as a space, as the entity decode did).
fn collapse_whitespace(text: &str) -> String {
    static SPACES: OnceLock<Regex> = OnceLock::new();
    static NEWLINES: OnceLock<Regex> = OnceLock::new();
    let spaces = SPACES.get_or_init(|| Regex::new(r"[ \t\x{A0}]+").expect("valid regex"));
    let newlines = NEWLINES.get_or_init(|| Regex::new(r"\s*\n\s*").expect("valid regex"));
    let spaced = spaces.replace_all(text, " ");
    newlines.replace_all(&spaced, "\n").trim().to_string()
}

// ---------------------------------------------------------------------------------------------
// RTF (small own reader: groups, control words, \'hh, \uN, a few symbols; no Cocoa)
// ---------------------------------------------------------------------------------------------

/// Windows code page for an RTF `\fcharset` value.
fn rtf_charset_codepage(charset: i64) -> Option<i64> {
    Some(match charset {
        0 => 1252,
        128 => 932,
        129 => 949,
        134 => 936,
        136 => 950,
        161 => 1253,
        162 => 1254,
        163 => 1258,
        177 => 1255,
        178 => 1256,
        186 => 1257,
        204 => 1251,
        222 => 874,
        238 => 1250,
        _ => return None,
    })
}

/// Font number to code page, from the `\fcharset` of each entry of the font table.
fn rtf_font_charsets(data: &[u8]) -> std::collections::HashMap<i64, i64> {
    let mut map = std::collections::HashMap::new();
    let Some(start) = data.windows(8).position(|w| w == b"\\fonttbl") else { return map };
    let mut depth = 0i32;
    let mut font: Option<i64> = None;
    let mut i = start;
    let mut opened = false;
    while i < data.len() {
        match data[i] {
            b'{' => {
                depth += 1;
                opened = true;
            }
            b'}' => {
                depth -= 1;
                if opened && depth < 0 {
                    break;
                }
            }
            b'\\' => {
                if data.get(i + 1).is_some_and(|b| !b.is_ascii_alphabetic()) {
                    i += 2; // an escaped `\{`, `\}` or `\\` is text, not a group brace
                    continue;
                }
                let rest = &data[i + 1..];
                let word_len = rest.iter().take_while(|b| b.is_ascii_alphabetic()).count();
                let word = &rest[..word_len];
                let digits_start = i + 1 + word_len;
                let digits_len = data[digits_start.min(data.len())..].iter().take_while(|b| b.is_ascii_digit()).count();
                let num = std::str::from_utf8(&data[digits_start..digits_start + digits_len]).ok().and_then(|s| s.parse::<i64>().ok());
                if word == b"f" {
                    font = num;
                } else if word == b"fcharset" {
                    if let (Some(f), Some(c)) = (font, num.and_then(rtf_charset_codepage)) {
                        map.insert(f, c);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    map
}

/// The number after the first `word` (given with its backslash) in `data`.
fn rtf_number(data: &[u8], word: &[u8]) -> Option<i64> {
    let at = data.windows(word.len()).position(|w| w == word)? + word.len();
    let len = data[at..].iter().take_while(|b| b.is_ascii_digit()).count();
    std::str::from_utf8(&data[at..at + len]).ok()?.parse().ok()
}

/// Decodes the pending `\'hh` bytes with the Windows code page and appends them as UTF-8.
fn rtf_flush(out: &mut Vec<u8>, pending: &mut Vec<u8>, codepage: i64) {
    let enc = match codepage {
        932 => encoding_rs::SHIFT_JIS,
        936 => encoding_rs::GBK,
        949 => encoding_rs::EUC_KR,
        950 => encoding_rs::BIG5,
        874 => encoding_rs::WINDOWS_874,
        1250 => encoding_rs::WINDOWS_1250,
        1251 => encoding_rs::WINDOWS_1251,
        1253 => encoding_rs::WINDOWS_1253,
        1254 => encoding_rs::WINDOWS_1254,
        1255 => encoding_rs::WINDOWS_1255,
        1256 => encoding_rs::WINDOWS_1256,
        1257 => encoding_rs::WINDOWS_1257,
        1258 => encoding_rs::WINDOWS_1258,
        _ => encoding_rs::WINDOWS_1252,
    };
    let (text, _, _) = enc.decode(pending);
    out.extend_from_slice(text.as_bytes());
    pending.clear();
}

/// Destinations whose content is not document text.
const RTF_SKIP_DESTINATIONS: &[&[u8]] = &[
    b"fonttbl", b"colortbl", b"stylesheet", b"info", b"pict", b"object", b"fldinst", b"header", b"footer",
    b"headerl", b"headerr", b"footerl", b"footerr", b"generator", b"xmlnstbl", b"listtable", b"listoverridetable",
];

/// Plain text of an RTF document. Paragraph and line controls become `\n`, `\tab` becomes a tab,
/// `\'hh` and `\uN` are decoded (the font's code page for `\'hh`), and the `\uc` fallback characters are skipped.
/// Font, colour, style and other destination groups are dropped. Output is trimmed at the end.
pub fn rtf_to_text(data: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(data.len() / 2);
    let mut stack: Vec<(bool, i64, usize)> = Vec::new();
    let mut uc_len = 1usize; // `\ucN`: fallback units after each `\u`
    let mut high_surrogate: Option<u32> = None;
    // `\'hh` bytes are in the code page of the current font's `\fcharset` (else `\ansicpg`, else 1252); runs of them
    // are decoded together so multi-byte pages (Shift-JIS, GBK) work.
    let fonts = rtf_font_charsets(data);
    let default_cp = rtf_number(data, b"\\ansicpg").unwrap_or(1252);
    let mut font: i64 = rtf_number(data, b"\\deff").unwrap_or(0);
    let mut pending: Vec<u8> = Vec::new();
    let mut pending_cp = default_cp;
    let mut skip = false;
    // Number of fallback units still to skip after a `\uN` (default `\uc1`).
    let mut uc_skip = 0usize;
    let mut i = 0usize;
    let n = data.len();

    while i < n {
        if !pending.is_empty() && !matches!(data[i], b'\r' | b'\n') && !(data[i] == b'\\' && data.get(i + 1) == Some(&b'\'')) {
            rtf_flush(&mut out, &mut pending, pending_cp);
        }
        match data[i] {
            b'{' => {
                stack.push((skip, font, uc_len));
                i += 1;
            }
            b'}' => {
                (skip, font, uc_len) = stack.pop().unwrap_or((false, font, uc_len));
                i += 1;
            }
            b'\r' | b'\n' => i += 1, // raw line breaks carry no meaning in RTF
            b'\\' => {
                i += 1;
                if i >= n {
                    break;
                }
                let d = data[i];
                if d.is_ascii_alphabetic() {
                    let start = i;
                    while i < n && data[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word = &data[start..i];
                    let mut param: Option<i64> = None;
                    if i < n && (data[i] == b'-' || data[i].is_ascii_digit()) {
                        let ps = i;
                        if data[i] == b'-' {
                            i += 1;
                        }
                        while i < n && data[i].is_ascii_digit() {
                            i += 1;
                        }
                        param = std::str::from_utf8(&data[ps..i]).ok().and_then(|s| s.parse().ok());
                    }
                    if i < n && data[i] == b' ' {
                        i += 1; // single space delimiter belongs to the control word
                    }
                    if RTF_SKIP_DESTINATIONS.contains(&word) {
                        skip = true;
                        continue;
                    }
                    if skip {
                        continue;
                    }
                    if word == b"f" {
                        if let Some(p) = param {
                            font = p;
                        }
                        continue;
                    }
                    match word {
                        b"uc" => uc_len = param.unwrap_or(1).max(0) as usize,
                        b"par" | b"line" | b"sect" | b"row" | b"page" => push_char(&mut out, '\n'),
                        b"tab" => push_char(&mut out, '\t'),
                        b"emdash" => push_char(&mut out, '\u{2014}'),
                        b"endash" => push_char(&mut out, '\u{2013}'),
                        b"lquote" => push_char(&mut out, '\u{2018}'),
                        b"rquote" => push_char(&mut out, '\u{2019}'),
                        b"ldblquote" => push_char(&mut out, '\u{201C}'),
                        b"rdblquote" => push_char(&mut out, '\u{201D}'),
                        b"bullet" => push_char(&mut out, '\u{2022}'),
                        b"u" => {
                            if let Some(p) = param {
                                let code = (if p < 0 { p + 65536 } else { p }) as u32;
                                // Astral characters arrive as two `\u` surrogates.
                                if (0xD800..0xDC00).contains(&code) {
                                    high_surrogate = Some(code);
                                } else {
                                    let scalar = match high_surrogate.take() {
                                        Some(h) if (0xDC00..0xE000).contains(&code) => {
                                            Some(0x10000 + ((h - 0xD800) << 10) + (code - 0xDC00))
                                        }
                                        _ => Some(code),
                                    };
                                    if let Some(c) = scalar.and_then(char::from_u32) {
                                        push_char(&mut out, c);
                                    }
                                }
                                uc_skip = uc_len;
                            }
                        }
                        _ => {}
                    }
                } else {
                    i += 1;
                    match d {
                        b'\\' | b'{' | b'}' => {
                            if !skip && uc_skip == 0 {
                                out.push(d);
                            } else {
                                uc_skip = uc_skip.saturating_sub(1);
                            }
                        }
                        b'\'' => {
                            let hex = data.get(i..i + 2).and_then(|h| std::str::from_utf8(h).ok());
                            let byte = hex.and_then(|h| u8::from_str_radix(h, 16).ok());
                            i += 2;
                            if let Some(b) = byte {
                                if uc_skip > 0 {
                                    uc_skip -= 1;
                                } else if !skip {
                                    let cp = fonts.get(&font).copied().unwrap_or(default_cp);
                                    if cp != pending_cp {
                                        rtf_flush(&mut out, &mut pending, pending_cp);
                                        pending_cp = cp;
                                    }
                                    pending.push(b);
                                }
                            }
                        }
                        b'*' => skip = true, // ignorable destination
                        b'~' => {
                            if !skip {
                                out.push(b' ');
                            }
                        }
                        b'-' | b'_' => {}
                        b'\n' | b'\r' if !skip => push_char(&mut out, '\n'),
                        _ => {}
                    }
                }
            }
            byte => {
                i += 1;
                if skip {
                    continue;
                }
                if uc_skip > 0 {
                    uc_skip -= 1;
                    continue;
                }
                out.push(byte);
            }
        }
    }
    rtf_flush(&mut out, &mut pending, pending_cp);
    String::from_utf8_lossy(&out).trim().to_string()
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

// ---------------------------------------------------------------------------------------------
// DOCX and ODT (zip + quick-xml)
// ---------------------------------------------------------------------------------------------

fn read_zip_entry(path: &Path, name: &str) -> Result<String, DocError> {
    let file = fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(xml_err)?;
    let mut entry = archive.by_name(name).map_err(xml_err)?;
    let mut xml = String::new();
    entry.read_to_string(&mut xml)?;
    Ok(xml)
}

/// Text of a .docx: `w:t` runs, `w:tab` as a tab, `w:br`/`w:cr` and the end of each `w:p` as `\n`.
/// Output is trimmed at the end.
pub fn docx_text(path: &Path) -> Result<String, DocError> {
    let xml = read_zip_entry(path, "word/document.xml")?;
    let mut reader = Reader::from_str(&xml);
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Start(e) => {
                if e.name().as_ref() == b"w:t" {
                    in_text = true;
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"w:t" => in_text = false,
                b"w:p" => out.push('\n'),
                _ => {}
            },
            Event::Empty(e) => match e.name().as_ref() {
                b"w:tab" => out.push('\t'),
                b"w:br" | b"w:cr" => out.push('\n'),
                _ => {}
            },
            Event::Text(t) => {
                if in_text {
                    out.push_str(&t.unescape().map_err(xml_err)?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out.trim_end().to_string())
}

/// Text of an .odt (`content.xml`): paragraphs and headings end with `\n`, `text:s` becomes `text:c` spaces
/// (default 1), `text:tab` a tab, `text:line-break` a newline. Output is trimmed at the end.
pub fn odt_text(path: &Path) -> Result<String, DocError> {
    let xml = read_zip_entry(path, "content.xml")?;
    let mut reader = Reader::from_str(&xml);
    let mut out = String::new();
    let mut paragraph_depth = 0u32;
    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Start(e) => {
                if matches!(e.name().as_ref(), b"text:p" | b"text:h") {
                    paragraph_depth += 1;
                }
            }
            Event::End(e) => {
                if matches!(e.name().as_ref(), b"text:p" | b"text:h") {
                    paragraph_depth = paragraph_depth.saturating_sub(1);
                    out.push('\n');
                }
            }
            Event::Empty(e) => match e.name().as_ref() {
                b"text:s" => {
                    let count = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == b"text:c")
                        .and_then(|a| a.unescape_value().ok())
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1);
                    out.extend(std::iter::repeat_n(' ', count));
                }
                b"text:tab" => out.push('\t'),
                b"text:line-break" => out.push('\n'),
                b"text:p" | b"text:h" => out.push('\n'),
                _ => {}
            },
            Event::Text(t) => {
                if paragraph_depth > 0 {
                    out.push_str(&t.unescape().map_err(xml_err)?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out.trim_end().to_string())
}

// ---------------------------------------------------------------------------------------------
// Chunker
// ---------------------------------------------------------------------------------------------

/// Swift `DocExtractor.chunks` pre-clean: `[ \t\u{A0}]+` -> " ", `\n\s*\n\s*\n+` -> "\n\n", trim.
fn normalise(text: &str) -> String {
    static SPACES: OnceLock<Regex> = OnceLock::new();
    static BLANK_LINES: OnceLock<Regex> = OnceLock::new();
    let spaces = SPACES.get_or_init(|| Regex::new(r"[ \t\x{A0}]+").expect("valid regex"));
    let blank = BLANK_LINES.get_or_init(|| Regex::new(r"\n\s*\n\s*\n+").expect("valid regex"));
    let spaced = spaces.replace_all(text, " ");
    blank.replace_all(&spaced, "\n\n").trim().to_string()
}

/// Core chunking loop, a port of Swift `DocExtractor.chunks` (Extractors.swift:222-245).
///
/// Sizes are in grapheme clusters. Returns the trimmed, non-empty chunk slices in order, stopping after
/// `limit` of them. Each chunk ends at the last whitespace in its window if that lies past half the size;
/// the next chunk starts `overlap` graphemes back, moved forward to the next whitespace.
fn split(clean: &str, size: usize, overlap: usize, limit: usize) -> Vec<&str> {
    let mut starts: Vec<usize> = clean.grapheme_indices(true).map(|(i, _)| i).collect();
    let n = starts.len();
    starts.push(clean.len());
    let is_ws = |i: usize| clean[starts[i]..starts[i + 1]].chars().next().is_some_and(char::is_whitespace);

    let mut out: Vec<&str> = Vec::new();
    let mut start = 0usize;
    while start < n && out.len() < limit {
        let mut end = (start + size).min(n);
        if end < n {
            if let Some(space) = (start..end).rev().find(|&i| is_ws(i)) {
                if space - start > size / 2 {
                    end = space;
                }
            }
        }
        let chunk = clean[starts[start]..starts[end]].trim();
        if !chunk.is_empty() {
            out.push(chunk);
        }
        if end >= n {
            break;
        }
        // index(end, offsetBy: -overlap, limitedBy: start) is nil when it would pass `start`.
        let mut next = if end >= start + overlap { end - overlap } else { end };
        if next <= start {
            next = end;
        }
        // Start the next chunk on a word boundary (lands on the whitespace grapheme).
        while next < end && !is_ws(next) {
            next += 1;
        }
        start = if next < end { next } else { end };
    }
    out
}

/// Chunks `text` with explicit size, overlap and count limit (Swift `DocExtractor.chunks`).
/// Sizes are grapheme clusters. Indices run from 0.
pub fn chunks(text: &str, size: usize, overlap: usize, limit: usize) -> Vec<DocChunk> {
    let clean = normalise(text);
    split(&clean, size, overlap, limit)
        .into_iter()
        .enumerate()
        .map(|(index, t)| DocChunk { index, text: t.to_string() })
        .collect()
}

/// Chunks at the production settings (1800 / 200, at most `MAX_CHUNKS`), returning indices `from..to`
/// (clamped to the available chunks). Chunking is deterministic, so `chunks_range(t, 0, 40)` followed by
/// `chunks_range(t, 40, n)` yields exactly the same chunks as one full call.
pub fn chunks_range(text: &str, from: usize, to: usize) -> Vec<DocChunk> {
    let clean = normalise(text);
    let parts = split(&clean, CHUNK_SIZE, CHUNK_OVERLAP, MAX_CHUNKS);
    let to = to.min(parts.len());
    let from = from.min(to);
    parts[from..to]
        .iter()
        .enumerate()
        .map(|(k, t)| DocChunk { index: from + k, text: t.to_string() })
        .collect()
}

/// Chunk count at production settings, capped at `MAX_CHUNKS + 1`. The extra one tells the caller the
/// document was truncated (Swift `limit = maxChunks + 1`).
pub fn total_chunks(text: &str) -> usize {
    let clean = normalise(text);
    split(&clean, CHUNK_SIZE, CHUNK_OVERLAP, MAX_CHUNKS + 1).len()
}

/// Swift `SearchText.hasWords`: true when letters (general categories L* and M*, per Unicode scalar)
/// are at least 20% of the non-whitespace scalars, i.e. `letters > 0 && letters * 4 >= others`.
/// Text that fails this (number dumps, tables) gets keyword search only, no embedding.
pub fn has_words(text: &str) -> bool {
    static LETTERS: OnceLock<Regex> = OnceLock::new();
    let letter_re = LETTERS.get_or_init(|| Regex::new(r"[\p{L}\p{M}]").expect("valid regex"));
    let letters = letter_re.find_iter(text).count();
    let non_space = text.chars().filter(|c| !c.is_whitespace()).count();
    let others = non_space - letters;
    letters > 0 && letters * 4 >= others
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_zip(path: &Path, entries: &[(&str, &str)]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, body) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn chunks_overlap_and_respect_words() {
        let text = (1..=600).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ");
        let got = chunks(&text, 500, 100, 100);
        assert!(got.len() > 5);
        assert!(got.iter().all(|c| c.text.graphemes(true).count() <= 500));
        assert!(got.iter().all(|c| c.text.starts_with("word")));
        assert!(got.last().unwrap().text.ends_with("word600"));
        assert!(got.iter().enumerate().all(|(i, c)| c.index == i));
    }

    #[test]
    fn chunk_limit_is_respected() {
        let text = (1..=600).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ");
        assert_eq!(chunks(&text, 10, 0, 5).len(), 5);
    }

    #[test]
    fn range_pieces_equal_full_chunking() {
        let mut text = String::new();
        while text.len() < 150_000 {
            text.push_str("lorem ipsum dolor sit amet consectetur ");
        }
        let total = total_chunks(&text);
        let full = chunks_range(&text, 0, total);
        let first = chunks_range(&text, 0, 40);
        let rest = chunks_range(&text, 40, total);
        assert_eq!(first.len(), 40);
        assert_eq!(first.len() + rest.len(), full.len());
        let joined: Vec<DocChunk> = first.into_iter().chain(rest).collect();
        assert_eq!(joined, full);
    }

    #[test]
    fn long_document_keyword_only_past_chunk_40() {
        let mut text = "lorem ipsum ".repeat(10_000); // 120,000 chars
        text.push_str("axolotl ");
        while text.len() < 150_000 {
            text.push_str("dolor sit amet ");
        }
        let total = total_chunks(&text);
        assert!(total > 40);
        assert!(chunks_range(&text, 0, 40).iter().all(|c| !c.text.contains("axolotl")));
        assert!(chunks_range(&text, 40, total).iter().any(|c| c.text.contains("axolotl")));
    }

    #[test]
    fn html_becomes_plain_text() {
        let html = r#"<html><head><title>T</title><style>p { color: red }</style></head>
            <body><h1>Trip &amp; plans</h1><p>Fly to <b>Tokyo</b>&nbsp;on Friday.</p><script>var x = 1;</script>
            <p>Caf&#233; &#x2014; ramen</p><!-- hidden --></body></html>"#;
        assert_eq!(plain_text_from_html(html), "Trip & plans\nFly to Tokyo on Friday.\nCafé — ramen");
    }

    #[test]
    fn has_words_gate() {
        let digits: String = (0..4000).map(|i| format!("{}\n", 4_815_162_342u64 + i)).collect();
        assert!(!has_words(&digits));
        assert!(has_words("Revenue 1,234,567 · Net income 234,567 · Margin 19% in the third quarter"));
        assert!(has_words("কলকাতায় বৃষ্টি হচ্ছে"));
        assert!(!has_words("   "));
    }

    #[test]
    fn plain_text_file_falls_back_to_latin1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, [b'C', b'a', b'f', 0xE9]).unwrap();
        assert_eq!(extract_text(&path).unwrap(), "Café");
    }

    #[test]
    fn rtf_arabic_charset_font() {
        let rtf = r"{\rtf1\ansi\ansicpg1252{\fonttbl\f0\fnil\fcharset178 GeezaPro;}\f0 \'e3\'cd\'d6\'d1 \'c7\'cc\'ca\'e3\'c7\'da}";
        assert_eq!(rtf_to_text(rtf.as_bytes()), "محضر اجتماع");
    }

    #[test]
    fn rtf_unicode_fallback_and_surrogates() {
        // \uc0: no fallback unit follows; astral character as a surrogate pair; \uc1 default elsewhere.
        assert_eq!(rtf_to_text(r"{\rtf1\uc0\u8364 Hello \u-10179\u-8704}".as_bytes()), "€Hello \u{1F600}");
        assert_eq!(rtf_to_text(r"{\rtf1\u8364?x}".as_bytes()), "€x");
    }

    #[test]
    fn rtf_multibyte_survives_line_wrap() {
        let rtf = "{\\rtf1\\ansi\\ansicpg932{\\fonttbl\\f0\\fcharset128 MS Mincho;}\\f0 \\'93\\'fa\r\n\\'96\\'7b}";
        assert_eq!(rtf_to_text(rtf.as_bytes()), "日本");
    }

    #[test]
    fn rtf_plain_text() {
        let rtf = r"{\rtf1\ansi{\fonttbl{\f0 Times;}}Caf\'e9 \b bold\b0 .\par Second\par}";
        assert_eq!(rtf_to_text(rtf.as_bytes()).trim(), "Café bold.\nSecond");
    }

    #[test]
    fn docx_fixture_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.docx");
        let xml = r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
            <w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t xml:space="preserve"> world</w:t></w:r><w:r><w:tab/><w:t>Tabbed</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second &amp; last</w:t></w:r></w:p></w:body></w:document>"#;
        write_zip(&path, &[("[Content_Types].xml", "<Types/>"), ("word/document.xml", xml)]);
        assert_eq!(extract_text(&path).unwrap(), "Hello world\tTabbed\nSecond & last");
    }

    #[test]
    fn odt_fixture_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.odt");
        let xml = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text>
            <text:h text:outline-level="1">Title</text:h>
            <text:p>Some <text:span>styled</text:span> words<text:s text:c="2"/>here</text:p>
            <text:p>Line<text:line-break/>two</text:p></office:text></office:body></office:document-content>"#;
        write_zip(&path, &[("mimetype", "application/vnd.oasis.opendocument.text"), ("content.xml", xml)]);
        let text = extract_text(&path).unwrap();
        assert_eq!(text, "Title\nSome styled words  here\nLine\ntwo");
    }

    #[test]
    fn legacy_doc_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.doc");
        fs::write(&path, b"\xD0\xCF\x11\xE0").unwrap();
        assert!(matches!(extract_text(&path), Err(DocError::Unsupported(_))));
    }
}
