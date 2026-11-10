using System.Globalization;
using System.Text.Json;

namespace NexaDesk.Localization;

/// <summary>
/// The app's text in Simplified Chinese, English and Japanese.
///
/// Strings live in <c>Localization/Strings/&lt;language&gt;/&lt;area&gt;.json</c>, one flat
/// object of <c>"Area.Name": "text"</c> per file, embedded in the assembly (no satellite
/// assemblies, so NativeAOT needs nothing special). The language is chosen once at start:
/// the setting, else the first of macOS's preferred languages that is supported, else
/// English. A key missing in that language falls back to English, then Chinese, then the
/// key itself, so a gap shows up as text rather than a crash.
///
/// Until <see cref="Use"/> or <see cref="Init"/> is called the language is Chinese, which
/// is what the tests read.
/// </summary>
public static class L
{
    public const string Chinese = "zh-Hans";
    public const string English = "en";
    public const string Japanese = "ja";

    /// <summary>Every supported language, each named in itself.</summary>
    public static IReadOnlyList<(string Code, string Name)> Languages { get; } =
        [(Chinese, "简体中文"), (English, "English"), (Japanese, "日本語")];

    const string ResourcePrefix = "i18n/";

    static readonly Lazy<Loaded> Strings = new(Load);
    static readonly AsyncLocal<string?> Scope = new();
    static string _current = Chinese;

    public static string Current
    {
        get => Scope.Value ?? _current;
        private set => _current = value;
    }

    /// <summary>
    /// Another language for this async flow only, until disposed: renders a window in English
    /// while tests on other threads keep reading Chinese.
    /// </summary>
    internal static IDisposable Scoped(string code)
    {
        var before = Scope.Value;
        Scope.Value = IsSupported(code) ? code : English;
        return new Restore(() => Scope.Value = before);
    }

    sealed class Restore(Action undo) : IDisposable
    {
        public void Dispose() => undo();
    }

    public static bool IsSupported(string code) => Languages.Any(l => l.Code == code);

    /// <summary>Switches language. Text already on screen stays as it was.</summary>
    public static void Use(string code) => Current = IsSupported(code) ? code : English;

    /// <summary>The setting if it names a language, else the best of <paramref name="preferred"/>.</summary>
    public static void Init(string setting, IEnumerable<string> preferred) =>
        Use(IsSupported(setting) ? setting : Pick(preferred));

    /// <summary>
    /// The first supported language in a macOS preference list ("zh-Hans-CN", "ja-JP", "en-GB"…).
    /// Traditional Chinese reads Simplified rather than English: closer for most readers.
    /// </summary>
    public static string Pick(IEnumerable<string> preferred)
    {
        foreach (var raw in preferred)
        {
            var p = raw.Replace('_', '-').ToLowerInvariant();
            if (p == "zh" || p.StartsWith("zh-", StringComparison.Ordinal)) return Chinese;
            if (p == "ja" || p.StartsWith("ja-", StringComparison.Ordinal)) return Japanese;
            if (p == "en" || p.StartsWith("en-", StringComparison.Ordinal)) return English;
        }
        return English;
    }

    public static string Get(string key) =>
        Lookup(Current, key) ?? Lookup(English, key) ?? Lookup(Chinese, key) ?? key;

    /// <summary><see cref="Get"/> with <c>{0}</c>-style placeholders filled in.</summary>
    public static string Format(string key, params object?[] args)
    {
        var pattern = Get(key);
        try
        {
            return string.Format(CultureInfo.InvariantCulture, pattern, args);
        }
        catch (FormatException)
        {
            // A broken translation must not take the window down with it.
            return pattern;
        }
    }

    static string? Lookup(string language, string key) =>
        Strings.Value.Table.TryGetValue(language, out var t) && t.TryGetValue(key, out var s) ? s : null;

    /// <summary>Every string of every language, for the completeness tests.</summary>
    internal static IReadOnlyDictionary<string, Dictionary<string, string>> Tables => Strings.Value.Table;

    /// <summary>Keys defined twice, or files that are not a flat object of strings.</summary>
    internal static IReadOnlyList<string> Problems => Strings.Value.Problems;

    sealed record Loaded(Dictionary<string, Dictionary<string, string>> Table, List<string> Problems);

    static Loaded Load()
    {
        var table = new Dictionary<string, Dictionary<string, string>>();
        var problems = new List<string>();
        var assembly = typeof(L).Assembly;
        foreach (var name in assembly.GetManifestResourceNames().Order(StringComparer.Ordinal))
        {
            if (!name.StartsWith(ResourcePrefix, StringComparison.Ordinal)) continue;
            // i18n/<language>/<area>.json
            var parts = name[ResourcePrefix.Length..].Split('/', '\\');
            if (parts.Length != 2) continue;
            var language = parts[0];
            if (!table.TryGetValue(language, out var strings)) table[language] = strings = new();
            try
            {
                using var stream = assembly.GetManifestResourceStream(name)!;
                using var doc = JsonDocument.Parse(stream, new JsonDocumentOptions { CommentHandling = JsonCommentHandling.Skip });
                if (doc.RootElement.ValueKind != JsonValueKind.Object)
                {
                    problems.Add($"{name}: not an object");
                    continue;
                }
                foreach (var p in doc.RootElement.EnumerateObject())
                {
                    if (p.Value.ValueKind != JsonValueKind.String)
                        problems.Add($"{name}: {p.Name} is not a string");
                    else if (!strings.TryAdd(p.Name, p.Value.GetString()!))
                        problems.Add($"{name}: {p.Name} is defined twice for {language}");
                }
            }
            catch (JsonException e)
            {
                problems.Add($"{name}: {e.Message}");
            }
        }
        return new Loaded(table, problems);
    }
}
