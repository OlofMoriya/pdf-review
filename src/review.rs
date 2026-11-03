//! Reviews travel as files. A reviewer's "Send review" writes their annotations, tagged with
//! their name, to a JSON file they email back; the owner drops it in the inbox (or opens it)
//! and sees everyone's comments in one document. A review list can also ship next to the PDF
//! as `<name>.review.json`, so a package of PDF + list + app needs no setup.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FORMAT: &str = "pdf-review/review";

/// What "Send review" writes. Generic over the annotation type so this module stays UI-free.
#[derive(Serialize, Deserialize)]
pub struct Sent<A> {
    pub format: String,
    pub document: String,
    pub reviewer: String,
    pub sent: u64,
    pub annotations: Vec<A>,
    /// Review-list items and whether the reviewer ticked them off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ItemState>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ItemState {
    pub title: String,
    pub done: bool,
}

/// Is this JSON a sent review (as opposed to a review list)?
pub fn is_review(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json).ok().and_then(|v| v.get("format")?.as_str().map(|f| f == FORMAT)).unwrap_or(false)
}

/// "Policy v2 - review - Anna Svensson.json", safe on macOS and Windows.
pub fn file_name(pdf: &Path, reviewer: &str) -> String {
    let stem = pdf.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "document".into());
    let clean = |s: &str| s.chars().map(|c| if r#"/\:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect::<String>().trim().to_string();
    format!("{} - review - {}.json", clean(&stem), clean(reviewer))
}

/// A review list shipped beside the PDF: `Policy v2.pdf` → `Policy v2.review.json`.
pub fn list_beside(pdf: &Path) -> Option<PathBuf> {
    let p = pdf.with_extension("review.json");
    p.is_file().then_some(p)
}

/// The person's display name from the OS, to prefill the name prompt.
pub fn os_user_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("id").arg("-F").output() {
        let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !full.is_empty() {
            return full;
        }
    }
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default()
}

/// Where sent reviews go: Downloads (both OSes have it), else home.
pub fn out_dir() -> PathBuf {
    let home = std::env::home_dir().unwrap_or_default();
    let dl = home.join("Downloads");
    if dl.is_dir() { dl } else { home }
}

/// Show a file in Finder / Explorer.
pub fn reveal(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_detection() {
        assert_eq!(file_name(Path::new("/x/Policy v2.pdf"), "Anna Svensson"), "Policy v2 - review - Anna Svensson.json");
        assert_eq!(file_name(Path::new("a.pdf"), " A/B: C "), "a - review - A_B_ C.json");
        let sent = Sent::<u8> { format: FORMAT.into(), document: "a.pdf".into(), reviewer: "A".into(), sent: 1, annotations: vec![], items: vec![] };
        assert!(is_review(&serde_json::to_string(&sent).unwrap()));
        assert!(!is_review(r#"{"document":"a.pdf","items":[{"title":"x"}]}"#));
        assert!(!is_review("nope"));
    }

    #[test]
    fn finds_list_beside_pdf() {
        let dir = std::env::temp_dir().join(format!("pdf-review-beside-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("Policy v2.pdf");
        assert_eq!(list_beside(&pdf), None);
        std::fs::write(dir.join("Policy v2.review.json"), "{}").unwrap();
        assert_eq!(list_beside(&pdf), Some(dir.join("Policy v2.review.json")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
