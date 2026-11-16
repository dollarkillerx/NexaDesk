// Builds the NexaDesk site into website/dist: one page per language from content.mjs,
// the stylesheet, the language script, and the README's screenshots made web-sized.
//
//   node website/build.mjs
//
// No dependencies; images are resized with macOS's sips when it is there.

import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { LANGS, REPO, TEXT } from "./content.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const out = join(here, "dist");
const SITE = process.env.SITE_URL ?? "https://nexadesk-bq9.pages.dev";

const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

// The screenshots, at most this wide: sharp on a 2x screen at the width they show.
const IMAGES = {
  "main.png": 1600,
  "web-session.png": 1600,
  "web-phone.png": 780,
  "web-phone-session.png": 1200,
  "consent.png": 900,
  "web-terminal.png": 1400,
};

const ICONS = {
  speed: '<path d="M4 14a8 8 0 1 1 16 0"/><path d="M12 14l4-5"/><path d="M4 18h16"/>',
  route: '<circle cx="6" cy="6" r="2"/><circle cx="18" cy="18" r="2"/><path d="M8 6h5a3 3 0 0 1 0 6h-2a3 3 0 0 0 0 6h5"/>',
  browser: '<rect x="3" y="4" width="18" height="16"/><path d="M3 9h18"/><path d="M6 6.5h1M9 6.5h1"/>',
  keyboard: '<rect x="2.5" y="6" width="19" height="12"/><path d="M6 10h1M9.5 10h1M13 10h1M16.5 10h1M7 14h10"/>',
  clipboard: '<rect x="5" y="4" width="14" height="17"/><path d="M9 4V3h6v1"/><path d="M8.5 12l2.5 2.5 4.5-5"/>',
  people: '<circle cx="9" cy="8" r="3"/><path d="M3.5 19a5.5 5.5 0 0 1 11 0"/><circle cx="17" cy="9" r="2.5"/><path d="M15.5 14.2A4.5 4.5 0 0 1 21 19"/>',
  terminal: '<rect x="3" y="4" width="18" height="16"/><path d="M7 9l3 3-3 3M12 15h5"/>',
  more: '<rect x="4" y="4" width="7" height="7"/><rect x="13" y="4" width="7" height="7"/><rect x="4" y="13" width="7" height="7"/><rect x="13" y="13" width="7" height="7"/>',
};

const icon = (name) =>
  `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round">${ICONS[name]}</svg>`;

const GITHUB_MARK =
  '<svg class="gh" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8Z"/></svg>';

const MARK = { yes: "✓", beta: "β", soon: "…", "—": "—" };

function page(lang) {
  const t = TEXT[lang.code];
  const up = lang.path === "/" ? "" : "../";
  const img = (name, alt, cls = "", w = "", h = "") =>
    `<img src="${up}img/${name}" alt="${esc(alt)}" loading="lazy" decoding="async"${cls ? ` class="${cls}"` : ""}${w ? ` width="${w}" height="${h}"` : ""}>`;
  const alternates = LANGS.map((l) => `<link rel="alternate" hreflang="${l.code}" href="${SITE}${l.path}">`).join("\n  ");
  const switcher = LANGS.map(
    (l) => `<a href="${up}${l.path.slice(1)}" hreflang="${l.code}" lang="${l.code}" data-lang="${l.code}"${l.code === lang.code ? ' aria-current="page"' : ""}>${l.name}</a>`,
  ).join("");
  return `<!doctype html>
<html lang="${lang.code}">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>${esc(t.title)}</title>
  <meta name="description" content="${esc(t.description)}">
  <meta name="color-scheme" content="light dark">
  <meta property="og:title" content="${esc(t.title)}">
  <meta property="og:description" content="${esc(t.description)}">
  <meta property="og:type" content="website">
  <meta property="og:image" content="${SITE}/img/main.png">
  <link rel="canonical" href="${SITE}${lang.path}">
  ${alternates}
  <link rel="alternate" hreflang="x-default" href="${SITE}/">
  <link rel="icon" href="${up}icon.png">
  <link rel="stylesheet" href="${up}style.css">
  <script src="${up}site.js" defer></script>
</head>
<body>
<header class="top">
  <div class="wrap top-row">
    <a class="brand" href="${up || "./"}"><img src="${up}icon.png" alt="" width="28" height="28"><span>Nexa<b>Desk</b></span></a>
    <nav class="links" aria-label="${esc(t.nav.features)}">
      <a href="#features">${t.nav.features}</a>
      <a href="#browser">${t.nav.browser}</a>
      <a href="#connect">${t.nav.connect}</a>
      <a href="#security">${t.nav.security}</a>
      <a href="#start">${t.nav.start}</a>
    </nav>
    <div class="top-actions">
      <details class="lang">
        <summary aria-label="${esc(t.lang)}">${lang.name}</summary>
        <div class="lang-menu">${switcher}</div>
      </details>
      <a class="gh-link" href="${REPO}" rel="noopener">${GITHUB_MARK}<span>${t.github}</span></a>
    </div>
  </div>
</header>

<main>
<section class="hero">
  <div class="wrap hero-grid">
    <div class="hero-copy">
      <span class="tag">${t.badge}</span>
      <h1>${t.heroTitle}</h1>
      <p class="lead">${esc(t.heroText)}</p>
      <div class="cta">
        <a class="button primary" href="#start">${t.heroPrimary}</a>
        <a class="button ghost" href="${REPO}" rel="noopener">${GITHUB_MARK}${t.heroSecondary}</a>
      </div>
      <p class="note">${esc(t.heroNote)}</p>
    </div>
    <div class="hero-shots">
      <figure class="shot window">${img("main.png", t.description, "", 1600, 1554)}</figure>
      <figure class="shot phone">${img("web-phone.png", t.shotPhone, "", 780, 1688)}</figure>
    </div>
  </div>
</section>

<section id="features" class="band">
  <div class="wrap">
    <h2>${t.featuresTitle}</h2>
    <p class="section-lead">${esc(t.featuresText)}</p>
    <div class="grid">
      ${t.features.map(([i, h, p]) => `<article class="card">${icon(i)}<h3>${esc(h)}</h3><p>${esc(p)}</p></article>`).join("\n      ")}
    </div>
  </div>
</section>

<section id="browser" class="band alt">
  <div class="wrap split">
    <div>
      <h2>${t.browserTitle}</h2>
      <p class="section-lead">${esc(t.browserText)}</p>
      <ul class="checks">${t.browserPoints.map((p) => `<li>${esc(p)}</li>`).join("")}</ul>
    </div>
    <figure class="shot window">${img("web-session.png", t.shotWeb, "", 1600, 950)}<figcaption>${esc(t.shotWeb)}</figcaption></figure>
  </div>
  <div class="wrap">
    <figure class="shot window wide">${img("web-phone-session.png", t.shotPhoneSession, "", 1200, 554)}<figcaption>${esc(t.shotPhoneSession)}</figcaption></figure>
  </div>
</section>

<section id="connect" class="band">
  <div class="wrap">
    <h2>${t.connectTitle}</h2>
    <p class="section-lead">${esc(t.connectText)}</p>
    <ol class="paths">
      ${t.connect.map(([h, p], i) => `<li><span class="step">${i + 1}</span><h3>${esc(h)}</h3><p>${esc(p)}</p></li>`).join("\n      ")}
    </ol>
  </div>
</section>

<section id="security" class="band alt">
  <div class="wrap split reverse">
    <figure class="shot window narrow">${img("consent.png", t.shotConsent, "", 900, 1208)}<figcaption>${esc(t.shotConsent)}</figcaption></figure>
    <div>
      <h2>${t.securityTitle}</h2>
      <p class="section-lead">${esc(t.securityText)}</p>
      <ul class="checks">${t.security.map((p) => `<li>${esc(p)}</li>`).join("")}</ul>
    </div>
  </div>
</section>

<section class="band">
  <div class="wrap">
    <h2>${t.platformsTitle}</h2>
    <div class="table-scroll">
      <table class="platforms">
        <thead><tr>${t.platformsHead.map((h) => `<th scope="col">${esc(h)}</th>`).join("")}</tr></thead>
        <tbody>
          ${t.platforms
            .map(
              ([name, host, viewer]) =>
                `<tr><th scope="row">${esc(name)}</th>${[host, viewer]
                  .map((v) => `<td><span class="mark ${v === "—" ? "none" : v}" title="${esc(t.platformLegend[v] ?? "")}">${MARK[v]}</span>${t.platformLegend[v] ? `<span class="mark-text">${esc(t.platformLegend[v])}</span>` : ""}</td>`)
                  .join("")}</tr>`,
            )
            .join("\n          ")}
        </tbody>
      </table>
    </div>
  </div>
</section>

<section id="start" class="band dark">
  <div class="wrap">
    <h2>${t.startTitle}</h2>
    <p class="section-lead">${esc(t.startText)}</p>
    <ol class="steps">
      ${t.startSteps
        .map(([h, p]) => {
          const code = /^(scripts|open)\b/.test(p);
          return `<li><h3>${esc(h)}</h3>${code ? `<pre><code>${esc(p)}</code></pre>` : `<p>${esc(p)}</p>`}</li>`;
        })
        .join("\n      ")}
    </ol>
    <p class="docs">${esc(t.startDocs)} <a href="${REPO}/tree/main/docs" rel="noopener">${t.docsLink} →</a></p>
  </div>
</section>
</main>

<footer class="foot">
  <div class="wrap foot-row">
    <span>${esc(t.footer)}</span>
    <span class="foot-langs">${switcher}</span>
  </div>
</footer>
</body>
</html>
`;
}

rmSync(out, { recursive: true, force: true });
mkdirSync(join(out, "img"), { recursive: true });
for (const lang of LANGS) {
  const dir = join(out, lang.path);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "index.html"), page(lang));
}
copyFileSync(join(here, "style.css"), join(out, "style.css"));
copyFileSync(join(here, "site.js"), join(out, "site.js"));
copyFileSync(join(root, "crates/nexa-web-page/web/icon.png"), join(out, "icon.png"));
const sips = existsSync("/usr/bin/sips");
for (const [name, width] of Object.entries(IMAGES)) {
  const from = join(root, "docs/images", name);
  const to = join(out, "img", name);
  if (sips) execFileSync("/usr/bin/sips", ["--resampleWidth", String(width), from, "--out", to], { stdio: "ignore" });
  else copyFileSync(from, to);
}
writeFileSync(join(out, "_headers"), `/*
  X-Content-Type-Options: nosniff
  Referrer-Policy: strict-origin-when-cross-origin
  Content-Security-Policy: default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'
/img/*
  Cache-Control: public, max-age=86400
`);
writeFileSync(join(out, "robots.txt"), `User-agent: *\nAllow: /\nSitemap: ${SITE}/sitemap.xml\n`);
writeFileSync(
  join(out, "sitemap.xml"),
  `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${LANGS.map((l) => `  <url><loc>${SITE}${l.path}</loc></url>`).join("\n")}\n</urlset>\n`,
);
console.log(`built ${LANGS.length} pages into ${out}`);
