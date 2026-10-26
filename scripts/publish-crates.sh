#!/bin/sh
# Publish the workspace's crates to crates.io, one at a time in dependency order, so a release can
# be retried after any hiccup:
#   - a crate whose version is already on crates.io is skipped (crates.io refuses it again anyway);
#   - before the next crate, wait (up to INDEX_WAIT seconds) for the one just published to appear
#     in the sparse index, which is what cargo resolves dependencies from. crates.io can lag well
#     past cargo's own 60s wait, which is what failed v0.1.0.
#
#   scripts/publish-crates.sh v1.2.3     # after scripts/set-version.sh v1.2.3
#
# Run by CI's `publish-crates` job (needs CARGO_REGISTRY_TOKEN). PUBLISH_DRY_RUN=1 only packages and
# verifies what would be published.
set -eu

# Dependency order: a crate comes after everything it depends on.
CRATES="obeliskd obeliskd-cli"
INDEX_WAIT="${INDEX_WAIT:-900}"
USER_AGENT="obeliskd-release (https://github.com/krr-software-solutions/obeliskd)"

tag="${1:?usage: scripts/publish-crates.sh <tag|version>}"
version="${tag#v}"
cd "$(dirname "$0")/.."

say() { printf 'publish-crates: %s\n' "$*" >&2; }

# Whether crates.io has <crate> <version> (its API knows the moment the upload is accepted).
on_crates_io() {
  code="$(curl --silent --output /dev/null --write-out '%{http_code}' --user-agent "$USER_AGENT" \
    "https://crates.io/api/v1/crates/$1/$2")"
  [ "$code" = 200 ]
}

# A crate's file in the sparse index: 1/a, 2/ab, 3/a/abc, ab/cd/abcd….
index_path() {
  name="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  case "${#name}" in
    1) echo "1/$name" ;;
    2) echo "2/$name" ;;
    3) echo "3/$(printf '%s' "$name" | cut -c1)/$name" ;;
    *) echo "$(printf '%s' "$name" | cut -c1-2)/$(printf '%s' "$name" | cut -c3-4)/$name" ;;
  esac
}

# Whether the sparse index lists <crate> <version>, i.e. other crates can depend on it. The same
# URL cargo reads, on purpose: the CDN in front of it can serve a cached 404 for a while after the
# origin has the crate, and cargo sees that same cached answer.
in_index() {
  curl --silent --fail --user-agent "$USER_AGENT" "https://index.crates.io/$(index_path "$1")" |
    grep -q "\"vers\":\"$2\""
}

wait_for_index() {
  waited=0
  until in_index "$1" "$2"; do
    if [ "$waited" -ge "$INDEX_WAIT" ]; then
      say "$1 $2 still isn't in the index after ${INDEX_WAIT}s; retry the job later"
      exit 1
    fi
    sleep 10
    waited=$((waited + 10))
  done
  say "$1 $2 is in the index"
}

if [ -z "${PUBLISH_DRY_RUN:-}" ]; then
  : "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is not set}"
fi

for crate in $CRATES; do
  if on_crates_io "$crate" "$version"; then
    say "$crate $version is already on crates.io; skipping it"
  elif [ -n "${PUBLISH_DRY_RUN:-}" ]; then
    say "$crate $version: dry run"
    cargo publish -p "$crate" --locked --allow-dirty --dry-run
    continue
  else
    say "publishing $crate $version"
    # set-version rewrote Cargo.toml/Cargo.lock, hence --allow-dirty. If cargo gives up waiting
    # after a successful upload, the upload still counts: the index wait below takes over.
    cargo publish -p "$crate" --locked --allow-dirty || on_crates_io "$crate" "$version" || exit 1
  fi
  wait_for_index "$crate" "$version"
done
say "done: $CRATES at $version"
