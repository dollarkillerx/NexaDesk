using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using NexaDesk.Localization;
using NexaDesk.Proto;

namespace NexaDesk.ViewModels;

/// <summary>One line of the HUD: a §6.1 segment's median and 99th percentile.</summary>
public sealed record HudSegment(string Label, string Value, bool IsTotal);

/// <summary>
/// The latency HUD (§10.3, §12): the segments the library measures, the path and
/// the presented frame rate, from the once-a-second Stats event.
/// </summary>
public sealed partial class HudViewModel : ObservableObject
{
    StreamStats? _last;
    DateTimeOffset _lastAt;

    public ObservableCollection<HudSegment> Segments { get; } = new();

    [ObservableProperty] private string _pathText = L.Get("Hud.PathNone");
    [ObservableProperty] private string _fpsText = "— fps";
    [ObservableProperty] private string _note = L.Get("Hud.WaitingFirstFrame");

    public static string Label(LatencySegment s) => s switch
    {
        LatencySegment.HostAndNetwork => L.Get("Hud.Segment.HostAndNetwork"),
        LatencySegment.Receive => L.Get("Hud.Segment.Receive"),
        LatencySegment.DecodeQueue => L.Get("Hud.Segment.DecodeQueue"),
        LatencySegment.Decode => L.Get("Hud.Segment.Decode"),
        LatencySegment.PresentQueue => L.Get("Hud.Segment.PresentQueue"),
        LatencySegment.Present => L.Get("Hud.Segment.Present"),
        _ => L.Get("Hud.Segment.Total"),
    };

    static string Ms(uint us) => us >= 10_000 ? $"{us / 1000.0:F0}" : $"{us / 1000.0:F1}";

    public void Update(StreamStats s, DateTimeOffset now)
    {
        PathText = s.Path == PathKind.Unknown ? L.Get("Hud.PathNone") : L.Format("Hud.Path", Texts.Path(s.Path, s.PathDetail));
        var latency = s.Latency ?? [];
        Segments.Clear();
        foreach (var seg in latency.OrderBy(l => l.Segment == LatencySegment.Total ? 1 : 0).ThenBy(l => l.Segment))
            Segments.Add(new HudSegment(Label(seg.Segment), $"{Ms(seg.P50Us)} / {Ms(seg.P99Us)} ms", seg.Segment == LatencySegment.Total));
        Note = latency.Count == 0
            ? L.Get("Hud.WaitingFirstFrame")
            : latency.Any(l => l.Segment == LatencySegment.HostAndNetwork)
                ? L.Get("Hud.NoteFull")
                : L.Get("Hud.NoteNoClock");
        var last = _last;
        var lastAt = _lastAt;
        _last = s;
        _lastAt = now;
        if (last == null) return;
        var dt = (now - lastAt).TotalSeconds;
        if (dt <= 0 || s.Presented < last.Presented) return;
        FpsText = $"{(s.Presented - last.Presented) / dt:F0} fps";
    }
}
