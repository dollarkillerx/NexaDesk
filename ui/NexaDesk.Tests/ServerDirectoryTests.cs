using System.Text;
using NexaDesk.Services;
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

    [Fact]
    public async Task The_list_is_never_fetched_until_it_is_switched_on()
    {
        var directory = new FakeDirectory(Listed);
        var store = new MemorySettings();
        var s = new NexaDesk.ViewModels.SettingsViewModel(store, new MemorySavedPasswords(), directory);
        Assert.False(s.UseServerList);
        await s.RefreshListAsync();
        Assert.Equal(0, directory.Loads);
        Assert.Null(s.ListStatus);

        s.UseServerList = true;
        await s.RefreshListAsync();
        Assert.True(directory.Loads >= 1);
        Assert.Equal([Listed], s.ListedServers);
        Assert.True(store.Current.UseServerList);

        // Off again: the list goes away, and a new settings window starts with it off.
        s.UseServerList = false;
        await s.RefreshListAsync();
        Assert.Empty(s.ListedServers);
        Assert.False(store.Current.UseServerList);
    }

    [Fact]
    public async Task Picking_a_listed_server_fills_the_boxes_and_saves_nothing_until_Save()
    {
        var store = new MemorySettings(new AppSettings(UseServerList: true));
        var keychain = new MemorySavedPasswords();
        var s = new NexaDesk.ViewModels.SettingsViewModel(store, keychain, new FakeDirectory(Listed));
        Assert.True(s.UseServerList);
        await s.RefreshListAsync();
        s.SelectedListed = s.ListedServers[0];
        Assert.Equal(("wss://signal.example.com", "a key for everyone"), (s.ServerInput, s.SecretInput));
        Assert.Equal("", s.SavedServer);
        Assert.Empty(keychain.Kept);
        Assert.True(s.SaveCommand.CanExecute(null));
        s.SaveCommand.Execute(null);
        Assert.Equal("wss://signal.example.com", s.SavedServer);
        Assert.Equal("a key for everyone", s.SavedSecret);
    }

    [Fact]
    public async Task An_empty_list_says_so_and_a_build_without_a_list_hides_the_switch()
    {
        var s = new NexaDesk.ViewModels.SettingsViewModel(
            new MemorySettings(new AppSettings(UseServerList: true)), null, new FakeDirectory());
        await s.RefreshListAsync();
        Assert.Empty(s.ListedServers);
        Assert.NotNull(s.ListStatus);

        var none = new NexaDesk.ViewModels.SettingsViewModel(
            new MemorySettings(new AppSettings(UseServerList: true)));
        Assert.False(none.CanUseServerList);
        Assert.False(none.UseServerList);
    }
}
