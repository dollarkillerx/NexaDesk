using Avalonia.Controls;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

public partial class ServerTrustDialog : Window
{
    public ServerTrustDialog()
    {
        InitializeComponent();
        Opened += (_, _) => DontTrustButton.Focus();
        Closed += (_, _) => (DataContext as ServerTrustViewModel)?.OnWindowClosed();
    }
}
