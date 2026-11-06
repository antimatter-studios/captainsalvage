// The File Provider extension that shows a canvas's rebuilt tree in Finder.
//
// It is read-only. Every folder and file comes from the app's private media port, which reads
// the disk through the map without writing to it. The port's address and token are left in the
// app group's container by the helper app, one file per Finder location, and change each time
// the app starts. When the app is closed, or another disk is open, Finder is told the files
// cannot be reached, rather than shown anything stale or from the wrong disk.

import FileProvider
import Foundation
import UniformTypeIdentifiers
import os

let log = Logger(subsystem: "com.antimatterstudios.captainsalvage", category: "finder")

let appGroup = "group.com.antimatterstudios.captainsalvage"

/// What the helper left for one Finder location.
struct Config: Decodable {
    let base: String
    let token: String
    let serial: String
    let canvas: Int64
    let revision: String
}

func readConfig(_ domain: NSFileProviderDomainIdentifier) -> Config? {
    guard let container = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup) else {
        return nil
    }
    let url = container.appendingPathComponent("domains").appendingPathComponent(domain.rawValue + ".json")
    guard let data = try? Data(contentsOf: url) else { return nil }
    return try? JSONDecoder().decode(Config.self, from: data)
}

func hex(_ data: Data) -> String { data.map { String(format: "%02x", $0) }.joined() }

func unhex(_ text: Substring) -> Data? {
    var data = Data()
    var index = text.startIndex
    while index < text.endIndex {
        let next = text.index(index, offsetBy: 2, limitedBy: text.endIndex) ?? text.endIndex
        guard next != index, let byte = UInt8(text[index..<next], radix: 16) else { return nil }
        data.append(byte)
        index = next
    }
    return data
}

/// Where an item is in the rebuilt tree: the path names of its folders, and its own name.
/// Folders are "d" and files "f", each followed by the path as JSON in hex.
enum Place {
    case folder([String])
    case file([String], String)

    init?(_ identifier: NSFileProviderItemIdentifier) {
        if identifier == .rootContainer {
            self = .folder([])
            return
        }
        let raw = identifier.rawValue
        guard let kind = raw.first, let data = unhex(raw.dropFirst()),
              let path = try? JSONDecoder().decode([String].self, from: data) else { return nil }
        switch kind {
        case "d": self = .folder(path)
        case "f":
            guard let name = path.last else { return nil }
            self = .file(Array(path.dropLast()), name)
        default: return nil
        }
    }

    static func identifier(folder path: [String]) -> NSFileProviderItemIdentifier {
        if path.isEmpty { return .rootContainer }
        return NSFileProviderItemIdentifier("d" + hex((try? JSONEncoder().encode(path)) ?? Data()))
    }

    static func identifier(file name: String, in path: [String]) -> NSFileProviderItemIdentifier {
        NSFileProviderItemIdentifier("f" + hex((try? JSONEncoder().encode(path + [name])) ?? Data()))
    }
}

/// One folder of the rebuilt tree, as the media port lists it.
struct Listing: Decodable {
    struct Folder: Decodable {
        let pathName: String
        let name: String
    }
    struct File: Decodable {
        let name: String
        let id: Int64
        let length: Int64
        let modified: Double?
    }
    let folders: [Folder]
    let files: [File]
}

final class Item: NSObject, NSFileProviderItem {
    let itemIdentifier: NSFileProviderItemIdentifier
    let parentItemIdentifier: NSFileProviderItemIdentifier
    let filename: String
    let entry: Int64?
    let length: Int64
    let modified: Date?

    init(folder name: String, path: [String]) {
        itemIdentifier = Place.identifier(folder: path)
        parentItemIdentifier = path.isEmpty ? .rootContainer : Place.identifier(folder: Array(path.dropLast()))
        filename = name
        entry = nil
        length = 0
        modified = nil
    }

    init(file: Listing.File, in path: [String]) {
        itemIdentifier = Place.identifier(file: file.name, in: path)
        parentItemIdentifier = Place.identifier(folder: path)
        filename = file.name
        entry = file.id
        length = file.length
        modified = file.modified.map { Date(timeIntervalSince1970: $0) }
    }

    var isFolder: Bool { entry == nil }
    var capabilities: NSFileProviderItemCapabilities {
        isFolder ? [.allowsReading, .allowsContentEnumerating] : [.allowsReading]
    }
    var contentType: UTType {
        isFolder ? .folder : (UTType(filenameExtension: (filename as NSString).pathExtension) ?? .data)
    }
    var documentSize: NSNumber? { isFolder ? nil : NSNumber(value: length) }
    var contentModificationDate: Date? { modified }
    var creationDate: Date? { modified }
    var itemVersion: NSFileProviderItemVersion {
        let content = Data("\(entry ?? 0)-\(length)".utf8)
        return NSFileProviderItemVersion(contentVersion: content, metadataVersion: content + Data(filename.utf8))
    }
}

func unreachable() -> NSError { NSFileProviderError(.serverUnreachable) as NSError }
func refused() -> NSError { NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError) }

/// Talks to the media port for one Finder location.
final class Port {
    let domain: NSFileProviderDomainIdentifier
    let session: URLSession

    init(_ domain: NSFileProviderDomainIdentifier) {
        self.domain = domain
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 60
        session = URLSession(configuration: configuration)
    }

    func url(_ path: String, _ extra: [URLQueryItem], _ config: Config) -> URL? {
        var parts = URLComponents(string: config.base + path)
        parts?.queryItems = [URLQueryItem(name: "t", value: config.token), URLQueryItem(name: "v", value: config.serial)] + extra
        return parts?.url
    }

    /// Lists one folder, or fails with the error Finder should see.
    func list(_ path: [String], _ done: @escaping (Listing?, Error?) -> Void) {
        guard let config = readConfig(domain) else { return done(nil, unreachable()) }
        let named = path.isEmpty ? [] : [URLQueryItem(name: "p", value: hex((try? JSONEncoder().encode(path)) ?? Data()))]
        guard let url = url("/canvas/\(config.canvas)", named, config) else { return done(nil, unreachable()) }
        session.dataTask(with: url) { data, response, error in
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            if status == 404 { return done(nil, NSFileProviderError(.noSuchItem)) }
            guard error == nil, status == 200, let data,
                  let listing = try? JSONDecoder().decode(Listing.self, from: data) else {
                return done(nil, unreachable())
            }
            done(listing, nil)
        }.resume()
    }

    /// Fetches a file's bytes into a temporary file of this extension's own.
    func fetch(_ entry: Int64, _ done: @escaping (URL?, Error?) -> Void) -> Progress {
        guard let config = readConfig(domain), let url = url("/file/\(entry)", [], config) else {
            done(nil, unreachable())
            return Progress()
        }
        let task = session.downloadTask(with: url) { downloaded, response, error in
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            guard error == nil, status == 200, let downloaded else { return done(nil, unreachable()) }
            let kept = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            do {
                try FileManager.default.moveItem(at: downloaded, to: kept)
                done(kept, nil)
            } catch {
                done(nil, error)
            }
        }
        task.resume()
        return task.progress
    }

    /// Finds one item by listing the folder it is in.
    func item(_ identifier: NSFileProviderItemIdentifier, _ done: @escaping (Item?, Error?) -> Void) {
        guard let place = Place(identifier) else { return done(nil, NSFileProviderError(.noSuchItem)) }
        switch place {
        case .folder(let path) where path.isEmpty:
            // macOS needs a name even for the top, though Finder shows the location's own.
            done(Item(folder: "Captain Salvage", path: []), nil)
        case .folder(let path):
            let parent = Array(path.dropLast())
            list(parent) { listing, error in
                guard let listing else { return done(nil, error) }
                guard let found = listing.folders.first(where: { $0.pathName == path.last }) else {
                    return done(nil, NSFileProviderError(.noSuchItem))
                }
                done(Item(folder: found.name, path: path), nil)
            }
        case .file(let path, let name):
            list(path) { listing, error in
                guard let listing else { return done(nil, error) }
                guard let found = listing.files.first(where: { $0.name == name }) else {
                    return done(nil, NSFileProviderError(.noSuchItem))
                }
                done(Item(file: found, in: path), nil)
            }
        }
    }

    func items(in listing: Listing, at path: [String]) -> [Item] {
        listing.folders.map { Item(folder: $0.name, path: path + [$0.pathName]) }
            + listing.files.map { Item(file: $0, in: path) }
    }
}

/// What this extension has shown in each folder, so a change can say what went away.
final class Shown {
    private var folders: [[String]: Set<NSFileProviderItemIdentifier>] = [:]
    private let lock = NSLock()

    func record(_ path: [String], _ items: [Item]) {
        log.notice("shown \(path.count, privacy: .public) deep: \(items.count, privacy: .public) items")
        lock.lock()
        folders[path] = Set(items.map(\.itemIdentifier))
        lock.unlock()
    }

    func all() -> [[String]: Set<NSFileProviderItemIdentifier>] {
        lock.lock()
        defer { lock.unlock() }
        return folders
    }

    func forget(_ path: [String]) {
        lock.lock()
        folders[path] = nil
        lock.unlock()
    }
}

final class Enumerator: NSObject, NSFileProviderEnumerator {
    let container: NSFileProviderItemIdentifier
    let port: Port
    let shown: Shown

    init(_ container: NSFileProviderItemIdentifier, _ port: Port, _ shown: Shown) {
        self.container = container
        self.port = port
        self.shown = shown
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        if container == .workingSet {
            // Finder only tracks what it has shown, and changes to those come from enumerateChanges.
            observer.finishEnumerating(upTo: nil)
            return
        }
        guard case .folder(let path)? = Place(container) else {
            observer.finishEnumerating(upTo: nil)
            return
        }
        port.list(path) { listing, error in
            guard let listing else { return observer.finishEnumeratingWithError(error ?? unreachable()) }
            let items = self.port.items(in: listing, at: path)
            self.shown.record(path, items)
            observer.didEnumerate(items)
            observer.finishEnumerating(upTo: nil)
        }
    }

    /// After the canvas changed, lists again every folder Finder has shown, and reports what
    /// is there now and what has gone.
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        guard let config = readConfig(port.domain) else {
            observer.finishEnumeratingWithError(unreachable())
            return
        }
        let current = NSFileProviderSyncAnchor(Data(config.revision.utf8))
        log.notice("changes for \(self.container.rawValue == NSFileProviderItemIdentifier.workingSet.rawValue ? "working set" : "a folder", privacy: .public) from \(String(decoding: anchor.rawValue, as: UTF8.self), privacy: .public) to \(config.revision, privacy: .public), \(self.shown.all().count, privacy: .public) folders shown")
        if anchor == current || container != .workingSet {
            observer.finishEnumeratingChanges(upTo: current, moreComing: false)
            return
        }
        let folders = shown.all()
        if folders.isEmpty {
            observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
            return
        }
        let group = DispatchGroup()
        let lock = NSLock()
        var updated: [Item] = []
        var deleted: [NSFileProviderItemIdentifier] = []
        for (path, before) in folders {
            group.enter()
            port.list(path) { listing, error in
                defer { group.leave() }
                lock.lock()
                defer { lock.unlock() }
                if let listing {
                    let items = self.port.items(in: listing, at: path)
                    self.shown.record(path, items)
                    updated += items
                    deleted += before.subtracting(items.map(\.itemIdentifier))
                } else if (error as NSError?)?.code == NSFileProviderError.noSuchItem.rawValue {
                    self.shown.forget(path)
                    deleted += before
                }
            }
        }
        group.notify(queue: .global()) {
            if !deleted.isEmpty { observer.didDeleteItems(withIdentifiers: deleted) }
            if !updated.isEmpty { observer.didUpdate(updated) }
            observer.finishEnumeratingChanges(upTo: current, moreComing: false)
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        completionHandler(readConfig(port.domain).map { NSFileProviderSyncAnchor(Data($0.revision.utf8)) })
    }
}

@objc(FileProviderExtension)
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
    let port: Port
    let shown = Shown()

    required init(domain: NSFileProviderDomain) {
        port = Port(domain.identifier)
        super.init()
    }

    func invalidate() {}

    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest,
              completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        port.item(identifier) { item, error in completionHandler(item, error) }
        return Progress()
    }

    func fetchContents(for identifier: NSFileProviderItemIdentifier, version: NSFileProviderItemVersion?,
                       request: NSFileProviderRequest,
                       completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 100)
        port.item(identifier) { item, error in
            guard let item, let entry = item.entry else {
                return completionHandler(nil, nil, error ?? NSFileProviderError(.noSuchItem))
            }
            let download = self.port.fetch(entry) { url, error in completionHandler(url, url == nil ? nil : item, error) }
            progress.addChild(download, withPendingUnitCount: 100)
        }
        return progress
    }

    // The rebuilt tree is only ever read: nothing made, changed or deleted here reaches the disk.
    func createItem(basedOn template: NSFileProviderItem, fields: NSFileProviderItemFields, contents: URL?,
                    options: NSFileProviderCreateItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        completionHandler(nil, [], false, refused())
        return Progress()
    }

    func modifyItem(_ item: NSFileProviderItem, baseVersion: NSFileProviderItemVersion,
                    changedFields: NSFileProviderItemFields, contents: URL?,
                    options: NSFileProviderModifyItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        completionHandler(nil, [], false, refused())
        return Progress()
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion: NSFileProviderItemVersion,
                    options: NSFileProviderDeleteItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {
        completionHandler(refused())
        return Progress()
    }

    // Finder can ask to watch a file as well as a folder. A file has nothing inside to list.
    func enumerator(for container: NSFileProviderItemIdentifier, request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        if container != .workingSet && Place(container) == nil {
            throw NSFileProviderError(.noSuchItem)
        }
        return Enumerator(container, port, shown)
    }
}
