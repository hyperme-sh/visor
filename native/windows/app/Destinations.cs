using System.Diagnostics;
using System.Text.Json;

namespace Hypermesh.Visor;

internal static class Destinations
{
    internal static string Resolve(JsonElement? response, string expected, string? current, bool editor)
    {
        if (!Guid.TryParseExact(expected, "D", out _) || current != expected || response is not { } value ||
            !value.TryGetProperty("status", out var status) || status.GetInt32() != 200 ||
            !value.TryGetProperty("body", out var body) ||
            !body.TryGetProperty("session", out var session) || session.GetString() != expected ||
            !body.TryGetProperty(editor ? "workspace" : "forum", out var destination) || destination.ValueKind != JsonValueKind.String)
            throw new InvalidOperationException("Session destination unavailable");
        string text = destination.GetString()!;
        if (text.Length == 0 || text.Length > 4096 || text.Any(char.IsControl))
            throw new InvalidOperationException("Invalid destination");
        if (editor)
        {
            if (!Path.IsPathFullyQualified(text) || !Directory.Exists(text))
                throw new InvalidOperationException("Workspace must be an existing absolute directory");
        }
        else if (!Uri.TryCreate(text, UriKind.Absolute, out var uri) || uri.Scheme != "https" ||
            string.IsNullOrEmpty(uri.Host) || uri.UserInfo.Length != 0 || uri.Fragment.Length != 0 ||
            !ForumQuery(uri, text))
            throw new InvalidOperationException("Invalid forum URL");
        return text;
    }

    private static bool ForumQuery(Uri uri, string text)
    {
        int start = text.IndexOf('?');
        if (start < 0)
            return true;
        string[] fields = text[(start + 1)..].Split('&');
        if (uri.AbsolutePath != "/feed/squad/live" || fields.Length > 4 ||
            !fields[0].StartsWith("task=", StringComparison.Ordinal) || !SquadId(fields[0][5..]))
            return false;
        var revisions = new HashSet<string>(StringComparer.Ordinal);
        foreach (string field in fields.Skip(1))
        {
            int separator = field.IndexOf('=');
            if (separator < 0)
                return false;
            string key = field[..separator];
            if (key is not ("briefRevision" or "workbookRevision" or "previewRevision") ||
                !SquadId(field[(separator + 1)..]) || !revisions.Add(key))
                return false;
        }
        return true;
    }

    private static bool SquadId(string value) =>
        Guid.TryParseExact(value, "D", out Guid id) && value == id.ToString("D") &&
        value[14] is >= '1' and <= '8' && value[19] is '8' or '9' or 'a' or 'b';

    internal static ProcessStartInfo EditorPlan(string executable, string workspace)
    {
        ProcessStartInfo plan = new(executable) { UseShellExecute = false };
        plan.ArgumentList.Add("--new-window");
        plan.ArgumentList.Add("--");
        plan.ArgumentList.Add(workspace);
        return plan;
    }

    internal static void Open(string destination, bool editor)
    {
        if (!editor) { Process.Start(new ProcessStartInfo(destination) { UseShellExecute = true }); return; }
        // Use the installed executable directly, never code.cmd or a shell command.
        string[] candidates = {
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Programs", "Microsoft VS Code", "Code.exe"),
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "Microsoft VS Code", "Code.exe")
        };
        string executable = candidates.FirstOrDefault(File.Exists) ?? throw new InvalidOperationException("Install Visual Studio Code first");
        Process.Start(EditorPlan(executable, destination));
    }
}
