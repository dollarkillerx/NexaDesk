#!/bin/sh
# Builds website/ and publishes it to the Cloudflare Pages project "nexadesk"
# (https://nexadesk-bq9.pages.dev). Needs `npx wrangler login` once on this machine.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SITE_URL=https://nexadesk-bq9.pages.dev node "$ROOT/website/build.mjs"
npx --yes wrangler@4 pages deploy "$ROOT/website/dist" --project-name nexadesk --branch main --commit-dirty=true
