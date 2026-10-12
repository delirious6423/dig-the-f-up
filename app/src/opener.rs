//! Opens a search hit where it is: a PDF page with the query's words to search for (Preview), a recording at its moment
//! (QuickTime Player, IINA). Everything else opens the plain way. Port of Swift `Opener` (Moments.swift).
//! The app-specific parts are macOS only; elsewhere every hit opens plainly.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use std::path::Path;
#[cfg(target_os = "macos")]
use std::sync::mpsc;

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

pub const QUICKTIME: &str = "com.apple.QuickTimePlayerX";
pub const IINA: &str = "com.colliderli.iina";

/// osascript for QuickTime Player (same script as Moments.swift); the file and the time in whole ms come as arguments.
#[cfg(target_os = "macos")]
const QUICKTIME_SCRIPT: &str = r#"on run argv
    set theFile to POSIX file (item 1 of argv)
    set theTime to (item 2 of argv as integer) / 1000
    tell application "QuickTime Player"
        activate
        set theDocument to open theFile
        set current time of theDocument to theTime
        try
            set miniaturized of (first window whose document is theDocument) to false
        end try
        play theDocument
        return current time of theDocument
    end tell
end run"#;

/// What a hit asks of its app.
#[derive(Debug, PartialEq)]
pub enum Request {
    /// The file's default app opens it, nothing more.
    Plain,
    /// A PDF: the default app opens it with this text to search for (keyAESearchText).
    Search(String),
    /// A recording: IINA opens it at this many seconds (iina:// URL).
    Iina(f64),
    /// A recording: QuickTime Player goes to this many seconds (osascript).
    QuickTime(f64),
}

/// A PDF page (1-based) with the text of the pages before it: searches for the first query word on that page, or
/// plain when there is none (always plain for page 1, as `landmark_find` says).
pub fn pdf_request(page: usize, texts: &[String], query: &str) -> Request {
    let words: Vec<String> = query.split_whitespace().map(str::to_string).collect();
    match dtfu_text::landmark_find(page, texts, &words) {
        Some(word) => Request::Search(word),
        None => Request::Plain,
    }
}

/// A recording at `secs`: a moment only counts from one second on, and only IINA and QuickTime Player take one.
pub fn media_request(app_id: Option<&str>, secs: f64) -> Request {
    if secs.is_nan() || secs < 1.0 {
        return Request::Plain;
    }
    match app_id {
        Some(IINA) => Request::Iina(secs),
        Some(QUICKTIME) => Request::QuickTime(secs),
        _ => Request::Plain,
    }
}

/// The IINA URL that starts `path` at `secs` (Swift `iinaLink`: the file's URL, and the time with one decimal).
/// None when `path` is not absolute.
pub fn iina_url(path: &Path, secs: f64) -> Option<String> {
    let file = url::Url::from_file_path(path).ok()?;
    let mut link = url::Url::parse("iina://open").ok()?;
    link.query_pairs_mut().append_pair("url", file.as_str()).append_pair("mpv_start", &format!("{secs:.1}"));
    Some(link.to_string())
}

/// osascript's arguments for `on run argv`: the path, then the time as whole milliseconds (truncated, as Swift does).
pub fn quicktime_args(path: &Path, secs: f64) -> [String; 2] {
    [path.to_string_lossy().into_owned(), ((secs * 1000.0) as i64).to_string()]
}

/// Opens `path` for a hit. `kind` and `loc` are the hit's (a PDF's page, a recording's seconds); `query` is the search
/// text. Blocks until the open was handed to the app, so call it off the main thread (AppKit work runs on it).
/// Any failure opens the file the plain way.
pub fn open_at(app: &AppHandle, path: &Path, kind: &str, loc: Option<f64>, query: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // PDFKit reads the page texts here, on the calling thread; the AppKit calls go to the main thread.
        let pdf = match (kind, loc) {
            ("pdf", Some(page)) if page >= 2.0 => pdf_request_for(path, page as usize, query),
            _ => Request::Plain,
        };
        let (tx, rx) = mpsc::channel();
        let (handle, owned, kind) = (app.clone(), path.to_path_buf(), kind.to_string());
        app.run_on_main_thread(move || {
            let request = match pdf {
                Request::Plain => match (kind.as_str(), loc) {
                    ("audio" | "video", Some(secs)) => media_request(mac::default_app_id(&owned).as_deref(), secs),
                    _ => Request::Plain,
                },
                search => search,
            };
            let _ = tx.send(run(&handle, &owned, request));
        })
        .map_err(|e| e.to_string())?;
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(r) => r,
            Err(_) => plain(app, path),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (kind, loc, query);
        plain(app, path)
    }
}

#[cfg(target_os = "macos")]
fn pdf_request_for(path: &Path, page: usize, query: &str) -> Request {
    let Ok(doc) = dtfu_extract_pdf::PdfDoc::open(path) else {
        return Request::Plain;
    };
    if doc.page_count() < page {
        return Request::Plain;
    }
    let texts: Vec<String> = (0..page).map(|i| doc.text(i).unwrap_or_default()).collect();
    pdf_request(page, &texts, query)
}

/// Carries out `request` (main thread). Whatever can't be done the app's way opens the plain way.
#[cfg(target_os = "macos")]
fn run(app: &AppHandle, path: &Path, request: Request) -> Result<(), String> {
    let done = match &request {
        Request::Plain => false,
        Request::Search(words) => mac::open_with_search(path, words),
        Request::Iina(secs) => {
            iina_url(path, *secs).is_some_and(|link| app.opener().open_url(link, None::<&str>).is_ok())
        }
        Request::QuickTime(secs) => {
            // Off the main thread: osascript waits for QuickTime (and for macOS's permission question).
            let (app, path, args) = (app.clone(), path.to_path_buf(), quicktime_args(path, *secs));
            std::thread::spawn(move || {
                let ok = std::process::Command::new("/usr/bin/osascript")
                    .args(["-e", QUICKTIME_SCRIPT])
                    .args(&args)
                    .output()
                    .is_ok_and(|out| out.status.success());
                if !ok {
                    let _ = plain(&app, &path);
                }
            });
            true
        }
    };
    if done { Ok(()) } else { plain(app, path) }
}

fn plain(app: &AppHandle, path: &Path) -> Result<(), String> {
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::Path;

    use objc2::rc::Retained;
    use objc2::AnyThread;
    use objc2_app_kit::{NSWorkspace, NSWorkspaceOpenConfiguration};
    use objc2_foundation::{NSAppleEventDescriptor, NSBundle, NSString, NSURL};

    /// A four-character code, as the Apple Event Manager has it.
    const fn code(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }

    /// keyAESearchText ('stxt', AERegistry.h): the text Preview fills its search with.
    pub const KEY_AE_SEARCH_TEXT: u32 = code(b"stxt");

    fn file_url(path: &Path) -> Option<Retained<NSURL>> {
        Some(NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?)))
    }

    /// The app that opens `url` (NSWorkspace's default for the file).
    fn default_app(url: &NSURL) -> Option<Retained<NSURL>> {
        NSWorkspace::sharedWorkspace().URLForApplicationToOpenURL(url)
    }

    /// The bundle id of the file's default app.
    pub fn default_app_id(path: &Path) -> Option<String> {
        let url = file_url(path)?;
        let app = default_app(&url)?;
        let bundle = NSBundle::bundleWithURL(&app)?;
        Some(bundle.bundleIdentifier()?.to_string())
    }

    /// Asks the file's default app to open the file with `words` as its search text: the Open Documents event carrying
    /// keyAESearchText, and nothing else (given the file as well, NSWorkspace sends its own event without the search).
    /// False when there is no app or the event can't be made; the caller then opens the file plainly.
    pub fn open_with_search(path: &Path, words: &str) -> bool {
        let Some(url) = file_url(path) else {
            return false;
        };
        let Some(app) = default_app(&url) else {
            return false;
        };
        let event = NSAppleEventDescriptor::initWithEventClass_eventID_targetDescriptor_returnID_transactionID(
            NSAppleEventDescriptor::alloc(),
            code(b"aevt"), // kCoreEventClass
            code(b"odoc"), // kAEOpenDocuments
            None,
            -1, // kAutoGenerateReturnID
            0,  // kAnyTransactionID
        );
        let files = NSAppleEventDescriptor::listDescriptor();
        // Apple Event lists are 1-based (Swift's insert(at: 0)).
        files.insertDescriptor_atIndex(&NSAppleEventDescriptor::descriptorWithFileURL(&url), 1);
        event.setParamDescriptor_forKeyword(&files, code(b"----")); // keyDirectObject
        event.setParamDescriptor_forKeyword(
            &NSAppleEventDescriptor::descriptorWithString(&NSString::from_str(words)),
            KEY_AE_SEARCH_TEXT,
        );
        let configuration = NSWorkspaceOpenConfiguration::configuration();
        configuration.setAppleEvent(Some(&event));
        NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(&app, &configuration, None);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn texts() -> Vec<String> {
        vec!["Contents and intro".into(), "The tiger sleeps here".into(), "Other page".into()]
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn search_text_key_is_stxt() {
        assert_eq!(mac::KEY_AE_SEARCH_TEXT, 0x7374_7874);
    }

    #[test]
    fn page_one_asks_for_nothing() {
        assert_eq!(pdf_request(1, &texts(), "tiger"), Request::Plain);
    }

    #[test]
    fn no_landmark_asks_for_nothing() {
        assert_eq!(pdf_request(3, &texts(), "tiger"), Request::Plain);
        assert_eq!(pdf_request(2, &texts(), ""), Request::Plain);
    }

    #[test]
    fn landmark_on_the_page_is_searched_for() {
        assert_eq!(pdf_request(2, &texts(), "zebra tiger"), Request::Search("tiger".into()));
    }

    #[test]
    fn moments_need_a_second_and_a_known_app() {
        assert_eq!(media_request(Some(IINA), 0.5), Request::Plain);
        assert_eq!(media_request(Some(IINA), f64::NAN), Request::Plain);
        assert_eq!(media_request(None, 30.0), Request::Plain);
        assert_eq!(media_request(Some("com.example.player"), 30.0), Request::Plain);
        assert_eq!(media_request(Some(IINA), 30.0), Request::Iina(30.0));
        assert_eq!(media_request(Some(QUICKTIME), 12.5), Request::QuickTime(12.5));
    }

    #[test]
    fn iina_url_carries_file_and_start() {
        let path = PathBuf::from("/Users/x/Music/a b&c.m4a");
        let link = iina_url(&path, 12.4).expect("absolute path");
        let parsed = url::Url::parse(&link).unwrap();
        assert_eq!(parsed.scheme(), "iina");
        assert_eq!(parsed.host_str(), Some("open"));
        let pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
        assert!(pairs.contains(&("url".into(), "file:///Users/x/Music/a%20b&c.m4a".into())));
        assert!(pairs.contains(&("mpv_start".into(), "12.4".into())));
        assert_eq!(iina_url(Path::new("relative/a.mp3"), 1.0), None);
    }

    #[test]
    fn iina_start_has_one_decimal() {
        let link = iina_url(Path::new("/a/b.mp3"), 75.0).unwrap();
        assert!(link.ends_with("mpv_start=75.0"), "{link}");
    }

    #[test]
    fn quicktime_gets_whole_milliseconds() {
        assert_eq!(quicktime_args(Path::new("/a/b.mp4"), 12.3456), ["/a/b.mp4".to_string(), "12345".to_string()]);
        assert_eq!(quicktime_args(Path::new("/a/b.mp4"), 1.0)[1], "1000");
    }
}
