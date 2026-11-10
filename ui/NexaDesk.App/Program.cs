using Avalonia;

namespace NexaDesk;

static class Program
{
    [STAThread]
    public static int Main(string[] args) => BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);

    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            // §10.3: the video is a native view nothing can draw over, so menus and popups
            // in the toolbar must be windows of their own.
            .With(new AvaloniaNativePlatformOptions { OverlayPopups = false });
}
