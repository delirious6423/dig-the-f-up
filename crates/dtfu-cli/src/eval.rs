//! `dtfu eval`: score a JSON file of queries against the indexes (port of `evaluate` in the Swift CLI).

use anyhow::{Context, Result};
use dtfu_core::{query_prompt, FileKind, SegmentKind};
use dtfu_embed::Embedder;
use dtfu_engine::describe_location;
use dtfu_search::{strong_count, SearchOptions, Searcher};
use dtfu_store::index::{IndexStore, CODE_FILE, MAIN_FILE};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub struct EvalArgs {
    pub index_dir: PathBuf,
    pub eval_file: PathBuf,
    pub depth: usize,
    pub z_gate: f64,
    pub word_weight: Option<f64>,
}

#[derive(Deserialize)]
struct EvalQuery {
    q: String,
    expect: Vec<String>,
    tag: Option<String>,
    kind: Option<String>,
}

#[derive(Deserialize)]
struct EvalFile {
    base: String,
    queries: Vec<EvalQuery>,
}

fn expand_tilde(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if p == "~" {
        home
    } else if let Some(rest) = p.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        p.to_string()
    }
}

/// An expected answer: a full path, optionally with a closed range of places.
#[derive(Debug, PartialEq)]
struct Target {
    path: String,
    range: Option<(f64, f64)>,
}

fn parse_target(base: &str, entry: &str) -> Target {
    let mut parts = entry.splitn(2, '@');
    let path = format!("{base}/{}", parts.next().unwrap_or(""));
    let range = parts.next().and_then(|r| {
        let nums: Vec<f64> = r.split('-').filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        (nums.len() == 2).then(|| (nums[0], nums[1]))
    });
    Target { path, range }
}

/// A page or moment starts in the range; a stretch of code or cells overlaps it.
fn at(segment: SegmentKind, loc: Option<f64>, loc_end: Option<f64>, (lo, hi): (f64, f64)) -> bool {
    let Some(loc) = loc else { return false };
    if matches!(segment, SegmentKind::Lines | SegmentKind::Cells) {
        return loc <= hi && loc_end.unwrap_or(loc) >= lo;
    }
    loc >= lo && loc <= hi
}

fn matches_target(path: &str, segment: SegmentKind, loc: Option<f64>, loc_end: Option<f64>, t: &Target) -> bool {
    path == t.path && t.range.is_none_or(|r| at(segment, loc, loc_end, r))
}

/// (R@1, R@5, R@10, MRR) over the ranks of the first matching hit.
fn metrics(ranks: &[Option<usize>]) -> (f64, f64, f64, f64) {
    let n = ranks.len().max(1) as f64;
    let within = |k: usize| ranks.iter().filter(|r| r.is_some_and(|r| r <= k)).count() as f64 / n;
    let mrr = ranks.iter().flatten().map(|&r| 1.0 / r as f64).sum::<f64>() / n;
    (within(1), within(5), within(10), mrr)
}

pub fn evaluate(args: &EvalArgs, embedder: &mut Embedder) -> Result<()> {
    let raw = std::fs::read_to_string(expand_tilde(&args.eval_file.to_string_lossy()))
        .with_context(|| format!("reading {}", args.eval_file.display()))?;
    let eval: EvalFile = serde_json::from_str(&raw).context("parsing the eval file")?;
    let base = expand_tilde(&eval.base);
    let texts: Vec<(bool, String)> = eval
        .queries
        .iter()
        .map(|q| match q.q.strip_prefix("code:") {
            Some(rest) => (true, rest.trim().to_string()),
            None => (false, q.q.clone()),
        })
        .collect();

    // Embed all queries of one index in one batch.
    let mut vectors: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
    for code in [false, true] {
        let idx: Vec<usize> = (0..texts.len()).filter(|&i| texts[i].0 == code).collect();
        if idx.is_empty() {
            continue;
        }
        let prompts: Vec<String> = idx.iter().map(|&i| query_prompt(&texts[i].1, code)).collect();
        for (i, v) in idx.into_iter().zip(embedder.embed_texts(&prompts)?) {
            vectors[i] = v;
        }
    }

    let need = |code: bool| texts.iter().any(|t| t.0 == code);
    let main_store = if need(false) { Some(IndexStore::open(&args.index_dir, MAIN_FILE)?) } else { None };
    let code_store = if need(true) { Some(IndexStore::open(&args.index_dir, CODE_FILE)?) } else { None };
    let main_searcher = main_store.as_ref().map(|s| Searcher::with_code(s.connection(), false)).transpose()?;
    let code_searcher = code_store.as_ref().map(|s| Searcher::new_code(s.connection())).transpose()?;

    let (mut shown, mut listed, mut folded) = (0usize, 0usize, 0usize);
    let mut by_tag: BTreeMap<String, Vec<Option<usize>>> = BTreeMap::new();
    for (i, query) in eval.queries.iter().enumerate() {
        let (code, text) = &texts[i];
        let searcher = if *code { code_searcher.as_ref() } else { main_searcher.as_ref() }.expect("searcher opened");
        let kinds: Option<HashSet<FileKind>> =
            query.kind.as_deref().and_then(FileKind::parse).map(|k| HashSet::from([k]));
        let opts = SearchOptions {
            kinds,
            limit: args.depth,
            z_gate: args.z_gate,
            word_weight: args.word_weight,
            ..SearchOptions::default()
        };
        let hits = searcher.search(&vectors[i], text, &opts)?;
        let strong = strong_count(&hits, 0.5);
        shown += strong;
        listed += hits.len();
        let targets: Vec<Target> = query.expect.iter().map(|e| parse_target(&base, e)).collect();
        let rank = hits
            .iter()
            .position(|h| targets.iter().any(|t| matches_target(&h.path, h.segment, h.loc, h.loc_end, t)))
            .map(|p| p + 1);
        if rank.is_some_and(|r| r > strong) {
            folded += 1;
        }
        let tag = query.tag.clone().unwrap_or_else(|| "all".into());
        by_tag.entry(tag.clone()).or_default().push(rank);
        if tag != "ALL" {
            by_tag.entry("ALL".into()).or_default().push(rank);
        }
        let top = hits
            .first()
            .map(|h| {
                let name = Path::new(&h.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let loc = describe_location(h);
                if loc.is_empty() { name } else { format!("{name} [{loc}]") }
            })
            .unwrap_or_else(|| "-".into());
        let rank_s = rank.map(|r| format!("{:>4}", format!("#{r}"))).unwrap_or_else(|| "miss".into());
        println!("{rank_s}  {:<11} {:<42} top: {top}", tag, format!("“{}”", query.q));
    }

    println!();
    println!("{:<12} {:>3}  {:>5}  {:>5}  {:>5}  {:>5}", "tag", "n", "R@1", "R@5", "R@10", "MRR");
    let mut rows: Vec<(&String, &Vec<Option<usize>>)> = by_tag.iter().collect();
    rows.sort_by_key(|(k, _)| if k.as_str() == "ALL" { "~".to_string() } else { (*k).clone() });
    for (tag, ranks) in rows {
        let (r1, r5, r10, mrr) = metrics(ranks);
        println!("{:<12} {:>3}  {r1:5.2}  {r5:5.2}  {r10:5.2}  {mrr:5.2}", tag, ranks.len());
    }
    let count = eval.queries.len().max(1) as f64;
    println!(
        "weaker-matches fold (below 0.5× the top score): {:.1} of {:.1} results shown per query; {folded} expected {} folded",
        shown as f64 / count,
        listed as f64 / count,
        if folded == 1 { "answer" } else { "answers" }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parsing() {
        let t = parse_target("/b", "a/x.py@8-29");
        assert_eq!(t, Target { path: "/b/a/x.py".into(), range: Some((8.0, 29.0)) });
        assert_eq!(parse_target("/b", "x.pdf").range, None);
        assert_eq!(parse_target("/b", "x.pdf@3").range, None);
    }

    #[test]
    fn range_rules() {
        let t = parse_target("/b", "f.py@10-20");
        // code: overlap
        assert!(matches_target("/b/f.py", SegmentKind::Lines, Some(5.0), Some(10.0), &t));
        assert!(matches_target("/b/f.py", SegmentKind::Lines, Some(20.0), Some(30.0), &t));
        assert!(!matches_target("/b/f.py", SegmentKind::Lines, Some(21.0), Some(30.0), &t));
        assert!(!matches_target("/b/f.py", SegmentKind::Lines, Some(1.0), Some(9.0), &t));
        // page: starts inside
        assert!(matches_target("/b/f.py", SegmentKind::Page, Some(10.0), None, &t));
        assert!(!matches_target("/b/f.py", SegmentKind::Page, Some(9.0), Some(15.0), &t));
        assert!(!matches_target("/b/f.py", SegmentKind::Page, None, None, &t));
        // wrong path; no range accepts any place
        assert!(!matches_target("/b/g.py", SegmentKind::Lines, Some(10.0), None, &t));
        assert!(matches_target("/b/f.py", SegmentKind::Name, None, None, &parse_target("/b", "f.py")));
    }

    #[test]
    fn metrics_aggregate() {
        let (r1, r5, r10, mrr) = metrics(&[Some(1), Some(3), Some(7), None]);
        assert_eq!((r1, r5, r10), (0.25, 0.5, 0.75));
        assert!((mrr - (1.0 + 1.0 / 3.0 + 1.0 / 7.0) / 4.0).abs() < 1e-12);
        assert_eq!(metrics(&[]), (0.0, 0.0, 0.0, 0.0));
    }
}
