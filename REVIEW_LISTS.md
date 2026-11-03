# Review lists: sending places to look at

An agent can hand the reviewer a checklist of places in a PDF. Write one JSON file into
`~/.pdf-review/inbox/`. PDF Review picks it up within a few seconds, shows it in the sidebar's
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
| `document` | yes | Absolute path to the PDF (preferred), `~/...`, or just its file name. |
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
"$HOME/Applications/PDF Review.app/Contents/MacOS/pdf-review" --text /path/to/file.pdf
```

Quote from this output. Other extractors may render some glyphs differently, and then the
quote would not match.

## How items find their place

- The `quote` is searched in the document's text. Whitespace and line breaks don't matter, so
  a quote can span lines. An exact-case match is preferred, then any case.
- When the quote appears several times, the match nearest `page` wins.
- Keep quotes short and distinctive: half a sentence copied verbatim is ideal. Don't paraphrase.
- If the quote is not found, the item falls back to `page` and is marked "quote not found".

## Delivery rules

- To ship a list with the PDF instead (for example to people without an inbox), save it as
  `<pdf name>.review.json` in the same folder as the PDF. It is imported when the PDF opens.

- Write the file under a temporary name first and rename it to `*.json` when complete,
  or just write it in one go. Files younger than about a second are left alone, so a
  half-written file is not read.
- If no document is open and `document` is an existing absolute path, PDF Review opens it.
- Lists for a document that is not open wait in the inbox until it is opened.
- Imported files move to `~/.pdf-review/inbox/imported/`, and invalid ones to `inbox/rejected/`
  (the reason shows in the app's footer).

## For the reviewer

- **Tab / ⇧Tab** (or `]` / `[`): next or previous open item.
- **x**: mark the current item done and go to the next one.
- **R**: switch between the Annotations and Review tabs.
- Done items, and which items arrived, are saved with the document's annotations, and
  "Export" includes them as a Markdown checklist.
