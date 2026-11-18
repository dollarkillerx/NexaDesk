using System.Text;
using System.Text.Json;
using NexaDesk.ViewModels;

namespace NexaDesk.Services;

/// <summary>One server of the published list: where it is, and the key it asks for.</summary>
public sealed record ServerEntry(string Endpoint, string Key)
{
    /// <summary>Never shows the key.</summary>
    public override string ToString() => $"ServerEntry({Endpoint})";
}

/// <summary>The servers published for everyone to use (a JSON file on GitHub), as this app reads them.</summary>
public interface IServerDirectory
{
    /// <summary>The listed servers: the file as it is now, else the one kept from the last time. Empty
    /// when there is none; never throws.</summary>
    Task<IReadOnlyList<ServerEntry>> LoadAsync(CancellationToken ct = default);
}

/// <summary>
/// The file: <c>{ "endpoints": [ { "endpoint": "wss://signal.example.com", "key": "…" } ] }</c>.
/// Each entry is a signaling server and its key, which is never empty: a signaling server always
/// asks for one, and an entry without one is not used. The key of a server offered to everyone is
/// public, so it only keeps strangers off; changing it in the file (and on the server) cuts off
/// whoever does not read the file again.
/// </summary>
public static class ServerList
{
    /// <summary>A bigger file is not read.</summary>
    public const int MaxBytes = 1 << 16;

    /// <summary>More entries than this are ignored.</summary>
    public const int MaxEntries = 16;

    /// <summary>The valid entries of a file, or null when it is not the kind of file this reads at all.
    /// An entry that is not valid (no key, an empty one, an address that is not wss://, another
    /// type) is left out and the others still count. Unknown fields are ignored.</summary>
    public static IReadOnlyList<ServerEntry>? Parse(ReadOnlySpan<byte> json)
    {
        if (json.Length == 0 || json.Length > MaxBytes) return null;
        try
        {
            using var doc = JsonDocument.Parse(json.ToArray(), new JsonDocumentOptions { MaxDepth = 8 });
            if (doc.RootElement.ValueKind != JsonValueKind.Object ||
                !doc.RootElement.TryGetProperty("endpoints", out var list) ||
                list.ValueKind != JsonValueKind.Array)
                return null;
            var entries = new List<ServerEntry>();
            foreach (var item in list.EnumerateArray())
            {
                if (entries.Count >= MaxEntries) break;
                if (item.ValueKind != JsonValueKind.Object) continue;
                var endpoint = Text(item, "endpoint");
                var key = Text(item, "key")?.Trim();
                // Only wss:// is a server for everyone; Normalize also turns "host:port" into wss://.
                var normalized = Texts.NormalizeServer(endpoint);
                if (normalized == null || !normalized.StartsWith("wss://", StringComparison.Ordinal)) continue;
                if (string.IsNullOrEmpty(key) || key.Any(char.IsControl)) continue;
                if (entries.Any(e => e.Endpoint == normalized)) continue;
                entries.Add(new ServerEntry(normalized, key));
            }
            return entries;
        }
        catch (JsonException)
        {
            return null;
        }
    }

    static string? Text(JsonElement o, string name) =>
        o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
}

/// <summary>
/// The list from <paramref name="url"/>, kept in <paramref name="cachePath"/> for when it cannot be
/// fetched. A fetched file that is not this kind of file changes nothing: what was kept stays.
/// </summary>
public sealed class ServerDirectory(string url, string cachePath,
    Func<string, CancellationToken, Task<byte[]>>? fetch = null) : IServerDirectory
{
    /// <summary>servers.json at the top of the project's repository.</summary>
    public const string DefaultUrl = "https://raw.githubusercontent.com/dollarkillerx/NexaDesk/main/servers.json";

    static readonly TimeSpan Timeout = TimeSpan.FromSeconds(8);

    readonly Func<string, CancellationToken, Task<byte[]>> _fetch = fetch ?? Download;

    public async Task<IReadOnlyList<ServerEntry>> LoadAsync(CancellationToken ct = default)
    {
        try
        {
            using var limit = CancellationTokenSource.CreateLinkedTokenSource(ct);
            limit.CancelAfter(Timeout);
            var body = await _fetch(url, limit.Token);
            if (ServerList.Parse(body) is { } fresh)
            {
                Keep(body);
                return fresh;
            }
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException
                                      or InvalidOperationException or UriFormatException)
        {
            if (ct.IsCancellationRequested) return [];
        }
        return Kept();
    }

    IReadOnlyList<ServerEntry> Kept()
    {
        try
        {
            if (!File.Exists(cachePath) || new FileInfo(cachePath).Length > ServerList.MaxBytes) return [];
            return ServerList.Parse(File.ReadAllBytes(cachePath)) ?? [];
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            return [];
        }
    }

    void Keep(byte[] body)
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(cachePath)!);
            var tmp = cachePath + ".tmp";
            File.WriteAllBytes(tmp, body);
            File.Move(tmp, cachePath, overwrite: true);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
        }
    }

    static async Task<byte[]> Download(string url, CancellationToken ct)
    {
        if (!url.StartsWith("https://", StringComparison.Ordinal)) throw new InvalidOperationException("https only");
        using var client = new HttpClient { Timeout = Timeout };
        using var response = await client.GetAsync(url, HttpCompletionOption.ResponseHeadersRead, ct);
        response.EnsureSuccessStatusCode();
        if (response.Content.Headers.ContentLength > ServerList.MaxBytes) throw new InvalidOperationException("too big");
        await using var stream = await response.Content.ReadAsStreamAsync(ct);
        using var buffer = new MemoryStream();
        var chunk = new byte[8192];
        int n;
        while ((n = await stream.ReadAsync(chunk, ct)) > 0)
        {
            buffer.Write(chunk, 0, n);
            if (buffer.Length > ServerList.MaxBytes) throw new InvalidOperationException("too big");
        }
        return buffer.ToArray();
    }
}
