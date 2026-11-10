using System.Text.RegularExpressions;
using Avalonia.Headless.XUnit;
using Avalonia.Styling;
using NexaDesk.Localization;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// Every string in every language, every key the code asks for, and no Chinese left in the
/// views or view models outside Localization/Strings.
/// </summary>
public partial class LocalizationTests
{
    static string AppDir()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !Directory.Exists(Path.Combine(dir.FullName, "NexaDesk.App"))) dir = dir.Parent;
        return Path.Combine(dir!.FullName, "NexaDesk.App");
    }

    static IEnumerable<string> Sources() =>
        Directory.EnumerateFiles(AppDir(), "*.*", SearchOption.AllDirectories)
            .Where(f => f.EndsWith(".cs") || f.EndsWith(".axaml"))
            .Where(f => !f.Contains($"{Path.DirectorySeparatorChar}obj{Path.DirectorySeparatorChar}")
                        && !f.Contains($"{Path.DirectorySeparatorChar}bin{Path.DirectorySeparatorChar}"));

    [GeneratedRegex(@"\{(\d+)(?:[,:][^}]*)?\}")]
    private static partial Regex Placeholder();

    [GeneratedRegex(@"L\.(?:Get|Format)\(\s*""([^""]+)""")]
    private static partial Regex CodeKey();

    [GeneratedRegex(@"\{l:T\s+(?:Key=)?([A-Za-z0-9_.]+)\s*\}")]
    private static partial Regex XamlKey();

    [GeneratedRegex(@"[一-鿿぀-ヿ]")]
    private static partial Regex Cjk();

    static SortedSet<string> Placeholders(string s) =>
        new(Placeholder().Matches(s).Select(m => m.Groups[1].Value));

    [Fact]
    public void Every_file_loads_and_no_key_is_defined_twice() => Assert.Empty(L.Problems);

    [Fact]
    public void Every_language_has_every_key_with_the_same_placeholders()
    {
        var tables = L.Tables;
        var zh = tables[L.Chinese];
        Assert.NotEmpty(zh);
        foreach (var (code, _) in L.Languages)
        {
            Assert.True(tables.ContainsKey(code), $"no strings for {code}");
            var t = tables[code];
            Assert.Empty(zh.Keys.Except(t.Keys).Select(k => $"{code} lacks {k}"));
            Assert.Empty(t.Keys.Except(zh.Keys).Select(k => $"{code} has extra {k}"));
            foreach (var (key, text) in zh)
            {
                Assert.True(t[key].Trim().Length > 0, $"{code}:{key} is empty");
                Assert.Equal(Placeholders(text), Placeholders(t[key]));
            }
        }
    }

    [Fact]
    public void Every_key_the_code_asks_for_exists()
    {
        var zh = L.Tables[L.Chinese];
        var missing = new List<string>();
        foreach (var file in Sources())
        {
            // Localization/ itself only has examples in its comments.
            if (file.Contains($"{Path.DirectorySeparatorChar}Localization{Path.DirectorySeparatorChar}")) continue;
            var text = File.ReadAllText(file);
            var keys = CodeKey().Matches(text).Select(m => m.Groups[1].Value)
                .Concat(XamlKey().Matches(text).Select(m => m.Groups[1].Value));
            missing.AddRange(keys.Where(k => !zh.ContainsKey(k)).Select(k => $"{Path.GetFileName(file)}: {k}"));
        }
        Assert.Empty(missing);
    }

    /// <summary>Comments may be in any language; text shown to people must come from the tables.</summary>
    [Fact]
    public void No_text_is_left_in_the_sources()
    {
        var left = new List<string>();
        foreach (var file in Sources())
        {
            if (file.Contains($"{Path.DirectorySeparatorChar}Localization{Path.DirectorySeparatorChar}")) continue;
            var inComment = false;
            var n = 0;
            foreach (var raw in File.ReadLines(file))
            {
                n++;
                var line = raw.Trim();
                if (file.EndsWith(".axaml"))
                {
                    // XML comments, possibly over several lines.
                    if (inComment) { if (line.Contains("-->")) inComment = false; continue; }
                    if (line.StartsWith("<!--")) { inComment = !line.Contains("-->"); continue; }
                }
                else
                {
                    if (line.StartsWith("//") || line.StartsWith("*") || line.StartsWith("/*")) continue;
                    var code = raw.Contains("//") ? raw[..raw.IndexOf("//", StringComparison.Ordinal)] : raw;
                    line = code;
                }
                if (Cjk().IsMatch(line)) left.Add($"{Path.GetFileName(file)}:{n}: {raw.Trim()}");
            }
        }
        Assert.Empty(left);
    }

    [Theory]
    [InlineData(new[] { "zh-Hans-CN", "en-US" }, "zh-Hans")]
    [InlineData(new[] { "zh-Hant-TW" }, "zh-Hans")]
    [InlineData(new[] { "ja-JP", "en-US" }, "ja")]
    [InlineData(new[] { "fr-FR", "ja-JP" }, "ja")]
    [InlineData(new[] { "en-GB" }, "en")]
    [InlineData(new[] { "fr-FR", "de-DE" }, "en")]
    [InlineData(new string[0], "en")]
    public void The_first_supported_preference_wins(string[] preferred, string expected) =>
        Assert.Equal(expected, L.Pick(preferred));

    [Fact]
    public void Macos_reports_its_preferred_languages()
    {
        if (!OperatingSystem.IsMacOS()) return;
        var list = SystemLanguages.Preferred();
        Assert.NotEmpty(list);
        Assert.All(list, l => Assert.Matches("^[a-z]{2,3}([-_][A-Za-z0-9]+)*$", l));
    }

    [Fact]
    public void Lookups_fall_back_and_formats_survive_bad_translations()
    {
        using (L.Scoped(L.English))
        {
            Assert.Equal("Language", L.Get("Settings.Language"));
            Assert.Equal("This Mac: x", L.Format("Settings.ThisDevice", "x"));
        }
        using (L.Scoped(L.Japanese)) Assert.Equal("言語", L.Get("Settings.Language"));
        Assert.Equal("语言", L.Get("Settings.Language"));
        Assert.Equal("No.Such.Key", L.Get("No.Such.Key"));
        using (L.Scoped("xx")) Assert.Equal("Language", L.Get("Settings.Language"));
    }

    [Fact]
    public void The_language_setting_is_saved_and_needs_a_restart()
    {
        var store = new MemorySettings(new AppSettings("wss://s.example", false, ""));
        var vm = new SettingsViewModel(store);
        Assert.Equal("", vm.SelectedLanguage.Code);
        Assert.Equal(["", "zh-Hans", "en", "ja"], vm.LanguageOptions.Select(o => o.Code));
        Assert.False(vm.LanguageRestartNeeded);
        vm.SelectedLanguage = vm.LanguageOptions.Single(o => o.Code == "ja");
        Assert.Equal(new AppSettings("wss://s.example", false, "ja"), store.Current);
        Assert.True(vm.LanguageRestartNeeded);
        vm.AnnounceOnLan = true;
        Assert.Equal("ja", store.Current.Language);
    }

    [Fact]
    public void An_unknown_language_in_the_file_reads_as_follow_system()
    {
        var dir = Path.Combine(Path.GetTempPath(), "nexa-lang-" + Guid.NewGuid());
        Directory.CreateDirectory(dir);
        try
        {
            var path = Path.Combine(dir, "settings.json");
            File.WriteAllText(path, """{ "signalServer": "", "language": "klingon" }""");
            Assert.Equal("", new SettingsStore(path).Load().Language);
            File.WriteAllText(path, """{ "signalServer": "", "language": "ja" }""");
            Assert.Equal("ja", new SettingsStore(path).Load().Language);
        }
        finally
        {
            Directory.Delete(dir, true);
        }
    }

    [Fact]
    public void Core_messages_are_worded_by_code_in_the_apps_language()
    {
        // Event{warning(11): {text "x", code GAME_WSS_RELAY, detail "d"}}
        var w = Assert.IsType<WarningEvent>(ClientCodec.DecodeEvent(Convert.FromHexString("5a080a017810011a0164")));
        Assert.Equal((WarningCode.GameWssRelay, "x", "d"), (w.Code, w.Text, w.Detail));
        Assert.Equal("游戏模式：UDP 被阻断，当前经 WSS（TCP）中继传输，延迟和卡顿会明显增加", Messages.Warning(w));
        using (L.Scoped(L.English)) Assert.StartsWith("Game mode: UDP is blocked", Messages.Warning(w));
        // Unknown codes, and old cores without one, show the text as sent.
        Assert.Equal("as sent", Messages.Warning(new WarningEvent(1, "as sent")));
        Assert.Equal("as sent", Messages.Warning(new WarningEvent(1, "as sent", (WarningCode)99)));
        using (L.Scoped(L.Japanese))
            Assert.Equal("ゲームモードでポインタをロックできません：denied",
                Messages.Warning(new WarningEvent(1, "x", WarningCode.PointerLock, "denied")));

        // AgentToUi{notice(7): {kind CONNECTION_FAILED, text "t", remote "r", reason "e"}}
        var n = Assert.IsType<AgentMessage.Notice>(AgentMessage.Decode(Convert.FromHexString("3a0b08011201741a0172220165")));
        Assert.Equal("来自 r 的连接失败：e", Messages.Notice(n));
        using (L.Scoped(L.English)) Assert.Equal("A connection from r failed: e", Messages.Notice(n));
        Assert.Equal("plain", Messages.Notice(new AgentMessage.Notice(NoticeKind.Warning, "plain")));

        Assert.Equal("keymap.toml 无法使用，已显示内置规则：line 3",
            Messages.KeymapError(new KeymapView([], "english", "line 3")));
        Assert.Equal("english", Messages.KeymapError(new KeymapView([], "english")));
    }

    [Fact]
    public void Clipboard_notices_are_worded_by_code()
    {
        // Event{clipboard(7): {text "x", code TOO_LARGE, kind "png", size 3 MiB}}
        var n = Assert.IsType<ClipboardNoticeEvent>(ClientCodec.DecodeEvent(Convert.FromHexString("3a0f0a017810021a03706e67208080c001")));
        Assert.Equal((ClipboardCode.TooLarge, "png", 3145728UL), (n.Code, n.Kind, n.Size));
        Assert.Equal("图片太大（3.0 MB），没有同步，请改用文件传输", Messages.Clipboard(n));
        using (L.Scoped(L.English)) Assert.Equal("image is too large (3.0 MB) to sync; use file transfer instead", Messages.Clipboard(n));
        Assert.Equal("Debug(1)", Messages.Clipboard(new ClipboardNoticeEvent(1, "Debug(1)")));
        var vm = new SessionViewModel(new FakeSession(1), Samples.Connected(1), "Studio", () => Samples.T0);
        vm.OnEvent(new ClipboardNoticeEvent(1, "x", ClipboardCode.SensitiveKeptLocal));
        Assert.Equal("剪贴板：敏感内容只保留在本机，没有同步", vm.ClipboardText);
    }

    [Fact]
    public void A_session_shows_core_warnings_in_the_apps_language()
    {
        var vm = new SessionViewModel(new FakeSession(1), Samples.Connected(1), "Studio", () => Samples.T0);
        vm.OnEvent(new WarningEvent(1, "english", WarningCode.GameDerp));
        Assert.Equal("游戏模式：Tailscale 当前经 DERP 中继，延迟和带宽受限", vm.Warning);
    }

    /// <summary>The main window, the session toolbar and the consent dialog in each language, to look at.</summary>
    [AvaloniaFact]
    public async Task Windows_render_in_every_language()
    {
        foreach (var (code, _) in L.Languages)
        {
            using var scope = L.Scoped(code);
            App.UseFontsFor(Avalonia.Application.Current!, code);
            var client = new FakeClient();
            var link = new FakeLink();
            link.ConnectResults.Enqueue(null);
            var main = new MainViewModel(client, new MemoryRecent(
                    new RecentDevice("192.168.1.20:47800", "Studio Mac mini", Samples.HostKey, Samples.T0.ToUnixTimeSeconds() - 7200)),
                link, new FakeProcess(), new FakePermissions(), new RecordingWindows(),
                new FakeAddresses("192.168.1.35", "100.101.22.7", "127.0.0.1"), "MacBook Pro", () => Samples.T0);
            await main.Host.StartCommand.ExecuteAsync(null);
            link.Raise(Samples.Status() with { Listen = "[::]:47800" });
            client.Raise(new ReadyEvent(0, Samples.ViewerKey, [new KnownHost(Samples.HostKey, "Studio Mac mini", 0)], ""));
            client.Raise(new LanDevicesEvent(0, [new LanDevice("Studio Mac mini", "192.168.1.20:47800", "macos", "0123456789abcdef0123456789abcdef")]));
            client.Raise(new TailnetDevicesEvent(0, [new TailnetDevice("iMac", "100.64.0.2:47800", false, "macOS", "imac.tail1234.ts.net")],
                TailscaleState.Running, "studio", "studio.tail1234.ts.net", ["100.101.22.7"]));
            var w = new MainWindow { DataContext = main };
            w.Show();
            RenderTestsShim.Save(w, $"i18n-main-{code}", ThemeVariant.Light);
            w.Close();

            var session = new SessionViewModel(new FakeSession(1), Samples.Connected(1), "Studio Mac mini", () => Samples.T0);
            var t = new ToolbarWindow { DataContext = session };
            t.Show();
            RenderTestsShim.Save(t, $"i18n-toolbar-{code}", ThemeVariant.Light);
            t.Close();
        }
        App.UseFontsFor(Avalonia.Application.Current!, L.Chinese);
    }
}
