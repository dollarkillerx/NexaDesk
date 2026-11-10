using NexaDesk.Native;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

public class TextsTests
{
    [Theory]
    [InlineData("abcd-2345", "ABCD2345")]
    [InlineData(" ABCD 2345 ", "ABCD2345")]
    [InlineData("ABCD234", null)]
    [InlineData("ABCD2341", null)] // 1 is not base32
    [InlineData("ABCD23456", null)]
    [InlineData("", null)]
    public void Codes_are_normalized_or_refused(string input, string? expected) =>
        Assert.Equal(expected, Texts.NormalizeCode(input));

    [Theory]
    [InlineData("192.168.1.20:47800", "192.168.1.20:47800")]
    [InlineData(" 192.168.1.20 ", "192.168.1.20:47800")]
    [InlineData("mac.tail1234.ts.net:9000", "mac.tail1234.ts.net:9000")]
    [InlineData("[::1]:47800", "[::1]:47800")]
    [InlineData("[fe80::1]", "[fe80::1]:47800")]
    [InlineData("::1", null)]
    [InlineData("host:0", null)]
    [InlineData("host:70000", null)]
    [InlineData("host:+80", null)]
    [InlineData(":80", null)]
    [InlineData("a b:80", null)]
    public void Addresses_are_normalized_or_refused(string input, string? expected) =>
        Assert.Equal(expected, Texts.NormalizeAddress(input));

    [Fact]
    public void Every_disconnect_reason_has_its_own_explanation()
    {
        var texts = Enum.GetValues<DisconnectReason>().Select(Texts.Disconnect).ToList();
        Assert.All(texts, t => Assert.False(string.IsNullOrWhiteSpace(t.Title) || string.IsNullOrWhiteSpace(t.Detail)));
        Assert.Equal(texts.Count, texts.Select(t => t.Title).Distinct().Count());
    }

    [Fact]
    public void Fingerprints_group_by_four_on_two_lines()
    {
        var f = Texts.Fingerprint(Samples.ViewerKey);
        var lines = f.Split('\n');
        Assert.Equal(2, lines.Length);
        Assert.StartsWith("3F2A 9C1B 0D4E 77AA", lines[0]);
        Assert.Equal(8, lines[1].Split(' ').Length);
        Assert.Equal("3F2A 9C1B 0D4E 77AA", Texts.ShortFingerprint(Samples.ViewerKey));
    }

    [Theory]
    [InlineData("192.168.1.5:47800", "局域网")]
    [InlineData("100.101.5.6:47800", "Tailscale")]
    [InlineData("127.0.0.1:47800", "回环")]
    [InlineData("8.8.8.8:47800", "公网")]
    [InlineData("[::1]:47800", "回环")]
    public void Paths_are_classified(string address, string expected) =>
        Assert.Contains(expected, Texts.PathOf(address));
}

public class ConnectViewModelTests
{
    readonly FakeClient _client = new();
    readonly RecordingWindows _windows = new();
    readonly MemoryRecent _recent = new();

    ConnectViewModel Vm() => new(_client, _recent, _windows, () => Samples.T0);

    [Fact]
    public void Connect_needs_a_valid_address_and_code()
    {
        var vm = Vm();
        Assert.False(vm.ConnectCommand.CanExecute(null));
        vm.Address = "192.168.1.20:47800";
        vm.Code = "ABCD";
        Assert.False(vm.ConnectCommand.CanExecute(null));
        Assert.NotNull(vm.CodeHint);
        vm.Code = "abcd-2345";
        Assert.True(vm.ConnectCommand.CanExecute(null));
        Assert.Null(vm.CodeHint);
    }

    [Fact]
    public void Connecting_sends_the_normalized_code_then_opens_the_session_window()
    {
        var vm = Vm();
        vm.Address = "192.168.1.20";
        vm.Code = "abcd-2345";
        vm.ConnectCommand.Execute(null);

        var req = Assert.Single(_client.Requests);
        Assert.Equal("192.168.1.20:47800", req.Address);
        Assert.Equal(new ConnectAuth.Code("ABCD2345"), req.Auth);
        Assert.True(vm.IsConnecting);
        Assert.False(vm.ConnectCommand.CanExecute(null), "one attempt at a time");

        _client.Raise(new ConnectingEvent(req.Tag));
        Assert.Contains("等待对方确认", vm.ProgressText);
        _client.Raise(Samples.Connected(req.Tag));
        Assert.False(vm.IsConnecting);
        Assert.Equal("", vm.Code);
        var session = Assert.Single(_windows.Sessions);
        Assert.Equal("工作室 Mac mini", session.HostName);
        var saved = Assert.Single(_recent.Saved);
        Assert.Equal(("192.168.1.20:47800", Samples.HostKey), (saved.Address, saved.HostKey));
        Assert.True(Assert.Single(vm.Recent).IsPaired, "paired_now marks it paired at once");
    }

    [Fact]
    public void A_refusal_before_connecting_shows_what_went_wrong()
    {
        var vm = Vm();
        vm.Address = "10.0.0.2:47800";
        vm.Code = "ABCD2345";
        vm.ConnectCommand.Execute(null);
        var tag = _client.Requests[0].Tag;
        _client.Raise(new DisconnectedEvent(tag, DisconnectReason.AuthFailed, "authentication failed"));
        Assert.False(vm.IsConnecting);
        Assert.Equal("一次性码不正确", vm.Error?.Title);
        Assert.True(_client.Sessions[0].IsClosed);
        Assert.Empty(_windows.Sessions);
    }

    [Fact]
    public void Cancel_closes_the_attempt_without_an_error()
    {
        var vm = Vm();
        vm.Address = "10.0.0.2:47800";
        vm.Code = "ABCD2345";
        vm.ConnectCommand.Execute(null);
        vm.CancelCommand.Execute(null);
        Assert.True(_client.Sessions[0].IsClosed);
        _client.Raise(new DisconnectedEvent(_client.Requests[0].Tag, DisconnectReason.ClosedLocally, ""));
        Assert.Null(vm.Error);
        Assert.False(vm.IsConnecting);
    }

    [Fact]
    public void A_paired_recent_device_connects_by_key_and_an_unpaired_one_asks_for_a_code()
    {
        _recent.Saved.Add(new RecentDevice("10.0.0.9:47800", "书房 iMac", Samples.HostKey, Samples.T0.ToUnixTimeSeconds() - 3600));
        _recent.Saved.Add(new RecentDevice("10.0.0.8:47800", "旧电脑", "ff", Samples.T0.ToUnixTimeSeconds() - 60));
        var vm = Vm();
        _client.Raise(new ReadyEvent(0, Samples.ViewerKey, [new KnownHost(Samples.HostKey, "书房 iMac", 0)], ""));
        Assert.StartsWith("3F2A", vm.ThisDeviceFingerprint);
        var imac = vm.Recent.Single(r => r.Name == "书房 iMac");
        var old = vm.Recent.Single(r => r.Name == "旧电脑");
        Assert.Equal("已配对", imac.PairedText);
        Assert.Equal("未配对", old.PairedText);
        Assert.Equal("1 小时前", imac.LastSeen);

        var focused = false;
        vm.FocusCodeRequested += () => focused = true;
        old.ConnectCommand.Execute(null);
        Assert.Empty(_client.Requests);
        Assert.Equal("10.0.0.8:47800", vm.Address);
        Assert.True(focused);

        imac.ConnectCommand.Execute(null);
        Assert.Equal(new ConnectAuth.KnownHost(Samples.HostKey), Assert.Single(_client.Requests).Auth);
    }

    [Fact]
    public void A_session_ended_by_the_host_is_explained_in_the_main_window()
    {
        var vm = Vm();
        vm.Address = "10.0.0.2:47800";
        vm.Code = "ABCD2345";
        vm.ConnectCommand.Execute(null);
        var tag = _client.Requests[0].Tag;
        _client.Raise(Samples.Connected(tag));
        _client.Raise(new DisconnectedEvent(tag, DisconnectReason.HostEnded, "ended by the host"));
        Assert.True(_windows.Sessions[0].IsEnded);
        Assert.Contains("对方结束了会话", vm.Error?.Title);
        Assert.Empty(vm.Sessions);
    }
}

public class SessionViewModelTests
{
    static (SessionViewModel vm, FakeSession s) Make(bool input = true, bool clipboard = true)
    {
        var s = new FakeSession(4);
        var vm = new SessionViewModel(s, Samples.Connected(4, input, clipboard), "192.168.1.20:47800", () => Samples.T0);
        return (vm, s);
    }

    [Fact]
    public void Capture_follows_window_activation()
    {
        var (vm, s) = Make();
        Assert.Empty(s.InputActive);
        vm.OnWindowActivated();
        vm.OnWindowActivated();
        vm.OnWindowDeactivated();
        vm.OnWindowActivated();
        Assert.Equal([true, false, true], s.InputActive);
        Assert.True(vm.InputActive);
    }

    [Fact]
    public void Without_input_permission_capture_is_never_requested()
    {
        var (vm, s) = Make(input: false);
        vm.OnWindowActivated();
        vm.PointerMoved(10, 10);
        Assert.DoesNotContain(true, s.InputActive);
        Assert.Empty(s.Commands);
        Assert.Contains("仅查看", vm.CaptureText);
    }

    [Fact]
    public void The_host_key_releases_capture_and_shows_the_toolbar_until_resumed()
    {
        var (vm, s) = Make();
        vm.OnWindowActivated();
        vm.IsToolbarVisible = false;
        var activations = 0;
        vm.ActivateRequested += () => activations++;

        vm.OnEvent(new HostKeyEvent(4));
        Assert.Equal([true, false], s.InputActive);
        Assert.True(vm.IsToolbarVisible);
        Assert.True(vm.IsSuspended);
        Assert.Contains("已释放键盘", vm.CaptureText);

        // Re-activating the window does not grab the keyboard again by itself.
        vm.OnWindowDeactivated();
        vm.OnWindowActivated();
        Assert.Equal([true, false], s.InputActive);

        // A click on the video resumes instead of being sent.
        Assert.True(vm.OnVideoPressed());
        Assert.Equal([true, false, true], s.InputActive);
        Assert.Equal(1, activations);
        Assert.False(vm.OnVideoPressed());
    }

    [Fact]
    public void A_refused_capture_is_shown_as_unavailable()
    {
        var (vm, s) = Make();
        s.SetInputResult = NativeMethods.NEXA_E_NOT_PERMITTED;
        vm.OnWindowActivated();
        Assert.Equal(CaptureState.Unavailable, vm.Capture);
        Assert.False(vm.InputActive);
        Assert.True(vm.CaptureNeedsPermission);
    }

    [Fact]
    public void Pointer_input_is_sent_only_while_captured_and_wheel_fractions_add_up()
    {
        var (vm, s) = Make();
        vm.PointerMoved(1, 1);
        Assert.Empty(s.Commands);
        vm.OnWindowActivated();
        vm.PointerMoved(100.5, 200);
        vm.PointerButton(0, true);
        vm.Wheel(0, 0.5);
        vm.Wheel(0, 0.5);
        vm.Wheel(1, 0);
        Assert.Equal<SessionCommand>(
        [
            new SessionCommand.PointerMove(100.5, 200),
            new SessionCommand.PointerButton(0, true),
            new SessionCommand.PointerWheel(0, 60),
            new SessionCommand.PointerWheel(0, 60),
            new SessionCommand.PointerWheel(-120, 0),
        ], s.Commands);
    }

    [Fact]
    public void Choosing_a_mode_asks_the_host_but_the_hosts_choice_is_not_echoed()
    {
        var (vm, s) = Make();
        vm.IsGame = true;
        Assert.Equal(new SessionCommand.SetMode(StreamMode.Game), Assert.Single(s.Commands));
        vm.OnEvent(new StreamChangedEvent(4, 2, StreamMode.Office, 3024, 1964, 60));
        Assert.True(vm.IsOffice);
        Assert.Single(s.Commands);
        Assert.Equal("HEVC · 3024×1964 · 60 fps", vm.StreamText);
    }

    [Fact]
    public void The_session_ends_once_and_closes_its_handle()
    {
        var (vm, s) = Make();
        var ended = new List<DisconnectReason>();
        vm.Ended += (_, r, _) => ended.Add(r);
        vm.OnWindowActivated();
        vm.OnEvent(new DisconnectedEvent(4, DisconnectReason.Network, "lost"));
        vm.DisconnectCommand.Execute(null);
        vm.OnWindowClosing();
        Assert.Equal([DisconnectReason.Network], ended);
        Assert.Equal(1, s.CloseCount);
        Assert.False(vm.InputActive);
        vm.OnWindowActivated();
        Assert.Equal([true], s.InputActive);
    }

    [Fact]
    public void Clipboard_status_follows_notices_and_paste_waits()
    {
        var (vm, _) = Make(clipboard: false);
        Assert.Contains("未允许", vm.ClipboardText);
        vm.OnEvent(new PasteGateEvent(4, 35, false));
        Assert.Contains("35 ms", vm.ClipboardText);
        vm.OnEvent(new PasteGateEvent(4, 500, true));
        Assert.Contains("超时", vm.ClipboardText);
    }
}

public class DiagnosticsTests
{
    static StreamStats S(ulong complete, ulong lost, ulong presented, ulong rx, ulong rtt = 2500) =>
        new(complete, lost, 0, presented, 0, presented, rtt, 0, 0, 0, rx);

    [Fact]
    public void Rates_come_from_the_change_between_two_stats()
    {
        var d = new DiagnosticsViewModel("192.168.1.20:47800");
        Assert.Contains("局域网", d.PathText);
        d.Update(S(100, 0, 100, 1_000_000), Samples.T0);
        Assert.Equal("2.5 ms", d.RttText);
        Assert.Equal("—", d.FpsText);
        d.Update(S(158, 2, 160, 3_500_000), Samples.T0.AddSeconds(1));
        Assert.Equal("60 fps", d.FpsText);
        Assert.Equal("3.3 %", d.LossText);
        Assert.Equal("20.0 Mbps", d.BitrateText);
    }

    [Fact]
    public void A_restarted_pipeline_skips_one_interval()
    {
        var d = new DiagnosticsViewModel("8.8.8.8:1");
        d.Update(S(1000, 0, 1000, 9_000_000), Samples.T0);
        d.Update(S(10, 0, 10, 9_100_000), Samples.T0.AddSeconds(1));
        Assert.Equal("—", d.FpsText);
        d.Update(S(40, 0, 40, 9_150_000), Samples.T0.AddSeconds(2));
        Assert.Equal("30 fps", d.FpsText);
        Assert.Equal("400 kbps", d.BitrateText);
    }
}

public class ConsentViewModelTests
{
    readonly FakeLink _link = new();

    [Fact]
    public void Allow_is_disabled_until_the_dialog_has_been_seen_for_a_second()
    {
        var vm = new ConsentViewModel(Samples.Request(), _link, Samples.T0);
        Assert.False(vm.AllowCommand.CanExecute(null));
        vm.AllowCommand.Execute(null);
        Assert.Empty(_link.Sent);
        vm.Tick(Samples.T0.AddMilliseconds(900));
        Assert.False(vm.AllowCommand.CanExecute(null));
        vm.Tick(Samples.T0.AddSeconds(1));
        Assert.True(vm.AllowCommand.CanExecute(null));
    }

    [Fact]
    public void Allow_sends_exactly_what_was_ticked()
    {
        var finished = 0;
        var vm = new ConsentViewModel(Samples.Request(input: true), _link, Samples.T0);
        vm.Finished += _ => finished++;
        Assert.True(vm.AllowInput, "proposed permissions start ticked");
        Assert.False(vm.SaveDevice, "save_default false leaves it unticked");
        vm.AllowClipboard = true;
        vm.SaveDevice = true;
        vm.SkipNextTime = true; // not offered for a code exchange, so never sent
        vm.Tick(Samples.T0.AddSeconds(2));
        vm.AllowCommand.Execute(null);
        var d = Assert.IsType<UiRequest.Decide>(Assert.Single(_link.Sent));
        Assert.Equal(new UiRequest.Decide(41, true, new Permissions(true, true, true), true, false), d);
        Assert.Equal(1, finished);
        vm.DenyCommand.Execute(null);
        vm.OnWindowClosed();
        Assert.Single(_link.Sent);
    }

    [Fact]
    public void Input_cannot_be_granted_when_this_mac_cannot_inject()
    {
        var vm = new ConsentViewModel(Samples.Request(input: true, inputAvailable: false), _link, Samples.T0);
        Assert.False(vm.AllowInput);
        vm.AllowInput = true;
        vm.Tick(Samples.T0.AddSeconds(2));
        vm.AllowCommand.Execute(null);
        Assert.False(_link.Last<UiRequest.Decide>().Perms.Input);
    }

    [Fact]
    public void Deny_closing_and_the_countdown_all_deny()
    {
        var a = new ConsentViewModel(Samples.Request(1), _link, Samples.T0);
        a.DenyCommand.Execute(null);
        var b = new ConsentViewModel(Samples.Request(2), _link, Samples.T0);
        b.OnWindowClosed();
        var c = new ConsentViewModel(Samples.Request(3, timeoutMs: 5000), _link, Samples.T0);
        c.Tick(Samples.T0.AddSeconds(2));
        Assert.Equal("2 秒内不回应将自动拒绝", c.CountdownText); // closes a second before the agent would
        c.Tick(Samples.T0.AddSeconds(4));
        Assert.Equal([UiRequest.Decide.Deny(1), UiRequest.Decide.Deny(2), UiRequest.Decide.Deny(3)], _link.Sent);
        Assert.True(c.IsFinished);
        c.AllowCommand.Execute(null);
        Assert.Equal(3, _link.Sent.Count);
    }

    [Fact]
    public void A_request_closed_by_the_agent_sends_nothing()
    {
        var vm = new ConsentViewModel(Samples.Request(), _link, Samples.T0);
        vm.Expire("cancelled");
        vm.OnWindowClosed();
        Assert.Empty(_link.Sent);
        Assert.Equal("对方取消了请求", vm.Outcome);
    }
}

public class HostViewModelTests
{
    readonly FakeLink _link = new();
    readonly FakeProcess _process = new();
    readonly FakePermissions _perms = new();
    readonly RecordingWindows _windows = new();

    HostViewModel Vm() => new(_link, _process, _perms, _windows, new FakeAddresses(), "工作室 Mac mini", () => Samples.T0);

    [Fact]
    public async Task Without_screen_recording_it_shows_the_guide_and_starts_nothing()
    {
        _perms.State = new PermissionState(false, true);
        var vm = Vm();
        await vm.StartCommand.ExecuteAsync(null);
        Assert.Empty(_process.Starts);
        Assert.True(vm.IsStopped);
        Assert.Contains("屏幕录制", vm.Error);
        var guide = Assert.Single(_windows.Guides);
        Assert.False(guide.ScreenRecording);
        Assert.Equal("未授予", guide.ScreenRecordingStatus);
    }

    [Fact]
    public async Task An_agent_left_running_is_reused()
    {
        _link.ConnectResults.Enqueue(null);
        var vm = Vm();
        await vm.StartCommand.ExecuteAsync(null);
        Assert.Empty(_process.Starts);
        Assert.True(vm.IsHosting);
        Assert.IsType<UiRequest.ListPairings>(Assert.Single(_link.Sent));
    }

    [Fact]
    public async Task Starting_launches_the_agent_and_attaches_when_its_socket_is_up()
    {
        _link.ConnectResults.Enqueue("no socket");
        _link.ConnectResults.Enqueue("not yet");
        _link.ConnectResults.Enqueue(null);
        var vm = Vm();
        Assert.Equal("192.168.1.20:47800", vm.ListenAddress);
        await vm.StartCommand.ExecuteAsync(null);
        Assert.Equal(("192.168.1.20:47800", "工作室 Mac mini"), Assert.Single(_process.Starts));
        Assert.True(vm.IsHosting);

        _link.Raise(Samples.Status());
        Assert.Equal("K7QM-2XPA", vm.CodeDisplay);
        Assert.Equal("192.168.1.20:47800", vm.ActiveListen);
        vm.RotateCodeCommand.Execute(null);
        Assert.IsType<UiRequest.RotateCode>(_link.Sent.Last());
    }

    [Fact]
    public async Task An_agent_that_dies_on_start_is_explained()
    {
        var vm = Vm();
        var start = vm.StartCommand.ExecuteAsync(null);
        _process.Exit(1, "nexa-agent: Screen Recording permission is missing. Grant it ...");
        await start;
        Assert.True(vm.IsStopped);
        Assert.Contains("屏幕录制", vm.Error);
        Assert.Single(_windows.Guides);
    }

    async Task<HostViewModel> Hosting()
    {
        _link.ConnectResults.Enqueue(null);
        var vm = Vm();
        await vm.StartCommand.ExecuteAsync(null);
        _link.Raise(Samples.Status());
        return vm;
    }

    [Fact]
    public async Task A_request_opens_the_consent_dialog_and_a_closed_request_closes_it()
    {
        var vm = await Hosting();
        _link.Raise(Samples.Request(41));
        var consent = Assert.Single(_windows.Consents);
        Assert.Equal(41ul, consent.ReqId);
        Assert.Same(consent, vm.Consent);
        _link.Raise(new AgentMessage.RequestClosed(41, "timed out"));
        Assert.True(consent.IsFinished);
        Assert.Null(vm.Consent);
        Assert.DoesNotContain(_link.Sent, r => r is UiRequest.Decide);
    }

    [Fact]
    public async Task The_banner_follows_the_session_and_disconnects_in_one_click()
    {
        var vm = await Hosting();
        _link.Raise(new AgentMessage.SessionStarted(Samples.Session(7)));
        var banner = Assert.Single(_windows.Banners);
        Assert.Equal("「王小明的 MacBook Pro」正在控制本机", banner.Headline);
        Assert.Equal("权限：查看屏幕、键盘鼠标、剪贴板", banner.PermsText);
        // The periodic Status with the same session keeps the same banner.
        _link.Raise(Samples.Status(Samples.Session(7)));
        Assert.Single(_windows.Banners);

        banner.DisconnectCommand.Execute(null);
        Assert.IsType<UiRequest.Disconnect>(_link.Sent.Last());
        _link.Raise(new AgentMessage.SessionEnded(7, "王小明的 MacBook Pro", "ended by the host"));
        Assert.Null(_windows.Banner);
        Assert.Contains("已结束", vm.Notice);
    }

    [Fact]
    public async Task A_reattached_ui_shows_the_banner_from_status()
    {
        _link.ConnectResults.Enqueue(null);
        var vm = Vm();
        await vm.TryAttachExistingAsync();
        _link.Raise(Samples.Status(Samples.Session(3, input: false)));
        Assert.True(vm.IsHosting);
        Assert.Equal("「王小明的 MacBook Pro」正在查看本机屏幕", Assert.Single(_windows.Banners).Headline);
    }

    [Fact]
    public async Task Losing_the_agent_takes_down_the_banner_and_the_question()
    {
        var vm = await Hosting();
        _link.Raise(new AgentMessage.SessionStarted(Samples.Session()));
        _link.Raise(Samples.Request(9));
        _link.Drop();
        Assert.True(vm.IsStopped);
        Assert.Null(_windows.Banner);
        Assert.True(_windows.Consents[0].IsFinished);
        Assert.Contains("断开", vm.Error);
    }

    [Fact]
    public async Task Stop_shuts_the_agent_down()
    {
        var vm = await Hosting();
        vm.StopCommand.Execute(null);
        Assert.IsType<UiRequest.Shutdown>(_link.Sent.Last());
        Assert.Equal(1, _link.CloseCount);
        Assert.True(vm.IsStopped);
        Assert.Null(vm.Error);
    }

    [Fact]
    public async Task Paired_devices_can_be_revoked_or_made_to_ask_again()
    {
        var vm = await Hosting();
        _link.Raise(new AgentMessage.Pairings([
            new PairedDevice(Samples.ViewerKey, "id", "王小明的 MacBook Pro", new Permissions(true, true, false), 1, (ulong)Samples.T0.AddDays(-2).ToUnixTimeSeconds(), true),
        ]));
        var d = Assert.Single(vm.Paired);
        Assert.Equal("2 天前", d.LastSeen);
        Assert.Equal("连接时不再询问", d.ConsentText);
        d.RequireConsentCommand.Execute(null);
        Assert.Equal(new UiRequest.SetAccessPolicy(Samples.ViewerKey, false, new Permissions(true, true, false)), _link.Sent.Last());
        d.RevokeCommand.Execute(null);
        Assert.Equal(new UiRequest.RevokePairing(Samples.ViewerKey), _link.Sent.Last());
    }
}

public class AgentLinkTests
{
    static string SocketPath() =>
        Path.Combine(Path.GetTempPath(), $"nd-{Guid.NewGuid():N}"[..12] + ".sock");

    sealed class FakeAgent : IDisposable
    {
        readonly System.Net.Sockets.Socket _listener;
        public string Path { get; }
        public System.Net.Sockets.Socket? Client { get; private set; }

        public FakeAgent()
        {
            Path = SocketPath();
            _listener = new System.Net.Sockets.Socket(System.Net.Sockets.AddressFamily.Unix,
                System.Net.Sockets.SocketType.Stream, System.Net.Sockets.ProtocolType.Unspecified);
            _listener.Bind(new System.Net.Sockets.UnixDomainSocketEndPoint(Path));
            _listener.Listen(1);
        }

        public async Task<byte[]> AcceptAndReadFrame()
        {
            Client = await _listener.AcceptAsync();
            return await ReadFrame();
        }

        public async Task<byte[]> ReadFrame()
        {
            var d = new FrameDecoder(Ipc.MaxFrame);
            var buf = new byte[1024];
            while (true)
            {
                if (d.Next() is { } f) return f;
                var n = await Client!.ReceiveAsync(buf);
                if (n == 0) throw new IOException("closed");
                d.Push(buf.AsSpan(0, n));
            }
        }

        public void Send(string hexBody) => Client!.Send(Framing.Encode(Convert.FromHexString(hexBody.Replace(" ", ""))));

        public void Dispose()
        {
            Client?.Dispose();
            _listener.Dispose();
            File.Delete(Path);
        }
    }

    [Fact]
    public async Task Says_hello_first_and_attaches_on_status()
    {
        using var agent = new FakeAgent();
        var ui = new QueueDispatcher();
        var link = new AgentLink(agent.Path, ui);
        var got = new List<AgentMessage>();
        string? lost = "not yet";
        link.MessageReceived += got.Add;
        link.Disconnected += w => lost = w;

        var connecting = link.ConnectAsync(CancellationToken.None);
        var hello = await agent.AcceptAndReadFrame();
        Assert.Equal(new UiRequest.Hello(1, "NexaDesk").Encode(), hello);
        agent.Send("0a0c0801 1a084b37514d32585041"); // Status{version 1, code K7QM2XPA}
        Assert.Null(await connecting);
        Assert.True(link.IsConnected);

        link.Send(new UiRequest.RotateCode());
        Assert.Equal("3a00", Convert.ToHexString(await agent.ReadFrame()).ToLowerInvariant());
        agent.Send("32020801"); // RequestClosed{req_id 1}
        await WaitFor(() => ui.Pending >= 2);
        ui.RunAll();
        Assert.Equal("K7QM2XPA", Assert.IsType<AgentMessage.Status>(got[0]).Code);
        Assert.Equal(1ul, Assert.IsType<AgentMessage.RequestClosed>(got[1]).ReqId);

        agent.Client!.Shutdown(System.Net.Sockets.SocketShutdown.Both);
        await WaitFor(() => ui.Pending >= 1);
        ui.RunAll();
        Assert.False(link.IsConnected);
        Assert.NotEqual("not yet", lost);
    }

    [Fact]
    public async Task A_refusal_is_reported_and_nothing_attaches()
    {
        using var agent = new FakeAgent();
        var link = new AgentLink(agent.Path, new QueueDispatcher());
        var connecting = link.ConnectAsync(CancellationToken.None);
        await agent.AcceptAndReadFrame();
        // Refused{command "hello", reason "busy"}
        agent.Send("420d 0a0568656c6c6f 120462757379");
        Assert.Equal("busy", await connecting);
        Assert.False(link.IsConnected);
    }

    [Fact]
    public async Task No_socket_is_an_error_not_an_exception()
    {
        var link = new AgentLink(SocketPath(), new QueueDispatcher());
        Assert.NotNull(await link.ConnectAsync(CancellationToken.None));
        link.Send(new UiRequest.Disconnect()); // ignored while not connected
    }

    static async Task WaitFor(Func<bool> f)
    {
        for (var i = 0; i < 200 && !f(); i++) await Task.Delay(10);
        Assert.True(f());
    }
}
