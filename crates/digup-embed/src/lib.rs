//! Safe wrapper over llama.cpp b11461 for EmbeddingGemma 2: text, image and audio embeddings in one 768-d space.
//! A direct port of Sources/LlamaRuntime/LlamaEmbedder.swift; the call sequence is the same, so vectors match by construction.

pub mod sizing;

use anyhow::{anyhow, bail, Result};
use digup_llama_sys as sys;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::ptr::{self, NonNull};

pub use digup_core::DIM;

/// Max sequences packed into one `llama_process` call (LlamaEmbedder.swift:42).
const MAX_SEQUENCES: usize = 16;
/// Audio past this many seconds is ignored, as in the reference.
const AUDIO_SECONDS: usize = 30;
/// Valid image token budgets.
pub const IMAGE_BUDGETS: [usize; 5] = [70, 140, 280, 560, 1120];

// The bindgen enum type is u32 on macOS and i32 on Windows: keep the casts for portability.
#[allow(clippy::unnecessary_cast)]
extern "C" fn log_cb(level: sys::ggml_log_level, text: *const c_char, _ud: *mut c_void) {
    // Keep warnings and errors only; loading is chatty.
    // CONT lines continue the previous message, so they inherit its level.
    static LAST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    use std::sync::atomic::Ordering::Relaxed;
    if level != sys::GGML_LOG_LEVEL_CONT {
        LAST.store(level as u32, Relaxed);
    }
    if LAST.load(Relaxed) >= sys::GGML_LOG_LEVEL_WARN as u32 && !text.is_null() {
        use std::io::Write;
        let _ = write!(std::io::stderr(), "{}", unsafe { CStr::from_ptr(text) }.to_string_lossy());
    }
}

fn backend_init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        sys::llama_log_set(Some(log_cb), ptr::null_mut());
        sys::mtmd_helper_log_set(Some(log_cb), ptr::null_mut());
        sys::llama_backend_init();
    });
}

#[derive(Clone, Debug)]
pub struct Options {
    /// None = llama.cpp default (4).
    pub threads: Option<i32>,
    /// Layers offloaded to the GPU backend (999 = all, ignored by CPU-only builds).
    pub gpu_layers: i32,
}
impl Default for Options {
    fn default() -> Self {
        Self { threads: None, gpu_layers: 999 }
    }
}

pub struct Embedder {
    model: NonNull<sys::llama_model>,
    ctx: NonNull<sys::llama_context>,
    batch: NonNull<sys::llama_batch_ext>,
    media: Option<NonNull<sys::mtmd_context>>,
    vocab: *const sys::llama_vocab,
    max_tokens: usize,
    embedding_size: usize,
    row_size: usize,
    // batch state
    sequences: usize,
    batch_tokens: usize,
    position: i32,
}

// The llama context is used from one thread at a time (&mut self everywhere).
unsafe impl Send for Embedder {}

fn cstr(p: &Path) -> Result<CString> {
    CString::new(p.to_string_lossy().as_bytes()).map_err(|_| anyhow!("path contains NUL: {}", p.display()))
}

impl Embedder {
    /// `mmproj = None` loads the text model only (queries): 512-token context, ~250 MB.
    pub fn new(text_model: &Path, mmproj: Option<&Path>, opts: &Options) -> Result<Self> {
        backend_init();
        let path = cstr(text_model)?;
        let mpath = mmproj.map(cstr).transpose()?;
        unsafe {
            let mut mp = sys::llama_model_default_params();
            mp.n_gpu_layers = opts.gpu_layers;
            let model = NonNull::new(sys::llama_model_load_from_file(path.as_ptr(), mp))
                .ok_or_else(|| anyhow!("can't load {}", text_model.display()))?;

            let max_tokens: usize = if mmproj.is_some() { 2048 } else { 512 };
            let mut cp = sys::llama_context_default_params();
            cp.n_ctx = max_tokens as u32;
            cp.n_batch = max_tokens as u32;
            cp.n_ubatch = max_tokens as u32;
            cp.n_seq_max = MAX_SEQUENCES as u32;
            cp.kv_unified = true;
            cp.embeddings = true;
            cp.pooling_type = sys::LLAMA_POOLING_TYPE_MEAN;
            if let Some(t) = opts.threads {
                cp.n_threads = t;
                cp.n_threads_batch = t;
            }
            let ctx = match NonNull::new(sys::llama_init_from_model(model.as_ptr(), cp)) {
                Some(c) => c,
                None => {
                    sys::llama_model_free(model.as_ptr());
                    bail!("can't create a context");
                }
            };

            let media = if let Some(mpath) = &mpath {
                let mm = mmproj.unwrap();
                let mut p = sys::mtmd_context_params_default();
                p.use_gpu = opts.gpu_layers > 0;
                p.print_timings = false;
                p.warmup = true;
                // mtmd must never rescale: images are resized here (sizing.rs) to the reference's pixels.
                p.image_min_tokens = 1;
                p.image_max_tokens = *IMAGE_BUDGETS.last().unwrap() as i32;
                if let Some(t) = opts.threads {
                    p.n_threads = t;
                }
                match NonNull::new(sys::mtmd_init_from_file(mpath.as_ptr(), model.as_ptr(), p)) {
                    Some(m) => Some(m),
                    None => {
                        sys::llama_free(ctx.as_ptr());
                        sys::llama_model_free(model.as_ptr());
                        bail!("can't load {}", mm.display());
                    }
                }
            } else {
                None
            };

            let batch = match NonNull::new(sys::llama_batch_ext_init(ctx.as_ptr())) {
                Some(b) => b,
                None => {
                    if let Some(m) = media {
                        sys::mtmd_free(m.as_ptr());
                    }
                    sys::llama_free(ctx.as_ptr());
                    sys::llama_model_free(model.as_ptr());
                    bail!("can't create a batch");
                }
            };
            let mut e = Self {
                model,
                ctx,
                batch,
                media,
                vocab: sys::llama_model_get_vocab(model.as_ptr()),
                max_tokens,
                embedding_size: sys::llama_model_n_embd_out(model.as_ptr()) as usize,
                row_size: sys::llama_model_n_embd_inp(model.as_ptr()) as usize,
                sequences: 0,
                batch_tokens: 0,
                position: 0,
            };
            if e.embedding_size != DIM {
                bail!("model dimension {} != {DIM}", e.embedding_size);
            }
            // Two warm-up passes build backend pipelines/kernels before the first real request.
            for _ in 0..2 {
                e.embed_texts(&["task: search result | query: warm up".to_string()])?;
            }
            Ok(e)
        }
    }

    pub fn has_media(&self) -> bool {
        self.media.is_some()
    }

    // ---- text ----

    /// `<bos> text <eos>`, cut to `max_tokens` the way transformers cuts (keeping both special tokens).
    fn tokenize(&self, text: &str) -> Vec<sys::llama_token> {
        let bytes = text.as_bytes();
        let Ok(len) = i32::try_from(bytes.len()) else { return Vec::new() };
        let mut tokens = vec![0 as sys::llama_token; bytes.len() + 8];
        let mut n = unsafe {
            sys::llama_tokenize(self.vocab, bytes.as_ptr() as *const c_char, len, tokens.as_mut_ptr(), tokens.len() as i32, true, true)
        };
        if n == i32::MIN {
            return Vec::new(); // llama.h: overflow
        }
        if n < 0 {
            tokens = vec![0; (-n) as usize];
            n = unsafe {
                sys::llama_tokenize(self.vocab, bytes.as_ptr() as *const c_char, len, tokens.as_mut_ptr(), tokens.len() as i32, true, true)
            };
        }
        tokens.truncate(n.max(0) as usize);
        if tokens.len() > self.max_tokens {
            tokens.truncate(self.max_tokens - 1);
            tokens.push(unsafe { sys::llama_vocab_eos(self.vocab) });
        }
        tokens
    }

    /// Embeds already-prompted texts. Inputs are packed into shared batches (<=16 sequences, <=max_tokens tokens).
    pub fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        self.reset();
        for t in texts {
            let tokens = self.tokenize(t);
            if self.sequences == MAX_SEQUENCES || self.batch_tokens + tokens.len() > self.max_tokens {
                out.extend(self.run()?);
            }
            self.begin();
            for tok in tokens {
                let idx = unsafe { sys::llama_batch_ext_add_token(self.batch.as_ptr(), (self.sequences - 1) as i32, tok) };
                self.place(idx)?;
            }
        }
        if self.sequences > 0 {
            out.extend(self.run()?);
        }
        Ok(out)
    }

    fn begin(&mut self) {
        self.sequences += 1;
        self.position = 0;
    }

    fn reset(&mut self) {
        unsafe { sys::llama_batch_ext_clear(self.batch.as_ptr()) };
        self.sequences = 0;
        self.batch_tokens = 0;
    }

    fn place(&mut self, idx: i32) -> Result<()> {
        if idx < 0 {
            bail!("batch full ({idx})");
        }
        // A 4-element position array even for non-M-RoPE models, as the reference does.
        let pos: [sys::llama_pos; 4] = [self.position, 0, 0, 0];
        let ok = unsafe {
            sys::llama_batch_ext_set_pos(self.batch.as_ptr(), idx, pos.as_ptr()) && sys::llama_batch_ext_set_output_embd(self.batch.as_ptr(), idx, true)
        };
        if !ok {
            bail!("can't place token {idx}");
        }
        self.position += 1;
        self.batch_tokens += 1;
        Ok(())
    }

    /// Runs the model over the batch; one L2-normalized vector per sequence, in order. Always resets the batch.
    fn run(&mut self) -> Result<Vec<Vec<f32>>> {
        let result = (|| {
            let status = unsafe { sys::llama_process(self.ctx.as_ptr(), sys::LLAMA_PROCESS_TYPE_ENCODE, self.batch.as_ptr()) };
            if status != 0 {
                bail!("llama_process failed ({status})");
            }
            (0..self.sequences)
                .map(|s| {
                    let p = unsafe { sys::llama_get_embeddings_seq(self.ctx.as_ptr(), s as i32) };
                    if p.is_null() {
                        bail!("no pooled embedding for input {s}");
                    }
                    let pooled = unsafe { std::slice::from_raw_parts(p, self.embedding_size) };
                    let sum: f64 = pooled.iter().map(|&x| x as f64 * x as f64).sum();
                    let scale = if sum > 0.0 { (1.0 / sum.sqrt()) as f32 } else { 0.0 };
                    Ok(pooled.iter().map(|&x| x * scale).collect())
                })
                .collect()
        })();
        self.reset();
        result
    }

    // ---- media ----

    fn media_ctx(&self) -> Result<*mut sys::mtmd_context> {
        self.media.map(|m| m.as_ptr()).ok_or_else(|| anyhow!("this embedder was opened text-only"))
    }

    /// Embeds an image already in memory (packed RGB8). Resizes to the reference's pixels for `budget` first.
    pub fn embed_image_rgb(&mut self, width: usize, height: usize, rgb: &[u8], budget: usize) -> Result<Vec<f32>> {
        self.media_ctx()?;
        if !IMAGE_BUDGETS.contains(&budget) {
            bail!("image budget must be one of {IMAGE_BUDGETS:?}");
        }
        if rgb.len() != width * height * 3 {
            bail!("pixel buffer is {} bytes, expected {}", rgb.len(), width * height * 3);
        }
        let (th, tw) = sizing::target_size(height, width, budget).ok_or_else(|| anyhow!("image too thin to embed"))?;
        let resized;
        let (w, h, px): (usize, usize, &[u8]) = if (th, tw) == (height, width) {
            (width, height, rgb)
        } else {
            resized = sizing::resize_bicubic(rgb, width, height, tw, th);
            (tw, th, &resized)
        };
        let bitmap = unsafe { sys::mtmd_bitmap_init(w as u32, h as u32, px.as_ptr()) };
        self.embed_bitmap(bitmap, "image")
    }

    /// Embeds mono 16 kHz f32 samples (first 30 s used).
    pub fn embed_audio_samples(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        let rate = unsafe { sys::mtmd_get_audio_sample_rate(self.media_ctx()?) } as usize;
        let n = samples.len().min(AUDIO_SECONDS * rate);
        let bitmap = unsafe { sys::mtmd_bitmap_init_from_audio(n, samples.as_ptr()) };
        self.embed_bitmap(bitmap, "audio")
    }

    /// Embeds an image file (decoded by stb_image inside mtmd).
    pub fn embed_image_file(&mut self, path: &Path, budget: usize) -> Result<Vec<f32>> {
        let bm = self.load_file(path)?;
        let result = (|| unsafe {
            if sys::mtmd_bitmap_is_audio(bm) {
                bail!("not an image: {}", path.display());
            }
            let (w, h) = (sys::mtmd_bitmap_get_nx(bm) as usize, sys::mtmd_bitmap_get_ny(bm) as usize);
            let n = sys::mtmd_bitmap_get_n_bytes(bm);
            let d = sys::mtmd_bitmap_get_data(bm);
            if d.is_null() || n == 0 {
                bail!("empty image: {}", path.display());
            }
            let px = std::slice::from_raw_parts(d, n);
            self.embed_image_rgb(w, h, px, budget)
        })();
        unsafe { sys::mtmd_bitmap_free(bm) };
        result
    }

    /// Embeds an audio file (wav/mp3/flac, decoded by miniaudio inside mtmd, resampled to 16 kHz).
    pub fn embed_audio_file(&mut self, path: &Path) -> Result<Vec<f32>> {
        let bm = self.load_file(path)?;
        let result = (|| unsafe {
            if !sys::mtmd_bitmap_is_audio(bm) {
                bail!("not audio: {}", path.display());
            }
            let n = sys::mtmd_bitmap_get_n_bytes(bm) / 4;
            let d = sys::mtmd_bitmap_get_data(bm);
            if d.is_null() || n == 0 {
                bail!("empty audio: {}", path.display());
            }
            let data = std::slice::from_raw_parts(d as *const f32, n);
            self.embed_audio_samples(data)
        })();
        unsafe { sys::mtmd_bitmap_free(bm) };
        result
    }

    fn load_file(&self, path: &Path) -> Result<*mut sys::mtmd_bitmap> {
        let mc = self.media_ctx()?;
        let p = cstr(path)?;
        let w = unsafe { sys::mtmd_helper_bitmap_init_from_file(mc, p.as_ptr(), false, sys::mtmd_helper_init_opt_default()) };
        if !w.video_ctx.is_null() {
            unsafe {
                if !w.bitmap.is_null() {
                    sys::mtmd_bitmap_free(w.bitmap);
                }
                sys::mtmd_helper_video_free(w.video_ctx);
            }
            bail!("video goes in as frames and sound, not as a file: {}", path.display());
        }
        if w.bitmap.is_null() {
            bail!("can't read {}", path.display());
        }
        Ok(w.bitmap)
    }

    /// Takes ownership of `bitmap` and frees it.
    fn embed_bitmap(&mut self, bitmap: *mut sys::mtmd_bitmap, what: &str) -> Result<Vec<f32>> {
        if bitmap.is_null() {
            bail!("can't build the {what} input");
        }
        let mc = match self.media_ctx() {
            Ok(m) => m,
            Err(e) => {
                unsafe { sys::mtmd_bitmap_free(bitmap) };
                return Err(e);
            }
        };
        let chunks = unsafe { sys::mtmd_input_chunks_init() };
        let result = (|| unsafe {
            let marker = CStr::from_ptr(sys::mtmd_default_marker());
            let text = sys::mtmd_input_text { text: marker.as_ptr(), text_len: marker.to_bytes().len(), add_special: true, parse_special: true };
            let bitmaps = [bitmap as *const sys::mtmd_bitmap];
            let status = sys::mtmd_tokenize(mc, chunks, &text, bitmaps.as_ptr(), 1);
            if status != 0 {
                bail!("can't tokenize the {what} ({status})");
            }
            self.reset();
            self.begin();
            for i in 0..sys::mtmd_input_chunks_size(chunks) {
                let chunk = sys::mtmd_input_chunks_get(chunks, i);
                if sys::mtmd_input_chunk_get_type(chunk) == sys::MTMD_INPUT_CHUNK_TYPE_TEXT {
                    let mut n = 0usize;
                    let toks = sys::mtmd_input_chunk_get_tokens_text(chunk, &mut n);
                    if toks.is_null() {
                        continue;
                    }
                    for j in 0..n {
                        let idx = sys::llama_batch_ext_add_token(self.batch.as_ptr(), (self.sequences - 1) as i32, *toks.add(j));
                        self.place(idx)?;
                    }
                } else {
                    let rows = if sys::mtmd_encode_chunk(mc, chunk) == 0 { sys::mtmd_get_output_embd(mc) } else { ptr::null_mut() };
                    if rows.is_null() {
                        bail!("the encoder failed on the {what}");
                    }
                    for r in 0..sys::mtmd_input_chunk_get_n_tokens(chunk) {
                        let embd = sys::llama_embd { data: rows.add(r * self.row_size), n_rows: 1, n_embd: self.row_size };
                        let idx = sys::llama_batch_ext_add_embd(self.batch.as_ptr(), (self.sequences - 1) as i32, embd);
                        self.place(idx)?;
                    }
                }
            }
            Ok(self.run()?.swap_remove(0))
        })();
        if result.is_err() {
            self.reset();
        }
        unsafe {
            sys::mtmd_input_chunks_free(chunks);
            sys::mtmd_bitmap_free(bitmap);
        }
        result
    }
}

impl Drop for Embedder {
    fn drop(&mut self) {
        unsafe {
            sys::llama_batch_ext_free(self.batch.as_ptr());
            if let Some(m) = self.media {
                sys::mtmd_free(m.as_ptr());
            }
            sys::llama_free(self.ctx.as_ptr());
            sys::llama_model_free(self.model.as_ptr());
        }
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
