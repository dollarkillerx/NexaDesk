using NexaDesk.Services;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>Both permissions are asked for through macOS, which lists NexaDesk itself.</summary>
public class PermissionRequestTests
{
    [Fact]
    public void Each_request_button_asks_macos_for_its_own_permission()
    {
        var perms = new FakePermissions { State = new PermissionState(false, false) };
        var guide = new PermissionGuideViewModel(perms, PermissionContext.Hosting);
        guide.RequestScreenRecordingCommand.Execute(null);
        guide.RequestAccessibilityCommand.Execute(null);
        Assert.Equal([PermissionKind.ScreenRecording, PermissionKind.Accessibility], perms.Requested);
        Assert.Empty(perms.Opened);
        guide.OpenAccessibilityCommand.Execute(null);
        Assert.Equal([PermissionKind.Accessibility], perms.Opened);
    }
}
