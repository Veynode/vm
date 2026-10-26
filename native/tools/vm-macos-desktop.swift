// vm-macos-desktop: the desktop and accessibility tools of macOS containers,
// run in the Guest over SSH by `veynodevm __desktop` (the first boot grants SSH
// sessions Accessibility). One method per run: arguments as a JSON object on
// stdin, the result (or {"error": ...}) as JSON on stdout. Positions are in
// screen pixels, the coordinate space of the Screen Sharing screenshots.
//
// usage: vm-macos-desktop desktop_state|desktop_apps|window_action|app_launch|
//                         accessibility_tree|accessibility_action < arguments
import AppKit
import ApplicationServices
import Carbon

@_silgen_name("_AXUIElementGetWindow")
func axWindowID(_ element: AXUIElement, _ id: UnsafeMutablePointer<CGWindowID>) -> AXError

struct Failure: Error {
    let message: String
}

let maxVisited = 6000
let maxChildren = 500
let maxText = 4000
let deadline = Date().addingTimeInterval(8)

// MARK: Screen

let display = CGMainDisplayID()
let scale: Double = {
    guard let mode = CGDisplayCopyDisplayMode(display), mode.width > 0 else { return 1 }
    return Double(mode.pixelWidth) / Double(mode.width)
}()
let screenPoints = CGDisplayBounds(display)

func px(_ value: CGFloat) -> Int { Int((Double(value) * scale).rounded()) }
func points(_ value: Int) -> CGFloat { CGFloat(Double(value) / scale) }

func box(_ rect: CGRect) -> [String: Int] {
    let x = px(rect.minX), y = px(rect.minY), width = px(rect.width), height = px(rect.height)
    return ["x": x, "y": y, "width": width, "height": height, "center_x": x + width / 2, "center_y": y + height / 2]
}

/// The screen minus the menu bar and the Dock, in top-left points.
func workArea() -> CGRect {
    guard let screen = NSScreen.screens.first else { return screenPoints }
    let visible = screen.visibleFrame
    return CGRect(x: visible.minX, y: screen.frame.height - visible.maxY, width: visible.width, height: visible.height)
}

// MARK: Accessibility values

func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else { return nil }
    return value
}

func string(_ element: AXUIElement, _ name: String) -> String? {
    (attribute(element, name) as? String).flatMap { $0.isEmpty ? nil : $0 }
}

func flag(_ element: AXUIElement, _ name: String) -> Bool? {
    (attribute(element, name) as? NSNumber)?.boolValue
}

func axValue(_ value: Any?) -> AXValue? {
    guard let value, CFGetTypeID(value as CFTypeRef) == AXValueGetTypeID() else { return nil }
    return (value as! AXValue)
}

func point(_ value: Any?) -> CGPoint? {
    var out = CGPoint.zero
    guard let value = axValue(value), AXValueGetValue(value, .cgPoint, &out) else { return nil }
    return out
}

func size(_ value: Any?) -> CGSize? {
    var out = CGSize.zero
    guard let value = axValue(value), AXValueGetValue(value, .cgSize, &out) else { return nil }
    return out
}

func frame(_ element: AXUIElement) -> CGRect? {
    guard let origin = point(attribute(element, kAXPositionAttribute)),
          let size = size(attribute(element, kAXSizeAttribute)) else { return nil }
    return CGRect(origin: origin, size: size)
}

func children(_ element: AXUIElement) -> [AXUIElement] {
    var count: CFIndex = 0
    guard AXUIElementGetAttributeValueCount(element, kAXChildrenAttribute as CFString, &count) == .success, count > 0 else { return [] }
    var values: CFArray?
    guard AXUIElementCopyAttributeValues(element, kAXChildrenAttribute as CFString, 0, min(count, maxChildren), &values) == .success else { return [] }
    return (values as? [AXUIElement]) ?? []
}

func settable(_ element: AXUIElement, _ name: String) -> Bool {
    var result = DarwinBoolean(false)
    return AXUIElementIsAttributeSettable(element, name as CFString, &result) == .success && result.boolValue
}

func set(_ element: AXUIElement, _ name: String, _ value: CFTypeRef) -> AXError {
    AXUIElementSetAttributeValue(element, name as CFString, value)
}

/// "AXStaticText" → "static text", "AXCloseButton" → "close button".
func words(_ name: String) -> String {
    let bare = name.hasPrefix("AX") ? String(name.dropFirst(2)) : name
    var out = ""
    for (i, c) in bare.enumerated() {
        if c.isUppercase, i > 0 { out.append(" ") }
        out.append(contentsOf: c.lowercased())
    }
    return out
}

/// Action names: AXPress → "press"; custom actions carry "Name:…" first.
func actionName(_ raw: String) -> String {
    if raw.hasPrefix("Name:") {
        return String(raw.dropFirst(5).prefix { $0 != "\n" })
    }
    return words(raw)
}

func actions(_ element: AXUIElement) -> [String] {
    var names: CFArray?
    guard AXUIElementCopyActionNames(element, &names) == .success else { return [] }
    return (names as? [String]) ?? []
}

func lastLines(_ text: String) -> String {
    var lines = text.split(separator: "\n", omittingEmptySubsequences: false)
    while let last = lines.last, last.trimmingCharacters(in: .whitespaces).isEmpty { lines.removeLast() }
    let tail = lines.suffix(40).joined(separator: "\n")
    return String(tail.suffix(maxText))
}

// MARK: Applications and windows

struct App {
    let running: NSRunningApplication
    var pid: pid_t { running.processIdentifier }
    var element: AXUIElement { AXUIElementCreateApplication(pid) }
    var name: String { running.localizedName ?? running.bundleIdentifier ?? "\(pid)" }
}

func apps() -> [App] {
    NSWorkspace.shared.runningApplications
        .filter { $0.activationPolicy == .regular && !$0.isTerminated }
        .map(App.init)
}

func frontPID() -> pid_t? {
    let system = AXUIElementCreateSystemWide()
    if let app = attribute(system, kAXFocusedApplicationAttribute) {
        var pid: pid_t = 0
        if AXUIElementGetPid(app as! AXUIElement, &pid) == .success { return pid }
    }
    return NSWorkspace.shared.frontmostApplication?.processIdentifier
}

struct Window {
    let id: CGWindowID
    let app: App
    let element: AXUIElement
}

func windows(of app: App) -> [Window] {
    ((attribute(app.element, kAXWindowsAttribute) as? [AXUIElement]) ?? []).compactMap { element in
        var id: CGWindowID = 0
        guard axWindowID(element, &id) == .success, id != 0 else { return nil }
        return Window(id: id, app: app, element: element)
    }
}

func describe(_ window: Window, front: pid_t?) -> [String: Any] {
    let element = window.element
    let minimized = flag(element, kAXMinimizedAttribute) ?? false
    let main = flag(element, kAXMainAttribute) ?? false
    let active = window.app.pid == front && main && !minimized
    var states: [String] = []
    if active { states.append("focused") }
    if main { states.append("main") }
    if minimized { states.append("minimized") }
    if flag(element, "AXFullScreen") == true { states.append("fullscreen") }
    if flag(element, kAXModalAttribute) == true { states.append("modal") }
    if window.app.running.isHidden { states.append("hidden") }
    var out: [String: Any] = [
        "id": Int(window.id), "title": string(element, kAXTitleAttribute) ?? "", "app": window.app.name,
        "pid": Int(window.app.pid), "type": words(string(element, kAXSubroleAttribute) ?? "AXWindow"),
        "states": states, "active": active, "minimized": minimized,
    ]
    if let bundle = window.app.running.bundleIdentifier { out["bundle_id"] = bundle }
    if let rect = frame(element) {
        let b = box(rect)
        for key in ["x", "y", "width", "height"] { out[key] = b[key] }
    }
    return out
}

func findWindow(_ id: CGWindowID) throws -> Window {
    for app in apps() {
        if let window = windows(of: app).first(where: { $0.id == id }) { return window }
    }
    throw Failure(message: "no window \(id); read desktop_state again")
}

func activate(_ app: App) {
    _ = set(app.element, kAXFrontmostAttribute, kCFBooleanTrue)
    app.running.activate()
}

func desktopState() -> [String: Any] {
    let front = frontPID()
    var all: [Window] = []
    for app in apps() { all += windows(of: app) }
    // Front-to-back order of the windows on screen, then the others.
    let onScreen = (CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? [])
        .compactMap { $0[kCGWindowNumber as String] as? Int }
    let rank = Dictionary(onScreen.enumerated().map { ($1, $0) }, uniquingKeysWith: { a, _ in a })
    all.sort { (rank[Int($0.id)] ?? Int.max) < (rank[Int($1.id)] ?? Int.max) }
    var result: [String: Any] = [
        "coordinate_space": "screen-pixels",
        "screen": ["width": px(screenPoints.width), "height": px(screenPoints.height)],
        "scale": scale,
        "work_area": box(workArea()).filter { !$0.key.hasPrefix("center") },
        "windows": all.map { describe($0, front: front) },
    ]
    if let location = CGEvent(source: nil)?.location {
        result["cursor"] = ["x": px(location.x), "y": px(location.y)]
    }
    if let source = TISCopyCurrentKeyboardInputSource()?.takeRetainedValue() {
        var keyboard: [String: String] = [:]
        if let id = TISGetInputSourceProperty(source, kTISPropertyInputSourceID) {
            keyboard["layout"] = Unmanaged<CFString>.fromOpaque(id).takeUnretainedValue() as String
        }
        if let name = TISGetInputSourceProperty(source, kTISPropertyLocalizedName) {
            keyboard["name"] = Unmanaged<CFString>.fromOpaque(name).takeUnretainedValue() as String
        }
        result["keyboard"] = keyboard
    }
    if let front, let app = apps().first(where: { $0.pid == front }) {
        var focused: [String: Any] = ["pid": Int(front), "name": app.name]
        if let bundle = app.running.bundleIdentifier { focused["bundle_id"] = bundle }
        if let window = attribute(app.element, kAXFocusedWindowAttribute) {
            var id: CGWindowID = 0
            if axWindowID(window as! AXUIElement, &id) == .success {
                focused["window"] = Int(id)
                focused["title"] = string(window as! AXUIElement, kAXTitleAttribute) ?? ""
            }
        }
        result["focused_app"] = focused
    }
    return result
}

func desktopApps() -> [String: Any] {
    let manager = FileManager.default
    let home = manager.homeDirectoryForCurrentUser.path
    let folders = ["/Applications", "/Applications/Utilities", "/System/Applications",
                   "/System/Applications/Utilities", home + "/Applications"]
    let running = Set(NSWorkspace.shared.runningApplications.compactMap(\.bundleIdentifier))
    var seen = Set<String>()
    var list: [[String: Any]] = []
    for folder in folders {
        for entry in (try? manager.contentsOfDirectory(atPath: folder)) ?? [] where entry.hasSuffix(".app") {
            let path = folder + "/" + entry
            guard let bundle = Bundle(path: path), let id = bundle.bundleIdentifier, seen.insert(id).inserted else { continue }
            let info = bundle.localizedInfoDictionary ?? [:]
            let name = info["CFBundleDisplayName"] as? String ?? bundle.infoDictionary?["CFBundleDisplayName"] as? String
                ?? bundle.infoDictionary?["CFBundleName"] as? String ?? String(entry.dropLast(4))
            var app: [String: Any] = ["id": id, "name": name, "path": path, "running": running.contains(id)]
            if let category = bundle.infoDictionary?["LSApplicationCategoryType"] as? String {
                app["categories"] = [category.replacingOccurrences(of: "public.app-category.", with: "")]
            }
            list.append(app)
        }
    }
    list.sort { ($0["name"] as! String).localizedCaseInsensitiveCompare($1["name"] as! String) == .orderedAscending }
    return ["apps": list]
}

func windowAction(_ args: [String: Any]) throws -> [String: Any] {
    guard let id = args["window"] as? Int, let action = args["action"] as? String else {
        throw Failure(message: "window and action required")
    }
    let window = try findWindow(CGWindowID(id))
    let element = window.element
    var error = AXError.success
    var settle = 0.15
    switch action {
    case "activate":
        if flag(element, kAXMinimizedAttribute) == true { _ = set(element, kAXMinimizedAttribute, kCFBooleanFalse) }
        activate(window.app)
        _ = set(element, kAXMainAttribute, kCFBooleanTrue)
        error = AXUIElementPerformAction(element, kAXRaiseAction as CFString)
    case "close":
        guard let button = attribute(element, kAXCloseButtonAttribute) else { throw Failure(message: "window \(id) has no close button") }
        error = AXUIElementPerformAction(button as! AXUIElement, kAXPressAction as CFString)
    case "minimize":
        error = set(element, kAXMinimizedAttribute, kCFBooleanTrue)
        settle = 0.6
    case "maximize":
        let area = workArea()
        var origin = area.origin, size = area.size
        _ = set(element, kAXPositionAttribute, AXValueCreate(.cgPoint, &origin)!)
        error = set(element, kAXSizeAttribute, AXValueCreate(.cgSize, &size)!)
    case "unmaximize":
        throw Failure(message: "macOS windows have no maximized state to leave; use move_resize")
    case "fullscreen", "unfullscreen":
        error = set(element, "AXFullScreen", (action == "fullscreen" ? kCFBooleanTrue : kCFBooleanFalse)!)
        settle = 1.0
    case "move_resize":
        let current = frame(element) ?? .zero
        if args["x"] != nil || args["y"] != nil {
            var origin = CGPoint(x: (args["x"] as? Int).map(points) ?? current.minX,
                                 y: (args["y"] as? Int).map(points) ?? current.minY)
            error = set(element, kAXPositionAttribute, AXValueCreate(.cgPoint, &origin)!)
        }
        if args["width"] != nil || args["height"] != nil {
            var size = CGSize(width: (args["width"] as? Int).map(points) ?? current.width,
                              height: (args["height"] as? Int).map(points) ?? current.height)
            error = set(element, kAXSizeAttribute, AXValueCreate(.cgSize, &size)!)
        }
    default:
        throw Failure(message: "unknown window action")
    }
    if error != .success {
        throw Failure(message: "\(action) failed: AXError \(error.rawValue)")
    }
    // Apps apply requests asynchronously; report what they did.
    Thread.sleep(forTimeInterval: settle)
    var result: [String: Any] = ["requested": true, "window": id, "action": action]
    result["window_after"] = (try? findWindow(CGWindowID(id))).map { describe($0, front: frontPID()) } ?? NSNull()
    return result
}

func appLaunch(_ args: [String: Any]) throws -> [String: Any] {
    guard let id = args["app"] as? String else { throw Failure(message: "app required") }
    guard let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: id) else {
        throw Failure(message: "no application \(id); see desktop_apps")
    }
    let before = Set(apps().filter { $0.running.bundleIdentifier == id }.flatMap { windows(of: $0).map(\.id) })
    let open = Process()
    open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
    open.arguments = ["-b", id]
    try open.run()
    open.waitUntilExit()
    if open.terminationStatus != 0 { throw Failure(message: "open -b \(id) failed") }
    var result: [String: Any] = ["launched": true, "app": id, "name": FileManager.default.displayName(atPath: url.path), "window": NSNull()]
    // The app's first new window, activated so keyboard input reaches it.
    let until = Date().addingTimeInterval(5)
    while Date() < until {
        for app in apps() where app.running.bundleIdentifier == id {
            result["pid"] = Int(app.pid)
            if let window = windows(of: app).first(where: { !before.contains($0.id) }) ?? (before.isEmpty ? nil : windows(of: app).first) {
                activate(app)
                _ = AXUIElementPerformAction(window.element, kAXRaiseAction as CFString)
                Thread.sleep(forTimeInterval: 0.15)
                result["window"] = describe(window, front: frontPID())
                return result
            }
        }
        Thread.sleep(forTimeInterval: 0.1)
    }
    result["note"] = "no new window within 5 s; it may still appear (use screen_wait or desktop_state) or the application runs without one"
    return result
}

// MARK: Accessibility tree

struct Node {
    let element: AXUIElement
    let pid: pid_t
    let path: [Int]
    let depth: Int
    let context: [String]
}

let batchAttributes = [kAXRoleAttribute, kAXSubroleAttribute, kAXTitleAttribute, kAXDescriptionAttribute,
                       kAXValueAttribute, kAXPositionAttribute, kAXSizeAttribute, kAXFocusedAttribute,
                       kAXEnabledAttribute, kAXSelectedAttribute, kAXExpandedAttribute, kAXMinimizedAttribute,
                       kAXMainAttribute, kAXIdentifierAttribute, kAXPlaceholderValueAttribute] as [String]

/// The element as a tree node (role empty when it is gone) and whether it is on screen.
func detail(_ node: Node, app: App?) -> (out: [String: Any], showing: Bool, role: String) {
    var values: CFArray?
    AXUIElementCopyMultipleAttributeValues(node.element, batchAttributes as CFArray, [], &values)
    var map: [String: Any] = [:]
    for (name, value) in zip(batchAttributes, (values as? [Any]) ?? []) {
        // Attributes the element lacks come back as AXValue errors.
        if let value = axValue(value), AXValueGetType(value) == .axError { continue }
        map[name] = value
    }
    let rawRole = map[kAXRoleAttribute] as? String ?? ""
    var out: [String: Any] = ["depth": node.depth, "role": words(rawRole)]
    if let subrole = map[kAXSubroleAttribute] as? String { out["subrole"] = words(subrole) }
    let title = (map[kAXTitleAttribute] as? String).flatMap { $0.isEmpty ? nil : $0 }
    let description = (map[kAXDescriptionAttribute] as? String).flatMap { $0.isEmpty ? nil : $0 }
    if let name = title ?? description ?? (node.depth == 0 ? app?.name : nil) { out["name"] = name }
    var states: [String] = []
    switch map[kAXValueAttribute] {
    case let text as String where !text.isEmpty:
        out["text"] = rawRole == kAXTextAreaRole ? lastLines(text) : String(text.prefix(maxText))
    case let number as NSNumber:
        if [kAXCheckBoxRole, kAXRadioButtonRole, "AXSwitch", kAXMenuItemRole].contains(rawRole) {
            if number.intValue == 1 { states.append("checked") }
            if number.intValue == 2 { states.append("mixed") }
        } else {
            out["value"] = number.doubleValue
        }
    default:
        break
    }
    if let placeholder = map[kAXPlaceholderValueAttribute] as? String, !placeholder.isEmpty { out["placeholder"] = placeholder }
    if let identifier = map[kAXIdentifierAttribute] as? String, !identifier.isEmpty { out["identifier"] = identifier }
    if (map[kAXFocusedAttribute] as? NSNumber)?.boolValue == true { states.append("focused") }
    if (map[kAXEnabledAttribute] as? NSNumber)?.boolValue == false { states.append("disabled") }
    if (map[kAXSelectedAttribute] as? NSNumber)?.boolValue == true { states.append("selected") }
    if let expanded = (map[kAXExpandedAttribute] as? NSNumber)?.boolValue { states.append(expanded ? "expanded" : "collapsed") }
    if (map[kAXMinimizedAttribute] as? NSNumber)?.boolValue == true { states.append("minimized") }
    if (map[kAXMainAttribute] as? NSNumber)?.boolValue == true { states.append("main") }
    if [kAXTextFieldRole, kAXTextAreaRole, kAXComboBoxRole].contains(rawRole), settable(node.element, kAXValueAttribute) {
        states.append("editable")
    }
    if !states.isEmpty { out["states"] = states }
    let names = actions(node.element).map(actionName)
    if !names.isEmpty { out["actions"] = names }
    var showing = node.depth == 0
    if let origin = point(map[kAXPositionAttribute]), let size = size(map[kAXSizeAttribute]) {
        let rect = CGRect(origin: origin, size: size)
        out["box"] = box(rect)
        showing = showing || (size.width > 0 && size.height > 0 && rect.intersects(screenPoints)
            && (map[kAXMinimizedAttribute] as? NSNumber)?.boolValue != true)
    }
    if node.depth == 0 {
        // Application elements report no meaningful frame or states.
        out["pid"] = Int(node.pid)
        out["box"] = nil
        out["states"] = nil
    }
    out["ref"] = ref(pid: node.pid, path: node.path, role: rawRole, key: key(node.element))
    return (out, showing, rawRole)
}

func matches(_ out: [String: Any], name: String?, role: String?) -> Bool {
    if let role, (out["role"] as? String)?.lowercased() != role.lowercased(),
       (out["subrole"] as? String)?.lowercased() != role.lowercased() { return false }
    if let name {
        let haystack = [out["name"], out["text"], out["placeholder"]].compactMap { $0 as? String }
        if !haystack.contains(where: { $0.localizedCaseInsensitiveContains(name) }) { return false }
    }
    return true
}

func accessibilityTree(_ args: [String: Any]) throws -> [String: Any] {
    let maxDepth = args["max_depth"] as? Int ?? 25, maxNodes = args["max_nodes"] as? Int ?? 400
    let includeHidden = args["include_hidden"] as? Bool ?? false
    let name = args["name"] as? String, role = args["role"] as? String
    let find = name != nil || role != nil
    let front = frontPID()
    var roots = apps()
    if let filter = args["app"] as? String {
        roots = roots.filter { $0.name.localizedCaseInsensitiveContains(filter) || "\($0.pid)" == filter
            || ($0.running.bundleIdentifier ?? "") == filter }
    }
    roots.sort { ($0.pid == front ? 0 : 1) < ($1.pid == front ? 0 : 1) }
    var out: [[String: Any]] = []
    var visited = 0
    var truncated: String?
    // Depth first, in tree order.
    func walk(_ node: Node, app: App) {
        if truncated != nil { return }
        if visited >= maxVisited { truncated = "visited"; return }
        if Date() > deadline { truncated = "deadline"; return }
        visited += 1
        var (described, showing, rawRole) = detail(node, app: app)
        if described["role"] as? String == "" { return }
        if !includeHidden && !showing { return }
        // Only the frontmost application's menu bar is the one on screen.
        if rawRole == kAXMenuBarRole && app.pid != front && !includeHidden { return }
        let kids = children(node.element)
        if node.depth >= maxDepth, !kids.isEmpty { described["unexpanded_children"] = kids.count }
        if !find || matches(described, name: name, role: role) {
            if out.count >= maxNodes { truncated = "max_nodes"; return }
            if find { described["context"] = node.context.joined(separator: " > ") }
            out.append(described)
        }
        guard node.depth < maxDepth else { return }
        var context = node.context
        if let label = described["name"] as? String, node.depth == 0 || context.count < 2 { context.append(label) }
        if kids.count >= maxChildren { truncated = "children" }
        for (i, kid) in kids.enumerated() {
            walk(Node(element: kid, pid: node.pid, path: node.path + [i], depth: node.depth + 1, context: context), app: app)
        }
    }
    for app in roots {
        walk(Node(element: app.element, pid: app.pid, path: [], depth: 0, context: []), app: app)
    }
    var result: [String: Any] = ["nodes": out, "count": out.count, "visited": visited, "coordinate_space": "screen-pixels"]
    if let truncated { result["truncated"] = truncated }
    return result
}

/// The element a ref names, if it is still there with the same role.
/// What tells an element apart from its siblings of the same role: its
/// identifier, else its title or description (refs carry it).
func key(_ element: AXUIElement) -> String {
    let text = string(element, kAXIdentifierAttribute) ?? string(element, kAXTitleAttribute)
        ?? string(element, kAXDescriptionAttribute) ?? ""
    return String(text.replacingOccurrences(of: "|", with: "/").prefix(80))
}

func ref(pid: pid_t, path: [Int], role: String, key: String) -> String {
    "\(pid)|\(path.map(String.init).joined(separator: "."))|\(role)|\(key)"
}

/// The element a ref names. Apps add and remove elements (Calculator turns
/// AC into C), shifting the path: when the element there is not the same
/// role and key, the one element of the application that is, if unique.
func resolve(_ ref: String) throws -> Node {
    let parts = ref.split(separator: "|", maxSplits: 3, omittingEmptySubsequences: false).map(String.init)
    guard parts.count == 4, let pid = pid_t(parts[0]) else { throw Failure(message: "ref must come from accessibility_tree") }
    let path = parts[1].isEmpty ? [] : parts[1].split(separator: ".").compactMap { Int($0) }
    let (role, wanted) = (parts[2], parts[3])
    let gone = Failure(message: "element \(ref) is gone; read the tree again")
    func same(_ element: AXUIElement) -> Bool {
        (attribute(element, kAXRoleAttribute) as? String ?? "") == role && key(element) == wanted
    }
    var element: AXUIElement? = AXUIElementCreateApplication(pid)
    for index in path {
        let kids = element.map(children) ?? []
        element = index < kids.count ? kids[index] : nil
    }
    if let element, same(element) {
        return Node(element: element, pid: pid, path: path, depth: 0, context: [])
    }
    guard !wanted.isEmpty else { throw gone }
    var found: [Node] = []
    var queue = [Node(element: AXUIElementCreateApplication(pid), pid: pid, path: [], depth: 0, context: [])]
    var visited = 0
    while !queue.isEmpty, visited < maxVisited, found.count < 2 {
        let node = queue.removeFirst()
        visited += 1
        if !node.path.isEmpty, same(node.element) { found.append(node) }
        for (i, kid) in children(node.element).enumerated() {
            queue.append(Node(element: kid, pid: pid, path: node.path + [i], depth: 0, context: []))
        }
    }
    guard found.count == 1 else { throw gone }
    return found[0]
}

func accessibilityAction(_ args: [String: Any]) throws -> [String: Any] {
    guard let ref = args["ref"] as? String, let action = args["action"] as? String else {
        throw Failure(message: "ref and action required")
    }
    let node = try resolve(ref)
    let element = node.element
    let app = apps().first { $0.pid == node.pid }
    let role = words(attribute(element, kAXRoleAttribute) as? String ?? "")
    let label = string(element, kAXTitleAttribute) ?? string(element, kAXDescriptionAttribute) ?? ""
    // The element's ref now, in case it moved since the tree was read.
    var result: [String: Any] = ["ref": detail(node, app: nil).out["ref"] ?? ref, "action": action]
    var error = AXError.success
    switch action {
    case "do":
        let raw = actions(element)
        guard !raw.isEmpty else { throw Failure(message: "\(role) \"\(label)\" has no actions") }
        var chosen: String?
        if let wanted = args["action_name"] as? String {
            chosen = raw.first { actionName($0).lowercased() == wanted.lowercased() || $0.lowercased() == wanted.lowercased() }
            if chosen == nil {
                throw Failure(message: "no action \"\(wanted)\"; it has \(raw.map(actionName).joined(separator: ", "))")
            }
        } else {
            chosen = [kAXPressAction, kAXConfirmAction, kAXPickAction, kAXShowMenuAction, kAXRaiseAction]
                .first { raw.contains($0) } ?? raw[0]
        }
        result["action_name"] = actionName(chosen!)
        error = AXUIElementPerformAction(element, chosen! as CFString)
    case "focus":
        if let app { activate(app) }
        error = set(element, kAXFocusedAttribute, kCFBooleanTrue)
        if error != .success, (attribute(element, kAXRoleAttribute) as? String) == kAXWindowRole {
            _ = set(element, kAXMainAttribute, kCFBooleanTrue)
            error = AXUIElementPerformAction(element, kAXRaiseAction as CFString)
        }
    case "set_text":
        guard let text = args["text"] as? String, settable(element, kAXValueAttribute) else {
            throw Failure(message: "set_text needs text and an editable text element")
        }
        error = set(element, kAXValueAttribute, text as CFString)
    case "set_value":
        guard let value = args["value"] as? Double, settable(element, kAXValueAttribute) else {
            throw Failure(message: "set_value needs value and an element with a settable value")
        }
        error = set(element, kAXValueAttribute, value as CFNumber)
    default:
        throw Failure(message: "action must be do, focus, set_text or set_value")
    }
    result["done"] = error == .success
    if error != .success { result["ax_error"] = Int(error.rawValue) }
    // Report the element as the application left it.
    Thread.sleep(forTimeInterval: 0.1)
    let after = detail(node, app: nil)
    result["element_after"] = after.role.isEmpty ? NSNull() : after.out
    return result
}

// MARK: Main

AXUIElementSetMessagingTimeout(AXUIElementCreateSystemWide(), 1.0)
let method = CommandLine.arguments.dropFirst().first ?? ""
var output: [String: Any]
do {
    let input = FileHandle.standardInput.readDataToEndOfFile()
    let args = input.isEmpty ? [:] : (try JSONSerialization.jsonObject(with: input) as? [String: Any] ?? [:])
    if !AXIsProcessTrusted() {
        throw Failure(message: "SSH sessions are not trusted for Accessibility in this Guest (the first-boot grant did not run or failed); turn on sshd-keygen-wrapper in System Settings > Privacy & Security > Accessibility")
    }
    switch method {
    case "desktop_state": output = desktopState()
    case "desktop_apps": output = desktopApps()
    case "window_action": output = try windowAction(args)
    case "app_launch": output = try appLaunch(args)
    case "accessibility_tree": output = try accessibilityTree(args)
    case "accessibility_action": output = try accessibilityAction(args)
    default: throw Failure(message: "usage: vm-macos-desktop METHOD < arguments")
    }
} catch let failure as Failure {
    output = ["error": failure.message]
} catch {
    output = ["error": "\(error)"]
}
let data = try JSONSerialization.data(withJSONObject: output, options: [.sortedKeys])
FileHandle.standardOutput.write(data)
FileHandle.standardOutput.write("\n".data(using: .utf8)!)
exit(output["error"] == nil ? 0 : 1)
