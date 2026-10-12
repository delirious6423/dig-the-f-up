//! Locates and downloads the EmbeddingGemma 2 model files (docs/spec-embedder.md section 1).
//!
//! Lookup order: `$DIGUP_MODELS` if set (only that folder), else the app's models folder, then the Hugging Face
//! cache snapshot for the pinned revision. A file counts as present only when its size matches; checksums are
//! checked by [`verify`] and by [`download`], never on lookup.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variable that replaces the model folder lookup.
pub const MODELS_ENV: &str = "DIGUP_MODELS";
/// Hugging Face repository that hosts the files.
pub const REPOSITORY: &str = "ggml-org/embeddinggemma-2-GGUF";
/// Pinned repository revision (the same commit the Swift app pins).
pub const REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";
/// Identity of the embedding model, for index fingerprints.
pub const MODEL_ID: &str = "google/embeddinggemma-2";
/// Vector format version; bump only when vectors change beyond cosine 0.999.
pub const VECTOR_VERSION: u32 = 1;

const RESOLVE_PREFIX: &str = "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve";
const CHUNK: usize = 1 << 20;

/// One model file: its name in the repository and folder, its exact size and its SHA-256 (lowercase hex).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// The text model (310 MB): all that queries need.
pub const TEXT: ModelFile = ModelFile {
    name: "embeddinggemma-2-Q8_0.gguf",
    bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
};

/// The image and audio encoders (555 MB).
pub const MMPROJ: ModelFile = ModelFile {
    name: "mmproj-embeddinggemma-2-Q8_0.gguf",
    bytes: 554_821_024,
    sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
};

/// The download URL of `file` at the pinned revision (Hugging Face redirects it to a CDN that takes ranges).
pub fn url(file: &ModelFile) -> String {
    format!("{RESOLVE_PREFIX}/{REVISION}/{}", file.name)
}

/// The app's models folder: `$DIGUP_MODELS` if set and non-empty, else `<platform data dir>/DigUp/Models`
/// (macOS: `~/Library/Application Support/DigUp/Models`).
pub fn models_dir() -> PathBuf {
    if let Some(dir) = env_dir(MODELS_ENV) {
        return dir;
    }
    dirs::data_dir()
        .unwrap_or_default()
        .join("DigUp")
        .join("Models")
}

/// The pinned revision's folder in the Hugging Face cache: `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else
/// `~/.cache/huggingface/hub`, then `models--<org>--<repo>/snapshots/<revision>`.
fn hf_hub_dir() -> Option<PathBuf> {
    env_dir("HF_HUB_CACHE")
        .or_else(|| env_dir("HF_HOME").map(|home| home.join("hub")))
        .or_else(|| {
            dirs::home_dir().map(|home| home.join(".cache").join("huggingface").join("hub"))
        })
}

fn hf_snapshot_dir() -> Option<PathBuf> {
    let hub = hf_hub_dir()?;
    Some(
        hub.join(format!("models--{}", REPOSITORY.replace('/', "--")))
            .join("snapshots")
            .join(REVISION),
    )
}

/// Finds a present copy of `file`. With `$DIGUP_MODELS` set only that folder is searched; otherwise the models
/// folder, then the Hugging Face snapshot. Returns the first path whose size equals `file.bytes`.
pub fn locate(file: &ModelFile) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = match env_dir(MODELS_ENV) {
        Some(dir) => vec![dir.join(file.name)],
        None => {
            let mut v = vec![models_dir().join(file.name)];
            if let Some(snapshot) = hf_snapshot_dir() {
                v.push(snapshot.join(file.name));
            }
            v
        }
    };
    candidates
        .into_iter()
        .find(|path| has_size(path, file.bytes))
}

/// Environment variable that replaces the LightOnOCR model folder lookup (the files sit directly in it).
pub const LIGHTON_ENV: &str = "DIGUP_LIGHTON_MODELS";
/// Community GGUF conversion of lightonai/LightOnOCR-3-0.8B (Apache-2.0); lightonai publishes safetensors only.
pub const LIGHTON_REPOSITORY: &str = "mradermacher/LightOnOCR-3-0.8B-GGUF";
/// Pinned revision of [`LIGHTON_REPOSITORY`].
pub const LIGHTON_REVISION: &str = "d76992384e501892b4e37bd55020d762d30b5b37";

/// LightOnOCR-3-0.8B language model, Q8_0 (812 MB).
pub const LIGHTON_TEXT: ModelFile = ModelFile {
    name: "LightOnOCR-3-0.8B.Q8_0.gguf",
    bytes: 811_844_128,
    sha256: "b2955ffcce11b0c1fce6dd38d5303a0f3284161c48ade7f62bda9804ce5ba4f7",
};

/// LightOnOCR-3-0.8B vision projector, F16 (205 MB).
pub const LIGHTON_MMPROJ: ModelFile = ModelFile {
    name: "LightOnOCR-3-0.8B.mmproj-f16.gguf",
    bytes: 204_987_200,
    sha256: "c70a0eeee9bf2c2626510631a4c707321d48a91f577673304dd8771f2664f9d2",
};

/// Folder URL of the pinned LightOnOCR revision; pass it as `base_url` to [`download`].
pub fn lighton_base_url() -> String {
    format!("https://huggingface.co/{LIGHTON_REPOSITORY}/resolve/{LIGHTON_REVISION}")
}

/// Finds a present copy of a LightOnOCR file (size must match). With `$DIGUP_LIGHTON_MODELS` set only that folder
/// is searched; otherwise `<models_dir>/lightonocr` (and `$DIGUP_MODELS/lightonocr`), then the Hugging Face
/// snapshot of the pinned revision.
pub fn locate_lighton(file: &ModelFile) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = match env_dir(LIGHTON_ENV) {
        Some(dir) => vec![dir.join(file.name)],
        None => {
            let mut v = vec![models_dir().join("lightonocr").join(file.name)];
            if let Some(hub) = hf_hub_dir() {
                v.push(
                    hub.join(format!("models--{}", LIGHTON_REPOSITORY.replace('/', "--")))
                        .join("snapshots")
                        .join(LIGHTON_REVISION)
                        .join(file.name),
                );
            }
            v
        }
    };
    candidates.into_iter().find(|path| has_size(path, file.bytes))
}

/// Checks that `path` is a file of exactly `bytes` bytes. Symlinks are followed.
fn has_size(path: &Path, bytes: u64) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.len() == bytes)
        .unwrap_or(false)
}

/// Streams `path` through SHA-256 and checks it against `file.sha256`. Does not check the size.
pub fn verify(path: &Path, file: &ModelFile) -> Result<()> {
    let actual = sha256_file(path)?;
    if !actual.eq_ignore_ascii_case(file.sha256) {
        bail!(
            "{}: sha256 is {actual}, expected {}",
            path.display(),
            file.sha256
        );
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut input = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Downloads `file` into `dest_dir` and returns its final path.
///
/// Writes `<name>.part`, resuming from its current size with `Range: bytes=N-`. A 200 reply to a range request
/// restarts the file; a 416 for a complete `.part` goes straight to verification. A finished file must match
/// `file.bytes` and `file.sha256`; on a checksum or size overrun the `.part` is deleted and an error returned.
/// A connection that ends early keeps the `.part` for the next call. On success the file is renamed to its
/// final name.
///
/// `base_url` replaces `https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/<revision>` (the folder
/// URL; the file name is appended). `progress` receives `(downloaded, total)` bytes.
pub fn download(
    file: &ModelFile,
    dest_dir: &Path,
    base_url: Option<&str>,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir).with_context(|| format!("create {}", dest_dir.display()))?;
    let final_path = dest_dir.join(file.name);
    let part_path = dest_dir.join(format!("{}.part", file.name));

    if has_size(&final_path, file.bytes) && verify(&final_path, file).is_ok() {
        progress(file.bytes, file.bytes);
        return Ok(final_path);
    }

    let url = match base_url {
        Some(base) => format!("{}/{}", base.trim_end_matches('/'), file.name),
        None => url(file),
    };
    let agent = ureq::AgentBuilder::new()
        .redirects(5)
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(60))
        .build();

    let mut offset = match fs::metadata(&part_path) {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    if offset > file.bytes {
        fs::remove_file(&part_path)?;
        offset = 0;
    }

    let response = match get(&agent, &url, offset) {
        Ok(resp) => resp,
        Err(ureq::Error::Status(416, _)) if offset > 0 => {
            if offset == file.bytes {
                progress(file.bytes, file.bytes);
                return finish(file, &part_path, &final_path);
            }
            // The range is unsatisfiable for a partial file: start over.
            fs::remove_file(&part_path)?;
            offset = 0;
            get(&agent, &url, 0).map_err(|e| http_error(&url, e))?
        }
        Err(e) => return Err(http_error(&url, e)),
    };

    let status = response.status();
    let resumed = status == 206;
    if !resumed && status != 200 {
        bail!("unexpected HTTP {status} from {url}");
    }
    if !resumed {
        offset = 0;
    }

    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&part_path)
        .with_context(|| format!("open {}", part_path.display()))?;

    let mut reader = response.into_reader();
    let mut buf = vec![0u8; CHUNK];
    let mut done = offset;
    progress(done, file.bytes);
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                out.flush().ok();
                return Err(anyhow!(e)).with_context(|| {
                    format!(
                        "download of {} stopped at {done} of {} bytes",
                        file.name, file.bytes
                    )
                });
            }
        };
        out.write_all(&buf[..n])
            .with_context(|| format!("write {}", part_path.display()))?;
        done += n as u64;
        progress(done, file.bytes);
    }
    out.flush()?;
    drop(out);

    let len = fs::metadata(&part_path)?.len();
    if len < file.bytes {
        bail!(
            "download of {} stopped at {len} of {} bytes; run again to resume",
            file.name,
            file.bytes
        );
    }
    finish(file, &part_path, &final_path)
}

/// Checks a complete `.part` (size, then SHA-256) and renames it to its final name.
fn finish(file: &ModelFile, part_path: &Path, final_path: &Path) -> Result<PathBuf> {
    let len = fs::metadata(part_path)?.len();
    if len != file.bytes {
        fs::remove_file(part_path)?;
        bail!("{} is {len} bytes, expected {}", file.name, file.bytes);
    }
    if let Err(e) = verify(part_path, file) {
        fs::remove_file(part_path)?;
        return Err(e.context(format!(
            "{} failed its checksum; partial file deleted",
            file.name
        )));
    }
    fs::rename(part_path, final_path)
        .with_context(|| format!("rename to {}", final_path.display()))?;
    Ok(final_path.to_path_buf())
}

// ureq's error type is large; it is matched on directly (416 handling), so it is not boxed.
#[allow(clippy::result_large_err)]
fn get(agent: &ureq::Agent, url: &str, offset: u64) -> Result<ureq::Response, ureq::Error> {
    let mut request = agent.get(url);
    if offset > 0 {
        request = request.set("Range", &format!("bytes={offset}-"));
    }
    request.call()
}

fn http_error(url: &str, err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, _) => anyhow!("HTTP {code} from {url}"),
        ureq::Error::Transport(t) => anyhow!("request to {url} failed: {t}"),
    }
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
