using System.Text;
using NexaDesk.Services;
using static NexaDesk.Native.NativeMethods;

namespace NexaDesk.Native;

/// <summary>Saved connections' passwords in the login Keychain, through nexa_saved_password_*.</summary>
public sealed unsafe class NativeSavedPasswords : ISavedPasswords
{
    public string? Save(string account, string password)
    {
        try
        {
            var a = Encoding.UTF8.GetBytes(account);
            var p = Encoding.UTF8.GetBytes(password);
            try
            {
                fixed (byte* ap = a)
                fixed (byte* pp = p)
                {
                    return nexa_saved_password_set(ap, (nuint)a.Length, pp, (nuint)p.Length) == NEXA_OK ? null : NativeError.Last();
                }
            }
            finally
            {
                Array.Clear(p);
            }
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
            return e.Message;
        }
    }

    public string? Load(string account)
    {
        try
        {
            var a = Encoding.UTF8.GetBytes(account);
            var buf = new byte[1024];
            nuint written = 0;
            int rc;
            fixed (byte* ap = a)
            fixed (byte* o = buf)
            {
                rc = nexa_saved_password_get(ap, (nuint)a.Length, o, (nuint)buf.Length, &written);
            }
            try
            {
                return rc == NEXA_OK && written > 0 ? Encoding.UTF8.GetString(buf, 0, (int)written) : null;
            }
            finally
            {
                Array.Clear(buf);
            }
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
            return null;
        }
    }

    public void Delete(string account)
    {
        try
        {
            var a = Encoding.UTF8.GetBytes(account);
            fixed (byte* ap = a) nexa_saved_password_delete(ap, (nuint)a.Length);
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
        {
        }
    }
}
