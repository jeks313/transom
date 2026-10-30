import ApplicationServices
import ArgumentParser
import Foundation
import TransomKit

/// `menus <app>`: print the app's menu bar as `serve` sends it, with how long the
/// read took, or press one item by path.
struct Menus: ParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "menus",
        abstract: "Print an app's menu bar as the client gets it, or press one item.")

    @Argument(help: "Target app: a name like \"Safari\" or a bundle id.")
    var app: String

    @Option(name: .long, help: "Press the item at this path, like 1/4 (indices as printed).")
    var press: String?

    @Option(name: .long, help: "The title the pressed item must have.")
    var title: String?

    @Flag(name: .long, help: "Show shortcuts with the namesake modifier map (Ctrl→Control).")
    var namesakeModifiers: Bool = false

    func run() throws {
        guard AXIsProcessTrusted() else { throw ProbeError("Accessibility is not granted.") }
        let target: TargetApp
        switch AppResolver.resolve(app) {
        case .success(let t): target = t
        case .failure(let e): throw e
        }
        if let press {
            let path = press.split(separator: "/").compactMap { Int($0) }
            let result = MenuReader.press(pid: target.pid, path: path, title: title ?? "")
            print("press \(press) \"\(title ?? "")\": \(result)")
            return
        }
        let started = CFAbsoluteTimeGetCurrent()
        guard let menus = MenuReader.read(pid: target.pid, namesake: namesakeModifiers) else {
            throw ProbeError("\(target.name) has no menu bar.")
        }
        let ms = (CFAbsoluteTimeGetCurrent() - started) * 1000
        func dump(_ items: [MenuItem], path: [Int], depth: Int) {
            for (i, item) in items.enumerated() {
                let p = path + [depth == 0 ? i + 1 : i]
                if item.separator { continue }
                let state = (item.enabled ? "" : " [off]") + (item.checked ? " [x]" : "")
                let key = item.shortcut.isEmpty ? "" : "  \(item.shortcut)"
                print(String(repeating: "  ", count: depth) + "\(p.map(String.init).joined(separator: "/")) \(item.title)\(key)\(state)")
                dump(item.items, path: p, depth: depth + 1)
            }
        }
        // The client's first menu is path index 1 (the Apple menu, 0, is not sent).
        dump(menus, path: [], depth: 0)
        func count(_ items: [MenuItem]) -> Int { items.reduce(items.count) { $0 + count($1.items) } }
        let json = (try? JSONEncoder().encode(menus))?.count ?? 0
        print(String(format: "%d menus, %d items, read in %.0f ms, %d bytes as JSON", menus.count, count(menus), ms, json))
    }
}
