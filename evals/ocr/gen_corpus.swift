// Renders the OCR benchmark corpus: PNG + ground-truth .txt per case (known strings, so no OCR engine is the reference).
// Usage: swiftc -O gen_corpus.swift -o /tmp/gen_corpus && /tmp/gen_corpus <out-dir> [multi]
// `multi` renders the 16-language set (evals/ocr/corpus_multi) instead of the main corpus (evals/ocr/corpus).
import AppKit
import CoreImage

struct Case { let id: String; let w: Int; let h: Int; let bg: UInt32; let fg: UInt32; let font: String; let pt: CGFloat; let lines: [String]; var rot: CGFloat = 0; var jpeg: CGFloat? = nil; var blur: Double = 0; var noise: Int = 0 }

let mainCases: [Case] = [
  Case(id: "en_body_light", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 18, lines: [
    "Quarterly planning notes", "The launch moved to November 14 because the payment provider needs a security review.",
    "Action items: Priya will update the roadmap, Mateo will draft the customer email,", "and Lin will confirm the budget of $42,500 with finance by Friday."]),
  Case(id: "en_small_ui", w: 1280, h: 720, bg: 0xF2F2F7, fg: 0x3C3C43, font: "SFNS", pt: 10, lines: [
    "File  Edit  View  Window  Help", "Sync status: 3 of 12 devices are up to date. Last sync 09:41", "Storage used 118.4 GB of 256 GB   Battery 87%   Wi-Fi: Office-5G",
    "Notifications are muted until tomorrow 8:00 AM", "Version 4.12.3 (build 20931) - check for updates"]),
  Case(id: "en_dark_mode", w: 1280, h: 720, bg: 0x1E1E1E, fg: 0xD4D4D4, font: "SFNS", pt: 14, lines: [
    "Settings > Privacy & Security", "Allow apps to access your location: Off", "Camera access requested by Zoom, Slack and Chrome",
    "Last scan: 12 October 2026, no threats found"]),
  Case(id: "code_dark", w: 1280, h: 720, bg: 0x1E1E1E, fg: 0xDCDCAA, font: "Menlo", pt: 13, lines: [
    "fn main() -> Result<(), Error> {", "    let cfg = Config::load(\"app.toml\")?;", "    for (i, item) in cfg.items.iter().enumerate() {",
    "        println!(\"{i}: {}\", item.name);", "    }", "    Ok(())", "}"]),
  Case(id: "low_contrast", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0xA6A6A6, font: "Helvetica", pt: 16, lines: [
    "Receipt number 88213-AB", "Total paid: 1,249.00 EUR", "Thank you for shopping with us"]),
  Case(id: "zh_hans_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x222222, font: "PingFang SC", pt: 18, lines: [
    "周末计划", "周六上午去海边散步，中午吃海鲜，下午回家整理照片。", "产品发布推迟到十一月，原因是支付系统出现问题。"]),
  Case(id: "zh_hant_small", w: 1280, h: 720, bg: 0xFAFAFA, fg: 0x333333, font: "PingFang TC", pt: 11, lines: [
    "會議紀要", "與會者：王小明、陳大文、林美玲", "決議：下週三前完成預算審查，並通知所有部門。"]),
  Case(id: "ar_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "GeezaPro", pt: 20, lines: [
    "مرحبا بكم في التطبيق", "تم تأجيل الإطلاق إلى شهر نوفمبر بسبب مشكلة في نظام الدفع."]),
  Case(id: "mixed_menu", w: 1280, h: 720, bg: 0x2B2B2B, fg: 0xEEEEEE, font: "PingFang SC", pt: 13, lines: [
    "Settings 设置", "Language 语言: 简体中文", "Display 显示 - Brightness 亮度 70%", "Contact: support@example.com  https://example.com/help"]),
  Case(id: "ru_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 18, lines: [
    "План на неделю", "Встреча с командой в среду в 10:30, отчёт нужно отправить до пятницы."]),
  Case(id: "table_numbers", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x111111, font: "Helvetica", pt: 14, lines: [
    "Invoice 2026-0417    Due 2026-11-02", "Widget A    12 x 3.50 = 42.00", "Widget B    7 x 11.25 = 78.75", "Total (incl. 8% tax)    130.14"]),
  Case(id: "retina_small_dark", w: 2560, h: 1440, bg: 0x121212, fg: 0xBBBBBB, font: "SFNS", pt: 20, lines: [
    "Terminal - zsh - 120x32", "user01@mac ~ % git status", "On branch main, your branch is up to date with origin/main.", "nothing to commit, working tree clean"]),
  // Hard variants (added after the first review): tiny text, JPEG artefacts, slight rotation, a simulated screen photo.
  Case(id: "tiny_8pt_light", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x222222, font: "Helvetica", pt: 8, lines: [
    "Terms: payment is due within 30 days of the invoice date.", "Support hours Mon-Fri 09:00-17:30 CET, ticket #48213 opened 2026-10-03.", "Reference code QX-7741-ZB, balance 2,310.55 USD"]),
  Case(id: "tiny_9pt_dark", w: 1280, h: 720, bg: 0x1B1B1B, fg: 0xC8C8C8, font: "Menlo", pt: 9, lines: [
    "$ docker ps --format '{{.Names}}'", "api-1  db-1  cache-1", "$ curl -s localhost:8080/health", "{\"status\":\"ok\",\"uptime\":86412}"]),
  Case(id: "jpeg_q15_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 15, lines: [
    "Shipping confirmation", "Order 55-2910 left the warehouse on Tuesday and arrives by Friday.", "Tracking number: 1Z999AA10123456784"], jpeg: 0.15),
  Case(id: "rotated_1_5deg", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 16, lines: [
    "Meeting moved to room 4B", "Bring the signed forms and a photo ID.", "Doors close at 9:05 sharp."], rot: 1.5),
  Case(id: "rotated_neg2_dark", w: 1280, h: 720, bg: 0x202124, fg: 0xE8EAED, font: "SFNS", pt: 14, lines: [
    "Reminder: dentist appointment", "Thursday 15 October, 14:20", "Clinic: 22 Harbour Road, second floor"], rot: -2),
  Case(id: "low_contrast_small", w: 1280, h: 720, bg: 0xF5F5F5, fg: 0xB0B0B0, font: "Helvetica", pt: 11, lines: [
    "Draft - not for distribution", "Last edited by Anna K. yesterday at 18:47", "3 comments, 2 unresolved"]),
  Case(id: "screen_photo", w: 1280, h: 720, bg: 0xE9ECEF, fg: 0x2A2A2A, font: "Helvetica", pt: 17, lines: [
    "Boarding pass", "Flight LH 717  Gate B32  Seat 14C", "Departure 07:55  Boarding 07:15"], rot: 1, jpeg: 0.4, blur: 1.1, noise: 6000),
]

// Scripts and languages beyond the main corpus (added to compare OCR engines on multilingual text).
let multiCases: [Case] = [
  Case(id: "ja_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Hiragino Sans", pt: 18, lines: [
    "週末の予定", "土曜日の朝は海辺を散歩して、昼に寿司を食べます。", "新製品の発売は支払いシステムの不具合で十一月に延期されました。"]),
  Case(id: "ja_kana_small", w: 1280, h: 720, bg: 0xF5F5F5, fg: 0x333333, font: "Hiragino Sans", pt: 11, lines: [
    "ファイルを保存しました", "ダウンロードフォルダにコピーしますか？", "キャンセル　　OK"]),
  Case(id: "ko_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Apple SD Gothic Neo", pt: 18, lines: [
    "회의 일정", "다음 주 수요일 오후 세 시에 팀 회의가 있습니다.", "보고서는 금요일까지 제출해 주세요."]),
  Case(id: "hi_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Kohinoor Devanagari", pt: 20, lines: [
    "सप्ताह की योजना", "बुधवार को सुबह दस बजे टीम के साथ बैठक है।", "रिपोर्ट शुक्रवार तक भेजनी है।"]),
  Case(id: "bn_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Kohinoor Bangla", pt: 20, lines: [
    "সাপ্তাহিক পরিকল্পনা", "বুধবার সকাল দশটায় দলের সাথে সভা আছে।"]),
  Case(id: "ta_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Tamil Sangam MN", pt: 20, lines: [
    "வாராந்திர திட்டம்", "புதன்கிழமை காலை பத்து மணிக்கு கூட்டம் உள்ளது."]),
  Case(id: "th_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Thonburi", pt: 20, lines: [
    "แผนประจำสัปดาห์", "ประชุมทีมวันพุธเวลาสิบโมงเช้า", "กรุณาส่งรายงานภายในวันศุกร์"]),
  Case(id: "he_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Arial Hebrew", pt: 20, lines: [
    "ברוכים הבאים לאפליקציה", "ההשקה נדחתה לחודש נובמבר בגלל בעיה במערכת התשלומים."]),
  Case(id: "fa_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "GeezaPro", pt: 20, lines: [
    "برنامه هفتگی", "جلسه تیم روز چهارشنبه ساعت ده صبح برگزار می‌شود."]),
  Case(id: "el_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 18, lines: [
    "Πρόγραμμα εβδομάδας", "Η συνάντηση της ομάδας είναι την Τετάρτη στις 10:30.", "Η αναφορά πρέπει να σταλεί μέχρι την Παρασκευή."]),
  Case(id: "uk_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 18, lines: [
    "План на тиждень", "Зустріч із командою в середу о 10:30, звіт треба надіслати до п’ятниці."]),
  Case(id: "vi_body", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 18, lines: [
    "Kế hoạch tuần này", "Cuộc họp nhóm vào thứ Tư lúc 10 giờ 30 sáng.", "Báo cáo phải được gửi trước thứ Sáu."]),
  Case(id: "de_fr_es", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 16, lines: [
    "Größe: 42 – Überweisung bis Freitag, Grüße aus München", "Réunion déplacée à jeudi, à côté de l'hôpital Saint-Étienne", "Año nuevo: mañana pagaré la cuenta del señor Muñoz"]),
  Case(id: "pl_cs_tr", w: 1280, h: 720, bg: 0xFFFFFF, fg: 0x1A1A1A, font: "Helvetica", pt: 16, lines: [
    "Zażółć gęślą jaźń, spotkanie w środę", "Příliš žluťoučký kůň úpěl ďábelské ódy", "Toplantı çarşamba günü, şirket ağı güncellendi"]),
  Case(id: "ko_dark_small", w: 1280, h: 720, bg: 0x1E1E1E, fg: 0xD4D4D4, font: "Apple SD Gothic Neo", pt: 12, lines: [
    "설정 > 개인정보 보호", "위치 접근 허용: 끔", "마지막 검사: 2026년 10월 12일"]),
  Case(id: "ja_en_mixed", w: 1280, h: 720, bg: 0x2B2B2B, fg: 0xEEEEEE, font: "Hiragino Sans", pt: 13, lines: [
    "Settings 設定", "Language 言語: 日本語", "Display 画面 - Brightness 明るさ 70%"]),
]

let cases = CommandLine.arguments.count > 2 && CommandLine.arguments[2] == "multi" ? multiCases : mainCases

func color(_ rgb: UInt32) -> NSColor {
  NSColor(srgbRed: CGFloat((rgb >> 16) & 255) / 255, green: CGFloat((rgb >> 8) & 255) / 255, blue: CGFloat(rgb & 255) / 255, alpha: 1)
}

let out = URL(fileURLWithPath: CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "corpus")
try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
for c in cases {
  var rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: c.w, pixelsHigh: c.h, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
  NSGraphicsContext.saveGraphicsState()
  NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
  color(c.bg).setFill(); NSRect(x: 0, y: 0, width: c.w, height: c.h).fill()
  let font = NSFont(name: c.font, size: c.pt) ?? NSFont.systemFont(ofSize: c.pt)
  let para = NSMutableParagraphStyle(); para.alignment = (c.id.hasPrefix("ar_") || c.id.hasPrefix("he_") || c.id.hasPrefix("fa_")) ? .right : .left
  let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color(c.fg), .paragraphStyle: para]
  if c.rot != 0 {
    let t = NSAffineTransform(); t.translateX(by: CGFloat(c.w) / 2, yBy: CGFloat(c.h) / 2); t.rotate(byDegrees: c.rot); t.translateX(by: -CGFloat(c.w) / 2, yBy: -CGFloat(c.h) / 2); t.concat()
  }
  var y = CGFloat(c.h) - 60 - c.pt
  let margin: CGFloat = 60
  for line in c.lines {
    (line as NSString).draw(in: NSRect(x: margin, y: y, width: CGFloat(c.w) - 2 * margin, height: c.pt * 1.6), withAttributes: attrs)
    y -= c.pt * 2.2
  }
  NSGraphicsContext.restoreGraphicsState()

  if c.noise > 0 {
    var g = SystemRandomNumberGenerator()
    for _ in 0..<c.noise {
      let x = Int.random(in: 0..<c.w, using: &g), y = Int.random(in: 0..<c.h, using: &g), v = Int.random(in: 60...200, using: &g)
      rep.setColor(NSColor(deviceRed: CGFloat(v) / 255, green: CGFloat(v) / 255, blue: CGFloat(v) / 255, alpha: 1), atX: x, y: y)
    }
  }
  if c.blur > 0, let ci = CIImage(bitmapImageRep: rep), let f = CIFilter(name: "CIGaussianBlur") {
    f.setValue(ci, forKey: kCIInputImageKey); f.setValue(c.blur, forKey: kCIInputRadiusKey)
    if let o = f.outputImage?.cropped(to: ci.extent) { rep = NSBitmapImageRep(ciImage: o) }
  }
  if let q = c.jpeg, let d = rep.representation(using: .jpeg, properties: [.compressionFactor: q]), let r2 = NSBitmapImageRep(data: d) { rep = r2 }
  try rep.representation(using: .png, properties: [:])!.write(to: out.appendingPathComponent("\(c.id).png"))
  try (c.lines.joined(separator: "\n") + "\n").write(to: out.appendingPathComponent("\(c.id).txt"), atomically: true, encoding: .utf8)
  print("wrote \(c.id) (font \(font.fontName))")
}
