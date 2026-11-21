// The site's language: a choice made on the page is remembered; a first visit to the
// Chinese root goes to the browser's language when that is Traditional Chinese, English,
// Japanese or Korean, and
// to English for any language the site does not have.
(() => {
  const KEY = "nexadesk.site.lang";
  const PATHS = { "zh-Hans": "/", "zh-Hant": "/zh-hant/", en: "/en/", ja: "/ja/", ko: "/ko/" };
  const read = () => {
    try {
      return localStorage.getItem(KEY);
    } catch {
      return null;
    }
  };
  for (const a of document.querySelectorAll("a[data-lang]")) {
    a.addEventListener("click", () => {
      try {
        localStorage.setItem(KEY, a.dataset.lang);
      } catch {}
    });
  }
  if (location.pathname === "/" || location.pathname === "/index.html") {
    const pick = (l) =>
      l.startsWith("zh-hant") || /^zh-(tw|hk|mo)(-|$)/.test(l) ? "zh-Hant"
      : l.startsWith("zh") ? "zh-Hans" : l.startsWith("ja") ? "ja" : l.startsWith("ko") ? "ko"
      : l.startsWith("en") ? "en" : null;
    const saved = read();
    const want = PATHS[saved]
      ? saved
      : (navigator.languages ?? [navigator.language]).map((l) => pick(String(l).toLowerCase())).find(Boolean) ?? "en";
    if (want !== "zh-Hans") location.replace(PATHS[want] + location.hash);
  }
  // The language menu closes when anything else is clicked.
  const menu = document.querySelector("details.lang");
  document.addEventListener("click", (e) => {
    if (menu?.open && !menu.contains(e.target)) menu.open = false;
  });
})();
