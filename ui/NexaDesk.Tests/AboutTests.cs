using NexaDesk.Localization;
using NexaDesk.ViewModels;
using Xunit;

namespace NexaDesk.Tests;

/// <summary>The links beside the version: GitHub, issues, and the website in the app's language.</summary>
public class AboutTests
{
    [Fact]
    public void Links_open_the_project_pages()
    {
        var windows = new RecordingWindows();
        var about = new AboutViewModel(windows);
        about.OpenRepositoryCommand.Execute(null);
        about.OpenIssuesCommand.Execute(null);
        Assert.Equal(["https://github.com/dollarkillerx/NexaDesk", "https://github.com/dollarkillerx/NexaDesk/issues"],
            windows.OpenedUrls);
    }

    [Theory]
    [InlineData(L.Chinese, "https://nexadesk-bq9.pages.dev/")]
    [InlineData(L.Japanese, "https://nexadesk-bq9.pages.dev/ja/")]
    [InlineData(L.English, "https://nexadesk-bq9.pages.dev/en/")]
    public void Website_follows_the_language(string language, string url)
    {
        var windows = new RecordingWindows();
        using (L.Scoped(language))
            new AboutViewModel(windows).OpenWebsiteCommand.Execute(null);
        Assert.Equal([url], windows.OpenedUrls);
    }

    [Fact]
    public void Every_link_is_one_the_browser_opener_accepts()
    {
        foreach (var url in new[] { AboutViewModel.RepositoryUrl, AboutViewModel.IssuesUrl,
                     AboutViewModel.WebsiteUrl(L.Chinese), AboutViewModel.WebsiteUrl(L.Japanese), AboutViewModel.WebsiteUrl(L.English) })
            Assert.NotNull(NexaDesk.Views.WindowService.OpenUrlCommand(url, windows: false));
    }
}
