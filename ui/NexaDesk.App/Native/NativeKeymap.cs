using NexaDesk.Localization;
using NexaDesk.Proto;
using NexaDesk.Services;
using static NexaDesk.Native.NativeMethods;

namespace NexaDesk.Native;

/// <summary>The keyboard settings through nexa_keymap_*: no client handle, nothing sent anywhere.</summary>
public sealed unsafe class NativeKeymap(string dataDir) : IKeymapService
{
    static byte[] Call(delegate*<byte*, nuint, byte*, nuint, nuint*, int> f, byte[] request)
    {
        var cap = 4096;
        for (var attempt = 0; attempt < 3; attempt++)
        {
            var buf = new byte[cap];
            nuint written = 0;
            int rc;
            fixed (byte* r = request)
            fixed (byte* o = buf)
            {
                rc = f(r, (nuint)request.Length, o, (nuint)buf.Length, &written);
            }
            if (rc == NEXA_OK) return buf.AsSpan(0, (int)written).ToArray();
            if (rc != NEXA_E_BUFFER_TOO_SMALL || written > 16u << 20) throw new NexaException(rc, NativeError.Last());
            cap = (int)written;
        }
        throw new NexaException(NEXA_E_INTERNAL, "keymap answer kept growing");
    }

    KeymapRequest Request(RemoteOs remote) => new(dataDir, RemoteOs.Macos, remote);

    public KeymapView Rules(RemoteOs remote)
    {
        try
        {
            return KeymapView.Decode(Call(&nexa_keymap_rules, Request(remote).Encode()));
        }
        catch (Exception e) when (e is NexaException or ProtoException or DllNotFoundException or EntryPointNotFoundException)
        {
            return new KeymapView([], L.Format("Keymap.CannotRead", e.Message));
        }
    }

    public string? Save(RemoteOs remote, IReadOnlyList<KeymapRule> rules)
    {
        try
        {
            var bytes = (Request(remote) with { Rules = rules }).Encode();
            int rc;
            fixed (byte* p = bytes) rc = nexa_keymap_save(p, (nuint)bytes.Length);
            return rc == NEXA_OK ? null : NativeError.Last();
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
            return e.Message;
        }
    }

    public KeyPreview Preview(RemoteOs remote, string chord, KeyContext ctx, IReadOnlyList<KeymapRule> rules)
    {
        try
        {
            var req = Request(remote) with { Rules = rules, Edited = true, Chord = chord, Ctx = ctx };
            return KeyPreview.Decode(Call(&nexa_keymap_preview, req.Encode()));
        }
        catch (Exception e) when (e is NexaException or ProtoException or DllNotFoundException or EntryPointNotFoundException)
        {
            return new KeyPreview([], [], false, e.Message);
        }
    }
}
