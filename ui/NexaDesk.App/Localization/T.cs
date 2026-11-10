using Avalonia.Markup.Xaml;

namespace NexaDesk.Localization;

/// <summary>
/// <c>Text="{l:T Main.Title}"</c>: the string for a key, in the language chosen at start.
/// Also usable where a format is expected: <c>StringFormat={l:T Main.ThisDevice}</c>.
/// </summary>
public sealed class T : MarkupExtension
{
    public T()
    {
    }

    public T(string key) => Key = key;

    public string Key { get; set; } = "";

    public override object ProvideValue(IServiceProvider serviceProvider) => L.Get(Key);
}
