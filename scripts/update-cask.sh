#!/bin/sh
# Point the Homebrew cask in OlofMoriya/homebrew-tap at a published release.
#   sh scripts/update-cask.sh v0.1.0
set -eu
TAG="$1"
VERSION="${TAG#v}"
export GH_HOST=github.com
WORK="$(mktemp -d)"
gh release download "$TAG" -R OlofMoriya/pdf-review -p PDF-Review-mac.dmg -D "$WORK"
SHA=$(shasum -a 256 "$WORK/PDF-Review-mac.dmg" | cut -d' ' -f1)
gh repo clone OlofMoriya/homebrew-tap "$WORK/tap" -- -q
mkdir -p "$WORK/tap/Casks"
cat > "$WORK/tap/Casks/pdf-review.rb" <<CASK
cask "pdf-review" do
  version "$VERSION"
  sha256 "$SHA"

  url "https://github.com/OlofMoriya/pdf-review/releases/download/v#{version}/PDF-Review-mac.dmg"
  name "PDF Review"
  desc "PDF reader for reviewing documents: annotate, comment, work through review lists"
  homepage "https://github.com/OlofMoriya/pdf-review"

  depends_on macos: :monterey

  app "PDF Review.app"

  zap trash: "~/.pdf-review"

  caveats <<~EOS
    PDF Review is not signed by a paid Apple developer account. On first launch, open
    System Settings > Privacy & Security and click "Open Anyway" next to PDF Review.
  EOS
end
CASK
cd "$WORK/tap"
git add Casks/pdf-review.rb
git -c user.name="Olof Moriya" -c user.email="13721900+OlofMoriya@users.noreply.github.com" commit -q -m "pdf-review $VERSION"
git push -q
echo "Cask updated to $VERSION ($SHA)"
rm -rf "$WORK"
