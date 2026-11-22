#!/usr/bin/env bash
# Checks scripts/assemble-site.sh: no CNAME without a domain, an exact CNAME with one.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

NGUDA_SITE_DOMAIN='' "$here/assemble-site.sh" "$tmp/none"
[ -f "$tmp/none/index.html" ] && [ -f "$tmp/none/privacy.html" ] || { echo "pages missing"; exit 1; }
[ ! -e "$tmp/none/CNAME" ] || { echo "CNAME written without a domain"; exit 1; }
[ ! -e "$tmp/none/.htmlvalidate.json" ] || { echo "validator config deployed"; exit 1; }

NGUDA_SITE_DOMAIN='nguda.example.com' "$here/assemble-site.sh" "$tmp/domain"
[ "$(cat "$tmp/domain/CNAME")" = 'nguda.example.com' ] || { echo "CNAME content wrong"; exit 1; }
[ "$(wc -c < "$tmp/domain/CNAME" | tr -d ' ')" = 17 ] || { echo "CNAME has extra bytes"; exit 1; }

echo "assemble-site: ok"
