# DigUp indexing pipeline: Rust porting spec

Source: /root/dtfu-src (NOT /home/claude/dtfu-src; that path does not exist). Read-only analysis, repo untouched.
Citation form: `File.swift:line` (files under Sources/DigUpKit unless a prefix is given). "Unsure" flags are marked UNSURE.
Embedding model, search/ranking, and the SQLite store are out of scope except where the pipeline touches them.

---------------------------------------------------------------------------------------------------
## 1. Data model

```rust
// Model.swift:4  FileKind. Order = first-pass priority (Model.swift:10-13, SQL CASE; unknown kind = 6)
enum FileKind { Screenshot /*0*/, Image /*1*/, Pdf /*2*/, Doc /*3*/, Audio /*4*/, Video /*5*/, Code /*6, code index only*/ }

// Rules.swift:4
enum Classification { Index(FileKind), Skip(&'static str /*reported reason*/), Ignore /*unhandled type, counted "other type"*/ }

// Crawler.swift:5
struct Candidate { path: String, kind: FileKind, size: i64, mtime: f64 /*secs.nanos*/, inode: u64, device: u64 }

// Crawler.swift:14
struct CrawlReport {
  candidates: Vec<Candidate>,
  skipped: HashMap<String,usize>,          // reason -> files
  skipped_folders: HashMap<String,usize>,  // reason -> folders not entered
  datasets: HashMap<String,usize>,         // folder -> files dropped as dataset
}

// IndexStore.swift:212 (files table; schema at IndexStore.swift:96-124, schemaVersion 5 at :23)
struct FileRecord { id: i64, path: String, kind: FileKind, size: i64, mtime: f64, inode: u64, device: u64,
                    state: State /* pending|done|partial|skipped|failed */ }
// files also has: detail TEXT (reason / "read to page N of M"), info TEXT (JSON string->string map), indexed_at REAL.
// info keys seen: width,height,taken,ocr_chars | pages,scanned_pages,read_to,read_end | chunks,chars | lines,cells,repo
//                 | duration,frames,sound_windows
// segments(id, file FK cascade, kind, modality, loc REAL, loc_end REAL, excerpt TEXT, vec BLOB 768xf16 L2-normalised)
// fts5(name, body, original UNINDEXED) rowid == segments.id; tokenizer "porter unicode61 remove_diacritics 2 categories 'L* N* Co M*'" (IndexStore.swift:27)

// Model.swift:17
enum SegmentKind { Name, Image, Ocr, Page, PageImage, Chunk, Audio, Frame, Lines, Cells }
// Model.swift:32  which encoder produced the vector (scores normalised per modality at search time)
enum Modality { None, Text, Image, Audio }

// Model.swift:37  IndexOptions (defaults literal, see section 3). `fingerprint` (Model.swift:80) is stored in meta.
struct IndexOptions {
  code: bool, code_stretch: usize, max_code_bytes: i64, max_notebook_bytes: i64,
  include_code: bool, include_datasets: bool, min_image_side: u32,
  big_pdf_pages: usize, pdf_step: usize, doc_step: usize, max_pages: usize, max_chunks: usize, min_page_text: usize,
  photo_budget: u32, screenshot_budget: u32, page_budget: u32, frame_budget: u32,
  frame_interval: f64, audio_window: f64, audio_hop: f64,
  excluded_folders: Vec<String>, excluded_extensions: HashSet<String> }

// FolderSelection.swift:15  (user-facing include/skip model; see 2.9)
struct FolderSelection { chosen: Vec<String>, skipped: Vec<String>, kept: Vec<String> }
enum Mark { On, Mixed, Off }

// Extractors.swift:8
enum ExtractError { Unreadable(String), TooSmall(w,h), Locked, Empty(String), LeftOut(String) /*=> state "skipped"*/ }

// Extractors.swift:28  ImageExtractor.Prepared: scratch file path of downscaled copy, orig width/height, EXIF DateTimeOriginal string
struct PreparedImage { path: PathBuf, width: u32, height: u32, taken: Option<String> }
// Extractors.swift:97
struct PdfPage { number: u32 /*1-based*/, text: String, image_path: Option<PathBuf>, ocr: bool, garbled: Option<String> }
// MediaExtractors.swift:22
struct MediaPiece { path: PathBuf, start: f64, end: f64 }          // WAV window or JPEG keyframe, seconds
// CodeExtractor.swift:5-24
struct Piece { text: String, first: usize, last: usize }           // lines (or cells), 1-based
struct CodeFile { pieces: Vec<Piece>, title: String, repo: Option<String>, lines: usize, cells: Option<usize> }

// Indexer.swift:239/297-326
struct Job { files: Vec<FileRecord>, step: Option<Range<usize>> }   // step = later pass of a long pdf/doc (0-based)
struct Prepared { batch: Vec<FileRecord>, step: Option<Range<usize>>, folder: PathBuf,
                  outcomes: HashMap<usize,Outcome /*state, detail*/>, payload: Payload }
enum Payload { Nothing,
  Images{screenshot: bool, ready: Vec<ReadyImage{slot, image: PreparedImage, ocr: String}>},
  Pdf{pages: Vec<PdfPage>, read: Range<usize>, total: usize, title: Option<String>},
  Doc{chunks: Vec<String>, read: Range<usize>, total: usize, characters: usize},
  Audio{pieces: Vec<MediaPiece>, duration: f64},
  Video{frames: Vec<MediaPiece>, sound: Vec<MediaPiece>, duration: f64},
  Code(CodeFile) }
struct SyncSummary { added, changed, moved, removed, unchanged: usize, crawl: CrawlReport, queued: Vec<i64> } // Indexer.swift:20
struct Event { done, total, path, kind, state: String, seconds: f64, note: Option<String> }                    // :27
struct Step  { done, total, files, path, kind, state, seconds, note }                                          // :158 (later pass)

// Worker.swift:31  the only model interface the pipeline sees. One thread at a time.
trait Embedder {
  fn info(&self) -> WorkerInfo;                              // {model, revision, runtime, dtype, dim, text_only, load_seconds}; vector_source = "model@rev7 runtime dtype" (Worker.swift:25)
  fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
  fn embed_images(&self, paths: &[PathBuf], budget: u32) -> Result<Vec<Embedding{vector: Option<Vec<f32>>, error: Option<String>}>>;
  fn embed_audio(&self, paths: &[PathBuf]) -> Result<Vec<Embedding>>;
  fn close(&self);                                           // frees ~1 GB model
}
// Prompts (Model.swift:89): document = "title: {title|none} | text: {text}"; query = "task: search result | query: {q}";
//   code query = "task: code retrieval | query: {q}". Images/audio take no prompt.
// Estimator (Estimator.swift:173,194): Costs{image .12, screenshot .32, pdf_page .09, doc_chunk .095, audio_second .0089, video_second .019, code_stretch .037}
//   Estimate{files, pdf_pages, pdf_pages_total, audio_seconds, video_seconds, doc_chunks, seconds, long_files, later_seconds, code_stretches, bytes}
// PageTextProblem (SearchText.swift:165): GlyphOrder | LostLetters ; Script ranges indic 0x0900..=0x0DFF, arabic 0x0600..=0x06FF (:159)
```

Index state machine for a file: `pending -> done | skipped | failed | partial`; `partial -> partial... -> done`. Any change on disk -> `pending` again (segments deleted except the `name` segment).

---------------------------------------------------------------------------------------------------
## 2. Pipeline flow

### 2.0 Process topology (macOS app; decide what to keep)
- GUI app (watcher, power policy, pause) <-> helper process `digup index-service` over stdin/stdout. stdin cmds, one/line: `sync | sync code | stop | backfill on | backfill off` (EOF = stop). stdout: JSON events `missing|pruned|sync|file|rest|summary|waiting|idle|error|exit|reindex` (IndexService.swift:4-22, 334-337).
- Reason for the helper process: Vision keeps ~100 MB of models resident; process exit returns it (IndexService.swift:6-8). Rust has no such issue; a library + thread is fine, but a separate process is still useful to free the ~1 GB embedding model.
- Embedding model runs in a 3rd process/worker (JSON-lines) or linked llama.cpp; `SharedEmbedder` (IndexService.swift:296) shares one model between main index and code index. `Kept` wrapper makes close() a no-op for the second indexer.
- Two independent indexes/stores: main (`IndexStore.mainFile`) and code (`IndexStore.codeFile`), each with its own Indexer and scratch dir (Indexer.swift:55-60).

### 2.1 Crawl (Crawler.swift:27-156) - metadata only, no content reads
For each root (standardized path):
1. code mode: `repos.enterRoot(root)` (finds enclosing .git by walking up, runs `git ls-files`); else if `Rules.isProjectRoot(root)` add to `projectRoots` (:40-44).
2. Compute `skips` = options.excludedFolders that lie inside the root (by literal or real path) (:47-50). A skip outside the root does not apply: a chosen folder inside a skipped one is walked whole.
3. Recursive walk with `.skipsHiddenFiles`, `.skipsPackageDescendants`, errors ignored (:51-53). Per entry:
   - Directory (not package): `Rules.skipReason(folder)` (dev folder / .noindex / ~/Library / excluded) or in code mode `codeSkipReason` -> `skipDescendants`, count in skipped_folders. Else in code mode `repos.enter`, else `isProjectRoot` -> projectRoots (:56-67).
   - Package (bundle) -> skipped["app or bundle"] (:68-71).
   - File: `classify` (non-code) or `classifyCode` -> Ignore => skipped["other type"], Skip(r) => skipped[r], Index(kind) => continue:
     - `lstat`; must be a regular file else skipped["link or special file"] (:80-84) (symlinks are never followed/indexed).
     - Cloud placeholder check: SF_DATALESS flag (0x40000000) or iCloud not-downloaded -> skipped["in the cloud, not downloaded"] (:85-91).
     - code mode: size > (ipynb ? maxNotebookBytes : maxCodeBytes) -> "too big to be written by hand"; `repos.ignores(path)` -> "ignored by git" (:92-104).
     - Dedup by path set (nested roots) (:105); push Candidate with `st_mtimespec` as secs+nsec/1e9, st_ino, st_dev.
4. Post-pass (after whole walk, all in memory):
   - Non-code: any file whose directory is (or is under) a project root and kind != screenshot -> skipped["inside a code project"] (:121-137). `isInProject` walks up parents with memo (parent.count > 1 guard).
   - Non-code && !includeDatasets: `Rules.datasetFolders` over the kept candidates; drop image/audio/video files whose *immediate* folder is a dataset (:139-153).

Not applied in the non-code path: `isSecret` (key/password files) is applied ONLY in `classifyCode` (Rules.swift:72). Only the code index excludes `.pem/.key/.env...`. Non-code kinds (txt/md/pdf...) named `secrets.txt` or `id_rsa.txt` ARE indexed. Flag for a Rust port: consider applying isSecret globally (security decision, differs from Swift).

### 2.2 Sync / incremental re-index (Indexer.swift:86-129)
Change detection is metadata only. No hashing, no content compare.
```
crawl = Crawler::crawl(roots)
existing = store.files_under(roots)   // nested roots deduped; range scan path >= prefix AND < prefix+U+10FFFF (IndexStore.swift:221-231)
byPath, byInode["{device}:{inode}"]
in ONE transaction, for each candidate c:
  if row = byPath[c.path]:
      if row.size != c.size || |row.mtime - c.mtime| > 0.001 || row.kind != c.kind  -> store.requeue(row.id, c)   [changed]
          // requeue: delete all segments except 'name', set state=pending, detail/info/indexed_at=NULL, update size/mtime/inode/device/kind
      else unchanged
  elif row = byInode[c.device:c.inode], !seen.contains(row.path), !movedFrom.contains(row.path),
       row.size == c.size, |row.mtime - c.mtime| < 0.001   -> store.move(row.id, c.path)  [moved; no re-embed; fts name/folder rows rewritten]
  else store.insert(c)  [added; also inserts 'name' segment + fts row so the file name is searchable immediately]
removed: every existing row not in `seen`, not in `movedFrom`, and not under a root that no longer exists on disk
         ("vanished" roots: unplugged drive keeps its rows, :118-126) -> store.delete (fts rows + cascade)
```
Consequences: a newly excluded folder/extension/type is handled by removal (rows not in `seen`). A new `Rules` change that alters kind -> requeue via `row.kind != file.kind`. Excluded-folder/extension settings are NOT in the fingerprint.
`summary.queued` = ids of added+changed (used as the "new files" set).

### 2.3 Model/settings fingerprint (Indexer.swift:262-292, Model.swift:80-85)
- fingerprint = `"{embedder vector_source} | {options.fingerprint}"`. Non-code options.fingerprint literal: `photo{140} shot{280} page{280} frame{70}/{3.0}s audio{30.0}/{25.0}s`. Code: `code1 lines{1800}`.
- `checkModel()` before any work if `embedderSource` is known; else checked when the model first starts (`fullWorker`).
- Mismatch: if `resetOnModelChange` set -> `store.resetAll()` (drop every non-name segment+fts, all files -> pending, meta fingerprint cleared) and emit `reindex`; else throw `fingerprintMismatch` ("re-run with --reindex").
- NOTE: chunk size (1800), overlap (200), minPageText, render sizes, maxPixel (1024/1536/1400) and OCR/PDF constants are NOT in the fingerprint; changing them does not reindex automatically (only the code tag "code1" is a manual version bump). A Rust port should add an "extractor version" to the fingerprint.

### 2.4 Run (first pass) (Indexer.swift:139-154, 217-260)
- `queue = store.pending(limit, only)`: `state='pending'` ordered `kindOrder, mtime DESC` (screenshots first, then image, pdf, doc, audio, video; newest first inside a kind) (IndexStore.swift:303-312). Scope clause limits to currently available roots.
- `batches(queue)` (:245): consecutive files of the same kind among {image, screenshot} are batched up to 8; everything else is a batch of 1. (Batching is on queue adjacency only, so a kind boundary ends a batch.)
- `process(jobs)` (:217-235): a 2-stage software pipeline.
  - Prepare thread (single serial queue `DigUp.prepare`, qos utility, :345): decodes/OCRs/renders job N+1 while the main thread embeds+commits job N. Exactly one batch lookahead (`ahead`).
  - Commit (main thread): model call(s) + DB transaction. Embedder is single-threaded.
  - `shouldStop()` is polled only BETWEEN jobs (:226). A prepared-but-unused lookahead is deleted (:222).
  - Scratch folder per job `scratch/batch-N`, removed after commit (:230); per-process root `$TMPDIR/DigUp-<pid>/<uuid>`; stale `DigUp-<pid>` dirs of dead pids are removed at startup via kill(pid,0)/ESRCH (IndexService.swift:147-154).
- Per-file errors are recorded as state `failed` (detail = error), too-small images / `LeftOut` become `skipped`. Fatal (`WorkerError`, `IndexerError`) abort the run; pending files stay pending so the next run resumes (:469-473).

### 2.5 Per-kind extraction + commit (Indexer.swift:358-682)
**image / screenshot** (prepare :365-384, commit :494-522)
- Decode-to-thumbnail: max side 1024 JPEG q0.9 (image) / 1536 PNG (screenshot); skip if `max(w,h) < minImageSide(256)` (state `skipped`, detail "small image WxH").
- Screenshot additionally: OCR on the FULL-resolution decode (a second decode), `accurate`, language correction, auto language detect.
- Embed images with `budget` 140 (photo) / 280 (screenshot). Segments: `image` (modality image, vector). If OCR non-empty: `ocr` segment (modality text, excerpt+text for FTS, NO vector - deliberately keyword-only, :509-512). info: width,height,taken (EXIF DateTimeOriginal), ocr_chars (screenshots). One transaction for the whole batch; a per-image embedding error marks just that file failed.

**pdf** (prepare :385-393, commit :526-580; PDFExtractor Extractors.swift:112-151)
- Range: `job.step ?? (total > bigPDFPages(50) ? 0..<pdfStep(30) : 0..<maxPages(5000))` clamped to `0..<min(total, maxPages)`. So <=50-page PDFs are read whole in one job; longer PDFs: first 30 pages now, rest later (2.6).
- Per page: `text = page.string.trim()`; `PageTextProblem::check(text)`:
  - LostLetters (Arabic lam-ligature loss): if the OS OCR can read Arabic, render at 2000 px, OCR, accept only if `>= minText` chars, no problem, and contains an Arabic-script scalar -> page text = OCR (flag ocr=true). Else keep text layer if `>= minText`, else fall through to render.
  - GlyphOrder (Bengali/Hindi vowel signs misplaced): `garbled = text; text = ""` -> always rendered.
  - none: `text.count >= minPageText(200)` -> text page. Otherwise render.
  - Render: page fitted into 1400x1400 px (`mediaBox`), PNG into scratch -> `imagePath` set (scans, slides, short pages).
- Commit: `textPages` (no image) with `SearchText.hasWords(text)` get a text vector from `Prompts.document(title, text.prefix(4000))` (title = PDF metadata title or file stem); numeric-only pages: keyword only. `imagePages` embedded with page budget 280. `garbledPages` additionally get a text vector from the garbled text (vector only, no FTS/excerpt - "meaning only", :555-558).
- Segments: `page` (modality text, loc=page#, excerpt+text, vec if wordy), `pageImage` (image modality, loc=page#, excerpt/text if any text), garbled `page` (vector only).
- State: `partial` while `read.upperBound < min(total,maxPages)`, with info `read_to`, `read_end`; else `done`. detail e.g. "read to page 30 of 120" or "the first 5000 of N pages". `skipped "no pages"` only for a first step with zero pages.
- Locked PDF -> `ExtractError.locked` ("password-protected") => failed.

**doc** (prepare :394-400, commit :583-614; DocExtractor Extractors.swift:164-245)
- Text: txt/md/text/markdown read as text (UTF-8, else Latin-1); html/htm -> regex tag stripper; rtf/docx/doc/odt -> NSAttributedString.
- `chunks(text, size 1800, overlap 200, limit maxChunks+1)`; read slice = `job.step ?? 0..<docStep(40)` clamped to `0..<min(chunks.count, maxChunks)`; payload.total = chunks.count (counted up to maxChunks+1 so "over the cap" is detectable).
- Commit: chunks with `hasWords` embedded as `Prompts.document(title=file stem, text=chunk)`. Segments `chunk`, loc = global chunk number (1-based), excerpt(240) + text for FTS, vector only if wordy. `total==0` => skipped "no text". info: chunks, chars, read_to/read_end while partial.

**audio** (prepare :401-406, commit :644-658): `AudioExtractor.windows(window 30, hop 25)` -> WAVs (section 3.7) -> `embedAudio` -> `audio` segments loc=start loc_end=end. No pieces => skipped "silent or shorter than 2 s". info: duration. No audio track => ([],0) => same skip message.

**video** (prepare :407-416, commit :660-682): keyframes (every 3 s, dedup) -> `frame` segments (image modality, budget 70, loc=start, loc_end=next kept frame start or duration); then if the file has an audio track, the same audio windowing on the soundtrack -> `audio` segments. Both embedded in one commit. skipped "no frames or sound" if both empty. info: duration, frames, sound_windows.

**code** (code index only): `CodeExtractor.read` (cut lines/cells), `hasWords` gate, `Prompts.document(title = path in repo, text)`; segments `lines`/`cells` with loc, loc_end = first/last line; info lines,cells,repo. See 3.9.

### 2.6 Long-file backfill / "later pass" (`finishLongFiles`, Indexer.swift:174-209)
- Runs only when nothing is pending (IndexService.swift:119-123). Rationale: a book's pages 31..N matter less than every other file's first items.
- `store.unfinished()` = state `partial`, same ordering (kind, mtime desc). Per file: `start = info.read_to ?? lastPlace(file) ?? 0`; `end = max(info.read_end ?? 0, start+1)` (at least one step: settles docs whose length was only guessed in schema <5). Job per step `start..<min(start+size,end)` with size = pdfStep(30) for pdf, docStep(40) for docs.
- Each step is re-extracted from scratch: a PDF is reopened, a doc's WHOLE text is re-read and re-chunked (see perf 5.x) because chunking is deterministic so step N continues exactly at chunk N.
- Each step = one transaction (segments + info update), so stopping between steps loses nothing. `read_to` updated each step. A step that errors calls `stopReading`: file becomes `done` with detail "read to page/chunk N, not after: <err>" and keeps what it has (:485-492).
- `skip:` closure drops queued steps of a file that already finished sooner than planned (:204-207).
- Progress (`Step.done/total`) counts pages+chunks across all queued steps.

### 2.7 Service loop, priorities, pause/resume (IndexService.swift:66-132)
State flags under a lock: `syncRequested`(init true), `codeSyncRequested`(init true), `stopRequested`, `backfillAllowed`; a semaphore `wake`.
```
startup: for each part: checkModel(); if keepOnly: store.prune(keepingUnder roots) -> event "pruned"
loop:
  if syncRequested(clear): sync(main) -> if queued.count <= newFilesLimit(50) add to newFiles   [event "sync"]
  if stop: break
  if !modelReady(): emit "waiting"(for model); break      // sync-only mode: file names searchable before the model downloads
  if newFiles non-empty: run(main, only=newFiles); newFiles = left(...); continue     // ALWAYS runs, any power state
  if pendingCount>0 && backfillAllowed: run(main, only=nil); if syncRequested continue
  if code part exists && (main.pending == 0 || !backfillAllowed):      // code index second class
        if codeSyncRequested(clear): sync(code)
        newCode -> run(code, only) ; else if code.pending>0 && backfillAllowed: run(code)
  if backfillAllowed && main.pending==0 && code.pending==0 && main.unfinished.files>0: finishLongFiles(); continue if sync requested
  if code && codeSyncRequested: continue
  emit "idle"; if model running: waitForWork(linger 60 s default) else exit
```
- Run-level interrupt: `shouldStop` = `syncRequested || stopRequested || (!isNew && !backfillAllowed) || (part.code && codeSyncRequested)`; later pass: `syncRequested || codeSyncRequested&&code!=nil || stopRequested || !backfillAllowed`. Checked between jobs only (latency = one job: up to 8 images, 30 PDF pages, 1 long video...).
- "New files" rule: a sync that queues <= 50 files indexes them immediately regardless of power; a bigger sync (first index, big copy) waits for `backfillAllowed` (CLI flag `--backfill`, toggled by `backfill on/off`).
- Pause = app-level (Engine.swift:697-702): sets paused, sends `stop` (helper finishes the current job, exits); resume = start helper again. Pending state is entirely in the DB, so resume is free. Helper exits ~60 s after idle (`--linger`, default 60, main.swift:429).
- Missing/unreadable roots are re-evaluated before every sync (`availableRoots`, :157-168) and the store `scope` is set to the available roots so pending/counts ignore files on unplugged drives; their rows are kept.

### 2.8 App-side hooks that a Rust port must reproduce (Engine.swift, FolderWatcher.swift, PowerPolicy.swift)
- Watcher: FSEvents file-level, `NoDefer`, ignore hidden paths (FolderWatcher.swift:3-6,17-18). Main folders: debounce **0.75 s** then `sync` (Engine.swift:441-447). Code folders: latency 2 s watcher; debounce = quiet `codeQuietSeconds` (default **60**, AppSettings.swift:39) capped by `codeLongestWait = 600` s; ignores paths with any hidden component or a component in `codeNoise` (node_modules, bower_components, venv, site-packages, __pycache__, DerivedData, Pods, Carthage, vendor, Vendor, third_party, build, Build, dist, out, target, obj, coverage, wandb, mlruns, lightning_logs) (Engine.swift:401-433).
- Power policy (`backfillBlocker`, PowerPolicy.swift:54-63): backfill waits when Low Power Mode, thermal state serious/critical ("Mac is warm"), or (setting "index on battery" OFF and not on AC). Default: battery allowed. Event-driven (IOPS notification, thermalStateDidChange, NSProcessInfoPowerStateDidChange); on change app sends `backfill on|off`. New-file indexing is never blocked.
- Helper CLI: `index-service <folders> --index DIR [--backfill] [--prune] --exclude F --exclude-ext E --code-root D --code-exclude D --new-limit 50 --linger 60`.

### 2.9 FolderSelection semantics (FolderSelection.swift) - port verbatim, pure logic
- `is_inside(path, folder)`: `path.starts_with(folder-with-trailing-slash)` (whole-name compare, `/a/bc` is not inside `/a/b`) (:95).
- `is_searched(path, folders, skipped)`: exists folder f where (path==f or inside f) AND NOT exists skip s where (s inside f) AND (path==s or inside s) (:32-36). A skip only affects folders that contain it, so a chosen folder inside a skipped folder is searched whole.
- `folders()` (what the indexer gets): unique(kept+chosen) minus any folder already searched by another (:47-53). `effective_skips` = skips inside some resulting folder (:58). `search_as(path)` = longest matching searching folder (:40).
- `mark(f)`: On/Mixed/Off (:65-70). `set_folder(f,on)` clears direct-child skips/choices of f then `set_subfolder`; `set_subfolder` on: remove skip, add to chosen if not already searched; off: remove from chosen, add to skipped if currently searched (:73-90). `unskip`. `real_path` (realpath, for /var vs /private/var) (:104). Preserve ordering (Vec) and uniqueness-by-first-occurrence (:109).

---------------------------------------------------------------------------------------------------
## 3. Exact rules and constants

### 3.1 IndexOptions defaults (Model.swift:37-69)
| field | value | line |
|---|---|---|
| codeStretch | 1800 chars | 43 |
| maxCodeBytes | 1<<20 (1 MiB) | 46 |
| maxNotebookBytes | 20<<20 (20 MiB) | 48 |
| includeCode / includeDatasets | false / false | 49-50 |
| minImageSide | 256 (applied to max(w,h)) | 51 |
| bigPDFPages | 50 | 54 |
| pdfStep | 30 pages | 55 |
| docStep | 40 chunks (~64,000 chars per comment; 40x1600) | 56 |
| maxPages | 5_000 | 58 |
| maxChunks | 5_000 (~8M chars) | 59 |
| minPageText | 200 chars | 60 |
| photoBudget / screenshotBudget / pageBudget / frameBudget | 140 / 280 / 280 / 70 vision soft tokens (valid: 70,140,280,560,1120) | 61-64 |
| frameInterval | 3.0 s | 65 |
| audioWindow / audioHop | 30.0 s / 25.0 s (5 s overlap) | 66-67 |

### 3.2 Other literal constants
- Image thumbnail max pixel: 1024 JPEG (photos), 1536 PNG (screenshots), 1024 JPEG (video frames), JPEG quality 0.9 (Indexer.swift:371-373, 410; Extractors.swift:68).
- PDF page render: 1400 px square-fit PNG (Indexer.swift:392); OCR re-render for Arabic: 2000 px (Extractors.swift:124).
- PDF page text passed to the model: first 4000 chars (Indexer.swift:546, 551). Doc chunk: 1800 chars so no cut.
- Excerpt: whitespace-collapsed one line, limit 240 chars then "…" (Extractors.swift:249). Code summary line limit 160 (CodeExtractor.swift:~142).
- Batch size for images: 8 (Indexer.swift:251).
- Video max frames 1200 (MediaExtractors.swift:105); aHash 8x8, distance <= 4 bits => same shot (:129).
- Audio: 16 kHz mono s16le, RMS threshold 0.003 (~-50 dBFS) on samples/32768, min tail 2.0 s (MediaExtractors.swift:29,34,54,76).
- New-files limit 50, linger 60 s, code quiet 60 s, longest wait 600 s, watcher debounce 0.75 s (see 2.7/2.8).
- Cost model (Estimator.swift:173-182): image .12, screenshot .32, pdfPage .09, docChunk .095, audioSecond .0089, videoSecond .019, codeStretch .037 s (M4 Pro, llama.cpp Q8_0; "will be calibrated on first launch" - not implemented in the files I read). Code size: 2.4 B/char + 1600 B/stretch, 1400 chars per stretch avg (:187-192).

### 3.3 Classification, non-code (Rules.swift:108-123)
Order of checks (first match wins):
1. `excludedExtensions.contains(ext)` -> Skip("excluded type"). ext = lowercased path extension.
2. ext in {ts, mts, m2ts}: if `isMPEGTransportStream` -> Index(video); else includeCode ? Index(doc) : Skip("code or data file").
3. ext in docExtensions -> Index(doc).
4. ext in codeExtensions -> includeCode ? Index(doc) : Skip("code or data file").
5. no UTType -> Ignore.
6. type conforms sourceCode|script -> same code rule. pdf -> Index(pdf). image -> Index(isScreenshot ? screenshot : image). movie -> video. audio -> audio. else Ignore.

Lists (literal):
- `docExtensions` (Rules.swift:25): txt text md markdown rtf docx doc odt html htm.
- `codeExtensions` (:28-35, data/code, skipped by default): py ipynb c h cc cpp cxx hpp hh m mm swift tsx cts js jsx mjs cjs go rs java kt kts scala rb php cs fs sh bash zsh fish ps1 lua pl r jl dart sql vue svelte css scss sass less json jsonl yaml yml toml xml plist gradle cmake mk lock log csv tsv ini cfg conf proto graphql tf nix zig hs ex exs erl clj elm ml vim el asm metal glsl hlsl wgsl cu pyi pyc.
- `screenshotPrefixes` (:37, case-sensitive hasPrefix on file name): "Screenshot", "Screen Shot", "CleanShot", "SCR-". Plus macOS xattr `com.apple.metadata:kMDItemIsScreenCapture` > 0 bytes (:140-144).
- Image/audio/video/pdf recognition is by UTType conformance (OS database), NOT by a list in the repo. UNSURE/important: the Rust port must define its own extension (+ magic) tables. Suggested starting points: image = jpg jpeg png gif webp bmp tif tiff heic heif avif jxl + RAW (cr2 cr3 nef arw dng orf rw2 raf); audio = mp3 m4a aac wav flac ogg oga opus wma aiff aif; video = mp4 m4v mov mkv avi webm wmv flv mpg mpeg 3gp mts/m2ts (TS sniff). NOTE Spotlight/UTType classes gif as image, svg excluded? (UNSURE: svg conforms to public.image on macOS, so Swift would send svg to ImageIO; decide explicitly).
- MPEG-TS sniff (:130-137): read first 600 bytes, need >= 400 bytes; true if bytes[0],[188],[376] == 0x47 OR bytes[4],[196],[388] == 0x47 (192-byte M2TS with 4-byte timecode).

### 3.4 Folder skip rules (Rules.swift:14-16, 146-152; Crawler enumerator options)
- Hidden files/dirs (name starts with "."; also macOS hidden flag via `.skipsHiddenFiles`): never entered/listed. Consequence: `.git`, `.venv`, `.cache`, `.Trash`, `.npm`... excluded automatically. UNSURE whether Foundation also honors UF_HIDDEN flag (it does on macOS); Windows hidden attribute should be a port decision.
- Packages/bundles (`isPackage`, e.g. .app, .photoslibrary, .xcodeproj) not descended; counted "app or bundle".
- `skippedFolderNames` = node_modules, bower_components, venv, site-packages, __pycache__, DerivedData, Pods, Carthage -> "developer folder" (matched on folder name anywhere in tree). Note `venv` but not `env`, `.venv` is hidden anyway.
- name suffix `.noindex` -> ".noindex folder". `path == $HOME + "/Library"` -> "~/Library" (exact; macOS-only meaning).
- `excluded` (user skips inside this root): equality or prefix + "/" -> "excluded folder".
- Code index extra (`codeSkippedFolderNames`, :60-64): vendor Vendor vendored third_party third-party thirdparty 3rdparty external extern deps build Build dist out target obj coverage htmlcov generated __generated__ wandb mlruns lightning_logs -> "vendored or built code"; a folder containing a `.git` FILE (submodule/worktree) -> "submodule or worktree" (:98-105).

### 3.5 Code-project rule (non-code index) (Rules.swift:19-23, 155-160; Crawler.swift:121-137)
- Folder is a "code project" if it directly contains any of: `.git package.json Package.swift pyproject.toml setup.py requirements.txt Cargo.toml go.mod pom.xml build.gradle build.gradle.kts Gemfile composer.json CMakeLists.txt` or any entry with extension `xcodeproj` / `xcworkspace`.
- Inside a project (the project root or any descendant), only `screenshot` kind survives; all docs/pdfs/images/audio/video -> "inside a code project". Applies even when the user chose the project folder itself as a root. (Test `nestedRootsAreCrawledOnce` uses a plain tmp folder.)
- `isProjectRoot` does a separate readdir for every visited folder (cost; see 5).

### 3.6 Dataset rule (Rules.swift:162-188)
- Group = (immediate parent folder, kind) over kept image/audio/video files (screenshots and docs/pdfs excluded) whose folder is not itself a chosen root. Group is a dataset if `count >= 300` AND `machine_named / count >= 0.7`. Reported map folder -> total files (sums kinds). Then ALL image/audio/video files in that folder are dropped ("dataset").
- `looksMachineNamed(stem)`: lowercase; if starts with any of `img dsc pxl mvimg vid gopr dji photo whatsapp screenshot` -> false; else true if `^[0-9a-f]{8,}([-_][0-9a-f]{4,})*$` (hex/uuid-ish) or `^[a-z]{0,16}[-_. ]?\d{3,}([-_.]\d+)*$` ("clip_000123", "frame-0042", "000123"). Regex engine: ICU (so `\d` is Unicode). In Rust use `regex` (Unicode `\d` too by default) - identical.
- stem = file name minus its extension (NSString deletingPathExtension).

### 3.7 Audio windowing algorithm (MediaExtractors.swift:34-78)
```
rate = 16000; W = 30*rate = 480000; H = 25*rate = 400000
buf: Vec<i16>; offset = 0 (sample index of buf[0])
for each decoded chunk (16 kHz mono s16le): buf.extend(chunk)
   while buf.len() >= W:
        emit(W): if rms(buf[..W]) > 0.003 { write WAV(buf[..W]); piece(start=offset/rate, end=(offset+W)/rate) }
        buf.drain(..H); offset += H
tail: covered = if offset==0 {0} else {W - H}          // 5 s overlap already emitted
      if buf.len() - covered >= 2*rate: emit(buf.len()) // whole remaining buffer incl. overlap
duration = asset duration if finite else (offset+buf.len())/rate
```
- rms = sqrt(mean((s/32768)^2)) over the whole window. Silent windows are dropped but offset still advances.
- WAV: 44-byte canonical PCM header (RIFF, fmt 16, fmt=1, ch=1, rate 16000, byterate 32000, align 2, bits 16, data). Each window = 960,000 B.
- Decoder is AVAssetReader linear-PCM output (Apple resampler + downmix). Track = first audio track only (`loadTracks(.audio).first`).
- Comment says the model ignores audio past 30 s of an input, so window <= 30.

### 3.8 Video keyframes (MediaExtractors.swift:105-138, 140-156)
- If duration not finite/<=0 or no video track -> no frames (but hasAudio reported).
- `step = max(interval(3.0), duration/1200)`; times = stride(from: min(0.5, duration/2), to: duration, by: step); AVAssetImageGenerator with `appliesPreferredTrackTransform = true` (rotation), max size 1024x1024 (aspect kept), tolerance before/after = step/2 (seek snaps to nearest sync sample within +-step/2; `actualTime` is stored as the frame start).
- Dedup: 8x8 grayscale, `CGContext.interpolationQuality = .low`, mean = integer mean (`sum/64` truncating), bit i set if `pixel > mean` (index order = row-major as drawn; CG context is bottom-up so row 0 of the buffer is the TOP row only if the context is flipped - UNSURE; irrelevant to the Rust port since hashes are not persisted). A frame is dropped if `popcount(h ^ last_kept_hash) <= 4`; `last_kept` only updates on kept frames (compares against last KEPT, not previous sampled). Hash is not stored in the DB.
- Frame `end` = next kept frame's start; last = duration. First kept = the first sampled frame always.

### 3.9 Code index (read for completeness; also in scope of "Rules")
- classifyCode (Rules.swift:68-85) order: excludedExt -> Skip("excluded type"); isSecret(name) -> Skip("key or password file"); lower name ends `.min.js|.min.mjs|.min.css` -> Skip("minified"); lower name has prefix license|licence|copying|notice -> Skip("license"); ext ts|mts and TS sniff -> Ignore (video); sourceExtensions | codeTextExtensions | ipynb | codeFileNames | name.hasPrefix("Dockerfile") -> Index(code); in codeExtensions -> Skip("data file"); else Ignore.
- sourceExtensions (:42-49), codeTextExtensions md markdown rst adoc org txt yaml yml toml (:51), codeFileNames Makefile makefile GNUmakefile Dockerfile Containerfile Justfile justfile Rakefile Gemfile Podfile Brewfile Procfile Vagrantfile Jenkinsfile (:53-56). NOTE html/htm/css are in sourceExtensions for code but html is also a doc in the main index.
- isSecret (:88-95): ext (lowercased) in pem key p12 pfx keystore jks ppk kdbx gpg asc crt cer der p8 mobileprovision; OR lowercased name hasPrefix any of id_rsa id_dsa id_ecdsa id_ed25519 .env secret credential .netrc .npmrc .pypirc htpasswd .htpasswd.
- Git: for a repo (dir with `.git` directory) run `git -C repo ls-files -z --cached --others --exclude-standard` with env GIT_OPTIONAL_LOCKS=0, PATH=/usr/bin:/bin, HOME; keep only listed files (relative paths). If git missing/failed: keep everything (Crawler.swift:211-247). Git binary lookup is macOS-specific (xcode-select link, CLT, Xcode, homebrew) (:215-224).
- CodeExtractor (CodeExtractor.swift): `longestPiece = 4000`; reject: contains NUL in first 8000 bytes ("not text"), contains "PRIVATE KEY-----" (anywhere), for source: top-1000-chars contains "DO NOT EDIT" | "@generated" | "Code generated by" | "Autogenerated by" | "Generated by the protocol"; minified if chars/lines > 300 and some line > 3000 chars. Stretch cutter: whole lines up to 1800 chars; prefer break (in last 60%, i.e. keep >= 40%) at a column-0 block start after a blank line, then any blank line / block start; if forced mid-block next stretch restarts 2 lines earlier (`max(end-2, start+1)`). Notebook: cells (nbformat 4 `cells`, nbformat 3 `worksheets[0].cells` with `input`), outputs ignored, cells packed up to 1800 chars joined with "\n\n"; a longer cell is line-split but all pieces point at that cell number.

### 3.10 Text chunking (DocExtractor.chunks, Extractors.swift:222-245) - port exactly
```
clean = text.replace(/[ \t ]+/g," ").replace(/\n\s*\n\s*\n+/g,"\n\n").trim()
start = 0
while start < len && chunks.len < limit:           // len/indices are Swift Characters (grapheme clusters), NOT bytes
    end = min(start+size, len)
    if end < len: if let space = last whitespace in clean[start..end] with (space - start) > size/2 { end = space }
    chunk = clean[start..end].trim(); if !empty push
    if end == len break
    next = end - overlap, clamped: if (end - overlap) would go before start then next = end   // index(offsetBy:-overlap, limitedBy:start) returns nil if it would pass `start`
    if next <= start { next = end }
    while next < end && !clean[next].isWhitespace { next += 1 }    // advance to a word boundary; lands ON the whitespace char
    start = if next < end { next } else { end }
```
Notes: size 1800 / overlap 200. Whitespace test = Character.isWhitespace (Unicode White_Space, incl. newlines). The next chunk begins at a whitespace char (trimmed away). `limit` = maxChunks+1. Because indices are grapheme clusters, in Rust use `unicode-segmentation` graphemes (or accept `char` indices; chunk boundaries will differ slightly for combining sequences/emoji/CRLF - harmless unless you must match an existing index). UNSURE: `index(offsetBy:-overlap, limitedBy:start)` returns nil when the target is before `start`, i.e. if chunk length < overlap, `?? end` => next=end.

### 3.11 HTML to text (Extractors.swift:186-219)
Sequential regex passes: remove `<script>..</script>`, `<style>..</style>`, `<!--..-->`, `<head>..</head>` (case-insens., dot-all) -> " "; block-ending tags `<br|/p|/div|/li|/h1-6|/tr|/section|/article ...>` -> "\n"; any `<...>` -> " "; decode entities &nbsp; &amp; &lt; &gt; &quot; &#39; &apos; &mdash; &ndash; &hellip; (dictionary iteration order is arbitrary in Swift: `&amp;lt;` double-decoding order-dependent, UNSURE/bug-prone) then numeric `&#NNN;` / `&#xHH;` (invalid scalar -> " "); `[ \t]+`->" "; `\s*\n\s*`->"\n"; trim. Rust: `scraper`/`html2text`/`lol_html` will do better (and decode all entities).

### 3.12 Text-vs-numeric gate: `SearchText.hasWords` (SearchText.swift:37-43)
Over non-whitespace Unicode scalars: `letters` = scalars in CharacterSet.letters (Apple: general categories L* AND M* - letters + marks, so Bengali/Arabic vowel signs count), `others` = all other non-whitespace (digits, punctuation, symbols). Wordy iff `letters > 0 && letters*4 >= others`. i.e. >= 20% letters. Applied to PDF text pages, doc chunks, code stretches. Failing => segment stored with excerpt/FTS text but NO vector (keyword only). Test: a 4000-row numeric dump is false; "Revenue 1,234,567 · Net income 234,567 · Margin 19% in the third quarter" and Bengali "কলকাতায় বৃষ্টি হচ্ছে" true (LongFilesTests.swift digitsGetKeywordSearchOnly). Rust: use `\p{L}|\p{M}` (`regex` crate / `unicode-general-category`), NOT `char::is_alphabetic` (doesn't include all Mn/Mc).

### 3.13 PageTextProblem detector (SearchText.swift:165-211)
Text first NFC (`precomposedStringWithCanonicalMapping`). Scan scalars with `previous`, `beforePrevious`:
- Count `vowelSigns` = Bengali (0x09BE-0x09C4, 09C7, 09C8, 09CB, 09CC, 09D7, 09E2, 09E3) and Devanagari (093A, 093B, 093E-094C, 094E, 094F, 0955-0957, 0962, 0963) dependent vowel signs; `misplaced` if previous scalar is not a consonant/nukta (Bengali 0995-09B9, 09BC, 09CE, 09DC, 09DD, 09DF, 09F0, 09F1; Devanagari 0915-0939, 093C, 0958-095F, 0979-097F).
- `lostLetters`++ when beforePrevious == U+0627 (bare alef), previous in {'?','.',U+FFFD}, current in Arabic block 0600-06FF and alphabetic.
- Result: `vowelSigns >= 10 && misplaced*20 >= vowelSigns` -> GlyphOrder; else `lostLetters >= 2` -> LostLetters; else none. (Evidence cited in comments: R@1 0.75 vs 0.73 for glyph-order picture + garbled-text vector, Extractors.swift:107-111.)
This is a workaround for PDFKit's text extraction; pdfium/poppler/mupdf behave differently, so the port should re-measure how often the check fires and may need a different threshold.

### 3.14 Estimator formulas (Estimator.swift:232-279)
- pdf: `total = pages(file)` (Spotlight kMDItemNumberOfPages, fallback CGPDFDocument pages, fallback 1); `all = min(total, maxPages)`; `first = total > 50 ? min(30, all) : all`; seconds += first*0.09; if all>first: longFiles+=1, laterSeconds += (all-first)*0.09.
- doc: `all = max(1, min(maxChunks, size/2000))`; `first = min(all, docStep)`; seconds += first*0.095; rest to laterSeconds. (2000 B/chunk estimate vs 1800 chars real.)
- audio/video: duration from Spotlight kMDItemDurationSeconds, fallback AVURLAsset; seconds += dur*cost.
- code: chars = ipynb ? size/8 : size; stretches = max(1, ceil(chars/1400)); bytes += chars*2.4 + stretches*1600.
- `estimates(bySubfolder:)` groups candidates by first path component below the folder; files directly in it keyed under the folder itself; handles /var vs /private/var by matching both prefixes (:284-296).

---------------------------------------------------------------------------------------------------
## 4. macOS-only APIs and Rust replacements

| Where | API | Purpose | Rust replacement / notes |
|---|---|---|---|
| Crawler.swift:30,51,55 | FileManager.enumerator, URLResourceValues (isDirectory, isPackage, contentType, isUbiquitousItem, ubiquitousItemDownloadingStatus), `.skipsHiddenFiles`, `.skipsPackageDescendants` | Walk + type via UTType + hidden/bundle skipping | `ignore::WalkBuilder` (parallel, skip hidden, no symlink follow) or `walkdir`. Replace UTType with extension+magic tables (`infer`, `mime_guess`, `file-format`). Package skip: macOS-only (`.app` etc.); keep a short list of bundle extensions on macOS only. |
| Crawler.swift:80-91 | `lstat`, `st_flags & SF_DATALESS (0x40000000)`, iCloud download status | Don't trigger cloud downloads | macOS: libc `lstat`/`MetadataExt` + `st_flags` (nix/libc), `NSURLUbiquitousItemDownloadingStatusKey` has no pure-Rust equivalent (objc2-foundation). Windows: `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS (0x400000)` / `FILE_ATTRIBUTE_RECALL_ON_OPEN (0x40000)` / `FILE_ATTRIBUTE_OFFLINE (0x1000)` via `MetadataExt::file_attributes()` (OneDrive Files On-Demand). Linux: no standard (FUSE/rclone mounts); skip nothing. |
| Crawler.swift:106-108 | `st_mtimespec`, `st_ino`, `st_dev` | change + move detection | Unix: `MetadataExt::{mtime, mtime_nsec, ino, dev}`. Windows: `st_ino` unavailable on stable (`file_index()` unstable); use crate `file-id` or `same-file`/`winapi-util` (`BY_HANDLE_FILE_INFORMATION`: volume serial + 64-bit file index). Note NTFS ino can be unreliable on FAT/exFAT/network shares; make move detection best-effort. mtime granularity differs (FAT 2 s, exFAT 10 ms, HFS+ 1 s) so the 0.001 s tolerance is fine only for ns-resolution FS. |
| Rules.swift:140-144 | `getxattr("com.apple.metadata:kMDItemIsScreenCapture")` | Detect real macOS screenshots | `xattr` crate on macOS only. Elsewhere: filename prefixes only. Add platform prefixes: Windows "Screenshot (N)", Snipping Tool "Screenshot YYYY-MM-DD", GNOME "Screenshot from ", KDE Spectacle "Screenshot_", ShareX/Greenshot patterns. (Swift prefix "Screenshot" already matches the first three.) |
| Rules.swift:146-152 | `home + "/Library"` | skip ~/Library | Per-platform defaults: macOS ~/Library; Windows AppData, Program Files, Windows; Linux ~/.cache, ~/.local/share/Trash (hidden anyway), /proc,/sys,/dev if root chosen. Use `dirs`. |
| Extractors.swift:38-57 | ImageIO `CGImageSourceCreateWithURL`, `CopyPropertiesAtIndex` (pixel w/h, EXIF DateTimeOriginal), `CreateThumbnailAtIndex(MaxPixelSize, WithTransform=EXIF orientation, FromImageAlways, ShouldCacheImmediately)` | Header read + decode-direct-to-thumbnail without full-res decode; HEIC/RAW/WebP/etc | `image` crate (jpeg/png/webp/gif/bmp/tiff/avif) + `zune-jpeg`/`fast_image_resize`; JPEG DCT-scaling draft mode via `jpeg-decoder`/`zune-jpeg`+`turbojpeg` (libjpeg-turbo scale 1/2,1/4,1/8) gives the same "never decode 48 MP" behavior; `image::ImageReader::into_dimensions` for header-only size. EXIF: `kamadak-exif` / `nom-exif` (DateTimeOriginal, Orientation - you must apply orientation manually). HEIC/HEIF: `libheif-rs` (needs libheif, LGPL) or `heic` bindings; macOS can call ImageIO. RAW: `rawloader`/`rawler`, or embed-preview extraction via `kamadak-exif`/`libraw`. Quality diff: ImageIO downscale is high quality (Lanczos-like); use `fast_image_resize` Lanczos3 or CatmullRom to match. Color management (ICC -> sRGB) is automatic in ImageIO; in Rust use `lcms2`/`moxcms` if wide-gamut photos matter (model embeds are probably insensitive). |
| Extractors.swift:64-71 | `CGImageDestination` PNG/JPEG encode, q 0.9 | scratch files for worker | `image::codecs::{jpeg,png}`; or skip the disk round trip and send bytes/memory to the in-process embedder. |
| Extractors.swift:74-94 | Vision `VNRecognizeTextRequest` (accurate, language correction, auto-detect language, topCandidates(1), joined "\n"), `supportedRecognitionLanguages` | Screenshot OCR (~0.1-0.24 s per Retina screenshot), Arabic page OCR | Cross-platform: `ocrs` (pure Rust, rten, English/Latin only, lower accuracy), `tesseract` / `leptess` (C, many languages, ~0.5-2 s/screenshot, weaker on small UI text), PaddleOCR via `ort` (ONNX; best accuracy; PP-OCRv4/v5 det+rec ~0.2-0.5 s on CPU for 1440x900), `rapidocr` ONNX. Windows: `Windows.Media.Ocr` via `windows` crate is built-in, good quality, fast. Linux has no system OCR. Plan: trait `Ocr` with platform backends (Vision on mac via `objc2-vision`, Windows.Media.Ocr, ONNX PaddleOCR/ocrs fallback). Expect worse exact-token recall on tiny UI text than Vision `.accurate`; the Swift author measured that exact lookups (e.g. "externally-managed-environment") depend on OCR quality (Extractors.swift:75-78). |
| Extractors.swift:114-158 | PDFKit `PDFDocument(url:)`, `isLocked`, `page(at:)`, `page.string`, `documentAttributes[title]`, `PDFPage.thumbnail(of:for:.mediaBox)` -> CGImage | Page text, page count, title, render page as picture | `pdfium-render` (best fidelity, needs pdfium binary; text + render + metadata + lock detect), or `mupdf` (AGPL!) , or `lopdf`/`pdf-extract` (text only, poor on layout/CJK/Indic), `poppler` via `poppler-rs`/CLI `pdftotext -layout`, `pdftoppm`. Recommendation: `pdfium-render` with pdfium-binaries. Behavior differences: (1) text order / ligature handling differs, so `PageTextProblem` heuristics must be revalidated (Bengali/Arabic claims are about PDFKit specifically); (2) `thumbnail(for: .mediaBox)` ignores rotation? (UNSURE) - use mediabox + page rotation consistently; (3) encrypted detection: pdfium returns password error. |
| Extractors.swift:164-176 | `NSAttributedString(url:options:[.documentType: .officeOpenXML/.docFormat/.openDocument/.rtf])` | docx/doc/odt/rtf to text. (Comment: HTML import runs WebKit and needs main thread; they avoided that by regexing HTML.) | docx: `docx-rs`/`docx-lite`/unzip + parse `word/document.xml` (`zip` + `quick-xml`; text in `<w:t>`, breaks `<w:p>`,`<w:br>`). odt: unzip `content.xml`. rtf: `rtf-parser`/`rtf-to-text` or `unrtf`. legacy .doc (OLE2/CFB): no good pure-Rust text extractor (use `cfb` + manual WordDocument stream parse - hard; or shell out to `antiword`/`catdoc`/LibreOffice headless; or skip with a clear reason). Quality: Cocoa does handle .doc; Rust loses this. |
| Extractors.swift:179-181 | `String(contentsOf:usedEncoding:)` then Latin-1 fallback | Charset sniffing for text | `encoding_rs` + `chardetng` (BOM, UTF-16, then detection), final fallback Windows-1252 rather than strict ISO-8859-1. |
| Extractors.swift:224-230,188-200 | `NSRegularExpression`/`replacingOccurrences(.regularExpression)` (ICU) | cleaning, html strip | Rust `regex` crate (Unicode-aware, no lookaround; patterns used need none). `\s` in ICU includes Unicode spaces; same in `regex` (Unicode by default). |
| MediaExtractors.swift:36-78 | AVFoundation `AVURLAsset`, `loadTracks`, `AVAssetReader`+`AVAssetReaderTrackOutput` with LPCM 16 kHz mono 16-bit output settings, `CMSampleBuffer*` | Decode any audio/video soundtrack, resample + downmix | `symphonia` (pure Rust demux/decode: mp3 aac(aac-lc) flac wav ogg/vorbis alac mkv/mp4 containers; no opus yet in symphonia mainline) + `rubato` resampler + manual downmix; or `ffmpeg-next` / ffmpeg CLI piping `-vn -ac 1 -ar 16000 -f s16le -` (broadest codec coverage incl. opus, wma, AC3, video soundtracks). ffmpeg is the pragmatic choice (license: ensure LGPL build, or ship user-installed ffmpeg). Streaming memory behavior can be kept: read pipe in 64 KB chunks into a ring buffer. |
| MediaExtractors.swift:101-138 | `AVAssetImageGenerator.images(for:)`, `appliesPreferredTrackTransform`, `maximumSize`, tolerance, `CGContext` gray 8x8 hash | Seek-sample video frames + rotate by display matrix + dedup | ffmpeg: either N fast-seek calls (`-ss T -i f -frames:v 1 -vf scale=...`), or one pass `-vf "fps=1/step,scale='min(1024,iw)':-2"` (+`-skip_frame nokey` for long files) to rawvideo/mjpeg pipe; ffmpeg auto-rotates by display matrix. `ffmpeg-next` gives frame-level control. Pure-Rust video decode (H.264/HEVC) does not exist at production quality; accept ffmpeg/gstreamer (`gstreamer-rs`). Hash: compute from decoded RGB downscaled with `image::imageops::resize(Triangle)` to 8x8 gray; replace `>mean` rule with float mean (not needed to be bit-identical). |
| MediaExtractors.swift:162; Estimator.swift:299-311 | `MDItemCreate/MDItemCopyAttribute(kMDItemNumberOfPages, kMDItemDurationSeconds)`, `CGPDFDocument` | Fast page count / duration via Spotlight without opening files | No Spotlight. Page count: `lopdf` (parses xref only; fast) or pdfium open. Duration: `symphonia` probe (format reader `time_base`/`n_frames`), `lofty`/`mp4`/`matroska` crates for tag-level duration, or `ffprobe -show_entries format=duration`. Estimation for a full folder will now cost an open per file; parallelize (rayon) and cache keyed by (path,size,mtime). |
| Rules.swift:2; Crawler.swift:2 | `UniformTypeIdentifiers.UTType` conformance | file type classification | See 3.3 (own tables; magic-byte sniff with `infer`/`file-format`). Beware `.ts`/`.mts` ambiguity handled by TS sniff; also `.m4a` vs video `.mp4` audio-only, `.ogg`/`.webm` audio vs video, `.mov` audio-only (UTType says movie; Swift would extract frames=[] and audio only). |
| FolderSelection.swift:104 | `realpath` | /var vs /private/var | `std::fs::canonicalize` (Windows returns `\\?\` UNC prefix; use `dunce::canonicalize`). Also on Windows normalise separators and compare case-insensitively (`FolderSelection::is_inside` is byte-prefix, case-sensitive; APFS default case-insensitive too, so Swift already has this latent issue). |
| IndexService.swift:151; Indexer.swift:57 | `kill(pid,0)`, `getpid`, `$TMPDIR/DigUp-<pid>` | stale scratch cleanup | Unix `libc::kill`/`nix::sys::signal::kill(…,None)`; Windows `OpenProcess` check or `sysinfo` crate. Use `tempfile` crate dirs + a pid file, or hold a file lock (`fs2`/`fd-lock`) instead of pid probing. |
| IndexService.swift:81,251-269; Thread.detachNewThread, DispatchSemaphore, NSLock | threads and semaphore | command reader + wake | `std::thread` + `crossbeam-channel`/`std::sync::mpsc`/`tokio::sync::Notify`; `parking_lot::{Mutex,Condvar}`. |
| FolderWatcher.swift:8-30 | FSEvents (`FSEventStreamCreate`, FileEvents, NoDefer, latency) | Change notifications | `notify` crate (inotify on Linux - watch limits `fs.inotify.max_user_watches`, must add watch per directory, no auto-recursive overflow handling; ReadDirectoryChangesW on Windows; FSEvents on macOS) with `notify-debouncer-full`. Add periodic full re-sync fallback because Linux/Windows can drop events (FSEvents is more reliable for large trees). |
| PowerPolicy.swift:2,23,54-70 | IOKit `IOPS*` (AC vs battery), `ProcessInfo.isLowPowerModeEnabled`, `thermalState` | Backfill gating | `battery` or `starship-battery` crate (AC state, Linux/Win/mac); Windows: `GetSystemPowerStatus` and battery saver flag (`SYSTEM_POWER_STATUS.SystemStatusFlag`); Linux: `/sys/class/power_supply/*/online`, power-profiles-daemon (`powerprofilesctl get` == power-saver) via zbus. Thermal: Linux `/sys/class/thermal`, `/sys/class/hwmon`; Windows no stable API (WMI `MSAcpi_ThermalZoneTemperature` unreliable); `sysinfo::Components`. Treat thermal as best-effort/optional. No notification APIs on Linux; poll every 30-60 s. |
| App (not required here) | `NSWorkspace` (open file, icons, app lookup, reveal in Finder) | opening results, icons | `opener`/`open` crate, `showfile`; icons: `freedesktop-icons`, Windows `SHGetFileInfo`. Only app UI, not pipeline. |
| Vectors.swift | `Accelerate` vImage f16->f32 | vector decode for search | `half` crate (+ `simsimd`/ndarray/`faer`), portable; out of scope. |
| Worker/llama | llama.cpp via embedded runtime or JSON-lines worker | embeddings | out of scope; keep the `Embedder` trait. |

---------------------------------------------------------------------------------------------------
## 5. Performance observations (where Rust should do better)

Serial / unparallelised:
1. **Crawl is single-threaded** (Crawler.swift:114-118) with several syscalls per file: `resourceValues` (stat + UTType lookup), `lstat` again (:80), possibly `getxattr` per image (Rules.swift:140), `open+read 600 B` per .ts file (Rules.swift:130). Rust: `ignore::WalkParallel`, one `symlink_metadata` per file, no UTType DB. Use extension-first classification; only sniff when ambiguous.
2. **`isProjectRoot` does a full `contentsOfDirectory` of every visited directory** (Crawler.swift:63; Rules.swift:155) on top of the walker's own readdir. In Rust detect project markers from the same readdir listing in the walker (set of names in the directory) - zero extra IO.
3. **Post-walk passes hold everything in memory** (`found` -> `kept` -> dataset pass, Crawler.swift:131-153), single-threaded; fine but `isInProject` is recursive+memoized; the dataset pass builds `String` keys with NUL separators and re-extracts stems (`NSString` bridging) per file. Use `(PathBuf, FileKind)` keys and borrowed stems.
4. **Prepare stage is ONE serial thread, one job ahead** (Indexer.swift:345, 229). Within a batch of 8 images, `ImageExtractor.prepare` + OCR run strictly sequentially (:368-383). Rust: rayon `par_iter` over the batch (decode + resize + OCR are independent), and a bounded channel (depth 2-4 jobs) between N prepare workers and the single embed thread. Keep embed single-threaded (model constraint) but prefetch deeper.
5. **Redundant decode for screenshots**: `ImageExtractor.prepare` decodes a thumbnail (:371) and then `fullImage` decodes the full image again for OCR (:374, Extractors.swift:59-62). Decode once at full res and derive the 1536 px PNG from it (screenshots are small, typically < 10 MP), or OCR from the same buffer.
6. **Scratch files for every model input**: images re-encoded to JPEG/PNG on disk (Extractors.swift:52-53, 145-146), WAVs written to disk (MediaExtractors.swift:55-56), then read back by the worker. For an in-process embedder pass raw bytes/tensors; PNG-encoding rendered PDF pages (1400 px, Extractors.swift:146) is expensive for no benefit.
7. **Video then audio are sequential and each blocks the thread** (`runBlocking`, Indexer.swift:409-415): frames are extracted fully, then the soundtrack is decoded separately (decodes the container twice). Run both concurrently (or a single demux pass with ffmpeg producing both streams).
8. **Audio buffering is O(n) per window**: `buffer.removeFirst(hopSamples)` (MediaExtractors.swift:70) shifts the remaining Int16s each hop, and `buffer.append(contentsOf:)` copies each decoded chunk; chunk is allocated per sample buffer (:63). Use a `VecDeque<i16>` or ring buffer / index offset and drain lazily. Also `rms` converts every sample to Double with a division (:83); compute on i32 sums, `sum_sq as f64 / 32768^2`, SIMD-friendly.
9. **Unbounded scratch/disk and embed batches for media**: a 3-hour file produces ~432 WAV windows x 960 KB = ~415 MB of scratch held at once (Indexer.swift:403-406 returns all pieces, then `embedAudio(all)` at :649). Video: up to 1200 JPEG frames plus all audio pieces held and embedded in one call (:667-668). Stream in fixed-size batches (e.g. 16-32 windows/frames), embedding as produced, committing per batch (segments appended; mark partial like long docs).
10. **Long docs re-read the whole file every step**: each `finishLongFiles` step calls `DocExtractor.text(of:)` + `chunks(text, limit: maxChunks+1)` (Indexer.swift:396-397) even to read 40 chunks, i.e. O(file_size x steps). For a 5,000-chunk doc that is 125 steps x full parse + chunk. Cache chunk boundaries (store byte offsets in `info`) or keep a per-file chunk cursor; for plain text seek by offset. Same for PDFs: `PDFDocument(url:)` is reopened for each 30-page step (Extractors.swift:114) - keep it open across steps or accept pdfium cheap reopen.
11. **Chunker is O(n) Character indexing with repeated `Substring` + trim and `lastIndex(where:)` scans** and uses Character (grapheme) iteration (Extractors.swift:229-243); the two whole-text regex replaces allocate full copies (:223-226). In Rust operate on `&str` byte indices with `char_indices`, or on bytes ASCII-fast-path; pre-size output; use `memchr`/`rfind` for the last whitespace.
12. **`hasWords` iterates `CharacterSet.contains` per scalar twice** (SearchText.swift:39-41; whitespace + letters). Rust: single pass with a fast ASCII path.
13. **Estimator does per-file Spotlight/PDF/AV opens serially** (Estimator.swift:240,260,265); with no Spotlight in Rust, parallelize (rayon) and cache; or estimate coarsely from file size first and refine lazily.
14. **Per-file transactions**: `sync` is one transaction (good); but `run` commits one transaction per job (one PDF, one doc step, 8 images). Fine, but each segment insert is its own prepared statement with a fresh `db.run` call (IndexStore.swift:374-381) - reuse prepared statements (`rusqlite::Connection::prepare_cached`) and batch.
15. **`store.files(under:)` loads every row under every root into two dictionaries** (Indexer.swift:88-94) - OK up to millions; `byInode` uses a formatted String key `"\(device):\(inode)"` (:93, 108): use `(u64,u64)` tuple keys.
16. **Mid-batch stop latency**: `shouldStop` only between jobs; a single huge video can hold the loop for minutes (a 2-hour video = ~2,400 sampled frames capped 1200 + 288 audio windows before any commit). With streaming per-batch commits (item 9) pause/resume would be responsive.
17. **Git ignore via a `git ls-files` subprocess per repo, whole list into a `Set<String>`** (Crawler.swift:227-247); no `.gitignore` evaluation without git installed (everything counts). Rust: `ignore` crate evaluates .gitignore/.git/info/exclude/global excludes natively and in the walker (no subprocess, works without git, and untracked-but-not-ignored semantics match `--cached --others --exclude-standard` approximately; "tracked but now ignored" files are the one difference, which ls-files --cached keeps).
18. **OCR serial with embedding only through the 1-job lookahead**; Vision cost 0.24 s/screenshot dominates (Estimator.swift:175). With 8-image batches OCR could run on N cores.
19. Misc correctness/robustness issues worth fixing in a port: (a) HTML entity decoding via an unordered dictionary (Extractors.swift:195-197) can double-decode `&amp;lt;`; (b) `isSecret` not applied to the main index (2.1); (c) `maxPixel` etc. not in the fingerprint (2.3); (d) `ImageExtractor.prepare` uses `max(w,h) >= minSide` although the name suggests min side (Extractors.swift:43); (e) full documents are read into memory without a byte cap (no `maxDocBytes`; `maxChunks` only truncates after reading/regexing the entire file; a multi-GB .txt/.log-like file named .txt would be loaded) - add a size guard/streaming reader; (f) `listing hidden` filter relies on Foundation behavior; (g) mtime comparison uses `abs(...) > 0.001` for change but `< 0.001` for move, so a delta exactly 0.001 is neither (harmless).

---------------------------------------------------------------------------------------------------
## 6. Test cases worth porting

RulesTests.swift (Tests/DigUpKitTests):
- `typeScriptIsNotVideo` (:12): `app.ts` with TS source -> Skip("code or data file"). `realTransportStreamIsVideo` (:18): 4 packets, 0x47 at offsets 0,188,376,564, rest 0xFF -> Index(video). Add a 192-byte M2TS variant (0x47 at 4,196,388) and a <400-byte file.
- `codeCanBeIncluded` (:26): includeCode => `main.py` -> Index(doc).
- `machineNames` (:33): clip_000123, frame-0042, 3f9a1c0e-12ab-4cde, 000123 true; IMG_1234, PXL_20260101_123456789, "Screenshot 2026-10-01 at 9.41.07 PM", "Tokyo itinerary" false.
- `datasetNeedsManyMachineNamedFiles` (:44): 400 audio `clip_%06d` in /x/clips -> dataset (400); 400 `IMG_n` images not; 50 clips not (< 300). Add 0.7 ratio boundary cases (209/300 false, 210/300 true) and "root is never a dataset".
- `chunksOverlapAndRespectWords` (:58): 600 "wordN" words, size 500, overlap 100 -> >5 chunks, all <= 500, all start with "word", last ends "word600".
- `htmlBecomesPlainText` (:67): exact expected output "Trip & plans\nFly to Tokyo on Friday.\nCafé — ramen" (script/style/head/comment dropped, &amp; &nbsp; &#233; &#x2014;). If you swap parsers, keep this as the contract.
- `nestedRootsAreCrawledOnce` (:134): roots [folder, folder/inner] -> trip.txt once.
- (Not pipeline but pure and portable: halfVectors f16 round-trip :76; meaning/z-score tests :87,:102; isLookup :114; strongCount :124; ftsQuery :52.)

FolderSelectionTests.swift: `uncheckingASubfolderSkipsIt` (:12), `checkingASubfolderChoosesItAlone` (:24), `uncheckingAFolderDropsItsSubfolderChoicesButNotDeeperSkips` (:39), `aChosenFolderInsideASkippedOneIsSearchedWhole` (:47), `foldersInsideAnotherChosenOneAreCovered` (:58), `keptFoldersStayWhateverIsClicked` (:64), `checkingASkippedFolderTakesTheSkipBack` (:74), `pathsAreComparedByWholeNames` (:84: `/a/bc` not inside `/a/b`), `crawlsLeaveSkipsOutButWalkChosenFoldersInsideThem` (:90: roots [D, D/Media/Courses], skips [D/Media, D/Media/Courses/Old] -> only top.txt and week1.txt), `estimatesComeBySubfolder` (:106; per-subfolder sums equal whole), `subfoldersAreNamedAsTheFolderWasGiven` (:121; /var vs /private/var - port as a symlinked-root test), `filesUnderNestedFoldersComeOnce` (:136: store.files_under(["/D","/D/Media/Courses"]) returns each row once).

LongFilesTests.swift (the best integration tests; need a FakeEmbedder that returns deterministic vectors and counts calls; PDF fixtures with N pages of text and a special word on one page):
- `longPDFIsReadInSteps` (:55): 120-page PDF -> after `run`: 1 partial, pages 1..30 stored, read_to=30, unfinishedCount=(1,90), keyword on page 100 not found; `finishLongFiles` steps `done` = [30,60,90], total [90,90,90], states [partial,partial,done], files [1,1,0]; all 120 pages; "quokka" found at page 100; info read_to/read_end gone, pages=120; calling again returns empty; embedder called exactly 120 times (every page once).
- `stoppingBetweenStepsLosesNothing` (:84): stop after 1 step -> read_to=60, (1,40) left; resume completes 1..100 and done.
- `aChangedFileStartsOver` (:99): replaced PDF -> sync.changed==1, state pending, no segments, rerun reads 1..90.
- `longDocumentGoesOnWhereItStopped` (:114): 150,000-char prose with "axolotl" at 120,000; first run stores chunks 1..40, read_end == chunk count, keyword absent; after finishLongFiles chunks 1..N, stored text sequence == `chunks()` output exactly, keyword found only at chunk > 40, state done.
- `digitsGetKeywordSearchOnly` (:142): numeric dump -> hasWords false; zero embed calls and zero vectors, but exact keyword "4815162342" found once; also hasWords true for "Revenue 1,234,567 ..." and Bengali.
- `machineMadeGiantsStopAtTheCap` (:165): maxPages=45 on 120-page PDF -> pages 1..45, done, detail "the first 45 of 120 pages", unfinished (1,15) after first run.
- `oldIndexesFinishWhatTheyCut` (:182): schema migration test (SQLite-specific; port only if you keep a compatible DB; otherwise skip).
- `estimatesCountTheRest` (:221): 120-page + 3-page PDFs -> pdfPages 33, total 123, longFiles 1, seconds = 33*pdfPage, later = 90*pdfPage.
- `float16VectorsScoreAcrossBlocks` (:236): storage test, search-side.
- Additional tests to ADD (gaps in the Swift suite): audio window math (exact boundaries: 59.9 s, 30 s, 31 s, 2 s tail rule, silent window dropped, offsets); video dedup (identical frames collapse; end times chain); sync move detection (rename keeps vectors; copy is added; rename + mtime change is changed+added); vanished-root keeps rows; fingerprint change resets; `hasWords` boundary 20%; PageTextProblem fixtures (Bengali glyph-order, Arabic "ا?ردن" x2); `classifyCode` secrets/minified/license/generated-header cases; project-root skip with screenshots exceptions; cloud-placeholder skip (Windows OneDrive attr).

---------------------------------------------------------------------------------------------------
## 7. Unsure / not verified
- Exact set of image/audio/video/pdf extensions: determined at runtime by macOS UTType, not in repo; table must be authored (see 3.3).
- Whether `.skipsHiddenFiles` also honors UF_HIDDEN (likely yes) and whether `isPackage` hits `.rtfd`, `.photoslibrary`, `.pages` etc. (it does for any directory UTType conforming to package).
- Swift's `String.count` / indices are grapheme clusters; porting chunking to byte/char indices changes boundaries slightly (all counts: 1800/200/240/4000/4000 are in Characters).
- The Python/mlx and llama.cpp worker protocols and the Searcher were not read; embedding batch size limits inside `embedImages/embedTexts` are unknown (the pipeline passes up to ~50 PDF pages, 40 chunks, 8 images, all audio windows of a file, all frames of a video in one call).
- "Costs ... will calibrate on first launch" (Estimator.swift:171-172): no calibration code found in the files I read.
- Pause semantic beyond `setPaused`->`stop` (Engine.swift:697-702) was only skimmed.
