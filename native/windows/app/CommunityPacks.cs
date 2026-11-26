using System.Media;
using System.Text.Json;

namespace Hypermesh.Visor;

internal static class CommunityPacks
{
    internal static async Task Browse(Form owner, CompanionPacks packs)
    {
        var catalog = await Progress(owner, "Loading community packs…", new { action = "pack-catalog-list" });
        if (owner.IsDisposed) return;
        var entries = catalog.GetProperty("packs").EnumerateArray().ToArray();
        if (entries.Length == 0) { Notice(owner, "No community packs are listed."); return; }
        var labels = entries.Select(entry => $"{Safe(entry, "name")} — {Safe(entry, "author")} ({Safe(entry, "license")})").ToArray();
        int choice = Choose(owner, "Community packs", labels);
        if (choice < 0) return;
        JsonElement entry = entries[choice];
        string revision = catalog.GetProperty("asset_revision").GetString()!;
        bool installed = entry.GetProperty("installed").GetBoolean();
        var actions = new List<string>();
        var media = new List<string?>();
        if (entry.GetProperty("art_preview").ValueKind == JsonValueKind.Object) { actions.Add("Preview artwork"); media.Add("art"); }
        if (entry.GetProperty("voice_preview").ValueKind == JsonValueKind.Object) { actions.Add("Play voice preview"); media.Add("voice"); }
        actions.Add(installed ? "Replace local copy" : "Download and install"); media.Add(null);
        while (!owner.IsDisposed)
        {
            int action = Choose(owner, labels[choice], actions);
            if (action < 0) return;
            if (media[action] is { } medium)
            {
                var result = await Progress(owner, "Downloading preview…",
                    new { action = "pack-catalog-preview", entry, revision, medium });
                if (owner.IsDisposed) return;
                byte[] bytes = result.GetProperty("media").GetBytesFromBase64();
                if (medium == "art") ShowArt(owner, Safe(entry, "name"), bytes);
                else
                {
                    packs.Quiet();
                    await Task.Run(() => {
                        using var wave = new MemoryStream(bytes, writable: false);
                        using var player = new SoundPlayer(wave);
                        player.PlaySync();
                    });
                }
                continue;
            }
            if (installed && !Confirm(owner, "Replace local pack?", "The installed copy and any edits to it will be replaced.")) continue;
            await Progress(owner, "Downloading and verifying pack…", new { action = "pack-catalog-install", entry, revision, replace = installed });
            if (!owner.IsDisposed) Notice(owner, "Pack installed. Choose it in Appearance or Voice to start using it.");
            return;
        }
    }

    internal static async Task Remove(Form owner, CompanionPacks packs)
    {
        var catalog = Checked(await Engine.Pack(new { action = "pack-list" }));
        if (owner.IsDisposed) return;
        var installed = new SortedDictionary<string, string>(StringComparer.Ordinal);
        foreach (string medium in new[] { "appearance", "voice" })
            foreach (var entry in catalog.GetProperty(medium).EnumerateArray())
                installed[entry[0].GetString()!] = McpTrustWindow.Safe(entry[1].GetString()!);
        var items = installed.ToArray();
        if (items.Length == 0) { Notice(owner, "No packs are installed."); return; }
        string[] labels = items.Select(item => $"{item.Value} [{item.Key}]").ToArray();
        int choice = Choose(owner, "Remove installed pack", labels);
        if (choice < 0 || !Confirm(owner, "Remove local pack?", $"Remove {labels[choice]}?")) return;
        try { Checked(await Engine.Pack(new { action = "pack-catalog-remove", id = items[choice].Key })); }
        finally { await packs.Load(); }
    }

    private static string Safe(JsonElement entry, string key) => McpTrustWindow.Safe(entry.GetProperty(key).GetString()!);

    private static JsonElement Checked(JsonElement? response)
    {
        if (response is not { } body) throw new InvalidOperationException("The companion pack request failed.");
        if (body.TryGetProperty("error", out var error)) throw new InvalidOperationException(error.GetString());
        return body;
    }

    private static async Task<JsonElement> Progress(Form owner, string message, object request)
    {
        using var dialog = new Form { Text = "Community packs", Width = 420, Height = 130,
            StartPosition = FormStartPosition.CenterParent, FormBorderStyle = FormBorderStyle.FixedDialog,
            ControlBox = false, ShowInTaskbar = false };
        dialog.Controls.Add(new Label { Text = message, Dock = DockStyle.Fill, Padding = new Padding(12) });
        dialog.Controls.Add(new ProgressBar { Dock = DockStyle.Bottom, Style = ProgressBarStyle.Marquee });
        dialog.Show(owner);
        return Checked(await Engine.Pack(request));
    }

    private static bool Confirm(Form owner, string title, string text) =>
        MessageBox.Show(owner, text, title, MessageBoxButtons.YesNo, MessageBoxIcon.Question,
            MessageBoxDefaultButton.Button2) == DialogResult.Yes;

    private static void Notice(Form owner, string text) =>
        MessageBox.Show(owner, text, "Companion packs", MessageBoxButtons.OK);

    private static int Choose(Form owner, string title, IReadOnlyList<string> choices)
    {
        using var dialog = new Form { Text = title, Width = 580, Height = 350,
            StartPosition = FormStartPosition.CenterParent, FormBorderStyle = FormBorderStyle.FixedDialog,
            MinimizeBox = false, MaximizeBox = false, ShowInTaskbar = false };
        var list = new ListBox { Dock = DockStyle.Fill, HorizontalScrollbar = true };
        list.Items.AddRange(choices.Cast<object>().ToArray()); list.SelectedIndex = 0;
        var row = new FlowLayoutPanel { Dock = DockStyle.Bottom, FlowDirection = FlowDirection.RightToLeft, AutoSize = true };
        var dismiss = new Button { Text = "Cancel", DialogResult = DialogResult.Cancel, AutoSize = true };
        var next = new Button { Text = "Continue", DialogResult = DialogResult.OK, AutoSize = true };
        row.Controls.Add(dismiss); row.Controls.Add(next);
        dialog.Controls.Add(list); dialog.Controls.Add(row);
        dialog.AcceptButton = next; dialog.CancelButton = dismiss;
        return dialog.ShowDialog(owner) == DialogResult.OK ? list.SelectedIndex : -1;
    }

    private static void ShowArt(Form owner, string title, byte[] bytes)
    {
        using var buffer = new MemoryStream(bytes, writable: false);
        using var image = Image.FromStream(buffer, useEmbeddedColorManagement: false, validateImageData: true);
        using var dialog = new Form { Text = title, Width = 355, Height = 455,
            StartPosition = FormStartPosition.CenterParent, ShowInTaskbar = false };
        dialog.Controls.Add(new PictureBox { Dock = DockStyle.Fill, SizeMode = PictureBoxSizeMode.Zoom, Image = image });
        dialog.ShowDialog(owner);
    }
}
