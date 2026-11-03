# PDF Review

A fast PDF reader for reviewing documents. Highlight, underline, strike out or box text, write
notes straight onto the page, work through review lists that point at places in the document,
and export everything as Markdown.

Built in Rust with [GPUI](https://www.gpui.rs) and [MuPDF](https://mupdf.com).

## Install

Download the latest version from [Releases](https://github.com/OlofMoriya/pdf-review/releases/latest).

### Mac

1. Open **PDF-Review-mac.dmg** and drag **PDF Review** into **Applications**.
2. Open it. macOS says it "cannot verify" the app, because it is not signed by a paid Apple
   developer account. Click **Done** (not "Move to Bin").
3. Open **System Settings → Privacy & Security**, scroll down to *"PDF Review" was blocked*, and
   click **Open Anyway**. Confirm with your password or Touch ID.

You only do this once. Works on Apple Silicon and Intel Macs, macOS 12 or later.

Or with [Homebrew](https://brew.sh):

```bash
brew install --cask olofmoriya/tap/pdf-review
```

The first launch still needs **Open Anyway** as above.

### Windows

1. Unzip **PDF-Review-windows.zip** somewhere, for example in Documents.
2. Double-click **PDF Review.exe**. Windows shows "Windows protected your PC", because the app
   is not signed. Click **More info**, then **Run anyway**.

You only do this once.

## Use

Open a PDF with ⌘O / Ctrl+O, by dropping it on the app, or with *Open With → PDF Review*.

| Key | Tool |
|---|---|
| `h` | Highlight following the text (⌥/Alt-click highlights a sentence) |
| `⇧h` | Box highlight |
| `u` / `s` / `b` / `p` | Underline / strike / box / pen |
| `c` | Note |
| `/`, `n`, `N` | Search, next, previous |
| `gg`, `G`, `g<n>`, `j`, `k` | First, last, page n, scroll |
| `m<a>`, `'<a>` | Set mark, jump to mark |
| `Tab`, `x` | Next review item, mark it done |
| ⌘E / Ctrl+E | Export the review as Markdown |

Annotations are saved automatically in `~/.pdf-review/`. The PDF itself is never changed.

## Review lists

Someone (or an agent) can send you a list of places to look at in a document. See
[REVIEW_LISTS.md](REVIEW_LISTS.md).

## Build from source

```bash
cargo run --release -- path/to/file.pdf
sh scripts/bundle.sh   # Mac: builds ~/Applications/PDF Review.app
```

Releases are built by GitHub Actions when a `v*` tag is pushed.

## License

[AGPL-3.0-or-later](LICENSE), as required by MuPDF.
