using System.Collections.ObjectModel;
using Avalonia.Input;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;
using NexaDesk.Proto;
using NexaDesk.Services;

namespace NexaDesk.ViewModels;

/// <summary>A target system in the settings view's picker.</summary>
public sealed record KeyTarget(RemoteOs Os, string Label)
{
    public override string ToString() => Label;
}

/// <summary>One rule in the list: built-in rules can only be switched off, the person's own also removed.</summary>
public sealed partial class KeyRuleViewModel(KeyboardSettingsViewModel owner, KeymapRule rule) : ObservableObject
{
    public KeymapRule Rule { get; } = rule;

    [ObservableProperty] private bool _isEnabled = rule.Enabled;

    public string When => Rule.When;
    public string Then => string.Join(L.Get("Keyboard.ThenSeparator"), Rule.Then);
    public bool IsUser => !Rule.Builtin;
    public string Source => Rule.Builtin ? L.Get("Keyboard.Builtin") : L.Get("Keyboard.Custom");
    public string Notes
    {
        get
        {
            var parts = new List<string>();
            if (Rule.Ctx == "terminal") parts.Add(L.Get("Keyboard.TerminalOnly"));
            else if (Rule.Ctx == "ide") parts.Add(L.Get("Keyboard.IdeOnly"));
            if (Rule.Tag == "paste") parts.Add(L.Get("Keyboard.PasteNote"));
            if (!string.IsNullOrEmpty(Rule.Hold)) parts.Add(L.Format("Keyboard.HoldNote", Rule.Hold));
            return string.Join(" · ", parts);
        }
    }

    public KeymapRule ToRule() => Rule with { Enabled = IsEnabled };

    partial void OnIsEnabledChanged(bool value) => owner.OnRulesChanged();

    [RelayCommand]
    private void Remove() => owner.Remove(this);
}

/// <summary>
/// Keyboard settings (§11.2, DESIGN §8.3): the rules for this Mac controlling the
/// chosen system, each switchable, plus the person's own; saved to keymap.toml and
/// used by sessions started afterwards. The preview runs the real rewriter.
/// </summary>
public sealed partial class KeyboardSettingsViewModel : ObservableObject
{
    readonly IKeymapService _keymap;
    bool _loading;

    public IReadOnlyList<KeyTarget> Targets { get; } =
    [
        new(RemoteOs.Windows, L.Get("Keyboard.TargetWindows")),
        new(RemoteOs.Macos, L.Get("Keyboard.TargetMac")),
    ];

    public ObservableCollection<KeyRuleViewModel> Rules { get; } = new();

    [ObservableProperty] private KeyTarget _target;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(SaveCommand), nameof(RevertCommand))]
    private bool _isDirty;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasError))]
    private string? _error;

    [ObservableProperty] private string? _status;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(AddRuleCommand))]
    private string _newWhen = "";

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(AddRuleCommand))]
    private string _newThen = "";

    [ObservableProperty] private bool _newTerminalOnly;

    [ObservableProperty] private string _previewChord = "";
    [ObservableProperty] private bool _previewInTerminal;
    [ObservableProperty] private string _previewText = L.Get("Keyboard.PreviewPrompt");
    [ObservableProperty] private string _previewDetail = "";
    [ObservableProperty] private bool _previewIsError;

    public bool HasError => Error != null;

    public KeyboardSettingsViewModel(IKeymapService keymap, RemoteOs initial = RemoteOs.Windows)
    {
        _keymap = keymap;
        _target = Targets.FirstOrDefault(t => t.Os == initial) ?? Targets[0];
        Load();
    }

    partial void OnTargetChanged(KeyTarget value) => Load();

    partial void OnPreviewInTerminalChanged(bool value) => RefreshPreview();

    void Load()
    {
        _loading = true;
        Rules.Clear();
        var view = _keymap.Rules(Target.Os);
        foreach (var r in view.Rules) Rules.Add(new KeyRuleViewModel(this, r));
        _loading = false;
        var error = Messages.KeymapError(view);
        Error = string.IsNullOrEmpty(error) ? null : error;
        IsDirty = false;
        RefreshPreview();
    }

    internal void OnRulesChanged()
    {
        if (_loading) return;
        IsDirty = true;
        Status = null;
        RefreshPreview();
    }

    internal void Remove(KeyRuleViewModel rule)
    {
        if (!rule.IsUser) return;
        Rules.Remove(rule);
        OnRulesChanged();
    }

    IReadOnlyList<KeymapRule> Current() => Rules.Select(r => r.ToRule()).ToList();

    bool CanAdd() => NewWhen.Trim().Length > 0 && NewThen.Trim().Length > 0;

    /// <summary>Adds a rule after checking it with the real rule compiler (through a preview).</summary>
    [RelayCommand(CanExecute = nameof(CanAdd))]
    private void AddRule()
    {
        var then = NewThen.Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        var rule = new KeymapRule(false, NewTerminalOnly ? "terminal" : "", NewWhen.Trim(), then, "", "", true);
        var trial = Current().Append(rule).ToList();
        var check = _keymap.Preview(Target.Os, rule.When, NewTerminalOnly ? KeyContext.Terminal : KeyContext.Default, trial);
        if (check.Error.Length > 0)
        {
            Error = L.Format("Keyboard.RuleUnusable", check.Error);
            return;
        }
        Error = null;
        Rules.Add(new KeyRuleViewModel(this, rule));
        NewWhen = "";
        NewThen = "";
        NewTerminalOnly = false;
        OnRulesChanged();
        OnChord(rule.When);
    }

    [RelayCommand(CanExecute = nameof(IsDirty))]
    private void Save()
    {
        var error = _keymap.Save(Target.Os, Current());
        if (error != null)
        {
            Error = L.Format("Keyboard.NotSaved", error);
            return;
        }
        Error = null;
        IsDirty = false;
        Status = L.Get("Keyboard.Saved");
    }

    [RelayCommand(CanExecute = nameof(IsDirty))]
    private void Revert()
    {
        Status = null;
        Load();
    }

    /// <summary>A chord pressed in the preview box, in nexa-input's spelling ("Cmd+Shift+Z").</summary>
    public void OnChord(string chord)
    {
        PreviewChord = chord;
        RefreshPreview();
    }

    void RefreshPreview()
    {
        if (PreviewChord.Length == 0) return;
        var p = _keymap.Preview(Target.Os, PreviewChord, PreviewInTerminal ? KeyContext.Terminal : KeyContext.Default, Current());
        PreviewIsError = p.Error.Length > 0;
        if (PreviewIsError)
        {
            PreviewText = L.Format("Keyboard.PreviewError", PreviewChord, p.Error);
            PreviewDetail = "";
            return;
        }
        var sent = p.Sent.Count == 0 ? L.Get("Keyboard.SendsNothing") : string.Join(L.Get("Keyboard.ThenSeparator"), p.Sent);
        PreviewText = L.Format("Keyboard.PreviewSends", PreviewChord, sent);
        var keys = string.Join(" ", p.Events.Select(e => (e.Down ? "↓" : "↑") + e.Key));
        PreviewDetail = p.Paste ? L.Format("Keyboard.PreviewPaste", keys) : keys;
    }
}

/// <summary>
/// Turns key events from the preview box into chords. A modifier pressed and released
/// on its own is a tap ("Cmd"); otherwise the chord is reported when its key goes down.
/// </summary>
public sealed class ChordCapture
{
    bool _keyWhileHeld;

    static string? Modifier(PhysicalKey k) => k switch
    {
        PhysicalKey.MetaLeft or PhysicalKey.MetaRight => "Cmd",
        PhysicalKey.ControlLeft or PhysicalKey.ControlRight => "Ctrl",
        PhysicalKey.AltLeft or PhysicalKey.AltRight => "Option",
        PhysicalKey.ShiftLeft or PhysicalKey.ShiftRight => "Shift",
        _ => null,
    };

    /// <summary>nexa-input's name for a non-modifier key, or null if it has none.</summary>
    public static string? KeyName(PhysicalKey k)
    {
        if (k >= PhysicalKey.A && k <= PhysicalKey.Z) return ((char)('A' + (k - PhysicalKey.A))).ToString();
        if (k >= PhysicalKey.Digit0 && k <= PhysicalKey.Digit9) return ((char)('0' + (k - PhysicalKey.Digit0))).ToString();
        if (k >= PhysicalKey.F1 && k <= PhysicalKey.F24) return $"F{k - PhysicalKey.F1 + 1}";
        return k switch
        {
            PhysicalKey.Enter or PhysicalKey.NumPadEnter => "Enter",
            PhysicalKey.Escape => "Esc",
            PhysicalKey.Backspace => "Backspace",
            PhysicalKey.Tab => "Tab",
            PhysicalKey.Space => "Space",
            PhysicalKey.Minus => "Minus",
            PhysicalKey.Equal => "Equal",
            PhysicalKey.BracketLeft => "BracketLeft",
            PhysicalKey.BracketRight => "BracketRight",
            PhysicalKey.Backslash => "Backslash",
            PhysicalKey.Semicolon => "Semicolon",
            PhysicalKey.Quote => "Quote",
            PhysicalKey.Backquote => "Grave",
            PhysicalKey.Comma => "Comma",
            PhysicalKey.Period => "Period",
            PhysicalKey.Slash => "Slash",
            PhysicalKey.CapsLock => "CapsLock",
            PhysicalKey.Insert => "Insert",
            PhysicalKey.Delete => "Delete",
            PhysicalKey.Home => "Home",
            PhysicalKey.End => "End",
            PhysicalKey.PageUp => "PageUp",
            PhysicalKey.PageDown => "PageDown",
            PhysicalKey.ArrowLeft => "Left",
            PhysicalKey.ArrowRight => "Right",
            PhysicalKey.ArrowUp => "Up",
            PhysicalKey.ArrowDown => "Down",
            PhysicalKey.IntlBackslash => "NonUsBackslash",
            PhysicalKey.IntlRo => "Ro",
            PhysicalKey.IntlYen => "Yen",
            PhysicalKey.Convert => "Henkan",
            PhysicalKey.NonConvert => "Muhenkan",
            PhysicalKey.KanaMode or PhysicalKey.Lang1 => "Kana",
            PhysicalKey.Lang2 => "Eisu",
            _ => null,
        };
    }

    static string Mods(KeyModifiers m)
    {
        var parts = new List<string>();
        if (m.HasFlag(KeyModifiers.Control)) parts.Add("Ctrl");
        if (m.HasFlag(KeyModifiers.Alt)) parts.Add("Option");
        if (m.HasFlag(KeyModifiers.Shift)) parts.Add("Shift");
        if (m.HasFlag(KeyModifiers.Meta)) parts.Add("Cmd");
        return string.Join("+", parts);
    }

    public string? KeyDown(PhysicalKey key, KeyModifiers mods)
    {
        if (Modifier(key) != null)
        {
            if ((mods & ~ModifierOf(key)) == KeyModifiers.None) _keyWhileHeld = false;
            return null;
        }
        _keyWhileHeld = true;
        var name = KeyName(key);
        if (name == null) return null;
        var m = Mods(mods);
        return m.Length == 0 ? name : $"{m}+{name}";
    }

    public string? KeyUp(PhysicalKey key, KeyModifiers mods)
    {
        var modifier = Modifier(key);
        if (modifier == null || _keyWhileHeld) return null;
        // Released alone, with nothing else held: a tap.
        if ((mods & ~ModifierOf(key)) != KeyModifiers.None) return null;
        _keyWhileHeld = true;
        return modifier;
    }

    static KeyModifiers ModifierOf(PhysicalKey k) => Modifier(k) switch
    {
        "Cmd" => KeyModifiers.Meta,
        "Ctrl" => KeyModifiers.Control,
        "Option" => KeyModifiers.Alt,
        "Shift" => KeyModifiers.Shift,
        _ => KeyModifiers.None,
    };
}
