using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>"符号按键盘上印的字输入" in the keyboard settings.</summary>
public class KeyboardSymbolsTests
{
    [Fact]
    public void The_choice_is_read_and_applied_at_once()
    {
        var keymap = new FakeKeymap { Symbols = false };
        var vm = new KeyboardSettingsViewModel(keymap);
        Assert.False(vm.SymbolsAsPrinted);
        vm.SymbolsAsPrinted = true;
        Assert.True(keymap.Symbols);
        Assert.Null(vm.Error);
    }

    [Fact]
    public void A_refusal_is_shown_and_the_box_goes_back()
    {
        var keymap = new FakeKeymap { SymbolsRefusal = "cannot write symbol-keys" };
        var vm = new KeyboardSettingsViewModel(keymap);
        Assert.True(vm.SymbolsAsPrinted);
        vm.SymbolsAsPrinted = false;
        Assert.True(vm.SymbolsAsPrinted);
        Assert.True(keymap.Symbols);
        Assert.Equal("cannot write symbol-keys", vm.Error);
    }
}
