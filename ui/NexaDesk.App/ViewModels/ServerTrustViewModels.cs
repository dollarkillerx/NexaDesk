using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>"Trust this signaling server?": its address and certificate fingerprint, to
/// compare with what the server's owner sees. Not trusting is the default.</summary>
public sealed partial class ServerTrustViewModel(string server, string fingerprint) : ObservableObject
{
    public string Server { get; } = server;
    public string Fingerprint { get; } = fingerprint;
    public string Intro => L.Format("Trust.Intro", Server);

    /// <summary>True for trust; raised once, whichever way the dialog ends.</summary>
    public event Action<bool>? Finished;
    bool _done;

    void Finish(bool trust)
    {
        if (_done) return;
        _done = true;
        Finished?.Invoke(trust);
    }

    [RelayCommand]
    private void Trust() => Finish(true);

    [RelayCommand]
    private void DontTrust() => Finish(false);

    /// <summary>Closing the window any other way does not trust.</summary>
    public void OnWindowClosed() => Finish(false);
}

public enum TrustOutcome { AlreadyTrusted, NowTrusted, Declined, Failed }

/// <summary>
/// Looks at a signaling server's certificate and, when nothing on this computer trusts
/// it, asks the person once. Used when a server is saved and when a connection fails on
/// the certificate.
/// </summary>
public sealed class ServerTrustFlow(IServerTrust trust, IWindowService windows)
{
    readonly HashSet<string> _asking = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>Why the last check or save failed, for showing.</summary>
    public string? LastError { get; private set; }

    public async Task<TrustOutcome> EnsureAsync(string server)
    {
        LastError = null;
        if (string.IsNullOrWhiteSpace(server) || !_asking.Add(server)) return TrustOutcome.Failed;
        try
        {
            var (info, error) = await trust.CheckAsync(server);
            if (info is null)
            {
                LastError = error;
                return TrustOutcome.Failed;
            }
            if (info.Trusted) return TrustOutcome.AlreadyTrusted;
            var vm = new ServerTrustViewModel(server, info.Fingerprint);
            var answer = new TaskCompletionSource<bool>();
            vm.Finished += yes => answer.TrySetResult(yes);
            windows.ShowServerTrust(vm);
            if (!await answer.Task) return TrustOutcome.Declined;
            LastError = await trust.TrustAsync(server, info.Fingerprint);
            return LastError is null ? TrustOutcome.NowTrusted : TrustOutcome.Failed;
        }
        finally
        {
            _asking.Remove(server);
        }
    }

    /// <summary>Whether a failure text is about the server's certificate.</summary>
    public static bool IsCertificateProblem(string? message) =>
        message is not null
        && (message.Contains("certificate", StringComparison.OrdinalIgnoreCase)
            || message.Contains("UnknownIssuer", StringComparison.OrdinalIgnoreCase)
            || message.Contains("TLS", StringComparison.Ordinal));
}

/// <summary>Where no trust can be checked (tests that do not care).</summary>
sealed class NoServerTrust : IServerTrust
{
    public Task<(ServerCertificateInfo? Info, string? Error)> CheckAsync(string server) =>
        Task.FromResult<(ServerCertificateInfo?, string?)>((null, "not available"));

    public Task<string?> TrustAsync(string server, string fingerprint) => Task.FromResult<string?>("not available");
}
