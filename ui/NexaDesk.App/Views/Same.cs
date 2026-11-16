using Avalonia.Data.Converters;

namespace NexaDesk.Views;

/// <summary>Whether two bound values are the same non-empty text: "is this the row just copied?"</summary>
public static class Same
{
    static bool Equal(IEnumerable<object?> values)
    {
        var v = values.ToList();
        return v.Count == 2 && v[0] is string a && a.Length > 0 && v[1] is string b && a == b;
    }

    public static readonly IMultiValueConverter Values = new FuncMultiValueConverter<object?, bool>(Equal);
    public static readonly IMultiValueConverter Differ = new FuncMultiValueConverter<object?, bool>(v => !Equal(v));
}
