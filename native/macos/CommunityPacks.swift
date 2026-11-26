import AppKit

private struct PackOperationError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

@MainActor enum NativeCommunityPacks {
    static func browse(_ packs: NativeCompanionPacks) async throws {
        let catalog = try await request(["action": "pack-catalog-list"], progress: "Loading community packs…")
        let (revision, entries) = try listing(catalog)
        let labels = entries.map {
            "\(safe($0, "name")) — \(safe($0, "author")) (\(safe($0, "license")))"
        }
        guard !labels.isEmpty else { notice("No community packs are listed."); return }
        guard let pick = choose("Community packs", labels) else { return }
        let entry = entries[pick]
        let installed = entry["installed"] as? Bool ?? false
        var choices: [(String, String?)] = []
        if entry["art_preview"] is [String: Any] { choices.append(("Preview artwork", "art")) }
        if entry["voice_preview"] is [String: Any] { choices.append(("Play voice preview", "voice")) }
        choices.append((installed ? "Replace local copy" : "Download and install", nil))
        defer { packs.stopPreview() }
        while let action = choose(labels[pick], choices.map { $0.0 }) {
            if let medium = choices[action].1 {
                let answer = try await request(["action": "pack-catalog-preview", "entry": entry,
                    "revision": revision, "medium": medium], progress: "Downloading preview…")
                guard let encoded = answer["media"] as? String, let data = Data(base64Encoded: encoded) else {
                    throw PackOperationError(message: "The preview could not be loaded.")
                }
                if medium == "art" { try showArt(safe(entry, "name"), data) }
                else if !packs.previewVoice(data) { throw PackOperationError(message: "The voice preview could not be played.") }
                continue
            }
            if installed && !confirm("Replace local pack?", "The installed copy and any edits to it will be replaced.") {
                continue
            }
            _ = try await request(["action": "pack-catalog-install", "entry": entry, "revision": revision,
                                   "replace": installed], progress: "Downloading and verifying pack…")
            notice("Pack installed. Choose it in Appearance or Voice to start using it.")
            return
        }
    }

    static func remove(_ packs: NativeCompanionPacks) async throws {
        let catalog = try await request(["action": "pack-list"])
        var installed: [String: String] = [:]
        for medium in ["appearance", "voice"] {
            for entry in (catalog[medium] as? [[String]] ?? []) where entry.count == 2 {
                installed[entry[0]] = nativeMcpSafe(entry[1])
            }
        }
        let ids = installed.keys.sorted()
        guard !ids.isEmpty else { notice("No packs are installed."); return }
        let labels = ids.map { "\(installed[$0] ?? "") [\($0)]" }
        guard let pick = choose("Remove installed pack", labels),
              confirm("Remove local pack?", "Remove \(labels[pick])?") else { return }
        do {
            _ = try await request(["action": "pack-catalog-remove", "id": ids[pick]])
        } catch {
            await packs.load()
            throw error
        }
        await packs.load()
    }

    private static func request(_ fields: [String: Any], progress message: String? = nil) async throws -> [String: Any] {
        let payload = try JSONSerialization.data(withJSONObject: fields)
        let progress = message.map(showProgress)
        defer { progress?.orderOut(nil) }
        guard let data = await Task.detached(priority: .userInitiated, operation: { packResponseData(payload) }).value,
              let reply = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              reply["status"] as? Int == 200, let body = reply["body"] as? [String: Any] else {
            throw PackOperationError(message: "The companion pack request failed.")
        }
        if let message = body["error"] as? String { throw PackOperationError(message: message) }
        return body
    }

    private static func showProgress(_ message: String) -> NSPanel {
        let panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 370, height: 80),
                           styleMask: [.titled], backing: .buffered, defer: false)
        panel.isReleasedWhenClosed = false
        panel.title = "Community packs"
        let spinner = NSProgressIndicator(frame: NSRect(x: 16, y: 30, width: 24, height: 24))
        spinner.style = .spinning; spinner.startAnimation(nil)
        let label = NSTextField(labelWithString: message)
        label.frame = NSRect(x: 56, y: 31, width: 294, height: 20)
        panel.contentView?.addSubview(spinner); panel.contentView?.addSubview(label)
        panel.center(); panel.orderFrontRegardless()
        return panel
    }

    private static func safe(_ entry: [String: Any], _ key: String) -> String {
        nativeMcpSafe(entry[key] as? String ?? "")
    }

    private static func listing(_ catalog: [String: Any]) throws -> (String, [[String: Any]]) {
        guard let revision = catalog["asset_revision"] as? String,
              let entries = catalog["packs"] as? [[String: Any]] else {
            throw PackOperationError(message: "The community catalog is unavailable.")
        }
        return (revision, entries)
    }

    private static func choose(_ title: String, _ items: [String]) -> Int? {
        let alert = NSAlert()
        alert.messageText = title
        alert.addButton(withTitle: "Continue"); alert.addButton(withTitle: "Cancel")
        let popup = NSPopUpButton(frame: NSRect(x: 0, y: 0, width: 450, height: 26))
        popup.addItems(withTitles: items)
        alert.accessoryView = popup
        NSApplication.shared.activate(ignoringOtherApps: true)
        guard alert.runModal() == .alertFirstButtonReturn, items.indices.contains(popup.indexOfSelectedItem) else { return nil }
        return popup.indexOfSelectedItem
    }

    private static func confirm(_ title: String, _ message: String) -> Bool {
        let alert = NSAlert()
        alert.messageText = title; alert.informativeText = message
        alert.addButton(withTitle: "Cancel"); alert.addButton(withTitle: "Confirm")
        return alert.runModal() == .alertSecondButtonReturn
    }

    private static func notice(_ message: String) {
        let alert = NSAlert()
        alert.messageText = message
        alert.runModal()
    }

    private static func showArt(_ title: String, _ data: Data) throws {
        let image = try decodeArt(data)
        let view = NSImageView(frame: NSRect(x: 0, y: 0, width: 280, height: 364))
        view.imageScaling = .scaleProportionallyUpOrDown; view.image = image
        let alert = NSAlert()
        alert.messageText = title
        alert.accessoryView = view
        alert.addButton(withTitle: "Close")
        alert.runModal()
    }

    private static func decodeArt(_ data: Data) throws -> NSImage {
        guard let image = NSImage(data: data) else { throw PackOperationError(message: "The artwork preview could not be shown.") }
        return image
    }

    static func checkContract(_ path: String) throws {
        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        guard let fixture = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let catalog = fixture["catalog"] as? [String: Any],
              let previews = fixture["previews"] as? [String: [String: Any]] else {
            throw PackOperationError(message: "Invalid catalog response fixture")
        }
        let (_, entries) = try listing(catalog)
        for id in ["ember-cat", "scout-dog", "clover-rabbit", "visor-beetle"] {
            guard let entry = entries.first(where: { $0["id"] as? String == id }),
                  let preview = previews[id], let art = preview["art"] as? String,
                  let picture = Data(base64Encoded: art) else {
                throw PackOperationError(message: "Missing preview for \(id)")
            }
            let image = try decodeArt(picture)
            try verifyNativeUnit(!safe(entry, "name").isEmpty && image.size == NSSize(width: 400, height: 520))
            if id != "visor-beetle" {
                let sound = (preview["voice"] as? String).flatMap { Data(base64Encoded: $0) }.flatMap { NSSound(data: $0) }
                try verifyNativeUnit((sound?.duration ?? 0) > 0)
            }
        }
        try NativeCompanionPacks.checkCatalogPair(data)
        print("PASS community pack previews, Beetle with Clover Rabbit voice, and bundled art and mute after removal")
    }
}
