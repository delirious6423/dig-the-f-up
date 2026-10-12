use dtfu_extract_video::*;
use std::path::Path;
use std::process::Command;

fn make(dir: &Path, name: &str, args: &[&str]) -> Option<std::path::PathBuf> {
    let ff = match find_tool("ffmpeg") {
        Some(f) => f,
        None => {
            println!("ffmpeg missing: skipping");
            return None;
        }
    };
    let out = dir.join(name);
    let st = Command::new(ff).args(["-v", "error", "-y"]).args(args).arg(&out).status().ok()?;
    assert!(st.success());
    Some(out)
}

const TEST: &[&str] = &["-f", "lavfi", "-i", "testsrc=duration=10:size=320x240:rate=10", "-pix_fmt", "yuv420p"];

#[test]
fn probe_and_times() {
    let d = tempfile::tempdir().unwrap();
    let Some(p) = make(d.path(), "a.mp4", TEST) else { return };
    let i = probe(&p).unwrap();
    assert!((i.duration_secs - 10.0).abs() < 0.2, "{i:?}");
    assert!(!i.has_audio);
    assert_eq!((i.width, i.height), (320, 240));
    let f = frames(&p, 3.0, 160).unwrap();
    // testsrc changes constantly, so all 4 frames should survive dedup.
    let times: Vec<f64> = f.iter().map(|x| x.time_secs).collect();
    assert_eq!(times, vec![0.5, 3.5, 6.5, 9.5]);
    assert!(f.iter().all(|x| (x.width, x.height) == (160, 120) && x.rgb.len() == 160 * 120 * 3));
}

#[test]
fn static_video_dedups() {
    let d = tempfile::tempdir().unwrap();
    let Some(p) = make(
        d.path(),
        "s.mp4",
        &["-f", "lavfi", "-i", "color=c=blue:size=160x120:rate=5:duration=10", "-pix_fmt", "yuv420p"],
    ) else {
        return;
    };
    let f = frames(&p, 3.0, 1024).unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].time_secs, 0.5);
    assert_eq!((f[0].width, f[0].height), (160, 120));
}

#[test]
fn long_static_video_keeps_one_frame() {
    let d = tempfile::tempdir().unwrap();
    let Some(p) = make(
        d.path(),
        "long.mp4",
        &["-f", "lavfi", "-i", "color=c=green:size=160x120:rate=5:duration=60", "-pix_fmt", "yuv420p"],
    ) else {
        return;
    };
    // 60 s at a 3 s interval gives 20 sampled times; a static picture must keep exactly one.
    let f = frames(&p, 3.0, 160).unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].time_secs, 0.5);
    assert_eq!(f[0].rgb.len(), 160 * 120 * 3);
}

#[test]
fn rotated_video() {
    let d = tempfile::tempdir().unwrap();
    let mut args = TEST.to_vec();
    args.extend(["-metadata:s:v", "rotate=90"]);
    let Some(p) = make(d.path(), "r.mp4", &args) else { return };
    let i = probe(&p).unwrap();
    if (i.width, i.height) != (240, 320) {
        println!("rotate metadata not honoured by this ffmpeg: skipping");
        return;
    }
    let f = frames(&p, 3.0, 1024).unwrap();
    assert!(f.iter().all(|x| (x.width, x.height) == (240, 320)));
}

#[test]
fn missing_file() {
    if find_tool("ffprobe").is_none() {
        return;
    }
    assert!(matches!(frames(Path::new("/nonexistent/x.mp4"), 3.0, 512), Err(VideoError::Unreadable(_))));
    assert!(matches!(probe(Path::new("/nonexistent/x.mp4")), Err(VideoError::Unreadable(_))));
}
