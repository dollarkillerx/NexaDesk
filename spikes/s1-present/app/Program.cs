// M0 S1: Avalonia hosts a native view; Rust presents into it from its own thread.
// Runs a scripted test and prints RESULT lines. Code-only UI keeps NativeAOT simple.

using System.Diagnostics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Platform;
using Avalonia.Threading;
using Avalonia.Themes.Fluent;

[StructLayout(LayoutKind.Sequential)]
struct S1Stats
{
    public ulong submitted, presented, max_interval_us, present_latency_avg_us, present_latency_max_us, next_drawable_max_us;
    public uint drawable_w, drawable_h;
    public ulong handler_calls;
}

static unsafe partial class Native
{
    const string L = "s1_native";
    [LibraryImport(L)] public static partial int s1_attach(nint view, uint w, uint h, float scale, byte sync);
    [LibraryImport(L)] public static partial int s1_resize(uint w, uint h, float scale);
    [LibraryImport(L)] public static partial int s1_set_sync(byte sync);
    [LibraryImport(L)] public static partial void s1_set_target_fps(uint fps);
    [LibraryImport(L)] public static partial void s1_reset_stats();
    [LibraryImport(L)] public static partial int s1_get_stats(S1Stats* s);
    [LibraryImport(L)] public static partial void s1_detach();
    [LibraryImport(L)] public static partial int s1_post_mouse(nint window, double x, double y, uint kind);
    [LibraryImport(L)] public static partial void s1_set_event_cb(delegate* unmanaged[Cdecl]<ulong, void> cb);
}

sealed class VideoHost : NativeControlHost
{
    public int AttachResult = int.MinValue;
    public string Descriptor = "";

    public (uint w, uint h, float s) PixelSize()
    {
        var s = (float)(TopLevel.GetTopLevel(this)?.RenderScaling ?? 1.0);
        return ((uint)Math.Round(Bounds.Width * s), (uint)Math.Round(Bounds.Height * s), s);
    }

    protected override IPlatformHandle CreateNativeControlCore(IPlatformHandle parent)
    {
        var h = base.CreateNativeControlCore(parent);
        Descriptor = h.HandleDescriptor ?? "";
        var (w, hp, s) = PixelSize();
        AttachResult = Native.s1_attach(h.Handle, w, hp, s, 1);
        return h;
    }

    protected override void DestroyNativeControlCore(IPlatformHandle control)
    {
        Native.s1_detach();
        base.DestroyNativeControlCore(control);
    }

    protected override void OnSizeChanged(SizeChangedEventArgs e)
    {
        base.OnSizeChanged(e);
        var (w, h, s) = PixelSize();
        Native.s1_resize(w, h, s);
    }
}

sealed class App : Application
{
    public override void Initialize() => Styles.Add(new FluentTheme());

    public override void OnFrameworkInitializationCompleted()
    {
        var life = (IClassicDesktopStyleApplicationLifetime)ApplicationLifetime!;
        var host = new VideoHost();
        if (Environment.GetEnvironmentVariable("S1_MOUSE") == "1")
        {
            // Mouse-routing check: an Avalonia strip above the native video view.
            var strip = new Border { Height = MouseRun.Strip, Background = Brushes.DarkSlateGray };
            var dock = new DockPanel();
            DockPanel.SetDock(strip, Dock.Top);
            dock.Children.Add(strip);
            dock.Children.Add(host);
            var w = new Window { Title = "NexaDesk S1 mouse", Width = 960, Height = 600, Content = dock, Background = Brushes.Black };
            life.MainWindow = w;
            var m = new MouseRun(w, life);
            w.Opened += async (_, _) => await m.Execute();
        }
        else
        {
            var main = new Window { Title = "NexaDesk S1", Width = 960, Height = 600, Content = host, Background = Brushes.Black };
            life.MainWindow = main;
            var t = new TestRun(main, host, life);
            main.Opened += async (_, _) => await t.Execute();
        }
        base.OnFrameworkInitializationCompleted();
    }
}

sealed class TestRun(Window main, VideoHost host, IClassicDesktopStyleApplicationLifetime life)
{
    static long cbCount, cbOnUi;
    readonly string outDir = Environment.GetEnvironmentVariable("S1_OUT") ?? "/tmp";
    Window? toolbar;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    static void OnEvent(ulong n)
    {
        Interlocked.Increment(ref cbCount);
        Dispatcher.UIThread.Post(() => Interlocked.Increment(ref cbOnUi));
    }

    static void R(string s) => Console.WriteLine("RESULT " + s);

    static unsafe S1Stats Get() { S1Stats s; Native.s1_get_stats(&s); return s; }

    static unsafe void SetCallback(bool on) => Native.s1_set_event_cb(on ? &OnEvent : null);

    async Task Measure(string name, double secs)
    {
        Native.s1_reset_stats();
        var sw = Stopwatch.StartNew();
        await Task.Delay((int)(secs * 1000));
        var s = Get();
        var t = sw.Elapsed.TotalSeconds;
        R($"{name}: submit_fps={s.submitted / t:F1} handler_fps={s.handler_calls / t:F1} presented_fps={s.presented / t:F1} max_interval_ms={s.max_interval_us / 1000.0:F1} " +
          $"present_latency_avg_ms={s.present_latency_avg_us / 1000.0:F2} max={s.present_latency_max_us / 1000.0:F2} next_drawable_max_ms={s.next_drawable_max_us / 1000.0:F2}");
    }

    void Toolbar()
    {
        toolbar = new Window
        {
            Width = 320, Height = 56, CanResize = false, ShowInTaskbar = false, Topmost = true,
            Title = "toolbar", ShowActivated = false,
            WindowDecorations = WindowDecorations.None,
            Background = Brushes.DarkSlateBlue,
            Content = new TextBlock { Text = "TOOLBAR OVER VIDEO", Foreground = Brushes.White, VerticalAlignment = VerticalAlignment.Center, HorizontalAlignment = HorizontalAlignment.Center },
        };
        toolbar.Show(main);
        PlaceToolbar();
        main.PositionChanged += (_, _) => PlaceToolbar();
        main.Resized += (_, _) => PlaceToolbar();
    }

    void PlaceToolbar()
    {
        if (toolbar == null) return;
        var s = main.DesktopScaling;
        var x = main.Position.X + (int)((main.Bounds.Width - toolbar.Width) / 2 * s);
        toolbar.Position = new PixelPoint(x, main.Position.Y + (int)(8 * s));
    }

    void Windows(string tag)
    {
        var tool = Path.Combine(outDir, "winlist");
        using var p = Process.Start(new ProcessStartInfo(tool, Environment.ProcessId.ToString()) { RedirectStandardOutput = true })!;
        var o = p.StandardOutput.ReadToEnd().Trim().Replace("\n", " | ");
        p.WaitForExit();
        R($"windows[{tag}]: {o}");
    }

    public async Task Execute()
    {
        var failed = false;
        try
        {
            SetCallback(true);
            await Task.Delay(1000);
            R($"attach: rc={host.AttachResult} descriptor={host.Descriptor}");
            Toolbar();
            await Task.Delay(800);
            Windows("windowed");

            Native.s1_set_sync(1);
            await Measure("windowed sync=on target=120", 3);
            Native.s1_set_sync(0);
            await Measure("windowed sync=off target=120", 3);
            Native.s1_set_target_fps(1000);
            await Measure("windowed sync=off target=uncapped", 3);
            Native.s1_set_target_fps(120);

            Native.s1_reset_stats();
            Thread.Sleep(500);
            var st = Get();
            R($"ui-stall 500ms: submitted_during={st.submitted} max_interval_ms={st.max_interval_us / 1000.0:F1}");

            var scale = (float)main.RenderScaling;
            int ok = 0, total = 0;
            foreach (var (w, h) in new[] { (640, 400), (1200, 700), (800, 800), (500, 300), (1100, 620), (960, 600) })
            {
                main.Width = w; main.Height = h;
                await Task.Delay(500);
                var s = Get();
                var (ew, eh, _) = host.PixelSize();
                total++;
                if (s.drawable_w == ew && s.drawable_h == eh) ok++;
                else R($"resize mismatch: drawable {s.drawable_w}x{s.drawable_h} expected {ew}x{eh}");
            }
            R($"resize: matched {ok}/{total} scale={scale}");

            main.WindowState = WindowState.FullScreen;
            await Task.Delay(3000);
            PlaceToolbar();
            await Task.Delay(500);
            Windows("fullscreen");
            Native.s1_set_sync(0);
            await Measure("fullscreen sync=off target=120", 3);
            Native.s1_set_target_fps(1000);
            await Measure("fullscreen sync=off target=uncapped", 3);
            Native.s1_set_target_fps(120);
            Native.s1_set_sync(1);
            await Measure("fullscreen sync=on target=120", 3);
            main.WindowState = WindowState.Normal;
            await Task.Delay(1500);

            R($"reverse-callback: rust_thread_calls={Interlocked.Read(ref cbCount)} ran_on_ui={Interlocked.Read(ref cbOnUi)}");
            SetCallback(false);
        }
        catch (Exception e)
        {
            failed = true;
            R("exception: " + e);
        }
        R("done");
        toolbar?.Close();
        life.Shutdown(failed ? 1 : 0);
    }
}

/// Posts synthetic mouse events to this process only (never the real cursor) and
/// records which region Avalonia saw them in.
sealed class MouseRun(Window w, IClassicDesktopStyleApplicationLifetime life)
{
    public const double Strip = 80;
    readonly string outDir = Environment.GetEnvironmentVariable("S1_OUT") ?? "/tmp";
    int stripPress, videoPress, stripMove, videoMove, stripWheel, videoWheel;

    bool InStrip(PointerEventArgs e) => e.GetPosition(w).Y < Strip;

    public async Task Execute()
    {
        w.AddHandler(InputElement.PointerPressedEvent, (_, e) => { if (InStrip(e)) stripPress++; else videoPress++; }, Avalonia.Interactivity.RoutingStrategies.Tunnel | Avalonia.Interactivity.RoutingStrategies.Bubble, true);
        w.AddHandler(InputElement.PointerMovedEvent, (_, e) => { if (InStrip(e)) stripMove++; else videoMove++; }, Avalonia.Interactivity.RoutingStrategies.Tunnel | Avalonia.Interactivity.RoutingStrategies.Bubble, true);
        w.AddHandler(InputElement.PointerWheelChangedEvent, (_, e) => { if (InStrip(e)) stripWheel++; else videoWheel++; }, Avalonia.Interactivity.RoutingStrategies.Tunnel | Avalonia.Interactivity.RoutingStrategies.Bubble, true);
        await Task.Delay(1500);
        var clientH = w.ClientSize.Height;
        var handle = w.TryGetPlatformHandle();
        Console.WriteLine($"RESULT window handle: {handle?.HandleDescriptor}");
        foreach (var region in new[] { "strip", "video" })
        {
            var y = region == "strip" ? Strip / 2 : Strip + (clientH - Strip) / 2;
            var codes = new List<int>();
            foreach (var kind in new uint[] { 0, 1, 2, 3 })
            {
                codes.Add(Native.s1_post_mouse(handle!.Handle, 300, y, kind));
                await Task.Delay(50);
            }
            Console.WriteLine($"RESULT posted to {region}: rc={string.Join(",", codes)}");
            await Task.Delay(300);
        }
        Console.WriteLine($"RESULT strip: press={stripPress} move={stripMove} wheel={stripWheel}");
        Console.WriteLine($"RESULT video: press={videoPress} move={videoMove} wheel={videoWheel}");
        life.Shutdown(0);
    }
}

static class Program
{
    [STAThread]
    public static int Main(string[] args) =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            .With(new MacOSPlatformOptions { })
            .StartWithClassicDesktopLifetime(args);
}
