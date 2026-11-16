using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>Trusting a self-hosted signaling server on first use: asked once, saved only on yes.</summary>
public class ServerTrustTests
{
    const string Fp = "AA:BB:CC";

    sealed class FakeTrust(bool trusted, string? checkError = null) : IServerTrust
    {
        public bool Trusted = trusted;
        public List<(string Server, string Fingerprint)> Saved { get; } = new();

        public Task<(ServerCertificateInfo? Info, string? Error)> CheckAsync(string server) =>
            Task.FromResult<(ServerCertificateInfo?, string?)>(checkError != null ? (null, checkError) : (new ServerCertificateInfo(Trusted, Fp), null));

        public Task<string?> TrustAsync(string server, string fingerprint)
        {
            Saved.Add((server, fingerprint));
            Trusted = true;
            return Task.FromResult<string?>(null);
        }
    }

    [Fact]
    public async Task A_trusted_server_asks_nothing()
    {
        var windows = new RecordingWindows();
        var flow = new ServerTrustFlow(new FakeTrust(trusted: true), windows);
        Assert.Equal(TrustOutcome.AlreadyTrusted, await flow.EnsureAsync("wss://signal.example.com"));
        Assert.Empty(windows.Trusts);
    }

    [Fact]
    public async Task An_untrusted_server_is_saved_only_when_the_person_trusts_it()
    {
        var windows = new RecordingWindows();
        var trust = new FakeTrust(trusted: false);
        var flow = new ServerTrustFlow(trust, windows);

        var declined = flow.EnsureAsync("wss://signal.example.com");
        var ask = Assert.Single(windows.Trusts);
        Assert.Equal(Fp, ask.Fingerprint);
        Assert.Contains("wss://signal.example.com", ask.Intro);
        ask.OnWindowClosed();
        Assert.Equal(TrustOutcome.Declined, await declined);
        Assert.Empty(trust.Saved);

        var accepted = flow.EnsureAsync("wss://signal.example.com");
        windows.Trusts[1].TrustCommand.Execute(null);
        Assert.Equal(TrustOutcome.NowTrusted, await accepted);
        Assert.Equal(("wss://signal.example.com", Fp), Assert.Single(trust.Saved));
    }

    [Fact]
    public async Task An_unreachable_server_says_why()
    {
        var flow = new ServerTrustFlow(new FakeTrust(false, checkError: "cannot reach the server"), new RecordingWindows());
        Assert.Equal(TrustOutcome.Failed, await flow.EnsureAsync("wss://signal.example.com"));
        Assert.Equal("cannot reach the server", flow.LastError);
    }

    [Theory]
    [InlineData("TLS: invalid peer certificate: UnknownIssuer", true)]
    [InlineData("the server refused: Forbidden", false)]
    [InlineData(null, false)]
    public void Only_certificate_failures_count(string? message, bool expected) =>
        Assert.Equal(expected, ServerTrustFlow.IsCertificateProblem(message));

    [Fact]
    public void A_connection_that_fails_on_the_certificate_offers_to_trust_and_then_connects_again()
    {
        var client = new FakeClient();
        var windows = new RecordingWindows();
        var trust = new FakeTrust(trusted: false);
        var main = new MainViewModel(client, new MemoryRecent(), new FakeLink(), new FakeProcess(), new FakePermissions(), windows,
            new FakeAddresses(), "王小明的 MacBook Pro", () => Samples.T0, new MemorySettings(new AppSettings("wss://signal.example.com")),
            serverTrust: trust);
        var vm = main.Connect;
        vm.Address = "314 159 265";
        vm.Code = "ABCD2345";
        vm.ConnectCommand.Execute(null);
        client.Raise(new DisconnectedEvent(client.Requests[0].Tag, DisconnectReason.SignalServer,
            "the server: TLS: invalid peer certificate: UnknownIssuer"));
        Assert.NotNull(vm.Error);
        Assert.Contains("UnknownIssuer", vm.Error!.Detail);

        Assert.Single(windows.Trusts).TrustCommand.Execute(null);
        Assert.Single(trust.Saved);
        Assert.Null(vm.Error);
        Assert.Equal(2, client.Requests.Count);
    }
}
