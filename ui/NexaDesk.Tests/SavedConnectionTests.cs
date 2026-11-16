using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// Recent connections as the web page's saved ones: a name of one's own, a fixed
/// password kept in the Keychain for one-click sign-in, and a request for an ID.
/// </summary>
public class SavedConnectionTests
{
    const string Server = "wss://signal.example:8443";
    const string Password = "correct horse battery";

    static ConnectViewModel Make(FakeClient client, MemoryRecent recent, MemorySavedPasswords passwords, string server = Server) =>
        new(client, recent, new RecordingWindows(), () => Samples.T0, () => server, passwords: passwords);

    /// <summary>A host that was never paired with: no PairedNow, so one click cannot use its key.</summary>
    static ConnectedEvent Unpaired(ulong tag) =>
        new(tag, Samples.HostKey, "工作室 Mac mini", new Permissions(true, true, true), 2, false);

    [Fact]
    public void A_password_that_gets_in_is_kept_when_asked()
    {
        var (client, recent, passwords) = (new FakeClient(), new MemoryRecent(), new MemorySavedPasswords());
        var vm = Make(client, recent, passwords);
        vm.Address = "192.168.1.20";
        vm.Code = Password;
        Assert.True(vm.CodeIsPassword);
        vm.RememberPassword = true;
        vm.ConnectCommand.Execute(null);
        client.Raise(Unpaired(client.Requests[0].Tag));
        Assert.Equal(Password, passwords.Kept[Samples.HostKey]);
        Assert.True(Assert.Single(recent.Saved).HasPassword);
        Assert.True(vm.Recent[0].HasPassword);
    }

    [Fact]
    public void Nothing_is_kept_unless_asked_or_for_a_code()
    {
        var (client, recent, passwords) = (new FakeClient(), new MemoryRecent(), new MemorySavedPasswords());
        var vm = Make(client, recent, passwords);
        vm.Address = "192.168.1.20";
        vm.Code = Password;
        vm.ConnectCommand.Execute(null);
        client.Raise(Unpaired(client.Requests[0].Tag));
        vm.Code = "ABCD-2345";
        Assert.False(vm.CodeIsPassword);
        Assert.Empty(passwords.Kept);
        Assert.False(recent.Saved[0].HasPassword);
    }

    [Fact]
    public void One_click_signs_in_with_the_kept_password()
    {
        var device = new RecentDevice("192.168.1.20", "工作室 Mac mini", Samples.HostKey, 1, HasPassword: true);
        var passwords = new MemorySavedPasswords();
        passwords.Kept[Samples.HostKey] = Password;
        var client = new FakeClient();
        var vm = Make(client, new MemoryRecent(device), passwords);
        vm.Recent[0].ConnectCommand.Execute(null);
        Assert.Equal(new ConnectAuth.Password(Password), Assert.Single(client.Requests).Auth);
    }

    [Fact]
    public void A_kept_password_comes_before_a_pairing_the_host_may_not_have()
    {
        // Signed in with the host's fixed password and kept it; this side also took it
        // as paired (an older version did). The host keeps no pairing for its password,
        // so one click must sign in with the password, not as a known device.
        var (client, recent, passwords) = (new FakeClient(), new MemoryRecent(), new MemorySavedPasswords());
        var vm = Make(client, recent, passwords);
        vm.Address = "192.168.1.20";
        vm.Code = Password;
        vm.RememberPassword = true;
        vm.ConnectCommand.Execute(null);
        client.Raise(new ConnectedEvent(client.Requests[0].Tag, Samples.HostKey, "工作室 Mac mini",
            new Permissions(true, true, true), 2, true));
        Assert.True(vm.Recent[0].IsPaired);
        Assert.True(vm.Recent[0].HasPassword);
        client.Sessions[0].Close();
        client.Raise(new DisconnectedEvent(client.Requests[0].Tag, DisconnectReason.ClosedLocally, ""));

        vm.Recent[0].ConnectCommand.Execute(null);
        Assert.Equal(new ConnectAuth.Password(Password), client.Requests[^1].Auth);
    }

    [Fact]
    public void A_kept_password_the_host_refuses_is_dropped_and_asked_for_again()
    {
        var device = new RecentDevice("192.168.1.20", "工作室 Mac mini", Samples.HostKey, 1, HasPassword: true);
        var passwords = new MemorySavedPasswords();
        passwords.Kept[Samples.HostKey] = "an old password";
        var (client, recent) = (new FakeClient(), new MemoryRecent(device));
        var vm = Make(client, recent, passwords);
        vm.Recent[0].ConnectCommand.Execute(null);
        client.Raise(new DisconnectedEvent(client.Requests[0].Tag, DisconnectReason.AuthFailed, "wrong password"));
        Assert.Empty(passwords.Kept);
        Assert.False(recent.Saved[0].HasPassword);
        Assert.Equal("192.168.1.20", vm.Address);
        Assert.NotNull(vm.Error);
    }

    [Fact]
    public void An_unpaired_id_with_nothing_kept_asks_the_host()
    {
        var device = new RecentDevice("546108077", "工作室 Mac mini", Samples.HostKey, 1);
        var client = new FakeClient();
        var vm = Make(client, new MemoryRecent(device), new MemorySavedPasswords());
        vm.Recent[0].ConnectCommand.Execute(null);
        Assert.IsType<ConnectAuth.Request>(Assert.Single(client.Requests).Auth);
    }

    [Fact]
    public void An_unpaired_address_with_nothing_kept_still_needs_a_code()
    {
        var device = new RecentDevice("192.168.1.20", "工作室 Mac mini", Samples.HostKey, 1);
        var client = new FakeClient();
        var vm = Make(client, new MemoryRecent(device), new MemorySavedPasswords());
        vm.Recent[0].ConnectCommand.Execute(null);
        Assert.Empty(client.Requests);
        Assert.Equal("192.168.1.20", vm.Address);
    }

    [Fact]
    public void A_name_of_one_s_own_and_a_new_password_are_kept_across_connections()
    {
        var device = new RecentDevice("192.168.1.20", "工作室 Mac mini", Samples.HostKey, 1);
        var (client, recent, passwords) = (new FakeClient(), new MemoryRecent(device), new MemorySavedPasswords());
        var vm = Make(client, recent, passwords);
        var row = vm.Recent[0];
        row.EditCommand.Execute(null);
        row.EditLabel = "书房的 Mac";
        row.EditPassword = Password;
        row.SaveEditCommand.Execute(null);
        Assert.Null(row.EditError);
        Assert.Equal("书房的 Mac", vm.Recent[0].Name);
        Assert.Equal(Password, passwords.Kept[Samples.HostKey]);

        // Connected again: the host's name does not replace the person's, the password stays.
        vm.Recent[0].ConnectCommand.Execute(null);
        client.Raise(Unpaired(client.Requests[0].Tag));
        Assert.Equal("书房的 Mac", vm.Recent[0].Name);
        Assert.True(recent.Saved[0].HasPassword);
    }

    [Fact]
    public void The_editor_refuses_a_code_like_password_and_can_forget_or_remove()
    {
        var device = new RecentDevice("192.168.1.20", "工作室 Mac mini", Samples.HostKey, 1, HasPassword: true);
        var passwords = new MemorySavedPasswords();
        passwords.Kept[Samples.HostKey] = Password;
        var recent = new MemoryRecent(device);
        var vm = Make(new FakeClient(), recent, passwords);
        var row = vm.Recent[0];
        row.EditCommand.Execute(null);
        row.EditPassword = "ABCD-2345";
        row.SaveEditCommand.Execute(null);
        Assert.NotNull(row.EditError);
        Assert.Equal(Password, passwords.Kept[Samples.HostKey]);

        row.EditPassword = "";
        row.ForgetPassword = true;
        row.SaveEditCommand.Execute(null);
        Assert.Empty(passwords.Kept);
        Assert.False(vm.Recent[0].HasPassword);

        passwords.Kept[Samples.HostKey] = Password;
        recent.Saved[0] = recent.Saved[0] with { HasPassword = true };
        var again = Make(new FakeClient(), recent, passwords);
        again.Recent[0].RemoveCommand.Execute(null);
        Assert.Empty(passwords.Kept);
        Assert.Empty(recent.Saved);
    }
}
