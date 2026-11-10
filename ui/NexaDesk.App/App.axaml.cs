using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using Avalonia.Platform;
using NexaDesk.Native;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;

namespace NexaDesk;

public partial class App : Application
{
    TrayIcon? _tray;

    public override void Initialize() => AvaloniaXamlLoader.Load(this);

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
                startupError = $"无法加载 libnexa_client：{e.Message}";
            }
            var windows = new WindowService();
            var vm = new MainViewModel(core, new RecentStore(Path.Combine(dataDir, "recent.json")),
                new AgentLink(Path.Combine(dataDir, Ipc.SocketName), ui), new AgentProcess(ui), new MacPermissions(),
                windows, new LocalAddresses(), deviceName, settings: new SettingsStore(Path.Combine(dataDir, "settings.json")),
                loginItem: new SmAppServiceLoginItem(), keymap: new NativeKeymap(dataDir));
            if (startupError != null) vm.Connect.IdentityError = startupError;
            var main = new MainWindow { DataContext = vm };
            windows.Owner = main;
            desktop.MainWindow = main;
            // The menu-bar item keeps NexaDesk (agent, consent prompts, banner) running
            // with the window closed; only Quit, or Cmd+Q, ends it.
            desktop.ShutdownMode = ShutdownMode.OnExplicitShutdown;
            var quitting = false;
            void Show()
            {
                main.Show();
                main.WindowState = WindowState.Normal;
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
            main.Opened += async (_, _) => await vm.Host.TryAttachExistingAsync();
        }
        base.OnFrameworkInitializationCompleted();
    }

    static string DeviceName()
    {
        var name = Environment.MachineName;
        if (name.EndsWith(".local", StringComparison.OrdinalIgnoreCase)) name = name[..^6];
        return string.IsNullOrWhiteSpace(name) ? "Mac" : name;
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

/// <summary>The NSStatusItem: what is happening, and the way back to the window or out.</summary>
static class MenuBar
{
    public static TrayIcon Create(Application app, TrayViewModel vm)
    {
        var status = new NativeMenuItem { Header = vm.StatusText, IsEnabled = false };
        var show = new NativeMenuItem { Header = "打开 NexaDesk", Command = vm.ShowCommand };
        var hosting = new NativeMenuItem { Header = vm.HostingLabel, Command = vm.ToggleHostingCommand };
        var disconnect = new NativeMenuItem { Header = "断开当前会话", Command = vm.DisconnectCommand };
        var quit = new NativeMenuItem { Header = "退出 NexaDesk（停止允许控制）", Command = vm.QuitCommand };
        var menu = new NativeMenu();
        foreach (var item in new NativeMenuItemBase[]
                 { status, new NativeMenuItemSeparator(), show, hosting, disconnect, new NativeMenuItemSeparator(), quit })
            menu.Items.Add(item);
        var tray = new TrayIcon
        {
            Icon = new WindowIcon(AssetLoader.Open(new Uri("avares://NexaDesk/Assets/tray.png"))),
            ToolTipText = vm.ToolTip,
            Menu = menu,
            IsVisible = true,
        };
        MacOSProperties.SetIsTemplateIcon(tray, true);
        vm.PropertyChanged += (_, _) =>
        {
            status.Header = vm.StatusText;
            hosting.Header = vm.HostingLabel;
            tray.ToolTipText = vm.ToolTip;
        };
        TrayIcon.SetIcons(app, [tray]);
        return tray;
    }
}
