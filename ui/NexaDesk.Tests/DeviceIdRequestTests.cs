using Avalonia.Headless.XUnit;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// A device ID alone asks the person at the host instead of proving a code; an address
/// alone does not, since nothing would say who answers. And the device ID can be reset.
/// </summary>
public class DeviceIdRequestTests
{
    static ConnectViewModel Connect(FakeClient client) =>
        new(client, new MemoryRecent(), new RecordingWindows(), () => Samples.T0, () => "wss://signal.example:8443");

    [Fact]
    public void A_device_id_with_nothing_typed_sends_a_request()
    {
        var client = new FakeClient();
        var vm = Connect(client);
        vm.Address = "546 108 077";
        vm.Code = "";
        Assert.True(vm.ConnectCommand.CanExecute(null));
        vm.ConnectCommand.Execute(null);
        Assert.IsType<ConnectAuth.Request>(Assert.Single(client.Requests).Auth);
        Assert.Equal("546108077", client.Requests[0].Address);
    }

    [Fact]
    public void An_address_with_nothing_typed_still_needs_a_code()
    {
        var vm = Connect(new FakeClient());
        vm.Address = "192.168.1.35";
        vm.Code = "";
        Assert.False(vm.ConnectCommand.CanExecute(null));
    }

    [Fact]
    public void A_typed_code_still_wins_over_a_request()
    {
        var client = new FakeClient();
        var vm = Connect(client);
        vm.Address = "546108077";
        vm.Code = "abcd-2345";
        vm.ConnectCommand.Execute(null);
        Assert.Equal(new ConnectAuth.Code("ABCD2345"), client.Requests[0].Auth);
    }

    [Fact]
    public void A_request_goes_out_as_field_9()
    {
        var bytes = new ConnectRequest("546108077", 1, StreamMode.Balanced, 0, new ConnectAuth.Request()).Encode();
        // Field 9, varint: 0x48, then 1.
        Assert.Contains("4801", Convert.ToHexString(bytes));
    }

    [Fact]
    public void The_consent_dialog_says_a_request_is_unverified_and_drops_a_zero_port()
    {
        Assert.Equal("126.109.234.172", Texts.Remote("126.109.234.172:0"));
        Assert.Equal("192.168.1.35:5000", Texts.Remote("192.168.1.35:5000"));
        Assert.Equal("2001:db8::1", Texts.Remote("[2001:db8::1]:0"));
        Assert.NotEqual(Texts.Auth(AuthKind.Code), Texts.Auth(AuthKind.Request));
    }

    [AvaloniaFact]
    public async Task Resetting_the_id_asks_first_then_restarts_hosting()
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var main = new MainViewModel(new FakeClient(), new MemoryRecent(), link, process, new FakePermissions(), new RecordingWindows(),
            new FakeAddresses(["192.168.1.35"]), "Mac", () => Samples.T0, new MemorySettings());
        await main.Host.StartCommand.ExecuteAsync(null);
        link.Raise(Samples.Status());
        var host = main.Host;

        host.AskResetDeviceIdCommand.Execute(null);
        Assert.True(host.IsConfirmingIdReset);
        host.CancelResetDeviceIdCommand.Execute(null);
        Assert.False(host.IsConfirmingIdReset);
        Assert.Equal(0, process.IdResets);

        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        host.AskResetDeviceIdCommand.Execute(null);
        await host.ResetDeviceIdCommand.ExecuteAsync(null);
        Assert.Equal(1, process.IdResets);
        Assert.False(host.IsConfirmingIdReset);
        Assert.Equal(2, process.Starts.Count);

        process.IdResetResult = "disk full";
        await host.ResetDeviceIdCommand.ExecuteAsync(null);
        Assert.Contains("disk full", host.Notice);
        Assert.Equal(2, process.Starts.Count);
    }
}
