using System.Diagnostics;
using System.Text;
using NexaDesk.Services;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// The agent's pipes are UTF-8 both ways (agent.log and error messages from it, a password
/// to it on stdin), whatever the system's code page: on Chinese Windows that was GBK, and
/// agent.log came out garbled.
/// </summary>
public class AgentPipeTests
{
    [Fact]
    public void The_agent_is_spoken_to_in_UTF8_without_a_byte_order_mark()
    {
        var info = AgentProcess.StartInfo("nexa-agent");
        foreach (var encoding in new[] { info.StandardErrorEncoding, info.StandardOutputEncoding, info.StandardInputEncoding })
        {
            Assert.NotNull(encoding);
            Assert.Equal(Encoding.UTF8.CodePage, encoding.CodePage);
            Assert.Empty(encoding.GetPreamble());
        }
        Assert.True(info.CreateNoWindow);
        Assert.False(info.UseShellExecute);
    }

    [Fact]
    public async Task Text_outside_ASCII_crosses_the_pipes_intact_on_Windows()
    {
        Assert.SkipUnless(OperatingSystem.IsWindows(), "the console code page is a Windows matter");
        // PowerShell stands in for the agent: it answers with the bytes it got on stdin, in
        // hex, and with UTF-8 bytes on stderr, as a Rust program writes to a pipe.
        var info = AgentProcess.StartInfo("powershell.exe");
        foreach (var arg in new[] { "-NoProfile", "-NonInteractive", "-Command",
            "$in = [Console]::OpenStandardInput(); $buf = New-Object byte[] 256; $n = $in.Read($buf, 0, 256); " +
            "$text = 'got ' + [BitConverter]::ToString($buf, 0, $n) + ' ' + [char]0x2014 + ' ' + [char]0x8DDF + [char]0x306E; " +
            "$b = [Text.Encoding]::UTF8.GetBytes($text); $err = [Console]::OpenStandardError(); $err.Write($b, 0, $b.Length); $err.Flush()" })
        {
            info.ArgumentList.Add(arg);
        }
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(TestContext.Current.CancellationToken);
        timeout.CancelAfter(TimeSpan.FromSeconds(30));
        using var p = Process.Start(info)!;
        const string password = "密码pässword";
        await p.StandardInput.WriteAsync(password);
        p.StandardInput.Close();
        var errors = p.StandardError.ReadToEndAsync(timeout.Token);
        await p.WaitForExitAsync(timeout.Token);

        var expected = BitConverter.ToString(Encoding.UTF8.GetBytes(password));
        Assert.Equal($"got {expected} — 跟の", (await errors).Trim());
    }
}
