// Text on a screendump, for scripts/internal/vm_drive.py: prints one line per
// piece of text Vision recognises, "x y w h text", in the image's pixels with
// the origin at the top left. vm_drive.py compiles this once (swiftc) and runs
// it on each screendump to tell which pane the guest shows and where its
// buttons are.
//
//   vm_ocr IMAGE.png
import AppKit
import Vision

guard CommandLine.arguments.count == 2,
      let image = NSImage(contentsOfFile: CommandLine.arguments[1]),
      let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil)
else {
    FileHandle.standardError.write("usage: vm_ocr IMAGE.png\n".data(using: .utf8)!)
    exit(2)
}
let w = CGFloat(cg.width), h = CGFloat(cg.height)
let request = VNRecognizeTextRequest()
request.recognitionLevel = .accurate
request.usesLanguageCorrection = false
try VNImageRequestHandler(cgImage: cg, options: [:]).perform([request])
for o in request.results ?? [] {
    guard let t = o.topCandidates(1).first else { continue }
    let b = o.boundingBox
    print(String(format: "%d %d %d %d ", Int(b.minX * w), Int((1 - b.maxY) * h),
                 Int(b.width * w), Int(b.height * h)) + t.string)
}
