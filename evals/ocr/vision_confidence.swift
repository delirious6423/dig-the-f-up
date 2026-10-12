// Prints Apple Vision's mean and minimum line confidence per corpus image (the data behind dtfu-ocr's
// VISION_MIN_CONFIDENCE). Usage: swiftc -O vision_confidence.swift -o /tmp/vc && /tmp/vc evals/ocr/corpus evals/ocr/corpus_multi
import Vision
import AppKit
for dir in CommandLine.arguments.dropFirst() {
  let fm = FileManager.default
  for f in (try! fm.contentsOfDirectory(atPath: dir)).sorted() where f.hasSuffix(".png") {
    let img = NSImage(contentsOfFile: dir + "/" + f)!
    var r = CGRect(origin: .zero, size: img.size)
    let cg = img.cgImage(forProposedRect: &r, context: nil, hints: nil)!
    let req = VNRecognizeTextRequest(); req.recognitionLevel = .accurate; req.usesLanguageCorrection = true; req.automaticallyDetectsLanguage = true
    try! VNImageRequestHandler(cgImage: cg).perform([req])
    let cs = (req.results ?? []).compactMap { $0.topCandidates(1).first?.confidence }
    let mean = cs.isEmpty ? -1 : cs.reduce(0, +) / Float(cs.count)
    print(String(format: "%-22@ n=%2d mean=%.2f min=%.2f", f as NSString, cs.count, mean, cs.min() ?? -1))
  }
}
