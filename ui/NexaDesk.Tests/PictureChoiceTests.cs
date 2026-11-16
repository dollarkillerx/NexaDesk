using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>Quality and resolution in the toolbar, and the one-line network summary.</summary>
public class PictureChoiceTests
{
    static string Hex(byte[] b) => Convert.ToHexString(b).ToLowerInvariant();

    static (SessionViewModel vm, FakeSession s) Make(IViewerPrefs prefs)
    {
        var s = new FakeSession(1);
        return (new SessionViewModel(s, Samples.Connected(1), "192.168.1.20:47800", () => Samples.T0, prefs), s);
    }

    /// <summary>The same golden bytes as crates/nexa-client/src/proto.rs.</summary>
    [Fact]
    public void Commands_match_the_rust_golden_bytes()
    {
        Assert.Equal("ca01020803", Hex(new SessionCommand.SetQuality(StreamQuality.Weak).Encode()));
        Assert.Equal("d2010308b808", Hex(new SessionCommand.SetResolution(1080).Encode()));
    }

    [Fact]
    public void A_new_session_starts_on_auto_and_says_nothing()
    {
        var (vm, s) = Make(new MemoryViewerPrefs());
        Assert.Equal("auto", vm.SelectedQuality.Key);
        Assert.Equal("auto", vm.SelectedResolution.Key);
        Assert.Empty(s.Commands);
    }

    [Fact]
    public void Choices_go_to_the_host_and_are_remembered()
    {
        var prefs = new MemoryViewerPrefs();
        var (vm, s) = Make(prefs);
        vm.SelectedQuality = vm.Qualities.Single(q => q.Key == "weak");
        vm.SelectedResolution = vm.Resolutions.Single(r => r.Key == "native");
        Assert.Equal(
            [new SessionCommand.SetQuality(StreamQuality.Weak), new SessionCommand.SetResolution(SessionCommand.SetResolution.Native)],
            s.Commands);
        Assert.Equal(new ViewerPrefs("weak", "native"), prefs.Current);
    }

    [Fact]
    public void The_next_session_asks_for_the_remembered_choices_at_once()
    {
        var (vm, s) = Make(new MemoryViewerPrefs(new ViewerPrefs("clear", "720")));
        Assert.Equal("clear", vm.SelectedQuality.Key);
        Assert.Equal("720", vm.SelectedResolution.Key);
        Assert.Equal([new SessionCommand.SetQuality(StreamQuality.Clear), new SessionCommand.SetResolution(720)], s.Commands);
    }

    [Fact]
    public void A_damaged_or_foreign_prefs_file_reads_as_auto()
    {
        var dir = Directory.CreateTempSubdirectory();
        try
        {
            var path = Path.Combine(dir.FullName, "viewer-prefs.json");
            var store = new ViewerPrefsStore(path);
            Assert.Equal(new ViewerPrefs(), store.Load());
            store.Save(new ViewerPrefs("balanced", "1080"));
            Assert.Equal(new ViewerPrefs("balanced", "1080"), store.Load());
            File.WriteAllText(path, """{"quality":"ultra","resolution":"4k"}""");
            Assert.Equal(new ViewerPrefs(), store.Load());
            File.WriteAllText(path, "not json");
            Assert.Equal(new ViewerPrefs(), store.Load());
        }
        finally
        {
            dir.Delete(true);
        }
    }

    [Fact]
    public void The_summary_says_how_it_travels_and_how_well()
    {
        var d = new DiagnosticsViewModel("192.168.1.20:47800");
        static StreamStats At(ulong presented, ulong complete, ulong lost, ulong rttUs) =>
            new(complete, lost, 0, 0, 0, presented, rttUs, 0, 0, 0, presented * 10_000, PathKind.Direct);
        d.Update(At(0, 0, 0, 8_000), Samples.T0);
        d.Update(At(60, 600, 0, 8_000), Samples.T0.AddSeconds(1));
        Assert.Contains("8 ms", d.Summary);
        Assert.Contains("60 fps", d.Summary);
        Assert.Equal(NetLevel.Good, d.Level);
        d.Update(At(90, 900, 20, 200_000), Samples.T0.AddSeconds(2));
        Assert.Equal(NetLevel.Poor, d.Level);
        Assert.True(d.IsPoor);
    }
}
