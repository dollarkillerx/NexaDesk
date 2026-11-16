using System.Text;
using static NexaDesk.Native.NativeMethods;

namespace NexaDesk.Services;

/// <summary>What a signaling server presents: trusted already, or the fingerprint of
/// the certificate that would make it trusted.</summary>
public sealed record ServerCertificateInfo(bool Trusted, string Fingerprint);

/// <summary>
/// Trusting a self-hosted signaling server on first use. Its certificate is checked and
/// saved by libnexa_client (signal-ca.pem in the data directory, which the agent reads too).
/// </summary>
public interface IServerTrust
{
    /// <summary>The certificate, or why it could not be looked at.</summary>
    Task<(ServerCertificateInfo? Info, string? Error)> CheckAsync(string server);
    /// <summary>Saves it if it still has <paramref name="fingerprint"/>; null on success.</summary>
    Task<string?> TrustAsync(string server, string fingerprint);
}

public sealed unsafe class NativeServerTrust : IServerTrust
{
    public Task<(ServerCertificateInfo? Info, string? Error)> CheckAsync(string server) => Task.Run(() =>
    {
        try
        {
            var url = Encoding.UTF8.GetBytes(server);
            var buf = new byte[512];
            nuint written = 0;
            int rc;
            fixed (byte* u = url)
            fixed (byte* o = buf)
            {
                rc = nexa_signal_certificate(u, (nuint)url.Length, o, (nuint)buf.Length, &written);
                if (rc != NEXA_OK) return ((ServerCertificateInfo?)null, (string?)Native.NativeError.Last());
            }
            var lines = Encoding.UTF8.GetString(buf, 0, (int)written).Split('\n');
            return lines.Length == 2
                ? (new ServerCertificateInfo(lines[0] == "trusted", lines[1]), null)
                : (null, "unexpected answer");
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
            return (null, e.Message);
        }
    });

    public Task<string?> TrustAsync(string server, string fingerprint) => Task.Run(() =>
    {
        try
        {
            var url = Encoding.UTF8.GetBytes(server);
            var fp = Encoding.UTF8.GetBytes(fingerprint);
            fixed (byte* u = url)
            fixed (byte* f = fp)
            {
                return nexa_signal_trust(u, (nuint)url.Length, f, (nuint)fp.Length) == NEXA_OK
                    ? null
                    : Native.NativeError.Last();
            }
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
            return e.Message;
        }
    });
}
