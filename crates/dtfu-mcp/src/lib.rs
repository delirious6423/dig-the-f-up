//! A minimal Model Context Protocol (MCP) server over stdio, so that agents (Claude Code, Codex, Cursor, ...) can
//! query the user's semantic index.
//!
//! # Privacy
//!
//! Nothing in this crate touches the network. The server only reads the local index through [`dtfu_engine::Engine`].
//! BUT: search results contain file paths, page or time locations and short text excerpts (at most 300 characters
//! each) taken from the user's own indexed files, and these are returned to whichever agent launched the server.
//! Starting this server therefore means the calling agent (and the model provider behind it) can read those excerpts.
//! Only run it for agents the user trusts with their files.
//!
//! # Safety properties
//!
//! - Read-only: no tool starts indexing, adds or removes folders, or opens or modifies files.
//! - Engine errors (for example a search model that is not downloaded) come back as tool results with `isError: true`,
//!   never as JSON-RPC errors.
//! - Stdout carries only protocol messages, one JSON-RPC message per line. Diagnostics go to stderr.
//! - Tool text is capped at about 20,000 characters per response.
//!
//! # Protocol
//!
//! JSON-RPC 2.0 over newline-delimited JSON (the MCP stdio transport). Handled methods: `initialize`,
//! `notifications/*` (ignored, never answered), `ping`, `tools/list`, `tools/call`. Any other request gets `-32601`.
//! Malformed JSON gets `-32700` with id `null`. Batch arrays are rejected with `-32600`. Protocol versions
//! `2024-11-05`, `2025-03-26` and `2025-06-18` are echoed back; any other requested version is answered with
//! `2025-06-18`.
//!
//! # Tools
//!
//! - `search {query, limit?, kinds?}`: semantic search (the `code:` prefix searches code; the engine's filter syntax
//!   is passed through unchanged).
//! - `status {}`: engine state, index counts and indexed folders.
//! - `folders {}`: indexed folders only.
//!
//! # Wiring
//!
//! The CLI calls [`serve_stdio`] with the shared engine. Tests use [`serve`] with in-memory buffers.

use anyhow::Result;
use dtfu_engine::{Engine, Hit, Status};
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

const SUPPORTED_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_VERSION: &str = "2025-06-18";
/// Maximum characters of text in one tool response.
const MAX_TEXT: usize = 20_000;
/// Maximum characters of one search excerpt.
const EXCERPT_MAX: usize = 300;
const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 50;
/// File kinds accepted by the `kinds` argument (`dtfu_core::FileKind`).
const KINDS: [&str; 7] = ["screenshot", "image", "pdf", "doc", "audio", "video", "code"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "Read-only search over the user's own indexed files. Results include file paths and short \
excerpts that are returned to you. These tools never index, add folders or open files.";

/// Serves MCP on the process's stdin and stdout until stdin closes.
pub fn serve_stdio(engine: Arc<Engine>) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(engine, stdin.lock(), stdout.lock())
}

/// Serves MCP on `input` and `output` (one JSON message per line) until `input` ends.
///
/// Returns `Err` only for I/O failures on `input` or `output`; protocol and tool errors are sent to the client.
pub fn serve<R: BufRead, W: Write>(engine: Arc<Engine>, mut input: R, mut output: W) -> Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let bytes = line.trim_ascii();
        if bytes.is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&engine, bytes) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
}

/// An error that becomes a JSON-RPC error object.
struct RpcError {
    code: i64,
    message: String,
}

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError { code, message: message.into() }
}

fn ok_reply(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_reply(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// Returns the reply for one input line, or `None` when nothing must be sent.
fn handle_line(engine: &Arc<Engine>, bytes: &[u8]) -> Option<Value> {
    let msg: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("dtfu-mcp: malformed JSON: {e}");
            return Some(error_reply(Value::Null, PARSE_ERROR, format!("Parse error: {e}")));
        }
    };
    let Value::Object(obj) = msg else {
        return Some(error_reply(
            Value::Null,
            INVALID_REQUEST,
            "Invalid Request: send one JSON-RPC object per line; batch arrays are not supported",
        ));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A response to a request we never sent, ignored. Anything else is an invalid request.
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(error_reply(id.unwrap_or(Value::Null), INVALID_REQUEST, "Invalid Request: missing string \"method\""));
    };
    // Notifications (no id) are never answered, whatever the method.
    let id = id?;
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    Some(match dispatch(engine, method, &params) {
        Ok(result) => ok_reply(id, result),
        Err(e) => error_reply(id, e.code, e.message),
    })
}

fn dispatch(engine: &Arc<Engine>, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Err(rpc_err(INVALID_PARAMS, "tools/call requires a string \"name\""));
            };
            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(m)) => m.clone(),
                Some(_) => return Ok(tool_error("Invalid arguments: expected a JSON object.")),
            };
            if !matches!(name, "search" | "status" | "folders") {
                return Err(rpc_err(INVALID_PARAMS, format!("unknown tool: {name}")));
            }
            // A panic in a tool must not kill the server: report it as a tool error.
            let result = catch_unwind(AssertUnwindSafe(|| match name {
                "search" => search_tool(engine, &arguments),
                "status" => status_tool(engine),
                _ => folders_tool(engine),
            }));
            Ok(result.unwrap_or_else(|_| tool_error("Internal error while running the tool.")))
        }
        _ => Err(rpc_err(METHOD_NOT_FOUND, format!("method not found: {method}"))),
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = match requested {
        Some(v) if SUPPORTED_VERSIONS.contains(&v) => v,
        _ => LATEST_VERSION,
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "dtfu", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "search",
            "description": "Semantic search over the user's indexed files (documents, PDFs, images with OCR and vision, \
audio, video, code). Returns, best first, the file path, kind, location in the file (for example \"p. 3\"), score and \
a short excerpt (at most 300 characters) of each match. PRIVACY: these paths and excerpts come from the user's own \
files and are returned to you; do not repeat them beyond what the task needs. Read-only. The query text is passed to \
the engine unchanged, so a \"code:\" prefix searches code and the engine's filter syntax (for example ext:, size:, \
mtime:) applies where supported. Errors, such as a missing search model, are reported with isError set.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to search for, in natural language, or code: followed by a code query."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT, "description": "Maximum number of results, 1 to 50."},
                    "kinds": {"type": "array", "items": {"type": "string", "enum": KINDS}, "description": "Only return these file kinds. Omit for all kinds."}
                },
                "required": ["query"]
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "status",
            "description": "Report the search engine state (no model, ready, indexing with progress, idle, paused), index \
counts (vectors, pending and unfinished files, files by kind and state) and the indexed folder paths. PRIVACY: folder \
paths are local paths of the user's files. Read-only; does not start indexing.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "folders",
            "description": "List the folders the user has added for indexing. PRIVACY: these are local paths of the \
user's files. Read-only; does not add, remove or index folders.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true}
        }
    ])
}

/// A successful tool result. `structured` must be a JSON object.
fn tool_ok(text: String, structured: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": cap_text(text, MAX_TEXT)}],
        "structuredContent": structured,
        "isError": false,
    })
}

fn tool_error(text: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": cap_text(text.into(), MAX_TEXT)}],
        "isError": true,
    })
}

fn search_tool(engine: &Engine, args: &Map<String, Value>) -> Value {
    let query = match args.get("query") {
        None | Some(Value::Null) => return tool_error("Missing required argument \"query\" (a string)."),
        Some(Value::String(q)) => q,
        Some(_) => return tool_error("Invalid argument \"query\": expected a string."),
    };
    if query.trim().is_empty() {
        return tool_error("Invalid argument \"query\": must not be empty.");
    }
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(v) => match v.as_i64() {
            Some(n) if (1..=MAX_LIMIT).contains(&n) => n,
            Some(_) => return tool_error("Invalid argument \"limit\": must be an integer from 1 to 50."),
            None => return tool_error("Invalid argument \"limit\": expected an integer from 1 to 50."),
        },
    };
    let kinds = match args.get("kinds") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(k) if KINDS.contains(&k) => out.push(k.to_string()),
                    Some(k) => {
                        return tool_error(format!(
                            "Invalid argument \"kinds\": unknown kind \"{}\". Use any of: {}.",
                            truncate_chars(k, 40),
                            KINDS.join(", ")
                        ))
                    }
                    None => return tool_error("Invalid argument \"kinds\": expected an array of strings."),
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        Some(_) => return tool_error("Invalid argument \"kinds\": expected an array of strings."),
    };

    let hits = match engine.search(query, limit as usize, kinds) {
        Ok(h) => h,
        Err(e) => {
            return tool_error(format!(
                "Search failed: {e:#}. If the search model is not downloaded, install it from the Dig-the-f-up app and retry. \
Call the status tool to see the engine state."
            ))
        }
    };
    format_search(&hits)
}

/// Builds the search result, keeping whole results until the text budget is spent.
fn format_search(hits: &[Hit]) -> Value {
    if hits.is_empty() {
        return tool_ok("No matches in the indexed files.".into(), json!({"results": []}));
    }
    // Reserve room for the footer, which is added when results are dropped.
    let budget = MAX_TEXT - 200;
    let mut text = String::new();
    let mut results = Vec::new();
    for hit in hits {
        let n = results.len() + 1;
        let excerpt = hit.excerpt.as_deref().map(|e| truncate_chars(&collapse_whitespace(e), EXCERPT_MAX));
        let score = (hit.score * 1000.0).round() / 1000.0;
        let mut block = format!("{n}. {}\n   {}", hit.path, hit.kind);
        if !hit.place.is_empty() {
            block.push_str(&format!(" · {}", hit.place));
        }
        block.push_str(&format!(" · score {score:.3}\n"));
        if let Some(e) = &excerpt {
            block.push_str(&format!("   {e}\n"));
        }
        if text.chars().count() + block.chars().count() > budget {
            break;
        }
        text.push_str(&block);
        results.push(json!({
            "path": hit.path,
            "kind": hit.kind,
            "segment": hit.segment,
            "place": hit.place,
            "score": score,
            "excerpt": excerpt,
        }));
    }
    let shown = results.len();
    let mut header = format!("{} of {} result(s), best first:\n", shown, hits.len());
    if shown < hits.len() {
        text.push_str(&format!(
            "(Truncated: showing {shown} of {} results to stay within the size limit. Use a smaller limit.)\n",
            hits.len()
        ));
    }
    header.push_str(&text);
    tool_ok(header, json!({"results": results}))
}

fn status_tool(engine: &Engine) -> Value {
    let status = engine.status();
    let counts = match engine.counts() {
        Ok(c) => c,
        Err(e) => return tool_error(format!("Could not read the index: {e:#}")),
    };
    let folders: Vec<String> = engine.folders().iter().map(|p| p.to_string_lossy().into_owned()).collect();

    let (state, label, progress) = match &status {
        Status::NoModel => ("no_model", "no search model downloaded".to_string(), None),
        Status::Ready => ("ready", "ready".to_string(), None),
        Status::Indexing { done, total, current } => (
            "indexing",
            format!("indexing {done} of {total}: {current}"),
            Some(json!({"done": done, "total": total, "current": current})),
        ),
        Status::Idle => ("idle", "idle".to_string(), None),
        Status::Paused => ("paused", "paused".to_string(), None),
    };

    let mut text = format!("Engine: {label}\n");
    text.push_str(&format!(
        "Index: {} vectors, {} pending, {} unfinished.\n",
        counts.vectors, counts.pending, counts.unfinished
    ));
    text.push_str("Files by kind:\n");
    if counts.files_by_kind_state.is_empty() {
        text.push_str("  (none)\n");
    }
    let mut files = Map::new();
    for (kind, states) in &counts.files_by_kind_state {
        let parts: Vec<String> = states.iter().map(|(s, n)| format!("{s} {n}")).collect();
        text.push_str(&format!("  {kind}: {}\n", parts.join(", ")));
        let by_state: Map<String, Value> = states.iter().map(|(s, n)| (s.clone(), json!(n))).collect();
        files.insert(kind.clone(), Value::Object(by_state));
    }
    text.push_str(&format!("Indexed folders ({}):\n", folders.len()));
    if folders.is_empty() {
        text.push_str("  (none)\n");
    }
    for f in &folders {
        text.push_str(&format!("  {f}\n"));
    }

    tool_ok(
        text,
        json!({
            "status": state,
            "progress": progress,
            "counts": {
                "vectors": counts.vectors,
                "pending": counts.pending,
                "unfinished": counts.unfinished,
                "files": Value::Object(files),
            },
            "folders": folders,
        }),
    )
}

fn folders_tool(engine: &Engine) -> Value {
    let folders: Vec<String> = engine.folders().iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let text = if folders.is_empty() {
        "No folders are indexed. The user adds folders in the Dig-the-f-up app.".to_string()
    } else {
        let mut t = format!("{} indexed folder(s):\n", folders.len());
        for f in &folders {
            t.push_str(&format!("- {f}\n"));
        }
        t
    };
    tool_ok(text, json!({"folders": folders}))
}

/// Collapses runs of whitespace (including newlines) to single spaces, so an excerpt fits on one line.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max` characters; a longer string is cut and ends with an ellipsis (counted in `max`).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// At most `max` characters, with a note when cut.
fn cap_text(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let note = "\n[output truncated]";
    let keep = max.saturating_sub(note.chars().count());
    let mut out: String = s.chars().take(keep).collect();
    out.push_str(note);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use dtfu_engine::EngineConfig;
    use dtfu_index::Embed;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    // The same fake embedder as dtfu-engine's tests (those items are private to that crate).
    fn unit(seed: &[u8]) -> Vec<f32> {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in seed {
            h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        let mut v = Vec::with_capacity(768);
        for _ in 0..768 {
            h ^= h << 13;
            h ^= h >> 7;
            h ^= h << 17;
            v.push(((h >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32);
        }
        let n = v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt() as f32;
        v.iter().map(|x| x / n).collect()
    }

    struct FakeEmbed;
    impl Embed for FakeEmbed {
        fn vector_source(&self) -> String {
            "fake".into()
        }
        fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| unit(t.as_bytes())).collect())
        }
        fn embed_image_rgb(&mut self, _: usize, _: usize, rgb: &[u8], _: usize) -> Result<Vec<f32>> {
            Ok(unit(&rgb[..rgb.len().min(64)]))
        }
        fn embed_audio_samples(&mut self, s: &[f32]) -> Result<Vec<f32>> {
            Ok(unit(&s.len().to_le_bytes()))
        }
    }

    fn engine_at(tmp: &Path, with_model: bool) -> Arc<Engine> {
        let cfg = EngineConfig { index_dir: tmp.join("index"), settings_file: tmp.join("settings.json") };
        let query: dtfu_engine::QueryEncoder = if with_model {
            Box::new(|q| Ok(unit(q.as_bytes())))
        } else {
            Box::new(|_| Err(anyhow!("the search model is not downloaded yet")))
        };
        Arc::new(Engine::open_with(cfg, Box::new(|| Ok(Box::new(FakeEmbed))), query).unwrap())
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    fn notes(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("a.txt"), "The walrus basked on the ice floe all afternoon.").unwrap();
        std::fs::write(dir.join("b.txt"), "An ostrich buried its head in the sand yesterday.").unwrap();
        std::fs::write(dir.join("c.txt"), "A zebra grazed quietly near the watering hole.").unwrap();
    }

    fn indexed(e: &Engine) -> bool {
        e.counts().map(|c| c.vectors >= 3 && c.pending == 0).unwrap_or(false)
    }

    /// Runs the server over `input` and returns every reply line, parsed.
    fn run(engine: &Arc<Engine>, input: &str) -> Vec<Value> {
        let mut out = Vec::new();
        serve(engine.clone(), Cursor::new(input.as_bytes().to_vec()), &mut out).unwrap();
        String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).expect("stdout line is JSON")).collect()
    }

    /// One `tools/call` round trip; returns the tool result object.
    fn call(engine: &Arc<Engine>, name: &str, args: Value) -> Value {
        let req = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": args}});
        let replies = run(engine, &format!("{req}\n"));
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert!(replies[0].get("error").is_none(), "{replies:?}");
        replies[0]["result"].clone()
    }

    fn text_of(result: &Value) -> String {
        result["content"][0]["text"].as_str().unwrap_or_default().to_string()
    }

    fn indexed_engine(tmp: &Path) -> (Arc<Engine>, PathBuf) {
        let docs = tmp.join("docs");
        notes(&docs);
        let e = engine_at(tmp, true);
        e.add_folder(docs.clone()).unwrap();
        e.start_indexing().unwrap();
        wait_for("indexing", || indexed(&e));
        wait_for("idle", || e.status() == Status::Idle);
        (e, docs)
    }

    #[test]
    fn initialize_echoes_supported_versions_and_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), true);
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"initialize"}"#,
            "\n",
        );
        let replies = run(&e, input);
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[0]["id"], 1);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(replies[0]["result"]["capabilities"], json!({"tools": {}}));
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "dtfu");
        assert_eq!(replies[1]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[2]["result"]["protocolVersion"], "2025-06-18");
        for v in ["2024-11-05", "2025-06-18"] {
            let r = json!({"jsonrpc":"2.0","id":9,"method":"initialize","params":{"protocolVersion":v}});
            assert_eq!(run(&e, &format!("{r}\n"))[0]["result"]["protocolVersion"], v);
        }
    }

    #[test]
    fn notifications_get_no_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), true);
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"folders"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"something/unknown"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#,
            "\n",
        );
        let replies = run(&e, input);
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0]["id"], 4);
        assert_eq!(replies[0]["result"], json!({}));
    }

    #[test]
    fn tools_list_has_the_three_read_only_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), true);
        let replies = run(&e, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n");
        let tools = replies[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["search", "status", "folders"]);
        for t in tools {
            assert!(!t["description"].as_str().unwrap_or_default().is_empty());
            assert_eq!(t["inputSchema"]["type"], "object");
            assert_eq!(t["annotations"]["readOnlyHint"], true);
        }
        let search = &tools[0]["inputSchema"];
        assert_eq!(search["required"], json!(["query"]));
        assert_eq!(search["properties"]["limit"]["minimum"], 1);
        assert_eq!(search["properties"]["limit"]["maximum"], 50);
        assert_eq!(search["properties"]["kinds"]["type"], "array");
    }

    #[test]
    fn search_finds_the_walrus_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _) = indexed_engine(tmp.path());
        let result = call(&e, "search", json!({"query": "walrus", "limit": 3}));
        assert_eq!(result["isError"], false, "{result}");
        let results = result["structuredContent"]["results"].as_array().unwrap();
        assert!(results[0]["path"].as_str().unwrap().ends_with("a.txt"), "{results:?}");
        for key in ["path", "kind", "segment", "place", "score", "excerpt"] {
            assert!(results[0].get(key).is_some(), "missing {key}");
        }
        assert!(results[0]["excerpt"].as_str().unwrap().chars().count() <= EXCERPT_MAX);
        let text = text_of(&result);
        assert!(text.starts_with("1 of 1 result(s)"), "{text}");
        assert!(text.contains("a.txt") && text.contains("score "), "{text}");
    }

    #[test]
    fn search_kinds_filter_and_default_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _) = indexed_engine(tmp.path());
        let result = call(&e, "search", json!({"query": "zebra", "kinds": ["doc"]}));
        assert_eq!(result["isError"], false, "{result}");
        let results = result["structuredContent"]["results"].as_array().unwrap();
        assert!(results[0]["path"].as_str().unwrap().ends_with("c.txt"));
        assert!(results.len() <= 10);
        let none = call(&e, "search", json!({"query": "zebra", "kinds": ["pdf"]}));
        assert_eq!(none["isError"], false);
    }

    #[test]
    fn status_and_folders_report_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, docs) = indexed_engine(tmp.path());
        let folders = call(&e, "folders", json!({}));
        assert_eq!(folders["isError"], false);
        assert_eq!(folders["structuredContent"]["folders"], json!([docs.to_string_lossy()]));
        assert!(text_of(&folders).contains("docs"));

        let status = call(&e, "status", json!({}));
        assert_eq!(status["isError"], false, "{status}");
        assert_eq!(status["structuredContent"]["status"], "idle");
        assert!(status["structuredContent"]["counts"]["vectors"].as_u64().unwrap() >= 3);
        assert!(text_of(&status).contains("Engine: idle"));
    }

    #[test]
    fn empty_index_reports_no_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), true);
        let folders = call(&e, "folders", json!({}));
        assert_eq!(folders["structuredContent"]["folders"], json!([]));
        let search = call(&e, "search", json!({"query": "anything"}));
        assert_eq!(search["isError"], false);
        assert_eq!(search["structuredContent"]["results"], json!([]));
    }

    #[test]
    fn bad_arguments_are_tool_errors_naming_the_field() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), true);
        let cases = [
            (json!({}), "query"),
            (json!({"query": 5}), "query"),
            (json!({"query": "   "}), "query"),
            (json!({"query": "x", "limit": "ten"}), "limit"),
            (json!({"query": "x", "limit": 0}), "limit"),
            (json!({"query": "x", "limit": 51}), "limit"),
            (json!({"query": "x", "limit": 2.5}), "limit"),
            (json!({"query": "x", "kinds": "pdf"}), "kinds"),
            (json!({"query": "x", "kinds": [3]}), "kinds"),
            (json!({"query": "x", "kinds": ["bogus"]}), "kinds"),
        ];
        for (args, field) in cases {
            let result = call(&e, "search", args.clone());
            assert_eq!(result["isError"], true, "{args}");
            assert!(text_of(&result).contains(field), "{args}: {}", text_of(&result));
        }
        let bad_args = run(
            &e,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"search\",\"arguments\":[1]}}\n",
        );
        assert_eq!(bad_args[0]["result"]["isError"], true);
    }

    #[test]
    fn search_without_a_model_is_a_tool_error() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), false);
        let result = call(&e, "search", json!({"query": "walrus"}));
        assert_eq!(result["isError"], true, "{result}");
        let text = text_of(&result);
        assert!(text.contains("model") && text.contains("status"), "{text}");
        assert!(result.get("structuredContent").is_none());
    }

    #[test]
    fn unknown_method_and_unknown_tool_are_json_rpc_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), true);
        let replies = run(
            &e,
            concat!(
                r#"{"jsonrpc":"2.0","id":"abc","method":"resources/list"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"index_now"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{}}"#,
                "\n",
            ),
        );
        assert_eq!(replies[0]["id"], "abc");
        assert_eq!(replies[0]["error"]["code"], -32601);
        assert_eq!(replies[1]["error"]["code"], -32602);
        assert_eq!(replies[2]["error"]["code"], -32602);
    }

    #[test]
    fn malformed_json_and_batches_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), true);
        let replies = run(
            &e,
            concat!(
                "{\"jsonrpc\": \"2.0\", \"id\": 1,\n",
                "not json at all\n",
                "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]\n",
                "{\"jsonrpc\":\"2.0\",\"id\":2}\n",
            ),
        );
        assert_eq!(replies.len(), 4, "{replies:?}");
        assert_eq!(replies[0]["error"]["code"], -32700);
        assert_eq!(replies[0]["id"], Value::Null);
        assert_eq!(replies[1]["error"]["code"], -32700);
        assert_eq!(replies[1]["id"], Value::Null);
        assert_eq!(replies[2]["error"]["code"], -32600);
        assert_eq!(replies[3]["error"]["code"], -32600);
        assert_eq!(replies[3]["id"], 2);
    }

    #[test]
    fn invalid_utf8_is_a_parse_error_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine_at(tmp.path(), true);
        let mut out = Vec::new();
        serve(e.clone(), Cursor::new(vec![b'{', 0xff, 0xfe, b'}', b'\n']), &mut out).unwrap();
        let reply: Value = serde_json::from_slice(out.trim_ascii()).unwrap();
        assert_eq!(reply["error"]["code"], -32700);
    }

    #[test]
    fn truncation_helpers() {
        assert_eq!(truncate_chars("short", 300), "short");
        let long: String = "é".repeat(400);
        let cut = truncate_chars(&long, EXCERPT_MAX);
        assert_eq!(cut.chars().count(), EXCERPT_MAX);
        assert!(cut.ends_with('…'));
        let capped = cap_text("x".repeat(30_000), MAX_TEXT);
        assert!(capped.chars().count() <= MAX_TEXT);
        assert!(capped.ends_with("[output truncated]"));
        assert_eq!(collapse_whitespace("a\n  b\tc"), "a b c");
    }

    #[test]
    fn stdio_style_session_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _) = indexed_engine(tmp.path());
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"query":"ostrich"}}}"#,
            "\n",
        );
        let replies = run(&e, input);
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[2]["result"]["isError"], false);
        assert!(replies[2]["result"]["structuredContent"]["results"][0]["path"].as_str().unwrap().ends_with("b.txt"));
    }
}
