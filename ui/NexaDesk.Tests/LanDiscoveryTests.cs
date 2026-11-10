using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>LAN discovery (§4.11): the viewer's list and the host's announce setting.</summary>
public class LanDiscoveryTests
{
    static byte[] Bytes(string hex) => Convert.FromHexString(hex.Replace(" ", ""));

    static MainViewModel Main(FakeLink link, FakeProcess process, MemorySettings settings) =>
        new(new FakeClient(), new MemoryRecent(), link, process, new FakePermissions(), new RecordingWindows(),
            new FakeAddresses(), "Mac", () => Samples.T0, settings);

    [Fact]
    public void Lan_event_decodes()
    {
        // Event{lan(43): {devices: [{name "a", address "b", os "c", device_id "d"}]}}
        var e = Assert.IsType<LanDevicesEvent>(ClientCodec.DecodeEvent(Bytes("da020e 0a0c 0a0161 120162 1a0163 220164")));
        Assert.Equal(0ul, e.Tag);
        Assert.Equal(new LanDevice("a", "b", "c", "d"), Assert.Single(e.Devices));
        // Unknown fields inside an entry are skipped.
        var withExtra = Assert.IsType<LanDevicesEvent>(ClientCodec.DecodeEvent(Bytes("da020a 0a08 0a0161 2a0178 3001")));
        Assert.Equal(new LanDevice("a", "", "", ""), Assert.Single(withExtra.Devices));
    }

    [Fact]
    public void Lan_devices_fill_the_address_or_connect_a_paired_one()
    {
        var client = new FakeClient();
        var recent = new MemoryRecent(
            new RecentDevice("192.168.1.5:47800", "studio", Samples.HostKey, Samples.T0.ToUnixTimeSeconds() - 60));
        var vm = new ConnectViewModel(client, recent, new RecordingWindows(), () => Samples.T0);
        Assert.False(vm.HasLan);
        client.Raise(new ReadyEvent(0, Samples.ViewerKey, [new KnownHost(Samples.HostKey, "studio", 0)], ""));
        client.Raise(new LanDevicesEvent(0, [
            new LanDevice("studio", "192.168.1.5:47800", "macos", "0123456789abcdef0123456789abcdef"),
            new LanDevice("", "192.168.1.6:47800", "", "fedcba9876543210fedcba9876543210"),
        ]));
        Assert.True(vm.HasLan);
        Assert.Equal("192.168.1.5:47800 · macos", vm.Lan[0].Detail);
        Assert.Equal("192.168.1.6:47800", vm.Lan[1].Name);

        var focused = false;
        vm.FocusCodeRequested += () => focused = true;
        vm.Lan[1].ConnectCommand.Execute(null);
        Assert.Empty(client.Requests);
        Assert.Equal("192.168.1.6:47800", vm.Address);
        Assert.True(focused);

        vm.Lan[0].ConnectCommand.Execute(null);
        var req = Assert.Single(client.Requests);
        Assert.Equal("192.168.1.5:47800", req.Address);
        Assert.Equal(new ConnectAuth.KnownHost(Samples.HostKey), req.Auth);

        client.Raise(new LanDevicesEvent(0, []));
        Assert.False(vm.HasLan);
    }

    [Fact]
    public async Task The_announce_setting_is_saved_and_reaches_the_agent()
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var settings = new MemorySettings();
        var main = Main(link, process, settings);
        Assert.True(main.Settings.AnnounceOnLan, "on by default");

        main.Settings.AnnounceOnLan = false;
        Assert.False(settings.Current.AnnounceOnLan);
        await main.Host.StartCommand.ExecuteAsync(null);
        Assert.False(Assert.Single(process.Announces));
        Assert.False(main.Host.RestartNeeded);

        main.Settings.AnnounceOnLan = true;
        Assert.True(main.Host.RestartNeeded, "applies on the next start");
        main.Settings.AnnounceOnLan = false;
        Assert.False(main.Host.RestartNeeded);
    }

    [Fact]
    public void Saving_the_server_keeps_the_announce_setting()
    {
        var settings = new MemorySettings(new AppSettings("", AnnounceOnLan: false));
        var main = Main(new FakeLink(), new FakeProcess(), settings);
        Assert.False(main.Settings.AnnounceOnLan);
        main.Settings.ServerInput = "wss://s.example";
        main.Settings.SaveCommand.Execute(null);
        Assert.Equal(new AppSettings("wss://s.example", false), settings.Current);
    }

    [Fact]
    public void A_settings_file_without_the_field_reads_as_announce_on()
    {
        var dir = Path.Combine(Path.GetTempPath(), "nexa-settings-" + Guid.NewGuid());
        Directory.CreateDirectory(dir);
        try
        {
            var path = Path.Combine(dir, "settings.json");
            File.WriteAllText(path, """{ "signalServer": "wss://s.example" }""");
            var store = new SettingsStore(path);
            Assert.Equal(new AppSettings("wss://s.example", true), store.Load());
            store.Save(new AppSettings("wss://s.example", false));
            Assert.False(store.Load().AnnounceOnLan);
        }
        finally
        {
            Directory.Delete(dir, true);
        }
    }
}
