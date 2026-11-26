import AppKit

private struct PackResponse<Body: Decodable>: Decodable {
    let status: Int
    let body: Body?
}

private func packRequest<Body: Decodable>(_ fields: [String: Any], as type: Body.Type) -> Body? {
    guard let request = try? JSONSerialization.data(withJSONObject: fields),
          let data = packResponseData(request),
          let answer = try? JSONDecoder().decode(PackResponse<Body>.self, from: data),
          answer.status == 200 else { return nil }
    return answer.body
}

func packResponseData(_ request: Data) -> Data? {
    var output: UnsafeMutablePointer<UInt8>?
    var size = 0
    let code = request.withUnsafeBytes { raw in
        hypermesh_native_control(raw.bindMemory(to: UInt8.self).baseAddress, request.count, &output, &size)
    }
    guard let output else { return nil }
    defer { free(output) }
    guard code == 0, size > 0, size <= 12 * 1024 * 1024 else { return nil }
    return Data(bytes: output, count: size)
}

private func loadPack<Body: Decodable & Sendable>(_ fields: [String: String], as type: Body.Type) async -> Body? {
    await Task.detached(priority: .userInitiated) {
        packRequest(fields, as: type)
    }.value
}

private struct PackCatalog: Decodable, Sendable {
    let appearance: [[String]]
    let voice: [[String]]
    let selected: [String: String]
}

private struct PackClip: Decodable, Sendable {
    let key: String
    let frames: [String]
    let frame_ms: UInt64
    let loop: Bool
}

private struct PackBundle: Decodable, Sendable {
    let id: String
    let mood: String?
    let vibe: String?
    let sleep_after_ms: UInt64
    let frames: [String: Data]?
    let clips: [PackClip]?
    let cursor: [String: Data]?
    let audio: [String: Data]?
    let cues: [[String]]?
}

private struct PackCues: Decodable {
    let label: String
    let keys: [String]
    let cursor: String?
}

func nativePackFrameIndex(count: Int, interval: UInt64, loop: Bool, elapsed: UInt64, reduced: Bool) -> Int {
    guard count > 1, !reduced else { return 0 }
    let step = elapsed / max(1, interval)
    return Int(loop ? step % UInt64(count) : min(step, UInt64(count - 1)))
}

@MainActor final class NativeCompanionPacks {
    private struct DisplayClip {
        let frames: [NSImage]
        let interval: UInt64
        let loop: Bool
    }

    private struct CueState: Equatable {
        let state: String
        let verb: Data?
        let mode: Data?
    }

    private struct DriveID: Equatable {
        let instance: UInt64
        let timestamp: UInt64
        let sequence: UInt64
    }

    // Format 1 matches the exact UTF-8 selector, including distinct NFC/NFD keys.
    private var clips: [Data: DisplayClip] = [:]
    private var cursorArt: [String: NSImage] = [:]
    private var cues: [Data: String] = [:]
    private var recordings: [String: Data] = [:]
    private var mood: String?, vibe: String?, verb: String?, mode: String?, notice: String?
    private var sessionIdle = UInt64.max
    private var sampledAt: UInt64 = 0
    private var sleepAfter: UInt64 = 900_000
    private let startedAt = NativeCompanionPacks.clock
    private var lastInput = NativeCompanionPacks.clock
    private var driveID: DriveID?
    private var driveAt: UInt64 = 0
    private var motion: [String: Any]?
    private var resolved: CueState?
    private var clipKey: Data?
    private var clipAt: UInt64 = 0
    private var selectedAudio: String?
    private var player: NSSound?
    private var previewPlayer: NSSound?
    private var pendingAudio = false
    private var readyAt: UInt64 = 0
    private var stopped = false
    private(set) var label = "Disconnected"

    var active: Bool { !clips.isEmpty || !cursorArt.isEmpty || !cues.isEmpty }

    private static var clock: UInt64 { UInt64(ProcessInfo.processInfo.systemUptime * 1000) }

    func load() async {
        async let appearance = loadPack(["action": "pack-load", "medium": "appearance"], as: PackBundle.self)
        async let voice = loadPack(["action": "pack-load", "medium": "voice"], as: PackBundle.self)
        if let pack = await appearance { apply(pack, medium: "appearance") }
        if let pack = await voice { apply(pack, medium: "voice") }
    }

    func choose(_ medium: String) async -> Bool {
        guard let catalog = await loadPack(["action": "pack-list"], as: PackCatalog.self), !stopped else { return false }
        let appearance = medium == "appearance"
        let choices = [["", appearance ? "Bundled appearance" : "Mute"]]
            + (appearance ? catalog.appearance : catalog.voice).filter { $0.count == 2 }
        let popup = NSPopUpButton(frame: NSRect(x: 0, y: 0, width: 325, height: 26))
        popup.addItems(withTitles: choices.map { $0[0].isEmpty ? $0[1] : "\(nativeMcpSafe($0[1])) [\($0[0])]" })
        popup.selectItem(at: choices.firstIndex(where: { $0[0] == catalog.selected[medium] }) ?? 0)
        let alert = NSAlert()
        alert.messageText = appearance ? "Companion appearance" : "Companion voice"
        alert.accessoryView = popup
        alert.addButton(withTitle: "Apply"); alert.addButton(withTitle: "Cancel")
        NSApplication.shared.activate(ignoringOtherApps: true)
        guard alert.runModal() == .alertFirstButtonReturn,
              choices.indices.contains(popup.indexOfSelectedItem), !stopped else { return true }
        let id = choices[popup.indexOfSelectedItem][0]
        guard let pack = await loadPack(["action": "pack-select", "medium": medium, "id": id], as: PackBundle.self) else { return false }
        apply(pack, medium: medium)
        return true
    }

    private func apply(_ pack: PackBundle, medium: String) {
        guard !stopped else { return }
        mood = pack.mood; vibe = pack.vibe; sleepAfter = pack.sleep_after_ms
        if medium == "appearance" {
            let frames = (pack.frames ?? [:]).compactMapValues { data -> NSImage? in
                guard let image = NSImage(data: data), image.size.height > 0 else { return nil }
                image.size = NSSize(width: image.size.width / image.size.height * 22, height: 22)
                return image
            }
            clips.removeAll()
            for value in pack.clips ?? [] {
                let images = value.frames.compactMap { frames[$0] }
                guard !images.isEmpty, images.count == value.frames.count else { continue }
                clips[Data(value.key.utf8)] = DisplayClip(frames: images, interval: value.frame_ms, loop: value.loop)
            }
            cursorArt = (pack.cursor ?? [:]).compactMapValues { NSImage(data: $0) }
            clipKey = nil
        } else {
            recordings = pack.audio ?? [:]
            cues.removeAll()
            for entry in pack.cues ?? [] where entry.count == 2 { cues[Data(entry[0].utf8)] = entry[1] }
            selectAudio(nil)
        }
        resolved = nil
    }

    func noteInput() { lastInput = Self.clock }

    func observe(_ body: [String: Any], automation: [String: Any]) {
        let session = body["session"] as? [String: Any]
        notice = body["notice"] as? String
        verb = session?["verb"] as? String; mode = session?["mode"] as? String
        sessionIdle = nativeEpoch(session?["idle_ms"]) ?? .max; sampledAt = Self.clock
        guard let sample = automation["drive"] as? [String: Any], let kind = sample["kind"] as? String else {
            driveID = nil; motion = nil
            return
        }
        let next = DriveID(instance: nativeEpoch(body["instance"]) ?? 0,
            timestamp: nativeEpoch(sample["at_ms"]) ?? 0, sequence: nativeEpoch(sample["inject_seq"]) ?? 0)
        guard driveID != next else { return }
        driveID = next; driveAt = sampledAt
        motion = ["kind": kind, "distance_px": nativeEpoch(sample["distance_px"]) ?? 0,
                  "elapsed_ms": nativeEpoch(sample["elapsed_ms"]) ?? 0,
                  "clickable": sample["clickable"] as? Bool ?? false]
    }

    func frame(connected: Bool, running: Bool, paused: Bool, reduced: Bool) -> (NSImage?, NSImage?) {
        guard active, !stopped else { return (nil, nil) }
        let now = Self.clock
        let sessionElapsed = sessionIdle + min(UInt64.max - sessionIdle, now - sampledAt)
        let state: String
        if connected && notice == "upgrading" { state = "upgrading" }
        else if !connected { state = "disconnected" }
        else if paused { state = "paused" }
        else if running { state = "automation" }
        else if min(sessionElapsed, now - lastInput) >= sleepAfter { state = "sleeping" }
        else { state = "idle" }
        let signal = CueState(state: state, verb: verb.map { Data($0.utf8) }, mode: mode.map { Data($0.utf8) })
        let wantsCursor = state == "automation" && !cursorArt.isEmpty
        var cursor: NSImage?
        if resolved != signal || wantsCursor {
            var fields: [String: Any] = ["action": "pack-cues", "state": state]
            fields["verb"] = verb; fields["mode"] = mode; fields["mood"] = mood; fields["vibe"] = vibe
            if wantsCursor {
                var pointer: [String: Any] = ["clock_ms": now - startedAt, "reduced_motion": reduced]
                if var motion {
                    motion["age_ms"] = now - driveAt
                    pointer["motion"] = motion
                }
                fields["cursor"] = pointer
            }
            if let cue = packRequest(fields, as: PackCues.self) {
                if resolved != signal {
                    resolved = signal
                    resolve(cue, now: now)
                }
                if let key = cue.cursor { cursor = cursorArt[key] ?? cursorArt["pointer"] }
            } else {
                resolved = nil; clipKey = nil; selectAudio(nil)
            }
        }
        if pendingAudio, now >= readyAt, let key = selectedAudio, let data = recordings[key] {
            pendingAudio = false; readyAt = now + 750
            player = NSSound(data: data)
            player?.play()
        }
        let art = clipKey.flatMap { clips[$0] }.map { clip in
            clip.frames[nativePackFrameIndex(count: clip.frames.count, interval: clip.interval,
                loop: clip.loop, elapsed: now - clipAt, reduced: reduced)]
        }
        return (art, cursor)
    }

    private func resolve(_ cue: PackCues, now: UInt64) {
        label = cue.label
        let keys = cue.keys.map { Data($0.utf8) }
        let next = keys.first { clips[$0] != nil }
        if clipKey != next { clipKey = next; clipAt = now }
        selectAudio(keys.lazy.compactMap { self.cues[$0] }.first { recordings[$0] != nil })
    }

    static func checkCatalogPair(_ data: Data) throws {
        struct Selection: Decodable {
            let appearance: PackBundle
            let voice: PackBundle
        }
        struct Fixture: Decodable {
            let appearance: PackBundle
            let voice: PackBundle
            let cues: PackCues
            let after_appearance_removal: Selection
            let after_voice_removal: Selection
        }
        let fixture = try JSONDecoder().decode(Fixture.self, from: data)
        let packs = NativeCompanionPacks()
        packs.apply(fixture.appearance, medium: "appearance")
        packs.apply(fixture.voice, medium: "voice")
        packs.resolve(fixture.cues, now: Self.clock)
        let art = packs.clipKey.flatMap { packs.clips[$0] }
        let cursor = fixture.cues.cursor.flatMap { packs.cursorArt[$0] }
        let sound = packs.selectedAudio.flatMap { packs.recordings[$0] }.flatMap { NSSound(data: $0) }
        try verifyNativeUnit(fixture.appearance.id == "visor-beetle" && art?.frames.count == 8)
        try verifyNativeUnit(cursor?.size == NSSize(width: 32, height: 32))
        try verifyNativeUnit(fixture.voice.id == "clover-rabbit" && packs.selectedAudio == "working.wav" && (sound?.duration ?? 0) > 0)

        let appearanceRemoval = fixture.after_appearance_removal
        packs.apply(appearanceRemoval.appearance, medium: "appearance")
        packs.apply(appearanceRemoval.voice, medium: "voice")
        packs.resolve(fixture.cues, now: Self.clock)
        try verifyNativeUnit(packs.clips.isEmpty && packs.cursorArt.isEmpty && packs.clipKey == nil)
        try verifyNativeUnit(packs.selectedAudio == "working.wav" && packs.pendingAudio)

        let voiceRemoval = fixture.after_voice_removal
        packs.apply(voiceRemoval.appearance, medium: "appearance")
        packs.apply(voiceRemoval.voice, medium: "voice")
        let (fallbackArt, fallbackCursor) = packs.frame(connected: true, running: true, paused: false, reduced: false)
        try verifyNativeUnit(!packs.active && fallbackArt == nil && fallbackCursor == nil)
        try verifyNativeUnit(packs.recordings.isEmpty && packs.selectedAudio == nil && !packs.pendingAudio)
    }

    private func selectAudio(_ key: String?) {
        guard selectedAudio != key else { return }
        player?.stop(); player = nil
        selectedAudio = key; pendingAudio = key != nil
    }

    func previewVoice(_ data: Data) -> Bool {
        pendingAudio = false
        player?.stop(); player = nil
        previewPlayer?.stop()
        previewPlayer = NSSound(data: data)
        return previewPlayer?.play() == true
    }

    func stopPreview() { previewPlayer?.stop(); previewPlayer = nil }

    func stop() {
        stopped = true; selectAudio(nil); stopPreview()
    }
}
