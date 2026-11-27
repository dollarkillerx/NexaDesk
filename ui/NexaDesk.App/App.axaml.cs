using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using Avalonia.Platform;
using NexaDesk.Localization;
using NexaDesk.Native;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;

namespace NexaDesk;

public partial class App : Application
{
    TrayIcon? _tray;

    /// <summary>Started by "Open at login" on Windows: no window, remote control on.</summary>
    internal static bool StartInBackground;

    public override void Initialize()
    {
        AvaloniaXamlLoader.Load(this);
        ArcLook.Apply(this);
        UseFontsFor(this, Localization.L.Current);
    }

    /// <summary>
    /// Han characters look different in Chinese and Japanese fonts (the same code points), so
    /// Japanese text needs a Japanese font ahead of any Chinese one, and Traditional Chinese a
    /// Traditional one (PingFang TC, Microsoft JhengHei); Korean wants a Hangul font first (Apple SD
    /// Gothic Neo, Malgun Gothic). Simplified Chinese and English keep the defaults.
    /// </summary>
    internal static void UseFontsFor(Application app, string language)
    {
        var japanese = language == Localization.L.Japanese;
        var traditional = language == Localization.L.TraditionalChinese;
        var korean = language == Localization.L.Korean;
        // Inter first for Latin, as Arc sets type (embedded: AppBuilder.WithInterFont); Han, kana
        // and Hangul fall through to the system font for the language, in list order.
        app.Resources["UiFont"] = new Avalonia.Media.FontFamily(
            japanese ? "fonts:Inter#Inter, Hiragino Sans, Hiragino Kaku Gothic ProN, Yu Gothic UI, Meiryo UI, PingFang SC, Microsoft YaHei UI, sans-serif"
            : traditional ? "fonts:Inter#Inter, PingFang TC, Heiti TC, Microsoft JhengHei UI, sans-serif"
            : korean ? "fonts:Inter#Inter, Apple SD Gothic Neo, Malgun Gothic, sans-serif"
            : "fonts:Inter#Inter, PingFang SC, Hiragino Sans GB, Heiti SC, Microsoft YaHei UI, sans-serif");
        app.Resources["MonoFont"] = new Avalonia.Media.FontFamily(
            japanese ? "SF Mono, Menlo, Hiragino Sans, Cascadia Mono, Consolas, Yu Gothic UI, monospace"
            : traditional ? "SF Mono, Menlo, PingFang TC, Cascadia Mono, Consolas, Microsoft JhengHei UI, monospace"
            : korean ? "SF Mono, Menlo, Apple SD Gothic Neo, Cascadia Mono, Consolas, Malgun Gothic, monospace"
            : "SF Mono, Menlo, PingFang SC, Cascadia Mono, Consolas, Microsoft YaHei UI, monospace");
    }

    public override void OnFrameworkInitializationCompleted()
    {
        // Only the real desktop lifetime builds the real services; headless tests build their own.
        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            var ui = new AvaloniaDispatcher();
            var deviceName = DeviceName();
            var dataDir = AgentLink.DefaultDataDir();
            IClientCore core;
            string? startupError = null;
            try
            {
                core = NexaClientHost.Create(new ClientConfig(deviceName), ui);
            }
            catch (Exception e) when (e is NexaException or DllNotFoundException or EntryPointNotFoundException)
            {
                core = new UnavailableClient(e.Message);
                // The viewer side on Windows is docs/WINDOWS.md stage 3; hosting works already.
                startupError = OperatingSystem.IsWindows() && e is NexaException
                    ? L.Get("App.ViewerNotYet")
                    : L.Format("App.LibraryError", e.Message);
            }
            var windows = new WindowService();
            ISystemService? systemService = OperatingSystem.IsWindows() ? new WindowsSystemService() : null;
            var vm = new MainViewModel(core, new RecentStore(Path.Combine(dataDir, "recent.json")),
                new AgentLink(Path.Combine(dataDir, Ipc.SocketName), ui), new AgentProcess(ui, systemService),
                OperatingSystem.IsWindows() ? new NoPermissionsNeeded() : new MacPermissions(),
                windows, new LocalAddresses(), deviceName, settings: new SettingsStore(Path.Combine(dataDir, "settings.json")),
                loginItem: OperatingSystem.IsWindows() ? new RunKeyLoginItem() : new SmAppServiceLoginItem(), keymap: new NativeKeymap(dataDir),
                viewerPrefs: new ViewerPrefsStore(Path.Combine(dataDir, "viewer-prefs.json")),
                passwords: new NativeSavedPasswords(), updater: new VelopackUpdater(),
                serverTrust: new NativeServerTrust(),
                serverDirectory: new ServerDirectory(ServerDirectory.DefaultUrl, Path.Combine(dataDir, "servers.json")),
                serverProbe: new ServerProbe(), agentServer: new NativeAgentServer(dataDir),
                elevation: OperatingSystem.IsWindows() ? new WindowsElevation() : null, systemService: systemService);
            if (startupError != null) vm.Connect.IdentityError = startupError;
            var main = new MainWindow { DataContext = vm };
            if (OperatingSystem.IsWindows())
                main.Icon = new WindowIcon(AssetLoader.Open(new Uri("avares://NexaDesk/Assets/app.ico")));
            windows.Owner = main;
            // At login on Windows only the tray icon appears; the window opens from it.
            if (!StartInBackground) desktop.MainWindow = main;
            // The menu-bar item keeps NexaDesk (agent, consent prompts, banner) running
            // with the window closed; only Quit, or Cmd+Q, ends it.
            desktop.ShutdownMode = ShutdownMode.OnExplicitShutdown;
            var quitting = false;
            void Show()
            {
                // Windows: a window already open stays where it is for Show and Activate,
                // behind other programs (Windows does not let a program in the background
                // take the focus) and, after a while hidden, without having drawn itself:
                // asked from another copy or the tray, nothing appeared. Minimised and
                // restored it draws and comes back; Topmost on and off puts it above the
                // others even where it may not take the focus.
                if (OperatingSystem.IsWindows() && main.IsVisible) main.WindowState = WindowState.Minimized;
                main.Show();
                main.WindowState = WindowState.Normal;
                if (OperatingSystem.IsWindows())
                {
                    main.Topmost = true;
                    main.Topmost = false;
                }
                main.Activate();
            }
            main.Closing += (_, e) =>
            {
                if (quitting) return;
                e.Cancel = true;
                main.Hide();
            };
            vm.Tray.ShowRequested += Show;
            vm.Tray.QuitRequested += () =>
            {
                quitting = true;
                desktop.Shutdown();
            };
            if (this.TryGetFeature<IActivatableLifetime>() is { } activation)
                activation.Activated += (_, e) =>
                {
                    if (e.Kind == ActivationKind.Reopen) Show();
                };
            _tray = MenuBar.Create(this, vm.Tray);
            if (OperatingSystem.IsWindows())
            {
                // A click on the notification-area icon opens the window, as Windows users expect.
                _tray.Clicked += (_, _) => Show();
                // Opening NexaDesk again (Start menu, desktop) shows this copy's window.
                SingleInstance.OnShowRequested(() => Avalonia.Threading.Dispatcher.UIThread.Post(Show));
            }
            desktop.ShutdownRequested += (_, _) =>
            {
                quitting = true;
                vm.OnExit();
            };
            desktop.Exit += (_, _) =>
            {
                vm.OnExit();
                _tray?.Dispose();
                core.Dispose();
            };
            // Remote control comes on by itself at a Windows login, and after a restart that
            // happened while hosting (see MainViewModel.ResumesHosting). That also picks up an
            // agent still running, so the window's own attach would only race it on the link.
            // The server's access key comes out of the Keychain first: the agent starts with it.
            if (StartInBackground || vm.ResumesHosting)
                // On the UI thread, and awaited there: the view models raise events as they load.
                _ = StartHostingAsync(vm);
            else
            {
                _ = vm.Settings.LoadSecretAsync();
                main.Opened += async (_, _) => await vm.Host.TryAttachExistingAsync();
            }
            // The published server list is fetched only when the person switched it on.
            if (vm.Settings.UseServerList) _ = vm.Settings.RefreshListAsync();
            _ = vm.Updates.CheckDailyAsync();
        }
        base.OnFrameworkInitializationCompleted();
    }

    static async Task StartHostingAsync(MainViewModel vm)
    {
        await vm.Settings.LoadSecretAsync();
        await vm.Host.AttachOrStartAsync();
    }

    static string DeviceName()
    {
        var name = Environment.MachineName;
        if (name.EndsWith(".local", StringComparison.OrdinalIgnoreCase)) name = name[..^6];
        return string.IsNullOrWhiteSpace(name) ? (OperatingSystem.IsWindows() ? "PC" : "Mac") : name;
    }
}

/// <summary>Stands in when the library could not load, so the window can say why.</summary>
sealed class UnavailableClient(string why) : IClientCore
{
    public event Action<ClientEvent>? EventReceived
    {
        add { }
        remove { }
    }

    public ISession Connect(ConnectRequest request) => throw new NexaException(NativeMethods.NEXA_E_INTERNAL, why);

    public void Dispose()
    {
    }
}

/// <summary>The NSStatusItem (the notification-area icon on Windows): what is happening, and the way back to the window or out.</summary>
static class MenuBar
{
    public static TrayIcon Create(Application app, TrayViewModel vm)
    {
        var status = new NativeMenuItem { Header = vm.StatusText, IsEnabled = false };
        var show = new NativeMenuItem { Header = L.Get("Tray.Open"), Command = vm.ShowCommand };
        var hosting = new NativeMenuItem { Header = vm.HostingLabel, Command = vm.ToggleHostingCommand };
        var disconnect = new NativeMenuItem { Header = L.Get("Tray.Disconnect"), Command = vm.DisconnectCommand };
        // Only while the banner's × has it hidden.
        var banner = new NativeMenuItem { Header = L.Get("Tray.ShowBanner"), Command = vm.ShowBannerCommand, IsVisible = vm.BannerHidden };
        var quit = new NativeMenuItem { Header = L.Get("Tray.Quit"), Command = vm.QuitCommand };
        var menu = new NativeMenu();
        foreach (var item in new NativeMenuItemBase[]
                 { status, new NativeMenuItemSeparator(), show, banner, hosting, disconnect, new NativeMenuItemSeparator(), quit })
            menu.Items.Add(item);
        // macOS: a template image the menu bar tints; Windows: the app icon in colour.
        var icon = OperatingSystem.IsWindows() ? "avares://NexaDesk/Assets/app.ico" : "avares://NexaDesk/Assets/tray.png";
        var tray = new TrayIcon
        {
            Icon = new WindowIcon(AssetLoader.Open(new Uri(icon))),
            ToolTipText = vm.ToolTip,
            Menu = menu,
            IsVisible = true,
        };
        if (OperatingSystem.IsMacOS()) MacOSProperties.SetIsTemplateIcon(tray, true);
        vm.PropertyChanged += (_, _) =>
        {
            status.Header = vm.StatusText;
            hosting.Header = vm.HostingLabel;
            banner.IsVisible = vm.BannerHidden;
            tray.ToolTipText = vm.ToolTip;
        };
        TrayIcon.SetIcons(app, [tray]);
        return tray;
    }
}
