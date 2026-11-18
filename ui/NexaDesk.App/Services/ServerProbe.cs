using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Net.WebSockets;

namespace NexaDesk.Services;

/// <summary>What looking at a signaling server found.</summary>
/// <param name="Available">A NexaDesk signaling server answered.</param>
/// <param name="Milliseconds">A round trip to it (a TCP connect), when it could be reached at all.</param>
/// <param name="Problem">"unreachable" (nothing answered) or "notsignal" (something answered, but it is not a signaling server).</param>
public sealed record ProbeResult(bool Available, int? Milliseconds, string? Problem);

/// <summary>Tests whether a signaling server can be reached, and how far away it is.</summary>
public interface IServerProbe
{
    /// <summary>Never throws; a cancelled look returns "unreachable".</summary>
    Task<ProbeResult> ProbeAsync(string endpoint, CancellationToken ct = default);
}

/// <summary>
/// The delay is the median of three TCP connects, which is one round trip to the server. Whether it
/// is a signaling server is told by its first message: it sends a challenge as soon as a WebSocket
/// opens. The probe sends no key and no device identity, so it accepts any certificate: a server
/// with a private one (trusted on first use later) is still measured, and its certificate is left
/// for the person to check when they use it.
/// </summary>
public sealed class ServerProbe : IServerProbe
{
    static readonly TimeSpan Limit = TimeSpan.FromSeconds(8);

    /// <summary>The shortest challenge: a protocol number and 32 bytes of nonce.</summary>
    const int MinChallenge = 32;

    public async Task<ProbeResult> ProbeAsync(string endpoint, CancellationToken ct = default)
    {
        if (!Uri.TryCreate(endpoint, UriKind.Absolute, out var uri) || uri.Scheme is not ("wss" or "ws"))
            return new ProbeResult(false, null, "unreachable");
        using var limit = CancellationTokenSource.CreateLinkedTokenSource(ct);
        limit.CancelAfter(Limit);
        int? ms = null;
        try
        {
            ms = await RoundTripAsync(uri, limit.Token);
            return await ChallengeAsync(uri, limit.Token)
                ? new ProbeResult(true, ms, null)
                : new ProbeResult(false, ms, "notsignal");
        }
        catch (Exception e) when (e is SocketException or OperationCanceledException or WebSocketException
                                      or HttpRequestException or IOException or InvalidOperationException
                                      or System.Security.Authentication.AuthenticationException)
        {
            return new ProbeResult(false, ms, ms == null ? "unreachable" : "notsignal");
        }
    }

    static async Task<int> RoundTripAsync(Uri uri, CancellationToken ct)
    {
        var port = uri.Port > 0 ? uri.Port : uri.Scheme == "wss" ? 443 : 80;
        var addresses = await Dns.GetHostAddressesAsync(uri.Host, ct);
        if (addresses.Length == 0) throw new SocketException((int)SocketError.HostNotFound);
        var times = new List<double>();
        for (var i = 0; i < 3; i++)
        {
            using var socket = new Socket(addresses[0].AddressFamily, SocketType.Stream, ProtocolType.Tcp) { NoDelay = true };
            var clock = Stopwatch.StartNew();
            await socket.ConnectAsync(new IPEndPoint(addresses[0], port), ct);
            times.Add(clock.Elapsed.TotalMilliseconds);
        }
        times.Sort();
        return Math.Max(1, (int)Math.Round(times[1]));
    }

    static async Task<bool> ChallengeAsync(Uri uri, CancellationToken ct)
    {
        using var socket = new ClientWebSocket();
        socket.Options.RemoteCertificateValidationCallback = (_, _, _, _) => true;
        await socket.ConnectAsync(uri, ct);
        var buffer = new byte[1024];
        var first = await socket.ReceiveAsync(buffer, ct);
        socket.Abort();
        return first.MessageType == WebSocketMessageType.Binary && first.Count >= MinChallenge;
    }
}
