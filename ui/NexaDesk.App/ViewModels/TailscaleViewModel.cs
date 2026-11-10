using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Proto;

namespace NexaDesk.ViewModels;

/// <summary>
/// This Mac's Tailscale, shared by both halves of the main window (§4.7). Tailscale is the
/// recommended way across networks: it punches through NATs and relays when it cannot,
/// so no server of our own is needed. Until the core reports anything, nothing is shown.
/// </summary>
public sealed partial class TailscaleViewModel : ObservableObject
{
    public const string DownloadUrl = "https://tailscale.com/download";

    readonly IWindowService _windows;

    public TailscaleViewModel(IWindowService windows) => _windows = windows;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsKnown), nameof(IsPresent), nameof(IsAbsent), nameof(ConnectName), nameof(StatusText))]
    private TailscaleState? _state;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ConnectName), nameof(StatusText))]
    private string _dnsName = "";

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ConnectName), nameof(StatusText))]
    private IReadOnlyList<string> _ips = [];

    /// <summary>Raised when anything here changes, for views that derive from it.</summary>
    public event Action? Changed;

    public bool IsKnown => State != null;
    public bool IsPresent => State is TailscaleState.Running or TailscaleState.AddressOnly;
    public bool IsAbsent => State == TailscaleState.Absent;

    /// <summary>What others in the tailnet type to reach this Mac: the MagicDNS name, else the IPv4 address.</summary>
    public string ConnectName => DnsName.Length > 0
        ? DnsName
        : Ips.FirstOrDefault(ip => !ip.Contains(':')) ?? Ips.FirstOrDefault() ?? "";

    public string StatusText => State switch
    {
        TailscaleState.Running or TailscaleState.AddressOnly when ConnectName.Length > 0 =>
            $"Tailscale 已连接：{ConnectName}",
        TailscaleState.Running or TailscaleState.AddressOnly => "Tailscale 已连接",
        TailscaleState.Absent => "未检测到 Tailscale",
        _ => "",
    };

    /// <summary>Whether <paramref name="ip"/> is one of this Mac's tailnet addresses.</summary>
    public bool Owns(string ip) => Ips.Contains(ip);

    public void Update(TailnetDevicesEvent e)
    {
        State = e.State;
        DnsName = e.SelfDnsName;
        Ips = e.Ips;
        Changed?.Invoke();
    }

    [RelayCommand]
    private void Download() => _windows.OpenUrl(DownloadUrl);
}
