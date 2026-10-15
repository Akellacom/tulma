#!/usr/bin/env bash
# Sets the version of the app (project.yml) and the daemon (core/Cargo.toml) in one go.
#
#   scripts/bump-version.sh 0.2.0
#
# Pushing the result to main publishes the release (see .github/workflows/release.yml).

set -euo pipefail

cd "$(dirname "$0")/.."

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

VERSION="${1:-}"
VERSION="${VERSION#v}"
[[ "$VERSION" =~ ^[0-9]+(\.[0-9]+){1,2}$ ]] || die "usage: scripts/bump-version.sh 1.2.3"

sed -i '' -E "s/^( *MARKETING_VERSION: *)\"?[0-9.]*\"?/\1\"$VERSION\"/" project.yml
sed -i '' -E "/^\[workspace\.package\]/,/^\[/ s/^version = \"[^\"]*\"/version = \"$VERSION\"/" core/Cargo.toml
# release.sh builds with --locked, so the lockfile must carry the new workspace version.
(cd core && cargo update --workspace --quiet)

grep -q "MARKETING_VERSION: \"$VERSION\"" project.yml || die "could not update project.yml"
grep -q "^version = \"$VERSION\"" core/Cargo.toml || die "could not update core/Cargo.toml"

if git rev-parse -q --verify "refs/tags/v$VERSION" > /dev/null; then
    printf 'warning: tag v%s already exists, so CI will not publish this version again\n' "$VERSION" >&2
fi

printf 'Version set to %s. Commit and push to main to release:\n\n' "$VERSION"
printf '  git commit -am "chore: release %s" && git push\n' "$VERSION"
