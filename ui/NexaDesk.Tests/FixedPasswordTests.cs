using Avalonia.Headless.XUnit;
using NexaDesk.Proto;
using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>
/// The fixed password: set and cleared through the agent while hosting, and typed in the
/// same field as the one-time code on the other side, which tells them apart by shape.
/// </summary>
public class FixedPasswordTests
{
    [Fact]
    public void What_is_typed_is_a_code_if_it_can_be_one_else_the_password()
    {
        Assert.Equal(new ConnectAuth.Code("ABCD2345"), Texts.SignInFor("abcd-2345"));
        Assert.Equal(new ConnectAuth.Password("correct horse"), Texts.SignInFor("correct horse"));
        Assert.Null(Texts.SignInFor("short"));
        Assert.Null(Texts.SignInFor(new string('x', 129)));
        Assert.Null(Texts.SignInFor(""));
    }

    [Fact]
    public void A_new_password_must_be_long_enough_and_not_look_like_a_code()
    {
        Assert.Equal("Host.PasswordTooShort", Texts.PasswordProblem("abc", "abc"));
        Assert.Equal("Host.PasswordLikeCode", Texts.PasswordProblem("ABCD2345", "ABCD2345"));
        Assert.Equal("Host.PasswordLikeCode", Texts.PasswordProblem("abcd-2345", "abcd-2345"));
        Assert.Equal("Host.PasswordMismatch", Texts.PasswordProblem("correct horse", "correct hose"));
        Assert.Equal("Host.PasswordBadCharacters", Texts.PasswordProblem("correct\thorse", "correct\thorse"));
        Assert.Null(Texts.PasswordProblem("correct horse", "correct horse"));
    }

    [Fact]
    public void A_password_goes_out_as_field_6()
    {
        var bytes = new ConnectRequest("10.0.0.2:47800", 1, StreamMode.Balanced, 0, new ConnectAuth.Password("pw pw pw")).Encode();
        // Field 6, length-delimited: 0x32, then 8 bytes of "pw pw pw".
        Assert.Contains("32087077207077207077", Convert.ToHexString(bytes));
    }

    static async Task<(MainViewModel, FakeProcess, FakeLink)> Hosting(bool passwordSet)
    {
        var link = new FakeLink();
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        var process = new FakeProcess();
        var main = new MainViewModel(new FakeClient(), new MemoryRecent(), link, process, new FakePermissions(), new RecordingWindows(),
            new FakeAddresses(["192.168.1.35"]), "Mac", () => Samples.T0, new MemorySettings());
        await main.Host.StartCommand.ExecuteAsync(null);
        link.Raise(Samples.Status() with { PasswordSet = passwordSet });
        return (main, process, link);
    }

    [AvaloniaFact]
    public async Task Setting_a_password_checks_it_saves_it_with_the_agent_and_restarts_hosting()
    {
        var (main, process, link) = await Hosting(passwordSet: false);
        var host = main.Host;
        Assert.False(host.PasswordSet);
        host.EditPasswordCommand.Execute(null);
        Assert.True(host.IsEditingPassword);

        host.NewPassword = "correct horse";
        host.ConfirmPassword = "correct hose";
        await host.SavePasswordCommand.ExecuteAsync(null);
        Assert.NotNull(host.PasswordError);
        Assert.Empty(process.Passwords);

        // The old agent is gone by the time the new start looks for one.
        link.ConnectResults.Enqueue("no socket");
        link.ConnectResults.Enqueue(null);
        host.ConfirmPassword = "correct horse";
        await host.SavePasswordCommand.ExecuteAsync(null);
        Assert.Equal(["correct horse"], process.Passwords);
        Assert.False(host.IsEditingPassword);
        Assert.Equal("", host.NewPassword);
        Assert.Equal(2, process.Starts.Count);
        Assert.Equal(1, process.Kills);
        Assert.NotNull(host.Notice);
    }

    [AvaloniaFact]
    public async Task Clearing_is_offered_once_set_and_a_failure_is_shown()
    {
        var (main, process, _) = await Hosting(passwordSet: true);
        var host = main.Host;
        Assert.True(host.PasswordSet);
        Assert.NotEqual(host.PasswordStateText, new MainViewModel(new FakeClient(), new MemoryRecent(), new FakeLink(), new FakeProcess(),
            new FakePermissions(), new RecordingWindows(), new FakeAddresses([]), "Mac", () => Samples.T0).Host.PasswordStateText);

        process.PasswordResult = "keychain: denied";
        await host.ClearPasswordCommand.ExecuteAsync(null);
        Assert.Equal([null], process.Passwords);
        Assert.Contains("keychain: denied", host.PasswordError);
        Assert.Single(process.Starts);
    }
}
