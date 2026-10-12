//! Port of Rules.swift. The OS type database (UTType) is replaced by extension and magic-byte tables.

use crate::crawler::CrawlOptions;
use crate::types::FileKind;
use regex::Regex;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

/// Rules.swift:4
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Index(FileKind),
    /// A reason worth reporting ("code or data file").
    Skip(&'static str),
    /// A type we don't handle at all (counted as "other type").
    Ignore,
}

/// Rules.swift:14-16
pub const SKIPPED_FOLDER_NAMES: &[&str] = &[
    "node_modules", "bower_components", "venv", "site-packages", "__pycache__", "DerivedData", "Pods", "Carthage",
    // Port additions. Python distributions hold thousands of package docs and images under non-hidden names.
    "anaconda3", "miniconda3", "miniforge3",
    // Steam game installs: assets, not user files.
    "steamapps",
    // Backups and recycle bins: copies of files already indexed elsewhere, or system-owned.
    "Backups.backupdb", "WindowsImageBackup", "FileHistory", "$RECYCLE.BIN", "Windows.old", "$WINDOWS.~BT",
    "System Volume Information",
];

/// Rules.swift:19-21
pub const PROJECT_MARKERS: &[&str] = &[
    ".git", "package.json", "Package.swift", "pyproject.toml", "setup.py", "requirements.txt", "Cargo.toml",
    "go.mod", "pom.xml", "build.gradle", "build.gradle.kts", "Gemfile", "composer.json", "CMakeLists.txt",
];
/// Rules.swift:22
pub const PROJECT_BUNDLE_EXTENSIONS: &[&str] = &["xcodeproj", "xcworkspace"];

/// Rules.swift:25
pub const DOC_EXTENSIONS: &[&str] = &["txt", "text", "md", "markdown", "rtf", "docx", "doc", "odt", "html", "htm"];

/// Rules.swift:28-35. Source code and data files: off by default.
pub const CODE_EXTENSIONS: &[&str] = &[
    "py", "ipynb", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "m", "mm", "swift", "tsx", "cts", "js", "jsx", "mjs",
    "cjs", "go", "rs", "java", "kt", "kts", "scala", "rb", "php", "cs", "fs", "sh", "bash", "zsh", "fish", "ps1",
    "lua", "pl", "r", "jl", "dart", "sql", "vue", "svelte", "css", "scss", "sass", "less", "json", "jsonl", "yaml",
    "yml", "toml", "xml", "plist", "gradle", "cmake", "mk", "lock", "log", "csv", "tsv", "ini", "cfg", "conf",
    "proto", "graphql", "tf", "nix", "zig", "hs", "ex", "exs", "erl", "clj", "elm", "ml", "vim", "el", "asm",
    "metal", "glsl", "hlsl", "wgsl", "cu", "pyi", "pyc",
];

/// Rules.swift:37 (case-sensitive hasPrefix on the file name)
pub const SCREENSHOT_PREFIXES: &[&str] = &["Screenshot", "Screen Shot", "CleanShot", "SCR-"];

/// Rules.swift:42-49
pub const SOURCE_EXTENSIONS: &[&str] = &[
    "py", "pyi", "pyx", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "m", "mm", "swift", "ts", "tsx", "mts",
    "cts", "js", "jsx", "mjs", "cjs", "go", "rs", "java", "kt", "kts", "scala", "groovy", "rb", "php", "cs", "fs",
    "sh", "bash", "zsh", "fish", "ps1", "lua", "pl", "pm", "r", "jl", "dart", "sql", "vue", "svelte", "astro",
    "html", "htm", "css", "scss", "sass", "less", "gradle", "cmake", "mk", "proto", "graphql", "gql", "tf", "hcl",
    "nix", "zig", "hs", "ex", "exs", "erl", "clj", "cljs", "elm", "ml", "mli", "vim", "el", "lisp", "asm", "s",
    "metal", "glsl", "hlsl", "wgsl", "cu", "cuh", "ino", "v", "sv", "tex",
];
/// Rules.swift:51
pub const CODE_TEXT_EXTENSIONS: &[&str] = &["md", "markdown", "rst", "adoc", "org", "txt", "yaml", "yml", "toml"];
/// Rules.swift:53-56
pub const CODE_FILE_NAMES: &[&str] = &[
    "Makefile", "makefile", "GNUmakefile", "Dockerfile", "Containerfile", "Justfile", "justfile", "Rakefile",
    "Gemfile", "Podfile", "Brewfile", "Procfile", "Vagrantfile", "Jenkinsfile",
];
/// Rules.swift:60-64
pub const CODE_SKIPPED_FOLDER_NAMES: &[&str] = &[
    "vendor", "Vendor", "vendored", "third_party", "third-party", "thirdparty", "3rdparty", "external", "extern",
    "deps", "build", "Build", "dist", "out", "target", "obj", "coverage", "htmlcov", "generated", "__generated__",
    "wandb", "mlruns", "lightning_logs",
];
/// Rules.swift:88-95 (extension part of `isSecret`)
const SECRET_TYPES: &[&str] = &[
    "pem", "key", "p12", "pfx", "keystore", "jks", "ppk", "kdbx", "gpg", "asc", "crt", "cer", "der", "p8",
    "mobileprovision",
];
/// Rules.swift:93 (name-prefix part of `isSecret`)
const SECRET_PREFIXES: &[&str] = &[
    "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", ".env", "secret", "credential", ".netrc", ".npmrc", ".pypirc",
    "htpasswd", ".htpasswd",
];
/// Rules.swift:150 `cameraPrefixes`
const CAMERA_PREFIXES: &[&str] = &["img", "dsc", "pxl", "mvimg", "vid", "gopr", "dji", "photo", "whatsapp", "screenshot"];

// ---------------------------------------------------------------------------------------------
// Replacement for UTType conformance (Rules.swift:113-122): our own extension tables.
// ---------------------------------------------------------------------------------------------

/// Media class decided by extension or magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Pdf,
    Image,
    Audio,
    Video,
}

/// Image: SVG is deliberately absent (text/vector, no raster decoder; macOS UTType would say image).
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif", "jxl", "cr2",
    "cr3", "nef", "arw", "dng", "orf", "rw2", "raf",
];
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma", "aiff", "aif", "aifc", "caf", "amr",
];
/// `ts`, `mts`, `m2ts` are not here: they need the MPEG-TS sniff (Rules.swift:110).
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "avi", "webm", "wmv", "flv", "f4v", "mpg", "mpeg", "mpe", "3gp", "3g2", "ogv",
    "asf", "vob", "mxf", "divx",
];

pub fn media_by_extension(ext: &str) -> Option<Media> {
    if ext == "pdf" {
        Some(Media::Pdf)
    } else if IMAGE_EXTENSIONS.contains(&ext) {
        Some(Media::Image)
    } else if AUDIO_EXTENSIONS.contains(&ext) {
        Some(Media::Audio)
    } else if VIDEO_EXTENSIONS.contains(&ext) {
        Some(Media::Video)
    } else {
        None
    }
}

/// Magic-byte table over the first bytes of a file (at least 16, ideally 32).
pub fn media_by_magic(h: &[u8]) -> Option<Media> {
    let starts = |p: &[u8]| h.starts_with(p);
    let at = |o: usize, p: &[u8]| h.len() >= o + p.len() && &h[o..o + p.len()] == p;
    if starts(b"%PDF") {
        return Some(Media::Pdf);
    }
    if starts(&[0xFF, 0xD8, 0xFF])
        || starts(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
        || starts(b"GIF87a")
        || starts(b"GIF89a")
        || starts(b"BM")
        || starts(&[b'I', b'I', 0x2A, 0x00])
        || starts(&[b'M', b'M', 0x00, 0x2A])
        || starts(&[0xFF, 0x0A])
        || starts(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' '])
    {
        // 'BM' is weak (two bytes): only trust it with a plausible BMP header size field.
        if starts(b"BM") && h.len() < 14 {
            return None;
        }
        return Some(Media::Image);
    }
    if starts(b"RIFF") {
        if at(8, b"WEBP") {
            return Some(Media::Image);
        }
        if at(8, b"WAVE") {
            return Some(Media::Audio);
        }
        if at(8, b"AVI ") {
            return Some(Media::Video);
        }
        return None;
    }
    if at(4, b"ftyp") {
        let brand = if h.len() >= 12 { &h[8..12] } else { &[][..] };
        return Some(match brand {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"avif" | b"avis" => Media::Image,
            b"M4A " | b"M4B " | b"F4A " => Media::Audio,
            _ => Media::Video,
        });
    }
    if starts(b"ID3") || starts(&[0xFF, 0xFB]) || starts(&[0xFF, 0xF3]) || starts(&[0xFF, 0xF2]) {
        return Some(Media::Audio);
    }
    if starts(b"fLaC") || starts(b"OggS") || (starts(b"FORM") && (at(8, b"AIFF") || at(8, b"AIFC"))) {
        return Some(Media::Audio);
    }
    if starts(&[0x1A, 0x45, 0xDF, 0xA3]) || starts(b"FLV\x01") || starts(&[0, 0, 1, 0xBA]) {
        return Some(Media::Video);
    }
    if starts(&[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11]) {
        return Some(Media::Video); // ASF container (wmv); wma would also match: ambiguous, call it video
    }
    None
}

/// Opens `path` for sniffing only if it is a regular file (lstat, so symlinks and special files are refused), then
/// re-checks the opened handle with fstat. On unix the open itself uses O_NONBLOCK | O_NOFOLLOW, so a FIFO swapped in
/// between the lstat and the open cannot block it.
fn open_regular(path: &Path) -> Option<File> {
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let f = opts.open(path).ok()?;
    if !f.metadata().ok()?.file_type().is_file() {
        return None;
    }
    Some(f)
}

fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    let mut f = open_regular(path)?;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(_) => return None,
        }
    }
    buf.truncate(got);
    Some(buf)
}

/// MPEG-TS packets start with 0x47 every 188 bytes (or every 192 with a 4-byte timecode, as in .m2ts / AVCHD .mts).
/// Rules.swift:130-137: read up to 600 bytes, need >= 400.
pub fn is_mpeg_transport_stream(path: &Path) -> bool {
    match read_head(path, 600) {
        Some(b) => is_mpeg_ts_bytes(&b),
        None => false,
    }
}

pub fn is_mpeg_ts_bytes(b: &[u8]) -> bool {
    if b.len() < 400 {
        return false;
    }
    (b[0] == 0x47 && b[188] == 0x47 && b[376] == 0x47) || (b[4] == 0x47 && b[196] == 0x47 && b[388] == 0x47)
}

fn lower_ext(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Rules.swift:108-123. `classify` takes the type from our tables (extension, then magic if `options.sniff_magic`).
pub fn classify(path: &Path, options: &CrawlOptions) -> Classification {
    let ext = lower_ext(path);
    if options.excluded_extensions.contains(&ext) {
        return Classification::Skip("excluded type");
    }
    // .ts / .mts are usually TypeScript, which Spotlight calls MPEG-2 video. Only a real transport stream is video.
    if ext == "ts" || ext == "mts" || ext == "m2ts" {
        if is_mpeg_transport_stream(path) {
            return Classification::Index(FileKind::Video);
        }
        return if options.include_code { Classification::Index(FileKind::Doc) } else { Classification::Skip("code or data file") };
    }
    if DOC_EXTENSIONS.contains(&ext.as_str()) {
        return Classification::Index(FileKind::Doc);
    }
    if CODE_EXTENSIONS.contains(&ext.as_str()) {
        return if options.include_code { Classification::Index(FileKind::Doc) } else { Classification::Skip("code or data file") };
    }
    let media = media_by_extension(&ext).or_else(|| {
        // Extension unknown to us: optionally trust the first bytes (costs one open+read per unknown file).
        if options.sniff_magic {
            read_head(path, 32).and_then(|h| media_by_magic(&h))
        } else {
            None
        }
    });
    match media {
        None => Classification::Ignore, // Rules.swift:117 `guard let type else { return .ignore }`
        Some(Media::Pdf) => Classification::Index(FileKind::Pdf),
        Some(Media::Image) => Classification::Index(if is_screenshot(path) { FileKind::Screenshot } else { FileKind::Image }),
        Some(Media::Video) => Classification::Index(FileKind::Video),
        Some(Media::Audio) => Classification::Index(FileKind::Audio),
    }
}

const LOCKFILE_NAMES: &[&str] = &[
    "package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "bun.lockb", "go.sum", "package.resolved",
    "cartfile.resolved", "gradle.lockfile", "packages.lock.json", "poetry.lock", "pipfile.lock",
];

/// Rules.swift:68-85.
pub fn classify_code(path: &Path, options: &CrawlOptions) -> Classification {
    let name = file_name(path);
    let ext = lower_ext(path);
    if options.excluded_extensions.contains(&ext) {
        return Classification::Skip("excluded type");
    }
    if is_secret(&name) {
        return Classification::Skip("key or password file");
    }
    let lower = name.to_lowercase();
    if lower.ends_with(".min.js") || lower.ends_with(".min.mjs") || lower.ends_with(".min.css") {
        return Classification::Skip("minified");
    }
    if ["license", "licence", "copying", "notice"].iter().any(|p| lower.starts_with(p)) {
        return Classification::Skip("license");
    }
    // Not in Rules.swift: dependency lockfiles are long machine-written lists that crowd out real code in results.
    if ext == "lock" || LOCKFILE_NAMES.contains(&lower.as_str()) {
        return Classification::Skip("lockfile");
    }
    if (ext == "ts" || ext == "mts") && is_mpeg_transport_stream(path) {
        return Classification::Ignore; // a video
    }
    if SOURCE_EXTENSIONS.contains(&ext.as_str())
        || CODE_TEXT_EXTENSIONS.contains(&ext.as_str())
        || ext == "ipynb"
        || CODE_FILE_NAMES.contains(&name.as_str())
        || name.starts_with("Dockerfile")
    {
        return Classification::Index(FileKind::Code);
    }
    if CODE_EXTENSIONS.contains(&ext.as_str()) {
        return Classification::Skip("data file");
    }
    Classification::Ignore
}

/// Rules.swift:88-95. Keys, certificates, password and credential files. NOTE: Swift applies this ONLY in the
/// code index (spec 2.1); `CrawlOptions::secrets_everywhere` extends it to the main index (a security opt-in).
pub fn is_secret(name: &str) -> bool {
    let lower = name.to_lowercase();
    // (lower as NSString).pathExtension
    let ext = match lower.rfind('.') {
        Some(i) if i > 0 && i + 1 < lower.len() => &lower[i + 1..],
        _ => "",
    };
    if SECRET_TYPES.contains(&ext) {
        return true;
    }
    SECRET_PREFIXES.iter().any(|p| lower.starts_with(p))
}

#[cfg(target_os = "macos")]
mod xattr {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    extern "C" {
        fn getxattr(path: *const c_char, name: *const c_char, value: *mut c_void, size: usize, position: u32, options: c_int) -> isize;
        #[cfg(test)]
        pub fn setxattr(path: *const c_char, name: *const c_char, value: *const c_void, size: usize, position: u32, options: c_int) -> c_int;
    }

    /// True when the file carries macOS's `kMDItemIsScreenCapture` attribute (it survives moves and copies).
    pub fn is_screen_capture(path: &Path) -> bool {
        let (Ok(p), Ok(n)) = (CString::new(path.as_os_str().as_bytes()), CString::new("com.apple.metadata:kMDItemIsScreenCapture")) else {
            return false;
        };
        // SAFETY: both strings are NUL-terminated; a null buffer with size 0 only asks for the attribute's length.
        unsafe { getxattr(p.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0, 0, 0) > 0 }
    }

    #[cfg(test)]
    pub fn mark_screen_capture(path: &Path) {
        let p = CString::new(path.as_os_str().as_bytes()).unwrap();
        let n = CString::new("com.apple.metadata:kMDItemIsScreenCapture").unwrap();
        let v = b"1";
        // SAFETY: valid NUL-terminated strings and a 1-byte buffer.
        let r = unsafe { setxattr(p.as_ptr(), n.as_ptr(), v.as_ptr().cast(), 1, 0, 0) };
        assert_eq!(r, 0, "setxattr failed");
    }
}

/// Rules.swift:138-145: the macOS `kMDItemIsScreenCapture` attribute (macOS only), then the file-name prefixes.
pub fn is_screenshot(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    if xattr::is_screen_capture(path) {
        return true;
    }
    let name = file_name(path);
    SCREENSHOT_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// `dir` is a `/`-separated folder path without a trailing slash: is it a drive or mount root?
fn is_volume_root(dir: &str) -> bool {
    let b = dir.as_bytes();
    dir.is_empty() // "/"
        || (b.len() == 2 && b[1] == b':') // "C:"
        || ["/Volumes/", "/mnt/", "/media/"].iter().any(|m| dir.strip_prefix(m).is_some_and(|r| !r.contains('/')))
}

/// Rules.swift:146-152.
pub fn skip_reason_for_folder(path: &str, home: &str, excluded: &[String]) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if SKIPPED_FOLDER_NAMES.contains(&name) {
        return Some("developer folder");
    }
    if name.ends_with(".noindex") {
        return Some(".noindex folder");
    }
    if !home.is_empty() && path == format!("{home}/Library") {
        return Some("~/Library");
    }
    // App data that is not user content: Windows %USERPROFILE%\AppData, and Snap's per-app copies on Linux.
    if !home.is_empty() && (path == format!("{home}/AppData") || path == format!("{home}/snap")) {
        return Some("app data");
    }
    let grandparent = path.rsplit('/').nth(2);
    // An old Mac's home copied to another drive (/Volumes/Old/Users/me/Library): the `home` rule cannot match it.
    if name == "Library" && grandparent == Some("Users") {
        return Some("~/Library");
    }
    // Time Machine in-flight/old snapshot folders sit directly under the backup volume's root.
    if (name.ends_with(".backup") || name.ends_with(".previous") || name.ends_with(".inprogress"))
        && is_volume_root(path[..path.len() - name.len()].trim_end_matches('/'))
    {
        return Some("backup");
    }
    if excluded.iter().any(|e| path == e || path.starts_with(&format!("{e}/"))) {
        return Some("excluded folder");
    }
    None
}

/// Rules.swift:92-100 `codeSkipReason`.
pub fn code_skip_reason_for_folder(path: &Path) -> Option<&'static str> {
    if CODE_SKIPPED_FOLDER_NAMES.contains(&file_name(path).as_str()) {
        return Some("vendored or built code");
    }
    // A submodule or a worktree (".git" is a file there).
    if let Ok(m) = std::fs::metadata(path.join(".git")) {
        if !m.is_dir() {
            return Some("submodule or worktree");
        }
    }
    None
}

/// Rules.swift:155-160, over a directory listing's names.
pub fn is_project_listing<'a, I: IntoIterator<Item = &'a str>>(names: I) -> bool {
    names.into_iter().any(|n| {
        PROJECT_MARKERS.contains(&n)
            || n.rfind('.').is_some_and(|i| i > 0 && PROJECT_BUNDLE_EXTENSIONS.contains(&&n[i + 1..]))
    })
}

/// Rules.swift:155-160 (reads the folder).
pub fn is_project_root(folder: &Path) -> bool {
    match std::fs::read_dir(folder) {
        Ok(rd) => {
            let names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            is_project_listing(names.iter().map(|s| s.as_str()))
        }
        Err(_) => false,
    }
}

static HEXISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{8,}([-_][0-9a-f]{4,})*$").unwrap());
static NUMBERED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]{0,16}[-_. ]?\d{3,}([-_.]\d+)*$").unwrap());

/// Rules.swift:152-160. Machine-made names like "clip_000123", "frame-0042" or "3f9a1c0e-...", but not "IMG_1234".
/// The Swift regexes run on ICU; the `regex` crate's `\d` is Unicode (Nd) too, so they are identical.
pub fn looks_machine_named(stem: &str) -> bool {
    let name = stem.to_lowercase();
    if CAMERA_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    HEXISH.is_match(&name) || NUMBERED.is_match(&name)
}

/// One media file for dataset detection.
#[derive(Debug, Clone)]
pub struct DatasetEntry {
    pub folder: String,
    pub stem: String,
    pub kind: FileKind,
}

/// Rules.swift:162-188. Folders with hundreds of same-kind, machine-named media files. Group = (folder, kind) over
/// image/audio/video whose folder is not a chosen root; dataset if count >= `minimum` (300) and machine/count >= 0.7.
/// Result: folder -> total files (summed over kinds).
pub fn dataset_folders(
    files: &[DatasetEntry],
    roots: &std::collections::HashSet<String>,
    minimum: usize,
) -> HashMap<String, usize> {
    let mut groups: HashMap<(&str, FileKind), Vec<&str>> = HashMap::new();
    for f in files {
        if matches!(f.kind, FileKind::Image | FileKind::Audio | FileKind::Video) && !roots.contains(&f.folder) {
            groups.entry((f.folder.as_str(), f.kind)).or_default().push(f.stem.as_str());
        }
    }
    let mut datasets = HashMap::new();
    for ((folder, _), stems) in groups {
        if stems.len() >= minimum {
            let machine = stems.iter().filter(|s| looks_machine_named(s)).count();
            if machine as f64 / stems.len() as f64 >= 0.7 {
                *datasets.entry(folder.to_string()).or_insert(0) += stems.len();
            }
        }
    }
    datasets
}

/// Default dataset minimum (Rules.swift:163).
pub const DATASET_MINIMUM: usize = 300;

#[cfg(all(test, target_os = "macos"))]
mod xattr_tests {
    use super::*;

    #[test]
    fn screen_capture_attribute_makes_a_screenshot() {
        let dir = std::env::temp_dir().join(format!("dtfu-xattr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("holiday.png");
        std::fs::write(&plain, b"x").unwrap();
        assert!(!is_screenshot(&plain));
        xattr::mark_screen_capture(&plain);
        assert!(is_screenshot(&plain));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
