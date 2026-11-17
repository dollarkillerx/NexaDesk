namespace NexaDesk.Terminal;

// From MySSH.Core (the same author's SSH client), which this control comes from.

/// <summary>
/// A byte-stream terminal backend consumed by <see cref="TerminalView"/>: in NexaDesk, the host's
/// shell over a session's terminal stream (libnexa_client).
/// </summary>
public interface ITerminalBackend : IAsyncDisposable
{
    /// <summary>Raised on a background thread. The memory is only valid during the callback.</summary>
    event Action<ReadOnlyMemory<byte>>? DataReceived;

    /// <summary>Raised once when the remote shell ends or the connection is lost.</summary>
    event Action<TerminalExit>? Exited;

    ValueTask WriteAsync(ReadOnlyMemory<byte> data, CancellationToken ct = default);

    /// <summary>Window-change request. Safe to call from the UI thread; must not block.</summary>
    void Resize(int columns, int rows);
}

/// <param name="ExitCode">Remote exit status when known.</param>
/// <param name="ConnectionLost">True when the transport dropped, false for a normal shell exit.</param>
public sealed record TerminalExit(int? ExitCode, bool ConnectionLost, string? Reason = null);
