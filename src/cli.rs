//! Command-line use, for agents and scripts (see REVIEW_LISTS.md).

use crate::{inbox_dir, load_pages, review, store_dir, tasks, Ann, SaveFile};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage:
  pdf-review --text <file.pdf>                     print each page's text as the app searches it
  pdf-review --check <file.json> [--pdf <file>]    anchor every quote without sending
  pdf-review --send <file.json> [--pdf <file>]     check, then put it in the inbox
  pdf-review --inbox [--status]                    the inbox folder, or what is waiting/imported/rejected
  pdf-review --annotations <file.pdf> [--author <name>] [--json]
                                                   the annotations stored for a PDF
  pdf-review --hash <file.pdf>                     the content hash a list can name as document_hash";

/// Runs one command; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let result = match (args[0].as_str(), args.get(1).filter(|a| !a.starts_with("--"))) {
        ("--text", Some(file)) => text(Path::new(file)),
        ("--check", Some(file)) => check(Path::new(file), value("--pdf").as_deref()).map(|ok| if ok { 0 } else { 1 }),
        ("--send", Some(file)) => send(Path::new(file), value("--pdf").as_deref()).map(|_| 0),
        ("--inbox", None) if flag("--status") => inbox_status().map(|_| 0),
        ("--inbox", None) => {
            println!("{}", inbox_dir().display());
            Ok(0)
        }
        ("--annotations", Some(file)) => annotations(Path::new(file), value("--author").as_deref(), flag("--json")).map(|_| 0),
        ("--hash", Some(file)) => tasks::file_hash(Path::new(file)).map(|h| println!("{h}")).map(|_| 0).ok_or(format!("could not read {file}")),
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    result.unwrap_or_else(|e| {
        eprintln!("{e}");
        1
    })
}

fn text(file: &Path) -> Result<i32, String> {
    let pages = load_pages(&file.to_path_buf()).map_err(|e| format!("could not read {}: {e}", file.display()))?;
    for (i, pv) in pages.iter().enumerate() {
        println!("=== page {} ===", i + 1);
        println!("{}", pv.flat.iter().collect::<String>());
    }
    Ok(0)
}

/// A review list or sent review, reduced to what the CLI needs.
struct Parsed {
    document: String,
    /// (quote, page, label) for every item or note.
    anchors: Vec<(String, Option<usize>, String)>,
}

fn parse(json: &str) -> Result<Parsed, String> {
    if review::is_review(json) {
        let sent: review::Sent<Ann> = serde_json::from_str(json).map_err(|e| format!("not a valid review: {e}"))?;
        if sent.reviewer.trim().is_empty() {
            return Err("\"reviewer\" is empty".into());
        }
        let anchors = sent.notes.into_iter().map(|n| (n.quote, n.page, n.note)).collect();
        Ok(Parsed { document: sent.document, anchors })
    } else {
        let list = tasks::parse(json)?;
        let anchors = list.items.into_iter().map(|it| (it.quote.unwrap_or_default(), it.page, it.title)).collect();
        Ok(Parsed { document: list.document, anchors })
    }
}

/// The PDF a file is about: `--pdf`, else `document` as a path, else that name next to the
/// JSON file or in the current folder.
fn find_pdf(doc: &str, json_file: &Path, explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(PathBuf::from(p));
    }
    tasks::document_path(doc).or_else(|| {
        let name = Path::new(doc.trim()).file_name()?;
        let beside = json_file.parent().map(|d| d.join(name));
        [beside, Some(PathBuf::from(name))].into_iter().flatten().find(|p| p.is_file())
    })
}

/// Print one line per anchor; true when every quote anchors cleanly.
fn report(parsed: &Parsed, pdf: &Path) -> Result<bool, String> {
    let pages = load_pages(&pdf.to_path_buf()).map_err(|e| format!("could not read {}: {e}", pdf.display()))?;
    let flats: Vec<&[char]> = pages.iter().map(|p| p.flat.as_slice()).collect();
    let mut all = true;
    for (i, (quote, page, label)) in parsed.anchors.iter().enumerate() {
        let (ok, what) = tasks::check(&flats, quote, *page);
        all &= ok;
        let label: String = label.lines().next().unwrap_or("").chars().take(60).collect();
        println!("{:>3}  {}{what:<24}  {label}", i + 1, if ok { "  " } else { "! " });
    }
    Ok(all)
}

fn check(file: &Path, pdf: Option<&str>) -> Result<bool, String> {
    let json = std::fs::read_to_string(file).map_err(|e| format!("could not read {}: {e}", file.display()))?;
    let parsed = parse(&json)?;
    let pdf = find_pdf(&parsed.document, file, pdf).ok_or(format!("can't find the PDF \"{}\"; pass --pdf <file>", parsed.document))?;
    report(&parsed, &pdf)
}

/// Validate, check quotes when the PDF can be found, stamp the PDF's content hash so a
/// renamed copy still matches, and move the file into the inbox atomically.
fn send(file: &Path, pdf: Option<&str>) -> Result<(), String> {
    let json = std::fs::read_to_string(file).map_err(|e| format!("could not read {}: {e}", file.display()))?;
    let parsed = parse(&json)?;
    let mut out = json.clone();
    match find_pdf(&parsed.document, file, pdf) {
        Some(pdf) => {
            report(&parsed, &pdf)?;
            if let (Some(hash), Ok(mut v)) = (tasks::file_hash(&pdf), serde_json::from_str::<serde_json::Value>(&json)) {
                v["document_hash"] = hash.into();
                out = serde_json::to_string_pretty(&v).unwrap_or(json);
            }
        }
        None => eprintln!("(quotes not checked: can't find \"{}\"; pass --pdf <file>)", parsed.document),
    }
    let dir = inbox_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stem = file.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let now = crate::now_secs();
    // two sends in the same second must not overwrite each other
    let name = (1..).map(|n| if n == 1 { format!("{now}-{stem}.json") } else { format!("{now}-{stem}-{n}.json") }).find(|n| !dir.join(n).exists()).unwrap();
    let tmp = dir.join(format!("{name}.part"));
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    let dest = dir.join(name);
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    println!("sent: {}", dest.display());
    Ok(())
}

/// What a JSON file in the inbox is, in a few words.
fn describe(path: &Path) -> String {
    let Ok(json) = std::fs::read_to_string(path) else { return "unreadable".into() };
    let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("?").to_string();
    if review::is_review(&json) {
        let n = v.get("notes").and_then(|x| x.as_array()).map_or(0, |a| a.len()) + v.get("annotations").and_then(|x| x.as_array()).map_or(0, |a| a.len());
        format!("review from {} for {}, {n} notes", s("reviewer"), s("document"))
    } else {
        let n = v.get("items").and_then(|x| x.as_array()).map_or(0, |a| a.len());
        format!("list \"{}\" from {} for {}, {n} items", v.get("title").and_then(|x| x.as_str()).unwrap_or("Review list"), v.get("from").and_then(|x| x.as_str()).unwrap_or("agent"), s("document"))
    }
}

fn inbox_status() -> Result<(), String> {
    let dir = inbox_dir();
    let jsons = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d).map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect()).unwrap_or_default();
        v.sort();
        v.reverse(); // newest first: names start with the time they were filed
        v
    };
    let when = |p: &Path| {
        let secs: u64 = p.file_name().and_then(|n| n.to_str()?.split('-').next()?.parse().ok()).unwrap_or(0);
        crate::ago(crate::now_secs().saturating_sub(secs))
    };
    println!("{}", dir.display());
    let waiting = jsons(&dir);
    println!("\nWaiting ({}): imported when the app has the document open", waiting.len());
    for p in &waiting {
        println!("  {}  {}", p.file_name().unwrap_or_default().to_string_lossy(), describe(p));
    }
    for (sub, title) in [("imported", "Imported"), ("rejected", "Rejected")] {
        let files = jsons(&dir.join(sub));
        println!("\n{title} ({}{})", files.len(), if files.len() > 10 { ", newest 10" } else { "" });
        for p in files.iter().take(10) {
            println!("  {}  {}  {}", when(p), p.file_name().unwrap_or_default().to_string_lossy(), describe(p));
            if let Ok(r) = std::fs::read_to_string(p.with_file_name(format!("{}.reason.txt", p.file_name().unwrap_or_default().to_string_lossy()))) {
                println!("      reason: {}", r.trim());
            }
        }
    }
    Ok(())
}

fn annotations(pdf: &Path, author: Option<&str>, json: bool) -> Result<(), String> {
    let hash = tasks::file_hash(pdf).ok_or(format!("could not read {}", pdf.display()))?;
    let saved = std::fs::read_to_string(store_dir().join(format!("{hash}.json")))
        .or_else(|_| std::fs::read_to_string(format!("{}.annotations.json", pdf.display())))
        .map_err(|_| format!("no annotations stored for {}", pdf.display()))?;
    let file: SaveFile = serde_json::from_str(&saved).map_err(|e| e.to_string())?;
    // "me" (or "") is you; anything else matches a reviewer's name exactly.
    let want = author.map(|a| if a.eq_ignore_ascii_case("me") { "" } else { a });
    let mut anns: Vec<&Ann> = file.annotations.iter().filter(|a| want.is_none_or(|w| a.author == w)).collect();
    anns.sort_by(|a, b| (a.page, a.rect.y as i32).cmp(&(b.page, b.rect.y as i32)));
    if json {
        let out: Vec<serde_json::Value> = anns
            .iter()
            .map(|a| serde_json::json!({ "page": a.page, "kind": a.kind, "author": a.author, "text": a.text, "note": a.note }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        for a in anns {
            let who = if a.author.is_empty() { "me" } else { &a.author };
            let quote = if a.text.trim().is_empty() { String::new() } else { format!(" \"{}\"", a.text.split_whitespace().collect::<Vec<_>>().join(" ")) };
            let note = if a.note.trim().is_empty() { String::new() } else { format!(": {}", a.note.trim()) };
            println!("p{} {} [{}]{quote}{note}", a.page, who, a.kind.label());
        }
    }
    Ok(())
}
