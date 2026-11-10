using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Proto;

namespace NexaDesk.ViewModels;

/// <summary>Where a transfer list sends what the person does: the session (viewer) or the agent (host).</summary>
public interface ITransferControl
{
    void Send(IReadOnlyList<string> paths);
    void Cancel(ulong id);
    void Answer(ulong id, bool accept);
}

public static partial class Texts
{
    /// <summary>Received files land here on both sides (§3.1 stream 4).</summary>
    public const string DownloadsFolder = "下载/NexaDesk";

    public static string Bytes(ulong n) => n switch
    {
        < 1024 => $"{n} B",
        < 1024 * 1024 => $"{n / 1024.0:F1} KB",
        < 1024UL * 1024 * 1024 => $"{n / 1024.0 / 1024:F1} MB",
        _ => $"{n / 1024.0 / 1024 / 1024:F2} GB",
    };

    /// <summary>Why a transfer failed, from this side's point of view.</summary>
    public static string TransferFailed(TransferFail reason, bool outgoing) => reason switch
    {
        TransferFail.Declined => outgoing ? "对方拒绝了" : "已拒绝",
        TransferFail.TooLarge => outgoing ? "文件超过对方允许的大小" : "文件超过本机允许的大小，已拒绝",
        TransferFail.NotPermitted => "这次会话没有文件传输权限",
        TransferFail.Invalid => "数据不一致，已丢弃",
        TransferFail.HashMismatch => "校验失败，已丢弃",
        TransferFail.NoSpace => outgoing ? "对方磁盘空间不足" : "本机磁盘空间不足",
        TransferFail.Io => "无法读取或写入文件",
        TransferFail.ConnectionLost => "连接中断",
        TransferFail.Timeout => "超时",
        TransferFail.NotAFile => "只能发送文件，不能发送文件夹",
        TransferFail.Busy => outgoing ? "对方正在接收太多文件，请稍后再试" : "同时接收的文件太多",
        _ => "失败",
    };
}

/// <summary>One row of the transfer list.</summary>
public sealed partial class TransferItemViewModel(TransferListViewModel owner, TransferInfo info) : ObservableObject
{
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(Name), nameof(Direction), nameof(Progress), nameof(ProgressText), nameof(StatusText),
        nameof(IsActive), nameof(NeedsAnswer), nameof(IsProblem), nameof(IsDone), nameof(ShowProgress))]
    [NotifyCanExecuteChangedFor(nameof(CancelCommand), nameof(AcceptCommand), nameof(DeclineCommand))]
    private TransferInfo _info = info;

    public ulong Id => Info.Id;
    public string Name => string.IsNullOrEmpty(Info.Name) ? "(无名文件)" : Info.Name;
    public string Direction => Info.Outgoing ? "发送 ↑" : "接收 ↓";
    public double Progress => Info.Size == 0 ? (Info.State == TransferState.Done ? 100 : 0) : 100.0 * Info.Done / Info.Size;
    public string ProgressText => Info.Size == 0 ? "" : $"{Texts.Bytes(Info.Done)} / {Texts.Bytes(Info.Size)}";
    public bool IsActive => !Info.IsFinal;
    public bool NeedsAnswer => Info.State == TransferState.NeedsConfirm;
    public bool IsProblem => Info.State is TransferState.Failed or TransferState.Cancelled;
    public bool IsDone => Info.State == TransferState.Done;
    public bool ShowProgress => Info.State is TransferState.Running or TransferState.Waiting;

    public string StatusText => Info.State switch
    {
        TransferState.Waiting => Info.Outgoing ? "等待对方接收…" : "准备中…",
        TransferState.NeedsConfirm => $"文件较大（{Texts.Bytes(Info.Size)}），是否接收？",
        TransferState.Running => $"{Progress:F0}%",
        TransferState.Done when Info.Outgoing => "已发送",
        TransferState.Done => $"已保存到 {Texts.DownloadsFolder}/{System.IO.Path.GetFileName(Info.Path)}",
        TransferState.Cancelled => Info.ByPeer ? "对方取消了" : "已取消",
        TransferState.Failed => Texts.TransferFailed(Info.Reason, Info.Outgoing),
        _ => "",
    };

    [RelayCommand(CanExecute = nameof(IsActive))]
    private void Cancel() => owner.Control.Cancel(Id);

    [RelayCommand(CanExecute = nameof(NeedsAnswer))]
    private void Accept() => owner.Control.Answer(Id, true);

    [RelayCommand(CanExecute = nameof(NeedsAnswer))]
    private void Decline() => owner.Control.Answer(Id, false);
}

/// <summary>
/// The transfers of one session, newest first. Files are only ever offered by the
/// side that has them: <see cref="SendFiles"/> takes local paths (a drop, or the
/// file picker); nothing here can ask for the other side's files.
/// </summary>
public sealed partial class TransferListViewModel : ObservableObject
{
    const int MaxRows = 50;
    const int MaxFilesPerDrop = 100;

    public ITransferControl Control { get; }
    public ObservableCollection<TransferItemViewModel> Items { get; } = new();

    public TransferListViewModel(ITransferControl control)
    {
        Control = control;
        Items.CollectionChanged += (_, _) =>
        {
            OnPropertyChanged(nameof(HasItems));
            OnPropertyChanged(nameof(Summary));
            ClearFinishedCommand.NotifyCanExecuteChanged();
        };
    }

    [ObservableProperty] private string? _error;

    public bool HasItems => Items.Count > 0;

    public string Summary
    {
        get
        {
            var active = Items.Count(i => i.IsActive);
            var asking = Items.Count(i => i.NeedsAnswer);
            if (asking > 0) return $"{asking} 个文件等待你确认";
            return active > 0 ? $"正在传输 {active} 个文件" : $"文件：{Items.Count} 项已结束";
        }
    }

    /// <summary>Local files to send. Folders and anything that is not a file are left out.</summary>
    public int SendFiles(IEnumerable<string> paths, Func<string, bool>? isFile = null)
    {
        isFile ??= File.Exists;
        var files = paths.Where(p => !string.IsNullOrWhiteSpace(p) && System.IO.Path.IsPathRooted(p) && isFile(p))
            .Distinct().ToList();
        Error = null;
        if (files.Count == 0)
        {
            Error = "只能发送文件（不支持文件夹）。";
            return 0;
        }
        if (files.Count > MaxFilesPerDrop)
        {
            Error = $"一次最多发送 {MaxFilesPerDrop} 个文件。";
            return 0;
        }
        Control.Send(files);
        return files.Count;
    }

    public void OnUpdate(TransferInfo info)
    {
        var existing = Items.FirstOrDefault(i => i.Id == info.Id);
        if (existing != null)
        {
            existing.Info = info;
        }
        else
        {
            Items.Insert(0, new TransferItemViewModel(this, info));
            while (Items.Count > MaxRows)
            {
                var old = Items.LastOrDefault(i => !i.IsActive);
                if (old == null) break;
                Items.Remove(old);
            }
        }
        OnPropertyChanged(nameof(Summary));
        ClearFinishedCommand.NotifyCanExecuteChanged();
    }

    bool HasFinished() => Items.Any(i => !i.IsActive);

    [RelayCommand(CanExecute = nameof(HasFinished))]
    private void ClearFinished()
    {
        foreach (var i in Items.Where(i => !i.IsActive).ToList()) Items.Remove(i);
    }
}
