// Renders the OCR benchmark corpus: PNG + ground-truth .txt per case (known strings, so no OCR engine is the reference).
// Usage: swiftc -O gen_corpus.swift -o /tmp/gen_corpus && /tmp/gen_corpus <out-dir>
import AppKit

struct Case { let id: String; let w: Int; let h: Int; let bg: UInt32; let fg: UInt32; let font: String; let pt: CGFloat; let lines: [String] }

let cases: [Case] = [
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
    "Terminal - zsh - 120x32", "yoimac@mac ~ % git status", "On branch main, your branch is up to date with origin/main.", "nothing to commit, working tree clean"]),
]

func color(_ rgb: UInt32) -> NSColor {
  NSColor(srgbRed: CGFloat((rgb >> 16) & 255) / 255, green: CGFloat((rgb >> 8) & 255) / 255, blue: CGFloat(rgb & 255) / 255, alpha: 1)
}

let out = URL(fileURLWithPath: CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "corpus")
try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
for c in cases {
  let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: c.w, pixelsHigh: c.h, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
  NSGraphicsContext.saveGraphicsState()
  NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
  color(c.bg).setFill(); NSRect(x: 0, y: 0, width: c.w, height: c.h).fill()
  let font = NSFont(name: c.font, size: c.pt) ?? NSFont.systemFont(ofSize: c.pt)
  let para = NSMutableParagraphStyle(); para.alignment = c.id.hasPrefix("ar_") ? .right : .left
  let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color(c.fg), .paragraphStyle: para]
  var y = CGFloat(c.h) - 60 - c.pt
  let margin: CGFloat = 60
  for line in c.lines {
    (line as NSString).draw(in: NSRect(x: margin, y: y, width: CGFloat(c.w) - 2 * margin, height: c.pt * 1.6), withAttributes: attrs)
    y -= c.pt * 2.2
  }
  NSGraphicsContext.restoreGraphicsState()
  try rep.representation(using: .png, properties: [:])!.write(to: out.appendingPathComponent("\(c.id).png"))
  try (c.lines.joined(separator: "\n") + "\n").write(to: out.appendingPathComponent("\(c.id).txt"), atomically: true, encoding: .utf8)
  print("wrote \(c.id) (font \(font.fontName))")
}
