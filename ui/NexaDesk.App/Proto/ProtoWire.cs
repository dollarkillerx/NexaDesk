using System.Buffers.Binary;
using System.Text;

namespace NexaDesk.Proto;

/// <summary>Malformed protobuf or framing from the other side.</summary>
public sealed class ProtoException(string message) : Exception(message);

/// <summary>
/// A minimal proto3 writer: enough for the messages crossing the C ABI and the agent IPC.
/// Hand-written so NativeAOT needs no reflection and the build needs no protoc. Default
/// values are omitted, as proto3 does.
/// </summary>
public sealed class ProtoWriter
{
    readonly List<byte> _buf = new(64);

    public byte[] ToArray() => _buf.ToArray();

    void Tag(int field, int wire) => RawVarint((ulong)((field << 3) | wire));

    void RawVarint(ulong v)
    {
        while (v >= 0x80)
        {
            _buf.Add((byte)(v | 0x80));
            v >>= 7;
        }
        _buf.Add((byte)v);
    }

    public ProtoWriter UInt64(int field, ulong v)
    {
        if (v != 0) { Tag(field, 0); RawVarint(v); }
        return this;
    }

    public ProtoWriter UInt32(int field, uint v) => UInt64(field, v);

    /// <summary>int32 and enums: negative values are sign-extended to ten bytes.</summary>
    public ProtoWriter Int32(int field, int v)
    {
        if (v != 0) { Tag(field, 0); RawVarint((ulong)(long)v); }
        return this;
    }

    public ProtoWriter SInt32(int field, int v)
    {
        if (v != 0) { Tag(field, 0); RawVarint((uint)((v << 1) ^ (v >> 31))); }
        return this;
    }

    public ProtoWriter Bool(int field, bool v)
    {
        if (v) { Tag(field, 0); RawVarint(1); }
        return this;
    }

    public ProtoWriter Double(int field, double v)
    {
        if (BitConverter.DoubleToInt64Bits(v) != 0)
        {
            Tag(field, 1);
            Span<byte> b = stackalloc byte[8];
            BinaryPrimitives.WriteDoubleLittleEndian(b, v);
            foreach (var x in b) _buf.Add(x);
        }
        return this;
    }

    public ProtoWriter String(int field, string? v)
    {
        if (string.IsNullOrEmpty(v)) return this;
        return Bytes(field, Encoding.UTF8.GetBytes(v));
    }

    public ProtoWriter Bytes(int field, ReadOnlySpan<byte> v)
    {
        Tag(field, 2);
        RawVarint((ulong)v.Length);
        foreach (var x in v) _buf.Add(x);
        return this;
    }

    /// <summary>A sub-message. Always written, even empty: presence matters for oneofs.</summary>
    public ProtoWriter Message(int field, Action<ProtoWriter> body)
    {
        var inner = new ProtoWriter();
        body(inner);
        return Bytes(field, inner._buf.ToArray());
    }
}

/// <summary>A proto3 reader over one message. Unknown fields are skipped.</summary>
public ref struct ProtoReader(ReadOnlySpan<byte> data)
{
    readonly ReadOnlySpan<byte> _data = data;
    int _pos = 0;
    int _wire = -1;

    public bool Next(out int field)
    {
        if (_pos >= _data.Length) { field = 0; return false; }
        var key = ReadVarint();
        field = (int)(key >> 3);
        _wire = (int)(key & 7);
        if (field <= 0) throw new ProtoException("bad field number");
        return true;
    }

    public ulong ReadVarint()
    {
        ulong v = 0;
        for (var shift = 0; shift < 70; shift += 7)
        {
            if (_pos >= _data.Length) throw new ProtoException("truncated varint");
            var b = _data[_pos++];
            v |= (ulong)(b & 0x7F) << shift;
            if ((b & 0x80) == 0) return v;
        }
        throw new ProtoException("varint too long");
    }

    void Expect(int wire)
    {
        if (_wire != wire) throw new ProtoException($"wire type {_wire}, expected {wire}");
    }

    public ulong UInt64() { Expect(0); return ReadVarint(); }
    public uint UInt32() => (uint)UInt64();
    public int Int32() => (int)UInt64();
    public bool Bool() => UInt64() != 0;

    public int SInt32()
    {
        var n = (uint)UInt64();
        return (int)(n >> 1) ^ -(int)(n & 1);
    }

    public double Double()
    {
        Expect(1);
        if (_pos + 8 > _data.Length) throw new ProtoException("truncated double");
        var v = BinaryPrimitives.ReadDoubleLittleEndian(_data.Slice(_pos, 8));
        _pos += 8;
        return v;
    }

    public ReadOnlySpan<byte> Bytes()
    {
        Expect(2);
        var len = ReadVarint();
        if (len > (ulong)(_data.Length - _pos)) throw new ProtoException("truncated field");
        var s = _data.Slice(_pos, (int)len);
        _pos += (int)len;
        return s;
    }

    public string String() => Encoding.UTF8.GetString(Bytes());

    public void Skip()
    {
        switch (_wire)
        {
            case 0: ReadVarint(); break;
            case 1: Advance(8); break;
            case 2: Bytes(); break;
            case 5: Advance(4); break;
            default: throw new ProtoException($"unsupported wire type {_wire}");
        }
    }

    void Advance(int n)
    {
        if (_pos + n > _data.Length) throw new ProtoException("truncated field");
        _pos += n;
    }
}

/// <summary>Reliable-stream framing (§3.1) without the stream-type byte: varint length + body.</summary>
public static class Framing
{
    public static byte[] Encode(ReadOnlySpan<byte> body)
    {
        var out_ = new List<byte>(body.Length + 5);
        var v = (ulong)body.Length;
        while (v >= 0x80) { out_.Add((byte)(v | 0x80)); v >>= 7; }
        out_.Add((byte)v);
        foreach (var b in body) out_.Add(b);
        return out_.ToArray();
    }
}

/// <summary>Incremental frame decoder: push what the socket delivered, then drain frames.</summary>
public sealed class FrameDecoder(int maxFrame)
{
    byte[] _buf = new byte[4096];
    int _len;

    public void Push(ReadOnlySpan<byte> data)
    {
        if (_len + data.Length > _buf.Length)
            Array.Resize(ref _buf, Math.Max(_buf.Length * 2, _len + data.Length));
        data.CopyTo(_buf.AsSpan(_len));
        _len += data.Length;
    }

    /// <summary>The next frame, or null if more bytes are needed. Throws on a bad frame.</summary>
    public byte[]? Next()
    {
        ulong len = 0;
        var used = 0;
        while (true)
        {
            if (used >= _len) return null;
            if (used == 9) throw new ProtoException("frame length too long");
            var b = _buf[used];
            len |= (ulong)(b & 0x7F) << (7 * used);
            used++;
            if ((b & 0x80) == 0) break;
        }
        if (len > (ulong)maxFrame) throw new ProtoException($"frame of {len} bytes is too large");
        if (_len - used < (int)len) return null;
        var frame = _buf.AsSpan(used, (int)len).ToArray();
        var rest = _len - used - (int)len;
        Array.Copy(_buf, used + (int)len, _buf, 0, rest);
        _len = rest;
        return frame;
    }
}
