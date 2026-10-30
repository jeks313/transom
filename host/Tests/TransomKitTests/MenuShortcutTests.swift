import Testing

@testable import TransomKit

@Suite("Menu shortcuts")
struct MenuShortcutTests {
    @Test("command maps through the swap to Ctrl, in Windows modifier order")
    func swap() {
        #expect(MenuShortcut.text(char: "n", modifiers: 0, namesake: false) == "Ctrl+N")
        #expect(MenuShortcut.text(char: "N", modifiers: 1, namesake: false) == "Ctrl+Shift+N")
        #expect(MenuShortcut.text(char: "h", modifiers: 2, namesake: false) == "Ctrl+Alt+H")
        // ⌃⌘N: Control is the Win key under the swap.
        #expect(MenuShortcut.text(char: "N", modifiers: 4, namesake: false) == "Ctrl+Win+N")
    }

    @Test("namesake mapping puts Command on Win")
    func namesake() {
        #expect(MenuShortcut.text(char: "c", modifiers: 0, namesake: true) == "Win+C")
        #expect(MenuShortcut.text(char: "f", modifiers: 4 | 8, namesake: true) == "Ctrl+F")
    }

    @Test("no key, or a key with no name here, shows no shortcut")
    func noKey() {
        #expect(MenuShortcut.text(char: nil, modifiers: 0, namesake: false) == "")
        #expect(MenuShortcut.text(char: "", modifiers: 0, namesake: false) == "")
        #expect(MenuShortcut.text(char: "\u{F702}", modifiers: 0, namesake: false) == "")
        #expect(MenuShortcut.text(char: ",", modifiers: 0, namesake: false) == "Ctrl+,")
        // ⌃fn F (Enter Full Screen) and a bare key name no typeable shortcut.
        #expect(MenuShortcut.text(char: "f", modifiers: 0x18 | 4, namesake: false) == "")
        #expect(MenuShortcut.text(char: "f", modifiers: 8, namesake: false) == "")
    }
}

@Suite("Menu shortcut keys")
struct MenuShortcutKeyTests {
    @Test("shortcut text becomes the keys that type it")
    func keys() {
        #expect(MenuShortcut.virtualKeys("Ctrl+Shift+B") == [0x11, 0x10, 0x42])
        #expect(MenuShortcut.virtualKeys("Ctrl+,") == [0x11, 0xBC])
        #expect(MenuShortcut.virtualKeys("Ctrl++") == [0x11, 0xBB])
        #expect(MenuShortcut.virtualKeys("Ctrl+Win+N") == [0x11, 0x5B, 0x4E])
        #expect(MenuShortcut.virtualKeys("") == nil)
        #expect(MenuShortcut.virtualKeys("Ctrl+F13") == nil)
    }

    @Test("every shortcut text the host writes can be typed back")
    func roundTrip() {
        for char in ["a", "Z", "5", ",", "/", "[", " ", "\t", "\r", "\u{1b}"] {
            for mods in [0, 1, 2, 4, 5, 8 | 4] {
                let text = MenuShortcut.text(char: char, modifiers: mods, namesake: false)
                #expect(MenuShortcut.virtualKeys(text) != nil, "\(text)")
            }
        }
    }
}
