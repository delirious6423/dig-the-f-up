//! Query-time filters typed inside the search query: `ext:`, `size:` and `mtime:`.
//!
//! Tokens are whitespace-separated and keys are case-insensitive. A token whose key is known and whose value parses is
//! taken out of the query text; anything else (an unknown key, a bad value, no colon) stays in the text unchanged.
//!
//! - `ext:pdf`, `ext:pdf,docx`, `ext:.PDF`: extensions (case-insensitive, leading dot optional).
//! - `size:>10mb`, `size:<500kb`, `size:1mb..5mb`: sizes with units b, kb, mb, gb (base 1024). `>` and `<` are strict;
//!   a range is inclusive. Units are required.
//! - `mtime:7d`: modified within the last 7 days. `mtime:>2024-01-01` and `mtime:<2024-06-01`: modified after or
//!   before a UTC date. `mtime:2024-01-01..2024-02-01`: between two dates. Relative ages take units d, w (7 days),
//!   m (30 days) and y (365 days); `mtime:>7d` means modified within 7 days, `mtime:<7d` means older than 7 days.
//!
//! Several tokens of one key narrow the result (`size:>1kb size:<2kb`), except several `ext:` tokens, which add up.

use std::collections::HashSet;

const KB: f64 = 1024.0;
const DAY: f64 = 86_400.0;

/// The filters of one query. An empty value matches every file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filters {
    /// Lowercase extensions without the dot; a file matches when its extension is one of them.
    pub exts: Option<HashSet<String>>,
    /// Inclusive byte bounds.
    pub size_min: Option<i64>,
    pub size_max: Option<i64>,
    /// Inclusive modification times, seconds since 1970 (dates are UTC midnights).
    pub mtime_min: Option<f64>,
    pub mtime_max: Option<f64>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.exts.is_none()
            && self.size_min.is_none()
            && self.size_max.is_none()
            && self.mtime_min.is_none()
            && self.mtime_max.is_none()
    }

    /// Whether a file with this path, size in bytes and modification time (seconds since 1970) may be returned.
    pub fn matches(&self, path: &str, size: i64, mtime: f64) -> bool {
        if let Some(exts) = &self.exts {
            match extension(path) {
                Some(e) if exts.contains(&e) => {}
                _ => return false,
            }
        }
        if self.size_min.is_some_and(|m| size < m) || self.size_max.is_some_and(|m| size > m) {
            return false;
        }
        if self.mtime_min.is_some_and(|m| mtime < m) || self.mtime_max.is_some_and(|m| mtime > m) {
            return false;
        }
        true
    }

    /// One line for the CLI, e.g. `ext pdf, docx · size 1048576..5242880 B · mtime 2024-01-01..-`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(exts) = &self.exts {
            let mut list: Vec<&str> = exts.iter().map(String::as_str).collect();
            list.sort_unstable();
            parts.push(format!("ext {}", list.join(", ")));
        }
        if self.size_min.is_some() || self.size_max.is_some() {
            let lo = self.size_min.map_or("-".to_string(), |v| v.to_string());
            let hi = self.size_max.map_or("-".to_string(), |v| v.to_string());
            parts.push(format!("size {lo}..{hi} B"));
        }
        if self.mtime_min.is_some() || self.mtime_max.is_some() {
            let lo = self.mtime_min.map_or("-".to_string(), iso_date);
            let hi = self.mtime_max.map_or("-".to_string(), iso_date);
            parts.push(format!("mtime {lo}..{hi}"));
        }
        parts.join(" · ")
    }
}

/// Splits the filter tokens out of `query`. Returns the remaining query text and the filters. `now` is seconds since
/// 1970 and anchors the relative times.
pub fn parse(query: &str, now: f64) -> (String, Filters) {
    let mut filters = Filters::default();
    let mut rest: Vec<&str> = Vec::new();
    for token in query.split_whitespace() {
        if !take(token, now, &mut filters) {
            rest.push(token);
        }
    }
    (rest.join(" "), filters)
}

/// Applies one token to `f`; `false` when it is not a valid filter token.
fn take(token: &str, now: f64, f: &mut Filters) -> bool {
    let Some((key, value)) = token.split_once(':') else {
        return false;
    };
    match key.to_ascii_lowercase().as_str() {
        "ext" => match parse_exts(value) {
            Some(list) => {
                f.exts.get_or_insert_with(HashSet::new).extend(list);
                true
            }
            None => false,
        },
        "size" => match parse_size(value) {
            Some((lo, hi)) => {
                tighten(&mut f.size_min, &mut f.size_max, lo, hi);
                true
            }
            None => false,
        },
        "mtime" => match parse_mtime(value, now) {
            Some((lo, hi)) => {
                tighten(&mut f.mtime_min, &mut f.mtime_max, lo, hi);
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// Narrows the bounds: a new lower bound raises the old one, a new upper bound lowers the old one.
fn tighten<T: PartialOrd + Copy>(min: &mut Option<T>, max: &mut Option<T>, lo: Option<T>, hi: Option<T>) {
    if let Some(lo) = lo {
        *min = Some(match *min {
            Some(m) if m > lo => m,
            _ => lo,
        });
    }
    if let Some(hi) = hi {
        *max = Some(match *max {
            Some(m) if m < hi => m,
            _ => hi,
        });
    }
}

fn extension(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next()?;
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    let ext = &name[dot + 1..];
    (!ext.is_empty()).then(|| ext.to_lowercase())
}

fn parse_exts(value: &str) -> Option<Vec<String>> {
    let list: Vec<String> =
        value.split(',').map(|e| e.strip_prefix('.').unwrap_or(e).to_ascii_lowercase()).collect();
    list.iter().all(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric())).then_some(list)
}

/// Inclusive byte bounds for a size value; `(None, None)` cannot occur.
fn parse_size(value: &str) -> Option<(Option<i64>, Option<i64>)> {
    if let Some(rest) = value.strip_prefix('>') {
        // size > x  <=>  size >= floor(x) + 1
        let x = size_bytes(rest)?;
        return Some((Some(x.floor() as i64 + 1), None));
    }
    if let Some(rest) = value.strip_prefix('<') {
        // size < x  <=>  size <= ceil(x) - 1
        let x = size_bytes(rest)?;
        return Some((None, Some(x.ceil() as i64 - 1)));
    }
    let (l, r) = value.split_once("..")?;
    let (a, b) = match (l.is_empty(), r.is_empty()) {
        (true, true) => return None,
        (true, false) => (None, Some(size_bytes(r)?)),
        (false, true) => (Some(size_bytes(l)?), None),
        (false, false) => {
            let (a, b) = (size_bytes(l)?, size_bytes(r)?);
            if a <= b {
                (Some(a), Some(b))
            } else {
                (Some(b), Some(a))
            }
        }
    };
    Some((a.map(|v| v.ceil() as i64), b.map(|v| v.floor() as i64)))
}

/// `10mb`, `1.5kb`: a non-negative number and a unit, as bytes.
fn size_bytes(s: &str) -> Option<f64> {
    let s = s.to_ascii_lowercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = s.split_at(split);
    let mult = match unit {
        "b" => 1.0,
        "kb" => KB,
        "mb" => KB * KB,
        "gb" => KB * KB * KB,
        _ => return None,
    };
    let n: f64 = num.parse().ok()?;
    (n.is_finite() && n >= 0.0).then_some(n * mult)
}

/// Inclusive time bounds for an mtime value.
fn parse_mtime(value: &str, now: f64) -> Option<(Option<f64>, Option<f64>)> {
    if let Some(rest) = value.strip_prefix('>') {
        return Some((Some(instant(rest, now)?), None));
    }
    if let Some(rest) = value.strip_prefix('<') {
        return Some((None, Some(instant(rest, now)?)));
    }
    if let Some((l, r)) = value.split_once("..") {
        return match (l.is_empty(), r.is_empty()) {
            (true, true) => None,
            (true, false) => Some((None, Some(instant(r, now)?))),
            (false, true) => Some((Some(instant(l, now)?), None)),
            (false, false) => {
                let (a, b) = (instant(l, now)?, instant(r, now)?);
                Some((Some(a.min(b)), Some(a.max(b))))
            }
        };
    }
    // A bare value is a relative age only: "within the last 7 days".
    Some((Some(now - age(value)?), None))
}

/// A relative age (`7d`, `2w`, `1m`, `1y`) or a date, as a time.
fn instant(s: &str, now: f64) -> Option<f64> {
    if let Some(a) = age(s) {
        return Some(now - a);
    }
    date(s)
}

/// `7d`, `2w`, `1m` (30 days), `1y` (365 days): seconds.
fn age(s: &str) -> Option<f64> {
    let s = s.to_ascii_lowercase();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(split);
    let days = match unit {
        "d" => 1.0,
        "w" => 7.0,
        "m" => 30.0,
        "y" => 365.0,
        _ => return None,
    };
    let n: f64 = num.parse().ok()?;
    Some(n * days * DAY)
}

/// `YYYY-MM-DD` as the UTC midnight, seconds since 1970.
fn date(s: &str) -> Option<f64> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 || !all_digits(y) || !all_digits(m) || !all_digits(d) {
        return None;
    }
    if m.len() > 2 || d.len() > 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d) as f64 * DAY)
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` (UTC) of a time in seconds since 1970.
fn iso_date(t: f64) -> String {
    let z = (t / DAY).floor() as i64 + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-06-15 12:00 UTC.
    const NOW: f64 = 1_718_452_800.0;

    fn p(q: &str) -> (String, Filters) {
        parse(q, NOW)
    }

    fn exts(list: &[&str]) -> Option<HashSet<String>> {
        Some(list.iter().map(|s| s.to_string()).collect())
    }

    fn day(y: i64, m: i64, d: i64) -> f64 {
        days_from_civil(y, m, d) as f64 * DAY
    }

    #[test]
    fn epoch_and_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(day(2024, 6, 15), NOW - 12.0 * 3600.0);
        assert_eq!(iso_date(day(2024, 2, 29)), "2024-02-29");
        assert_eq!(iso_date(day(1999, 12, 31)), "1999-12-31");
        assert_eq!(iso_date(-DAY), "1969-12-31");
    }

    #[test]
    fn plain_text_is_untouched() {
        for q in ["", "walrus", "  walrus   ice floe ", "note: hello", "code: foo", "ext:", "ext:pdf,"] {
            let (rest, f) = p(q);
            assert!(f.is_empty(), "{q:?}");
            assert_eq!(rest, q.split_whitespace().collect::<Vec<_>>().join(" "), "{q:?}");
        }
    }

    #[test]
    fn extension_tokens() {
        assert_eq!(p("ext:pdf"), (String::new(), Filters { exts: exts(&["pdf"]), ..Default::default() }));
        assert_eq!(p("ext:.PDF").1.exts, exts(&["pdf"]));
        assert_eq!(p("EXT:pdf,DOCX").1.exts, exts(&["pdf", "docx"]));
        assert_eq!(p("ext:.pdf,.docx").1.exts, exts(&["pdf", "docx"]));
        assert_eq!(p("ext:pdf ext:docx").1.exts, exts(&["pdf", "docx"]));
        assert_eq!(p("ext:..pdf").1.exts, None);
        assert_eq!(p("ext:tar.gz").1.exts, None);
        assert_eq!(p("ext:p-df").1.exts, None);
        assert_eq!(p("ext:,pdf").1.exts, None);
    }

    #[test]
    fn size_tokens() {
        assert_eq!(p("size:>10mb").1.size_min, Some(10 * 1024 * 1024 + 1));
        assert_eq!(p("size:>10MB").1.size_max, None);
        assert_eq!(p("Size:>1KB").1.size_min, Some(1025));
        assert_eq!(p("size:<500kb").1.size_max, Some(500 * 1024 - 1));
        assert_eq!(p("size:<1b").1.size_max, Some(0));
        assert_eq!(p("size:>0b").1.size_min, Some(1));
        assert_eq!(p("size:1mb..5mb").1.size_min, Some(1024 * 1024));
        assert_eq!(p("size:1mb..5mb").1.size_max, Some(5 * 1024 * 1024));
        assert_eq!(p("size:5mb..1mb").1.size_min, Some(1024 * 1024));
        assert_eq!(p("size:1.5gb..2gb").1.size_min, Some(1_610_612_736));
        assert_eq!(p("size:..5mb").1.size_min, None);
        assert_eq!(p("size:..5mb").1.size_max, Some(5 * 1024 * 1024));
        assert_eq!(p("size:2kb..").1.size_min, Some(2048));
        assert_eq!(p("size:>1kb size:<2kb").1.size_min, Some(1025));
        assert_eq!(p("size:>1kb size:<2kb").1.size_max, Some(2047));
    }

    #[test]
    fn bad_size_tokens_stay_in_text() {
        for q in [
            "size:10",
            "size:10tb",
            "size:>-1b",
            "size:1mb..5",
            "size:>1kb>2kb",
            "size:",
            "size:..",
            "size:1.2.3kb",
            "size:kb",
            "size:2gb..1mb..3gb",
        ] {
            let (rest, f) = p(q);
            assert!(f.is_empty(), "{q:?} should not be a filter");
            assert_eq!(rest, q);
        }
    }

    #[test]
    fn mtime_relative() {
        assert_eq!(p("mtime:7d").1.mtime_min, Some(NOW - 7.0 * DAY));
        assert_eq!(p("mtime:7d").1.mtime_max, None);
        assert_eq!(p("mtime:2w").1.mtime_min, Some(NOW - 14.0 * DAY));
        assert_eq!(p("mtime:1m").1.mtime_min, Some(NOW - 30.0 * DAY));
        assert_eq!(p("mtime:1y").1.mtime_min, Some(NOW - 365.0 * DAY));
        assert_eq!(p("mtime:>7d").1.mtime_min, Some(NOW - 7.0 * DAY));
        assert_eq!(p("mtime:<7d").1.mtime_max, Some(NOW - 7.0 * DAY));
        assert_eq!(p("mtime:7d..1d").1.mtime_min, Some(NOW - 7.0 * DAY));
        assert_eq!(p("mtime:7d..1d").1.mtime_max, Some(NOW - DAY));
    }

    #[test]
    fn mtime_dates() {
        let f = p("mtime:>2024-01-01").1;
        assert_eq!((f.mtime_min, f.mtime_max), (Some(day(2024, 1, 1)), None));
        let f = p("mtime:<2024-06-01").1;
        assert_eq!((f.mtime_min, f.mtime_max), (None, Some(day(2024, 6, 1))));
        let f = p("mtime:2024-01-01..2024-02-01").1;
        assert_eq!((f.mtime_min, f.mtime_max), (Some(day(2024, 1, 1)), Some(day(2024, 2, 1))));
        let f = p("mtime:2024-02-01..2024-01-01").1;
        assert_eq!((f.mtime_min, f.mtime_max), (Some(day(2024, 1, 1)), Some(day(2024, 2, 1))));
        let f = p("mtime:>2024-01-01 mtime:<2024-06-01").1;
        assert_eq!((f.mtime_min, f.mtime_max), (Some(day(2024, 1, 1)), Some(day(2024, 6, 1))));
        assert_eq!(p("mtime:2024-02-29").1.mtime_min, None);
        assert_eq!(p("mtime:2024-2-9..2024-12-1").1.mtime_min, Some(day(2024, 2, 9)));
    }

    #[test]
    fn bad_mtime_tokens_stay_in_text() {
        for q in [
            "mtime:2024-02-30",
            "mtime:2023-02-29",
            "mtime:2024-13-01",
            "mtime:24-01-01",
            "mtime:2024-01-01-01",
            "mtime:2024-01-001",
            "mtime:7x",
            "mtime:-7d",
            "mtime:2024-01-01",
            "mtime:",
            "mtime:..",
            "mtime:>",
            "mtime:>2024-01-01..2024-02-01",
        ] {
            let (rest, f) = p(q);
            assert!(f.is_empty(), "{q:?} should not be a filter");
            assert_eq!(rest, q);
        }
    }

    #[test]
    fn filters_mix_with_words_and_keep_unknown_keys() {
        let (rest, f) = p("walrus ext:pdf size:>1mb mtime:7d");
        assert_eq!(rest, "walrus");
        assert_eq!(f.exts, exts(&["pdf"]));
        assert_eq!(f.size_min, Some(1024 * 1024 + 1));
        assert_eq!(f.mtime_min, Some(NOW - 7.0 * DAY));

        let (rest, f) = p("foo ext:txt bar");
        assert_eq!(rest, "foo bar");
        assert_eq!(f.exts, exts(&["txt"]));

        let (rest, f) = p("ext:pdf size:bogus walrus");
        assert_eq!(rest, "size:bogus walrus");
        assert_eq!(f.exts, exts(&["pdf"]));
        assert_eq!(f.size_min, None);
    }

    #[test]
    fn matches_checks_every_part() {
        let f = p("ext:PDF,docx size:1mb..2mb mtime:>2024-01-01").1;
        assert!(f.matches("/d/rent.pdf", 1024 * 1024, day(2024, 3, 1)));
        assert!(f.matches("/d/a.DOCX", 2 * 1024 * 1024, day(2024, 1, 1)));
        assert!(!f.matches("/d/rent.txt", 1024 * 1024, day(2024, 3, 1)));
        assert!(!f.matches("/d/rent.pdf", 1024 * 1024 - 1, day(2024, 3, 1)));
        assert!(!f.matches("/d/rent.pdf", 2 * 1024 * 1024 + 1, day(2024, 3, 1)));
        assert!(!f.matches("/d/rent.pdf", 1024 * 1024, day(2023, 12, 31)));
        assert!(!f.matches("/d/pdf", 1024 * 1024, day(2024, 3, 1)));
        assert!(!f.matches("/d/.pdf", 1024 * 1024, day(2024, 3, 1)));
        assert!(!f.matches("/d.pdf/readme", 1024 * 1024, day(2024, 3, 1)));
        assert!(Filters::default().matches("/x/anything", -5, -1.0));
        assert!(Filters::default().is_empty());
        assert!(!f.is_empty());
    }

    #[test]
    fn describe_is_one_line() {
        let f = p("ext:pdf,docx size:1mb..5mb mtime:>2024-01-01").1;
        assert_eq!(f.describe(), "ext docx, pdf · size 1048576..5242880 B · mtime 2024-01-01..-");
    }

    fn want(exts: Option<&[&str]>, size: (Option<i64>, Option<i64>), mtime: (Option<f64>, Option<f64>)) -> Filters {
        Filters {
            exts: exts.map(|e| e.iter().map(|s| s.to_string()).collect()),
            size_min: size.0,
            size_max: size.1,
            mtime_min: mtime.0,
            mtime_max: mtime.1,
        }
    }

    const MB: i64 = 1024 * 1024;
    const NONE: (Option<i64>, Option<i64>) = (None, None);
    const NO_TIME: (Option<f64>, Option<f64>) = (None, None);

    #[test]
    fn parser_table() {
        // (query, text left over, expected filters)
        let cases: Vec<(&str, &str, Filters)> = vec![
            ("ext:pdf", "", want(Some(&["pdf"]), NONE, NO_TIME)),
            ("ext:PDF,Docx", "", want(Some(&["pdf", "docx"]), NONE, NO_TIME)),
            ("ext:.md", "", want(Some(&["md"]), NONE, NO_TIME)),
            ("EXT:txt", "", want(Some(&["txt"]), NONE, NO_TIME)),
            ("walrus ext:pdf ice", "walrus ice", want(Some(&["pdf"]), NONE, NO_TIME)),
            ("ext:tar.gz", "ext:tar.gz", Filters::default()),
            ("ext: walrus", "ext: walrus", Filters::default()),
            ("size:>10mb", "", want(None, (Some(10 * MB + 1), None), NO_TIME)),
            ("size:<1b", "", want(None, (None, Some(0)), NO_TIME)),
            ("size:>0b", "", want(None, (Some(1), None), NO_TIME)),
            ("size:1kb..2kb", "", want(None, (Some(1024), Some(2048)), NO_TIME)),
            ("size:2kb..", "", want(None, (Some(2048), None), NO_TIME)),
            ("size:..5mb", "", want(None, (None, Some(5 * MB)), NO_TIME)),
            ("size:1mb..5mb", "", want(None, (Some(MB), Some(5 * MB)), NO_TIME)),
            ("size:5mb..1mb", "", want(None, (Some(MB), Some(5 * MB)), NO_TIME)),
            ("size:>1kb size:<2kb", "", want(None, (Some(1025), Some(2047)), NO_TIME)),
            ("size:10", "size:10", Filters::default()),
            ("size:10tb", "size:10tb", Filters::default()),
            ("size:2gb..1mb..3gb", "size:2gb..1mb..3gb", Filters::default()),
            ("mtime:7d", "", want(None, NONE, (Some(NOW - 7.0 * DAY), None))),
            ("mtime:2w", "", want(None, NONE, (Some(NOW - 14.0 * DAY), None))),
            ("mtime:1m", "", want(None, NONE, (Some(NOW - 30.0 * DAY), None))),
            ("mtime:1y", "", want(None, NONE, (Some(NOW - 365.0 * DAY), None))),
            ("mtime:<7d", "", want(None, NONE, (None, Some(NOW - 7.0 * DAY)))),
            ("mtime:7d..1d", "", want(None, NONE, (Some(NOW - 7.0 * DAY), Some(NOW - DAY)))),
            ("mtime:>2024-01-01", "", want(None, NONE, (Some(day(2024, 1, 1)), None))),
            ("mtime:<2024-06-01", "", want(None, NONE, (None, Some(day(2024, 6, 1))))),
            ("mtime:2024-02-01..2024-01-01", "", want(None, NONE, (Some(day(2024, 1, 1)), Some(day(2024, 2, 1))))),
            ("mtime:2024-02-30", "mtime:2024-02-30", Filters::default()),
            ("mtime:2024-13-01", "mtime:2024-13-01", Filters::default()),
            ("mtime:24-01-01", "mtime:24-01-01", Filters::default()),
            ("ext:pdf size:>1mb mtime:7d walrus", "walrus", want(Some(&["pdf"]), (Some(MB + 1), None), (Some(NOW - 7.0 * DAY), None))),
            ("code: foo", "code: foo", Filters::default()),
            ("ext:p-df", "ext:p-df", Filters::default()),
        ];
        assert!(cases.len() >= 30, "{}", cases.len());
        for (q, rest, f) in cases {
            assert_eq!(p(q), (rest.to_string(), f), "{q:?}");
        }
    }
}
