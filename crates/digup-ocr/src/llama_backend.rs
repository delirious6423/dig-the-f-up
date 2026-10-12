//! LightOnOCR-3-0.8B through the vendored llama.cpp/mtmd (`digup-llama-sys`), behind feature `lighton`.
//!
//! The model is loaded once and kept in a process-wide `Mutex` ([`recognize`]); calls are serialized. Every
//! public entry point returns `None`/`Err` instead of panicking (the FFI calls are wrapped in `catch_unwind`).
//! Prompt: empty (the model only follows an empty prompt or its grounding prompt), greedy decoding.
//!
//! Guards: images are rejected above [`MAX_PIXELS`] and downscaled to a longest side of [`MAX_SIDE`] px (model
//! card); output is capped at [`DEFAULT_MAX_TOKENS`] (env `DIGUP_OCR_MAX_TOKENS`) and a call is aborted after
//! [`DEFAULT_TIMEOUT_SECS`] (env `DIGUP_OCR_TIMEOUT_SECS`) through ggml's abort callback.
//!
//! `unsafe`: all of it is FFI into llama.cpp. Raw pointers are owned by [`LightOn`] and freed in `Drop`;
//! `unsafe impl Send` is sound because the context is only used through `&mut self` (one thread at a time).

use digup_llama_sys as sys;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Longest image side fed to the model (model card).
pub const MAX_SIDE: usize = 2048;
/// Inputs above this many pixels are refused outright (decompression-bomb style guard).
pub const MAX_PIXELS: usize = 100_000_000;
/// Default cap on generated tokens.
pub const DEFAULT_MAX_TOKENS: usize = 1024;
/// Default wall-clock limit for one image.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Vision tokens allowed per image (2048 px / 32 px per merged patch, squared).
const IMAGE_MAX_TOKENS: i32 = 4096;
const N_CTX: u32 = 6144;
const N_BATCH: u32 = 2048;

/// Tuning knobs; `Default` reads the `DIGUP_OCR_*` environment variables.
#[derive(Debug, Clone)]
pub struct Options {
    pub max_tokens: usize,
    pub timeout: Duration,
    /// Layers offloaded to the GPU backend (999 = all, ignored by CPU-only builds).
    pub gpu_layers: i32,
    pub threads: Option<i32>,
}

impl Default for Options {
    fn default() -> Self {
        let env = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok());
        Self {
            max_tokens: env("DIGUP_OCR_MAX_TOKENS").map(|v| v.clamp(1, 4096) as usize).unwrap_or(DEFAULT_MAX_TOKENS),
            timeout: Duration::from_secs(env("DIGUP_OCR_TIMEOUT_SECS").unwrap_or(DEFAULT_TIMEOUT_SECS).max(1)),
            gpu_layers: 999,
            threads: None,
        }
    }
}

// --- logging and abort plumbing -------------------------------------------------------------------------------

static VERBOSE: OnceLock<bool> = OnceLock::new();

#[allow(clippy::unnecessary_cast)]
extern "C" fn log_cb(level: sys::ggml_log_level, text: *const c_char, _ud: *mut c_void) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    if level != sys::GGML_LOG_LEVEL_CONT {
        LAST.store(level as u64, Relaxed);
    }
    let verbose = *VERBOSE.get_or_init(|| std::env::var_os("DIGUP_OCR_VERBOSE").is_some());
    if (verbose || LAST.load(Relaxed) >= sys::GGML_LOG_LEVEL_WARN as u64) && !text.is_null() {
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

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// Deadline in nanoseconds since [`epoch`]; 0 = none. One engine runs at a time (the global mutex), so a single
/// process-wide slot is enough for the abort callback.
static DEADLINE_NS: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn abort_cb(_data: *mut c_void) -> bool {
    let d = DEADLINE_NS.load(Relaxed);
    d != 0 && epoch().elapsed().as_nanos() as u64 > d
}

fn cstr(p: &Path) -> Result<CString, String> {
    CString::new(p.to_string_lossy().as_bytes()).map_err(|_| format!("path contains NUL: {}", p.display()))
}

// --- engine ----------------------------------------------------------------------------------------------------

/// A loaded LightOnOCR model plus its vision projector.
pub struct LightOn {
    model: NonNull<sys::llama_model>,
    ctx: NonNull<sys::llama_context>,
    mtmd: NonNull<sys::mtmd_context>,
    batch: NonNull<sys::llama_batch_ext>,
    vocab: *const sys::llama_vocab,
    opts: Options,
}

// The llama context is used from one thread at a time (&mut self everywhere).
unsafe impl Send for LightOn {}

impl LightOn {
    /// Loads the language model and the projector. Errors describe what failed (llama.cpp's own log goes to
    /// stderr for warnings/errors, or fully with `DIGUP_OCR_VERBOSE=1`).
    pub fn load(model_path: &Path, mmproj_path: &Path, opts: Options) -> Result<Self, String> {
        std::panic::catch_unwind(|| Self::load_inner(model_path, mmproj_path, opts))
            .unwrap_or_else(|_| Err("panic while loading LightOnOCR".into()))
    }

    fn load_inner(model_path: &Path, mmproj_path: &Path, opts: Options) -> Result<Self, String> {
        backend_init();
        let mp = cstr(model_path)?;
        let pp = cstr(mmproj_path)?;
        unsafe {
            let mut params = sys::llama_model_default_params();
            params.n_gpu_layers = opts.gpu_layers;
            let model = NonNull::new(sys::llama_model_load_from_file(mp.as_ptr(), params))
                .ok_or_else(|| format!("can't load {}", model_path.display()))?;

            let mut cp = sys::llama_context_default_params();
            cp.n_ctx = N_CTX;
            cp.n_batch = N_BATCH;
            cp.n_ubatch = N_BATCH;
            cp.abort_callback = Some(abort_cb);
            if let Some(t) = opts.threads {
                cp.n_threads = t;
                cp.n_threads_batch = t;
            }
            let Some(ctx) = NonNull::new(sys::llama_init_from_model(model.as_ptr(), cp)) else {
                sys::llama_model_free(model.as_ptr());
                return Err("can't create a llama context".into());
            };

            let mut p = sys::mtmd_context_params_default();
            p.use_gpu = opts.gpu_layers > 0;
            p.print_timings = false;
            p.warmup = false;
            p.image_max_tokens = IMAGE_MAX_TOKENS;
            if let Some(t) = opts.threads {
                p.n_threads = t;
            }
            let Some(mtmd) = NonNull::new(sys::mtmd_init_from_file(pp.as_ptr(), model.as_ptr(), p)) else {
                sys::llama_free(ctx.as_ptr());
                sys::llama_model_free(model.as_ptr());
                return Err(format!("can't load vision projector {}", mmproj_path.display()));
            };
            let Some(batch) = NonNull::new(sys::llama_batch_ext_init(ctx.as_ptr())) else {
                sys::mtmd_free(mtmd.as_ptr());
                sys::llama_free(ctx.as_ptr());
                sys::llama_model_free(model.as_ptr());
                return Err("can't create a batch".into());
            };
            Ok(Self { model, ctx, mtmd, batch, vocab: sys::llama_model_get_vocab(model.as_ptr()), opts })
        }
    }

    /// Recognizes the text in a packed RGB8 image. `Ok("")` means the model saw no text.
    pub fn recognize(&mut self, width: usize, height: usize, rgb: &[u8]) -> Result<String, String> {
        let need = width.checked_mul(height).and_then(|n| n.checked_mul(3));
        match need {
            Some(n) if width > 0 && height > 0 && rgb.len() >= n && width * height <= MAX_PIXELS => {}
            _ => return Err("invalid or oversized image".into()),
        }
        let resized;
        let (w, h, px) = if width.max(height) > MAX_SIDE {
            let s = MAX_SIDE as f64 / width.max(height) as f64;
            let (tw, th) = (((width as f64 * s).round() as usize).max(1), ((height as f64 * s).round() as usize).max(1));
            resized = box_resize(rgb, width, height, tw, th);
            (tw, th, &resized[..])
        } else {
            (width, height, rgb)
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run(w, h, px)))
            .unwrap_or_else(|_| Err("panic while running LightOnOCR".into()))
    }

    fn run(&mut self, w: usize, h: usize, px: &[u8]) -> Result<String, String> {
        let deadline = epoch().elapsed() + self.opts.timeout;
        DEADLINE_NS.store(deadline.as_nanos() as u64, Relaxed);
        let r = unsafe { self.run_unsafe(w, h, px) };
        DEADLINE_NS.store(0, Relaxed);
        r
    }

    unsafe fn run_unsafe(&mut self, w: usize, h: usize, px: &[u8]) -> Result<String, String> {
        // Forget the previous image (also after an aborted or failed call).
        let mem = sys::llama_get_memory(self.ctx.as_ptr());
        if !mem.is_null() {
            sys::llama_memory_clear(mem, true);
        }
        let bitmap = sys::mtmd_bitmap_init(w as u32, h as u32, px.as_ptr());
        if bitmap.is_null() {
            return Err("can't create bitmap".into());
        }
        let chunks = sys::mtmd_input_chunks_init();
        let result = self.generate(bitmap, chunks);
        sys::mtmd_input_chunks_free(chunks);
        sys::mtmd_bitmap_free(bitmap);
        result
    }

    unsafe fn generate(&mut self, bitmap: *mut sys::mtmd_bitmap, chunks: *mut sys::mtmd_input_chunks) -> Result<String, String> {
        // Qwen chat format with an empty user prompt: only the image marker.
        // The GGUF's own chat template with thinking disabled: an empty user turn holding only the image.
        // Without the empty think block the model starts with "<think>" and a markdown image stub (measured).
        let marker = CStr::from_ptr(sys::mtmd_default_marker()).to_string_lossy().into_owned();
        let prompt = format!("<|im_start|>user\n{marker}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n");
        let prompt = CString::new(prompt).map_err(|_| "prompt".to_string())?;
        let text = sys::mtmd_input_text { text: prompt.as_ptr(), text_len: prompt.as_bytes().len(), add_special: true, parse_special: true };
        let bitmaps = [bitmap as *const sys::mtmd_bitmap];
        let st = sys::mtmd_tokenize(self.mtmd.as_ptr(), chunks, &text, bitmaps.as_ptr(), 1);
        if st != 0 {
            return Err(format!("mtmd_tokenize failed ({st})"));
        }
        let mut n_past: sys::llama_pos = 0;
        let st = sys::mtmd_helper_eval_chunks(self.mtmd.as_ptr(), self.ctx.as_ptr(), chunks, 0, 0, N_BATCH as i32, true, &mut n_past);
        if st != 0 {
            return Err(format!("mtmd_helper_eval_chunks failed ({st}); timeout or image too large"));
        }
        if n_past as usize + self.opts.max_tokens > N_CTX as usize {
            return Err(format!("prompt of {n_past} positions leaves no room for output"));
        }

        let smpl = sys::llama_sampler_init_greedy();
        if smpl.is_null() {
            return Err("can't create sampler".into());
        }
        let mut bytes: Vec<u8> = Vec::new();
        let mut err = None;
        for _ in 0..self.opts.max_tokens {
            let tok = sys::llama_sampler_sample(smpl, self.ctx.as_ptr(), -1);
            sys::llama_sampler_accept(smpl, tok);
            if sys::llama_vocab_is_eog(self.vocab, tok) {
                break;
            }
            let mut buf = [0 as c_char; 128];
            let n = sys::llama_token_to_piece(self.vocab, tok, buf.as_mut_ptr(), buf.len() as i32, 0, false);
            if n > 0 {
                bytes.extend(buf[..n as usize].iter().map(|&c| c as u8));
            }
            sys::llama_batch_ext_clear(self.batch.as_ptr());
            let idx = sys::llama_batch_ext_add_token(self.batch.as_ptr(), 0, tok);
            let pos: [sys::llama_pos; 4] = [n_past, 0, 0, 0];
            if idx < 0
                || !sys::llama_batch_ext_set_pos(self.batch.as_ptr(), idx, pos.as_ptr())
                || !sys::llama_batch_ext_set_output_logits(self.batch.as_ptr(), idx, true)
            {
                err = Some("can't build decode batch".to_string());
                break;
            }
            n_past += 1;
            let st = sys::llama_process(self.ctx.as_ptr(), sys::LLAMA_PROCESS_TYPE_DECODE, self.batch.as_ptr());
            if st != 0 {
                err = Some(format!("decode failed ({st}); timeout?"));
                break;
            }
        }
        sys::llama_sampler_free(smpl);
        match err {
            // Keep partial text from an aborted run rather than lose it.
            Some(e) if bytes.is_empty() => Err(e),
            _ => Ok(String::from_utf8_lossy(&bytes).trim().to_string()),
        }
    }
}

impl Drop for LightOn {
    fn drop(&mut self) {
        unsafe {
            sys::llama_batch_ext_free(self.batch.as_ptr());
            sys::mtmd_free(self.mtmd.as_ptr());
            sys::llama_free(self.ctx.as_ptr());
            sys::llama_model_free(self.model.as_ptr());
        }
    }
}

/// Area-average downscale of packed RGB8.
fn box_resize(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 3];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = (((y + 1) * sh).div_ceil(dh)).clamp(y0 + 1, sh);
        for x in 0..dw {
            let x0 = x * sw / dw;
            let x1 = (((x + 1) * sw).div_ceil(dw)).clamp(x0 + 1, sw);
            let mut acc = [0u32; 3];
            for yy in y0..y1 {
                let row = &src[(yy * sw + x0) * 3..(yy * sw + x1) * 3];
                #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
                for px in row.chunks_exact(3) {
                    acc[0] += px[0] as u32;
                    acc[1] += px[1] as u32;
                    acc[2] += px[2] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * dw + x) * 3;
            out[o] = (acc[0] / n) as u8;
            out[o + 1] = (acc[1] / n) as u8;
            out[o + 2] = (acc[2] / n) as u8;
        }
    }
    out
}

// --- process-wide instance -----------------------------------------------------------------------------------

enum State {
    Empty,
    Ready(Box<LightOn>),
    /// Loading failed once; do not retry on every image.
    Failed,
}

static ENGINE: Mutex<State> = Mutex::new(State::Empty);

/// Where the two model files are, or `None` when they are not installed.
pub fn model_paths() -> Option<(PathBuf, PathBuf)> {
    Some((digup_models::locate_lighton(&digup_models::LIGHTON_TEXT)?, digup_models::locate_lighton(&digup_models::LIGHTON_MMPROJ)?))
}

/// True when the model files are installed.
pub fn available() -> bool {
    model_paths().is_some()
}

/// Recognizes text with the shared model, loading it on first use. `None` when the model is missing, loading
/// failed, the image is invalid, or nothing was found.
pub fn recognize(width: usize, height: usize, rgb: &[u8]) -> Option<String> {
    let mut guard = ENGINE.lock().unwrap_or_else(|p| p.into_inner());
    if matches!(*guard, State::Empty) {
        *guard = match model_paths().map(|(m, p)| LightOn::load(&m, &p, Options::default())) {
            Some(Ok(e)) => {
                register_exit_cleanup();
                State::Ready(Box::new(e))
            }
            Some(Err(e)) => {
                eprintln!("digup-ocr: LightOnOCR unavailable: {e}");
                State::Failed
            }
            None => State::Failed,
        };
    }
    let State::Ready(engine) = &mut *guard else { return None };
    match engine.recognize(width, height, rgb) {
        Ok(t) if !t.is_empty() => Some(t),
        Ok(_) => None,
        Err(e) => {
            eprintln!("digup-ocr: LightOnOCR failed: {e}");
            None
        }
    }
}

extern "C" {
    fn atexit(cb: extern "C" fn()) -> i32;
}

extern "C" fn free_at_exit() {
    // try_lock: never block or panic inside an exit handler.
    if let Ok(mut g) = ENGINE.try_lock() {
        *g = State::Empty;
    }
}

/// ggml's Metal device is a C++ static that asserts at process exit if buffers are still alive, so the shared
/// engine must be freed first. `atexit` handlers run in reverse registration order and this one is registered
/// after the model (and with it ggml's statics) was created, so it runs before them.
fn register_exit_cleanup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: `atexit` is a libc function; the callback is a plain `extern "C" fn` that does not unwind.
    ONCE.call_once(|| unsafe {
        atexit(free_at_exit);
    });
}

/// Frees the shared model (it is reloaded on the next call). Call before process exit to release GPU memory early.
pub fn unload() {
    let mut guard = ENGINE.lock().unwrap_or_else(|p| p.into_inner());
    *guard = State::Empty;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_resize_averages() {
        let src = vec![0u8, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 0];
        let out = box_resize(&src, 2, 2, 1, 1);
        assert_eq!(out, vec![127, 127, 127]);
        assert_eq!(box_resize(&src, 2, 2, 2, 2), src);
    }
}
