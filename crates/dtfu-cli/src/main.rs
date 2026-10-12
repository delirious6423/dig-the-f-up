//! `dtfu`: command-line front end (port of the Swift CLI, `Sources/digup/main.swift`).
//! Commands: index, search, status, mcp, download, estimate, eval.

mod estimate;
mod eval;

use anyhow::{anyhow, bail, Result};
use dtfu_core::{query_prompt, FileKind};
use dtfu_embed::{Embedder, Options};
use dtfu_engine::{code_query_text, describe_location};
use dtfu_index::run::{check_model, finish_long_files, run, Event};
use dtfu_index::sync::sync;
use dtfu_index::{IndexOptions, SystemOcr};
use dtfu_search::filters;
use dtfu_search::{SearchOptions, Searcher};
use dtfu_store::index::{IndexStore, CODE_FILE, MAIN_FILE};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use estimate::tilde;

struct Args {
    flags: HashSet<String>,
    values: std::collections::HashMap<String, String>,
    positional: Vec<String>,
}

const VALUE_FLAGS: &[&str] = &["--index", "--limit", "-n", "--kinds", "--zgate", "--word-weight", "--settings"];

fn parse(raw: &[String]) -> Args {
    let mut a = Args { flags: HashSet::new(), values: Default::default(), positional: Vec::new() };
    let mut it = raw.iter();
    while let Some(x) = it.next() {
        if VALUE_FLAGS.contains(&x.as_str()) {
            if let Some(v) = it.next() {
                a.values.insert(x.clone(), v.clone());
            }
        } else if x.starts_with("--") {
            a.flags.insert(x.clone());
        } else {
            a.positional.push(x.clone());
        }
    }
    a
}

impl Args {
    fn index_dir(&self) -> PathBuf {
        match self.values.get("--index") {
            Some(d) => PathBuf::from(d),
            None => {
                // `~/.dtfu`; an index left in `~/.digup` by earlier builds is used while `~/.dtfu` does not exist.
                let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
                let (current, legacy) = (home.join(".dtfu"), home.join(".digup"));
                if !current.exists() && legacy.is_dir() { legacy } else { current }
            }
        }
    }
    fn file(&self) -> &'static str {
        if self.flags.contains("--code") {
            CODE_FILE
        } else {
            MAIN_FILE
        }
    }
}

fn load_embedder(text_only: bool) -> Result<Embedder> {
    let missing = || anyhow!("model not found in {} (run `dtfu download`, or set DTFU_MODELS)", dtfu_models::models_dir().display());
    let text = dtfu_models::locate(&dtfu_models::TEXT).ok_or_else(missing)?;
    let mmproj = if text_only { None } else { Some(dtfu_models::locate(&dtfu_models::MMPROJ).ok_or_else(missing)?) };
    Embedder::new(&text, mmproj.as_deref(), &Options::default())
}

/// `dtfu download`: fetches the model files that are missing (resumable): the two embedding models into the models
/// folder, the two LightOnOCR files into `dtfu_models::lighton_dir()` and, off macOS, the two `ocrs` OCR models into
/// `dtfu_models::ocrs_dir()`.
fn cmd_download() -> Result<()> {
    let dir = dtfu_models::models_dir();
    std::fs::create_dir_all(&dir)?;
    let mut wanted: Vec<(dtfu_models::ModelFile, std::path::PathBuf, Option<&str>)> =
        [dtfu_models::TEXT, dtfu_models::MMPROJ].into_iter().map(|f| (f, dir.clone(), None)).collect();
    if !cfg!(target_os = "macos") {
        let ocr = dtfu_models::ocrs_dir();
        std::fs::create_dir_all(&ocr)?;
        wanted.extend(dtfu_models::OCRS_FILES.into_iter().map(|f| (f, ocr.clone(), Some(dtfu_models::OCRS_BASE_URL))));
    }
    let lighton_url = dtfu_models::lighton_base_url();
    let lighton = dtfu_models::lighton_dir();
    std::fs::create_dir_all(&lighton)?;
    wanted.extend(dtfu_models::LIGHTON_FILES.into_iter().map(|f| (f, lighton.clone(), Some(lighton_url.as_str()))));
    for (file, dir, base_url) in wanted {
        let present = if dtfu_models::LIGHTON_FILES.contains(&file) {
            dtfu_models::locate_lighton(&file)
        } else if base_url.is_none() {
            dtfu_models::locate(&file)
        } else {
            Some(dir.join(file.name)).filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() == file.bytes))
        };
        if let Some(found) = present {
            // The lookup only compares sizes; a download command is the place to pay for the checksum.
            dtfu_models::verify(&found, &file)?;
            println!("{}: already there, checksum ok", file.name);
            continue;
        }
        let mut last = 0u64;
        let path = dtfu_models::download(&file, &dir, base_url, &mut |done, total| {
            if done - last >= 8 << 20 || done == total {
                last = done;
                eprint!("\r{}: {:.0} / {:.0} MB", file.name, done as f64 / 1e6, total as f64 / 1e6);
            }
        })?;
        eprintln!();
        println!("{}: saved to {}", file.name, path.display());
    }
    Ok(())
}

fn states_line(states: &std::collections::BTreeMap<String, usize>) -> String {
    states.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(" · ")
}

fn cmd_index(args: &Args) -> Result<()> {
    let code = args.flags.contains("--code");
    std::fs::create_dir_all(args.index_dir())?;
    let _indexer = match dtfu_store::lock::IndexerLock::try_acquire(&args.index_dir())? {
        Some(l) => l,
        None => bail!("another dtfu process (the app?) is indexing this index; try again when it is done"),
    };
    let mut store = IndexStore::open(&args.index_dir(), args.file())?;
    let opts = IndexOptions { include_code: code, ..IndexOptions::default() };
    let roots: Vec<PathBuf> = args.positional.iter().map(PathBuf::from).collect();
    if roots.is_empty() {
        bail!("index which folders?");
    }
    let started = Instant::now();
    let mut embedder = load_embedder(false)?;
    println!("model loaded in {:.2} s", started.elapsed().as_secs_f64());
    if args.flags.contains("--reindex") {
        store.reset_all()?;
    }
    check_model(&mut store, &embedder, &opts, code, false)?;
    println!("index: {}", tilde(&store.file().to_string_lossy()));
    let s = sync(&mut store, &roots, &opts, code)?;
    println!("sync: {} new · {} changed · {} moved · {} removed · {} unchanged", s.added, s.changed, s.moved, s.removed, s.unchanged);
    let limit = args.values.get("--limit").and_then(|v| v.parse().ok());
    let started = Instant::now();
    let mut print = |e: &Event| {
        let name = Path::new(&e.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let note = e.note.as_ref().map(|n| format!("  ({n})")).unwrap_or_default();
        println!("[{:>4}/{}] {:<10} {:<7} {:5.2}s  {}{}", e.done, e.total, e.kind.as_str(), e.state, e.seconds, name, note);
    };
    let states = run(&mut store, &mut embedder, &SystemOcr, &opts, limit, None, &|| false, &mut print)?;
    if states.is_empty() {
        println!("nothing pending");
    } else {
        println!("done in {:.1} s: {}", started.elapsed().as_secs_f64(), states_line(&states));
    }
    if limit.is_none() && store.unfinished_count()?.0 > 0 {
        let started = Instant::now();
        let finished = finish_long_files(&mut store, &mut embedder, &SystemOcr, &opts, &|| false, &mut print)?;
        println!(
            "the rest of long files in {:.1} s: {}",
            started.elapsed().as_secs_f64(),
            if finished.is_empty() { "none finished".to_string() } else { states_line(&finished) }
        );
    }
    Ok(())
}

fn cmd_search(args: &Args) -> Result<()> {
    let mut query = args.positional.join(" ");
    if query.is_empty() {
        bail!("search for what?");
    }
    let mut code = args.flags.contains("--code");
    if let Some(text) = code_query_text(&query) {
        query = text;
        code = true;
        if query.is_empty() {
            bail!("search the code for what?");
        }
    }
    // `ext:`, `size:` and `mtime:` terms restrict hits before the limit; the engine does the same in `search`.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    let (text, filters) = filters::parse(&query, now);
    query = text.trim().to_string();
    if query.is_empty() && filters.is_empty() {
        bail!("search for what?");
    }
    let z_gate = if query.is_empty() { f64::NEG_INFINITY } else { SearchOptions::default().z_gate };
    let store = IndexStore::open(&args.index_dir(), if code { CODE_FILE } else { MAIN_FILE })?;
    let searcher = Searcher::with_code(store.connection(), code)?;
    let started = Instant::now();
    let mut embedder = load_embedder(true)?;
    let vector = embedder.embed_texts(&[query_prompt(&query, code)])?.remove(0);
    let encoded = Instant::now();
    let kinds: Option<HashSet<FileKind>> = args
        .values
        .get("--kinds")
        .map(|k| k.split(',').filter_map(|s| FileKind::parse(s.trim())).collect());
    let opts = SearchOptions {
        kinds,
        limit: args.values.get("-n").and_then(|v| v.parse().ok()).unwrap_or(10),
        filters: filters.clone(),
        z_gate,
        ..SearchOptions::default()
    };
    let hits = searcher.search(&vector, &query, &opts)?;
    let searched = Instant::now();
    if args.flags.contains("--json") {
        let rows: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                let mut row = serde_json::Map::new();
                row.insert("path".into(), h.path.clone().into());
                row.insert("kind".into(), h.kind.as_str().into());
                row.insert("segment".into(), h.segment.as_str().into());
                row.insert("score".into(), h.score.into());
                if let Some(v) = h.z { row.insert("z".into(), v.into()); }
                if let Some(v) = h.cosine { row.insert("cosine".into(), v.into()); }
                if let Some(v) = h.keyword_rank { row.insert("keyword_rank".into(), v.into()); }
                if let Some(v) = h.loc { row.insert("loc".into(), v.into()); }
                if let Some(v) = h.loc_end { row.insert("loc_end".into(), v.into()); }
                if let Some(v) = h.focus { row.insert("focus".into(), v.into()); }
                if let Some(v) = &h.excerpt { row.insert("excerpt".into(), v.clone().into()); }
                row.into()
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if !filters.is_empty() {
        println!("filters: {}", filters.describe());
    }
    if hits.is_empty() {
        println!("no good matches");
    }
    for (rank, hit) in hits.iter().enumerate() {
        let why = [hit.z.map(|z| format!("z {z:.1}")), hit.keyword_rank.map(|r| format!("words #{r}"))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        let path = Path::new(&hit.path);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let place = Some(describe_location(hit)).filter(|p| !p.is_empty()).map(|p| format!("  [{p}]")).unwrap_or_default();
        println!("{:>2}. {:<10} {}{}   ({})", rank + 1, hit.kind.as_str(), name, place, why);
        println!("      {}", tilde(&path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()));
        if let Some(e) = hit.excerpt.as_deref().filter(|e| !e.is_empty()) {
            println!("      “{}”", e.chars().take(160).collect::<String>());
        }
    }
    println!(
        "({} vectors · query encoder {:.2} s incl. start-up · search {:.0} ms)",
        searcher.vector_count(),
        encoded.duration_since(started).as_secs_f64(),
        searched.duration_since(encoded).as_secs_f64() * 1000.0
    );
    Ok(())
}

fn cmd_status(args: &Args) -> Result<()> {
    let store = IndexStore::open(&args.index_dir(), args.file())?;
    let c = store.counts()?;
    println!("index: {}", tilde(&store.file().to_string_lossy()));
    println!("vectors: {} in {} segments", c.vectors, c.segments);
    for (kind, states) in &c.by_kind_and_state {
        let total: usize = states.values().sum();
        let line = states.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(" · ");
        println!("  {kind:<11} {total:>6}   {line}");
    }
    if c.unfinished > 0 {
        println!("long files still being read: {} ({} pages and chunks left)", c.unfinished, c.left);
    }
    if let Some(f) = store.meta("fingerprint")? {
        println!("built with: {f}");
    }
    println!("size: {:.1} MB", store.size_on_disk() as f64 / 1e6);
    Ok(())
}

/// Serves the index to AI agents over the Model Context Protocol on stdin/stdout (`dtfu mcp --index DIR`). Read-only:
/// the tools search the index and report its status; they never index, add folders or open files. `--settings FILE`
/// names the settings file (the indexed folders); without it the app's `settings.json` beside an `index/` folder is used. Add it to an agent
/// as a stdio server, e.g. `claude mcp add dtfu -- dtfu mcp --index ~/.dtfu`.
fn cmd_mcp(args: &Args) -> Result<()> {
    let dir = args.index_dir();
    // The desktop app keeps `settings.json` (the indexed folders) next to its `index/` folder; use it when the index
    // is the app's, so the `folders` tool and the removed-folder filter match what the app shows.
    let app_settings = dir.parent().map(|p| p.join("settings.json")).filter(|p| dir.file_name().is_some_and(|n| n == "index") && p.exists());
    let settings_file = args.values.get("--settings").map(PathBuf::from).or(app_settings).unwrap_or_else(|| dir.join("mcp-settings.json"));
    let cfg = dtfu_engine::EngineConfig { index_dir: dir, settings_file };
    let engine = std::sync::Arc::new(dtfu_engine::Engine::open(cfg)?);
    dtfu_mcp::serve_stdio(engine)
}

fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = raw.first() else {
        bail!("usage: dtfu <index|search|status|mcp|eval|estimate|download> [--index DIR] [--code] ...");
    };
    let args = parse(&raw[1..]);
    match cmd.as_str() {
        "index" => cmd_index(&args),
        "search" => cmd_search(&args),
        "status" => cmd_status(&args),
        "mcp" => cmd_mcp(&args),
        "download" => cmd_download(),
        "estimate" => {
            if args.positional.is_empty() {
                bail!("which folders? e.g. dtfu estimate ~/Documents");
            }
            let options = IndexOptions { include_code: args.flags.contains("--code"), ..IndexOptions::default() };
            estimate::estimate(&estimate::EstimateArgs {
                folders: args.positional.iter().map(PathBuf::from).collect(),
                options,
            })
        }
        "eval" => {
            let Some(file) = args.positional.first() else { bail!("eval which file?") };
            let a = eval::EvalArgs {
                index_dir: args.index_dir(),
                eval_file: PathBuf::from(file),
                depth: args.values.get("-n").and_then(|v| v.parse().ok()).unwrap_or(10),
                z_gate: args.values.get("--zgate").and_then(|v| v.parse().ok()).unwrap_or(2.0),
                word_weight: args.values.get("--word-weight").and_then(|v| v.parse().ok()),
            };
            let mut embedder = load_embedder(true)?;
            eval::evaluate(&a, &mut embedder)
        }
        other => bail!("unknown command {other} (commands: index, search, status, mcp, eval, estimate, download)"),
    }
}
