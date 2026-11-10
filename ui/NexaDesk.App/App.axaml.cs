using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using NexaDesk.Native;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;

namespace NexaDesk;

public partial class App : Application
{
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
                windows, new LocalAddresses(), deviceName);
            if (startupError != null) vm.Connect.IdentityError = startupError;
            var main = new MainWindow { DataContext = vm };
            windows.Owner = main;
            desktop.MainWindow = main;
            desktop.ShutdownMode = ShutdownMode.OnMainWindowClose;
            desktop.ShutdownRequested += (_, _) => vm.OnExit();
            desktop.Exit += (_, _) =>
            {
                vm.OnExit();
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
