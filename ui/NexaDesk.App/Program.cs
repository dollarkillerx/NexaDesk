using Avalonia;
using NexaDesk.Localization;
using NexaDesk.Services;

namespace NexaDesk;

static class Program
{
    [STAThread]
    public static int Main(string[] args)
    {
        // Before any window: text is looked up when views are built.
        var settings = new SettingsStore(Path.Combine(AgentLink.DefaultDataDir(), "settings.json")).Load();
        L.Init(settings.Language, SystemLanguages.Preferred());
        return BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);
    }

    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            // §10.3: the video is a native view nothing can draw over, so menus and popups
            // in the toolbar must be windows of their own.
            .With(new AvaloniaNativePlatformOptions { OverlayPopups = false });
}
