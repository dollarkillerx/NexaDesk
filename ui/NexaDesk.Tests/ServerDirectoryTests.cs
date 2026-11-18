using System.Text;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>The published list of servers: what counts as an entry, and what is kept for when GitHub cannot be reached.</summary>
public class ServerDirectoryTests
{
    static IReadOnlyList<ServerEntry>? Parse(string json) => ServerList.Parse(Encoding.UTF8.GetBytes(json));

    [Fact]
    public void A_listed_server_has_an_address_and_a_key()
    {
        var list = Parse("""
            { "endpoints": [
                { "endpoint": "wss://150.251.158.58:8443", "key": "  a long key of its own  ", "name": "official" },
                { "endpoint": "signal.example.com", "key": "another key" }
            ], "updated": "2026-11-18" }
            """);
        Assert.NotNull(list);
        Assert.Equal(
            [("wss://150.251.158.58:8443", "a long key of its own"), ("wss://signal.example.com", "another key")],
            list.Select(e => (e.Endpoint, e.Key)));
    }

    [Fact]
    public void An_entry_without_a_key_is_never_used_and_the_others_still_are()
    {
        var list = Parse("""
            { "endpoints": [
                { "endpoint": "wss://a.example" },
                { "endpoint": "wss://b.example", "key": "" },
                { "endpoint": "wss://c.example", "key": "   " },
                { "endpoint": "wss://d.example", "key": null },
                { "endpoint": "wss://e.example", "key": 12345 },
                { "endpoint": "wss://f.example", "key": "bad\u0007key" },
                { "endpoint": "wss://good.example", "key": "the one that counts" }
            ] }
            """);
        Assert.Equal(["wss://good.example"], list!.Select(e => e.Endpoint));
    }

    [Fact]
    public void Only_wss_servers_and_each_once()
    {
        var list = Parse("""
            { "endpoints": [
                { "endpoint": "ws://evil.example:80", "key": "key key key" },
                { "endpoint": "https://web.example", "key": "key key key" },
                { "endpoint": "wss://a b.example", "key": "key key key" },
                { "endpoint": "", "key": "key key key" },
                "wss://not-an-object.example",
                { "endpoint": "wss://dup.example", "key": "first key" },
                { "endpoint": "WSS://dup.example", "key": "second key" }
            ] }
            """);
        var only = Assert.Single(list!);
        Assert.Equal(("wss://dup.example", "first key"), (only.Endpoint, only.Key));
    }

    [Fact]
    public void A_file_that_is_not_a_list_is_not_read_and_an_empty_list_is_a_list()
    {
        foreach (var bad in new[] { "", "not json", "[]", "{}", """{ "endpoints": {} }""", """{ "endpoints": "x" }""" })
            Assert.Null(Parse(bad));
        Assert.Empty(Parse("""{ "endpoints": [] }""")!);
        Assert.Null(ServerList.Parse(new byte[ServerList.MaxBytes + 1]));
    }

    [Fact]
    public void No_more_than_the_limit_is_taken()
    {
        var items = string.Join(",", Enumerable.Range(0, 40).Select(i => $$"""{ "endpoint": "wss://s{{i}}.example", "key": "key number {{i}}" }"""));
        Assert.Equal(ServerList.MaxEntries, Parse($$"""{ "endpoints": [{{items}}] }""")!.Count);
    }

    [Fact]
    public void An_entry_never_prints_its_key()
    {
        Assert.DoesNotContain("secret key", new ServerEntry("wss://a.example", "secret key").ToString());
    }

    static (ServerDirectory Directory, string Cache, DirectoryInfo Dir) Directory_(
        Func<string, CancellationToken, Task<byte[]>> fetch)
    {
        var dir = Directory.CreateTempSubdirectory();
        var cache = Path.Combine(dir.FullName, "sub", "servers.json");
        return (new ServerDirectory("https://example.test/servers.json", cache, fetch), cache, dir);
    }

    static Task<byte[]> Body(string json) => Task.FromResult(Encoding.UTF8.GetBytes(json));

    const string One = """{ "endpoints": [ { "endpoint": "wss://a.example", "key": "key for a" } ] }""";

    [Fact]
    public async Task A_fetched_list_is_used_and_kept_for_the_next_time_it_cannot_be_fetched()
    {
        var online = true;
        var (directory, cache, dir) = Directory_((_, _) => online ? Body(One) : throw new HttpRequestException("offline"));
        try
        {
            Assert.Equal("wss://a.example", Assert.Single(await directory.LoadAsync()).Endpoint);
            Assert.True(File.Exists(cache));
            online = false;
            var kept = Assert.Single(await directory.LoadAsync());
            Assert.Equal(("wss://a.example", "key for a"), (kept.Endpoint, kept.Key));
        }
        finally { dir.Delete(true); }
    }

    [Fact]
    public async Task A_list_that_became_empty_replaces_the_kept_one_and_a_bad_file_does_not()
    {
        var body = One;
        var (directory, _, dir) = Directory_((_, _) => Body(body));
        try
        {
            Assert.Single(await directory.LoadAsync());
            // Garbage changes nothing: the kept list stays in use.
            body = "<html>rate limited</html>";
            Assert.Single(await directory.LoadAsync());
            // A published empty list means none, and says so from then on.
            body = """{ "endpoints": [] }""";
            Assert.Empty(await directory.LoadAsync());
            body = "not json";
            Assert.Empty(await directory.LoadAsync());
        }
        finally { dir.Delete(true); }
    }

    [Fact]
    public async Task Nothing_listed_and_nothing_kept_is_an_empty_list_and_a_slow_fetch_gives_up()
    {
        var (directory, _, dir) = Directory_(async (_, ct) =>
        {
            await Task.Delay(Timeout.Infinite, ct);
            return [];
        });
        try
        {
            using var cancel = new CancellationTokenSource(TimeSpan.FromMilliseconds(100));
            Assert.Empty(await directory.LoadAsync(cancel.Token));
        }
        finally { dir.Delete(true); }
    }

    [Fact]
    public async Task A_damaged_cache_reads_as_nothing()
    {
        var (directory, cache, dir) = Directory_((_, _) => throw new HttpRequestException("offline"));
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(cache)!);
            File.WriteAllText(cache, "{{{");
            Assert.Empty(await directory.LoadAsync());
            File.WriteAllText(cache, One);
            Assert.Single(await directory.LoadAsync());
        }
        finally { dir.Delete(true); }
    }

    // ---- the settings window's use of the list: off until switched on ----

    sealed class FakeDirectory(params ServerEntry[] entries) : IServerDirectory
    {
        public int Loads;
        public Task<IReadOnlyList<ServerEntry>> LoadAsync(CancellationToken ct = default)
        {
            Loads++;
            return Task.FromResult<IReadOnlyList<ServerEntry>>(entries);
        }
    }

    static readonly ServerEntry Listed = new("wss://signal.example.com", "a key for everyone");

    static SettingsViewModel Settings(FakeDirectory directory, MemorySettings? store = null, MemorySavedPasswords? keychain = null,
        IServerProbe? probe = null) =>
        new(store ?? new MemorySettings(), keychain ?? new MemorySavedPasswords(), directory, probe);

    sealed class FakeProbe(Func<string, ProbeResult> answer) : IServerProbe
    {
        public readonly List<string> Asked = [];
        public Task<ProbeResult> ProbeAsync(string endpoint, CancellationToken ct = default)
        {
            lock (Asked) Asked.Add(endpoint);
            return Task.FromResult(answer(endpoint));
        }
    }

    [Fact]
    public async Task Each_listed_server_shows_whether_it_answers_and_how_far_away_it_is()
    {
        var other = new ServerEntry("wss://down.example.com", "another key for it");
        var probe = new FakeProbe(e => e.Contains("down") ? new ProbeResult(false, null, "unreachable") : new ProbeResult(true, 38, null));
        var s = Settings(new FakeDirectory(Listed, other), probe: probe, store: new MemorySettings(new AppSettings(UseServerList: true)));
        await s.RefreshListAsync();
        var health = s.ListedServers.ToDictionary(x => x.Endpoint, x => x.Health);
        Assert.Contains("38", health["wss://signal.example.com"]);
        Assert.DoesNotContain("38", health["wss://down.example.com"]);
        Assert.NotEqual("", health["wss://down.example.com"]);
        // Nothing is looked at outside the published mode.
        var quiet = new FakeProbe(_ => new ProbeResult(true, 1, null));
        var own = Settings(new FakeDirectory(Listed), probe: quiet);
        await own.RefreshListAsync();
        Assert.Empty(quiet.Asked);
    }

    [Fact]
    public async Task A_server_typed_by_hand_can_be_tested_before_it_is_saved()
    {
        var probe = new FakeProbe(_ => new ProbeResult(true, 120, null));
        var s = Settings(new FakeDirectory(), probe: probe);
        Assert.False(s.TestCommand.CanExecute(null));
        s.ServerInput = "signal.example.com";
        Assert.True(s.TestCommand.CanExecute(null));
        await s.TestCommand.ExecuteAsync(null);
        Assert.Equal(["wss://signal.example.com"], probe.Asked);
        Assert.Contains("120", s.TestResult);
        Assert.Equal("", s.SavedServer);
        // Typing again clears the old answer.
        s.ServerInput = "other.example.com";
        Assert.Null(s.TestResult);
    }

    // ---- the probe itself, against servers on this machine ----

    static int FreePort()
    {
        var l = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, 0);
        l.Start();
        var port = ((System.Net.IPEndPoint)l.LocalEndpoint).Port;
        l.Stop();
        return port;
    }

    [Fact]
    public async Task Nothing_listening_is_unreachable()
    {
        var r = await new ServerProbe().ProbeAsync($"ws://127.0.0.1:{FreePort()}", TestContext.Current.CancellationToken);
        Assert.Equal(new ProbeResult(false, null, "unreachable"), r);
        Assert.False((await new ServerProbe().ProbeAsync("not a url", TestContext.Current.CancellationToken)).Available);
    }

    [Fact]
    public async Task Something_that_is_not_a_signaling_server_is_told_apart()
    {
        var listener = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, 0);
        listener.Start();
        var port = ((System.Net.IPEndPoint)listener.LocalEndpoint).Port;
        var accepting = Task.Run(async () =>
        {
            for (var i = 0; i < 4; i++)
            {
                using var c = await listener.AcceptTcpClientAsync();
                await c.GetStream().WriteAsync("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"u8.ToArray());
            }
        }, TestContext.Current.CancellationToken);
        try
        {
            var r = await new ServerProbe().ProbeAsync($"ws://127.0.0.1:{port}", TestContext.Current.CancellationToken);
            Assert.False(r.Available);
            Assert.Equal("notsignal", r.Problem);
            Assert.NotNull(r.Milliseconds);
        }
        finally { listener.Stop(); }
    }

    [Fact]
    public async Task A_server_that_sends_a_challenge_is_available_with_a_delay()
    {
        var port = FreePort();
        using var http = new System.Net.HttpListener();
        http.Prefixes.Add($"http://127.0.0.1:{port}/");
        http.Start();
        var serving = Task.Run(async () =>
        {
            while (http.IsListening)
            {
                System.Net.HttpListenerContext ctx;
                try { ctx = await http.GetContextAsync(); } catch (Exception) { return; }
                if (!ctx.Request.IsWebSocketRequest) { ctx.Response.StatusCode = 400; ctx.Response.Close(); continue; }
                var ws = (await ctx.AcceptWebSocketAsync(null)).WebSocket;
                await ws.SendAsync(new byte[34], System.Net.WebSockets.WebSocketMessageType.Binary, true, CancellationToken.None);
                await Task.Delay(200);
                ws.Abort();
            }
        }, TestContext.Current.CancellationToken);
        var r = await new ServerProbe().ProbeAsync($"ws://127.0.0.1:{port}", TestContext.Current.CancellationToken);
        http.Stop();
        Assert.True(r.Available, r.Problem);
        Assert.InRange(r.Milliseconds ?? -1, 1, 1000);
        Assert.Null(r.Problem);
    }

    [Fact]
    public async Task The_list_is_never_fetched_unless_the_published_mode_is_chosen()
    {
        var directory = new FakeDirectory(Listed);
        var store = new MemorySettings();
        var s = Settings(directory, store);
        Assert.True(s.IsModeNone);
        await s.RefreshListAsync();
        s.IsModeOwn = true;
        await s.RefreshListAsync();
        Assert.Equal(0, directory.Loads);
        Assert.Null(s.ListStatus);

        s.IsModePublished = true;
        await s.RefreshListAsync();
        Assert.True(directory.Loads >= 1);
        Assert.Equal([Listed], s.ListedServers.Select(x => x.Entry));
        Assert.True(store.Current.UseServerList);

        // Another mode: the list goes away, and a new window starts in that mode.
        s.IsModeOwn = true;
        await s.RefreshListAsync();
        Assert.Empty(s.ListedServers);
        Assert.False(store.Current.UseServerList);
    }

    [Fact]
    public async Task Picking_a_published_server_uses_it_at_once_with_no_Save()
    {
        var store = new MemorySettings(new AppSettings(UseServerList: true));
        var keychain = new MemorySavedPasswords();
        var s = Settings(new FakeDirectory(Listed), store, keychain);
        Assert.True(s.IsModePublished);
        await s.RefreshListAsync();
        var changed = new List<string>();
        s.ServerChanged += changed.Add;

        s.SelectedListed = s.ListedServers[0];
        await WaitFor(() => changed.Count == 1);
        Assert.Equal("wss://signal.example.com", s.SavedServer);
        Assert.Equal("a key for everyone", s.SavedSecret);
        Assert.Equal("wss://signal.example.com", store.Current.SignalServer);
        Assert.Equal("a key for everyone", keychain.Kept["signal-access:wss://signal.example.com"]);
        Assert.False(s.IsDirty);
    }

    [Fact]
    public async Task The_server_in_use_shows_as_chosen_in_the_list_without_being_chosen_again()
    {
        var store = new MemorySettings(new AppSettings("wss://signal.example.com", UseServerList: true));
        var s = Settings(new FakeDirectory(Listed), store);
        var changed = 0;
        s.ServerChanged += _ => changed++;
        await s.RefreshListAsync();
        Assert.Equal(Listed, s.SelectedListed?.Entry);
        await Task.Delay(50, TestContext.Current.CancellationToken);
        Assert.Equal(0, changed);
    }

    [Fact]
    public async Task Choosing_no_server_gives_the_server_and_its_key_up_at_once()
    {
        var store = new MemorySettings(new AppSettings("wss://signal.example.com"));
        var keychain = new MemorySavedPasswords();
        keychain.Kept["signal-access:wss://signal.example.com"] = "a key for everyone";
        var s = Settings(new FakeDirectory(Listed), store, keychain);
        Assert.True(s.IsModeOwn);
        s.IsModeNone = true;
        await WaitFor(() => s.SavedServer == "");
        Assert.Equal("", store.Current.SignalServer);
        Assert.Empty(keychain.Kept);
    }

    [Fact]
    public async Task A_server_set_up_before_the_list_starts_as_my_own_and_an_empty_list_says_so()
    {
        Assert.True(Settings(new FakeDirectory(), new MemorySettings(new AppSettings("wss://mine.example"))).IsModeOwn);
        var s = Settings(new FakeDirectory(), new MemorySettings(new AppSettings(UseServerList: true)));
        await s.RefreshListAsync();
        Assert.Empty(s.ListedServers);
        Assert.NotNull(s.ListStatus);

        // A build without a list hides the choice and never starts in it.
        var none = new SettingsViewModel(new MemorySettings(new AppSettings(UseServerList: true)));
        Assert.False(none.CanUseServerList);
        Assert.False(none.IsModePublished);
    }

    static async Task WaitFor(Func<bool> done)
    {
        for (var i = 0; i < 100 && !done(); i++) await Task.Delay(20, TestContext.Current.CancellationToken);
        Assert.True(done());
    }
}
