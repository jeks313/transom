import ApplicationServices
import Foundation

/// One item of an app's menu bar as the client shows it (protocol.md §4
/// `menuBar`): a title, its state, the shortcut text in Windows terms, and its
/// submenu. A separator has `separator` set and nothing else.
public struct MenuItem: Codable, Equatable, Sendable {
    public var title: String
    public var enabled: Bool
    public var checked: Bool
    /// Shortcut as the client's keyboard types it ("Ctrl+Shift+N"), "" for none.
    public var shortcut: String
    public var separator: Bool
    /// The submenu, empty for a leaf.
    public var items: [MenuItem]

    public init(
        title: String, enabled: Bool = true, checked: Bool = false, shortcut: String = "",
        separator: Bool = false, items: [MenuItem] = []
    ) {
        self.title = title
        self.enabled = enabled
        self.checked = checked
        self.shortcut = shortcut
        self.separator = separator
        self.items = items
    }

    public static let separatorItem = MenuItem(title: "", enabled: false, separator: true)
}

/// Shortcut text for a menu item, as the client's keyboard reaches it through the
/// host's modifier map (`ModifierMap`): with the default swap, ⌘C is "Ctrl+C".
public enum MenuShortcut {
    /// `AXMenuItemCmdModifiers` bits. Command is implied unless `noCommand` is set.
    static let shift = 1, option = 2, control = 4, noCommand = 8

    /// Text for a shortcut, or "" when there is none or the key has no name here
    /// (a glyph-only key such as an arrow is left out rather than guessed).
    public static func text(char: String?, modifiers: Int?, namesake: Bool) -> String {
        guard let char, let key = keyName(char) else { return "" }
        let mods = modifiers ?? 0
        // Fn (and any other bit) has no key on the client, and a lone key with
        // no modifier is not a shortcut a keyboard can type into a window.
        guard mods & ~0xF == 0, mods & (noCommand | shift | option | control) != noCommand else {
            return ""
        }
        // The client's key that becomes each Mac modifier (protocol.md §4).
        let command = namesake ? "Win" : "Ctrl"
        let controlKey = namesake ? "Ctrl" : "Win"
        var parts: [String] = []
        if mods & control != 0 { parts.append(controlKey) }
        if mods & noCommand == 0 { parts.append(command) }
        if mods & option != 0 { parts.append("Alt") }
        if mods & shift != 0 { parts.append("Shift") }
        // Windows lists Ctrl, Win, Alt, Shift in that order.
        let order = ["Ctrl", "Win", "Alt", "Shift"]
        parts.sort { order.firstIndex(of: $0)! < order.firstIndex(of: $1)! }
        return (parts + [key]).joined(separator: "+")
    }

    /// The Windows virtual keys that type `text` (as `text(char:modifiers:namesake:)`
    /// writes it): modifiers in order, then the key. Nil for a key it cannot name.
    public static func virtualKeys(_ text: String) -> [UInt32]? {
        let parts = text.split(separator: "+", omittingEmptySubsequences: false).map(String.init)
        // "Ctrl++" names the plus key: the last part is the key, empty when it is "+".
        guard parts.count >= 2 else { return nil }
        var keys: [UInt32] = []
        for modifier in parts.dropLast() where !modifier.isEmpty {
            switch modifier {
            case "Ctrl": keys.append(0x11)
            case "Win": keys.append(0x5B)
            case "Alt": keys.append(0x12)
            case "Shift": keys.append(0x10)
            default: return nil
            }
        }
        let key = parts.last!.isEmpty ? "+" : parts.last!
        let named: [String: UInt32] = [
            "Space": 0x20, "Tab": 0x09, "Enter": 0x0D, "Esc": 0x1B,
            ";": 0xBA, "=": 0xBB, "+": 0xBB, ",": 0xBC, "-": 0xBD, ".": 0xBE, "/": 0xBF,
            "`": 0xC0, "[": 0xDB, "\\": 0xDC, "]": 0xDD, "'": 0xDE,
        ]
        if let vk = named[key] {
            keys.append(vk)
        } else if key.count == 1, let c = key.unicodeScalars.first,
            ("A"..."Z").contains(Character(c)) || ("0"..."9").contains(Character(c))
        {
            keys.append(c.value)
        } else {
            return nil
        }
        return keys
    }

    private static func keyName(_ char: String) -> String? {
        switch char {
        case "": return nil
        case " ": return "Space"
        case "\t": return "Tab"
        case "\r": return "Enter"
        case "\u{1b}": return "Esc"
        default:
            guard char.count == 1, let scalar = char.unicodeScalars.first, scalar.value >= 0x21,
                scalar.value < 0x7f
            else { return nil }
            return char.uppercased()
        }
    }
}

/// Reads and presses an app's menu bar through Accessibility. Ported in spirit
/// from Sill's `MenuReader` (Apache-2.0): the bar's items are the children of
/// `kAXMenuBarAttribute`, the first is the Apple menu, and each has one AXMenu
/// child holding the items. Reading a menu's children makes the app validate it
/// (`validateMenuItem:`), so enabled and checked states are current as read.
///
/// Items are addressed by **path**: their indices among their menu's AX children,
/// from the bar. The Apple menu is index 0 and is never sent, so the client's
/// first menu is path index 1. A press walks the path afresh and checks the
/// item's title, because an app can rebuild a menu in place between a read and
/// a press.
///
/// Every call blocks on the app (up to `timeout` per AX call); run it off the
/// watcher and network threads.
public enum MenuReader {
    static let timeout: Float = 1.0
    /// Items read of one menu; the rest are left off.
    public static let maxItems = 200
    /// Submenu levels below the bar's menus.
    public static let maxDepth = 3
    /// Seconds for a whole read; past it, menus not yet read are sent empty.
    public static let readBudget = 2.0

    /// The bar's menus but the Apple menu, with their items. Nil when the app has
    /// no menu bar (or does not answer).
    public static func read(pid: pid_t, namesake: Bool) -> [MenuItem]? {
        guard let bar = menuBar(pid: pid) else { return nil }
        let started = CFAbsoluteTimeGetCurrent()
        let tops = children(of: bar)
        var menus: [MenuItem] = []
        for top in tops.dropFirst() {
            let v = values(of: top, [kAXTitleAttribute, kAXEnabledAttribute])
            var menu = MenuItem(title: v[0] as? String ?? "", enabled: v[1] as? Bool ?? true)
            if CFAbsoluteTimeGetCurrent() - started < readBudget {
                menu.items = items(of: top, depth: 1, namesake: namesake, started: started)
            }
            menus.append(menu)
        }
        return menus
    }

    private static let itemAttributes = [
        kAXRoleAttribute, kAXTitleAttribute, kAXDescriptionAttribute, kAXEnabledAttribute,
        "AXMenuItemMarkChar", "AXMenuItemCmdChar", "AXMenuItemCmdModifiers", kAXChildrenAttribute,
    ]

    /// The items of the menu an item opens, keeping AX child order (paths are
    /// indices). A child that is not a menu item becomes a disabled separator so
    /// the indices stay aligned.
    private static func items(of opener: AXUIElement, depth: Int, namesake: Bool, started: Double)
        -> [MenuItem]
    {
        guard let kids = menuChildren(of: opener) else { return [] }
        var out: [MenuItem] = []
        for kid in kids.prefix(maxItems) {
            let v = values(of: kid, itemAttributes)
            guard (v[0] as? String) == kAXMenuItemRole else {
                out.append(.separatorItem)
                continue
            }
            let title = (v[1] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? (v[2] as? String ?? "")
            if title.isEmpty {
                out.append(.separatorItem)
                continue
            }
            var item = MenuItem(
                title: title, enabled: v[3] as? Bool ?? true,
                checked: !((v[4] as? String) ?? "").isEmpty,
                shortcut: MenuShortcut.text(
                    char: v[5] as? String, modifiers: (v[6] as? NSNumber)?.intValue, namesake: namesake))
            let hasSubmenu = !((v[7] as? [AXUIElement]) ?? []).isEmpty
            if hasSubmenu, depth < maxDepth, CFAbsoluteTimeGetCurrent() - started < readBudget {
                item.items = items(of: kid, depth: depth + 1, namesake: namesake, started: started)
            }
            if hasSubmenu, item.items.isEmpty {
                // A submenu left unread still shows as one, so its item is not a
                // pressable leaf.
                item.items = [MenuItem(title: "…", enabled: false)]
            }
            out.append(item)
        }
        return out
    }

    public enum PressResult: Equatable, Sendable {
        case pressed
        /// The app did not answer the press in time; its action is usually running
        /// (a modal dialog).
        case pressedNoAnswer
        /// Nothing pressable with that title at that path now.
        case changed
        case failed(Int32)
    }

    /// The leaf at `path` when its title is still `title` and it opens no submenu.
    public static func item(pid: pid_t, path: [Int], title: String) -> AXUIElement? {
        guard path.count >= 2, path[0] >= 1, let bar = menuBar(pid: pid) else { return nil }
        var level = children(of: bar)
        var element: AXUIElement?
        for (depth, index) in path.enumerated() {
            if depth > 0 {
                guard let opener = element, let kids = menuChildren(of: opener) else { return nil }
                level = kids
            }
            guard index < level.count else { return nil }
            element = level[index]
        }
        guard let target = element else { return nil }
        let v = values(of: target, [kAXTitleAttribute, kAXDescriptionAttribute, kAXChildrenAttribute])
        let now = (v[0] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? (v[1] as? String ?? "")
        guard now == title, ((v[2] as? [AXUIElement]) ?? []).isEmpty else { return nil }
        return target
    }

    /// An item's shortcut in the client's terms ("Ctrl+Shift+B"), "" for none.
    public static func shortcut(of item: AXUIElement, namesake: Bool) -> String {
        let v = values(of: item, ["AXMenuItemCmdChar", "AXMenuItemCmdModifiers"])
        return MenuShortcut.text(
            char: v[0] as? String, modifiers: (v[1] as? NSNumber)?.intValue, namesake: namesake)
    }

    /// AXPress an item. Its enabled flag is not checked: AppKit revalidates a menu
    /// at most about once a second, and ignores a press on an item it holds
    /// disabled. An app that enables window items only while it is frontmost
    /// (Chrome) ignores this one in the background; use its shortcut instead.
    public static func press(_ target: AXUIElement) -> PressResult {
        let started = CFAbsoluteTimeGetCurrent()
        let e = AXUIElementPerformAction(target, kAXPressAction as CFString)
        if e == .success { return .pressed }
        if e == .cannotComplete, CFAbsoluteTimeGetCurrent() - started >= Double(timeout) * 0.9 {
            return .pressedNoAnswer
        }
        return .failed(e.rawValue)
    }

    /// `press`, finding the item by path and title first.
    public static func press(pid: pid_t, path: [Int], title: String) -> PressResult {
        guard let target = item(pid: pid, path: path, title: title) else { return .changed }
        return press(target)
    }

    // MARK: Accessibility, typed

    private static func menuBar(pid: pid_t) -> AXUIElement? {
        let app = AXUIElementCreateApplication(pid)
        AXUIElementSetMessagingTimeout(app, timeout)
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(app, kAXMenuBarAttribute as CFString, &value) == .success,
            let value, CFGetTypeID(value) == AXUIElementGetTypeID()
        else { return nil }
        let bar = value as! AXUIElement
        AXUIElementSetMessagingTimeout(bar, timeout)
        return bar
    }

    /// The children of the AXMenu among an item's children, or nil when it opens
    /// no menu.
    private static func menuChildren(of item: AXUIElement) -> [AXUIElement]? {
        for kid in children(of: item) {
            AXUIElementSetMessagingTimeout(kid, timeout)
            var role: CFTypeRef?
            if AXUIElementCopyAttributeValue(kid, kAXRoleAttribute as CFString, &role) == .success,
                (role as? String) == kAXMenuRole
            {
                return children(of: kid)
            }
        }
        return nil
    }

    private static func children(of element: AXUIElement) -> [AXUIElement] {
        AXUIElementSetMessagingTimeout(element, timeout)
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXChildrenAttribute as CFString, &value) == .success
        else { return [] }
        return (value as? [AXUIElement]) ?? []
    }

    /// Several attributes in one call; a missing one is nil.
    private static func values(of element: AXUIElement, _ attributes: [String]) -> [CFTypeRef?] {
        AXUIElementSetMessagingTimeout(element, timeout)
        var array: CFArray?
        guard
            AXUIElementCopyMultipleAttributeValues(
                element, attributes as CFArray, AXCopyMultipleAttributeOptions(rawValue: 0), &array)
                == .success,
            let list = array as [AnyObject]?
        else { return Array(repeating: nil, count: attributes.count) }
        return (0..<attributes.count).map { i -> CFTypeRef? in
            guard i < list.count else { return nil }
            let v = list[i] as CFTypeRef
            if CFGetTypeID(v) == AXValueGetTypeID() || CFGetTypeID(v) == CFNullGetTypeID() {
                return nil
            }
            return v
        }
    }
}
