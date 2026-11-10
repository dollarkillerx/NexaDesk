using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Platform.Storage;

namespace NexaDesk.Views;

public partial class TransferListView : UserControl
{
    public TransferListView() => InitializeComponent();
}

/// <summary>Local paths of files dropped from the Finder. Only files this Mac has; folders are left out later.</summary>
public static class DroppedFiles
{
    public static IReadOnlyList<string> Of(DragEventArgs e)
    {
        var items = e.DataTransfer.TryGetFiles() ?? [];
        return items.OfType<IStorageFile>().Select(f => f.TryGetLocalPath()).OfType<string>().ToList();
    }

    public static bool HasFiles(DragEventArgs e) => e.DataTransfer.Contains(DataFormat.File);
}

/// <summary>Drag-and-drop of Finder files onto a session window or its toolbar.</summary>
static class FileDrop
{
    public static void Over(Control target, DragEventArgs e)
    {
        var ok = target.DataContext is ViewModels.SessionViewModel { CanSendFiles: true, IsEnded: false } && DroppedFiles.HasFiles(e);
        e.DragEffects = ok ? DragDropEffects.Copy : DragDropEffects.None;
        e.Handled = true;
    }

    public static void Drop(Control target, DragEventArgs e)
    {
        e.Handled = true;
        if (target.DataContext is not ViewModels.SessionViewModel { CanSendFiles: true, IsEnded: false } vm) return;
        var paths = DroppedFiles.Of(e);
        if (paths.Count > 0) vm.Transfers.SendFiles(paths);
    }
}
