using Avalonia.Controls;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

public partial class PermissionGuideWindow : Window
{
    public PermissionGuideWindow()
    {
        InitializeComponent();
        Activated += (_, _) => (DataContext as PermissionGuideViewModel)?.RecheckCommand.Execute(null);
    }

    protected override void OnDataContextChanged(EventArgs e)
    {
        base.OnDataContextChanged(e);
        if (DataContext is PermissionGuideViewModel vm) vm.CloseRequested += Close;
    }
}
