# Review lists: sending places to look at

An agent can hand the reviewer a checklist of places in a PDF. Write one JSON file and run
`pdf-review --send list.json`, or write it straight into the inbox: `~/.pdf-review/inbox/` on Mac,
`%USERPROFILE%\.pdf-review\inbox\` on Windows (`pdf-review --inbox` prints it). PDF Review picks it up within a few seconds, shows it in the sidebar's
**Review** tab, and pins each item to the page with a numbered badge.

## Format

```json
{
  "document": "/Users/me/Downloads/security-policy.pdf",
  "title": "Security policy: first pass",
  "from": "claude",
  "items": [
    {
      "title": "Decide who owns the logging question",
      "detail": "The TODO has no owner or date.",
      "quote": "Decide whether central logging is needed",
      "page": 16,
      "priority": "high"
    },
    {
      "title": "Check the 72-hour breach deadline matches 5.2.2",
      "quote": "Breaches are reported within 72 hours",
      "priority": "medium"
    }
  ]
}
```

| Field | Required | Meaning |
|---|---|---|
| `document` | yes | Absolute path to the PDF (preferred), `~/...`, just its file name, or its content hash. |
| `document_hash` | no | The PDF's content hash (`pdf-review --hash`). Matches a renamed or copied PDF. `--send` adds it for you. |
| `title` | no | Name of the list. Default "Review list". |
| `from` | no | Who sent it, shown next to the list. Default "agent". |
| `items[].title` | yes | What to do, in a few words. |
| `items[].detail` | no | Why, or what to check. |
| `items[].quote` | no | Text copied from the document. This is how an item finds its place. |
| `items[].page` | no | 1-based page. Picks between repeated quotes, or places the item when there is no quote. |
| `items[].priority` | no | `high`, `medium` or `low`. |

## Getting quotable text

Print the text exactly as the app searches it, page by page:

```bash
"/Applications/PDF Review.app/Contents/MacOS/pdf-review" --text /path/to/file.pdf   # Mac
& "C:\path\to\PDF-Review\pdf-review.exe" --text C:\path\to\file.pdf          # Windows
```

Quote from this output. Other extractors may render some glyphs differently, and then the
quote would not match.

## How items find their place

- The `quote` is searched in the document's text. Whitespace and line breaks don't matter, so
  a quote can span lines. An exact-case match is preferred, then any case.
- When the quote appears several times, the match nearest `page` wins.
- Keep quotes short and distinctive: half a sentence copied verbatim is ideal. Don't paraphrase.
- Look-alike characters match each other: curly and straight quotes, the hyphens and dashes
  (‐ ‑ – — −) and non-breaking spaces. Ligatures and soft hyphens do not.
- If the quote is not found, the item falls back to `page` and is marked "quote not found".

Check before sending: `pdf-review --check list.json --pdf file.pdf` prints one line per item
(`ok p14`, `not found`, `found on p16, not p14`, `ambiguous: p7, p9`, `page only p3`) and exits
with 1 if any line is marked `!`. `--send` runs the same check whenever it can find the PDF.

## Delivery rules

- To ship a list with the PDF instead (for example to people without an inbox), save it as
  `<pdf name>.review.json` in the same folder as the PDF. It is imported when the PDF opens.

- Write the file under a temporary name first and rename it to `*.json` when complete,
  or just write it in one go. Files younger than about a second are left alone, so a
  half-written file is not read.
- If no document is open and `document` is an existing absolute path, PDF Review opens it.
- Lists for a document that is not open wait in the inbox until it is opened.
- Sending a list again with the same `title` and `from` replaces the earlier copy. Items with
  the same title keep their done state. Use a new `title` for a separate list.
- `pdf-review --inbox --status` shows what is waiting, imported and rejected, with the reason.
- Imported files move to `~/.pdf-review/inbox/imported/`, and invalid ones to `inbox/rejected/`
  (the reason shows in the app's footer).

## Reading notes: the agent's own comments

A review list asks the reader to do something. To leave comments instead (summaries,
explanations, flags), send a review with `notes`. Each note becomes a highlight on its quote
with the note attached, tagged with `reviewer`, exactly like a person's sent review.

```json
{
  "format": "pdf-review/review",
  "document": "/Users/me/Downloads/security-policy.pdf",
  "reviewer": "Claude",
  "notes": [
    { "quote": "Breaches are reported within 72 hours", "page": 12, "note": "Matches GDPR art. 33, but 5.2.2 says 48 hours." },
    { "page": 3, "note": "Section 3 in short: who owns which system." }
  ]
}
```

| Field | Required | Meaning |
|---|---|---|
| `format` | yes | Always `pdf-review/review`. |
| `document` | yes | As for review lists. |
| `reviewer` | yes | Name the notes are tagged with. A new file with the same name replaces that reviewer's notes. |
| `notes[].note` | yes | The comment. |
| `notes[].quote` | no | Verbatim text to highlight, found the same way as review list quotes. |
| `notes[].page` | no | Picks between repeated quotes. Without a quote, or if it is not found, the note becomes a sticky note at the top left of this page (marked "quote not found"). |

Deliver it with `pdf-review --send notes.json` (or write it into the inbox). It is imported when the document is open, otherwise it
waits. Keep notes few and useful: one per passage worth a comment, not one per paragraph.

- Sending again with the same `reviewer` replaces that reviewer's earlier notes on the document,
  so fix and re-send freely. Use a new `reviewer` name to add a separate set.
- To ship notes with the PDF, put the JSON in the same folder (any name, for example
  `Policy.notes.json`). It is imported when the PDF is opened, once: removing those notes in
  the app does not bring them back, but a changed file is imported again.

## For the reviewer

- **Tab / ⇧Tab** (or `]` / `[`): next or previous open item.
- **x**: mark the current item done and go to the next one.
- **R**: switch between the Annotations and Review tabs.
- **Remove** next to a list's name deletes the whole list (click twice).
- In the Annotations tab, each reviewer chip shows or hides that person's annotations (on the
  pages and in the list). ⌥-click shows only that person, **All** shows everyone. This is saved
  with the document. With only one other reviewer showing, **Remove <name>'s annotations**
  deletes theirs (⌘Z undoes it).
- Done items, and which items arrived, are saved with the document's annotations, and
  "Export" includes them as a Markdown checklist.
