using NexaDesk.Localization;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>The signaling server's access key: required with a server, kept in the Keychain under
/// that server, given to the agent in its environment and to the viewer's request.</summary>
public class SignalSecretTests
{
    const string Server = "wss://signal.example.com";

    static (SettingsViewModel Settings, MemorySettings Store, MemorySavedPasswords Keychain) Settings(
        string server = "", string? kept = null)
    {
        var store = new MemorySettings(new AppSettings(server));
        var keychain = new MemorySavedPasswords();
        if (kept != null) keychain.Kept["signal-access:" + server] = kept;
        return (new SettingsViewModel(store, keychain), store, keychain);
    }

    [Fact]
    public void The_key_is_kept_in_the_Keychain_under_its_server_and_not_in_settings_json()
    {
        var (s, store, keychain) = Settings();
        s.ServerInput = "signal.example.com";
        s.SecretInput = "  open sesame \n";
        s.SaveCommand.Execute(null);
        Assert.Equal("open sesame", s.SavedSecret);
        Assert.Equal("open sesame", keychain.Kept["signal-access:" + Server]);
        Assert.DoesNotContain("open sesame", System.Text.Json.JsonSerializer.Serialize(store.Current, SettingsJson.Default.AppSettings));
        Assert.False(s.IsDirty);
        // Giving the server up takes its key with it.
        s.ServerInput = "";
        s.SaveCommand.Execute(null);
        Assert.Equal("", s.SavedSecret);
        Assert.Empty(keychain.Kept);
    }

    [Fact]
    public async Task The_saved_key_is_read_back_from_the_Keychain_at_start()
    {
        var (s, _, _) = Settings(Server, kept: "from the keychain");
        Assert.Equal("", s.SavedSecret);
        await s.LoadSecretAsync();
        Assert.Equal("from the keychain", s.SavedSecret);
        Assert.Equal("from the keychain", s.SecretInput);
        Assert.False(s.IsDirty);
    }

    [Fact]
    public void A_server_saved_before_keys_asks_for_one_and_changing_only_the_key_is_a_change()
    {
        var (s, _, _) = Settings(Server);
        Assert.NotNull(s.SecretHint);
        Assert.False(s.SaveCommand.CanExecute(null));
        s.SecretInput = "now it has one";
        Assert.True(s.IsDirty);
        Assert.True(s.SaveCommand.CanExecute(null));
        var changed = 0;
        s.ServerChanged += _ => changed++;
        s.SaveCommand.Execute(null);
        // The host reads it at its next start: the change is announced like a server's.
        Assert.Equal(1, changed);
        Assert.Equal("now it has one", s.SavedSecret);
    }

    [Fact]
    public void A_key_the_Keychain_refuses_is_not_saved_and_says_why()
    {
        var (s, store, keychain) = Settings();
        keychain.Refusal = "locked";
        s.ServerInput = "signal.example.com";
        s.SecretInput = "open sesame";
        s.SaveCommand.Execute(null);
        Assert.Equal("", store.Current.SignalServer);
        Assert.Equal("", s.SavedServer);
        Assert.Contains("locked", s.SecretError);
    }

    [Fact]
    public void Hosting_starts_with_the_key_and_needs_a_restart_when_it_changes()
    {
        var process = new FakeProcess();
        var store = new MemorySettings(new AppSettings(Server));
        var keychain = new MemorySavedPasswords();
        keychain.Kept["signal-access:" + Server] = "open sesame";
        var vm = new MainViewModel(new FakeClient(), new MemoryRecent(), new FakeLink(), process, new FakePermissions(),
            new RecordingWindows(), new FakeAddresses(["192.168.1.35"]), "Mac", () => Samples.T0, store, passwords: keychain);
        vm.Settings.LoadSecretAsync().GetAwaiter().GetResult();
        vm.Host.StartCommand.Execute(null);
        Assert.Equal(("wss://signal.example.com", "open sesame"), (process.Servers.Last(), process.Secrets.Last()));

        vm.Host.State = HostState.Hosting;
        Assert.False(vm.Host.RestartNeeded);
        vm.Settings.SecretInput = "a new key";
        vm.Settings.SaveCommand.Execute(null);
        Assert.True(vm.Host.RestartNeeded);
    }

    [Fact]
    public void A_connection_by_device_ID_carries_the_key_and_one_by_address_does_not()
    {
        var client = new FakeClient();
        var store = new MemorySettings(new AppSettings(Server));
        var keychain = new MemorySavedPasswords();
        keychain.Kept["signal-access:" + Server] = "open sesame";
        var vm = new MainViewModel(client, new MemoryRecent(), new FakeLink(), new FakeProcess(), new FakePermissions(),
            new RecordingWindows(), new FakeAddresses(), "Mac", () => Samples.T0, store, passwords: keychain);
        vm.Settings.LoadSecretAsync().GetAwaiter().GetResult();
        vm.Connect.Address = "314 159 265";
        vm.Connect.Code = "ABCD2345";
        vm.Connect.ConnectCommand.Execute(null);
        var byId = Assert.Single(client.Requests);
        Assert.Equal((Server, "open sesame"), (byId.SignalServer, byId.SignalSecret));
        Assert.DoesNotContain("open sesame", byId.ToString());

        vm.Connect.CancelCommand.Execute(null);
        vm.Connect.Address = "10.0.0.2:47800";
        vm.Connect.Code = "ABCD2345";
        vm.Connect.ConnectCommand.Execute(null);
        var byAddress = client.Requests.Last();
        Assert.Equal(("", ""), (byAddress.SignalServer, byAddress.SignalSecret));
    }
}
