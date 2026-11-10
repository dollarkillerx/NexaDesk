using Avalonia.Controls;
using Avalonia.Threading;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

public partial class ConsentDialog : Window
{
    readonly DispatcherTimer _timer = new() { Interval = TimeSpan.FromMilliseconds(250) };

    public ConsentDialog()
    {
        InitializeComponent();
        _timer.Tick += (_, _) => (DataContext as ConsentViewModel)?.Tick(DateTimeOffset.Now);
        Opened += (_, _) =>
        {
            (DataContext as ConsentViewModel)?.MarkShown(DateTimeOffset.Now);
            _timer.Start();
            DenyButton.Focus();
        };
        // However the window goes away, that is a denial unless Allow already answered.
        Closed += (_, _) =>
        {
            _timer.Stop();
            (DataContext as ConsentViewModel)?.OnWindowClosed();
        };
    }
}
