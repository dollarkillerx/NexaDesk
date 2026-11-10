using Avalonia.Input;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>File transfer, the menu-bar item, login at start-up, keyboard settings and the HUD.</summary>
public class BackgroundAndFilesTests
{
    static string Hex(byte[] b) => Convert.ToHexString(b).ToLowerInvariant();
    static byte[] Bytes(string hex) => Convert.FromHexString(hex.Replace(" ", ""));

    // ----- wire formats, the same golden bytes as the Rust tests ----------------------

    [Fact]
    public void Transfer_messages_match_the_rust_golden_bytes()
    {
        // crates/nexa-client/src/proto.rs
        Assert.Equal("b201040a022f61", Hex(new SessionCommand.SendFiles(["/a"]).Encode()));
        Assert.Equal("c2010408031001", Hex(new SessionCommand.AnswerTransfer(3, true).Encode()));
        var t = Assert.IsType<TransferEvent>(ClientCodec.DecodeEvent(Bytes("d20213 0802 1a0161 20ac02 28ac02 3003 42042f642f61")));
        Assert.Equal(new TransferInfo(2, false, "a", 300, 300, TransferState.Done, TransferFail.None, "/d/a"), t.Info);
        var stats = Assert.IsType<StatsEvent>(ClientCodec.DecodeEvent(Bytes("420bc20208080310dc0b18a01f")));
        Assert.Equal([new SegmentLatency(LatencySegment.Decode, 1500, 4000)], stats.Stats.Latency!);
        // crates/nexa-agent/src/ipc/proto.rs
        Assert.Equal("52040a022f61", Hex(new UiRequest.SendFiles(["/a"]).Encode()));
        Assert.Equal("620408031001", Hex(new UiRequest.AnswerTransfer(3, true).Encode()));
        var ipc = Assert.IsType<AgentMessage.Transfer>(AgentMessage.Decode(Bytes("4a12 0801 1002 1801 220161 28ac02 3005 3804 4005")));
        Assert.Equal(1ul, ipc.SessionId);
        Assert.Equal(new TransferInfo(2, true, "a", 300, 5, TransferState.Failed, TransferFail.HashMismatch), ipc.Info);
        // An unknown state from a newer peer is not shown as success.
        var odd = Assert.IsType<TransferEvent>(ClientCodec.DecodeEvent(Bytes("d20204 0802 3063")));
        Assert.Equal(TransferState.Failed, odd.Info.State);
    }

    [Fact]
    public void Keymap_requests_encode_and_answers_decode()
    {
        var rule = new KeymapRule(false, "terminal", "Cmd+K", ["Ctrl+K"], "", "", true);
        var req = new KeymapRequest("/d", RemoteOs.Macos, RemoteOs.Windows, [rule], true, "Cmd+K", KeyContext.Terminal);
        Assert.Equal("0a022f64 1002 1801 221b 1208 7465726d696e616c 1a05 436d642b4b 2206 4374726c2b4b 3801 2801 3205 436d642b4b 3801".Replace(" ", ""),
            Hex(req.Encode()));
        var view = KeymapView.Decode(Bytes("0a07 0801 1a0141 3801 1202 6e6f"));
        Assert.Single(view.Rules);
        Assert.True(view.Rules[0].Builtin && view.Rules[0].Enabled);
        Assert.Equal("A", view.Rules[0].When);
        Assert.Equal("no", view.Error);
        var preview = KeyPreview.Decode(Bytes("0a05 0a0159 1001 1206 4374726c2b59 1801"));
        Assert.Equal([new KeyOut("Y", true)], preview.Events);
        Assert.Equal(["Ctrl+Y"], preview.Sent);
        Assert.True(preview.Paste);
    }

    // ----- files: the viewer ----------------------------------------------------------

    static SessionViewModel Viewer(FakeSession session, bool file)
    {
        var connected = new ConnectedEvent(1, Samples.HostKey, "工作室 Mac mini", new Permissions(true, true, true, File: file), 2, false);
        return new SessionViewModel(session, connected, "192.168.1.20:47800", () => Samples.T0);
    }

    [Fact]
    public void The_viewer_sends_only_its_own_files_and_follows_progress()
    {
        var session = new FakeSession(1);
        var vm = Viewer(session, file: true);
        Assert.True(vm.CanSendFiles);
        // A folder, a relative path and duplicates are left out; nothing is read here.
        var sent = vm.Transfers.SendFiles(["/Users/me/a.txt", "/Users/me/Folder", "b.txt", "/Users/me/a.txt"], p => p.EndsWith(".txt"));
        Assert.Equal(1, sent);
        var cmd = Assert.IsType<SessionCommand.SendFiles>(Assert.Single(session.Commands));
        Assert.Equal(["/Users/me/a.txt"], cmd.Paths);
        Assert.Equal(0, vm.Transfers.SendFiles(["/Users/me/Folder"], _ => false));
        Assert.NotNull(vm.Transfers.Error);

        vm.OnEvent(new TransferEvent(1, new TransferInfo(5, true, "a.txt", 2_000_000, 0, TransferState.Waiting)));
        vm.OnEvent(new TransferEvent(1, new TransferInfo(5, true, "a.txt", 2_000_000, 500_000, TransferState.Running)));
        var row = Assert.Single(vm.Transfers.Items);
        Assert.Equal(25, row.Progress);
        Assert.Equal("25%", row.StatusText);
        Assert.Equal("488.3 KB / 1.9 MB", row.ProgressText);
        row.CancelCommand.Execute(null);
        Assert.Equal(new SessionCommand.CancelTransfer(5), session.Commands[^1]);

        // A large file from the host waits for an answer here.
        vm.OnEvent(new TransferEvent(1, new TransferInfo(6, false, "big.iso", 900_000_000, 0, TransferState.NeedsConfirm)));
        var ask = vm.Transfers.Items[0];
        Assert.True(ask.NeedsAnswer);
        Assert.Contains("是否接收", ask.StatusText);
        ask.AcceptCommand.Execute(null);
        Assert.Equal(new SessionCommand.AnswerTransfer(6, true), session.Commands[^1]);
        vm.OnEvent(new TransferEvent(1, new TransferInfo(6, false, "big.iso", 900_000_000, 900_000_000, TransferState.Done,
            Path: "/Users/me/Downloads/NexaDesk/big (1).iso")));
        Assert.Equal("已保存到 下载/NexaDesk/big (1).iso", ask.StatusText);
        Assert.False(ask.AcceptCommand.CanExecute(null));
        vm.OnEvent(new TransferEvent(1, new TransferInfo(5, true, "a.txt", 2_000_000, 600_000, TransferState.Failed, TransferFail.HashMismatch)));
        Assert.Equal("校验失败，已丢弃", row.StatusText);
        Assert.True(row.IsProblem);
        vm.Transfers.ClearFinishedCommand.Execute(null);
        Assert.Empty(vm.Transfers.Items);
    }

    [Fact]
    public void Without_the_permission_the_viewer_offers_no_file_transfer()
    {
        var vm = Viewer(new FakeSession(1), file: false);
        Assert.False(vm.CanSendFiles);
        Assert.False(vm.Transfers.HasItems);
    }

    // ----- files: the host ------------------------------------------------------------

    static async Task<(HostViewModel host, FakeLink link)> Hosting(bool file)
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue(null);
        var host = new HostViewModel(link, new FakeProcess(), new FakePermissions(), new RecordingWindows(), new FakeAddresses(), "mac",
            () => Samples.T0);
        await host.StartCommand.ExecuteAsync(null);
        var session = Samples.Session() with { Perms = new Permissions(true, true, true, File: file) };
        link.Raise(new AgentMessage.SessionStarted(session));
        return (host, link);
    }

    [Fact]
    public async Task Files_dropped_on_the_banner_go_to_the_agent_and_transfers_show_there()
    {
        var (host, link) = await Hosting(file: true);
        var banner = host.Banner!;
        Assert.True(banner.CanSendFiles);
        Assert.Contains("拖到这里", banner.DropHint);
        banner.Transfers.SendFiles(["/Users/me/report.pdf"], _ => true);
        var send = Assert.IsType<UiRequest.SendFiles>(link.Sent[^1]);
        Assert.Equal(["/Users/me/report.pdf"], send.Paths);

        link.Raise(new AgentMessage.Transfer(7, new TransferInfo(9, true, "report.pdf", 100, 40, TransferState.Running)));
        link.Raise(new AgentMessage.Transfer(99, new TransferInfo(10, true, "other", 1, 0, TransferState.Running)));
        Assert.Single(banner.Transfers.Items);
        banner.Transfers.Items[0].CancelCommand.Execute(null);
        Assert.Equal(new UiRequest.CancelTransfer(9), link.Sent[^1]);
        link.Raise(new AgentMessage.Transfer(7, new TransferInfo(11, false, "big.mov", 1 << 30, 0, TransferState.NeedsConfirm)));
        banner.Transfers.Items[0].DeclineCommand.Execute(null);
        Assert.Equal(new UiRequest.AnswerTransfer(11, false), link.Sent[^1]);
        link.Raise(new AgentMessage.Refused("files", "no running session may transfer files"));
        Assert.Contains("无法发送", banner.Transfers.Error);
        Assert.Null(host.Error);
    }

    [Fact]
    public async Task Without_the_permission_the_banner_takes_no_files()
    {
        var (host, _) = await Hosting(file: false);
        Assert.False(host.Banner!.CanSendFiles);
        Assert.Equal("", host.Banner.DropHint);
    }

    [Fact]
    public void The_file_box_starts_unticked_even_when_proposed_and_grants_only_when_ticked()
    {
        var req = Samples.Request() with { Perms = new Permissions(true, true, true, File: true) };
        var link = new FakeLink();
        var vm = new ConsentViewModel(req, link, Samples.T0);
        Assert.False(vm.AllowFile);
        vm.Tick(Samples.T0.AddSeconds(2));
        vm.AllowCommand.Execute(null);
        Assert.False(Assert.IsType<UiRequest.Decide>(link.Sent[^1]).Perms.File);

        var link2 = new FakeLink();
        var vm2 = new ConsentViewModel(req, link2, Samples.T0) { AllowFile = true };
        vm2.Tick(Samples.T0.AddSeconds(2));
        vm2.AllowCommand.Execute(null);
        Assert.True(Assert.IsType<UiRequest.Decide>(link2.Sent[^1]).Perms.File);
    }

    // ----- menu bar and login item ----------------------------------------------------

    [Fact]
    public async Task The_menu_bar_item_reflects_hosting_and_controls_it()
    {
        var link = new FakeLink();
        var host = new HostViewModel(link, new FakeProcess(), new FakePermissions(), new RecordingWindows(), new FakeAddresses(), "mac",
            () => Samples.T0);
        var tray = new TrayViewModel(host);
        Assert.Equal("未允许别人控制本机", tray.StatusText);
        Assert.Equal("允许别人控制本机", tray.HostingLabel);
        Assert.False(tray.DisconnectCommand.CanExecute(null));

        link.ConnectResults.Enqueue(null);
        await tray.ToggleHostingCommand.ExecuteAsync(null);
        Assert.Equal(HostState.Hosting, host.State);
        Assert.Equal("停止允许控制", tray.HostingLabel);
        link.Raise(Samples.Status(shortId: "271828182", signal: SignalState.Online));
        Assert.Equal("允许控制中 · ID 271 828 182", tray.StatusText);

        link.Raise(new AgentMessage.SessionStarted(Samples.Session()));
        Assert.Contains("正在控制本机", tray.StatusText);
        Assert.True(tray.DisconnectCommand.CanExecute(null));
        tray.DisconnectCommand.Execute(null);
        Assert.IsType<UiRequest.Disconnect>(link.Sent[^1]);

        var shown = 0;
        var quit = 0;
        tray.ShowRequested += () => shown++;
        tray.QuitRequested += () => quit++;
        tray.ShowCommand.Execute(null);
        tray.QuitCommand.Execute(null);
        Assert.Equal((1, 1), (shown, quit));

        await tray.ToggleHostingCommand.ExecuteAsync(null);
        Assert.Equal(HostState.Stopped, host.State);
        Assert.IsType<UiRequest.Shutdown>(link.Sent[^1]);
    }

    [Fact]
    public async Task A_failed_start_from_the_menu_bar_brings_the_window_back()
    {
        var host = new HostViewModel(new FakeLink(), new FakeProcess(), new FakePermissions { State = new PermissionState(false, true) },
            new RecordingWindows(), new FakeAddresses(), "mac", () => Samples.T0);
        var tray = new TrayViewModel(host);
        var shown = 0;
        tray.ShowRequested += () => shown++;
        await tray.ToggleHostingCommand.ExecuteAsync(null);
        Assert.Equal(1, shown);
        Assert.True(host.HasError);
    }

    [Fact]
    public void The_login_item_toggle_registers_through_the_fake_only()
    {
        var item = new FakeLoginItem();
        var vm = new LoginItemViewModel(item);
        Assert.False(vm.IsEnabled);
        Assert.True(vm.IsSupported);
        vm.IsEnabled = true;
        Assert.Equal(1, item.Registers);
        Assert.Equal(LoginItemStatus.Enabled, vm.Status);
        Assert.Contains("自动打开", vm.StatusText);
        vm.IsEnabled = false;
        Assert.Equal(1, item.Unregisters);
        Assert.Equal(LoginItemStatus.NotRegistered, vm.Status);

        // macOS may want the person to approve it in System Settings.
        item.AfterRegister = LoginItemStatus.RequiresApproval;
        vm.IsEnabled = true;
        Assert.True(vm.NeedsApproval && vm.IsEnabled);
        vm.OpenSettingsCommand.Execute(null);
        Assert.Equal(1, item.SettingsOpened);

        // A failure says why and leaves the box as the system has it.
        var failing = new FakeLoginItem { Fail = "Operation not permitted" };
        var vm2 = new LoginItemViewModel(failing);
        vm2.IsEnabled = true;
        Assert.False(vm2.IsEnabled);
        Assert.Contains("Operation not permitted", vm2.Error);

        var unsupported = new LoginItemViewModel(new FakeLoginItem { Status = LoginItemStatus.Unsupported });
        Assert.False(unsupported.IsSupported);
        Assert.Contains("macOS 13", unsupported.StatusText);
    }

    [Fact]
    public void The_main_window_opens_keyboard_settings_and_defaults_to_no_login_item()
    {
        var windows = new RecordingWindows();
        var vm = new MainViewModel(new FakeClient(), new MemoryRecent(), new FakeLink(), new FakeProcess(), new FakePermissions(), windows,
            new FakeAddresses(), "mac", () => Samples.T0, keymap: new FakeKeymap());
        Assert.False(vm.LoginItem.IsSupported);
        Assert.True(vm.HasKeyboardSettings);
        vm.ShowKeyboardSettingsCommand.Execute(null);
        Assert.Single(windows.KeyboardSettings);
    }

    // ----- keyboard settings ----------------------------------------------------------

    [Fact]
    public void Keyboard_settings_list_toggle_add_preview_and_save()
    {
        var keymap = new FakeKeymap();
        var vm = new KeyboardSettingsViewModel(keymap);
        Assert.Equal(4, vm.Rules.Count);
        Assert.False(vm.IsDirty);
        Assert.Equal("仅终端", vm.Rules[2].Notes);
        Assert.Contains("粘贴", vm.Rules[1].Notes);

        vm.OnChord("Cmd+Shift+Z");
        Assert.Equal("按下 Cmd+Shift+Z → 发送 Ctrl+Y", vm.PreviewText);
        Assert.Equal("↓Ctrl ↓Y", vm.PreviewDetail);
        vm.Rules[0].IsEnabled = false;
        Assert.True(vm.IsDirty);
        Assert.Equal("按下 Cmd+Shift+Z → 发送 Ctrl+Shift+Z", vm.PreviewText);
        Assert.Equal(0, keymap.Saves);

        vm.NewWhen = "Cmd+Nope";
        vm.NewThen = "Ctrl+K";
        vm.AddRuleCommand.Execute(null);
        Assert.Contains("无法使用", vm.Error);
        Assert.Equal(4, vm.Rules.Count);
        vm.NewWhen = "Cmd+K";
        vm.NewThen = "Ctrl+K, Ctrl+C";
        vm.NewTerminalOnly = true;
        vm.AddRuleCommand.Execute(null);
        Assert.Null(vm.Error);
        var added = vm.Rules[^1];
        Assert.True(added.IsUser);
        Assert.Equal("Ctrl+K，再 Ctrl+C", added.Then);
        Assert.Equal("仅终端", added.Notes);
        vm.PreviewInTerminal = true;
        vm.OnChord("Cmd+K");
        Assert.Equal("按下 Cmd+K → 发送 Ctrl+K，再 Ctrl+C", vm.PreviewText);
        Assert.Contains(keymap.Previews, p => p.Item3 == KeyContext.Terminal && p.Item4 == 5);

        vm.SaveCommand.Execute(null);
        Assert.Equal(1, keymap.Saves);
        Assert.False(vm.IsDirty);
        Assert.Contains("已保存", vm.Status);
        Assert.False(keymap.Saved[RemoteOs.Windows][0].Enabled);
        Assert.Equal("terminal", keymap.Saved[RemoteOs.Windows][^1].Ctx);

        added.RemoveCommand.Execute(null);
        Assert.True(vm.IsDirty);
        vm.RevertCommand.Execute(null);
        Assert.Equal(5, vm.Rules.Count);
        vm.Rules[0].RemoveCommand.Execute(null);
        Assert.Equal(5, vm.Rules.Count);

        vm.Target = vm.Targets[1];
        Assert.Single(vm.Rules);
        Assert.False(vm.IsDirty);
    }

    [Fact]
    public void A_damaged_keymap_file_is_reported_and_a_bad_save_keeps_the_edits()
    {
        var keymap = new FakeKeymap { FileError = "keymap.toml 无法使用" };
        var vm = new KeyboardSettingsViewModel(keymap);
        Assert.Equal("keymap.toml 无法使用", vm.Error);
        keymap.Saved[RemoteOs.Windows].Add(new KeymapRule(false, "", "Cmd+Nope", ["Ctrl+K"], "", "", true));
        vm.RevertCommand.Execute(null);
        vm.Rules[0].IsEnabled = false;
        vm.SaveCommand.Execute(null);
        Assert.Contains("没有保存", vm.Error);
        Assert.True(vm.IsDirty);
        vm.OnChord("Cmd+Nope");
        Assert.True(vm.PreviewIsError);
    }

    [Fact]
    public void Pressed_keys_become_chords_in_nexa_input_spelling()
    {
        var c = new ChordCapture();
        Assert.Null(c.KeyDown(PhysicalKey.MetaLeft, KeyModifiers.Meta));
        Assert.Null(c.KeyDown(PhysicalKey.ShiftLeft, KeyModifiers.Meta | KeyModifiers.Shift));
        Assert.Equal("Shift+Cmd+Z", c.KeyDown(PhysicalKey.Z, KeyModifiers.Meta | KeyModifiers.Shift));
        Assert.Null(c.KeyUp(PhysicalKey.ShiftLeft, KeyModifiers.Meta));
        Assert.Null(c.KeyUp(PhysicalKey.MetaLeft, KeyModifiers.None));
        // A modifier on its own is a tap.
        Assert.Null(c.KeyDown(PhysicalKey.MetaRight, KeyModifiers.Meta));
        Assert.Equal("Cmd", c.KeyUp(PhysicalKey.MetaRight, KeyModifiers.None));
        Assert.Equal("Ctrl+Option+Left", c.KeyDown(PhysicalKey.ArrowLeft, KeyModifiers.Control | KeyModifiers.Alt));
        Assert.Equal("Q", ChordCapture.KeyName(PhysicalKey.Q));
        Assert.Equal("7", ChordCapture.KeyName(PhysicalKey.Digit7));
        Assert.Equal("F12", ChordCapture.KeyName(PhysicalKey.F12));
        Assert.Equal("Grave", ChordCapture.KeyName(PhysicalKey.Backquote));
        Assert.Equal("Yen", ChordCapture.KeyName(PhysicalKey.IntlYen));
        Assert.Null(ChordCapture.KeyName(PhysicalKey.MediaPlayPause));
    }

    // ----- HUD ------------------------------------------------------------------------

    [Fact]
    public void The_hud_shows_segments_path_and_frame_rate()
    {
        var vm = new SessionViewModel(new FakeSession(1), Samples.Connected(1), "192.168.1.20:47800", () => Samples.T0);
        Assert.False(vm.IsHudVisible);
        vm.ToggleHudCommand.Execute(null);
        Assert.True(vm.IsHudVisible);
        var t = Samples.T0;
        SegmentLatency[] segs =
        [
            new(LatencySegment.Total, 31_200, 48_000), new(LatencySegment.HostAndNetwork, 21_000, 30_100),
            new(LatencySegment.Decode, 2_400, 5_900),
        ];
        var hud = new HudViewModel();
        hud.Update(new StreamStats(100, 0, 0, 100, 0, 100, 3000, 0, 0, 0, 0, PathKind.Punched, "198.51.100.7:40211", 0, Latency: segs), t);
        hud.Update(new StreamStats(160, 0, 0, 160, 0, 160, 3000, 0, 0, 0, 0, PathKind.Punched, "198.51.100.7:40211", 0, Latency: segs),
            t.AddSeconds(1));
        Assert.Equal(["被控端 + 网络", "解码", "总计"], hud.Segments.Select(s => s.Label));
        Assert.Equal("21 / 30 ms", hud.Segments[0].Value);
        Assert.Equal("2.4 / 5.9 ms", hud.Segments[1].Value);
        Assert.True(hud.Segments[2].IsTotal);
        Assert.Equal("60 fps", hud.FpsText);
        Assert.StartsWith("路径：", hud.PathText);
        hud.Update(new StreamStats(1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, Latency: [new(LatencySegment.Decode, 1, 1)]), t.AddSeconds(2));
        Assert.Contains("时钟尚未对齐", hud.Note);
    }
}
