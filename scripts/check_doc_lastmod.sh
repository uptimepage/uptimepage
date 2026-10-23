#!/usr/bin/env bash
# Doc `lastmod` freshness check.
#
# `docs/index.toml` carries a hand-written `lastmod` per page and the
# sitemap serves it verbatim. Crawlers schedule a recrawl off that date, so a
# rewritten page still claiming last month's date is invisible to search until
# the next full crawl — silently, because nothing else notices.
#
# The date each page SHOULD carry is the commit date of its markdown. That is
# readable here but not at image build time (`.git/` is outside the Docker
# context), so this is a check rather than a generator.
#
# Staged-but-uncommitted edits have no commit date yet, so a page being edited
# right now must simply carry today's date.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

MANIFEST=docs/index.toml
today="$(date -u +%F)"
fail=0

# Shallow clones (CI checkout defaults to depth 1) collapse every file's
# history onto one commit, which would make every date look wrong.
if [ "$(git rev-parse --is-shallow-repository)" = "true" ]; then
  echo "doc lastmod: skipped (shallow clone has no per-file history)"
  exit 0
fi

# Read before the loop: a failure inside the process substitution would leave
# it nothing to check, and the hook would pass without checking.
pages="$(python3 -c '
import sys, tomllib
for page in tomllib.load(open(sys.argv[1], "rb"))["page"]:
    print(page["slug"], page["lastmod"])
' "$MANIFEST")" || {
  echo "::error::doc lastmod: could not read $MANIFEST (needs python3 3.11+ for tomllib)" >&2
  exit 1
}

while read -r slug declared; do
  md="docs/${slug}.md"
  [ -f "$md" ] || continue

  staged="$(git diff --cached --name-only -- "$md")"
  unstaged="$(git diff --name-only -- "$md")"
  if [ -n "$staged$unstaged" ]; then
    expected="$today"
    why="edited in this commit"
  else
    expected="$(git log -1 --format=%cs -- "$md")"
    why="last committed then"
    [ -n "$expected" ] || continue
  fi

  if [ "$declared" != "$expected" ]; then
    echo "  $md $why → lastmod should be $expected, $MANIFEST says $declared"
    fail=1
  fi
done <<<"$pages"

if [ "$fail" -ne 0 ]; then
  echo
  echo "::error::doc lastmod is stale — the sitemap would tell crawlers these pages did not change." >&2
  exit 1
fi
