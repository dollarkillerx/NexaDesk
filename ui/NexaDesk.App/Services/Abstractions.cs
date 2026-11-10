using NexaDesk.Proto;

namespace NexaDesk.Services;

/// <summary>Runs work on the UI thread. Callbacks from Rust threads go through this (§10.1).</summary>
public interface IUiDispatcher
{
    void Post(Action action);
}

/// <summary>The viewer library (libnexa_client). Events arrive on the UI thread.</summary>
public interface IClientCore : IDisposable
{
    event Action<ClientEvent>? EventReceived;
    ISession Connect(ConnectRequest request);
}

/// <summary>One session handle. Every call returns at once; status codes are NativeMethods.NEXA_*.</summary>
public interface ISession
{
    ulong Tag { get; }
    bool IsClosed { get; }
    int AttachView(nint nsView, uint widthPx, uint heightPx, float scale);
    int ResizeView(uint widthPx, uint heightPx, float scale);
    int DetachView();
    int SetInputActive(bool active);
    int Send(SessionCommand command);
    string LastError { get; }
    void Close();
}

/// <summary>The IPC connection to the local agent. Events arrive on the UI thread.</summary>
public interface IAgentLink
{
    bool IsConnected { get; }
    event Action<AgentMessage>? MessageReceived;
    /// <summary>The connection ended; the argument says why, if known.</summary>
    event Action<string?>? Disconnected;
    /// <summary>Connects and says hello. Returns null once attached, or why it could not.</summary>
    Task<string?> ConnectAsync(CancellationToken ct);
    void Send(UiRequest request);
    void Close();
}

public sealed record AgentExit(int Code, string LastOutput);

/// <summary>The nexa-agent child process.</summary>
public interface IAgentProcess
{
    bool IsRunning { get; }
    /// <summary>Raised on the UI thread when a process this started exits.</summary>
    event Action<AgentExit>? Exited;
    /// <summary>Null on success, else why it could not start.</summary>
    string? Start(string listen, string deviceName);
    void Kill();
}

public sealed record PermissionState(bool ScreenRecording, bool Accessibility);

public enum PermissionKind { ScreenRecording, Accessibility }

/// <summary>macOS privacy permissions. Checking never prompts; only Request does.</summary>
public interface IPermissionProbe
{
    PermissionState Check();
    void Request(PermissionKind kind);
    void OpenSettings(PermissionKind kind);
}

public sealed record RecentDevice(string Address, string Name, string HostKey, long LastConnected);

public interface IRecentStore
{
    IReadOnlyList<RecentDevice> Load();
    void Save(IReadOnlyList<RecentDevice> devices);
}

public interface ILocalAddresses
{
    /// <summary>This machine's addresses worth listening on, best first.</summary>
    IReadOnlyList<string> Candidates();
}
