//! Review lists: agents drop a JSON file in ~/.pdf-review/inbox/ naming a document and
//! places in it to look at. Places are anchored by quoting the document's text (agents
//! can read a PDF's text but not its layout); `page` is a hint and a fallback.
//! See REVIEW_LISTS.md for the format.

use serde::{Deserialize, Serialize};

/// The file an agent writes.
#[derive(Deserialize, Debug)]
pub struct Incoming {
    /// Absolute path to the PDF, or just its file name.
    pub document: String,
    /// Content hash of the PDF (`pdf-review --hash`), so a renamed copy still matches.
    #[serde(default)]
    pub document_hash: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// Who sent it ("claude", "owl", ...).
    #[serde(default)]
    pub from: Option<String>,
    pub items: Vec<IncomingItem>,
}

#[derive(Deserialize, Debug)]
pub struct IncomingItem {
    /// Short instruction, e.g. "Check who owns the SOC decision".
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// Text copied from the document to anchor the item. Optional; whitespace-insensitive.
    #[serde(default)]
    pub quote: Option<String>,
    /// 1-based page; used to pick between repeated quotes, or alone when there is no quote.
    #[serde(default)]
    pub page: Option<usize>,
    /// "high" | "medium" | "low"
    #[serde(default)]
    pub priority: Option<String>,
}

/// A review item as stored with the document's annotations.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Task {
    pub id: String,
    pub list: String,
    pub from: String,
    pub received: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quote: String,
    #[serde(default)]
    pub page: Option<usize>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub priority: String,
    #[serde(default)]
    pub done: bool,
}

pub fn parse(json: &str) -> Result<Incoming, String> {
    let list: Incoming = serde_json::from_str(json).map_err(|e| format!("not a valid review list: {e}"))?;
    if list.document.trim().is_empty() {
        return Err("\"document\" is empty".into());
    }
    if list.items.is_empty() {
        return Err("\"items\" is empty".into());
    }
    if let Some((i, _)) = list.items.iter().enumerate().find(|(_, it)| it.title.trim().is_empty()) {
        return Err(format!("item {} has no title", i + 1));
    }
    Ok(list)
}

/// FNV-1a 64 of the file's bytes: the key the app stores a document's annotations under.
pub fn file_hash(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:016x}", bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))))
}

/// Does a list's or review's document refer to the open PDF? The content hash wins when
/// either side has one, so renamed and copied PDFs match. Otherwise full paths must match
/// (after resolving symlinks) or have the same content; a bare file name matches
/// case-insensitively.
pub fn matches_document(doc: &str, doc_hash: Option<&str>, open: &std::path::Path, open_hash: Option<&str>) -> bool {
    let doc = doc.trim();
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    if let Some(h) = open_hash {
        if doc_hash.is_some_and(|d| same(d, h)) || same(doc, h) {
            return true;
        }
    }
    let expanded = match doc.strip_prefix("~/") {
        Some(rest) => std::env::home_dir().unwrap_or_default().join(rest),
        None => std::path::PathBuf::from(doc),
    };
    if expanded.is_absolute() {
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        return canon(&expanded) == canon(open) || (open_hash.is_some() && expanded.is_file() && file_hash(&expanded).as_deref() == open_hash);
    }
    open.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(doc))
}

/// Where a list's `document` points, if it is a path to an existing file.
pub fn document_path(doc: &str) -> Option<std::path::PathBuf> {
    let doc = doc.trim();
    let p = match doc.strip_prefix("~/") {
        Some(rest) => std::env::home_dir()?.join(rest),
        None => std::path::PathBuf::from(doc),
    };
    (p.is_absolute() && p.is_file()).then_some(p)
}

pub fn into_tasks(list: Incoming, received: u64, new_id: impl Fn() -> String) -> Vec<Task> {
    let name = list.title.unwrap_or_else(|| "Review list".into());
    let from = list.from.unwrap_or_else(|| "agent".into());
    list.items
        .into_iter()
        .map(|it| Task {
            id: new_id(),
            list: name.clone(),
            from: from.clone(),
            received,
            title: it.title.trim().to_string(),
            detail: it.detail.unwrap_or_default().trim().to_string(),
            quote: it.quote.unwrap_or_default().trim().to_string(),
            page: it.page.filter(|p| *p > 0),
            priority: it.priority.unwrap_or_default().trim().to_lowercase(),
            done: false,
        })
        .collect()
}

/// Find a task's quote in the pages' search text. Exact (case-sensitive) matches win,
/// then case-insensitive; among several, the one nearest the page hint.
/// Returns (1-based page, start, end) indices into that page's text.
pub fn locate(pages: &[&[char]], quote: &str, page_hint: Option<usize>) -> Option<(usize, usize, usize)> {
    if quote.split_whitespace().next().is_none() {
        return None;
    }
    for exact in [true, false] {
        let mut hits = vec![];
        for (i, text) in pages.iter().enumerate() {
            for (a, b) in crate::search::find_with(text, quote, exact) {
                hits.push((i + 1, a, b));
            }
        }
        if let Some(best) = hits.into_iter().min_by_key(|(p, _, _)| page_hint.map(|h| p.abs_diff(h)).unwrap_or(0)) {
            return Some(best);
        }
    }
    None
}

/// How a quote anchors, for `pdf-review --check`: (fine?, "ok p14" / "not found" /
/// "found on p16, not p14" / "ambiguous: p7, p9"). Mirrors `locate`: exact-case hits win.
pub fn check(pages: &[&[char]], quote: &str, hint: Option<usize>) -> (bool, String) {
    if quote.split_whitespace().next().is_none() {
        return match hint {
            Some(p) if p >= 1 && p <= pages.len() => (true, format!("page only p{p}")),
            Some(p) => (false, format!("no quote, and p{p} is past the last page ({})", pages.len())),
            None => (false, "no quote and no page".into()),
        };
    }
    let hits = |exact: bool| -> Vec<usize> {
        pages.iter().enumerate().flat_map(|(i, t)| crate::search::find_with(t, quote, exact).into_iter().map(move |_| i + 1)).collect()
    };
    let mut on = hits(true);
    if on.is_empty() {
        on = hits(false);
    }
    let list = |v: &[usize]| v.iter().map(|p| format!("p{p}")).collect::<Vec<_>>().join(", ");
    match (on.as_slice(), hint) {
        ([], _) => (false, "not found".into()),
        ([p], None) => (true, format!("ok p{p}")),
        (_, None) => (false, format!("ambiguous: {}", list(&on))),
        (_, Some(h)) => match on.iter().filter(|p| **p == h).count() {
            0 => (false, format!("found on {}, not p{h}", list(&on))),
            1 => (true, format!("ok p{h}")),
            _ => (false, format!("ambiguous: {}", list(&on))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_validates() {
        let ok = r#"{"document":"policy.pdf","title":"Pass 1","from":"claude",
            "items":[{"title":"Check SOC","quote":"needs a SOC","page":16,"priority":"High"}]}"#;
        let list = parse(ok).unwrap();
        let tasks = into_tasks(list, 7, || "id".into());
        assert_eq!(tasks.len(), 1);
        assert_eq!((tasks[0].list.as_str(), tasks[0].from.as_str(), tasks[0].priority.as_str()), ("Pass 1", "claude", "high"));
        assert_eq!(tasks[0].page, Some(16));

        assert!(parse(r#"{"document":"","items":[{"title":"x"}]}"#).is_err());
        assert!(parse(r#"{"document":"a.pdf","items":[]}"#).is_err());
        assert!(parse(r#"{"document":"a.pdf","items":[{"title":"  "}]}"#).unwrap_err().contains("item 1"));
        assert!(parse("not json").is_err());
    }

    #[test]
    fn document_matching() {
        let dir = std::env::temp_dir().join(format!("pdf-review-tasks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("Policy Draft.pdf");
        std::fs::write(&pdf, b"%PDF").unwrap();
        let m = |doc: &str| matches_document(doc, None, &pdf, None);
        assert!(m(pdf.to_str().unwrap()));
        assert!(m("policy draft.PDF"));
        assert!(!m("other.pdf"));
        assert!(!m("/elsewhere/Policy Draft.pdf"), "full paths must match exactly");
        // by content: a renamed copy, a hash in `document_hash`, or a hash as `document`
        let h = file_hash(&pdf).unwrap();
        let copy = dir.join("Policy Draft (1).pdf");
        std::fs::write(&copy, b"%PDF").unwrap();
        assert!(matches_document(pdf.to_str().unwrap(), None, &copy, Some(&h)));
        assert!(matches_document("Policy Draft.pdf", Some(&h), &copy, Some(&h)));
        assert!(matches_document(&h, None, &copy, Some(&h)));
        assert!(!matches_document("Policy Draft.pdf", Some("0000000000000000"), &copy, Some(&h)));
        assert_eq!(document_path(pdf.to_str().unwrap()), Some(pdf.clone()));
        assert_eq!(document_path("Policy Draft.pdf"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn checks_quotes() {
        let p1: Vec<char> = "Alarm goes off. Doors lock.".chars().collect();
        let p2: Vec<char> = "Doors lock at night.".chars().collect();
        let pages: Vec<&[char]> = vec![&p1, &p2];
        assert_eq!(check(&pages, "Alarm goes", None), (true, "ok p1".into()));
        assert_eq!(check(&pages, "Alarm goes", Some(2)), (false, "found on p1, not p2".into()));
        assert_eq!(check(&pages, "Doors lock", None), (false, "ambiguous: p1, p2".into()));
        assert_eq!(check(&pages, "Doors lock", Some(2)), (true, "ok p2".into()));
        assert_eq!(check(&pages, "nowhere", Some(1)), (false, "not found".into()));
        assert_eq!(check(&pages, "", Some(2)), (true, "page only p2".into()));
        assert_eq!(check(&pages, "", None).0, false);
    }

    #[test]
    fn locates_quotes() {
        let p1: Vec<char> = "Intro. The SOC decision\nis open.".chars().collect();
        let p2: Vec<char> = "the soc decision is open here too".chars().collect();
        let pages: Vec<&[char]> = vec![&p1, &p2];
        // whitespace-insensitive, exact case first
        assert_eq!(locate(&pages, "SOC  decision is", None).map(|h| h.0), Some(1));
        // falls back to case-insensitive, nearest the page hint
        assert_eq!(locate(&pages, "The Soc Decision", Some(2)).map(|h| h.0), Some(2));
        assert_eq!(locate(&pages, "the soc decision", Some(1)).map(|h| h.0), Some(2), "exact-case match beats hint");
        assert_eq!(locate(&pages, "missing text", None), None);
        assert_eq!(locate(&pages, "   ", None), None);
    }
}
