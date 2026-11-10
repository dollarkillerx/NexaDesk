using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
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

    [ObservableProperty] private string _pathText = "路径：—";
    [ObservableProperty] private string _fpsText = "— fps";
    [ObservableProperty] private string _note = "等待第一帧…";

    public static string Label(LatencySegment s) => s switch
    {
        LatencySegment.HostAndNetwork => "被控端 + 网络",
        LatencySegment.Receive => "收齐分片",
        LatencySegment.DecodeQueue => "等待解码",
        LatencySegment.Decode => "解码",
        LatencySegment.PresentQueue => "等待呈现",
        LatencySegment.Present => "呈现",
        _ => "总计",
    };

    static string Ms(uint us) => us >= 10_000 ? $"{us / 1000.0:F0}" : $"{us / 1000.0:F1}";

    public void Update(StreamStats s, DateTimeOffset now)
    {
        PathText = s.Path == PathKind.Unknown ? "路径：—" : $"路径：{Texts.Path(s.Path, s.PathDetail)}";
        var latency = s.Latency ?? [];
        Segments.Clear();
        foreach (var seg in latency.OrderBy(l => l.Segment == LatencySegment.Total ? 1 : 0).ThenBy(l => l.Segment))
            Segments.Add(new HudSegment(Label(seg.Segment), $"{Ms(seg.P50Us)} / {Ms(seg.P99Us)} ms", seg.Segment == LatencySegment.Total));
        Note = latency.Count == 0
            ? "等待第一帧…"
            : latency.Any(l => l.Segment == LatencySegment.HostAndNetwork)
                ? "中位数 / P99，最近约 240 帧"
                : "中位数 / P99；时钟尚未对齐，暂缺被控端段";
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
