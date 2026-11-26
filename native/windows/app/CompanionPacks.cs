using System.Media;
using System.Text.Json;

namespace Hypermesh.Visor;

internal sealed class CompanionPacks : IDisposable
{
    private Appearance? appearance;
    private Voice? voice;
    private string? mood, vibe, verb, mode, notice;
    private ulong sessionIdle, sleepAfter = 900_000;
    private long sampledAt;
    private readonly long startedAt = Environment.TickCount64;
    private Motion? motion;
    private (ulong Instance, ulong Timestamp, ulong Sequence)? driveId;
    private long driveAt;
    private string state = "", label = "Disconnected";
    private (string State, string? Verb, string? Mode)? resolved;
    private Clip? clip;
    private long clipAt;
    private string? selectedAudio;
    private long readyAt;
    private SoundPlayer? player;
    private MemoryStream? playbackStream;
    private bool pendingAudio, disposed;

    internal bool Active => appearance != null || voice != null;
    internal string Label => label;

    internal async Task Load()
    {
        var appearanceTask = Engine.Pack(new { action = "pack-load", medium = "appearance" });
        var voiceTask = Engine.Pack(new { action = "pack-load", medium = "voice" });
        Apply("appearance", await appearanceTask);
        Apply("voice", await voiceTask);
    }

    internal async Task Choose(Form owner, string medium)
    {
        if (await Engine.Pack(new { action = "pack-list" }) is not { } catalog || disposed)
            throw new InvalidOperationException("Installed companion packs are unavailable.");
        string title = medium == "appearance" ? "Companion appearance" : "Companion voice";
        string fallback = medium == "appearance" ? "Bundled appearance" : "Mute";
        var choices = new List<Choice> { new("", fallback) };
        foreach (JsonElement entry in catalog.GetProperty(medium).EnumerateArray())
            choices.Add(new(entry[0].GetString()!, entry[1].GetString()!));
        string? selected = catalog.GetProperty("selected").GetProperty(medium).GetString();
        using Form dialog = new() { Text = title, Width = 490, Height = 150, StartPosition = FormStartPosition.CenterParent,
            FormBorderStyle = FormBorderStyle.FixedDialog, MinimizeBox = false, MaximizeBox = false, ShowInTaskbar = false };
        var list = new ComboBox { DropDownStyle = ComboBoxStyle.DropDownList, Dock = DockStyle.Top };
        list.Items.AddRange(choices.Cast<object>().ToArray());
        list.SelectedIndex = Math.Max(0, choices.FindIndex(choice => choice.Id == selected));
        var row = new FlowLayoutPanel { Dock = DockStyle.Bottom, FlowDirection = FlowDirection.RightToLeft, AutoSize = true };
        var dismiss = new Button { Text = "Cancel", DialogResult = DialogResult.Cancel, AutoSize = true };
        var apply = new Button { Text = "Apply", DialogResult = DialogResult.OK, AutoSize = true };
        row.Controls.Add(dismiss); row.Controls.Add(apply);
        dialog.Controls.Add(list); dialog.Controls.Add(row);
        dialog.AcceptButton = apply; dialog.CancelButton = dismiss;
        if (dialog.ShowDialog(owner) != DialogResult.OK || list.SelectedItem is not Choice choice || disposed) return;
        if (await Engine.Pack(new { action = "pack-select", medium, id = choice.Id }) is not { } pack)
            throw new InvalidOperationException("The companion pack could not be loaded or saved.");
        Apply(medium, pack);
    }

    private sealed record Choice(string Id, string Name)
    {
        public override string ToString() => Id.Length == 0 ? Name : $"{McpTrustWindow.Safe(Name)} [{Id}]";
    }

    private void Apply(string medium, JsonElement? response)
    {
        if (disposed || response is not { } pack) return;
        bool chosen = Text(pack, "id") is { Length: > 0 };
        if (medium == "appearance")
        {
            var next = chosen ? new Appearance(pack) : null;
            appearance?.Dispose(); appearance = next; clip = null;
        }
        else
        {
            voice = chosen ? new Voice(pack) : null;
            SelectAudio(null);
        }
        mood = Text(pack, "mood"); vibe = Text(pack, "vibe");
        sleepAfter = Number(pack, "sleep_after_ms", 900_000);
        resolved = null;
    }

    internal void Observe(JsonElement body, JsonElement session, JsonElement automation, long now)
    {
        notice = Text(body, "notice"); verb = Text(session, "verb"); mode = Text(session, "mode");
        sessionIdle = Number(session, "idle_ms", ulong.MaxValue); sampledAt = now;
        if (!automation.TryGetProperty("drive", out JsonElement drive))
        {
            driveId = null; motion = null;
            return;
        }
        var next = (Number(body, "instance"), Number(drive, "at_ms"), Number(drive, "inject_seq"));
        if (driveId == next) return;
        driveId = next; driveAt = now;
        motion = new(Text(drive, "kind") ?? "none", (uint)Number(drive, "distance_px"),
            (uint)Number(drive, "elapsed_ms"), drive.TryGetProperty("clickable", out var clickable) && clickable.ValueKind == JsonValueKind.True);
    }

    internal (Bitmap? Art, Bitmap? Cursor) Frame(bool connected, string phase, bool reducedMotion,
        long now, long lastInput)
    {
        if (!Active) return (null, null);
        ulong sinceSample = (ulong)Math.Max(0, now - sampledAt);
        ulong idle = sessionIdle + Math.Min(ulong.MaxValue - sessionIdle, sinceSample);
        ulong localIdle = (ulong)Math.Max(0, now - Math.Max(lastInput, startedAt));
        state = notice == "upgrading" && connected ? "upgrading" : !connected ? "disconnected"
            : phase == "paused" ? "paused" : phase == "running" ? "automation"
            : Math.Min(idle, localIdle) >= sleepAfter ? "sleeping" : "idle";
        var signal = (state, verb, mode);
        bool wantsCursor = state == "automation" && appearance?.HasCursor == true;
        Bitmap? pointer = null;
        if (resolved != signal || wantsCursor)
        {
            var cursor = wantsCursor ? new
            {
                clock_ms = (ulong)Math.Max(0, now - startedAt),
                reduced_motion = reducedMotion,
                motion = motion is { } item ? new { item.kind, item.distance_px, item.elapsed_ms, item.clickable,
                    age_ms = (ulong)Math.Max(0, now - driveAt) } : null
            } : null;
            if (Engine.PackRequest(new { action = "pack-cues", state, verb, mode, mood, vibe, cursor }) is { } cues)
            {
                if (resolved != signal)
                {
                    resolved = signal;
                    Resolve(cues, now);
                }
                pointer = appearance?.Cursor(Text(cues, "cursor"));
            }
            else
            {
                resolved = null; clip = null; SelectAudio(null);
            }
        }
        if (pendingAudio && now >= readyAt && selectedAudio is { } key && voice?.Audio(key) is { } bytes)
        {
            pendingAudio = false; readyAt = now + 750;
            try
            {
                playbackStream = new MemoryStream(bytes, writable: false);
                player = new SoundPlayer(playbackStream);
                player.Play();
            }
            catch { StopAudio(); }
        }
        return (clip?.Frame(now - clipAt, reducedMotion), pointer);
    }

    private void Resolve(JsonElement cues, long now)
    {
        label = Text(cues, "label") ?? "Visor";
        string[] keys = cues.GetProperty("keys").EnumerateArray().Select(key => key.GetString()!).ToArray();
        var next = appearance?.Select(keys);
        if (!ReferenceEquals(clip, next)) { clip = next; clipAt = now; }
        SelectAudio(voice?.Select(keys));
    }

    private sealed record Motion(string kind, uint distance_px, uint elapsed_ms, bool clickable);

    private void SelectAudio(string? next)
    {
        if (next == selectedAudio) return;
        StopAudio();
        selectedAudio = next; pendingAudio = next != null;
    }

    private void StopAudio()
    {
        try { player?.Stop(); } catch { }
        player?.Dispose(); player = null;
        playbackStream?.Dispose(); playbackStream = null;
    }

    internal void Quiet()
    {
        pendingAudio = false;
        StopAudio();
    }

    public void Dispose()
    {
        disposed = true; StopAudio(); appearance?.Dispose();
    }

    private static string? Text(JsonElement value, string key) =>
        value.ValueKind == JsonValueKind.Object && value.TryGetProperty(key, out var item)
        && item.ValueKind == JsonValueKind.String ? item.GetString() : null;

    private static ulong Number(JsonElement value, string key, ulong fallback = 0) =>
        value.ValueKind == JsonValueKind.Object && value.TryGetProperty(key, out var item)
        && item.ValueKind == JsonValueKind.Number && item.TryGetUInt64(out ulong number) ? number : fallback;

    internal static int FrameIndex(int count, long interval, bool loop, long elapsed, bool reducedMotion)
    {
        if (count <= 1 || reducedMotion) return 0;
        long step = Math.Max(0, elapsed) / Math.Max(1, interval);
        return (int)(loop ? step % count : Math.Min(step, count - 1));
    }

    internal static void CheckCatalogPair(JsonElement fixture)
    {
        using var packs = new CompanionPacks();
        var art = fixture.GetProperty("appearance");
        var audio = fixture.GetProperty("voice");
        var cues = fixture.GetProperty("cues");
        packs.Apply("appearance", art);
        packs.Apply("voice", audio);
        packs.Resolve(cues, Environment.TickCount64);
        var clip = packs.clip;
        string? recording = packs.selectedAudio;
        if (Text(art, "id") != "visor-beetle" || Text(audio, "id") != "clover-rabbit"
            || clip?.Frames.Length != 8 || clip.Frame(0, false).Width != 128
            || packs.appearance?.Cursor(Text(cues, "cursor"))?.Width != 32
            || recording != "working.wav" || packs.voice?.Audio(recording) is not { } bytes)
            throw new InvalidOperationException("Visor Beetle and Clover Rabbit did not resolve together");
        using var wave = new MemoryStream(bytes, writable: false);
        using var player = new SoundPlayer(wave);
        player.Load();

        var appearanceRemoval = fixture.GetProperty("after_appearance_removal");
        packs.Apply("appearance", appearanceRemoval.GetProperty("appearance"));
        packs.Apply("voice", appearanceRemoval.GetProperty("voice"));
        packs.Resolve(cues, Environment.TickCount64);
        if (packs.appearance != null || packs.clip != null || packs.selectedAudio != recording || !packs.pendingAudio)
            throw new InvalidOperationException("Removing Visor Beetle must restore bundled art and keep Clover Rabbit’s voice");

        var voiceRemoval = fixture.GetProperty("after_voice_removal");
        packs.Apply("appearance", voiceRemoval.GetProperty("appearance"));
        packs.Apply("voice", voiceRemoval.GetProperty("voice"));
        if (packs.Active || packs.selectedAudio != null || packs.pendingAudio)
            throw new InvalidOperationException("Removing Clover Rabbit must clear the voice and pending playback");
        var fallback = packs.Frame(true, "running", false, Environment.TickCount64, 0);
        if (fallback.Art != null || fallback.Cursor != null)
            throw new InvalidOperationException("Removing Visor Beetle must clear custom art and cursor frames");
    }

    private sealed record Clip(Bitmap[] Frames, long Interval, bool Loop)
    {
        internal Bitmap Frame(long age, bool reduced) => Frames[FrameIndex(Frames.Length, Interval, Loop, age, reduced)];
    }

    private sealed class Appearance : IDisposable
    {
        private readonly Dictionary<string, Bitmap> frames;
        private readonly Dictionary<string, Bitmap> cursor;
        private readonly Dictionary<string, Clip> clips = new();
        internal bool HasCursor => cursor.Count != 0;

        internal Appearance(JsonElement pack)
        {
            frames = Images(pack.GetProperty("frames"));
            cursor = Images(pack.GetProperty("cursor"));
            foreach (var entry in pack.GetProperty("clips").EnumerateArray())
            {
                var selected = entry.GetProperty("frames").EnumerateArray()
                    .Select(frame => frames.GetValueOrDefault(frame.GetString()!)).ToArray();
                if (selected.Length == 0 || selected.Any(frame => frame == null)) continue;
                clips[entry.GetProperty("key").GetString()!] = new(selected.Select(frame => frame!).ToArray(),
                    entry.GetProperty("frame_ms").GetInt64(), entry.GetProperty("loop").GetBoolean());
            }
        }

        internal Clip? Select(string[] keys) => keys.Select(key => clips.GetValueOrDefault(key)).FirstOrDefault(clip => clip != null);
        internal Bitmap? Cursor(string? key) => key == null ? null : cursor.GetValueOrDefault(key) ?? cursor.GetValueOrDefault("pointer");
        public void Dispose()
        {
            foreach (var bitmap in frames.Values.Concat(cursor.Values)) bitmap.Dispose();
        }

        private static Dictionary<string, Bitmap> Images(JsonElement images)
        {
            var result = new Dictionary<string, Bitmap>();
            foreach (var item in images.EnumerateObject())
            {
                try
                {
                    using var stream = new MemoryStream(item.Value.GetBytesFromBase64(), writable: false);
                    using var image = Image.FromStream(stream, useEmbeddedColorManagement: false, validateImageData: true);
                    result.Add(item.Name, new Bitmap(image));
                }
                catch (ArgumentException) { }
            }
            return result;
        }
    }

    private sealed class Voice(JsonElement pack)
    {
        private readonly Dictionary<string, string> cues = pack.GetProperty("cues").EnumerateArray()
            .ToDictionary(entry => entry[0].GetString()!, entry => entry[1].GetString()!);
        private readonly Dictionary<string, byte[]> audio = pack.GetProperty("audio").EnumerateObject()
            .ToDictionary(entry => entry.Name, entry => entry.Value.GetBytesFromBase64());
        internal string? Select(string[] keys) => keys.Select(key => cues.GetValueOrDefault(key))
            .FirstOrDefault(key => key != null && audio.ContainsKey(key));
        internal byte[]? Audio(string key) => audio.GetValueOrDefault(key);
    }
}
