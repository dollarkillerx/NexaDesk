using System.Text;
using System.Text.Json;
using Avalonia.Threading;
using NexaDesk.Services;
using static NexaDesk.Native.NativeMethods;

namespace NexaDesk.Native;

/// <summary>
/// NexaDesk's MCP server for AI apps on this computer (docs/MCP.md §16), in the core through nexa_mcp_*.
/// While it serves, what AI agents operate is read every two seconds and <see cref="Changed"/> raised on
/// the UI thread when it differs.
/// </summary>
public sealed unsafe class NativeAgentServer(string dataDir) : IAgentServer
{
    static readonly TimeSpan Every = TimeSpan.FromSeconds(2);
    Timer? _poll;
    IReadOnlyList<AgentSessionInfo> _sessions = [];

    public IReadOnlyList<AgentSessionInfo> Sessions => _sessions;
    public event Action? Changed;

    public string? Start(string deviceName)
    {
        try
        {
            var d = Encoding.UTF8.GetBytes(dataDir);
            var n = Encoding.UTF8.GetBytes(deviceName);
            int rc;
            fixed (byte* dp = d)
            fixed (byte* np = n)
            {
                rc = nexa_mcp_start(dp, (nuint)d.Length, np, (nuint)n.Length);
            }
            if (rc != NEXA_OK) return NativeError.Last();
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
            return e.Message;
        }
        _poll ??= new Timer(_ => Poll(), null, TimeSpan.Zero, Every);
        return null;
    }

    public void Stop()
    {
        _poll?.Dispose();
        _poll = null;
        try
        {
            nexa_mcp_stop();
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
        }
        Publish([]);
    }

    public void StopSessions()
    {
        try
        {
            nexa_mcp_stop_sessions();
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
        }
        Poll();
    }

    void Poll()
    {
        try
        {
            var buf = new byte[16 << 10];
            nuint written = 0;
            int rc;
            fixed (byte* o = buf)
            {
                rc = nexa_mcp_status(o, (nuint)buf.Length, &written);
            }
            if (rc != NEXA_OK || written == 0 || written > (nuint)buf.Length) return;
            using var doc = JsonDocument.Parse(buf.AsMemory(0, (int)written));
            var now = new List<AgentSessionInfo>();
            if (doc.RootElement.TryGetProperty("sessions", out var list))
                foreach (var s in list.EnumerateArray())
                    now.Add(new AgentSessionInfo(
                        s.TryGetProperty("client", out var c) ? c.GetString() ?? "" : "",
                        s.TryGetProperty("device", out var d) ? d.GetString() ?? "" : ""));
            Publish(now);
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException or JsonException)
        {
        }
    }

    void Publish(IReadOnlyList<AgentSessionInfo> now)
    {
        if (now.SequenceEqual(_sessions)) return;
        Dispatcher.UIThread.Post(() =>
        {
            _sessions = now;
            Changed?.Invoke();
        });
    }
}
