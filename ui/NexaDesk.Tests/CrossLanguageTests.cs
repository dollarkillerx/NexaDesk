using System.Diagnostics;
using NexaDesk.Proto;
using NexaDesk.Services;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// The C# AgentLink against the real Rust IPC server and consent hub, running with fake
/// devices (crates/nexa-agent/examples/ipc_fake_host.rs). Build it first with
/// `cargo build -p nexa-agent --example ipc_fake_host`; without it the test is skipped.
/// </summary>
public class CrossLanguageTests
{
    static string? FakeHost()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !File.Exists(Path.Combine(dir.FullName, "Cargo.toml"))) dir = dir.Parent;
        if (dir == null) return null;
        var path = Path.Combine(dir.FullName, "target", "debug", "examples", "ipc_fake_host");
        return File.Exists(path) ? path : null;
    }

    sealed class Lines
    {
        readonly List<string> _lines = new();
        public void Add(string? l) { if (l != null) lock (_lines) _lines.Add(l); }

        public async Task<string> WaitFor(string prefix)
        {
            for (var i = 0; i < 1000; i++)
            {
                lock (_lines)
                {
                    var hit = _lines.FirstOrDefault(l => l.StartsWith(prefix));
                    if (hit != null) return hit;
                }
                await Task.Delay(20);
            }
            lock (_lines) throw new Xunit.Sdk.XunitException($"no {prefix} line; got: {string.Join(" | ", _lines)}");
        }
    }

    static async Task<T> Next<T>(List<AgentMessage> got, InlineDispatcher _) where T : AgentMessage
    {
        for (var i = 0; i < 1000; i++)
        {
            lock (got)
            {
                var m = got.OfType<T>().FirstOrDefault();
                if (m != null) return m;
            }
            await Task.Delay(20);
        }
        throw new Xunit.Sdk.XunitException($"no {typeof(T).Name}");
    }

    [Fact]
    public async Task The_ui_allows_a_real_viewer_and_ends_its_session()
    {
        var exe = FakeHost();
        Assert.SkipWhen(exe == null, "build it with: cargo build -p nexa-agent --example ipc_fake_host");
        var dir = Path.Combine(Path.GetTempPath(), $"nd-x-{Guid.NewGuid():N}"[..14]);
        Directory.CreateDirectory(dir);
        var lines = new Lines();
        using var proc = new Process
        {
            StartInfo = new ProcessStartInfo(exe!, [dir]) { RedirectStandardOutput = true, RedirectStandardError = true, UseShellExecute = false },
        };
        proc.OutputDataReceived += (_, e) => lines.Add(e.Data);
        proc.ErrorDataReceived += (_, e) => lines.Add(e.Data);
        proc.Start();
        proc.BeginOutputReadLine();
        proc.BeginErrorReadLine();
        try
        {
            await lines.WaitFor("READY");
            var ui = new InlineDispatcher();
            var link = new AgentLink(Path.Combine(dir, Ipc.SocketName), ui);
            var got = new List<AgentMessage>();
            link.MessageReceived += m => { lock (got) got.Add(m); };
            Assert.Null(await link.ConnectAsync(CancellationToken.None));
            var status = await Next<AgentMessage.Status>(got, ui);
            Assert.Equal(8, status.Code.Length);
            Assert.Equal("fake host", status.DeviceName);

            var req = await Next<AgentMessage.IncomingRequest>(got, ui);
            Assert.Equal("cross-language viewer", req.DeviceName);
            Assert.Equal(64, req.DeviceKey.Length);
            Assert.Equal(AuthKind.Code, req.Auth);
            link.Send(new UiRequest.Decide(req.ReqId, true, new Permissions(true, true, false), false, false));
            Assert.Equal("VIEWER accepted view=true input=true clipboard=false", await lines.WaitFor("VIEWER"));

            var started = await Next<AgentMessage.SessionStarted>(got, ui);
            Assert.Equal(req.DeviceKey, started.Session.DeviceKey);
            link.Send(new UiRequest.Disconnect());
            Assert.Equal("VIEWER bye HostEnded", await lines.WaitFor("VIEWER bye"));
            await Next<AgentMessage.SessionEnded>(got, ui);
            link.Close();
            Assert.True(proc.WaitForExit(10_000));
        }
        finally
        {
            if (!proc.HasExited) proc.Kill();
            Directory.Delete(dir, true);
        }
    }

    [Fact]
    public async Task Dropping_the_ui_while_asked_denies_the_real_viewer()
    {
        var exe = FakeHost();
        Assert.SkipWhen(exe == null, "build it with: cargo build -p nexa-agent --example ipc_fake_host");
        var dir = Path.Combine(Path.GetTempPath(), $"nd-x-{Guid.NewGuid():N}"[..14]);
        Directory.CreateDirectory(dir);
        var lines = new Lines();
        using var proc = new Process
        {
            StartInfo = new ProcessStartInfo(exe!, [dir]) { RedirectStandardOutput = true, UseShellExecute = false },
        };
        proc.OutputDataReceived += (_, e) => lines.Add(e.Data);
        proc.Start();
        proc.BeginOutputReadLine();
        try
        {
            await lines.WaitFor("READY");
            var ui = new InlineDispatcher();
            var link = new AgentLink(Path.Combine(dir, Ipc.SocketName), ui);
            var got = new List<AgentMessage>();
            link.MessageReceived += m => { lock (got) got.Add(m); };
            Assert.Null(await link.ConnectAsync(CancellationToken.None));
            await Next<AgentMessage.IncomingRequest>(got, ui);
            var dropped = Stopwatch.StartNew();
            link.Close();
            Assert.Equal("VIEWER rejected ConsentDenied", await lines.WaitFor("VIEWER"));
            Assert.True(dropped.Elapsed < TimeSpan.FromSeconds(5), $"denied at once, not at the 20 s timeout ({dropped.Elapsed})");
        }
        finally
        {
            if (!proc.HasExited) proc.Kill();
            Directory.Delete(dir, true);
        }
    }
}
