use super::{PdfError, Rendered};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, AnyThread, ClassType};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSString, NSURL};
use objc2_pdf_kit::{PDFDisplayBox, PDFDocument};
use std::path::Path;

/// Not `Send`/`Sync` (holds a Retained ObjC object): use from one thread at a time.
pub struct PdfDoc {
    doc: Retained<PDFDocument>,
}

impl PdfDoc {
    pub fn open(path: &Path) -> Result<PdfDoc, PdfError> {
        let p = path.to_str().ok_or_else(|| PdfError::Unreadable("non-utf8 path".into()))?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(p));
        let doc = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }
            .ok_or_else(|| PdfError::Unreadable("can't open PDF".into()))?;
        if unsafe { doc.isLocked() } {
            return Err(PdfError::Locked);
        }
        Ok(PdfDoc { doc })
    }

    pub fn page_count(&self) -> usize {
        unsafe { self.doc.pageCount() }
    }

    pub fn title(&self) -> Option<String> {
        objc2::rc::autoreleasepool(|_| unsafe {
            let attrs = self.doc.documentAttributes()?;
            let key = NSString::from_str("Title");
            let v: Option<Retained<AnyObject>> = msg_send![&*attrs, objectForKey: &*key];
            let v = v?;
            let is_str: bool = msg_send![&*v, isKindOfClass: NSString::class()];
            if !is_str {
                return None;
            }
            let s: &NSString = &*(Retained::as_ptr(&v) as *const NSString);
            let t = s.to_string();
            let t = t.trim();
            (!t.is_empty()).then(|| t.to_string())
        })
    }

    pub fn text(&self, page: usize) -> Option<String> {
        objc2::rc::autoreleasepool(|_| unsafe {
            let p = self.doc.pageAtIndex(page)?;
            Some(p.string().map(|s| s.to_string()).unwrap_or_default())
        })
    }

    pub fn render(&self, page: usize, max_side: u32) -> Result<Rendered, PdfError> {
        objc2::rc::autoreleasepool(|_| unsafe {
            let p = self
                .doc
                .pageAtIndex(page)
                .ok_or_else(|| PdfError::Unreadable("no such page".into()))?;
            let b = p.boundsForBox(PDFDisplayBox::MediaBox);
            let (bw, bh) = (b.size.width, b.size.height);
            if !(bw > 0.0 && bh > 0.0) || max_side == 0 {
                return Err(PdfError::Unreadable("empty page".into()));
            }
            // boundsForBox is the unrotated box but drawWithBox applies /Rotate itself (verified by test),
            // so only swap the output dimensions for 90/270.
            let rot = p.rotation().rem_euclid(360);
            let (pw, ph) = if rot == 90 || rot == 270 { (bh, bw) } else { (bw, bh) };
            let scale = max_side as f64 / pw.max(ph);
            let w = ((pw * scale).round() as u32).max(1);
            let h = ((ph * scale).round() as u32).max(1);
            let cs = CGColorSpace::new_device_rgb()
                .ok_or_else(|| PdfError::Unreadable("colorspace".into()))?;
            let mut buf = vec![0u8; w as usize * h as usize * 4];
            let ctx = CGBitmapContextCreate(
                buf.as_mut_ptr().cast(),
                w as usize,
                h as usize,
                8,
                w as usize * 4,
                Some(&cs),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
            .ok_or_else(|| PdfError::Unreadable("bitmap context".into()))?;
            CGContext::set_rgb_fill_color(Some(&ctx), 1.0, 1.0, 1.0, 1.0);
            CGContext::fill_rect(Some(&ctx), CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(w as f64, h as f64),
            ));
            let (sx, sy) = (w as f64 / pw, h as f64 / ph);
            CGContext::scale_ctm(Some(&ctx), sx, sy);
            CGContext::translate_ctm(Some(&ctx), -b.origin.x, -b.origin.y);
            p.drawWithBox_toContext(PDFDisplayBox::MediaBox, &ctx);
            drop(ctx);
            // bitmap rows are top-first in memory already; strip alpha
            let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
            #[allow(clippy::chunks_exact_to_as_chunks)]
            for px in buf.chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
            Ok(Rendered { width: w, height: h, rgb })
        })
    }
}
