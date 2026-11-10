using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Styling;
using NexaDesk.Proto;
using NexaDesk.ViewModels;
using NexaDesk.Views;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>Mute, display switching and the audio permission, from the UI's side.</summary>
public class SessionFeatureTests
{
    static string Hex(byte[] b) => Convert.ToHexString(b).ToLowerInvariant();
    static byte[] Bytes(string hex) => Convert.FromHexString(hex.Replace(" ", ""));

    static readonly IReadOnlyList<DisplayInfo> TwoDisplays =
    [
        new DisplayInfo(0, "Built-in display", 3024, 1964),
        new DisplayInfo(1, "Display 2", 2560, 1440),
    ];

    static ConnectedEvent Connected(bool audio, IReadOnlyList<DisplayInfo>? displays = null) =>
        new(4, Samples.HostKey, "工作室 Mac mini", new Permissions(true, true, true, Audio: audio), 2, false,
            displays ?? TwoDisplays);

    static (SessionViewModel vm, FakeSession s) Make(bool audio = true, IReadOnlyList<DisplayInfo>? displays = null)
    {
        var s = new FakeSession(4);
        return (new SessionViewModel(s, Connected(audio, displays), "192.168.1.20:47800", () => Samples.T0), s);
    }

    // Same golden bytes as crates/nexa-client/src/proto.rs.
    [Fact]
    public void New_commands_match_the_rust_golden_bytes()
    {
        Assert.Equal("a201020801", Hex(new SessionCommand.SetAudio(true).Encode()));
        Assert.Equal("a20100", Hex(new SessionCommand.SetAudio(false).Encode()));
        Assert.Equal("aa01020801", Hex(new SessionCommand.SelectDisplay(1).Encode()));
    }

    [Fact]
    public void Displays_stream_display_and_audio_stats_decode()
    {
        // Event{ connected{ host_name: "n", displays: [{id 1, name "d", w 2, h 3}] } }
        var c = Assert.IsType<ConnectedEvent>(ClientCodec.DecodeEvent(Bytes("1a0f 12016e a201 09 0801 120164 1802 2003")));
        Assert.Equal("n", c.HostName);
        Assert.Equal([new DisplayInfo(1, "d", 2, 3)], c.Displays);
        // Event{ stream{ codec 2, mode game, display 1 } }
        var st = Assert.IsType<StreamChangedEvent>(ClientCodec.DecodeEvent(Bytes("2207 0802 1002 a00101")));
        Assert.Equal((StreamMode.Game, 1u), (st.Mode, st.Display));
        // Event{ stats{ audio_played 5, audio_concealed 2, audio_buffer_us 1000 } }
        var stats = Assert.IsType<StatsEvent>(ClientCodec.DecodeEvent(Bytes("420a f00105 f80102 8002e807"))).Stats;
        Assert.Equal((5ul, 2ul, 1000u), (stats.AudioPlayed, stats.AudioConcealed, stats.AudioBufferUs));
        // An older library sends neither: no displays, display 0.
        var old = Assert.IsType<ConnectedEvent>(ClientCodec.DecodeEvent(Bytes("1a03 12016e")));
        Assert.Empty(old.Displays!);
    }

    [Fact]
    public void Mute_tells_the_library_and_unmute_undoes_it()
    {
        var (vm, s) = Make();
        Assert.True(vm.CanHearAudio);
        Assert.Equal("声音：播放中", vm.AudioText);
        vm.ToggleMuteCommand.Execute(null);
        Assert.True(vm.IsMuted);
        Assert.Equal("声音：已静音", vm.AudioText);
        Assert.Equal("取消静音", vm.MuteLabel);
        vm.ToggleMuteCommand.Execute(null);
        Assert.Equal([new SessionCommand.SetAudio(false), new SessionCommand.SetAudio(true)],
            s.Commands.OfType<SessionCommand.SetAudio>());
    }

    [Fact]
    public void Without_the_audio_permission_there_is_no_mute_and_nothing_is_sent()
    {
        var (vm, s) = Make(audio: false);
        Assert.False(vm.CanHearAudio);
        Assert.Equal("声音：对方未共享", vm.AudioText);
        vm.ToggleMuteCommand.Execute(null);
        Assert.Empty(s.Commands.OfType<SessionCommand.SetAudio>());
    }

    [Fact]
    public void Picking_a_display_asks_the_host_and_the_host_has_the_last_word()
    {
        var (vm, s) = Make();
        Assert.True(vm.HasManyDisplays);
        Assert.Equal(["Built-in display · 3024×1964", "Display 2 · 2560×1440"], vm.Displays.Select(d => d.Label));
        Assert.Equal(0u, vm.SelectedDisplay!.Id);
        Assert.Empty(s.Commands);

        vm.SelectedDisplay = vm.Displays[1];
        Assert.Equal([new SessionCommand.SelectDisplay(1)], s.Commands.OfType<SessionCommand.SelectDisplay>());

        // The host stayed on display 0 (say the other one went away): the picker follows
        // without asking again.
        vm.OnEvent(new StreamChangedEvent(4, 2, StreamMode.Balanced, 3024, 1964, 60, Display: 0));
        Assert.Equal(0u, vm.SelectedDisplay!.Id);
        vm.OnEvent(new StreamChangedEvent(4, 2, StreamMode.Balanced, 2560, 1440, 60, Display: 1));
        Assert.Equal(1u, vm.SelectedDisplay!.Id);
        Assert.Single(s.Commands.OfType<SessionCommand.SelectDisplay>());
    }

    [Fact]
    public void One_display_hides_the_picker()
    {
        var (vm, _) = Make(displays: [new DisplayInfo(0, "Built-in display", 3024, 1964)]);
        Assert.False(vm.HasManyDisplays);
    }

    [Fact]
    public void The_mode_switch_drives_the_host_and_follows_it()
    {
        var (vm, s) = Make();
        vm.IsGame = true;
        Assert.Equal(new SessionCommand.SetMode(StreamMode.Game), s.Commands.OfType<SessionCommand.SetMode>().Last());
        // The host answers in its own time; until then and after, the toolbar shows what
        // the host does, and following it sends nothing.
        var sent = s.Commands.Count;
        vm.OnEvent(new StreamChangedEvent(4, 2, StreamMode.Game, 3024, 1964, 120));
        Assert.True(vm.IsGame);
        Assert.Equal(sent, s.Commands.Count);
    }

    [Fact]
    public void Consent_audio_is_off_by_default_and_granted_only_when_ticked()
    {
        var link = new FakeLink();
        var first = new ConsentViewModel(Samples.Request(1), link, Samples.T0);
        Assert.False(first.AllowAudio);
        first.Tick(Samples.T0.AddSeconds(2));
        first.AllowCommand.Execute(null);
        Assert.False(link.Last<UiRequest.Decide>().Perms.Audio);

        var second = new ConsentViewModel(Samples.Request(2), link, Samples.T0);
        second.AllowAudio = true;
        second.Tick(Samples.T0.AddSeconds(2));
        second.AllowCommand.Execute(null);
        var decide = link.Last<UiRequest.Decide>();
        Assert.True(decide.Perms.Audio);
        // On the wire: permissions field 5.
        Assert.Contains("2801", Hex(decide.Encode()));
    }

    // ----- rendering -------------------------------------------------------------------

    static string OutDir()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !Directory.Exists(Path.Combine(dir.FullName, "NexaDesk.App"))) dir = dir.Parent;
        var path = Path.Combine(dir?.FullName ?? Path.GetTempPath(), "out");
        Directory.CreateDirectory(path);
        return path;
    }

    static void Save(Window w, string name, ThemeVariant theme)
    {
        for (var i = 0; i < 3; i++)
        {
            Avalonia.Threading.Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        }
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
        var frame = w.CaptureRenderedFrame() ?? throw new Xunit.Sdk.XunitException("no frame");
        var file = Path.Combine(OutDir(), $"{name}-{(theme == ThemeVariant.Dark ? "dark" : "light")}.png");
#pragma warning disable CS0618 // the replacement takes encoder options these tests do not need
        frame.Save(file);
#pragma warning restore CS0618
        Assert.True(new FileInfo(file).Length > 1000);
    }

    static void WithTheme(ThemeVariant theme, Action a)
    {
        var app = Application.Current!;
        var before = app.RequestedThemeVariant;
        app.RequestedThemeVariant = theme;
        try { a(); }
        finally { app.RequestedThemeVariant = before; }
    }

    [AvaloniaFact]
    public void Toolbar_with_sound_and_two_displays()
    {
        foreach (var theme in new[] { ThemeVariant.Light, ThemeVariant.Dark })
            WithTheme(theme, () =>
            {
                var (vm, s) = Make();
                vm.OnEvent(new StreamChangedEvent(4, 2, StreamMode.Office, 3024, 1964, 60));
                var t = new ToolbarWindow { DataContext = vm };
                t.Show();
                Save(t, "toolbar-sound-displays", theme);
                Assert.True(t.FindControl<Button>("MuteButton")!.IsVisible);
                Assert.True(t.FindControl<ComboBox>("DisplayPicker")!.IsVisible);
                vm.ToggleMuteCommand.Execute(null);
                Save(t, "toolbar-muted", theme);
                Assert.Equal("取消静音", t.FindControl<Button>("MuteButton")!.Content);
                t.Close();

                var (plain, _) = Make(audio: false, displays: [new DisplayInfo(0, "Built-in display", 3024, 1964)]);
                var t2 = new ToolbarWindow { DataContext = plain };
                t2.Show();
                Assert.False(t2.FindControl<Button>("MuteButton")!.IsVisible);
                Assert.False(t2.FindControl<ComboBox>("DisplayPicker")!.IsVisible);
                t2.Close();
            });
    }

    [AvaloniaFact]
    public void Consent_dialog_offers_sound_unticked()
    {
        foreach (var theme in new[] { ThemeVariant.Light, ThemeVariant.Dark })
            WithTheme(theme, () =>
            {
                var vm = new ConsentViewModel(Samples.Request(input: true), new FakeLink(), DateTimeOffset.Now);
                var w = new ConsentDialog { DataContext = vm };
                w.Show();
                Save(w, "consent-sound", theme);
                Assert.False(w.FindControl<CheckBox>("AudioBox")!.IsChecked);
                w.Close();
            });
    }
}
