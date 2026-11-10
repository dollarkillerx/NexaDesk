using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
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
    public static string DownloadsFolder => L.Get("Transfer.DownloadsFolder");

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
        TransferFail.Declined => outgoing ? L.Get("Transfer.FailDeclinedByPeer") : L.Get("Transfer.FailDeclined"),
        TransferFail.TooLarge => outgoing ? L.Get("Transfer.FailTooLargeForPeer") : L.Get("Transfer.FailTooLarge"),
        TransferFail.NotPermitted => L.Get("Transfer.FailNotPermitted"),
        TransferFail.Invalid => L.Get("Transfer.FailInvalid"),
        TransferFail.HashMismatch => L.Get("Transfer.FailHashMismatch"),
        TransferFail.NoSpace => outgoing ? L.Get("Transfer.FailNoSpacePeer") : L.Get("Transfer.FailNoSpace"),
        TransferFail.Io => L.Get("Transfer.FailIo"),
        TransferFail.ConnectionLost => L.Get("Transfer.FailConnectionLost"),
        TransferFail.Timeout => L.Get("Transfer.FailTimeout"),
        TransferFail.NotAFile => L.Get("Transfer.FailNotAFile"),
        TransferFail.Busy => outgoing ? L.Get("Transfer.FailBusyPeer") : L.Get("Transfer.FailBusy"),
        _ => L.Get("Transfer.Failed"),
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
    public string Name => string.IsNullOrEmpty(Info.Name) ? L.Get("Transfer.Unnamed") : Info.Name;
    public string Direction => Info.Outgoing ? L.Get("Transfer.Outgoing") : L.Get("Transfer.Incoming");
    public double Progress => Info.Size == 0 ? (Info.State == TransferState.Done ? 100 : 0) : 100.0 * Info.Done / Info.Size;
    public string ProgressText => Info.Size == 0 ? "" : $"{Texts.Bytes(Info.Done)} / {Texts.Bytes(Info.Size)}";
    public bool IsActive => !Info.IsFinal;
    public bool NeedsAnswer => Info.State == TransferState.NeedsConfirm;
    public bool IsProblem => Info.State is TransferState.Failed or TransferState.Cancelled;
    public bool IsDone => Info.State == TransferState.Done;
    public bool ShowProgress => Info.State is TransferState.Running or TransferState.Waiting;

    public string StatusText => Info.State switch
    {
        TransferState.Waiting => Info.Outgoing ? L.Get("Transfer.WaitingForPeer") : L.Get("Transfer.Preparing"),
        TransferState.NeedsConfirm => L.Format("Transfer.ConfirmLarge", Texts.Bytes(Info.Size)),
        TransferState.Running => $"{Progress:F0}%",
        TransferState.Done when Info.Outgoing => L.Get("Transfer.Sent"),
        TransferState.Done => L.Format("Transfer.SavedTo", Texts.DownloadsFolder, System.IO.Path.GetFileName(Info.Path)),
        TransferState.Cancelled => Info.ByPeer ? L.Get("Transfer.CancelledByPeer") : L.Get("Transfer.Cancelled"),
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
            if (asking > 0) return L.Format("Transfer.SummaryAsking", asking);
            return active > 0 ? L.Format("Transfer.SummaryActive", active) : L.Format("Transfer.SummaryFinished", Items.Count);
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
            Error = L.Get("Transfer.OnlyFiles");
            return 0;
        }
        if (files.Count > MaxFilesPerDrop)
        {
            Error = L.Format("Transfer.TooMany", MaxFilesPerDrop);
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
