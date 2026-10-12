use super::text_problem::page_text_problem as check;

fn make_pdf(pages: &[&str]) -> Vec<u8> {
    // objects: 1 catalog, 2 pages, 3 font, then per page (page, content)
    let n = pages.len();
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!(
            "<< /Type /Pages /Count {n} /Kids [{}] >>",
            (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect::<Vec<_>>().join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    for (i, t) in pages.iter().enumerate() {
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * i
        ));
        let s = format!("BT /F1 36 Tf 20 100 Td ({t}) Tj ET");
        objs.push(format!("<< /Length {} >>\nstream\n{s}\nendstream", s.len()));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offs = vec![];
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let x = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in offs {
        out.extend(format!("{o:010} 00000 n \n").bytes());
    }
    out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).bytes());
    out
}

fn write(name: &str, bytes: &[u8]) -> (tempdir::D, std::path::PathBuf) {
    let d = tempdir::D::new();
    let p = d.0.join(name);
    std::fs::write(&p, bytes).unwrap();
    (d, p)
}

mod tempdir {
    pub struct D(pub std::path::PathBuf);
    impl D {
        pub fn new() -> D {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "digup-pdf-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&p).unwrap();
            D(p)
        }
    }
    impl Drop for D {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// 400x200 page, black rect on the left half, with /Rotate.
fn make_rotated_pdf(rot: i32) -> Vec<u8> {
    let s = "0 g 0 0 200 200 re f";
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_string(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] /Rotate {rot} /Contents 4 0 R >>"),
        format!("<< /Length {} >>\nstream\n{s}\nendstream", s.len()),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offs = vec![];
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let x = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in offs {
        out.extend(format!("{o:010} 00000 n \n").bytes());
    }
    out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).bytes());
    out
}

/// Backend-generic tests. `$doc` is the `PdfDoc` under test; the tests return early (with a note)
/// when `$avail` is false, e.g. when no pdfium library can be bound.
macro_rules! backend_tests {
    ($name:ident, $doc:path, $avail:expr) => {
        mod $name {
            use super::{make_pdf, make_rotated_pdf, write};
            use crate::PdfError;
            use $doc as PdfDoc;

            fn skip() -> bool {
                if !$avail {
                    eprintln!("{}: skipped, no pdfium library (set DIGUP_PDFIUM_DIR)", stringify!($name));
                }
                !$avail
            }

            #[test]
            fn text_count_render() {
                if skip() {
                    return;
                }
                let (_d, p) = write("a.pdf", &make_pdf(&["Hello penguin", "Second page"]));
                let doc = PdfDoc::open(&p).unwrap();
                assert_eq!(doc.page_count(), 2);
                assert!(doc.text(0).unwrap().contains("penguin"));
                assert!(doc.text(1).unwrap().contains("Second"));
                assert!(doc.text(2).is_none());
                let _ = doc.title();
                let r = doc.render(0, 600).unwrap();
                assert_eq!((r.width, r.height), (600, 400));
                assert_eq!(r.rgb.len(), 600 * 400 * 3);
                assert!(r.rgb.iter().any(|&b| b < 128), "no dark pixels");
                assert!(r.rgb.contains(&255), "no white background");
            }

            #[test]
            fn rotated_pages_render_upright() {
                if skip() {
                    return;
                }
                // (rotate, expect_portrait, black side: 't','b','l','r')
                for (rot, portrait, side) in [(0, false, 'l'), (90, true, 't'), (180, false, 'r'), (270, true, 'b')] {
                    let (_d, p) = write("r.pdf", &make_rotated_pdf(rot));
                    let doc = PdfDoc::open(&p).unwrap();
                    let r = doc.render(0, 200).unwrap();
                    assert_eq!(r.width.max(r.height), 200, "rot {rot}");
                    assert_eq!(r.width < r.height, portrait, "rot {rot} dims {}x{}", r.width, r.height);
                    let (w, h) = (r.width as usize, r.height as usize);
                    let dark = |x: usize, y: usize| r.rgb[(y * w + x) * 3] < 64;
                    let (a, b) = match side {
                        't' => ((w / 2, h / 4), (w / 2, 3 * h / 4)),
                        'b' => ((w / 2, 3 * h / 4), (w / 2, h / 4)),
                        'l' => ((w / 4, h / 2), (3 * w / 4, h / 2)),
                        _ => ((3 * w / 4, h / 2), (w / 4, h / 2)),
                    };
                    assert!(dark(a.0, a.1), "rot {rot}: expected black at {side}");
                    assert!(!dark(b.0, b.1), "rot {rot}: expected white opposite {side}");
                }
            }

            #[test]
            fn garbage_is_unreadable() {
                if skip() {
                    return;
                }
                let (_d, p) = write("g.pdf", b"this is not a pdf at all");
                assert!(matches!(PdfDoc::open(&p), Err(PdfError::Unreadable(_))));
            }


        }
    };
}

#[cfg(target_os = "macos")]
backend_tests!(pdfkit, crate::PdfDoc, true);

#[cfg(any(not(target_os = "macos"), feature = "pdfium"))]
backend_tests!(pdfium_backend, crate::pdfium::PdfDoc, crate::pdfium::available());

#[cfg(any(not(target_os = "macos"), feature = "pdfium"))]
#[test]
fn pdfium_missing_library_is_unsupported() {
    use crate::PdfError;
    if crate::pdfium::available() {
        eprintln!("pdfium_missing_library_is_unsupported: skipped, pdfium library bound");
        return;
    }
    let (_d, p) = write("m.pdf", &make_pdf(&["x"]));
    match crate::pdfium::PdfDoc::open(&p) {
        Err(PdfError::Unsupported(m)) => assert!(m.contains("DIGUP_PDFIUM_DIR"), "{m}"),
        other => panic!("expected Unsupported, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn problem_cases() {
    let glyph = "বািড় ভাড়ার চিপ ধানমি ৭ নর রােডর িতন বডরেমর াট মািসক ২৫,০০০ টাকা ভাড়ায় দওয়া হেলা। ভাড়ায়া দুই মােসর";
    let bengali = "বাড়ি ভাড়ার চুক্তিপত্র। ধানমন্ডি ৭ নম্বর রোডের তিন বেডরুমের ফ্ল্যাটটি মাসিক ২৫,০০০ টাকা ভাড়ায় দেওয়া হলো।";
    let lost = "ارتفعت ا.بيعات بنسبة خمسة عشر با.ئة في الربع الثالث. زرنا مدينة البتراء ا?ثرية في جنوب ا?ردن.";
    let arabic = "ارتفعت المبيعات بنسبة خمسة عشر بالمئة في الربع الثالث. زرنا مدينة البتراء الأثرية في جنوب الأردن.";
    assert_eq!(check(glyph).as_deref(), Some("glyphOrder"));
    assert_eq!(check(bengali), None);
    assert_eq!(check(lost).as_deref(), Some("lostLetters"));
    assert_eq!(check(arabic), None);
    assert_eq!(check("Revenue grew 20%. See p.4?No. Done."), None);
    assert_eq!(check("أ.د.محمد علي، أستاذ في الجامعة. د.خالد و د.سارة من شركة ذ.م.م للتجارة"), None);
    assert_eq!(check(&"\u{F0B7} Item one of the slide\n".repeat(6)), None);
}
