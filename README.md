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
brew trust olofmoriya/tap
```

```bash
brew install --cask olofmoriya/tap/pdf-review
```

Homebrew asks you to trust third-party taps once (`brew trust`).

macOS still blocks the first launch. Either use **Open Anyway** as above, or clear the
quarantine flag once after installing (and after each `brew upgrade`):

```bash
xattr -dr com.apple.quarantine "/Applications/PDF Review.app"
```

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
| ⇧⌘E / Ctrl+Shift+E | Send review (your comments, with your name) |

Annotations are saved automatically in `~/.pdf-review/`. The PDF itself is never changed.

## Reviewing a document someone sent you

1. Keep the PDF and its review list together in one folder: `Policy.pdf` and
   `Policy.review.json`. Opening the PDF loads the list into the **Review** tab, with numbered
   badges on the pages. **Tab** goes to the next item, **x** ticks it off.
2. Highlight, underline and write notes as you like.
3. Click **Send review** (⇧⌘E / Ctrl+Shift+E). The first time, it asks for your name. It saves
   `Policy - review - Your Name.json` (plus a readable `.md`) in **Downloads** and shows it.
   Attach the `.json` to an email back.

## Collecting reviews

Open each `… - review - Name.json` you get back with PDF Review (or drop it in the
[inbox](#command-line)) while the PDF is open. Everyone's comments appear on the pages, tagged
with their name, and the sidebar filters by person. A newer review from the same person
replaces their earlier one. **Export** gives one Markdown list with names.

## Having an AI review a document

An AI agent (Claude Code, Codex, or similar) never edits the PDF. It sends one of two files,
both pinned to quoted text (formats in [REVIEW_LISTS.md](REVIEW_LISTS.md)):

- a **review list**: places *you* must check or decide, a checklist in the **Review** tab;
- **reading notes**: the agent's own comments, shown as highlights with notes, tagged with its
  name like any reviewer's, in the **Annotations** tab.

How the agent should work:

1. **Read the text the way the app sees it**, so quotes match:
   `pdf-review --text file.pdf` (see [Command line](#command-line) for where the binary is).
2. **Find what a human must decide or check**: contradictions, missing owners or dates, vague
   requirements, numbers that disagree between sections, open TODOs, things that need sign-off.
   Skip typos and style unless asked.
3. **One item per place.** `title` says what to do in a few words, `detail` says why. Set
   `priority` (`high` for blocking issues).
4. **Quote verbatim**: half a sentence copied from the `--text` output, never paraphrased, and
   set `page` too.
5. **Deliver it**: `pdf-review --send list.json` checks the file and puts it in the inbox, on
   Mac and Windows alike. Or save it as
   `<pdf name>.review.json` next to the PDF when sending to someone else.

A prompt you can give the agent:

> Review `/path/to/file.pdf` with PDF Review. Read `REVIEW_LISTS.md` in the pdf-review repo,
> extract the text with `pdf-review --text`, and write a review list of the places I need to
> check or decide, quoting the text verbatim. Deliver it with `pdf-review --send`.

For reading notes, the same steps apply, but step 3 is one note per passage that explains,
summarises or flags something, and the file is a review with `notes`:

> Read `/path/to/file.pdf` and push reading notes to PDF Review as "Claude", following the
> reading notes section of `REVIEW_LISTS.md`. Quote verbatim from `pdf-review --text`.

Sending notes again under the same name replaces that agent's earlier notes.

## Command line

The app binary also works as a command-line tool:

| Command | Does |
|---|---|
| `pdf-review --text file.pdf` | Print each page's text exactly as the app searches it |
| `pdf-review --send file.json` | Check a review list or reading notes and put it in the inbox |
| `pdf-review --inbox` | Print the inbox folder |

Where the binary is:

- **Mac:** `"/Applications/PDF Review.app/Contents/MacOS/pdf-review"` (or under
  `~/Applications`). To call it as `pdf-review`:
  `ln -s "/Applications/PDF Review.app/Contents/MacOS/pdf-review" /opt/homebrew/bin/pdf-review`
- **Windows:** `"PDF Review.exe"` in the folder you unzipped, for example
  `& "$HOME\Documents\PDF-Review\PDF Review.exe" --inbox` in PowerShell.

The inbox is `~/.pdf-review/inbox/` on Mac and `%USERPROFILE%\.pdf-review\inbox\` on Windows.
Files can be copied there directly too; `--send` just checks them first and writes safely.

## Optional: ask Owl about a passage

PDF Review can send a passage and a question to [Owl](https://github.com/OlofMoriya/owl), an
AI assistant CLI (local via Ollama, or external via Codex), and save the answer as a note.
It is off, and hidden, unless you set it up:

1. Install Owl by following its [README](https://github.com/OlofMoriya/owl#quick-start).
2. Add `"owl"` to `~/.pdf-review/settings.json`: `"auto"` finds `owl` on your PATH (and in
   `~/go/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`), or give the full path:

   ```json
   { "owl": "auto" }
   ```

3. Restart PDF Review. An **Ask** tool (`o`) appears in the toolbar.

## Build from source

```bash
cargo run --release -- path/to/file.pdf
sh scripts/bundle.sh   # Mac: builds ~/Applications/PDF Review.app
```

Releases are built by GitHub Actions when a `v*` tag is pushed.

## License

[AGPL-3.0-or-later](LICENSE), as required by MuPDF.
