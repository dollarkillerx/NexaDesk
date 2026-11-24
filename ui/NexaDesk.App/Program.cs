using Avalonia;
using NexaDesk.Localization;
using NexaDesk.Services;

namespace NexaDesk;

static class Program
{
    [STAThread]
    public static int Main(string[] args)
    {
        // First: the Windows installer's hooks run here and may end the process. Nothing
        // happens when the app was not installed by Velopack (macOS, a development build).
        var velopack = Velopack.VelopackApp.Build();
        // Uninstalling also removes "Open at login", so nothing is left starting a removed app.
#pragma warning disable CA1416 // the hook only runs on Windows, inside the IsWindows check
        if (OperatingSystem.IsWindows()) velopack.OnBeforeUninstallFastCallback(_ => RunKeyLoginItem.Remove());
#pragma warning restore CA1416
        velopack.Run();
        CrashLog.Install();
        // Windows, unlike macOS, starts a second copy of an app: hand over to the running one.
        // The elevated copy that "Run as administrator" starts waits for this one to quit instead.
        var elevatedRestart = args.Contains("--elevated-restart");
        if (OperatingSystem.IsWindows() && !SingleInstance.TryBecomeFirst(waitForPrevious: elevatedRestart)) return 0;
        App.StartInBackground = OperatingSystem.IsWindows() && args.Contains(RunKeyLoginItem.BackgroundArg);
        // Before any window: text is looked up when views are built.
        var settings = new SettingsStore(Path.Combine(AgentLink.DefaultDataDir(), "settings.json")).Load();
        L.Init(settings.Language, SystemLanguages.Preferred());
#pragma warning disable CA1416 // inside the IsWindows check
        // "Run as administrator": opened by hand, NexaDesk asks Windows (the UAC prompt) and makes
        // way for the elevated copy. Not at login: nobody may be there to answer, and remote control
        // must start all the same. A no carries on without administrator rights.
        if (OperatingSystem.IsWindows() && settings.RunAsAdministrator && !elevatedRestart && !App.StartInBackground
            && !WindowsElevation.IsProcessElevated())
        {
            SingleInstance.Release();
            if (WindowsElevation.Start(["--elevated-restart", .. args]) == null) return 0;
            if (!SingleInstance.TryBecomeFirst()) return 0;
        }
#pragma warning restore CA1416
        return BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);
    }

    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            // §10.3: the video is a native view nothing can draw over, so menus and popups
            // in the toolbar must be windows of their own.
            .With(new AvaloniaNativePlatformOptions { OverlayPopups = false });
}

/// <summary>
/// One NexaDesk per Windows session: a named mutex says one is running, and a named event
/// asks it to show its window when another copy is opened.
/// </summary>
static class SingleInstance
{
    const string MutexName = @"Local\NexaDesk.Running";
    const string ShowEventName = @"Local\NexaDesk.Show";
    static Mutex? _mutex;
    static EventWaitHandle? _show;

    /// <summary>True for the first copy; a later one wakes the first and gets false. With
    /// <paramref name="waitForPrevious"/>, a copy that is quitting for this one is waited for.</summary>
    public static bool TryBecomeFirst(bool waitForPrevious = false)
    {
        try
        {
            _mutex = new Mutex(initiallyOwned: true, MutexName, out var first);
            _show = new EventWaitHandle(false, EventResetMode.AutoReset, ShowEventName);
            if (first) return true;
            if (waitForPrevious)
            {
                try
                {
                    if (_mutex.WaitOne(TimeSpan.FromSeconds(20))) return true;
                }
                catch (AbandonedMutexException)
                {
                    return true; // it quit without letting go: it is ours now
                }
            }
            _show.Set();
            return false;
        }
        catch (UnauthorizedAccessException)
        {
            // A copy running as administrator owns them, and this one may not touch them.
            return false;
        }
    }

    /// <summary>Lets the next copy be first: this one is making way for an elevated one.</summary>
    public static void Release()
    {
        try { _mutex?.ReleaseMutex(); } catch (ApplicationException) { }
        _mutex?.Dispose();
        _show?.Dispose();
        _mutex = null;
        _show = null;
    }

    /// <summary>Calls <paramref name="show"/> (on a background thread) whenever another copy asks.</summary>
    public static void OnShowRequested(Action show)
    {
        if (_show is not { } handle) return;
        new Thread(() =>
        {
            while (handle.WaitOne()) show();
        }) { IsBackground = true, Name = "NexaDesk.Show" }.Start();
    }
}

/// <summary>
/// Writes unhandled exceptions to crash.log next to agent.log before the app goes, so a
/// window that vanished on someone's PC leaves a trace.
/// </summary>
static class CrashLog
{
    public static void Install()
    {
        AppDomain.CurrentDomain.UnhandledException += (_, e) => Write("unhandled", e.ExceptionObject);
        TaskScheduler.UnobservedTaskException += (_, e) => Write("unobserved task", e.Exception);
    }

    static void Write(string kind, object? error)
    {
        try
        {
            var path = Path.Combine(Path.GetDirectoryName(AgentLog.DefaultPath())!, "crash.log");
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            File.AppendAllText(path, $"{DateTime.Now:yyyy-MM-dd HH:mm:ss.fff} {kind}: {error}{Environment.NewLine}");
        }
        catch
        {
            // Nothing more to do while the process goes down.
        }
    }
}
