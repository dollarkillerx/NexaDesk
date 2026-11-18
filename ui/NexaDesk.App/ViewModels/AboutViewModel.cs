using CommunityToolkit.Mvvm.Input;
using NexaDesk.Localization;

namespace NexaDesk.ViewModels;

/// <summary>
/// Where the project lives, next to the version: its source on GitHub, where to report a problem,
/// and the website in the app's language (the pages the READMEs link). The iOS app's About page
/// has the same links (apps/ios/NexaDesk/UI/AboutView.swift).
/// </summary>
public sealed partial class AboutViewModel(IWindowService windows)
{
    public const string RepositoryUrl = "https://github.com/dollarkillerx/NexaDesk";
    public const string IssuesUrl = RepositoryUrl + "/issues";

    /// <summary>The website's page for <paramref name="language"/>: Chinese is its front page.</summary>
    public static string WebsiteUrl(string language) => language switch
    {
        L.Chinese => "https://nexadesk-bq9.pages.dev/",
        L.Japanese => "https://nexadesk-bq9.pages.dev/ja/",
        _ => "https://nexadesk-bq9.pages.dev/en/",
    };

    [RelayCommand]
    private void OpenRepository() => windows.OpenUrl(RepositoryUrl);

    [RelayCommand]
    private void OpenIssues() => windows.OpenUrl(IssuesUrl);

    [RelayCommand]
    private void OpenWebsite() => windows.OpenUrl(WebsiteUrl(L.Current));
}
