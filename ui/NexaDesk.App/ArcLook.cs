using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.Themes.Fluent;
using TKCode.Arc;

namespace NexaDesk;

/// <summary>
/// The colours of the whole app, from TKCode.Arc's palette (https://github.com/AIDotNet/TKCode.Arc):
/// Arc's neutrals and one of its eight accents, light and dark. Our own brushes (PageBrush,
/// CardBrush...) and the Fluent keys its controls read (ButtonBackground, TextControlBorderBrush...)
/// all come from here, one dictionary per theme variant, so a window that asks for dark (the
/// toolbar, the banner) gets Arc's dark values whatever the app's own theme is. The shapes (radii,
/// heights, shadows, type) are in App.axaml.
/// </summary>
static class ArcLook
{
    /// <summary>NexaDesk's accent. Any of Arc's presets works; everything follows it.</summary>
    internal const ArcAccentPreset Accent = ArcAccentPreset.Blue;

    internal static void Apply(Application app)
    {
        app.Resources.ThemeDictionaries[ThemeVariant.Light] = Dictionary(dark: false);
        app.Resources.ThemeDictionaries[ThemeVariant.Dark] = Dictionary(dark: true);
        // Line icons on a 24×24 grid, Lucide as Arc draws them (Path.icon in App.axaml).
        app.Resources["IconMonitor"] = ArcIcons.Monitor;
        app.Resources["IconConnect"] = ArcIcons.ArrowUpRight;
        app.Resources["IconGlobe"] = ArcIcons.Globe;
        app.Resources["IconScreens"] = ArcIcons.Laptop;
        app.Resources["IconCopy"] = ArcIcons.Copy;
        app.Resources["IconCheck"] = ArcIcons.Check;
        app.Resources["IconSliders"] = ArcIcons.Settings;
        // Progress bars, focus rings and selection take the accent from Fluent's palette.
        foreach (var fluent in app.Styles.OfType<FluentTheme>())
        {
            foreach (var (variant, dark) in new[] { (ThemeVariant.Light, false), (ThemeVariant.Dark, true) })
            {
                if (fluent.Palettes.TryGetValue(variant, out var palette))
                    palette.Accent = AccentColors(dark).Accent;
            }
        }
    }

    static ArcAccentColors AccentColors(bool dark) => ArcPalette.Resolve(ArcPalette.GetSeed(Accent), dark);

    static ResourceDictionary Dictionary(bool dark)
    {
        var n = ArcPalette.Neutrals(dark);
        var a = AccentColors(dark);
        var d = new ResourceDictionary();
        void B(string key, Color c) => d[key] = new SolidColorBrush(c);
        void Bs(Color c, params string[] keys) { foreach (var k in keys) B(k, c); }

        // A fill under the pointer and under a press, on a surface.
        var hover = Mix(n.Surface, n.Foreground, dark ? 0.065 : 0.045);
        var pressed = Mix(n.Surface, n.Foreground, dark ? 0.11 : 0.085);
        // Secondary buttons: a quiet grey fill.
        var secondary = dark ? n.SurfaceMuted : n.BorderSubtle;
        var secondaryHover = dark ? Mix(n.SurfaceMuted, n.Foreground, 0.06) : n.Border;
        var secondaryPressed = dark ? Mix(n.SurfaceMuted, n.Foreground, 0.11) : n.ControlTrack;
        var disabledBack = dark ? n.SurfaceRaised : Mix(n.Surface, n.Foreground, 0.055);
        var disabledText = WithAlpha(n.Foreground, dark ? 0.30 : 0.28);
        var accentHover = dark ? Mix(a.Accent, Colors.White, 0.16) : Mix(a.Accent, Colors.Black, 0.12);
        var accentPressed = dark ? Mix(a.Accent, Colors.White, 0.30) : Mix(a.Accent, Colors.Black, 0.24);
        var field = dark ? n.SurfaceRaised : n.Surface;
        var fieldBorder = dark ? Mix(n.Border, n.BorderStrong, 0.35) : Mix(n.Border, n.BorderStrong, 0.3);
        // A filled danger button keeps light's red in both themes, under white text.
        var danger = ArcNeutralColors.Light.Danger;

        // Ours.
        B("PageBrush", dark ? n.Background : n.SurfaceMuted);
        B("CardBrush", n.Surface);
        B("CardBorderBrush", n.Border);
        B("StrongBorderBrush", n.BorderStrong);
        B("SubtleBrush", n.SurfaceMuted);
        B("SubtleHoverBrush", hover);
        B("SubtlePressedBrush", pressed);
        B("TextBrush", n.Foreground);
        B("MutedBrush", n.TextSecondary);
        B("FaintBrush", n.TextMuted);
        B("InverseBrush", n.Foreground);
        B("InverseTextBrush", n.Background);
        B("AccentBrush", a.Accent);
        B("AccentStrongBrush", a.Accent);
        B("AccentHoverBrush", accentHover);
        B("AccentTextBrush", a.AccentForeground);
        B("AccentSubtleBrush", a.AccentSubtle);
        B("BadgeBrush", a.AccentSubtle);
        B("BadgeTextBrush", a.AccentStrong);
        B("ErrorBrush", n.Danger);
        B("ErrorBackBrush", WithAlpha(n.Danger, dark ? 0.13 : 0.07));
        B("ErrorEdgeBrush", WithAlpha(n.Danger, dark ? 0.38 : 0.26));
        B("DangerBrush", danger);
        B("DangerHoverBrush", Mix(danger, Colors.Black, 0.14));
        B("DangerPressedBrush", Mix(danger, Colors.Black, 0.28));
        B("DangerTextBrush", Colors.White);
        B("WarnBrush", n.Warning);
        B("WarnStripeBrush", n.Warning);
        B("WarnBackBrush", WithAlpha(n.Warning, dark ? 0.13 : 0.09));
        B("WarnEdgeBrush", WithAlpha(n.Warning, dark ? 0.40 : 0.32));
        B("OkBrush", n.Success);
        B("OkStripeBrush", n.Success);
        B("OkBackBrush", WithAlpha(n.Success, dark ? 0.13 : 0.07));
        B("OkEdgeBrush", WithAlpha(n.Success, dark ? 0.38 : 0.26));
        B("InfoStripeBrush", a.Accent);
        B("InfoBackBrush", WithAlpha(ArcPalette.GetSeed(Accent), dark ? 0.15 : 0.06));
        B("InfoEdgeBrush", WithAlpha(ArcPalette.GetSeed(Accent), dark ? 0.42 : 0.22));
        // The session's own windows are always dark.
        B("BannerBrush", ArcNeutralColors.Dark.Background);
        B("ToolbarBrush", ArcNeutralColors.Dark.Background);
        B("FieldBrush", field);
        B("FieldBorderBrush", fieldBorder);
        B("SegmentTrackBrush", dark ? n.SurfaceMuted : n.BorderSubtle);
        B("SegmentOnBrush", dark ? Mix(n.SurfaceMuted, n.Foreground, 0.14) : n.Surface);
        B("NavSelectedBrush", dark ? n.SurfaceMuted : n.Surface);
        B("NavSelectedBorderBrush", n.Border);
        B("ScrimBrush", n.Scrim);

        // Fluent: buttons (secondary by default; "accent" is primary).
        B("ButtonBackground", secondary);
        B("ButtonBackgroundPointerOver", secondaryHover);
        B("ButtonBackgroundPressed", secondaryPressed);
        B("ButtonBackgroundDisabled", disabledBack);
        Bs(n.Foreground, "ButtonForeground", "ButtonForegroundPointerOver", "ButtonForegroundPressed");
        B("ButtonForegroundDisabled", disabledText);
        Bs(Colors.Transparent, "ButtonBorderBrush", "ButtonBorderBrushPointerOver", "ButtonBorderBrushPressed", "ButtonBorderBrushDisabled");
        B("AccentButtonBackground", a.Accent);
        B("AccentButtonBackgroundPointerOver", accentHover);
        B("AccentButtonBackgroundPressed", accentPressed);
        B("AccentButtonBackgroundDisabled", disabledBack);
        Bs(a.AccentForeground, "AccentButtonForeground", "AccentButtonForegroundPointerOver", "AccentButtonForegroundPressed");
        B("AccentButtonForegroundDisabled", disabledText);
        Bs(Colors.Transparent, "AccentButtonBorderBrush", "AccentButtonBorderBrushPointerOver", "AccentButtonBorderBrushPressed", "AccentButtonBorderBrushDisabled");
        // Toggle buttons: quiet, inverse when on.
        B("ToggleButtonBackground", Colors.Transparent);
        B("ToggleButtonBackgroundPointerOver", hover);
        B("ToggleButtonBackgroundPressed", pressed);
        B("ToggleButtonBackgroundChecked", n.Foreground);
        B("ToggleButtonBackgroundCheckedPointerOver", Mix(n.Foreground, n.Background, 0.14));
        B("ToggleButtonBackgroundCheckedPressed", Mix(n.Foreground, n.Background, 0.26));
        Bs(n.Foreground, "ToggleButtonForeground", "ToggleButtonForegroundPointerOver", "ToggleButtonForegroundPressed");
        Bs(n.Background, "ToggleButtonForegroundChecked", "ToggleButtonForegroundCheckedPointerOver", "ToggleButtonForegroundCheckedPressed");
        Bs(Colors.Transparent, "ToggleButtonBorderBrush", "ToggleButtonBorderBrushPointerOver", "ToggleButtonBorderBrushPressed",
            "ToggleButtonBorderBrushChecked", "ToggleButtonBorderBrushCheckedPointerOver", "ToggleButtonBorderBrushCheckedPressed");
        // Fields: white (or raised) with a hairline, the accent when focused.
        Bs(field, "TextControlBackground", "TextControlBackgroundPointerOver", "TextControlBackgroundFocused");
        B("TextControlBackgroundDisabled", disabledBack);
        Bs(n.Foreground, "TextControlForeground", "TextControlForegroundPointerOver", "TextControlForegroundFocused");
        Bs(n.TextMuted, "TextControlPlaceholderForeground", "TextControlPlaceholderForegroundPointerOver", "TextControlPlaceholderForegroundFocused");
        B("TextControlBorderBrush", fieldBorder);
        B("TextControlBorderBrushPointerOver", n.BorderStrong);
        B("TextControlBorderBrushFocused", a.Accent);
        B("TextControlBorderBrushDisabled", n.Border);
        Bs(field, "ComboBoxBackground", "ComboBoxBackgroundUnfocused");
        B("ComboBoxBackgroundPointerOver", dark ? Mix(field, n.Foreground, 0.05) : n.SurfaceMuted);
        B("ComboBoxBackgroundPressed", dark ? Mix(field, n.Foreground, 0.09) : n.BorderSubtle);
        B("ComboBoxBorderBrush", fieldBorder);
        B("ComboBoxBorderBrushPointerOver", n.BorderStrong);
        B("ComboBoxBorderBrushPressed", a.Accent);
        B("ComboBoxDropDownBackground", dark ? n.SurfaceRaised : n.Surface);
        B("ComboBoxDropDownBorderBrush", n.Border);
        B("ComboBoxItemBackgroundPointerOver", hover);
        B("ComboBoxItemBackgroundPressed", pressed);
        Bs(a.AccentSubtle, "ComboBoxItemBackgroundSelected", "ComboBoxItemBackgroundSelectedPointerOver", "ComboBoxItemBackgroundSelectedPressed");
        Bs(n.Foreground, "ComboBoxItemForegroundSelected", "ComboBoxItemForegroundSelectedPointerOver", "ComboBoxItemForegroundSelectedPressed");
        // Expanders: a hairline card.
        B("ExpanderHeaderBackground", Colors.Transparent);
        B("ExpanderHeaderBackgroundPointerOver", hover);
        B("ExpanderHeaderBackgroundPressed", pressed);
        Bs(n.Border, "ExpanderHeaderBorderBrush", "ExpanderHeaderBorderBrushPointerOver", "ExpanderHeaderBorderBrushPressed");
        B("ExpanderContentBackground", Colors.Transparent);
        B("ExpanderContentBorderBrush", n.Border);
        // Checks and radios: the accent when on.
        Bs(a.ControlOn, "CheckBoxCheckBackgroundFillChecked", "CheckBoxCheckBackgroundStrokeChecked");
        Bs(accentHover, "CheckBoxCheckBackgroundFillCheckedPointerOver", "CheckBoxCheckBackgroundStrokeCheckedPointerOver");
        Bs(accentPressed, "CheckBoxCheckBackgroundFillCheckedPressed", "CheckBoxCheckBackgroundStrokeCheckedPressed");
        B("CheckBoxCheckBackgroundStrokeUnchecked", n.BorderStrong);
        B("CheckBoxCheckBackgroundStrokeUncheckedPointerOver", n.TextMuted);
        B("CheckBoxCheckBackgroundFillUnchecked", field);
        B("CheckBoxCheckBackgroundFillUncheckedPointerOver", field);
        Bs(a.ControlGlyph, "CheckBoxCheckGlyphForegroundChecked", "CheckBoxCheckGlyphForegroundCheckedPointerOver", "CheckBoxCheckGlyphForegroundCheckedPressed");
        B("RadioButtonOuterEllipseStroke", n.BorderStrong);
        B("RadioButtonOuterEllipseStrokePointerOver", n.TextMuted);
        B("RadioButtonOuterEllipseFill", field);
        B("RadioButtonOuterEllipseFillPointerOver", field);
        Bs(a.ControlOn, "RadioButtonOuterEllipseCheckedStroke", "RadioButtonOuterEllipseCheckedFill");
        Bs(accentHover, "RadioButtonOuterEllipseCheckedStrokePointerOver", "RadioButtonOuterEllipseCheckedFillPointerOver");
        Bs(a.ControlGlyph, "RadioButtonCheckGlyphFill", "RadioButtonCheckGlyphFillPointerOver", "RadioButtonCheckGlyphStroke", "RadioButtonCheckGlyphStrokePointerOver");
        // Tooltips: Arc's dark chip.
        B("ToolTipBackground", dark ? n.SurfaceRaised : n.Foreground);
        B("ToolTipForeground", dark ? n.Foreground : n.Background);
        B("ToolTipBorderBrush", dark ? n.Border : n.Foreground);
        return d;
    }

    /// <summary><paramref name="t"/> of the way from <paramref name="a"/> to <paramref name="b"/>.</summary>
    static Color Mix(Color a, Color b, double t) => Color.FromArgb(
        (byte)Math.Round(a.A + (b.A - a.A) * t), (byte)Math.Round(a.R + (b.R - a.R) * t),
        (byte)Math.Round(a.G + (b.G - a.G) * t), (byte)Math.Round(a.B + (b.B - a.B) * t));

    static Color WithAlpha(Color c, double alpha) => Color.FromArgb((byte)Math.Round(255 * alpha), c.R, c.G, c.B);
}
