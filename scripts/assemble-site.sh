#!/usr/bin/env bash
# Assembles the GitHub Pages artifact from site/ into the given directory.
# Writes a CNAME holding exactly $NGUDA_SITE_DOMAIN when it is set and non-empty;
# with no domain, the site deploys to the default github.io address.
set -euo pipefail

out="${1:?usage: assemble-site.sh <output-dir>}"
root="$(cd "$(dirname "$0")/.." && pwd)"

mkdir -p "$out"
rsync -a --exclude .htmlvalidate.json "$root/site/" "$out/"
if [ -n "${NGUDA_SITE_DOMAIN:-}" ]; then
  printf '%s' "$NGUDA_SITE_DOMAIN" > "$out/CNAME"
fi
