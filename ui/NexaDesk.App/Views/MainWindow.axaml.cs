using Avalonia.Controls;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

public partial class MainWindow : Window
{
    public MainWindow()
    {
        InitializeComponent();
        DataContextChanged += (_, _) =>
        {
            if (DataContext is MainViewModel vm)
                vm.Connect.FocusCodeRequested += () => CodeBox.Focus();
        };
    }
}
