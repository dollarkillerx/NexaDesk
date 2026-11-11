// README screenshots: the real App, views and view-models, headless, with fakes and
// made-up data. Usage: NexaDesk.Screenshots <output dir>.

using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Avalonia.Threading;
using NexaDesk;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;

var outDir = args.Length > 0 ? args[0] : "docs/images";
Directory.CreateDirectory(outDir);
const double Scale = 2;

void Pump()
{
    for (var i = 0; i < 5; i++)
    {
        Dispatcher.UIThread.RunJobs();
        AvaloniaHeadlessPlatform.ForceRenderTimerTick();
    }
    Dispatcher.UIThread.RunJobs();
}

#pragma warning disable CS0618 // Bitmap.Save(string): the replacement takes options we do not need
void Shot(Window w, string name)
{
    Pump();
    var size = new PixelSize((int)Math.Ceiling(w.Bounds.Width * Scale), (int)Math.Ceiling(w.Bounds.Height * Scale));
    using var bitmap = new RenderTargetBitmap(size, new Vector(96 * Scale, 96 * Scale));
    bitmap.Render(w);
    var path = Path.Combine(outDir, $"{name}.png");
    bitmap.Save(path);
    Console.WriteLine($"{path} {size.Width}x{size.Height}");
}

AppBuilder.Configure<App>()
    .UseSkia()
    .UseHeadless(new AvaloniaHeadlessPlatformOptions { UseHeadlessDrawing = false })
    .SetupWithoutStarting();

var now = DateTimeOffset.Now;
var client = new Client();
var link = new Link();
var windows = new WindowService();
var temp = Directory.CreateTempSubdirectory("nexadesk-shots");
var store = new RecentStore(Path.Combine(temp.FullName, "recent.json"));
const string StudioKey = "5c1e9a7f03b2d84e6a1f97c2b05d3e8814a6f0c97d2b3e5a1c8f604d92e7b13a";
store.Save([
    new RecentDevice("studio.tail1234.ts.net:47800", "工作室 Mac mini", StudioKey, now.AddHours(-2).ToUnixTimeSeconds()),
    new RecentDevice("192.168.1.42:47800", "客厅 iMac", "", now.AddDays(-3).ToUnixTimeSeconds()),
]);
var vm = new MainViewModel(client, store, link, new Process(), new Perms(), windows, new Addresses(), "我的 MacBook Pro", () => now);
client.Raise(new ReadyEvent(0, new string('3', 64), [new KnownHost(StudioKey, "工作室 Mac mini", 0)], ""));
client.Raise(new TailnetDevicesEvent(0,
    [
        new TailnetDevice("studio", "100.84.17.23:47800", true, "macOS", "studio.tail1234.ts.net"),
        new TailnetDevice("office-mbp", "100.92.5.110:47800", false, "macOS", "office-mbp.tail1234.ts.net"),
    ],
    TailscaleState.Running, "macbook-pro", "macbook-pro.tail1234.ts.net", ["100.71.3.58"]));
client.Raise(new LanDevicesEvent(0, [new LanDevice("客厅 iMac", "192.168.1.42:47800", "macOS", "8e2f61d07a9c4b35")]));

// Taller than the default so both device lists show without scrolling.
var main = new MainWindow { DataContext = vm, Height = 1010 };
windows.Owner = main;
main.Show();
Pump();

link.Next = null;
await vm.Host.StartCommand.ExecuteAsync(null);
link.Raise(new AgentMessage.Status(1, "[::]:47800", "K7QM2XPA", "我的 MacBook Pro", "3f9a1c07d2e84b56", new string('a', 64), null, true));
link.Raise(new AgentMessage.Pairings([
    new PairedDevice("9d4b2e71c0a83f56e1b7d9204c6a58f3b2e0d1c7a94f6e8b03d5c2a17e9b4f60", "a41c7e09b3d25f68", "设计部 MacBook Air",
        new Permissions(true, true, true, true, false), (ulong)now.AddDays(-12).ToUnixTimeSeconds(), (ulong)now.AddDays(-1).ToUnixTimeSeconds(), false),
]));
Pump();
Shot(main, "main");
Application.Current!.RequestedThemeVariant = ThemeVariant.Dark;
Shot(main, "main-dark");
Application.Current.RequestedThemeVariant = ThemeVariant.Light;

link.Raise(new AgentMessage.IncomingRequest(5, "设计部 MacBook Air", "a41c7e09b3d25f68",
    "9d4b2e71c0a83f56e1b7d9204c6a58f3b2e0d1c7a94f6e8b03d5c2a17e9b4f60", "100.92.5.110:51934", AuthKind.Code,
    new Permissions(true, true, true, true, false), true, false, false, false, 60_000, true));
Pump();
var dialog = windows.OpenWindows.OfType<ConsentDialog>().Single();
// As it looks a few seconds in: Allow armed (it stays disabled for the first second).
((ConsentViewModel)dialog.DataContext!).Tick(DateTimeOffset.Now.AddSeconds(3));
Shot(dialog, "consent");
dialog.Close();
Pump();

link.Raise(new AgentMessage.SessionStarted(new HostSession(9, "设计部 MacBook Air", "a41c7e09b3d25f68",
    "9d4b2e71c0a83f56e1b7d9204c6a58f3b2e0d1c7a94f6e8b03d5c2a17e9b4f60", "100.92.5.110:51934",
    new Permissions(true, true, true, true, false))));
Pump();
if (windows.Banner is { } banner) Shot(banner, "banner");

vm.Connect.Address = "studio.tail1234.ts.net";
vm.Connect.Code = "abcd-2345";
vm.Connect.ConnectCommand.Execute(null);
client.Raise(new ConnectedEvent(client.Last!.Tag, StudioKey, "工作室 Mac mini", new Permissions(true, true, true, true, true), 2, false));
Pump();
var session = windows.OpenWindows.OfType<SessionWindow>().Single();
var svm = (SessionViewModel)session.DataContext!;
svm.OnEvent(new StreamChangedEvent(client.Last.Tag, 2, StreamMode.Balanced, 3024, 1964, 60));
Pump();
Shot(windows.OpenWindows.OfType<ToolbarWindow>().Single(), "toolbar");

temp.Delete(true);
return 0;

sealed class Session(ulong tag) : ISession
{
    public ulong Tag { get; } = tag;
    public bool IsClosed { get; private set; }
    public string LastError => "";
    public int AttachView(nint v, uint w, uint h, float s) => 0;
    public int ResizeView(uint w, uint h, float s) => 0;
    public int DetachView() => 0;
    public int SetInputActive(bool a) => 0;
    public int Send(SessionCommand c) => 0;
    public void Close() => IsClosed = true;
}

sealed class Client : IClientCore
{
    public ConnectRequest? Last;
    public event Action<ClientEvent>? EventReceived;
    public ISession Connect(ConnectRequest r) { Last = r; return new Session(r.Tag); }
    public void Raise(ClientEvent e) => EventReceived?.Invoke(e);
    public void Dispose() { }
}

sealed class Link : IAgentLink
{
    public string? Next = "none";
    public bool IsConnected { get; private set; }
    public event Action<AgentMessage>? MessageReceived;
    public event Action<string?>? Disconnected { add { } remove { } }
    public Task<string?> ConnectAsync(CancellationToken ct) { IsConnected = Next == null; return Task.FromResult(Next); }
    public void Send(UiRequest r) { }
    public void Close() => IsConnected = false;
    public void Raise(AgentMessage m) => MessageReceived?.Invoke(m);
}

sealed class Process : IAgentProcess
{
    public bool IsRunning => false;
    public event Action<AgentExit>? Exited { add { } remove { } }
    public string? Start(string l, string n, string s, bool a, string? w = null) => null;
    public void Kill() { }
}

sealed class Perms : IPermissionProbe
{
    public PermissionState Check() => new(true, true);
    public void Request(PermissionKind k) { }
    public void OpenSettings(PermissionKind k) { }
}

sealed class Addresses : ILocalAddresses
{
    public IReadOnlyList<string> Candidates() => ["192.168.1.20", "100.71.3.58", "127.0.0.1"];
}
