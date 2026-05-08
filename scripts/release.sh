#!/bin/bash
# Release script: bumps Cargo.toml version, commits, tags, and pushes.
#
# Usage:
#   ./scripts/release.sh v1.2.0
#   ./scripts/release.sh 1.2.0    # v prefix optional

set -e

if [ -z "$1" ]; then
    echo "Usage: $0 <version>"
    echo "Example: $0 v1.2.0"
    current=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
    latest_tag=$(git describe --tags --abbrev=0 2>/dev/null || echo "none")
    echo ""
    echo "Current Cargo.toml: $current"
    echo "Latest git tag:     $latest_tag"
    exit 1
fi

VERSION="${1#v}"
TAG="v$VERSION"

# Check for uncommitted changes
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "Error: uncommitted changes. Commit or stash first."
    exit 1
fi

# Check tag doesn't exist
if git rev-parse "$TAG" >/dev/null 2>&1; then
    echo "Error: tag $TAG already exists"
    exit 1
fi

echo "Releasing $TAG..."

# Update Cargo.toml
sed -i '' "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml

# Verify it compiles
cargo check --quiet 2>/dev/null

# Commit and tag
git add Cargo.toml
git commit -m "chore: bump version to $VERSION"
git tag "$TAG"

echo ""
echo "Done! Committed and tagged $TAG."
echo "Push when ready: git push origin main --tags"
