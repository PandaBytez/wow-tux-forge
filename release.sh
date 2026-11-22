#!/usr/bin/env bash
set -euo pipefail

RAW_VERSION="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}"
VERSION="${RAW_VERSION#v}"
TAG="v${VERSION}"

if [ -z "$VERSION" ]; then
  echo "Usage: $0 [version]" >&2
  exit 1
fi

cargo test

# Collect commits since previous tag (or all commits for first release)
PREV_TAG=$(git describe --tags --abbrev=0 2>/dev/null || true)
if [ -n "$PREV_TAG" ]; then
  COMMITS=$(git log "${PREV_TAG}..HEAD" --pretty=format:"- %s (%h)" --no-merges)
else
  COMMITS=$(git log --pretty=format:"- %s (%h)" --no-merges)
fi
[ -z "$COMMITS" ] && COMMITS="- Release ${TAG}"

NOTES=$(printf "## %s (%s)\n\n%s\n" "$TAG" "$(date +%Y-%m-%d)" "$COMMITS")

# Prepend to CHANGELOG.md if this version isn't already recorded
if [ ! -f CHANGELOG.md ] || ! grep -q "^## ${TAG} " CHANGELOG.md; then
  TMP_CL=$(mktemp)
  if [ -f CHANGELOG.md ]; then
    printf "# Changelog\n\n%s\n\n%s\n" "$NOTES" "$(sed '1{/^# Changelog$/d}; /^$/d' CHANGELOG.md)" > "$TMP_CL"
  else
    printf "# Changelog\n\n%s\n" "$NOTES" > "$TMP_CL"
  fi
  mv "$TMP_CL" CHANGELOG.md
fi

# Sync Cargo.toml / Cargo.lock if version changed
CUR_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
if [ "$CUR_VERSION" != "$VERSION" ]; then
  sed -i "0,/^version = \".*\"/s//version = \"${VERSION}\"/" Cargo.toml
  cargo check --quiet
fi

if ! git diff --quiet || [ -n "$(git status --porcelain)" ]; then
  git add Cargo.toml Cargo.lock CHANGELOG.md README.md 2>/dev/null || git add -A
  git commit -m "Release ${TAG}"
fi

git push origin main

if ! git rev-parse "$TAG" >/dev/null 2>&1; then
  git tag -a "$TAG" -m "Release ${TAG}"
fi
git push origin "$TAG"

if gh release view "$TAG" >/dev/null 2>&1; then
  gh release edit "$TAG" --notes "$NOTES"
else
  gh release create "$TAG" --title "$TAG" --notes "$NOTES"
fi

# Update Homebrew tap formula to point at the release tag
TAP_DIR=$(mktemp -d)
trap 'rm -rf "$TAP_DIR"' EXIT
TAR_URL="https://github.com/PandaBytez/wow-tux-forge/archive/refs/tags/${TAG}.tar.gz"
SHA256=$(curl -fsSL "$TAR_URL" | sha256sum | awk '{print $1}')

gh repo clone PandaBytez/homebrew-tap "$TAP_DIR" -- --quiet
cat > "${TAP_DIR}/Formula/wow-tux-forge.rb" <<EOF
class WowTuxForge < Formula
  desc "Terminal UI (TUI) manager for World of Warcraft addons hosted on GitHub"
  homepage "https://github.com/PandaBytez/wow-tux-forge"
  url "${TAR_URL}"
  version "${VERSION}"
  sha256 "${SHA256}"
  license "GPL-3.0"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_predicate bin/"wow-tux-forge", :exist?
  end
end
EOF

git -C "$TAP_DIR" add Formula/wow-tux-forge.rb
if ! git -C "$TAP_DIR" diff --cached --quiet; then
  git -C "$TAP_DIR" commit -m "wow-tux-forge ${VERSION}"
  git -C "$TAP_DIR" push
fi
