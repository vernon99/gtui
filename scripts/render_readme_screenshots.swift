import AppKit
import Foundation
import WebKit

struct Shot {
    let elementId: String
    let outputPath: String
}

final class Renderer: NSObject, WKNavigationDelegate {
    private let fixtureUrl: URL
    private let readAccessUrl: URL
    private var shots: [Shot]
    private let webView: WKWebView
    private let window: NSWindow

    init(fixturePath: String, shots: [Shot]) {
        fixtureUrl = URL(fileURLWithPath: fixturePath)
        readAccessUrl = URL(fileURLWithPath: fixturePath).deletingLastPathComponent().deletingLastPathComponent()
        self.shots = shots

        let frame = NSRect(x: 0, y: 0, width: 1400, height: 1752)
        let configuration = WKWebViewConfiguration()
        webView = WKWebView(frame: frame, configuration: configuration)
        window = NSWindow(
            contentRect: frame,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )

        super.init()

        webView.navigationDelegate = self
        window.contentView = webView
        window.setFrameOrigin(NSPoint(x: -10000, y: -10000))
    }

    func start() {
        window.orderFrontRegardless()
        webView.loadFileURL(fixtureUrl, allowingReadAccessTo: readAccessUrl)
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) {
            self.captureNext()
        }
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        fail("Failed to load screenshot fixture: \(error.localizedDescription)")
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        fail("Failed to load screenshot fixture: \(error.localizedDescription)")
    }

    private func captureNext() {
        guard !shots.isEmpty else {
            NSApp.terminate(nil)
            return
        }

        let shot = shots.removeFirst()
        let script = """
        (() => {
          const element = document.getElementById('\(shot.elementId)');
          if (!element) {
            throw new Error('Missing screenshot element: \(shot.elementId)');
          }
          const rect = element.getBoundingClientRect();
          return {
            x: rect.left,
            y: rect.top,
            width: rect.width,
            height: rect.height
          };
        })()
        """

        webView.evaluateJavaScript(script) { result, error in
            if let error {
                self.fail("Could not measure #\(shot.elementId): \(error.localizedDescription)")
                return
            }

            guard
                let rect = result as? [String: Any],
                let x = rect["x"] as? Double,
                let y = rect["y"] as? Double,
                let width = rect["width"] as? Double,
                let height = rect["height"] as? Double
            else {
                self.fail("Could not read bounds for #\(shot.elementId).")
                return
            }

            let configuration = WKSnapshotConfiguration()
            configuration.rect = CGRect(x: x, y: y, width: width, height: height)

            self.webView.takeSnapshot(with: configuration) { image, error in
                if let error {
                    self.fail("Could not capture #\(shot.elementId): \(error.localizedDescription)")
                    return
                }

                guard let image else {
                    self.fail("Could not capture #\(shot.elementId): empty snapshot.")
                    return
                }

                self.writePng(image, to: shot.outputPath)
                self.captureNext()
            }
        }
    }

    private func writePng(_ image: NSImage, to outputPath: String) {
        guard
            let tiff = image.tiffRepresentation,
            let bitmap = NSBitmapImageRep(data: tiff),
            let png = bitmap.representation(using: .png, properties: [:])
        else {
            fail("Could not encode PNG: \(outputPath)")
            return
        }

        do {
            try png.write(to: URL(fileURLWithPath: outputPath), options: [.atomic])
        } catch {
            fail("Could not write \(outputPath): \(error.localizedDescription)")
        }
    }

    private func fail(_ message: String) {
        fputs("\(message)\n", stderr)
        NSApp.terminate(nil)
        exit(1)
    }
}

let args = CommandLine.arguments
guard args.count == 4 else {
    fputs("Usage: render_readme_screenshots.swift <fixture.html> <task-spine.png> <mayor-chat.png>\n", stderr)
    exit(2)
}

let app = NSApplication.shared
app.setActivationPolicy(.prohibited)

let renderer = Renderer(
    fixturePath: args[1],
    shots: [
        Shot(elementId: "shot-task-spine", outputPath: args[2]),
        Shot(elementId: "shot-mayor-chat", outputPath: args[3]),
    ]
)
renderer.start()
app.run()
