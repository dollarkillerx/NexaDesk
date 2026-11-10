using NexaDesk.Native;
using NexaDesk.Proto;
using Xunit;

namespace NexaDesk.Tests;

public class ProtoTests
{
    static string Hex(byte[] b) => Convert.ToHexString(b).ToLowerInvariant();
    static byte[] Bytes(string hex) => Convert.FromHexString(hex.Replace(" ", ""));

    // The same golden bytes as crates/nexa-agent/src/ipc/proto.rs, so both ends agree.
    [Fact]
    public void Ipc_encodings_match_the_rust_golden_bytes()
    {
        var decide = new UiRequest.Decide(300, true, new Permissions(true, false, true), true, false);
        Assert.Equal("420d08ac0210011a04080118012001", Hex(decide.Encode()));
        Assert.Equal("0a06080112027569", Hex(new UiRequest.Hello(1, "ui").Encode()));
    }

    [Fact]
    public void Incoming_request_decodes_from_the_rust_golden_bytes()
    {
        var bytes = Bytes("12290807 1203e78e8b 1a0164 22026162 2a09312e322e332e343a35 3002 3a020801 4001 5801 60e0d403 6801");
        var r = Assert.IsType<AgentMessage.IncomingRequest>(AgentMessage.Decode(bytes));
        Assert.Equal(7ul, r.ReqId);
        Assert.Equal("王", r.DeviceName);
        Assert.Equal("ab", r.DeviceKey);
        Assert.Equal("1.2.3.4:5", r.Remote);
        Assert.Equal(AuthKind.Code, r.Auth);
        Assert.Equal(new Permissions(true, false, false), r.Perms);
        Assert.True(r.OfferSave);
        Assert.False(r.SaveDefault);
        Assert.True(r.Takeover);
        Assert.Equal(60_000u, r.TimeoutMs);
        Assert.True(r.InputAvailable);
    }

    [Fact]
    public void A_default_decide_is_a_denial_on_the_wire()
    {
        // allow and every permission absent: the agent reads that as deny.
        Assert.Equal("4204 0801 1a00".Replace(" ", ""), Hex(UiRequest.Decide.Deny(1).Encode()));
    }

    [Fact]
    public void Empty_commands_are_still_present_on_the_wire()
    {
        Assert.Equal("3200", Hex(new UiRequest.Disconnect().Encode()));
        Assert.Equal("4a00", Hex(new UiRequest.Shutdown().Encode()));
        Assert.Equal("1200", Hex(new UiRequest.GetStatus().Encode()));
    }

    [Fact]
    public void Connect_request_carries_the_code_in_field_5_and_the_key_in_field_4()
    {
        var withCode = new ConnectRequest("10.0.0.2:47800", 3, StreamMode.Balanced, 0, new ConnectAuth.Code("ABCD2345"));
        Assert.Equal("0a0e31302e302e302e323a3437383030" + "1003" + "1801" + "2a084142434432333435", Hex(withCode.Encode()));
        var known = new ConnectRequest("h:1", 1, StreamMode.Office, 60, new ConnectAuth.KnownHost("ab"));
        Assert.Equal("0a03683a31" + "1001" + "383c" + "22026162", Hex(known.Encode()));
        Assert.DoesNotContain("ABCD", withCode.ToString());
    }

    [Fact]
    public void Wheel_uses_zigzag_and_move_uses_doubles()
    {
        Assert.Equal("1a0608ef0110f001", Hex(new SessionCommand.PointerWheel(-120, 120).Encode()));
        var move = new SessionCommand.PointerMove(1.5, 0).Encode();
        Assert.Equal("0a0909000000000000f83f", Hex(move));
        Assert.Equal("2a020802", Hex(new SessionCommand.SetMode(StreamMode.Game).Encode()));
    }

    [Fact]
    public void Events_decode_with_the_tag_wherever_it_is()
    {
        // Event{ connected{host_key:"k", host_name:"n", perms{view, input}}, tag: 9 }
        var bytes = Bytes("1a0c 0a016b 12016e 1a0408011001" + "a00109");
        var e = Assert.IsType<ConnectedEvent>(ClientCodec.DecodeEvent(bytes));
        Assert.Equal(9ul, e.Tag);
        Assert.Equal("k", e.HostKey);
        Assert.Equal("n", e.HostName);
        Assert.Equal(new Permissions(true, true, false), e.Perms);
    }

    [Fact]
    public void Stats_include_received_bytes()
    {
        var bytes = Bytes("420b 0864 1005 38e807 58c0843d");
        var s = Assert.IsType<StatsEvent>(ClientCodec.DecodeEvent(bytes)).Stats;
        Assert.Equal(100ul, s.Complete);
        Assert.Equal(5ul, s.Lost);
        Assert.Equal(1000ul, s.RttUs);
        Assert.Equal(1_000_000ul, s.RxBytes);
    }

    [Fact]
    public void Unknown_fields_and_bodies_are_skipped_and_bad_reasons_are_clamped()
    {
        // Disconnected{reason: 99, message: "x", field 7 fixed32} then an unknown field 30.
        var bytes = Bytes("4a0a 0863 120178 3d01020304" + "f00101");
        var d = Assert.IsType<DisconnectedEvent>(ClientCodec.DecodeEvent(bytes));
        Assert.Equal(DisconnectReason.Unspecified, d.Reason);
        Assert.Equal("x", d.Message);
        Assert.IsType<UnknownEvent>(ClientCodec.DecodeEvent(Bytes("f2010100")));
    }

    [Fact]
    public void Malformed_input_throws_a_proto_exception()
    {
        Assert.Throws<ProtoException>(() => ClientCodec.DecodeEvent(Bytes("1aff")));
        Assert.Throws<ProtoException>(() => ClientCodec.DecodeEvent(Bytes("ffffffffffffffffffffff01")));
        Assert.Throws<ProtoException>(() => AgentMessage.Decode(Bytes("0a05 0801")));
    }

    [Fact]
    public void Frames_split_anywhere_reassemble_and_oversize_is_refused()
    {
        var a = Framing.Encode(new byte[] { 1, 2, 3 });
        var b = Framing.Encode(new byte[300]);
        var wire = a.Concat(b).ToArray();
        var d = new FrameDecoder(1024);
        var frames = new List<byte[]>();
        foreach (var x in wire)
        {
            d.Push([x]);
            while (d.Next() is { } f) frames.Add(f);
        }
        Assert.Equal(2, frames.Count);
        Assert.Equal(new byte[] { 1, 2, 3 }, frames[0]);
        Assert.Equal(300, frames[1].Length);

        var big = new FrameDecoder(16);
        big.Push(Framing.Encode(new byte[17]));
        Assert.Throws<ProtoException>(() => big.Next());
    }

    [Fact]
    public void The_event_pump_copies_then_posts_and_drops_garbage()
    {
        var ui = new QueueDispatcher();
        var got = new List<ClientEvent>();
        var pump = new EventPump(ui, got.Add);
        var buffer = Bytes("5200 a00105"); // HostKeyPressed, tag 5
        pump.Receive(buffer);
        Array.Clear(buffer); // the library reuses its buffer after the callback returns
        pump.Receive(Bytes("1aff"));
        Assert.Empty(got);
        Assert.Equal(1, ui.Pending);
        ui.RunAll();
        Assert.Equal(new HostKeyEvent(5), Assert.Single(got));
        Assert.Equal(1, pump.Dropped);
    }
}
