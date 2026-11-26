// The menu bar of a running Radiotrope on macOS, read and used as a person
// would, through the Accessibility API: the menus and items that are there,
// their check marks, greyed items and keys, and what choosing an item does
// to them. Run by scripts/macos-menu-check.sh (CI, or by hand on a Mac):
//
//   swift scripts/macos-menu-check.swift <pid> <folder for the evidence>
//
// Exit status: 0 the menu bar is right, 1 something is wrong (each printed
// as an error), 3 this process may not use the Accessibility API (System
// Settings > Privacy & Security > Accessibility), so nothing was looked at.

import AppKit
import ApplicationServices

setvbuf(stdout, nil, _IOLBF, 0)

let arguments = CommandLine.arguments
guard arguments.count == 3, let pid = pid_t(arguments[1]) else {
    print("usage: macos-menu-check.swift <pid> <evidence folder>")
    exit(2)
}
let evidence = arguments[2]

guard AXIsProcessTrusted() else {
    print("::warning::This process may not use the Accessibility API: the menu bar was not looked at")
    exit(3)
}

var failures = 0
func expect(_ condition: Bool, _ message: String) {
    if condition {
        print("ok: \(message)")
    } else {
        print("::error::menu bar: \(message)")
        failures += 1
    }
}

/// As `expect`, for what a choice leads to: the app changes its menu a
/// moment later (it may be opening a dialog), so look until it is so, for
/// a few seconds
func expectSoon(_ condition: @autoclosure () -> Bool, _ message: @autoclosure () -> String) {
    for _ in 0..<25 where !condition() {
        RunLoop.current.run(until: Date().addingTimeInterval(0.2))
    }
    expect(condition(), message())
}

func attribute<T>(_ element: AXUIElement, _ name: String) -> T? {
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else {
        return nil
    }
    return value as? T
}

func children(_ element: AXUIElement) -> [AXUIElement] {
    attribute(element, kAXChildrenAttribute) ?? []
}

func title(_ element: AXUIElement) -> String {
    attribute(element, kAXTitleAttribute) ?? ""
}

func role(_ element: AXUIElement) -> String {
    attribute(element, kAXRoleAttribute) ?? ""
}

func enabled(_ element: AXUIElement) -> Bool {
    attribute(element, kAXEnabledAttribute) ?? false
}

func marked(_ element: AXUIElement) -> Bool {
    let mark: String = attribute(element, kAXMenuItemMarkCharAttribute) ?? ""
    return !mark.isEmpty
}

/// "⌘O", "⌥⌘H", "F1"; "" for no keys
func keys(_ element: AXUIElement) -> String {
    let character: String = attribute(element, kAXMenuItemCmdCharAttribute) ?? ""
    let virtualKey: Int? = attribute(element, kAXMenuItemCmdVirtualKeyAttribute)
    let glyph: Int? = attribute(element, kAXMenuItemCmdGlyphAttribute)
    var key = character.uppercased()
    // A function key: its character is one of AppKit's own (NSF1FunctionKey),
    // which prints as nothing. kVK_F1, kMenuF1Glyph.
    if virtualKey == 0x7A || glyph == 111 {
        key = "F1"
    }
    if key.isEmpty {
        return ""
    }
    // kAXMenuItemModifier...: 1 Shift, 2 Option, 4 Control, 8 no Command
    let modifiers: Int = attribute(element, kAXMenuItemCmdModifiersAttribute) ?? 0
    var text = ""
    if modifiers & 4 != 0 { text += "⌃" }
    if modifiers & 2 != 0 { text += "⌥" }
    if modifiers & 1 != 0 { text += "⇧" }
    if modifiers & 8 == 0 { text += "⌘" }
    return text + key
}

/// The items of a menu bar item or of an item with a submenu
func items(_ element: AXUIElement) -> [AXUIElement] {
    children(element).first.map(children) ?? []
}

let app = AXUIElementCreateApplication(pid)

func menuBar() -> [AXUIElement] {
    let bar: AXUIElement? = attribute(app, kAXMenuBarAttribute)
    return bar.map(children) ?? []
}

/// The menu with this title; the application menu is the one after Apple's
func menu(_ name: String) -> AXUIElement? {
    let bar = menuBar()
    if name == "Radiotrope" {
        return bar.count > 1 ? bar[1] : nil
    }
    return bar.first { title($0) == name }
}

/// "View", "Theme", "Dark" -> that item
func item(_ path: String...) -> AXUIElement? {
    guard var found = menu(path[0]) else { return nil }
    for name in path.dropFirst() {
        guard let next = items(found).first(where: { title($0) == name }) else { return nil }
        found = next
    }
    return found
}

func names(_ elements: [AXUIElement]) -> [String] {
    elements.map { title($0).isEmpty ? "-" : title($0) }
}

func outline(_ element: AXUIElement, depth: Int, into text: inout String) {
    for child in items(element) {
        let name = title(child)
        var line = String(repeating: "    ", count: depth)
        if name.isEmpty {
            line += "--------"
        } else {
            line += marked(child) ? "✓ " : "  "
            line += name
            if !enabled(child) { line += "  (greyed)" }
            let pressed = keys(child)
            if !pressed.isEmpty { line += "  [\(pressed)]" }
        }
        text += line + "\n"
        outline(child, depth: depth + 1, into: &text)
    }
}

func dump(_ label: String) -> String {
    var text = "== \(label)\n"
    for top in menuBar() {
        text += title(top) + "\n"
        // Not Apple's own menu
        if title(top) != "Apple" {
            outline(top, depth: 1, into: &text)
        }
    }
    return text
}

/// Let the app work: the check marks are set right a moment after a choice
func settle(_ seconds: Double = 1.0) {
    RunLoop.current.run(until: Date().addingTimeInterval(seconds))
}

func press(_ element: AXUIElement?, _ what: String) {
    guard let element = element else {
        expect(false, "\(what) is there to choose")
        return
    }
    let result = AXUIElementPerformAction(element, kAXPressAction as CFString)
    expect(result == .success, "\(what) can be chosen (AX result \(result.rawValue))")
    settle()
}

// The app may still be starting
for _ in 0..<30 where menu("Help") == nil {
    settle(1.0)
}

var log = dump("at the start")
print(log)

// The menus: Apple's, the application menu, the app's five, and Window
// before Help
let bar = names(menuBar())
expect(bar.count == 8, "eight menus in the bar: \(bar)")
expect(Array(bar.dropFirst(2)) == ["Open", "Stations", "View", "Tools", "Window", "Help"],
       "Open, Stations, View, Tools, Window, Help after the application menu: \(bar)")

// The application menu: ours, not Slint's default (which says "About
// <program>" and quits around the app's exit)
let application = menu("Radiotrope").map { names(items($0)) } ?? []
expect(application == ["About Radiotrope", "-", "Services", "-", "Hide Radiotrope", "Hide Others",
                       "Show All", "-", "Quit Radiotrope"],
       "the application menu: \(application)")
expect(item("Radiotrope", "Quit Radiotrope").map(keys) == "⌘Q", "Quit Radiotrope is ⌘Q")
expect(item("Radiotrope", "Hide Radiotrope").map(keys) == "⌘H", "Hide Radiotrope is ⌘H")
expect(item("Radiotrope", "Hide Others").map(keys) == "⌥⌘H", "Hide Others is ⌥⌘H")

expect(names(menu("Open").map(items) ?? []) == ["Network Stream"], "Open has Network Stream only")
expect(item("Open", "Network Stream").map(keys) == "⌘O", "Network Stream is ⌘O")
expect(names(menu("Stations").map(items) ?? [])
       == ["Search Stations", "Find in Favorites", "-", "Export Favorites", "Import Favorites", "-",
           "Report Play Count"], "the Stations menu")
expect(item("Stations", "Search Stations").map(keys) == "⌘F", "Search Stations is ⌘F")
expect(item("Stations", "Find in Favorites").map(keys) == "", "Find in Favorites takes no key from the text fields")
expect(names(menu("View").map(items) ?? [])
       == ["Theme", "Accent Color", "-", "Visualizer", "Station Colors in Visualizer", "-",
           "Show Listening Stats", "Zoom", "-", "System Tray"], "the View menu")
expect(names(item("View", "Theme").map(items) ?? []) == ["System", "Dark", "Light"], "View > Theme")
expect(names(item("View", "System Tray").map(items) ?? [])
       == ["Show Tray Icon", "Start Hidden in Tray", "Start at Login"], "View > System Tray")
let tools = names(menu("Tools").map(items) ?? [])
expect(tools == ["Start Recording", "Recordings", "-", "Sleep Timer", "Scheduler", "-", "Filters",
                 "Equalizer", "Statistics", "-", "Connections"], "the Tools menu: \(tools)")
expect(item("Tools", "Start Recording").map(enabled) == false, "Start Recording is greyed with no station playing")
expect(item("Tools", "Start Recording").map(keys) == "", "Start Recording takes no key from the text fields")
expect(names(item("Tools", "Recordings").map(items) ?? []) == ["Settings", "Open Folder"], "Tools > Recordings")
expect(names(item("Tools", "Connections").map(items) ?? []) == ["Agents (MCP)", "Remote Control"],
       "Tools > Connections, with nobody connected")
expect(names(menu("Window").map(items) ?? []).prefix(4) == ["Minimize", "Zoom", "-", "Bring All to Front"],
       "the Window menu")
expect(item("Window", "Minimize").map(keys) == "⌘M", "Minimize is ⌘M")
let help = names(menu("Help").map(items) ?? [])
expect(help.contains("Keyboard Shortcuts") && !help.contains("About Radiotrope"),
       "Help has Keyboard Shortcuts, and About is in the application menu: \(help)")
if let shortcuts = item("Help", "Keyboard Shortcuts") {
    let character: String? = attribute(shortcuts, kAXMenuItemCmdCharAttribute)
    let virtualKey: Int? = attribute(shortcuts, kAXMenuItemCmdVirtualKeyAttribute)
    let glyph: Int? = attribute(shortcuts, kAXMenuItemCmdGlyphAttribute)
    let modifiers: Int? = attribute(shortcuts, kAXMenuItemCmdModifiersAttribute)
    print("Keyboard Shortcuts' key as the system has it: character \(String(describing: character)), "
          + "virtual key \(String(describing: virtualKey)), glyph \(String(describing: glyph)), "
          + "modifiers \(String(describing: modifiers))")
}
expect(item("Help", "Keyboard Shortcuts").map(keys) == "F1", "Keyboard Shortcuts is F1")

// A check mark follows its setting, both ways
let visualizer = item("View", "Visualizer").map(marked) ?? false
press(item("View", "Visualizer"), "View > Visualizer")
expectSoon(item("View", "Visualizer").map(marked) == !visualizer, "Visualizer's check mark flipped")
expectSoon(item("View", "Station Colors in Visualizer").map(enabled) == !visualizer,
       "Station Colors in Visualizer is greyed while the visualizer is off")
press(item("View", "Visualizer"), "View > Visualizer again")
expectSoon(item("View", "Visualizer").map(marked) == visualizer, "Visualizer's check mark is back")

// One of a set: choosing the one in use leaves its mark
func markedNames(_ path: String...) -> [String] {
    let group = path.count == 2 ? item(path[0], path[1]) : nil
    return (group.map(items) ?? []).filter(marked).map(title)
}
let theme = markedNames("View", "Theme")
expectSoon(theme.count == 1, "one theme has the mark: \(theme)")
if let current = theme.first {
    press(item("View", "Theme", current), "View > Theme > \(current) (in use)")
    expectSoon(markedNames("View", "Theme") == [current], "the theme in use keeps its mark: \(markedNames("View", "Theme"))")
    let other = current == "Dark" ? "Light" : "Dark"
    press(item("View", "Theme", other), "View > Theme > \(other)")
    expectSoon(markedNames("View", "Theme") == [other], "\(other) has the mark now: \(markedNames("View", "Theme"))")
    press(item("View", "Theme", current), "View > Theme > \(current)")
    expectSoon(markedNames("View", "Theme") == [current], "\(current) has the mark again: \(markedNames("View", "Theme"))")
}
let zoom = markedNames("View", "Zoom")
expectSoon(zoom.count == 1, "one zoom has the mark: \(zoom)")
if let current = zoom.first {
    press(item("View", "Zoom", current), "View > Zoom > \(current) (in use)")
    expectSoon(markedNames("View", "Zoom") == [current], "the zoom in use keeps its mark: \(markedNames("View", "Zoom"))")
}

log += dump("after the check marks")

// A picture of an open menu. Opening one starts the system's menu
// tracking, so the menu is closed again before anything else.
if let view = menu("View") {
    let opened = AXUIElementPerformAction(view, kAXPressAction as CFString)
    settle(1.5)
    let capture = Process()
    capture.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
    capture.arguments = ["-x", "\(evidence)/menu-open.png"]
    try? capture.run()
    capture.waitUntilExit()
    let closed = AXUIElementPerformAction(view, kAXCancelAction as CFString)
    settle(1.0)
    print("the View menu for its picture: opened \(opened.rawValue), closed \(closed.rawValue)")
}

// Report Play Count asks first, in a dialog: no mark until the answer,
// and the other items are greyed while the dialog is open. Quit is not.
expectSoon(item("Stations", "Report Play Count").map(marked) == false, "Report Play Count is off at first")
press(item("Stations", "Report Play Count"), "Stations > Report Play Count")
expectSoon(item("Stations", "Report Play Count").map(marked) == false, "Report Play Count has no mark while it asks")
expectSoon(item("Open", "Network Stream").map(enabled) == false, "Network Stream is greyed while a dialog is open")
expectSoon(item("Radiotrope", "About Radiotrope").map(enabled) == false, "About is greyed while a dialog is open")
expectSoon(item("Window", "Minimize").map(enabled) == true, "Minimize is not greyed while a dialog is open")
expectSoon(item("Radiotrope", "Quit Radiotrope").map(enabled) == true, "Quit is not greyed while a dialog is open")

log += dump("with a dialog open")
try? log.write(toFile: "\(evidence)/menu.txt", atomically: true, encoding: .utf8)
print(dump("with a dialog open"))

// Quit in the application menu: the caller looks at what the exit left
press(item("Radiotrope", "Quit Radiotrope"), "Radiotrope > Quit Radiotrope")

exit(failures == 0 ? 0 : 1)
