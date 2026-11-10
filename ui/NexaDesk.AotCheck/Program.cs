// NativeAOT smoke check: the real App, views and view-models, headless, with fakes.
// Usage: NexaDesk.AotCheck <output dir>. Exit code 0 means every check passed.

using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Styling;
using Avalonia.Threading;
using NexaDesk;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using NexaDesk.Views;

var outDir = args.Length > 0 ? args[0] : Path.Combine(Path.GetTempPath(), "nexadesk-aot");
Directory.CreateDirectory(outDir);
var failures = new List<string>();

void Check(bool ok, string what)
{
    Console.WriteLine($"{(ok ? "ok  " : "FAIL")} {what}");
    if (!ok) failures.Add(what);
}

void Pump()
{
    for (var i = 0; i < 3; i++)
    {
        Dispatcher.UIThread.RunJobs();
        AvaloniaHeadlessPlatform.ForceRenderTimerTick();
    }
    Dispatcher.UIThread.RunJobs();
}

#pragma warning disable CS0618 // Bitmap.Save(string): the replacement takes options we do not need
void Save(Window w, string name)
{
    Pump();
    var frame = w.CaptureRenderedFrame();
    if (frame == null)
    {
        Check(false, $"render {name}");
        return;
    }
    var path = Path.Combine(outDir, $"aot-{name}.png");
    frame.Save(path);
    Check(new FileInfo(path).Length > 1000, $"render {name}");
}

T Find<T>(Window w, string name) where T : Control => w.FindControl<T>(name) ?? throw new Exception($"no {name}");

AppBuilder.Configure<App>()
    .UseSkia()
    .UseHeadless(new AvaloniaHeadlessPlatformOptions { UseHeadlessDrawing = false })
    .SetupWithoutStarting();

var now = new DateTimeOffset(2026, 11, 10, 12, 0, 0, TimeSpan.Zero);
var client = new Client();
var link = new Link();
var windows = new WindowService();
var recentPath = Path.Combine(outDir, "recent.json");
File.Delete(recentPath);
var store = new RecentStore(recentPath);
store.Save([new RecentDevice("192.168.1.20:47800", "工作室 Mac mini", "aa", now.AddHours(-1).ToUnixTimeSeconds())]);
var vm = new MainViewModel(client, store, link, new Process(), new Perms(), windows, new Addresses(), "AOT Mac", () => now);
client.Raise(new ReadyEvent(0, new string('3', 64), [new KnownHost("aa", "工作室 Mac mini", 0)], ""));
Check(vm.Connect.Recent.Count == 1 && vm.Connect.Recent[0].IsPaired, "source-generated JSON store and Ready event");

var main = new MainWindow { DataContext = vm };
windows.Owner = main;
main.Show();
Pump();
vm.Connect.Address = "10.0.0.2";
vm.Connect.Code = "abcd-2345";
Pump();
Check(Find<Button>(main, "ConnectButton").IsEffectivelyEnabled, "compiled binding enables Connect");
vm.Connect.ConnectCommand.Execute(null);
Check(client.Last?.Address == "10.0.0.2:47800", "connect request built");
Check(Convert.ToHexString(client.Last!.Encode()).Contains("2A084142434432333435"), "code encoded in field 5");
client.Raise(new ConnectedEvent(client.Last.Tag, "aa", "工作室 Mac mini", new Permissions(true, true, true), 2, true));
Pump();
var session = windows.OpenWindows.OfType<SessionWindow>().SingleOrDefault();
var toolbar = windows.OpenWindows.OfType<ToolbarWindow>().SingleOrDefault();
Check(session != null && toolbar != null, "session window and toolbar opened");
if (session != null && toolbar != null)
{
    var svm = (SessionViewModel)session.DataContext!;
    svm.OnEvent(new StreamChangedEvent(client.Last.Tag, 2, StreamMode.Balanced, 3024, 1964, 60));
    svm.OnHostKey();
    Pump();
    Check(Find<Button>(toolbar, "ResumeButton").IsVisible, "host key shows Resume");
    Save(toolbar, "toolbar");
    Save(session, "session");
}
client.Raise(new DisconnectedEvent(client.Last.Tag, DisconnectReason.PinMismatch, ""));
Pump();
Check(!windows.OpenWindows.OfType<SessionWindow>().Any(), "session window closed on Disconnected");
Check(vm.Connect.Error?.Title.Contains("身份") == true, "disconnect reason explained");

link.Next = null;
await vm.Host.StartCommand.ExecuteAsync(null);
link.Raise(new AgentMessage.Status(1, "192.168.1.20:47800", "K7QM2XPA", "AOT Mac", "id", new string('a', 64), null, true));
Pump();
Check(Find<SelectableTextBlock>(main, "CodeText").Text == "K7QM-2XPA", "code shown large");
Save(main, "main");

link.Raise(new AgentMessage.IncomingRequest(5, "王小明", "id", new string('b', 64), "192.168.1.35:5000", AuthKind.Code,
    new Permissions(true, true, false), true, false, false, false, 60_000, true));
Pump();
var dialog = windows.OpenWindows.OfType<ConsentDialog>().SingleOrDefault();
Check(dialog != null, "consent dialog opened");
if (dialog != null)
{
    Check(Find<Button>(dialog, "DenyButton").IsFocused, "deny focused");
    Check(!Find<Button>(dialog, "AllowButton").IsEffectivelyEnabled, "allow not armed");
    Save(dialog, "consent");
    dialog.Close();
    Pump();
    Check(link.Sent.LastOrDefault() is UiRequest.Decide { Allow: false, ReqId: 5 }, "closing the dialog denies");
}

link.Raise(new AgentMessage.SessionStarted(new HostSession(9, "王小明", "id", new string('b', 64), "192.168.1.35:5000", new Permissions(true, true, true))));
Pump();
var banner = windows.Banner;
Check(banner != null, "banner shown");
if (banner != null)
{
    banner.Close();
    Pump();
    Check(banner.IsVisible, "banner refuses to close");
    Save(banner, "banner");
}

Application.Current!.RequestedThemeVariant = ThemeVariant.Dark;
Save(main, "main-dark");

Console.WriteLine(failures.Count == 0 ? "AOT CHECK PASSED" : $"AOT CHECK FAILED: {failures.Count}");
return failures.Count == 0 ? 0 : 1;

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
    public List<UiRequest> Sent = new();
    public bool IsConnected { get; private set; }
    public event Action<AgentMessage>? MessageReceived;
    public event Action<string?>? Disconnected { add { } remove { } }
    public Task<string?> ConnectAsync(CancellationToken ct) { IsConnected = Next == null; return Task.FromResult(Next); }
    public void Send(UiRequest r) => Sent.Add(r);
    public void Close() => IsConnected = false;
    public void Raise(AgentMessage m) => MessageReceived?.Invoke(m);
}

sealed class Process : IAgentProcess
{
    public bool IsRunning => false;
    public event Action<AgentExit>? Exited { add { } remove { } }
    public string? Start(string l, string n) => null;
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
    public IReadOnlyList<string> Candidates() => ["192.168.1.20", "127.0.0.1"];
}
