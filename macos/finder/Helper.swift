// The helper app that owns Captain Salvage's Finder locations, one per canvas shown in Finder.
//
// Only the app that contains a File Provider extension may add or remove its locations, and it
// must be signed into the app group the extension reads, so this small app does that for the
// main window. The window runs it with a command and a JSON request on standard input, and it
// prints one line of JSON. It never reads the disk: the extension gets everything from the
// main app's media port, whose address and token this helper leaves in the group container.
//
//   show     {base, token, serial, canvas, name, revision}  adds a location, prints its folder
//   hide     {serial, canvas}                                removes it
//   sync     {base, token, serial, canvases: [{id, name, revision}]}
//            refreshes this disk's locations, removes those whose canvas has gone, and marks
//            other disks' locations as unreachable
//   offline  {}                                              marks every location unreachable
//   list     {}                                              the locations there are

import FileProvider
import Foundation

let appGroup = "group.com.antimatterstudios.captainsalvage"

struct Canvas: Codable {
    let id: Int64
    let name: String
    let revision: String
}

struct Request: Codable {
    var base: String?
    var token: String?
    var serial: String?
    var canvas: Int64?
    var name: String?
    var revision: String?
    var canvases: [Canvas]?
}

struct Config: Codable {
    let base: String
    let token: String
    let serial: String
    let canvas: Int64
    let revision: String
}

func reply(_ value: [String: Any]) -> Never {
    let data = (try? JSONSerialization.data(withJSONObject: value)) ?? Data("{}".utf8)
    FileHandle.standardOutput.write(data + Data("\n".utf8))
    exit(value["error"] == nil ? 0 : 1)
}

func fail(_ text: String) -> Never { reply(["error": text]) }

/// Runs a call that finishes through a completion handler, and waits for it.
func wait<T>(_ seconds: Double = 30, _ start: (@escaping (T) -> Void) -> Void) -> T? {
    let done = DispatchSemaphore(value: 0)
    var result: T?
    start { value in
        result = value
        done.signal()
    }
    return done.wait(timeout: .now() + seconds) == .timedOut ? nil : result
}

func domainsFolder() -> URL {
    guard let container = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup) else {
        fail("This helper is not signed into the app group.")
    }
    let folder = container.appendingPathComponent("domains")
    try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
    return folder
}

func identifier(_ serial: String, _ canvas: Int64) -> NSFileProviderDomainIdentifier {
    NSFileProviderDomainIdentifier("\(serial)-\(canvas)")
}

func configURL(_ domain: NSFileProviderDomainIdentifier) -> URL {
    domainsFolder().appendingPathComponent(domain.rawValue + ".json")
}

func write(_ config: Config) {
    let url = configURL(identifier(config.serial, config.canvas))
    guard let data = try? JSONEncoder().encode(config) else { fail("Could not write the location's settings.") }
    // Only this user may read the port's token.
    FileManager.default.createFile(atPath: url.path, contents: data, attributes: [.posixPermissions: 0o600])
}

func forget(_ domain: NSFileProviderDomainIdentifier) {
    try? FileManager.default.removeItem(at: configURL(domain))
}

func domains() -> [NSFileProviderDomain] {
    let found: ([NSFileProviderDomain], Error?)? = wait { done in
        NSFileProviderManager.getDomainsWithCompletionHandler { domains, error in done((domains, error)) }
    }
    return found?.0 ?? []
}

func signal(_ domain: NSFileProviderDomain) {
    guard let manager = NSFileProviderManager(for: domain) else { return }
    let _: Error?? = wait(10) { done in manager.signalEnumerator(for: .workingSet) { done($0) } }
}

/// The folder Finder shows for a location. It can take a moment to appear after adding.
func visibleFolder(_ domain: NSFileProviderDomain) -> String? {
    guard let manager = NSFileProviderManager(for: domain) else { return nil }
    for _ in 0..<20 {
        let found: (URL?, Error?)? = wait(10) { done in
            manager.getUserVisibleURL(for: .rootContainer) { url, error in done((url, error)) }
        }
        if let url = found?.0 { return url.path }
        Thread.sleep(forTimeInterval: 1)
    }
    return nil
}

let command = CommandLine.arguments.dropFirst().first ?? "list"
let input = FileHandle.standardInput.readDataToEndOfFile()
let request = (try? JSONDecoder().decode(Request.self, from: input.isEmpty ? Data("{}".utf8) : input)) ?? Request()

switch command {
case "show":
    guard let base = request.base, let token = request.token, let serial = request.serial,
          let canvas = request.canvas, let name = request.name else { fail("show needs base, token, serial, canvas and name.") }
    write(Config(base: base, token: token, serial: serial, canvas: canvas, revision: request.revision ?? "0"))
    let domain = NSFileProviderDomain(identifier: identifier(serial, canvas), displayName: name)
    let added: Error?? = wait { done in NSFileProviderManager.add(domain) { done($0) } }
    if let error = added ?? nil { fail("macOS would not add the Finder location: \(error.localizedDescription)") }
    if added == nil { fail("macOS did not answer when adding the Finder location.") }
    signal(domain)
    guard let folder = visibleFolder(domain) else {
        reply(["shown": true, "folder": NSNull(), "note": "Turn on Captain Salvage in System Settings, General, Login Items & Extensions, File Providers."])
    }
    reply(["shown": true, "folder": folder])

case "hide":
    guard let serial = request.serial, let canvas = request.canvas else { fail("hide needs serial and canvas.") }
    let id = identifier(serial, canvas)
    if let domain = domains().first(where: { $0.identifier == id }) {
        let removed: Error?? = wait { done in NSFileProviderManager.remove(domain) { done($0) } }
        if let error = removed ?? nil { fail("macOS would not remove the Finder location: \(error.localizedDescription)") }
    }
    forget(id)
    reply(["hidden": true])

case "sync":
    guard let base = request.base, let token = request.token, let serial = request.serial else {
        fail("sync needs base, token and serial.")
    }
    let canvases = Dictionary((request.canvases ?? []).map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
    var shown: [Int64] = []
    for domain in domains() {
        let parts = domain.identifier.rawValue.split(separator: "-")
        guard parts.count == 2, let canvas = Int64(parts[1]) else { continue }
        if parts[0] != Substring(serial) {
            // Another disk's canvas: unreachable until that disk is open again.
            forget(domain.identifier)
            signal(domain)
        } else if let found = canvases[canvas] {
            write(Config(base: base, token: token, serial: serial, canvas: canvas, revision: found.revision))
            signal(domain)
            shown.append(canvas)
        } else {
            let _: Error?? = wait { done in NSFileProviderManager.remove(domain) { done($0) } }
            forget(domain.identifier)
        }
    }
    reply(["shown": shown])

case "offline":
    for domain in domains() {
        forget(domain.identifier)
        signal(domain)
    }
    reply(["offline": true])

default:
    reply(["domains": domains().map { ["id": $0.identifier.rawValue, "name": $0.displayName] }])
}
