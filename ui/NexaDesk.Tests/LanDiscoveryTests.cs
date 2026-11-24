using NexaDesk.Localization;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Styling;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>LAN discovery (§4.11): the viewer's list and the host's announce setting.</summary>
public class LanDiscoveryTests
{
    static byte[] Bytes(string hex) => Convert.FromHexString(hex.Replace(" ", ""));

    static MainViewModel Main(FakeLink link, FakeProcess process, MemorySettings settings) =>
        new(new FakeClient(), new MemoryRecent(), link, process, new FakePermissions(), new RecordingWindows(),
            new FakeAddresses(), "Mac", () => Samples.T0, settings);

    [AvaloniaFact]
    public void Main_window_lists_lan_and_tailnet_devices()
    {
        foreach (var theme in new[] { ThemeVariant.Light, ThemeVariant.Dark })
        {
            var app = Avalonia.Application.Current!;
            var before = app.RequestedThemeVariant;
            app.RequestedThemeVariant = theme;
            try
            {
                var client = new FakeClient();
                var recent = new MemoryRecent(new RecentDevice("192.168.1.5:47800", "工作室 Mac mini", Samples.HostKey,
                    Samples.T0.ToUnixTimeSeconds() - 60));
                var main = new MainViewModel(client, recent, new FakeLink(), new FakeProcess(), new FakePermissions(),
                    new RecordingWindows(), new FakeAddresses("192.168.1.35", "100.101.22.7"), "王小明的 MacBook Pro", () => Samples.T0);
                client.Raise(new ReadyEvent(0, Samples.ViewerKey, [new KnownHost(Samples.HostKey, "工作室 Mac mini", 0)], ""));
                // Empty first: looking, and why nothing may be found.
                client.Raise(new LanDevicesEvent(0, []));
                var empty = new MainWindow { DataContext = main };
                empty.Show();
                RenderTestsShim.Save(empty, "main-lan-empty", theme);
                Assert.True(empty.FindControl<StackPanel>("LanEmpty")!.IsVisible);
                empty.Close();
                client.Raise(new LanDevicesEvent(0, [
                    new LanDevice("会议室 PC", "192.168.1.31:47800", "windows", "0123456789abcdef0123456789abcdef"),
                    new LanDevice("工作室 Mac mini", "192.168.1.20:47800", "macos", HostDeviceId),
                ]));
                main.Connect.Lan[1].IsEnteringCode = true;
                client.Raise(new TailnetDevicesEvent(0, [
                    new TailnetDevice("家里的 iMac", "100.64.0.2:47800", false, "macOS", "imac.tail1234.ts.net"),
                ]));
                var w = new MainWindow { DataContext = main };
                w.Show();
                RenderTestsShim.Save(w, "main-lan", theme);
                Assert.True(w.FindControl<StackPanel>("LanSection")!.IsVisible);
                w.Close();
            }
            finally
            {
                app.RequestedThemeVariant = before;
            }
        }
    }

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

    /// <summary>nexa-auth's DevicePublicKey::device_id of Samples.HostKey.</summary>
    const string HostDeviceId = "30e9e595e05745406321cb47c3e68adc";

    [Fact]
    public void A_hosts_device_id_is_made_from_its_key_as_the_core_makes_it()
    {
        Assert.Equal(HostDeviceId, ConnectViewModel.DeviceIdOf(Samples.HostKey));
        Assert.Equal("", ConnectViewModel.DeviceIdOf("not hex"));
    }

    [Fact]
    public void The_lan_section_shows_while_looking_and_says_why_nothing_is_found()
    {
        var client = new FakeClient();
        var vm = new ConnectViewModel(client, new MemoryRecent(), new RecordingWindows(), () => Samples.T0);
        Assert.False(vm.ShowLan, "hidden until the core is looking");
        client.Raise(new LanDevicesEvent(0, []));
        Assert.True(vm.ShowLan);
        Assert.True(vm.LanEmpty);
        client.Raise(new LanDevicesEvent(0, [new LanDevice("a", "192.168.1.6:47800", "", "")]));
        Assert.True(vm.ShowLan);
        Assert.False(vm.LanEmpty);
    }

    [Fact]
    public void A_saved_host_is_known_by_its_device_id_at_a_new_address_and_connects_in_one_click()
    {
        var client = new FakeClient();
        // Paired at an address it no longer has.
        var recent = new MemoryRecent(
            new RecentDevice("192.168.1.5:47800", "studio", Samples.HostKey, Samples.T0.ToUnixTimeSeconds() - 60));
        var vm = new ConnectViewModel(client, recent, new RecordingWindows(), () => Samples.T0);
        client.Raise(new ReadyEvent(0, Samples.ViewerKey, [new KnownHost(Samples.HostKey, "studio", 0)], ""));
        client.Raise(new LanDevicesEvent(0, [
            new LanDevice("aaa unknown", "192.168.1.9:47800", "", "fedcba9876543210fedcba9876543210"),
            new LanDevice("studio", "192.168.1.77:47800", "macos", HostDeviceId),
        ]));
        // Saved ones first, with what they have.
        Assert.Equal("studio", vm.Lan[0].Name);
        Assert.True(vm.Lan[0].IsPaired && vm.Lan[0].OneClick);
        Assert.Equal(L.Get("Connect.Paired"), vm.Lan[0].Badge);
        Assert.False(vm.Lan[1].HasBadge);
        Assert.True(vm.Lan[1].NeedsCode);

        vm.Lan[0].ConnectCommand.Execute(null);
        var req = Assert.Single(client.Requests);
        Assert.Equal("192.168.1.77:47800", req.Address);
        Assert.Equal(new ConnectAuth.KnownHost(Samples.HostKey), req.Auth);
    }

    [Fact]
    public async Task A_saved_password_signs_in_at_the_lan_address()
    {
        var client = new FakeClient();
        var passwords = new MemorySavedPasswords();
        passwords.Kept[Samples.HostKey] = "correct horse battery";
        var recent = new MemoryRecent(new RecentDevice("123456789", "studio", Samples.HostKey,
            Samples.T0.ToUnixTimeSeconds() - 60, "", HasPassword: true));
        var vm = new ConnectViewModel(client, recent, new RecordingWindows(), () => Samples.T0, passwords: passwords);
        client.Raise(new LanDevicesEvent(0, [new LanDevice("studio", "192.168.1.77:47800", "macos", HostDeviceId)]));
        Assert.Equal(L.Get("Connect.PasswordSaved"), vm.Lan[0].Badge);
        await vm.Lan[0].ConnectCommand.ExecuteAsync(null);
        var req = Assert.Single(client.Requests);
        Assert.Equal("192.168.1.77:47800", req.Address);
        Assert.Equal(new ConnectAuth.Password("correct horse battery"), req.Auth);
    }

    [Fact]
    public async Task A_host_saved_by_device_id_stays_filed_under_it_after_a_lan_connection()
    {
        var client = new FakeClient();
        var passwords = new MemorySavedPasswords();
        passwords.Kept[Samples.HostKey] = "correct horse battery";
        var recent = new MemoryRecent(new RecentDevice("370794910", "GitrhubXnoMac-mini", Samples.HostKey,
            Samples.T0.ToUnixTimeSeconds() - 60, "", HasPassword: true));
        var vm = new ConnectViewModel(client, recent, new RecordingWindows(), () => Samples.T0, passwords: passwords);
        client.Raise(new LanDevicesEvent(0, [new LanDevice("GitrhubXnoMac-mini", "192.168.0.70:47800", "macos", HostDeviceId)]));
        await vm.Lan[0].ConnectCommand.ExecuteAsync(null);
        var req = Assert.Single(client.Requests);
        Assert.Equal("192.168.0.70:47800", req.Address);
        client.Raise(new ConnectedEvent(req.Tag, Samples.HostKey, "GitrhubXnoMac-mini", new Permissions(true, true, true), 2, false));
        // Still one entry, still the device ID (it works from anywhere), password kept.
        var entry = Assert.Single(vm.Recent);
        Assert.Equal("370794910", entry.Device.Address);
        Assert.True(entry.HasPassword);
    }

    [Fact]
    public async Task A_listed_address_saved_with_a_password_connects_in_one_click()
    {
        var client = new FakeClient();
        var passwords = new MemorySavedPasswords();
        passwords.Kept[Samples.HostKey] = "correct horse battery";
        var recent = new MemoryRecent(new RecentDevice("100.104.200.119:47800", "mini", Samples.HostKey,
            Samples.T0.ToUnixTimeSeconds() - 60, "", HasPassword: true));
        var vm = new ConnectViewModel(client, recent, new RecordingWindows(), () => Samples.T0, passwords: passwords);
        client.Raise(new TailnetDevicesEvent(0, [new TailnetDevice("mini", "100.104.200.119:47800", true, "macOS", "mini.ts.net")]));
        vm.Tailnet[0].ConnectCommand.Execute(null);
        await Task.Yield();
        var req = Assert.Single(client.Requests);
        Assert.Equal(new ConnectAuth.Password("correct horse battery"), req.Auth);
    }

    [Fact]
    public async Task An_unknown_host_takes_its_code_in_its_own_row()
    {
        var client = new FakeClient();
        var vm = new ConnectViewModel(client, new MemoryRecent(), new RecordingWindows(), () => Samples.T0);
        client.Raise(new LanDevicesEvent(0, [new LanDevice("", "192.168.1.6:47800", "", "fedcba9876543210fedcba9876543210")]));
        var row = vm.Lan[0];
        Assert.Equal("192.168.1.6:47800", row.Name);
        await row.ConnectCommand.ExecuteAsync(null);
        Assert.True(row.IsEnteringCode);
        Assert.Empty(client.Requests);
        Assert.Equal("", vm.Address);

        row.Code = "nope";
        Assert.False(row.ConnectWithCodeCommand.CanExecute(null));
        row.Code = "K7QM-2XPA";
        row.ConnectWithCodeCommand.Execute(null);
        var req = Assert.Single(client.Requests);
        Assert.Equal("192.168.1.6:47800", req.Address);
        Assert.IsType<ConnectAuth.Code>(req.Auth);
        Assert.False(row.IsEnteringCode);
        Assert.Equal("", row.Code);

        // Cancelling closes the box without connecting.
        await row.ConnectCommand.ExecuteAsync(null);
        row.Code = "K7QM";
        row.CancelCodeCommand.Execute(null);
        Assert.False(row.IsEnteringCode);
        Assert.Equal("", row.Code);
        Assert.Single(client.Requests);
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
