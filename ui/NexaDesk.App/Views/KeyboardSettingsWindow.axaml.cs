using Avalonia.Controls;
using Avalonia.Input;
using NexaDesk.ViewModels;

namespace NexaDesk.Views;

/// <summary>
/// Keyboard settings. The preview box turns the keys pressed into a chord for the
/// view-model; nothing typed there leaves this Mac. Shortcuts macOS keeps for itself
/// (Cmd+Tab, Cmd+Space) never reach the box.
/// </summary>
public partial class KeyboardSettingsWindow : Window
{
    readonly ChordCapture _capture = new();

    public KeyboardSettingsWindow()
    {
        InitializeComponent();
        PreviewBox.PointerPressed += (_, _) => PreviewBox.Focus();
        PreviewBox.KeyDown += (_, e) =>
        {
            e.Handled = true;
            if (_capture.KeyDown(e.PhysicalKey, e.KeyModifiers) is { } chord) (DataContext as KeyboardSettingsViewModel)?.OnChord(chord);
        };
        PreviewBox.KeyUp += (_, e) =>
        {
            e.Handled = true;
            if (_capture.KeyUp(e.PhysicalKey, e.KeyModifiers) is { } chord) (DataContext as KeyboardSettingsViewModel)?.OnChord(chord);
        };
    }
}
