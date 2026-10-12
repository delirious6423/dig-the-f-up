# DigUp storage and search: Rust porting spec

Source: /root/dtfu-src (NOT /home/claude/dtfu-src; that path does not exist). All paths below are relative to it.
Abbreviations: DB = Sources/DigUpKit/Database.swift, IS = IndexStore.swift, V = Vectors.swift, S = Searcher.swift,
ST = SearchText.swift, CE = CodeExtractor.swift, M = Model.swift, R = Rules.swift, CR = Crawler.swift,
IX = Indexer.swift, MAIN = Sources/digup/main.swift.

Honesty note: everything in sections 1-5 is read from source. Section 6 numbers marked (est.) are my estimates, not measurements,
except those the Swift comments themselves cite (marked "per author").

---------------------------------------------------------------------------------------------------

## 1. SQLite schema

### 1.1 Files on disk
- Two separate SQLite files in the index directory: `index.sqlite` (main) and `code.sqlite` (code index). IS:49, IS:52.
  Same schema, same code (`IndexStore(directory:file:)`, IS:56). The "code index" is chosen by file name:
  `Searcher.isCode = store.file.lastPathComponent == "code.sqlite"` (S:49). Code search is only triggered by the query prefix
  `code:` (case-insensitive, leading whitespace allowed; M:104-112), which is stripped before search.
- Rust: keep two DB files with identical schema; the code-vs-main switch changes constants (word weight, query prompt, identifier lookup).

### 1.2 Connection pragmas (DB:21-22)
```
sqlite3_busy_timeout(5000)
PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;
```
Opened `READWRITE | CREATE`. Single connection, single thread. During migration busy_timeout is raised to 60000 then restored (IS:82-83).
Writes use `BEGIN IMMEDIATE` ... `COMMIT` / `ROLLBACK` (DB:46-56). No mmap/cache_size/page_size pragmas are set.

### 1.3 Exact DDL (IS:94-125, schema version 5, `PRAGMA user_version = 5`, IS:23)
```sql
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE files(
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime REAL NOT NULL,
    inode INTEGER NOT NULL,
    device INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',   -- pending | done | partial | skipped | failed
    detail TEXT,                              -- why skipped/failed, or how far a long file has been read
    info TEXT,                                -- JSON: pixels, pages, duration, date taken; read_to/read_end
    indexed_at REAL
);
CREATE INDEX files_inode ON files(device, inode);
CREATE INDEX files_state ON files(state, kind);
CREATE TABLE segments(
    id INTEGER PRIMARY KEY,
    file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,       -- SegmentKind
    modality TEXT NOT NULL,   -- Modality of the vector ('none' when there is no vector)
    loc REAL,                 -- PDF page number, or start second for audio/video
    loc_end REAL,
    excerpt TEXT,
    vec BLOB                  -- 768 x float16, L2-normalized
);
CREATE INDEX segments_file ON segments(file);
CREATE VIRTUAL TABLE fts USING fts5(
    name, body, original UNINDEXED,
    tokenize = "porter unicode61 remove_diacritics 2 categories 'L* N* Co M*'"
);
PRAGMA user_version = 5;
```
Notes:
- FTS5 table `fts` is a normal (content-owning, not external-content) table. `fts.rowid == segments.id` by convention (set explicitly on
  insert, IS:379, IS:392). Nothing enforces it in SQL, and there is no trigger; Swift code deletes fts rows manually before segment rows
  (IS:272, IS:399-400). The `ON DELETE CASCADE` only cleans `segments`, so `delete(file)` first deletes fts rows (IS:272-273).
- Tokenizer: `porter unicode61 remove_diacritics 2 categories 'L* N* Co M*'` (IS:27). The `categories` option makes combining marks (M*)
  and private-use (Co) token characters; default unicode61 would split Bengali vowel signs / Arabic harakat out of words (comment IS:25-26).
  Needs SQLite >= 3.31 (unicode61 categories option; I recall 3.31 added it; verify against the bundled SQLite in rusqlite).
- Columns: `name` (file name, boosted 3x in bm25), `body` (normalized text), `original` (UNINDEXED; raw text only when normalization changed it,
  else NULL; IS:385-388). Name rows: `name`=normalized file name, `body`=normalized folder path relative to $HOME with `/`->space (IS:390-414).
  Content rows: `name=''`, body/original as above.
- No FTS5 `prefix=` index option is declared; trailing `*` queries run as prefix scans on the main index.

### 1.4 segments semantics
- `kind` values (M:17-28): `name`, `image`, `ocr`, `page`, `page_image`, `chunk`, `audio`, `frame`, `lines`, `cells`.
  Every file gets one `name` segment at insert (IS:391) with `modality='none'`, no vector.
- `modality` values: `none`, `text`, `image`, `audio` (M:32-34).
- `loc`/`loc_end` REAL: PDF page (1-based), audio/video start/end seconds, code first/last line (1-based), notebook first/last cell (1-based), doc chunk index (1-based).
- A segment with text but no vector (e.g. OCR text, number dumps) has `vec NULL`, `modality='none'` (IS:376: modality forced to none when vector nil).
- `files.kind` values: screenshot, image, pdf, doc, audio, video, code (M:4-7). `files.info` is a JSON object with *string* values
  (parsed as `[String:String]`, IS:348, S:208), keys seen: pages, chars, chunks, read_to, read_end, lines, cells, repo, width, height, taken, duration.
  `files.inode/device` store `UInt64` via bit-pattern cast to Int64 (IS:238). Move detection uses (device, inode, size, mtime) (IS:261-269).
- `files.path` has UNIQUE index; folder-prefix queries use `path >= prefix AND path < prefix||U+10FFFF` (IS:41, IS:227-228).

### 1.5 Vector storage format
- Column `segments.vec` BLOB: exactly 768 x IEEE-754 binary16 (float16), little-endian (native Apple silicon/x86), row-major, = 1536 bytes.
  V:4, V:8-11 (`half()` maps `Float16($0)` over the f32 vector, round-to-nearest-even), IS:120 DDL comment.
- Dimension constant 768 (V:6). A blob with `count != 1536` is silently skipped when loading (S:74).
- Normalization: vectors are L2-normalized by the embedder before storage ("mean-pooled, L2-normalized", LlamaEmbedder.swift:6,283).
  DB doesn't re-normalize; scoring is a plain dot product = cosine (S:283-284). Port must L2-normalize query vector too (embedder does it).
- Model: EmbeddingGemma 2 via llama.cpp (GGUF text q8_0 + mmproj q8_0): `modelInfo = {model:"google/embeddinggemma-2", dtype:"text-q8_0+mmproj-q8_0", dim:768, runtime:"llama.cpp-vectors1"}` (LlamaEmbedder.swift:28-31).
  Multimodal: text, image, audio, video frames all embed into the same 768-d space.
- Prompts (M:88-100): query `"task: search result | query: {text}"`; code query `"task: code retrieval | query: {text}"`;
  document `"title: {title or 'none'} | text: {text}"`. Code docs use the repo-relative path as title (IX:~617, CE:15-17).

### 1.6 Model/version tagging (meta table)
- `meta('vector_source')` = `"{model}@{revision[0..7]} {runtime} {dtype}"` e.g. `google/embeddinggemma-2@bfcd298 llama.cpp-vectors1 text-q8_0+mmproj-q8_0` (Worker.swift:25; example in SearchTextTests:61,97).
- `meta('fingerprint')` = `"{vector_source} | {IndexOptions.fingerprint}"` (IX:281). Main fingerprint: `photo140 shot280 page280 frame70/3.0s audio30.0/25.0s` (M:84);
  code fingerprint: `code1 lines1800` (M:83).
- On mismatch: `Indexer.adopt` either calls `resetAll()` (deletes every non-`name` segment + their fts rows, sets all files pending, deletes fingerprint meta; IS:293-300) or throws `fingerprintMismatch` (IX:270-276).
- CLI search refuses to run if `searcher.vectorSource != worker.info.vectorSource` (MAIN:327-329). Port should do the same: vectors from a different model/quantization must not mix.
  For Rust parity of existing indexes, the embedder must reproduce llama.cpp Q8_0 EmbeddingGemma outputs closely; otherwise define a new `runtime` tag and re-embed.

### 1.7 Migrations (IS:71-192) via `PRAGMA user_version`
Single `BEGIN IMMEDIATE` transaction; refuses newer schema (IS:76-78). Steps:
- v0 -> create() (current DDL, v5).
- v1 -> `migrateKeywordSearch`: rebuild fts as `fts_new` with new tokenizer + normalized text; queue garbled PDFs again; set v2.
- v1-2 -> `migrateScreenshotText`: `UPDATE segments SET vec=NULL, modality='none' WHERE kind='ocr'` (v3).
- v1-3 -> `migrateVectorNames`: rename `llama.cpp-b11461-metal` -> `llama.cpp-vectors1` in meta (v4).
- v1-4 -> `migrateLongFiles`: drop vectors of digit-heavy text (`SearchText.hasWords`), turn old truncated PDFs/docs into `partial` with `read_to`/`read_end` in `info` via `json_set` (v5).
A Rust port can start at schema 5 and only need `create()`; implement version check. Keep `user_version=5` for file compatibility with Swift-made indexes.
Uses SQLite JSON1 functions (`json_extract`, `json_set`) in queries (S:71, IS:336), so the SQLite build needs JSON1 (default on in modern SQLite).

### 1.8 Misc store queries worth reproducing
- pending: `SELECT ... FROM files WHERE state='pending' [scope] ORDER BY CASE kind WHEN 'screenshot' THEN 0 WHEN 'image' THEN 1 WHEN 'pdf' THEN 2 WHEN 'doc' THEN 3 WHEN 'audio' THEN 4 WHEN 'video' THEN 5 ELSE 6 END, mtime DESC LIMIT ?` (IS:307-311, M:10-13).
- counts: `SELECT COUNT(*), COUNT(vec) FROM segments WHERE kind != 'name'` (IS:434).
- Segment insert: `INSERT INTO segments(file,kind,modality,loc,loc_end,excerpt,vec) VALUES(...)`, then `INSERT INTO fts(rowid,name,body,original) VALUES(last_insert_rowid, '', ?, ?)` only if text non-empty (IS:374-381).
- Per-file index writes are one transaction per file/step (e.g. code: IX:629-639).

---------------------------------------------------------------------------------------------------

## 2. Search algorithm (Searcher.search, S:116-238)

Signature: `search(text, queryVector?, kinds?, limit=20, zGate=2.0, wordWeight?, prefixLast=false)` (S:116-117). CLI/eval pass `limit` = `-n` (default 10), `zGate` from `--zgate` default 2.0 (MAIN:129, 346).

### 2.1 Constants
| constant | value | where |
|---|---|---|
| `Searcher.wordWeight` (main) | 2.0 | S:113 |
| `Searcher.codeWordWeight` (code index) | 1.0 | S:114 |
| `zGate` default | 2.0 | S:117, MAIN:129 |
| lookup mode (`isLookup` or code identifier): meaningWeight / wordsWeight | 0.5 / wordWeight*2 | S:170 |
| normal mode: meaningWeight / wordsWeight | 1 / wordWeight | S:170 |
| word coverage exponent | 1.5 (`pow(coverage, 1.5)`) | S:176 |
| coverage qualification threshold | share >= 0.5 | S:173 |
| kind-hint boost | +1.0 | S:177 |
| FTS fetch limit | 1000 rows; max 100 distinct files kept | S:146, S:149 |
| bm25 column weights | name 3.0, body 1.0 | S:146 |
| snippet() | col 1 (body), marks `«` `»`, ellipsis `…`, 12 tokens | S:144 |
| z std floor | 0.02 | S:329 |
| small-modality group threshold | n < 20 uses global mean | S:343 |
| strongCount ratio (UI fold) | 0.5 x top score | S:278 |
| excerpt limit for snippets | 400 chars | S:222,229 |
| definition lookup only for first 10 results (code) | rank < 10 | S:197 |
| code definition scan limit | LIMIT 300 stretches | S:250 |
Not RRF: the author compared and chose this linear fusion (S:166-168: "R@1 0.88 vs 0.73 synthetic, 0.86 vs 0.82 real").

### 2.2 Step by step
0. If a query vector is supplied and vectors aren't loaded, `reload()` (S:119).
1. `fileKind` map (file id -> FileKind) held in memory; `allowed(file)` implements the `kinds` filter (S:120-121). The filter is applied *after* scoring
   (rows of disallowed kinds still contribute to the z-score scale) for vectors (S:128) and in the FTS loop (S:149). `kinds` is a hard filter; the kind *hints* below are a soft boost.
2. Vector scoring (S:123-132), only if `queryVector.count == 768` and there are rows:
   a. `scores = score(query)`: brute-force dot product of the query against *every* vector (exact, no ANN, no quantization beyond fp16 storage), S:285-304.
   b. `meaning = Searcher.meaning(scores, rowModality, setsScale: rowSetsScale)` (S:316-331):
      - per-modality mean: `modalityMeans` (S:334-350): mean of scores among rows with `setsScale==true` per modality; modalities with < 20 such rows use the global mean of all setsScale rows; missing modality -> global mean.
      - `centered[i] = score[i] - modalityMean[modality[i]]`.
      - over setsScale rows only: `mean = avg(centered)`, `deviation = max(sqrt(avg((centered-mean)^2)), 0.02)` (population std, one deviation for all modalities together).
      - `z[i] = (centered[i] - mean) / deviation` for ALL rows (including non-setsScale).
      - `rowSetsScale[i]` is false for "later pass" segments of long files: PDF pages > pdfStep(30) when pdf pages > bigPDFPages(50), or doc chunks > docStep(40) (S:80-85; IndexOptions M:54-56). They are scored but don't affect mean/std.
   c. For each row whose file is `allowed`: keep per-file best (max z): `best[file] = (z, cosine, row)` (S:128-131). So grouping/dedup per file happens here (one best segment per file).
3. Keyword search (S:134-163):
   a. `terms = Searcher.terms(text)` (S:375-380): `SearchText.normalized(text).lowercased()` split on non-alphanumerics (`CharacterSet.alphanumerics.inverted`), keep tokens with >= 2 unicode scalars, not in `stopwords` (S:360-370, English + Bengali + Arabic lists incl. kind words like screenshot/photo/pdf/video), de-duplicated preserving order.
   b. `identifier = isCode ? CodeLookup.identifier(text) : nil` (S:137; CE:256-264). Query that looks like snake_case/camelCase/dotted/with digits -> lower-cased word list; searched as one FTS phrase `"a b c"` (CE:267).
   c. `patterns`: if identifier: `[phrase]`; else per term `"\"term\""` plus `*` suffix if (prefixLast and last term) or `SearchText.inflects(term)` (contains Indic 0x0900-0x0DFF or Arabic 0x0600-0x06FF scalar) (S:384-389).
   d. Main FTS query (S:143-147):
      ```sql
      SELECT s.file, s.id, snippet(fts, 1, '«', '»', '…', 12), fts.original FROM fts
      JOIN segments s ON s.id = fts.rowid
      WHERE fts MATCH ?  ORDER BY bm25(fts, 3.0, 1.0) LIMIT 1000
      ```
      MATCH string = patterns joined with ` OR `. Rows iterate in bm25 order; first row per file wins (`seen` set), stop at 100 files, skip disallowed kinds (S:149). `words[]` order = keyword rank (1-based `keywordRank`).
      Snippet: if `original` non-null use `SearchText.snippet(original, terms)` (Swift implementation to emulate normalized matching on raw text) else the FTS snippet (S:151).
   e. Coverage (S:154-162): for each pattern, `SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid WHERE fts MATCH ? AND s.file IN (<ids of words>)`; `coverage[file] += 1/patterns.count`. I.e. fraction of query terms found *anywhere in the file* (across different segments, incl. name/folder rows).
4. Candidate set (S:172-173): files with best z >= zGate, plus files with coverage >= 0.5.
5. Score (S:169-179):
   ```
   lookup = isLookup(terms) || identifier != nil
   (mw, ww) = lookup ? (0.5, wordWeight*2) : (1.0, wordWeight)
   score = mw * max(best.z, 0) + ww * coverage^1.5 (+ 1.0 if file.kind in kindHints)
   ```
   `isLookup(terms)` (S:269-273): non-empty and every term contains a digit and (contains a letter or has >= 4 chars) - codes/IDs/long numbers.
   `kindHints(text)` (S:412-419, table S:398-408): query words (normalized, lowercased, split on non-alphanumeric) mapped to FileKind via `kindWords` (English, Bengali, Arabic); Indic/Arabic kind words with >= 3 scalars also match as word prefix.
6. Ranking: sort by score desc, ties by file id asc; take `limit` (S:184-185). No pagination beyond `limit`.
7. Segment resolution per hit (S:186-238):
   - `useMeaning = best exists && (best.z >= zGate || no keyword hit)`. Pointed segment = best-meaning segment if `useMeaning` else the keyword-matched segment. If neither -> skip hit (S:190-191).
   - Page/moment: the segment row's `kind`, `loc`, `loc_end`, `excerpt` are loaded (S:212-217): `loc` is PDF page or start-seconds (audio/video frame/window), so "page/moment resolution" = whichever segment was picked. There is no windowing/merging of adjacent segments at search time.
   - Snippet: only from the keyword hit if its segment is the chosen segment and contains `«`; else fall back to stored `excerpt` (S:221-235). Special-case: a segment with no loc and no excerpt (screenshot image vector) borrows OCR snippet from the matched segment unless that is a `name` segment.
   - Output `SearchHit{path, kind, score, z, cosine, keywordRank, segment, segmentID, loc, locEnd, excerpt, modified, size, info, focus}` (S:4-25).
8. UI fold: `strongCount(hits, 0.5)` = number of leading hits scoring >= 0.5 x top (S:278-281). Not part of retrieval; used by eval stats (MAIN:48).

### 2.3 Code search (separate index; S:136-139, S:193-199, S:243-263)
- Same algorithm on `code.sqlite` with: `wordWeight=1.0` (S:114; rationale S:109-112), `identifier` phrase search, query prompt `task: code retrieval | query:`.
- After picking a segment per hit, `codePlace()` (S:243-263): if identifier, (first 10 hits only) scan up to 300 stretches of that file that FTS-match the phrase (ORDER BY loc), run `CodeLookup.definition` (CE:275-287, `definitionStrength` CE:304-315) to find the line that *defines* the name (strength 2 = def/func/class/#define/ObjC/Go-method/C-style definition; 1 = declaration ending with `;`); switch segment and set `focus` line.
  Else if words matched here, `focus = firstLine(with: terms, in: segmentText, first: loc)` (CE:320-333).
- Segments are `lines` (loc..loc_end = first/last line) or `cells` (notebook cells). Segment text stored in fts.body (normalized) / original.

### 2.4 Filters
- `kinds: Set<FileKind>?` (hard, post-score; see 2.2.1). `FileKind` list: screenshot, image, pdf, doc, audio, video, code. Eval JSON has an optional `kind` field mapped to this (MAIN:437,45) - none of the three shipped evals use it.
- `IndexStore.scope` (folders currently connected) filters only pending/unfinished/counts, NOT search (IS:32-46). Search has no path/date/size filter.

### 2.5 Pseudocode for the Rust port
```rust
let (z, cos, row) = if let Some(q) { scan(q) } ...   // z over all rows; best-per-file where kind allowed
let terms = terms(text); let ident = if is_code { identifier(text) } else { None };
let patterns = ident.map(|w| vec![phrase(w)]).unwrap_or_else(|| patterns(&terms, prefix_last));
// fts query + coverage as in 2.2.3 (identical SQL)
let lookup = is_lookup(&terms) || ident.is_some();
let (mw, ww) = if lookup {(0.5, word_weight*2.0)} else {(1.0, word_weight)};
cand = {f | best[f].z >= z_gate} ∪ {f | coverage[f] >= 0.5};
score[f] = mw*max(best[f].z,0) + ww*coverage[f].powf(1.5) + (kind_hint(f) as f64);
sort by (-score, file_id); take(limit); resolve segment as in 2.2.7
```

---------------------------------------------------------------------------------------------------

## 3. Text normalization / tokenization (SearchText.swift)

Applied to every text before it enters FTS (`IndexStore.keywordText`, IS:385) and to every query (`Searcher.terms`, S:377).
`SearchText.normalized` (ST:8-25):
1. NFKC (`precomposedStringWithCompatibilityMapping`): Arabic presentation forms/ligatures -> plain letters, ﬁ -> fi, full-width -> ASCII, Bengali two spellings of "ো" unify.
2. Per scalar after NFKC:
   - U+200B (ZWSP) -> space ("a word break (Thai, Khmer)", ST:13).
   - Dropped: U+00AD, U+061C, U+200C-200F, U+202A-202E, U+2060-2064, U+2066-2069, U+FEFF.
   - Dropped (Arabic): U+0640 tatweel, U+064B-065F harakat, U+0670 dagger alef.
   - U+0622, 0623, 0625, 0671 -> U+0627 (alef).
   - everything else unchanged.
3. Case folding is done by the FTS5 tokenizer (unicode61 lowercases) and by `.lowercased()` for query terms. Diacritics: `remove_diacritics 2` (unicode 6.1+ data) by the tokenizer. Porter stemming (English) via `porter` wrapper.
If `normalized(text) == text`, `original` is stored NULL; otherwise `original` = raw text (shown to user / used for snippets).

Other helpers:
- `inflects(word)`: contains scalar in 0x0900-0x0DFF (Indic) or 0x0600-0x06FF (Arabic) -> query term gets FTS `*` prefix (ST:29-31, ST:160-162). Test: `ftsQuery("কুমির") == "\"কুমির\"*"`, `"payment error" -> "\"payment\" OR \"error\""` (SearchTextTests:24-27).
- `hasWords(text)`: letters*4 >= others (non-whitespace) and letters > 0 (ST:37-43). If false, segment is stored without a vector (keyword-only). Used at index time (IX code(): IX:~620 `worded`) and migration v5.
- `matches(of:in:)` and `snippet(...)` (ST:49-105): re-find query terms in the *original* text under normalization + case + diacritic folding with an index map, only at word boundaries (or whole-word extension for inflecting scripts), up to 500 matches; builds a ~12-word window with `«»` marks. Needed so Arabic/Bengali snippets keep original spelling. Port with `unicode-normalization` (NFKC), `unicode-segmentation` (graphemes), and char mapping.
- `PageTextProblem.check` (ST:165-211): heuristics for garbled PDF text (Indic glyph order; Arabic lost lam-ligature) - indexing-side only, can defer.

### Languages without spaces (CJK, Thai, Khmer, Lao, Burmese)
- There is NO word segmentation anywhere (no ICU BreakIterator, NLTokenizer, jieba). Grep for CJK/Thai/Japanese/Chinese finds only a comment about IME composition (SearchPanel.swift:272) and the ZWSP rule (ST:13).
- Consequence (inference from FTS5 unicode61 behavior, not verified by test): CJK ideographs are category Lo (letters) so a contiguous run of CJK characters is a single token; keyword search only matches the whole run (or prefix with `*`, but `inflects()` is false for CJK so no `*`). Thai without ZWSP is one long token. Query side `terms()` has the same behavior (`alphanumerics.inverted` split).
- So non-space languages are effectively supported only through semantic (embedding) search, which is multilingual. The eval suite covers only Bengali and Arabic (+ English); there is a file named with Japanese in synthetic.json but only as a path trap.
- Rust levers if CJK keyword search is wanted: custom FTS5 tokenizer or pre-segmentation (insert spaces/ZWSP via `jieba-rs` / `lindera` / ICU `icu_segmenter` crate, or character-bigram indexing) applied in `normalized()` for both index and query. This would be a divergence from Swift parity; treat as an enhancement.

---------------------------------------------------------------------------------------------------

## 4. Code extractor (CodeExtractor.swift, Rules.swift, Crawler.swift)

### 4.1 Approach: NO tree-sitter, NO regex AST; line-based chunking heuristics
- Output is `Piece{text, first, last}` "stretches of whole lines" (CE:7-11), default `codeStretch = 1800` characters (M:43; tested at 1200 and 3600, both worse: M:41-42). Hard cap `longestPiece = 4000` chars for a single over-long line (CE:27, CE:121).
- Algorithm `lines(text, size)` (CE:77-126):
  1. Split on `\n` and `\r\n` (Swift treats `\r\n` as one Character); drop trailing empty final line. `offsets[i]` = cumulative characters (count in Characters, +1 per newline).
  2. Skip leading blank lines. Greedily extend `end` while `offsets[end+1]-offsets[start] <= size`.
  3. If the stretch doesn't reach EOF: find a natural break, scanning backwards from `end` but never below `floor = offsets[start] + size*2/5` (40% of a stretch): first preference a line that *starts a block and follows a blank line*, else any line that follows a blank line or starts a block. `startsBlock(i)`: first character is not whitespace AND the line is not a closer (starts with `}`, `)`, `]` or trimmed in {end, fi, done, esac, @end, #endif, end;, end)}) (CE:86-91, 146).
  4. Trailing blank lines trimmed from the stretch's `last`.
  5. Next start: `end` if cut was natural; else (forced cut mid-block) `max(end-2, start+1)` i.e. 2-line overlap (CE:123; test CodeTests:66-73).
  6. A single line longer than size becomes its own piece, truncated to 4000 chars.
- Test-verified behaviours: CodeTests:41-84 (one stretch for a short file; 3 functions of ~1000 chars split at blank lines; 2-line overlap; long-line cut; CRLF).
- `summary(text)` (CE:130-143): excerpt stored per stretch (<=160 chars + "…"): first line whose first 4 whitespace tokens contain a definer word (def, func, fn, class, struct, enum, ... excluding let/var/const/val), else first non-comment/import line.
- Rust: this ports directly with `str::lines`-like logic; use char counts (Swift `count` counts grapheme Characters; Rust `chars().count()` is close but differs for combining sequences; for exact parity use `unicode-segmentation` graphemes - low priority). tree-sitter crates (`tree-sitter` + `tree-sitter-rust/python/javascript/typescript/go/java/c/cpp/...`) are NOT needed for parity; they could be an optional improvement (chunk at function/class nodes), but the author measured the line heuristic against real repos and notes shorter stretches cut functions apart (M:41-42).

### 4.2 Languages and file types (Rules.swift)
- Source extensions (R:42-49): py pyi pyx c h cc cpp cxx hpp hh hxx m mm swift ts tsx mts cts js jsx mjs cjs go rs java kt kts scala groovy rb php cs fs sh bash zsh fish ps1 lua pl pm r jl dart sql vue svelte astro html htm css scss sass less gradle cmake mk proto graphql gql tf hcl nix zig hs ex exs erl clj cljs elm ml mli vim el lisp asm s metal glsl hlsl wgsl cu cuh ino v sv tex.
- Code-adjacent text (R:51): md markdown rst adoc org txt yaml yml toml. Names (R:53-56): Makefile makefile GNUmakefile Dockerfile* Containerfile Justfile justfile Rakefile Gemfile Podfile Brewfile Procfile Vagrantfile Jenkinsfile. Notebooks: `.ipynb`.
- `language(of:)` display table at CE:229-247 (display only).
- Skipped with reason: excluded type; secrets (R:88-95: ext pem key p12 pfx keystore jks ppk kdbx gpg asc crt cer der p8 mobileprovision; name prefixes id_rsa id_dsa id_ecdsa id_ed25519 .env secret credential .netrc .npmrc .pypirc htpasswd); `.min.js/.min.mjs/.min.css`; names starting license/licence/copying/notice; data files in `codeExtensions` (json, csv, lock, log, plist, xml...) -> "data file"; `.ts/.mts` that are really MPEG-TS video (0x47 sync bytes at 0/188/376 or 4/196/388 in first 600 bytes; R:129-137).
- Content checks (CE:41-63): first 8000 bytes containing a NUL -> "not text"; UTF-8 else ISO-Latin-1 decode; text containing `PRIVATE KEY-----` -> left out; for source (not repo notes) files: top 1000 chars containing "DO NOT EDIT", "@generated", "Code generated by", "Autogenerated by", "Generated by the protocol" -> left out; minified: avg chars/line > 300 AND some line > 3000 chars -> left out.
- Segments whose text fails `SearchText.hasWords` get keyword-only (no vector) (IX code(): worded filter).

### 4.3 Size limits
- `maxCodeBytes = 1<<20` (1 MiB) for source (M:46, CR:~"too big to be written by hand"; checked at crawl with `st_size`, and again for notebook text in CE:36); `maxNotebookBytes = 20<<20` for the .ipynb file itself (M:48).
- Piece cap 4000 chars (CE:27). No cap on number of stretches per file other than file size.

### 4.4 Ignore rules
- Folder skip: hidden folders and packages skipped by the enumerator; `skippedFolderNames` (node_modules bower_components venv site-packages __pycache__ DerivedData Pods Carthage; R:14-16), `*.noindex`, `~/Library`, user-excluded folders; and for code (R:60-64): vendor Vendor vendored third_party third-party thirdparty 3rdparty external extern deps build Build dist out target obj coverage htmlcov generated __generated__ wandb mlruns lightning_logs. Submodule/worktree (a `.git` *file* rather than directory) skipped (R:100-104).
- Git: for each repo (dir with `.git` directory) shells out to `git -C <repo> ls-files -z --cached --others --exclude-standard` and keeps only listed files (CR:211-240, env `GIT_OPTIONAL_LOCKS=0`). A subfolder chosen on its own still uses the enclosing repo's list (CR:`enterRoot`). If git is unavailable or fails: no filtering. Also tracked files are kept even if in vendor/ is NOT applicable because vendor folders are skipped first (CodeTests:209).
- Dotfiles: `.skipsHiddenFiles` -> all hidden files/dirs skipped (CR:~:46).
- Repo detection for the title: walk up from the file until `$HOME` for `.git` or a project marker (`.git package.json Package.swift pyproject.toml setup.py requirements.txt Cargo.toml go.mod pom.xml build.gradle build.gradle.kts Gemfile composer.json CMakeLists.txt`, or *.xcodeproj/*.xcworkspace), title = path from repo's *parent* ("billing/payments/retry.py"); no repo -> last two path components (CE:201-218; test CodeTests:166-175).
- Rust crate mapping: `ignore` crate (`WalkBuilder` with `git_ignore(true)`, `hidden(true)`, `filter_entry` for the skipped-folder list) replaces `git ls-files` (it respects .gitignore/.git/info/exclude/global; note slight difference: git ls-files `--cached` also lists tracked files that are now gitignored, `ignore` would drop them; and `ignore` needs `require_git(false)` handling for non-repos). `gix`/`git2` if exact `ls-files` parity is wanted. `walkdir` for plain walk, `infer`/manual NUL sniff for binary check, `serde_json` for notebooks, `unicode-normalization`, `regex`, `memchr`.

### 4.5 Notebooks (CE:152-195)
- `ipynb` parsed as JSON; nbformat 4 `cells[]`, nbformat 3 `worksheets[0].cells[]`; cell text = `source` (array of strings joined, or string) or `input` (nbformat 3). Outputs are never read. Cell types are not distinguished (markdown, code, raw alike).
- `cells(cells, size)`: skip cells empty after trim; a cell longer than `size` becomes its own `lines()` pieces all with first=last=cell number; otherwise pack whole cells joined by `"\n\n"` up to `size` (adding 2 chars separator) and flush when exceeding. Piece `first/last` = first/last cell *numbers* (1-based, index+1, empty cells inside range included). Segment kind `cells` (loc/loc_end = cell numbers). Tests: CodeTests:103-120.
- Whole-notebook text > `maxCodeBytes` -> left out ("cells over 1 MB").

---------------------------------------------------------------------------------------------------

## 5. Eval harness

### 5.1 File structure (MAIN:435-444)
```json
{ "base": "~/DigUpTestbed",
  "queries": [ { "q": "a penguin", "expect": ["synthetic/img/Animals-Penguin.jpg", "...mp4@0-6"], "tag": "image", "kind": "<optional FileKind>" } ] }
```
- `base`: directory (tilde-expanded) prepended to each expected path; hit.path must equal `base + "/" + path` (string equality on absolute path; MAIN:54).
- `expect`: list of acceptable answers (any one counts). Entry `path@a-b` additionally requires the hit's `loc` to fall in the closed interval [a, b]: for PDF pages / audio/video seconds `range.contains(loc)`; for code (`lines`/`cells` segments) the hit's [loc, locEnd] must *overlap* [a, b] (MAIN:57-61). Without `@`, any segment of that file counts.
- `tag`: category label for per-tag aggregation (also "ALL" row). `kind`: optional filter passed to `search(kinds:)`.
- Sizes: synthetic.json 37 queries, multilingual.json 48, code.json 41 (grep `"q"` counts) = 126 total. Author comments mention "105" queries across the three evals at an earlier date (S:315), and "27 queries" for a separate real-code eval (not in repo).
- Tags: synthetic: image(9) screenshot(6) exact(5) pdf-page(4) long(4) audio(3) video-moment(2) long-exact(2) doc(2); multilingual: en->bn(7) en->ar(6) bn-doc(4) ar-word(4) ar-doc(4) bn->media(3) bn-word(3) ar->media(3) bn->en(2) bn-shot(2) bn-pdf(2) ar-shot(2) ar-pdf(2) ar->en(2) bn-audio(1) ar-audio(1); code: code(33) code-name(6) code-doc(2).
- Queries starting with `code:` go to code.sqlite with code prompt; text after prefix is searched (MAIN:26-28,451-452).
- The eval corpus is `~/DigUpTestbed` (NOT in the repo; created by testbed scripts; Swift comments mention `testbed/make_lang.swift`). Check whether the repo has `testbed/`: not verified. Without that corpus and same embeddings, parity can't be run; plan: reproduce the testbed or diff Swift vs Rust result lists on a shared index.

### 5.2 Metrics (MAIN:446-... evaluate)
- Per query: `rank` = 1-based position of first hit matching any target (null if absent among top `-n`, default 10; `limit: depth`).
- Aggregates per tag and "ALL": `n`, `R@1`, `R@5`, `R@10` (fraction with rank <=1/5/10), `MRR` (mean of 1/rank, 0 for misses). Printed with `%5.2f`.
- Extra: "weaker-matches fold": average number of strongly-scoring results (>= 0.5 x top) vs total per query, and count of expected answers that rank beyond the strong fold.
- Notes: recall@k here is "first relevant in top k" (hit-rate), not fraction of expected items found. Eval uses file-level dedup (one hit per file) so an `@range` target on a file only matches if the *best* segment for that file lies in range.
- Eval flow: load all vectors, batch-embed all queries with `Prompts.query` or `Prompts.codeQuery`, then `searcher.search(...)` with CLI `--zgate` (default 2.0) and `--word-weight` (nil -> index default).
- Rust: implement identical `EvalFile` serde struct and the same overlap/contains rule; assert on tags; report R@1/5/10/MRR. Expect small numeric differences if the Rust embedder isn't bit-identical to llama.cpp Q8_0.

---------------------------------------------------------------------------------------------------

## 6. Performance observations and Rust levers

### 6.1 Current Swift behaviour (evidence)
- Whole index vectors are loaded into one anonymous `mmap` (MAP_ANON, not file-backed) at `reload()`: fp16 matrix `rows x 768` plus a 1024-row float32 scratch block (V:36-47; S:59-89). Loading = one SQL pass over every segment with vec (`SELECT ... FROM segments s JOIN files f ... WHERE s.vec IS NOT NULL ORDER BY s.id`) and `memcpy` of each blob (S:70-86). Row-parallel arrays: `rowFile`, `rowSegment`, `rowModality`, `rowSetsScale`.
- Reason for anonymous mmap: memory is returned to OS on `munmap`; malloc cached freed blocks (V:25-27). Author measured "70,000 vectors: 108 MB instead of 215 (f32)" and "~2 ms more a query" for fp16->fp32 conversion (V:22-24). Header says ~20k vectors, ~30 MB, "brute-force search over an in-memory copy is a few milliseconds" (IS:17-19).
- Scan = per 1024-row block: `vImageConvert_Planar16FtoPlanarF` then `vDSP_mmul` (matrix x vector) (S:285-304, V:14-18). Single-threaded. Apple Accelerate, i.e. SIMD/AMX-backed.
- Per-query extra passes over all N rows: `modalityMeans` (hash-map updates per row, S:334-341), `centered` map allocation (Array of Double N), second and third loops for variance, `meaning` array of N doubles, then `for row in 0..<scores.count` with dictionary lookups `best[rowFile[row]]` (S:128-131) and `allowed()` closure doing a dictionary lookup `fileKind[file]` per row. These are likely heavier than the dot product at scale in Swift (Dictionary in a hot loop) - my inference, not measured.
- FTS: one query with LIMIT 1000 + bm25 + `snippet()` computed for up to 1000 rows even though only the first 100 distinct files are kept (S:143-153); then one extra FTS query *per query term* for coverage (S:156-161). Then 2 point SELECTs per result hit (S:202, S:212) plus occasional 3rd. Author timings for code search: 560 -> 100 ms for a common identifier in llama.cpp after limiting definition lookup to top 10; others 10-50 ms (S:194-196).
- Reload cost: whole `reload()` re-reads every vector from SQLite after each index run; no incremental append.
- No ANN, no quantization beyond fp16, no sharding, no reuse of pre-sorted structures. WAL + synchronous NORMAL; per-file transactions; `Statement` is prepared every call (no statement cache; DB:37-41, 73-80) - a cost during bulk indexing.
- Batched inserts: per file/step one transaction (IX:629-639, IX:98); inserts go segment then fts, 2 statements per segment, each re-prepared.

### 6.2 Rust levers
1. Vector layout: keep fp16 blob on disk for compatibility (`half::f16`, LE). In memory choose one of:
   - fp16 resident + convert-on-the-fly blocked dot (as Swift). With `half` + `std::arch` (AVX2+F16C `_mm256_cvtph_ps`, NEON `vcvt_f32_f16` / native `fmla` fp16 on ARMv8.2) a fused convert+FMA kernel avoids the scratch block.
   - int8 scalar-quantized rows (per-row scale, or global scale since vectors are unit-norm) + i8 dot (AVX2 `vpmaddubsw`/VNNI `vpdpbusd`, NEON `sdot`) : 4x less memory traffic than fp32, 2x less than fp16. Keep an optional fp16 rerank of top ~100-200 to restore exact cosine. NOTE: the z-score step needs scores of *all* rows (mean/std over the whole distribution), so approximate scores would shift z slightly; keep int8 error small (cosine error ~1e-3) or compute mean/std from the int8 pass and rerank only for ordering - this changes parity subtly; test with the eval.
   - f32 for speed at 2x memory (not recommended past ~1M).
2. mmap: either anonymous (as Swift) or put a derived sidecar file `vectors.f16` (row-major, header + rowid list) memmapped read-only with `memmap2`; OS page cache shares across processes (app + CLI helper), cheap reload (append-only + rowid map), and release is free (unmap). Must stay consistent with SQLite (use `segments.id` as row key; rebuild on mismatch). The author deliberately kept vectors only in SQLite for crash-consistency (IS:17-18); a sidecar is a cache rebuilt from `segments.vec`.
3. Top-k / grouping: avoid materializing N doubles of z. Two passes: (a) pass 1 computes dot products into a reusable `Vec<f32>` while accumulating per-modality sum/count (only setsScale rows; use 4 f64 accumulators indexed by a `u8` modality) and global sum; (b) pass 2 computes centered mean and variance (or compute with Welford/sums of squares in pass 1 with a per-modality shift: Var of centered = E[(s - m_mod)^2] - mean^2 requires modality mean first, so 2 passes or use sum and sum-of-squares per modality and combine algebraically: centered=s-m_mod => sum(centered)=Σ_mod(Σs - n·m) , sumsq per modality = Σs² - 2mΣs + n m²; so 1 pass over data suffices). (c) per-file best via a dense `Vec<f32>` indexed by file index (map `file_id -> dense idx` once at load) - no hashing in the hot loop; since z is monotone in score *within a modality only*, keep per-file best z computed after stats known (z depends on modality mean so best-by-score != best-by-z across modalities; compute z in a final tight loop or keep best candidate per (file, modality) in pass 1 then merge). Kind filter via a dense `Vec<bool>`/bitset by file idx.
4. Parallel scan: `rayon` `par_chunks` over row blocks (e.g. 4096-16384 rows), each thread produces partial modality sums + score slice; second parallel loop for z/best with thread-local dense arrays merged by max. 1M-row workloads are memory-bandwidth-bound, so gains saturate at ~4-8 threads.
5. Top-k: only `limit` (<= ~20-50) files are returned, but candidate set includes all files with z >= 2.0 (could be many); use `select_nth_unstable_by` or a `BinaryHeap` of size `limit` after scoring candidates (needs coverage first, which involves FTS). Sorting only candidates, not N.
6. SQLite/FTS (rusqlite with `bundled` and features `["bundled", "functions"]`; FTS5 and JSON1 are enabled in the bundled build via `SQLITE_ENABLE_FTS5`; verify feature flag `bundled-full` or `bundled` includes FTS5 - rusqlite bundled compiles with FTS5 on):
   - `prepare_cached` for all hot statements; `BEGIN IMMEDIATE` per file; consider `PRAGMA cache_size`, `mmap_size`, `temp_store=MEMORY` for search-only connections; open search connection read-only (`SQLITE_OPEN_READ_ONLY`) separate from the writer.
   - FTS: avoid `snippet()` over 1000 rows (compute it only for the <=100 retained files/limit hits via a second query, or use `highlight`/own snippet on `original`); fetch `LIMIT 1000` rows from a subquery with `bm25()` only, then join. Replace the per-term coverage queries with one query per candidate set using `fts5vocab`-free tactic: MATCH each term restricted to `rowid IN (segments of candidate files)` or compute coverage once with a single `GROUP BY file` over `OR` match plus per-term rank via `matchinfo`-like approach (FTS5 aux function `fts5_decode`/custom aux function `xPhraseCount`). A custom auxiliary function in Rust (rusqlite `functions` does not expose FTS5 aux API; need `fts5` C API via `libsqlite3-sys`) is overkill; simplest: keep the current N+1 queries (they are small, 10-50 ms in author's numbers).
   - Custom tokenizer is not needed for parity; the built-in `porter unicode61 ... categories` works if SQLite >= 3.31.
   - `PRAGMA optimize` / `INSERT INTO fts(fts) VALUES('optimize')` after bulk indexing; `automerge` default is fine.
   - Bulk insert: reuse prepared statements; for a 1M-segment initial build consider `journal_mode=WAL`, `synchronous=OFF` during build (if crash-safety re-index is acceptable), and `INSERT INTO fts(fts) VALUES('rebuild')` pattern is unavailable (not external content), so insert rows directly.
   - Row ids: `fts.rowid == segments.id`; keep this invariant.
7. Avoid per-hit point queries: batch `SELECT ... WHERE id IN (...)` for files and segments; trivial win for limit<=20.
8. Float determinism: Swift scores in Float32 via vDSP, z-stats in Double (S:318-330). To keep rankings stable, accumulate means/variance in f64; ties broken by lower file id (S:184).

### 6.3 Size and speed estimates for 1M vectors x 768 dims (est., not measured)
- Raw vector bytes: fp16 = 1,000,000 x 1536 B = 1.536 GB (1.43 GiB); fp32 = 3.07 GB; int8 = 0.768 GB (+ 4 B scale per row).
  Swift's own figure: 70k vectors = 108 MB (fp16) (V:23) -> consistent (70,000 x 1536 = 107.5 MB).
- On disk in SQLite: blob + row overhead (~25-40 B/row: rowid, kind, modality, loc, loc_end, excerpt, record header) + excerpt text + FTS5 index. Vector blob 1536 B fits within a page (default 4096 B) but one 1.5 KB blob per row leaves ~2.4 KB/page for other columns; expect ~1.6-1.7 GB for `segments` before text and FTS. FTS5 size depends on text (rule of thumb 30-60% of text size + the `original` column copy); text volume at ~1,600 chars per chunk (IX comment 1600 chars/chunk; code stretch 1800) => ~1.6-1.8 GB raw text per 1M segments, so FTS + stored `body`/`original` could be 1.5-3 GB. Total DB roughly 3.5-5 GB (rough; media segments have no text, so less).
- RAM for scan: 1.536 GB (fp16) resident, plus side arrays: rowFile (8 B) + rowSegment (8 B) + modality (1 B) + setsScale (1 B) ~ 18 MB; Swift's 4-byte-wide Float scores array 4 MB and doubles 8-16 MB. With the swift arrays of Doubles `centered` + `meaning`: ~16 MB; fine.
- Scan throughput: dot product is memory-bound. Single thread streaming fp16 from RAM at ~10-20 GB/s -> 1.536 GB / 15 GB/s ~ 100 ms; with 4-8 cores and ~40-60 GB/s on a modern desktop ~ 30-40 ms. int8: half of those (~15-25 ms). Compute: 768 MACs x 1M = 0.77 GFLOP-pairs, negligible for SIMD cores (~tens of ms single-core AVX2 even if compute-bound). Convert-to-f32 overhead adds ~0.1-0.2 ns/element on x86 with F16C; the author measured 2 ms extra per 70k rows, i.e. ~29 ns/row => 29 s... no: 2 ms/70k = 28.6 ns per row => ~29 ms extra for 1M rows (scales linearly) on their Mac.
- The post-scan Swift hash-map loops would dominate at 1M rows (est. 50-200 ms), so the dense-array rewrite in 6.2.3 matters more than the SIMD kernel for latency at that scale.
- Loading 1M vectors from SQLite at startup: one pass, ~1.5 GB memcpy plus row decode; est. several seconds (disk/page-cache bound). A sidecar mmap file brings this to ~0 s warm.
- Brute force at 1M x 768 is still feasible interactively (< 100 ms with SIMD + rayon); ANN (HNSW via `hnsw_rs`/`usearch` crate) not needed, and would break the full-distribution z-score design (z needs scores of all vectors; the whole design "every vector is scored anyway, so the distribution comes for free", S:29-30).

### 6.4 Porting checklist / risks
- The z-score fusion depends on scanning ALL vectors and on per-modality means: any ANN or sampling changes behavior.
- `setsScale` flags depend on `files.info.pages` JSON and `segments.loc/kind`; reproduce S:70-85 exactly (join `files f`, `CAST(json_extract(f.info,'$.pages') AS INTEGER)`).
- Unicode behaviour must match: NFKC (Rust `unicode-normalization::nfkc`), `CharacterSet.alphanumerics` (Unicode L*, M*? Foundation's alphanumerics = letters + marks + decimal digits; Swift `CharacterSet.alphanumerics` includes categories L, M, N (Nd,Nl,No)? uncertain - verify; I believe it is Letter+Mark+Number), lowercasing, `isNumber`/`isLetter` on Character (grapheme-level).
- FTS5 tokenization of non-Latin scripts depends on the SQLite build's unicode tables; use the same bundled SQLite version (>= 3.45 recommended) so tokens match old indexes.
- Eval parity requires the exact embedder (Q8_0 GGUF EmbeddingGemma 2 via llama.cpp: `llama-cpp-2` crate or candle). If the Rust embedder differs numerically, tag `vector_source` differently and re-embed instead of reusing Swift indexes.
- Uncertain / unverified items: bundled SQLite minimum for the `categories` tokenizer option; exact definition of `CharacterSet.alphanumerics`; testbed corpus availability; behaviour of CJK in FTS (inferred); throughput numbers (estimates).
