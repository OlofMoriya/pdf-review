// No console window behind the app on Windows (release builds).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! pdf-review: a GPUI PDF viewer for reviewing documents.
//! Mark up pages (highlight, underline, strike, box, pen, pinned note), attach a note to
//! each mark, and export the review as Markdown. Annotations autosave to
//! `~/.pdf-review/annotations/<content-hash>.json`, so they follow the PDF across renames/moves.

mod owl;
mod review;
mod search;
mod tasks;
mod vim;

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, sync::Arc};

use gpui::{
    App, Application, BorderStyle, Bounds, ClipboardItem, Context, FocusHandle, FontWeight, Hsla,
    TitlebarOptions, WindowBackgroundAppearance,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder,
    PathPromptOptions, Pixels, Point, Render, RenderImage, SharedString, Window, WindowBounds,
    WindowOptions, DispatchPhase, anchored, deferred, ScrollDelta, ScrollHandle, ScrollWheelEvent, canvas, div, fill, img, outline, point, prelude::*, px, rgb, rgba, size,
};
use mupdf::{Colorspace, Document, Matrix, TextPageFlags};
use serde::{Deserialize, Serialize};

const RENDER_SCALE: f32 = 2.0; // raster density relative to PDF points
const PALETTE: [u32; 5] = [0xf5c542, 0x4caf50, 0x42a5f5, 0xef5350, 0xab47bc];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum Kind {
    /// Rectangle highlight (serialized as "highlight" for older review files).
    Highlight,
    /// Highlight that follows the text lines between two points.
    TextHighlight,
    Underline,
    Strike,
    Box,
    Pen,
    Note,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Highlight => "Box highlight",
            Kind::TextHighlight => "Highlight",
            Kind::Underline => "Underline",
            Kind::Strike => "Strike",
            Kind::Box => "Box",
            Kind::Pen => "Drawing",
            Kind::Note => "Note",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Select,
    Mark(Kind),
    /// Drag over text (or Alt+click a sentence) to ask Owl about it.
    Ask,
}

#[derive(Clone, Copy, PartialEq)]
enum SideTab {
    Annotations,
    Review,
}

fn inbox_dir() -> PathBuf {
    home().join(".pdf-review").join("inbox")
}

/// Move a processed inbox file into a subfolder, keeping it for reference.
fn file_away(path: &std::path::Path, sub: &str) {
    let dir = inbox_dir().join(sub);
    let _ = std::fs::create_dir_all(&dir);
    let name = format!("{}-{}", now_secs(), path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
    if std::fs::rename(path, dir.join(name)).is_err() {
        let _ = std::fs::remove_file(path);
    }
}

/// What an Owl question is about.
#[derive(Clone)]
enum AskSource {
    /// An existing annotation; a saved answer is appended to its note.
    Ann(String),
    /// A fresh selection; saving the answer creates a highlight with it as the note.
    Fresh { spans: Vec<R> },
}

struct AskState {
    source: AskSource,
    page: usize,
    quote: String,
    question: String,
    /// Byte offset of the cursor in `question`.
    caret: usize,
    external: bool,
    /// Question the current answer is for.
    asked: Option<String>,
    answer: String,
    error: Option<String>,
    running: Option<owl::Running>,
    /// Bumped per request so output from a cancelled run is ignored.
    generation: u64,
}

/// Rect in PDF points, top-left origin.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default)]
struct R {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl R {
    fn from_points(a: (f32, f32), b: (f32, f32)) -> Self {
        R { x: a.0.min(b.0), y: a.1.min(b.1), w: (a.0 - b.0).abs(), h: (a.1 - b.1).abs() }
    }
    fn contains(&self, p: (f32, f32), pad: f32) -> bool {
        p.0 >= self.x - pad && p.0 <= self.x + self.w + pad && p.1 >= self.y - pad && p.1 <= self.y + self.h + pad
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
struct Ann {
    id: String,
    page: usize, // 1-based
    kind: Kind,
    color: String,
    rect: R,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    points: Vec<(f32, f32)>,
    /// Per-line rects for text highlights.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    spans: Vec<R>,
    text: String,
    note: String,
    /// Who wrote it, for annotations imported from someone's sent review. Empty = you.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    author: String,
}

#[derive(Serialize, Deserialize)]
struct SaveFile {
    document: String,
    #[serde(default)]
    path: String, // last known location, for humans browsing ~/.pdf-review
    annotations: Vec<Ann>,
    /// Review items sent by agents (see REVIEW_LISTS.md).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tasks: Vec<tasks::Task>,
    /// Vim marks: letter -> (page, y in PDF points from the page top).
    #[serde(default)]
    marks: std::collections::BTreeMap<char, (usize, f32)>,
}

/// Per-user UI preferences, in ~/.pdf-review/settings.json.
#[derive(Serialize, Deserialize, Default)]
struct Settings {
    #[serde(default)]
    sidebar_w: Option<f32>,
    #[serde(default)]
    minimap: bool,
    /// Your name, put on reviews you send.
    #[serde(default)]
    name: Option<String>,
    /// Owl integration (https://github.com/OlofMoriya/owl). Off unless set: "auto" finds `owl`
    /// on PATH and in the usual install folders, anything else is the path to the binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owl: Option<String>,
}

fn settings_file() -> PathBuf {
    home().join(".pdf-review").join("settings.json")
}

fn load_settings() -> Settings {
    std::fs::read_to_string(settings_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

struct SearchState {
    query: String,
    /// Still typing the query (bottom bar is an input) vs. confirmed with Enter.
    typing: bool,
    matches: Vec<SearchHit>,
    cur: usize,
}

struct SearchHit {
    page: usize,
    spans: Vec<R>,
    /// Surrounding text for the results list.
    before: String,
    matched: String,
    after: String,
}

/// Text around a match, whitespace collapsed, cut at word boundaries.
fn context(flat: &[char], a: usize, b: usize) -> (String, String, String) {
    let squash = |cs: &[char]| cs.iter().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    let start = a.saturating_sub(48);
    let end = (b + 1 + 72).min(flat.len());
    let mut before = squash(&flat[start..a]);
    if start > 0 {
        before = before.split_once(' ').map(|(_, r)| format!("…{r}")).unwrap_or(before);
    }
    let mut after = squash(&flat[b + 1..end]);
    if end < flat.len() {
        after = after.rsplit_once(' ').map(|(l, _)| format!("{l}…")).unwrap_or(after);
    }
    let pad = |s: &str, left: bool| match (flat.get(if left { a.wrapping_sub(1) } else { b + 1 }), s.is_empty()) {
        (Some(c), false) if c.is_whitespace() => " ",
        _ => "",
    };
    (format!("{before}{}", pad(&before, true)), squash(&flat[a..=b]), format!("{}{after}", pad(&after, false)))
}

const MAX_UNDO: usize = 200;

struct TextLine {
    block: usize,
    bbox: R,
    chars: Vec<(char, f32, f32)>, // char, x0, x1
}

struct PageView {
    image: Arc<RenderImage>,
    w: f32,
    h: f32,
    lines: Vec<TextLine>,
    /// Page text for search, with the char position each entry came from.
    flat: Vec<char>,
    flat_pos: Vec<Pos>,
}

/// Search text for a page: lines joined with '\n', with spaces inserted where words
/// are positioned apart without a space glyph (same rule as `join_chars`).
fn flatten(lines: &[TextLine]) -> (Vec<char>, Vec<Pos>) {
    let (mut flat, mut pos) = (vec![], vec![]);
    for (li, l) in lines.iter().enumerate() {
        if l.chars.is_empty() {
            continue;
        }
        if !flat.is_empty() {
            flat.push('\n');
            pos.push((li, 0));
        }
        let mut prev: Option<&(char, f32, f32)> = None;
        for (ci, c) in l.chars.iter().enumerate() {
            if let Some(p) = prev {
                if !p.0.is_whitespace() && !c.0.is_whitespace() && c.1 - p.2 > l.bbox.h * 0.15 {
                    flat.push(' ');
                    pos.push((li, ci));
                }
            }
            flat.push(c.0);
            pos.push((li, ci));
            prev = Some(c);
        }
    }
    (flat, pos)
}

/// Position of a char on a page: (line index, char index).
type Pos = (usize, usize);

/// Chars to text, adding a space where the PDF positions words apart without a space glyph.
fn join_chars(chars: &[(char, f32, f32)], line_h: f32) -> String {
    let mut out = String::new();
    let mut prev: Option<&(char, f32, f32)> = None;
    for c in chars {
        if let Some(p) = prev {
            if !p.0.is_whitespace() && !c.0.is_whitespace() && c.1 - p.2 > line_h * 0.15 {
                out.push(' ');
            }
        }
        out.push(c.0);
        prev = Some(c);
    }
    out
}

fn is_term(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…')
}

impl PageView {
    /// Char closest to a point: the line under it (or the vertically closest one), then the
    /// char in that line closest by x.
    fn nearest_char(&self, p: (f32, f32)) -> Option<Pos> {
        let dist = |l: &TextLine| {
            let dy = if p.1 < l.bbox.y { l.bbox.y - p.1 } else if p.1 > l.bbox.y + l.bbox.h { p.1 - l.bbox.y - l.bbox.h } else { 0.0 };
            let dx = if p.0 < l.bbox.x { l.bbox.x - p.0 } else if p.0 > l.bbox.x + l.bbox.w { p.0 - l.bbox.x - l.bbox.w } else { 0.0 };
            dy * 4.0 + dx // prefer the line you are on over a horizontally closer one
        };
        let (li, line) = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.chars.is_empty())
            .min_by(|a, b| dist(a.1).total_cmp(&dist(b.1)))?;
        let ci = line
            .chars
            .iter()
            .enumerate()
            .min_by(|a, b| ((a.1.1 + a.1.2) / 2.0 - p.0).abs().total_cmp(&((b.1.1 + b.1.2) / 2.0 - p.0).abs()))?
            .0;
        Some((li, ci))
    }

    /// Per-line rects and text for the chars from `a` to `b` inclusive (any order).
    fn range(&self, a: Pos, b: Pos) -> (Vec<R>, String) {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut spans = vec![];
        let mut text = String::new();
        for li in a.0..=b.0 {
            let line = &self.lines[li];
            if line.chars.is_empty() {
                continue;
            }
            let from = if li == a.0 { a.1 } else { 0 };
            let to = if li == b.0 { b.1 } else { line.chars.len() - 1 };
            let chars = &line.chars[from..=to.min(line.chars.len() - 1)];
            let visible: Vec<_> = chars.iter().filter(|c| !c.0.is_whitespace()).collect();
            if let (Some(first), Some(last)) = (visible.first(), visible.last()) {
                spans.push(R { x: first.1, y: line.bbox.y, w: last.2 - first.1, h: line.bbox.h });
            }
            let piece = join_chars(chars, line.bbox.h);
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            if text.ends_with('-') {
                text.pop(); // re-join hyphenated words across lines
            } else if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(piece);
        }
        (spans, text.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    /// Bounds of the sentence containing `at`, limited to its text block (paragraph).
    fn sentence(&self, at: Pos) -> (Pos, Pos) {
        let block = self.lines[at.0].block;
        // Flatten the block, with a virtual space between lines.
        let mut flat: Vec<(Pos, char)> = vec![];
        for (li, l) in self.lines.iter().enumerate().filter(|(_, l)| l.block == block) {
            if !flat.is_empty() {
                flat.push(((li, 0), ' '));
            }
            flat.extend(l.chars.iter().enumerate().map(|(ci, c)| ((li, ci), c.0)));
        }
        let Some(i) = flat.iter().position(|(p, c)| *p == at && !(c.is_whitespace() && at.1 == 0 && flat.iter().filter(|x| x.0 == at).count() > 1)) else {
            return (at, at);
        };
        let mut s = i;
        while s > 0 && !(flat[s - 1].1.is_whitespace() && s >= 2 && is_term(flat[s - 2].1)) {
            s -= 1;
        }
        while s < i && flat[s].1.is_whitespace() {
            s += 1;
        }
        let mut e = i;
        while e + 1 < flat.len() && !(is_term(flat[e].1) && flat[e + 1].1.is_whitespace()) {
            e += 1;
        }
        (flat[s].0, flat[e].0)
    }
}

struct Drag {
    page: usize,
    start: (f32, f32),
    cur: (f32, f32),
    points: Vec<(f32, f32)>,
}

struct Reviewer {
    focus: FocusHandle,
    path: Option<PathBuf>,
    doc_hash: Option<String>,
    pages: Vec<PageView>,
    anns: Vec<Ann>,
    tool: Tool,
    tool_colors: HashMap<Kind, u32>,
    palette_open: bool,
    zoom: f32,
    selected: Option<String>,
    editing: bool,
    drag: Option<Drag>,
    page_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    /// Laid-out bounds of on-page note text (window coords), for clicking notes.
    note_bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    status: SharedString,
    scroll: ScrollHandle,
    mouse: Point<Pixels>,
    #[cfg(target_os = "macos")]
    _pinch_monitor: Option<objc2::rc::Retained<objc2::runtime::AnyObject>>,
    recent: Vec<Recent>,
    caret: usize, // byte offset into the selected note while editing
    list_scroll: ScrollHandle,
    minimap: bool,
    minimap_scroll: ScrollHandle,
    vim: vim::Vim,
    search: Option<SearchState>,
    search_scroll: ScrollHandle,
    /// Set only when Owl is configured in settings.json and found; every Owl feature checks it.
    owl_path: Option<PathBuf>,
    owl_setting: Option<String>,
    ask: Option<AskState>,
    marks: std::collections::BTreeMap<char, (usize, f32)>,
    last_jump: Option<(usize, f32)>,
    undo: Vec<Vec<Ann>>,
    redo: Vec<Vec<Ann>>,
    /// Annotation whose note edits are already covered by an undo checkpoint.
    edit_checkpoint: Option<String>,
    sidebar_w: Option<f32>,
    win_w: f32,
    tasks: Vec<tasks::Task>,
    /// Where each task points: page and the quote's line rects (empty = whole page).
    task_anchor: HashMap<String, (usize, Vec<R>)>,
    current_task: Option<String>,
    side_tab: SideTab,
    /// Review lists waiting in the inbox for a document that is not open.
    waiting: Vec<String>,
    resizing: Option<(Pixels, f32)>,
    last_page: usize,
    picker: Option<Picker>,
    picker_scroll: ScrollHandle,
    reviewer_name: Option<String>,
    /// "Your name" prompt, shown the first time you send a review.
    name_prompt: Option<String>,
    /// Sidebar filter: None = everyone, Some("") = you, Some(name) = that reviewer.
    author_filter: Option<String>,
}

/// ⌘ on macOS, Ctrl elsewhere. On Windows Ctrl+D/U/B stay Vim scrolls (half page down/up, page up).
fn shortcut(k: &gpui::Keystroke) -> bool {
    if cfg!(target_os = "macos") {
        k.modifiers.platform
    } else {
        k.modifiers.control && !matches!(k.key.as_str(), "d" | "u" | "b")
    }
}

const MIN_ZOOM: f32 = 0.5;
const MAX_ZOOM: f32 = 4.0;

#[cfg(target_os = "macos")]
/// GPUI has no pinch event on macOS, so tap NSEvent magnify events with a local monitor
/// and feed them to the view over a channel.
fn install_pinch(cx: &mut Context<Reviewer>) -> Option<objc2::rc::Retained<objc2::runtime::AnyObject>> {
    use futures::StreamExt;
    use objc2_app_kit::{NSEvent, NSEventMask};
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<f32>();
    let block = block2::RcBlock::new(move |ev: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
        let m = unsafe { ev.as_ref() }.magnification();
        tx.unbounded_send(m as f32).ok();
        ev.as_ptr()
    });
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::Magnify, &block) };
    cx.spawn(async move |this, cx| {
        while let Some(m) = rx.next().await {
            let alive = this.update(cx, |this, cx| {
                let anchor = this.mouse;
                this.zoom_to(this.zoom * (1.0 + m), Some(anchor));
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
    monitor
}

/// Caret editing on a plain String; carets are byte offsets on char boundaries.
mod note_edit {
    pub fn prev(s: &str, c: usize) -> usize {
        s[..c].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
    }
    pub fn next(s: &str, c: usize) -> usize {
        s[c..].chars().next().map(|ch| c + ch.len_utf8()).unwrap_or(c)
    }
    pub fn line_start(s: &str, c: usize) -> usize {
        s[..c].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }
    pub fn line_end(s: &str, c: usize) -> usize {
        s[c..].find('\n').map(|i| c + i).unwrap_or(s.len())
    }
    fn col(s: &str, c: usize) -> usize {
        s[line_start(s, c)..c].chars().count()
    }
    fn at_col(s: &str, start: usize, col: usize) -> usize {
        let end = line_end(s, start);
        s[start..end].char_indices().nth(col).map(|(i, _)| start + i).unwrap_or(end)
    }
    pub fn up(s: &str, c: usize) -> usize {
        let ls = line_start(s, c);
        if ls == 0 {
            return 0;
        }
        at_col(s, line_start(s, ls - 1), col(s, c))
    }
    pub fn down(s: &str, c: usize) -> usize {
        let le = line_end(s, c);
        if le == s.len() {
            return s.len();
        }
        at_col(s, le + 1, col(s, c))
    }
    pub fn word_left(s: &str, c: usize) -> usize {
        let mut i = c;
        while i > 0 && !s[..i].chars().next_back().unwrap().is_alphanumeric() {
            i = prev(s, i);
        }
        while i > 0 && s[..i].chars().next_back().unwrap().is_alphanumeric() {
            i = prev(s, i);
        }
        i
    }
    pub fn word_right(s: &str, c: usize) -> usize {
        let mut i = c;
        while i < s.len() && !s[i..].chars().next().unwrap().is_alphanumeric() {
            i = next(s, i);
        }
        while i < s.len() && s[i..].chars().next().unwrap().is_alphanumeric() {
            i = next(s, i);
        }
        i
    }
    pub fn insert(s: &mut String, c: &mut usize, t: &str) {
        s.insert_str(*c, t);
        *c += t.len();
    }
    /// Remove between the caret and `other` (either side); caret ends at the start.
    pub fn remove(s: &mut String, c: &mut usize, other: usize) {
        let (a, b) = if other < *c { (other, *c) } else { (*c, other) };
        s.replace_range(a..b, "");
        *c = a;
    }
}

/// On-page note text: size and wrap width in PDF points (scaled with zoom).
const NOTE_FONT: f32 = 10.0;
const NOTE_MAX_W: f32 = 240.0;

/// Text colour for a mark colour: light colours (yellow) are darkened to stay readable on paper.
fn ink(c: u32) -> Hsla {
    let (r, g, b) = ((c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff);
    let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if lum > 150.0 {
        let k = 0.6;
        let d = |v: u32| (v as f32 * k) as u32;
        rgb((d(r) << 16) | (d(g) << 8) | d(b)).into()
    } else {
        rgb(c).into()
    }
}

fn default_tool_colors() -> HashMap<Kind, u32> {
    use Kind::*;
    // yellow highlights, blue underline, red strike/box/pen, yellow notes
    [(TextHighlight, PALETTE[0]), (Highlight, PALETTE[0]), (Underline, PALETTE[2]), (Strike, PALETTE[3]), (Box, PALETTE[3]), (Pen, PALETTE[3]), (Note, PALETTE[3])]
        .into_iter()
        .collect()
}

const MAX_RECENT: usize = 30;

#[derive(Clone, Serialize, Deserialize)]
struct Recent {
    path: String,
    opened: u64, // unix seconds
}

#[derive(Default)]
struct Picker {
    query: String,
    sel: usize,
}

/// The user's home folder (HOME on macOS, the profile folder on Windows).
fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

fn recent_file() -> PathBuf {
    home().join(".pdf-review").join("recent.json")
}

fn load_recent() -> Vec<Recent> {
    std::fs::read_to_string(recent_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_recent(r: &[Recent]) {
    let f = recent_file();
    if let Some(dir) = f.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(f, serde_json::to_string_pretty(r).unwrap());
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn expand_tilde(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(s),
    }
}

fn tilde(s: &str) -> String {
    let h = home().display().to_string();
    match s.strip_prefix(&h) {
        Some(rest) if !h.is_empty() => format!("~{rest}"),
        _ => s.to_string(),
    }
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        86400..172800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86400),
    }
}

fn store_dir() -> PathBuf {
    home().join(".pdf-review").join("annotations")
}

/// FNV-1a 64: stable across Rust versions, unlike DefaultHasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

fn hex(c: u32) -> String {
    format!("#{c:06x}")
}
fn parse_hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(PALETTE[0])
}
fn with_alpha(c: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a = a;
    h
}

impl Reviewer {
    fn new(path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let settings = load_settings();
        let mut this = Reviewer {
            focus: cx.focus_handle(),
            path: None,
            doc_hash: None,
            pages: vec![],
            anns: vec![],
            tool: Tool::Select,
            tool_colors: default_tool_colors(),
            palette_open: false,
            zoom: 1.25,
            selected: None,
            editing: false,
            drag: None,
            page_bounds: Default::default(),
            note_bounds: Default::default(),
            status: "".into(),
            scroll: ScrollHandle::new(),
            mouse: Point::default(),
            #[cfg(target_os = "macos")]
            _pinch_monitor: None,
            recent: load_recent(),
            caret: 0,
            list_scroll: ScrollHandle::new(),
            minimap: settings.minimap,
            minimap_scroll: ScrollHandle::new(),
            vim: vim::Vim::default(),
            search: None,
            search_scroll: ScrollHandle::new(),
            owl_path: owl::configured(settings.owl.as_deref()),
            owl_setting: settings.owl.clone(),
            ask: None,
            marks: Default::default(),
            last_jump: None,
            undo: vec![],
            redo: vec![],
            edit_checkpoint: None,
            sidebar_w: settings.sidebar_w,
            win_w: 1400.0,
            tasks: vec![],
            task_anchor: HashMap::new(),
            current_task: None,
            side_tab: SideTab::Annotations,
            waiting: vec![],
            resizing: None,
            last_page: 0,
            picker: None,
            picker_scroll: ScrollHandle::new(),
            reviewer_name: settings.name.clone(),
            name_prompt: None,
            author_filter: None,
        };
        #[cfg(target_os = "macos")]
        {
            this._pinch_monitor = install_pinch(cx);
        }
        // Agents drop review lists in ~/.pdf-review/inbox; look every couple of seconds.
        this.check_inbox();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(std::time::Duration::from_secs(2)).await;
            let alive = this.update(cx, |this, cx| {
                if this.check_inbox() {
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
        if let Some(p) = path {
            this.open(p);
        } else if !this.recent.is_empty() {
            this.picker = Some(Picker::default());
        }
        this
    }

    /// Zoom to `z`, keeping the content under `anchor` (window coords) fixed.
    fn zoom_to(&mut self, z: f32, anchor: Option<Point<Pixels>>) {
        let z = z.clamp(MIN_ZOOM, MAX_ZOOM);
        if (z - self.zoom).abs() < 1e-4 {
            return;
        }
        let k = z / self.zoom;
        let vb = self.scroll.bounds();
        let a = anchor.filter(|p| vb.contains(p)).unwrap_or_else(|| vb.center());
        let local = a - vb.origin;
        let content = local - self.scroll.offset();
        let off = local - point(content.x * k, content.y * k);
        self.zoom = z;
        self.scroll.set_offset(point(off.x.min(px(0.)), off.y.min(px(0.))));
    }

    fn sidecar(&self) -> Option<PathBuf> {
        let hash = self.doc_hash.as_ref()?;
        Some(store_dir().join(format!("{hash}.json")))
    }

    /// Pre-~/.pdf-review location, read once so earlier reviews are not lost.
    fn legacy_sidecar(&self) -> Option<PathBuf> {
        self.path.as_ref().map(|p| PathBuf::from(format!("{}.annotations.json", p.display())))
    }

    fn open(&mut self, path: PathBuf) {
        if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")) {
            self.status = match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| self.import_review(&s)) {
                Ok(Some(msg)) => msg.into(),
                Ok(None) => "That review is for another document. Open the PDF first, then the review.".into(),
                Err(e) => e.into(),
            };
            return;
        }
        match load_pages(&path) {
            Ok(pages) => {
                self.pages = pages;
                self.doc_hash = std::fs::read(&path).ok().map(|b| format!("{:016x}", fnv1a(&b)));
                self.path = Some(path);
                let saved = [self.sidecar(), self.legacy_sidecar()]
                    .into_iter()
                    .flatten()
                    .find_map(|s| std::fs::read_to_string(s).ok())
                    .and_then(|s| serde_json::from_str::<SaveFile>(&s).ok());
                (self.anns, self.marks, self.tasks) = saved.map(|f| (f.annotations, f.marks, f.tasks)).unwrap_or_default();
                self.current_task = None;
                self.undo.clear();
                self.redo.clear();
                self.search = None;
                self.last_jump = None;
                self.vim.reset();
                self.repair_quotes();
                self.resolve_tasks();
                self.author_filter = None;
                self.import_list_beside();
                self.selected = None;
                self.editing = false;
                self.picker = None;
                self.status = "".into();
                self.remember(self.path.clone().unwrap());
            }
            Err(e) => self.status = format!("Could not open PDF: {e}").into(),
        }
    }

    fn save(&mut self) {
        let (Some(side), Some(path)) = (self.sidecar(), self.path.as_ref()) else { return };
        let file = SaveFile {
            document: path.file_name().unwrap().to_string_lossy().into(),
            path: path.display().to_string(),
            annotations: self.sorted(),
            marks: self.marks.clone(),
            tasks: self.tasks.clone(),
        };
        let res = std::fs::create_dir_all(store_dir()).and_then(|_| std::fs::write(&side, serde_json::to_string_pretty(&file).unwrap()));
        if let Err(e) = res {
            self.status = format!("Save failed: {e}").into();
        }
    }

    fn sorted(&self) -> Vec<Ann> {
        let mut v = self.anns.clone();
        v.sort_by(|a, b| (a.page, a.rect.y as i32, a.rect.x as i32).cmp(&(b.page, b.rect.y as i32, b.rect.x as i32)));
        v
    }

    fn markdown(&self) -> String {
        let mut md = self.markdown_of(&self.sorted(), "Review");
        let mut people: Vec<&str> = self.anns.iter().map(|a| a.author.as_str()).filter(|a| !a.is_empty()).collect();
        people.sort();
        people.dedup();
        if !people.is_empty() {
            md += "\n## By reviewer\n\n";
            let me = self.reviewer_name.clone().unwrap_or_else(|| "Me".into());
            for who in std::iter::once("").chain(people) {
                let n = self.anns.iter().filter(|a| a.author == who).count();
                if n > 0 {
                    md += &format!("- {}: {n}\n", if who.is_empty() { me.as_str() } else { who });
                }
            }
        }
        md
    }

    /// Markdown for some annotations, by page. Names the author when several people commented.
    fn markdown_of(&self, anns: &[Ann], heading: &str) -> String {
        let name = self.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut md = format!("# {heading}: {name}\n\n{} annotations.\n", anns.len());
        let named = anns.iter().any(|a| !a.author.is_empty());
        let me = self.reviewer_name.clone().unwrap_or_else(|| "Me".into());
        let mut page = 0;
        for a in anns.iter().cloned() {
            if a.page != page {
                page = a.page;
                md += &format!("\n## Page {page}\n");
            }
            md += &format!("\n- **{}**", a.kind.label());
            if named {
                md += &format!(" · {}", if a.author.is_empty() { me.as_str() } else { a.author.as_str() });
            }
            if !a.text.is_empty() {
                md += &format!("\n  > {}", a.text.replace('\n', "\n  > "));
            }
            if !a.note.is_empty() {
                md += &format!("\n  {}", a.note.replace('\n', "\n  "));
            }
            md += "\n";
        }
        if !self.tasks.is_empty() {
            md += "\n## Review items\n\n";
            for t in self.sorted_tasks() {
                let page = self.task_anchor.get(&t.id).map(|(p, _)| *p).or(t.page).map(|p| format!(" (p. {p})")).unwrap_or_default();
                md += &format!("- [{}] {}{page}", if t.done { "x" } else { " " }, t.title);
                if !t.detail.is_empty() {
                    md += &format!(": {}", t.detail);
                }
                md += "\n";
            }
        }
        md
    }

    fn export_md(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else { return };
        let md = self.markdown();
        let out = path.with_extension("review.md");
        cx.write_to_clipboard(ClipboardItem::new_string(md.clone()));
        self.status = match std::fs::write(&out, md) {
            Ok(_) => format!("Exported {} (also copied to clipboard)", out.display()).into(),
            Err(e) => format!("Export failed: {e}").into(),
        };
    }

    fn remember(&mut self, path: PathBuf) {
        let path = std::fs::canonicalize(&path).unwrap_or(path).display().to_string();
        self.recent.retain(|r| r.path != path);
        self.recent.insert(0, Recent { path, opened: now_secs() });
        self.recent.truncate(MAX_RECENT);
        save_recent(&self.recent);
    }

    fn show_picker(&mut self) {
        self.editing = false;
        self.picker = Some(Picker::default());
        self.picker_scroll.scroll_to_item(0);
    }

    /// Rows in the open palette: a typed path that exists, then recents matching the query.
    fn picker_entries(&self) -> Vec<(PathBuf, Option<u64>)> {
        let Some(picker) = &self.picker else { return vec![] };
        let q = picker.query.trim();
        let mut out = vec![];
        if !q.is_empty() {
            let typed = expand_tilde(q);
            if typed.is_file() {
                out.push((typed, None));
            }
        }
        let toks: Vec<String> = q.to_lowercase().split_whitespace().map(String::from).collect();
        for r in &self.recent {
            let hay = r.path.to_lowercase();
            let dup = out.first().is_some_and(|(p, _)| std::fs::canonicalize(p).ok().is_some_and(|c| c.display().to_string() == r.path));
            if !dup && toks.iter().all(|t| hay.contains(t.as_str())) {
                out.push((PathBuf::from(&r.path), Some(r.opened)));
            }
        }
        out
    }

    fn picker_open(&mut self, path: PathBuf) {
        if !path.is_file() {
            let p = path.display().to_string();
            self.recent.retain(|r| r.path != p);
            save_recent(&self.recent);
            self.status = format!("Not found, removed from recent: {p}").into();
            if let Some(pk) = &mut self.picker {
                pk.sel = 0;
            }
            return;
        }
        self.open(path);
    }

    fn picker_key(&mut self, k: &gpui::Keystroke, cx: &mut Context<Self>) {
        let n = self.picker_entries().len();
        let Some(pk) = &mut self.picker else { return };
        if shortcut(k) {
            match k.key.as_str() {
                "o" => {
                    self.picker = None;
                    self.prompt_open(cx);
                }
                "v" => {
                    if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        pk.query.push_str(t.trim());
                        pk.sel = 0;
                    }
                }
                "backspace" => {
                    pk.query.clear();
                    pk.sel = 0;
                }
                "q" => cx.quit(),
                _ => {}
            }
            return;
        }
        match k.key.as_str() {
            "escape" => self.picker = None,
            "up" => pk.sel = pk.sel.saturating_sub(1),
            "down" => pk.sel = (pk.sel + 1).min(n.saturating_sub(1)),
            "enter" => {
                let sel = pk.sel;
                if let Some((path, _)) = self.picker_entries().into_iter().nth(sel) {
                    self.picker_open(path);
                }
                return;
            }
            "backspace" => {
                pk.query.pop();
                pk.sel = 0;
            }
            _ => {
                if k.modifiers.control || k.modifiers.alt && k.key_char.is_none() {
                    return;
                }
                if let Some(c) = &k.key_char {
                    pk.query.push_str(c);
                    pk.sel = 0;
                }
            }
        }
        if let Some(pk) = &self.picker {
            self.picker_scroll.scroll_to_item(pk.sel);
        }
    }

    fn minimap_view(&self, current: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        const THUMB_W: f32 = 112.0;
        let thumbs: Vec<_> = self
            .pages
            .iter()
            .enumerate()
            .map(|(i, pv)| {
                let page = i + 1;
                let z = THUMB_W / pv.w;
                let marks: Vec<(u32, Vec<R>)> = self
                    .anns
                    .iter()
                    .filter(|a| a.page == page)
                    .map(|a| (parse_hex(&a.color), if a.spans.is_empty() { vec![a.rect] } else { a.spans.clone() }))
                    .collect();
                let active = page == current;
                div()
                    .id(("thumb", i))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .child(
                        div()
                            .relative()
                            .w(px(THUMB_W))
                            .h(px(pv.h * z))
                            .rounded_sm()
                            .overflow_hidden()
                            .border_2()
                            .border_color(rgb(if active { C_ACCENT } else { C_PANEL }))
                            .hover(|d| d.border_color(rgb(if active { C_ACCENT } else { C_SEG_ACTIVE })))
                            .child(img(pv.image.clone()).size_full())
                            .child(
                                canvas(
                                    |_, _, _| {},
                                    move |b, _, window, _| {
                                        for (c, rects) in &marks {
                                            for r in rects {
                                                let rr = Bounds::new(
                                                    point(b.origin.x + px(r.x * z), b.origin.y + px(r.y * z)),
                                                    size(px((r.w * z).max(2.0)), px((r.h * z).max(2.0))),
                                                );
                                                window.paint_quad(fill(rr, with_alpha(*c, 0.6)));
                                            }
                                        }
                                    },
                                )
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full(),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if active { C_TEXT } else { C_FAINT }))
                            .child(page.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scroll_to_page(page);
                        cx.notify();
                    }))
            })
            .collect();
        div()
            .id("minimap")
            .track_scroll(&self.minimap_scroll)
            .w(px(152.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(C_PANEL))
            .border_r_1()
            .border_color(rgb(C_BORDER))
            .py_3()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .children(thumbs)
    }

    /// Floating `/` search: query, live match count, and a results list with context.
    fn search_view(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(s) = &self.search else { return div() };
        let query = s.query.clone();
        let count = if query.trim().is_empty() {
            String::new()
        } else if s.matches.is_empty() {
            "No matches".into()
        } else {
            format!("{} of {}", s.cur + 1, s.matches.len())
        };
        let none = !query.trim().is_empty() && s.matches.is_empty();
        let rows: Vec<_> = s
            .matches
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let active = i == s.cur;
                div()
                    .id(("hit", i))
                    .flex()
                    .items_baseline()
                    .gap_3()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(active, |d| d.bg(rgb(C_SEG_ACTIVE)))
                    .when(!active, |d| d.hover(|d| d.bg(rgb(C_SEG))))
                    .child(div().flex_none().w(px(44.)).text_xs().text_color(rgb(C_FAINT)).child(format!("p. {}", m.page)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(rgb(C_MUTED))
                            .child(
                                gpui::StyledText::new(format!("{}{}{}", m.before, m.matched, m.after)).with_highlights([(
                                    m.before.len()..m.before.len() + m.matched.len(),
                                    gpui::HighlightStyle {
                                        color: Some(rgb(0xffffff).into()),
                                        background_color: Some(with_alpha(0xff9f1a, 0.45)),
                                        font_weight: Some(FontWeight::SEMIBOLD),
                                        ..Default::default()
                                    },
                                )]),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(s) = &mut this.search {
                            s.cur = i;
                            s.typing = false;
                        }
                        this.last_jump = Some(this.viewport_pos());
                        this.reveal_hit();
                        cx.notify();
                    }))
            })
            .collect();
        let has_rows = !rows.is_empty();
        let kbd = |t: &'static str| div().px_1().rounded_sm().bg(rgb(C_SEG)).text_color(rgb(C_MUTED)).child(t);

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(rgba(0x00000040))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(70.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.search = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(660.))
                    .max_h(px(460.))
                    .flex()
                    .flex_col()
                    .bg(rgb(C_PANEL))
                    .border_1()
                    .border_color(rgb(C_BORDER))
                    .rounded_xl()
                    .shadow_2xl()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(50.))
                            .px_4()
                            .border_b_1()
                            .border_color(rgb(C_BORDER))
                            .text_base()
                            .child(div().text_color(rgb(C_ACCENT)).font_weight(FontWeight::SEMIBOLD).child("/"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .child(div().truncate().child(query.clone()))
                                    .child(div().text_color(rgb(C_ACCENT)).child("▏"))
                                    .when(query.is_empty(), |d| d.child(div().text_color(rgb(C_FAINT)).child("Search this document… (lowercase ignores case)"))),
                            )
                            .child(div().flex_none().text_sm().text_color(rgb(if none { 0xef5350 } else { C_MUTED })).child(count)),
                    )
                    .when(has_rows, |d| {
                        d.child(
                            div()
                                .id("hit-list")
                                .track_scroll(&self.search_scroll)
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .p_1p5()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .children(rows),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(rgb(C_BORDER))
                            .text_xs()
                            .text_color(rgb(C_FAINT))
                            .child(div().flex().gap_1().items_center().child(kbd("↑↓")).child("preview"))
                            .child(div().flex().gap_1().items_center().child(kbd("↵")).child("jump"))
                            .child(div().flex().gap_1().items_center().child(kbd("esc")).child("cancel"))
                            .child(div().flex_1())
                            .child("then n / N for next and previous"),
                    ),
            )
    }

    fn picker_view(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let entries = self.picker_entries();
        let (query, sel) = self.picker.as_ref().map(|p| (p.query.clone(), p.sel)).unwrap_or_default();
        let empty_msg = if self.recent.is_empty() { "No recent documents yet. Press ⌘O to browse." } else { "No matches. Type a full path or press ⌘O to browse." };
        let now = now_secs();

        let input = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .h(px(50.))
            .px_4()
            .border_b_1()
            .border_color(rgb(C_BORDER))
            .text_base()
            .child(div().text_color(rgb(C_FAINT)).child("Open"))
            .child(if query.is_empty() {
                div()
                    .flex()
                    .child(div().text_color(rgb(C_ACCENT)).child("▏"))
                    .child(div().text_color(rgb(C_FAINT)).child("Search recent documents or type a path…"))
            } else {
                div().flex().child(query.clone()).child(div().text_color(rgb(C_ACCENT)).child("▏"))
            });

        let rows = entries.into_iter().enumerate().map(|(i, (path, opened))| {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let dir = path.parent().map(|d| tilde(&d.display().to_string())).unwrap_or_default();
            let exists = path.is_file();
            let right = match opened {
                None => "Open path".to_string(),
                Some(_) if !exists => "Missing".to_string(),
                Some(t) => ago(now.saturating_sub(t)),
            };
            let active = i == sel;
            div()
                .id(("recent", i))
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_lg()
                .cursor_pointer()
                .when(active, |d| d.bg(rgb(C_SEG_ACTIVE)))
                .when(!active, |d| d.hover(|d| d.bg(rgb(C_SEG))))
                .when(!exists, |d| d.opacity(0.5))
                .child(
                    div()
                        .flex_none()
                        .w(px(32.))
                        .h(px(38.))
                        .rounded_sm()
                        .bg(rgb(0xe5484d))
                        .flex()
                        .items_end()
                        .justify_center()
                        .pb_1()
                        .text_color(rgb(0xffffff))
                        .text_size(px(9.))
                        .font_weight(FontWeight::BOLD)
                        .child("PDF"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(name))
                        .child(div().text_xs().text_color(rgb(C_MUTED)).truncate().child(dir)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(rgb(if exists { C_FAINT } else { 0xef5350 }))
                        .child(right),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.picker_open(path.clone());
                    cx.notify();
                }))
        });
        let rows: Vec<_> = rows.collect();
        let has_rows = !rows.is_empty();

        let kbd = |s: &'static str| div().px_1().rounded_sm().bg(rgb(C_SEG)).text_color(rgb(C_MUTED)).child(s);
        let footer = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(rgb(C_BORDER))
            .text_xs()
            .text_color(rgb(C_FAINT))
            .child(div().flex().gap_1().items_center().child(kbd("↑↓")).child("navigate"))
            .child(div().flex().gap_1().items_center().child(kbd("↵")).child("open"))
            .child(div().flex().gap_1().items_center().child(kbd("esc")).child("close"))
            .child(div().flex_1())
            .child(
                plain_button("browse")
                    .text_color(rgb(C_TEXT))
                    .child("Browse…")
                    .child(div().text_color(rgb(C_FAINT)).child("⌘O"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker = None;
                        this.prompt_open(cx);
                        cx.notify();
                    })),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(rgba(0x00000099))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(90.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.picker = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(620.))
                    .max_h(px(540.))
                    .flex()
                    .flex_col()
                    .bg(rgb(C_PANEL))
                    .border_1()
                    .border_color(rgb(C_BORDER))
                    .rounded_xl()
                    .shadow_2xl()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(input)
                    .child(
                        div()
                            .id("picker-list")
                            .track_scroll(&self.picker_scroll)
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_1p5()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .when(has_rows, |d| d.child(div().px_3().pt_1().pb_1p5().text_xs().text_color(rgb(C_FAINT)).child("Recent")))
                            .children(rows)
                            .when(!has_rows, |d| d.child(div().p_6().text_center().text_sm().text_color(rgb(C_MUTED)).child(empty_msg))),
                    )
                    .child(footer),
            )
    }

    fn prompt_open(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open PDF".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    this.update(cx, |this, cx| {
                        this.open(p);
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// Text under a rect: chars whose centre is inside it, on lines that overlap it vertically.
    fn text_in(&self, page: usize, r: R) -> String {
        let Some(pv) = self.pages.get(page - 1) else { return String::new() };
        let mut out: Vec<String> = vec![];
        for line in &pv.lines {
            let ov = (line.bbox.y + line.bbox.h).min(r.y + r.h) - line.bbox.y.max(r.y);
            if ov < line.bbox.h * 0.4 {
                continue;
            }
            let inside: Vec<_> = line.chars.iter().copied().filter(|(_, x0, x1)| (x0 + x1) / 2.0 >= r.x && (x0 + x1) / 2.0 <= r.x + r.w).collect();
            let s = join_chars(&inside, line.bbox.h);
            let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if !s.is_empty() {
                out.push(s);
            }
        }
        out.join("\n")
    }

    fn to_pdf(&self, page: usize, pos: Point<Pixels>) -> Option<(f32, f32)> {
        let b = *self.page_bounds.borrow().get(&page)?;
        Some((f32::from(pos.x - b.origin.x) / self.zoom, f32::from(pos.y - b.origin.y) / self.zoom))
    }

    fn add(&mut self, page: usize, kind: Kind, rect: R, points: Vec<(f32, f32)>) {
        let text = if kind == Kind::Note { String::new() } else { self.text_in(page, rect) };
        self.push_ann(page, kind, rect, points, vec![], text);
    }

    /// Text highlight from char `a` to char `b` on `page`.
    fn add_text_highlight(&mut self, page: usize, a: Pos, b: Pos) {
        let Some(pv) = self.pages.get(page - 1) else { return };
        let (spans, text) = pv.range(a, b);
        if spans.is_empty() {
            return;
        }
        let pts: Vec<(f32, f32)> = spans.iter().flat_map(|r| [(r.x, r.y), (r.x + r.w, r.y + r.h)]).collect();
        self.push_ann(page, Kind::TextHighlight, bbox(&pts), vec![], spans, text);
    }

    fn push_ann(&mut self, page: usize, kind: Kind, rect: R, points: Vec<(f32, f32)>, spans: Vec<R>, text: String) {
        let id = uuid::Uuid::new_v4().to_string();
        self.checkpoint();
        self.status = "".into();
        let color = self.tool_colors.get(&kind).copied().unwrap_or(PALETTE[0]);
        self.anns.push(Ann { id: id.clone(), page, kind, color: hex(color), rect, points, spans, text, note: String::new(), author: String::new() });
        self.selected = Some(id);
        self.begin_edit();
        self.save();
    }

    /// Start editing the selected note: caret at the end, card scrolled into view.
    fn begin_edit(&mut self) {
        self.editing = self.selected.is_some();
        self.edit_checkpoint = None;
        self.vim.reset();
        self.caret = self.selected_note().map(|n| n.len()).unwrap_or(0);
        self.reveal_card();
    }

    fn selected_note(&self) -> Option<&String> {
        let id = self.selected.as_ref()?;
        self.anns.iter().find(|a| &a.id == id).map(|a| &a.note)
    }

    fn reveal_card(&self) {
        let Some(id) = &self.selected else { return };
        if let Some(ix) = self.sorted().iter().position(|a| &a.id == id) {
            self.list_scroll.scroll_to_item(ix);
        }
    }

    /// Page under the middle of the viewer (1-based), from last frame's layout.
    fn current_page(&self) -> usize {
        let vb = self.scroll.bounds();
        let cy = vb.origin.y + vb.size.height / 2.0;
        let pb = self.page_bounds.borrow();
        let mut best = (1, f32::MAX);
        for (&p, b) in pb.iter() {
            if p > self.pages.len() {
                continue;
            }
            let d = if cy < b.top() { f32::from(b.top() - cy) } else if cy > b.bottom() { f32::from(cy - b.bottom()) } else { 0.0 };
            if d < best.1 || (d == best.1 && p < best.0) {
                best = (p, d);
            }
        }
        best.0
    }

    /// Re-extract quotes saved with unresolved glyphs (from before private-use glyph mapping).
    fn repair_quotes(&mut self) {
        let broken = |t: &str| t.chars().any(|c| is_pua(c) || c == '\u{FFFD}');
        let mut changed = false;
        for i in 0..self.anns.len() {
            let a = &self.anns[i];
            if !broken(&a.text) {
                continue;
            }
            let fresh = match (a.kind, a.spans.first(), a.spans.last()) {
                (Kind::TextHighlight, Some(f), Some(l)) => self.pages.get(a.page - 1).and_then(|pv| {
                    let s = pv.nearest_char((f.x + 0.5, f.y + f.h / 2.0))?;
                    let e = pv.nearest_char((l.x + l.w - 0.5, l.y + l.h / 2.0))?;
                    Some(pv.range(s, e).1)
                }),
                (Kind::Note, ..) => None,
                _ => Some(self.text_in(a.page, a.rect)),
            };
            if let Some(t) = fresh.filter(|t| !t.is_empty() && *t != self.anns[i].text) {
                self.anns[i].text = t;
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }

    /// A note's text box in PDF points, from last frame's layout.
    fn note_rect(&self, a: &Ann) -> Option<R> {
        let wb = *self.note_bounds.borrow().get(&a.id)?;
        let pb = *self.page_bounds.borrow().get(&a.page)?;
        let z = self.zoom;
        Some(R {
            x: f32::from(wb.origin.x - pb.origin.x) / z,
            y: f32::from(wb.origin.y - pb.origin.y) / z,
            w: f32::from(wb.size.width) / z,
            h: f32::from(wb.size.height) / z,
        })
    }

    // ---------- review lists ----------

    /// Import review lists for the open document; open the document first if nothing is open.
    /// Returns whether anything changed.
    fn check_inbox(&mut self) -> bool {
        let Ok(entries) = std::fs::read_dir(inbox_dir()) else { return false };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
            // skip files still being written
            .filter(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_millis() > 800))
            .collect();
        files.sort();
        let mut changed = false;
        let mut waiting = vec![];
        for file in files {
            if let Some(json) = std::fs::read_to_string(&file).ok().filter(|s| review::is_review(s)) {
                match self.import_review(&json) {
                    Ok(Some(msg)) => {
                        self.status = msg.into();
                        file_away(&file, "imported");
                    }
                    Ok(None) => waiting.push(file.file_name().unwrap_or_default().to_string_lossy().to_string()),
                    Err(e) => {
                        self.status = e.into();
                        file_away(&file, "rejected");
                    }
                }
                changed = true;
                continue;
            }
            let list = match std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|s| tasks::parse(&s)) {
                Ok(l) => l,
                Err(e) => {
                    self.status = format!("Rejected review list {}: {e}", file.file_name().unwrap_or_default().to_string_lossy()).into();
                    file_away(&file, "rejected");
                    changed = true;
                    continue;
                }
            };
            if self.path.is_none() {
                if let Some(doc) = tasks::document_path(&list.document) {
                    self.open(doc);
                    changed = true;
                }
            }
            let ours = self.path.as_ref().is_some_and(|p| tasks::matches_document(&list.document, p));
            if !ours {
                waiting.push(list.document.clone());
                continue;
            }
            let name = list.title.clone().unwrap_or_else(|| "Review list".into());
            let from = list.from.clone().unwrap_or_else(|| "agent".into());
            let new = tasks::into_tasks(list, now_secs(), || uuid::Uuid::new_v4().to_string());
            let n = new.len();
            self.tasks.extend(new);
            self.resolve_tasks();
            self.save();
            file_away(&file, "imported");
            self.side_tab = SideTab::Review;
            self.status = format!("{name} from {from}: {n} item{} to review", if n == 1 { "" } else { "s" }).into();
            changed = true;
        }
        if waiting != self.waiting {
            self.waiting = waiting;
            changed = true;
        }
        changed
    }

    /// Import someone's sent review into the open document, replacing what they sent before.
    /// Ok(None) when it is for another document.
    fn import_review(&mut self, json: &str) -> Result<Option<String>, String> {
        let sent: review::Sent<Ann> = serde_json::from_str(json).map_err(|e| format!("Not a valid review: {e}"))?;
        let who = sent.reviewer.trim().to_string();
        if who.is_empty() {
            return Err("Review has no reviewer name".into());
        }
        let ours = self.path.as_ref().is_some_and(|p| tasks::matches_document(&sent.document, p));
        if !ours {
            return Ok(None);
        }
        self.checkpoint();
        self.anns.retain(|a| a.author != who);
        let n = sent.annotations.len();
        self.anns.extend(sent.annotations.into_iter().map(|mut a| {
            a.author = who.clone();
            a.id = uuid::Uuid::new_v4().to_string();
            a
        }));
        self.save();
        self.side_tab = SideTab::Annotations;
        self.author_filter = None;
        Ok(Some(format!("Imported {n} annotation{} from {who}", if n == 1 { "" } else { "s" })))
    }

    /// A review list shipped next to the PDF (`name.review.json`), imported once.
    fn import_list_beside(&mut self) {
        let Some(file) = self.path.as_deref().and_then(review::list_beside) else { return };
        let Ok(list) = std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|s| tasks::parse(&s)) else { return };
        let name = list.title.clone().unwrap_or_else(|| "Review list".into());
        let from = list.from.clone().unwrap_or_else(|| "agent".into());
        if self.tasks.iter().any(|t| t.list == name && t.from == from) {
            return;
        }
        let new = tasks::into_tasks(list, now_secs(), || uuid::Uuid::new_v4().to_string());
        let n = new.len();
        self.tasks.extend(new);
        self.resolve_tasks();
        self.save();
        self.side_tab = SideTab::Review;
        self.status = format!("{name} from {from}: {n} item{} to review", if n == 1 { "" } else { "s" }).into();
    }

    /// Write your annotations, with your name, to Downloads for emailing back.
    fn send_review(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let Some(me) = self.reviewer_name.clone() else {
            self.name_prompt = Some(review::os_user_name());
            return;
        };
        let mine: Vec<Ann> = self.sorted().into_iter().filter(|a| a.author.is_empty()).collect();
        let sent = review::Sent {
            format: review::FORMAT.into(),
            document: path.file_name().unwrap_or_default().to_string_lossy().into(),
            reviewer: me.clone(),
            sent: now_secs(),
            items: self.sorted_tasks().into_iter().map(|t| review::ItemState { title: t.title.clone(), done: t.done }).collect(),
            annotations: mine.clone(),
        };
        let out = review::out_dir().join(review::file_name(&path, &me));
        let md = self.markdown_of(&mine, &format!("Review by {me}"));
        let res = std::fs::write(&out, serde_json::to_string_pretty(&sent).unwrap()).and_then(|_| std::fs::write(out.with_extension("md"), md));
        self.status = match res {
            Ok(_) => {
                review::reveal(&out);
                format!("Saved {} ({} annotations). Attach it to an email to send it back.", out.display(), mine.len()).into()
            }
            Err(e) => format!("Could not save the review: {e}").into(),
        };
    }

    fn name_key(&mut self, k: &gpui::Keystroke) {
        let Some(name) = &mut self.name_prompt else { return };
        match k.key.as_str() {
            "escape" => self.name_prompt = None,
            "enter" => {
                let n = name.trim().to_string();
                if !n.is_empty() {
                    self.reviewer_name = Some(n);
                    self.name_prompt = None;
                    self.save_settings();
                    self.send_review();
                }
            }
            "backspace" => {
                name.pop();
            }
            _ => {
                if let Some(t) = k.key_char.clone().filter(|_| !k.modifiers.control && !k.modifiers.platform) {
                    name.push_str(&t);
                }
            }
        }
    }

    fn name_view(&self) -> impl IntoElement + use<> {
        let name = self.name_prompt.clone().unwrap_or_default();
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt(px(140.))
            .bg(with_alpha(0x000000, 0.45))
            .child(
                div()
                    .w(px(460.))
                    .h_full()
                    .max_h(px(190.))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .rounded_xl()
                    .bg(rgb(C_PANEL))
                    .border_1()
                    .border_color(rgb(C_BORDER))
                    .shadow_lg()
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("Your name"))
                    .child(div().text_sm().text_color(rgb(C_MUTED)).child("Shown next to your comments when the review is read. Asked once."))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(36.))
                            .px_3()
                            .rounded_md()
                            .bg(rgb(C_BG))
                            .border_1()
                            .border_color(rgb(C_ACCENT))
                            .child(name)
                            .child(div().text_color(rgb(C_ACCENT)).child("▏")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .text_xs()
                            .text_color(rgb(C_FAINT))
                            .child("↵ save and send")
                            .child("esc cancel"),
                    ),
            )
    }

    fn resolve_tasks(&mut self) {
        let flats: Vec<&[char]> = self.pages.iter().map(|p| p.flat.as_slice()).collect();
        let n = self.pages.len();
        self.task_anchor = self
            .tasks
            .iter()
            .filter_map(|t| {
                let found = tasks::locate(&flats, &t.quote, t.page).and_then(|(page, a, b)| {
                    let pv = &self.pages[page - 1];
                    let spans = pv.range(pv.flat_pos[a], pv.flat_pos[b]).0;
                    (!spans.is_empty()).then_some((page, spans))
                });
                let anchor = found.or_else(|| t.page.filter(|p| *p <= n).map(|p| (p, vec![])))?;
                Some((t.id.clone(), anchor))
            })
            .collect();
    }

    /// Tasks in reading order (by where they point; unplaced ones last).
    fn sorted_tasks(&self) -> Vec<&tasks::Task> {
        let mut v: Vec<_> = self.tasks.iter().collect();
        let key = |t: &tasks::Task| match self.task_anchor.get(&t.id) {
            Some((p, spans)) => (*p, spans.first().map(|r| r.y as i64).unwrap_or(-1)),
            None => (usize::MAX, 0),
        };
        v.sort_by_key(|t| key(t));
        v
    }

    fn goto_task(&mut self, id: &str) {
        self.current_task = Some(id.to_string());
        self.side_tab = SideTab::Review;
        let Some((page, spans)) = self.task_anchor.get(id).cloned() else {
            self.status = "This item has no place in the document".into();
            return;
        };
        self.last_jump = Some(self.viewport_pos());
        match spans.first() {
            Some(r) => {
                let Some(b) = self.page_bounds.borrow().get(&page).copied() else { return };
                let vb = self.scroll.bounds();
                let off = self.scroll.offset();
                let content_y = b.top() - vb.top() - off.y + px((r.y + r.h / 2.0) * self.zoom);
                self.set_scroll_y(vb.size.height / 2.0 - content_y);
            }
            None => self.scroll_to_page(page),
        }
    }

    /// Jump to the next (or previous) open item after the current one.
    fn step_task(&mut self, forward: bool) {
        let order: Vec<String> = self.sorted_tasks().iter().filter(|t| !t.done).map(|t| t.id.clone()).collect();
        if order.is_empty() {
            self.status = if self.tasks.is_empty() { "No review items" } else { "All review items are done" }.into();
            return;
        }
        let all: Vec<String> = self.sorted_tasks().iter().map(|t| t.id.clone()).collect();
        let cur = self.current_task.as_ref().and_then(|c| all.iter().position(|x| x == c));
        let pos_of = |id: &String| all.iter().position(|x| x == id).unwrap();
        let next = match cur {
            None => if forward { order.first() } else { order.last() },
            Some(c) if forward => order.iter().find(|id| pos_of(id) > c).or(order.first()),
            Some(c) => order.iter().rev().find(|id| pos_of(id) < c).or(order.last()),
        }
        .cloned();
        if let Some(id) = next {
            self.goto_task(&id);
        }
    }

    fn toggle_task(&mut self, id: &str) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.done = !t.done;
        }
        self.save();
    }

    fn review_view(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let order = self.sorted_tasks();
        let numbers: HashMap<&str, usize> = order.iter().enumerate().map(|(i, t)| (t.id.as_str(), i + 1)).collect();
        // group by list, newest list first, items in reading order
        let mut lists: Vec<(String, String, u64)> = vec![];
        for t in &self.tasks {
            if !lists.iter().any(|(l, f, r)| *l == t.list && *f == t.from && *r == t.received) {
                lists.push((t.list.clone(), t.from.clone(), t.received));
            }
        }
        lists.sort_by(|a, b| b.2.cmp(&a.2));
        let now = now_secs();
        let mut body = div().flex().flex_col().gap_3();
        if self.tasks.is_empty() {
            body = body.child(
                div()
                    .mt_10()
                    .px_6()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .text_center()
                    .text_color(rgb(C_MUTED))
                    .child("No review items yet.")
                    .child(div().text_xs().text_color(rgb(C_FAINT)).child("Agents can send a list of places to look at by writing a JSON file to ~/.pdf-review/inbox/ (see REVIEW_LISTS.md).")),
            );
        }
        for (list, from, received) in lists {
            let items: Vec<&tasks::Task> = order.iter().copied().filter(|t| t.list == list && t.from == from && t.received == received).collect();
            let open = items.iter().filter(|t| !t.done).count();
            body = body.child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .px_1()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(list.clone()))
                    .child(div().text_xs().text_color(rgb(C_FAINT)).child(format!("from {from} · {}", ago(now.saturating_sub(received)))))
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(rgb(C_MUTED)).child(format!("{open} open"))),
            );
            for t in items {
                let id = t.id.clone();
                let tid = t.id.clone();
                let num = numbers.get(t.id.as_str()).copied().unwrap_or(0);
                let current = self.current_task.as_deref() == Some(t.id.as_str());
                let anchor = self.task_anchor.get(&t.id);
                let unplaced = anchor.is_none();
                let quote_missing = !t.quote.is_empty() && anchor.is_some_and(|(_, s)| s.is_empty());
                let page = anchor.map(|(p, _)| *p).or(t.page);
                let prio = match t.priority.as_str() {
                    "high" => Some(0xef5350),
                    "medium" => Some(0xf5a623),
                    "low" => Some(C_FAINT),
                    _ => None,
                };
                body = body.child(
                    div()
                        .id(SharedString::from(format!("task-{}", t.id)))
                        .flex()
                        .gap_2p5()
                        .p_2p5()
                        .rounded_lg()
                        .bg(rgb(if current { C_CARD_SEL } else { C_CARD }))
                        .border_1()
                        .border_color(if current { rgb(0xab47bc) } else { rgb(C_BORDER) })
                        .cursor_pointer()
                        .when(t.done, |d| d.opacity(0.55))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.goto_task(&id);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .id(SharedString::from(format!("chk-{}", t.id)))
                                .flex_none()
                                .mt_0p5()
                                .size(px(16.))
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(if t.done { 0x4caf50 } else { C_MUTED }))
                                .when(t.done, |d| d.bg(rgb(0x4caf50)))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(11.))
                                .text_color(rgb(0xffffff))
                                .child(if t.done { "✓" } else { "" })
                                .hover(|d| d.border_color(rgb(0x4caf50)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_task(&tid);
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .text_xs()
                                        .child(div().px_1().rounded_sm().bg(with_alpha(0xab47bc, 0.3)).text_color(rgb(0xf0d4ff)).child(num.to_string()))
                                        .children(prio.map(|c| div().size(px(7.)).rounded_full().bg(rgb(c))))
                                        .child(div().text_color(rgb(C_MUTED)).child(match page {
                                            Some(p) => format!("Page {p}"),
                                            None => "No page".into(),
                                        }))
                                        .when(quote_missing || unplaced, |d| {
                                            d.child(div().text_color(rgb(0xf5a623)).child(if unplaced { "quote not found" } else { "quote not found · page only" }))
                                        }),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .when(t.done, |d| d.line_through())
                                        .child(t.title.clone()),
                                )
                                .when(!t.detail.is_empty(), |d| d.child(div().text_sm().text_color(rgb(C_MUTED)).child(t.detail.clone())))
                                .when(!t.quote.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .pl_2()
                                            .border_l_2()
                                            .border_color(rgb(0xab47bc))
                                            .text_xs()
                                            .italic()
                                            .text_color(rgb(0xb5b6ba))
                                            .line_clamp(3)
                                            .child(t.quote.clone()),
                                    )
                                }),
                        ),
                );
            }
        }
        if self.tasks.iter().any(|t| t.done) {
            body = body.child(
                div().flex().justify_end().child(
                    plain_button("clear-done")
                        .text_xs()
                        .text_color(rgb(C_MUTED))
                        .child("Remove done items")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tasks.retain(|t| !t.done);
                            this.resolve_tasks();
                            this.save();
                            cx.notify();
                        })),
                ),
            );
        }
        if !self.waiting.is_empty() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(rgb(C_FAINT))
                    .child(format!("Waiting in the inbox for other documents: {}", self.waiting.join(", "))),
            );
        }
        body.into_any_element()
    }

    // ---------- Owl ----------

    fn open_ask(&mut self, source: AskSource, page: usize, quote: String) {
        if let Some(r) = self.ask.as_ref().and_then(|a| a.running.clone()) {
            r.cancel();
        }
        self.editing = false;
        self.search = self.search.take().filter(|s| !s.typing);
        let generation = self.ask.as_ref().map(|a| a.generation + 1).unwrap_or(0);
        self.ask = Some(AskState {
            source,
            page,
            quote,
            question: String::new(),
            caret: 0,
            external: false, // internal by default: document text stays on this machine
            asked: None,
            answer: String::new(),
            error: None,
            running: None,
            generation,
        });
    }

    fn ask_about_range(&mut self, page: usize, a: Pos, b: Pos) {
        let Some(pv) = self.pages.get(page - 1) else { return };
        let (spans, text) = pv.range(a, b);
        if !spans.is_empty() && !text.is_empty() {
            self.open_ask(AskSource::Fresh { spans }, page, text);
        }
    }

    fn ask_about_ann(&mut self, id: &str) {
        let Some(a) = self.anns.iter().find(|a| a.id == id) else { return };
        if a.text.is_empty() {
            self.status = "This annotation has no quoted text to ask about".into();
            return;
        }
        let (page, text) = (a.page, a.text.clone());
        self.open_ask(AskSource::Ann(id.to_string()), page, text);
    }

    fn run_ask(&mut self, cx: &mut Context<Self>) {
        let Some(owl_path) = self.owl_path.clone() else { return };
        let document = self.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let Some(ask) = &mut self.ask else { return };
        if let Some(r) = ask.running.take() {
            r.cancel();
        }
        let question = if ask.question.trim().is_empty() { "Explain this passage.".to_string() } else { ask.question.trim().to_string() };
        let prompt = owl::prompt(&document, ask.page, &ask.quote, &question);
        ask.generation += 1;
        let generation = ask.generation;
        ask.asked = Some(question);
        ask.answer.clear();
        ask.error = None;
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        match owl::spawn(&owl_path, ask.external, &prompt, tx) {
            Ok(r) => ask.running = Some(r),
            Err(e) => {
                ask.error = Some(e);
                return;
            }
        }
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(msg) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    let Some(ask) = this.ask.as_mut().filter(|a| a.generation == generation) else { return };
                    match msg {
                        owl::Msg::Chunk(t) => ask.answer.push_str(&t),
                        owl::Msg::Done(r) => {
                            ask.running = None;
                            if let Err(e) = r {
                                if e != "Cancelled" {
                                    ask.error = Some(e);
                                }
                            }
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Save the answer: append to the annotation's note, or create a highlight carrying it.
    fn ask_to_note(&mut self) {
        let Some(ask) = self.ask.take() else { return };
        let (Some(q), answer) = (ask.asked.clone(), ask.answer.trim().to_string()) else {
            self.ask = Some(ask);
            return;
        };
        if answer.is_empty() || ask.running.is_some() {
            self.ask = Some(ask);
            return;
        }
        let who = if ask.external { "external" } else { "internal" };
        let text = format!("Owl ({who}) on \"{q}\":\n{answer}");
        match ask.source {
            AskSource::Ann(id) => {
                self.checkpoint();
                if let Some(a) = self.anns.iter_mut().find(|a| a.id == id) {
                    a.note = if a.note.trim().is_empty() { text } else { format!("{}\n\n{text}", a.note.trim_end()) };
                }
                self.selected = Some(id);
                self.save();
            }
            AskSource::Fresh { spans } => {
                let pts: Vec<(f32, f32)> = spans.iter().flat_map(|r| [(r.x, r.y), (r.x + r.w, r.y + r.h)]).collect();
                self.push_ann(ask.page, Kind::TextHighlight, bbox(&pts), vec![], spans, ask.quote.clone());
                // push_ann took its own undo checkpoint; set the note on the new annotation
                if let Some(a) = self.anns.last_mut() {
                    a.note = text;
                }
                self.save();
            }
        }
        self.editing = false;
        self.reveal_card();
        self.status = "Owl answer added as a note".into();
    }

    fn close_ask(&mut self) {
        if let Some(r) = self.ask.take().and_then(|a| a.running) {
            r.cancel();
        }
    }

    fn ask_key(&mut self, k: &gpui::Keystroke, cx: &mut Context<Self>) {
        let cmd = shortcut(k);
        let Some(ask) = &mut self.ask else { return };
        match k.key.as_str() {
            "escape" => {
                // first Esc stops a running answer, second closes
                if let Some(r) = ask.running.take() {
                    r.cancel();
                } else {
                    self.close_ask();
                }
            }
            "enter" if cmd => self.ask_to_note(),
            "enter" if k.modifiers.shift => note_edit::insert(&mut ask.question, &mut ask.caret, "\n"),
            "enter" => self.run_ask(cx),
            "tab" => ask.external = !ask.external,
            "c" if cmd => {
                if !ask.answer.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(ask.answer.trim().to_string()));
                    self.status = "Answer copied".into();
                }
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    note_edit::insert(&mut ask.question, &mut ask.caret, &t);
                }
            }
            _ => {
                let (q, c) = (&mut ask.question, &mut ask.caret);
                *c = (*c).min(q.len());
                let alt = k.modifiers.alt;
                match k.key.as_str() {
                    "backspace" if cmd => {
                        let s = note_edit::line_start(q, *c);
                        note_edit::remove(q, c, s);
                    }
                    "backspace" if alt => {
                        let s = note_edit::word_left(q, *c);
                        note_edit::remove(q, c, s);
                    }
                    "backspace" => {
                        let s = note_edit::prev(q, *c);
                        note_edit::remove(q, c, s);
                    }
                    "delete" => {
                        let e = note_edit::next(q, *c);
                        note_edit::remove(q, c, e);
                    }
                    "left" if cmd => *c = note_edit::line_start(q, *c),
                    "right" if cmd => *c = note_edit::line_end(q, *c),
                    "up" if cmd => *c = 0,
                    "down" if cmd => *c = q.len(),
                    "left" if alt => *c = note_edit::word_left(q, *c),
                    "right" if alt => *c = note_edit::word_right(q, *c),
                    "left" => *c = note_edit::prev(q, *c),
                    "right" => *c = note_edit::next(q, *c),
                    "up" => *c = note_edit::up(q, *c),
                    "down" => *c = note_edit::down(q, *c),
                    "home" => *c = note_edit::line_start(q, *c),
                    "end" => *c = note_edit::line_end(q, *c),
                    _ => {
                        if let Some(t) = k.key_char.clone().filter(|_| !cmd && !k.modifiers.control) {
                            note_edit::insert(q, c, &t);
                        }
                    }
                }
            }
        }
    }

    fn ask_view(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(ask) = &self.ask else { return div() };
        let external = ask.external;
        let running = ask.running.is_some();
        let has_answer = !ask.answer.trim().is_empty();
        let can_save = has_answer && !running;
        let kbd = |t: &'static str| div().px_1().rounded_sm().bg(rgb(C_SEG)).text_color(rgb(C_MUTED)).child(t);
        let mode_btn = |id: &'static str, label: &'static str, sub: &'static str, on: bool, ext: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1p5()
                .h_full()
                .px_2()
                .rounded_md()
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(C_SEG_ACTIVE)).text_color(rgb(C_TEXT)))
                .when(!on, |d| d.text_color(rgb(C_MUTED)).hover(|d| d.text_color(rgb(C_TEXT))))
                .child(div().size(px(7.)).rounded_full().bg(rgb(if ext { 0xff9f1a } else { 0x4caf50 })))
                .child(label)
                .child(div().text_xs().text_color(rgb(C_FAINT)).child(sub))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(a) = &mut this.ask {
                        a.external = ext;
                    }
                    cx.notify();
                }))
        };
        let status: gpui::AnyElement = if let Some(e) = &ask.error {
            div().text_sm().text_color(rgb(0xef5350)).child(e.clone()).into_any_element()
        } else if running && !has_answer {
            div().text_sm().text_color(rgb(C_MUTED)).child(if external { "Asking Codex…" } else { "Asking local model…" }).into_any_element()
        } else if has_answer {
            div().text_sm().text_color(rgb(C_TEXT)).child(format!("{}{}", ask.answer.trim_start(), if running { "▍" } else { "" })).into_any_element()
        } else {
            div().text_sm().text_color(rgb(C_FAINT)).child("Ask a question about the passage, or press ↵ to have it explained.").into_any_element()
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(rgba(0x00000066))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(70.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.close_ask();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(720.))
                    .max_h(gpui::relative(0.85))
                    .flex()
                    .flex_col()
                    .bg(rgb(C_PANEL))
                    .border_1()
                    .border_color(rgb(C_BORDER))
                    .rounded_xl()
                    .shadow_2xl()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // passage
                    .child(
                        div()
                            .flex_none()
                            .px_4()
                            .pt_3()
                            .pb_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(rgb(C_FAINT))
                                    .child(icon("owl").size(px(13.)).text_color(rgb(0xb98ad0)))
                                    .child(format!("Ask Owl · page {}", ask.page))
                                    .child(div().flex_1())
                                    .child(
                                        segment()
                                            .h(px(26.))
                                            .text_xs()
                                            .child(mode_btn("ask-int", "Internal", "Ollama", !external, false, cx))
                                            .child(mode_btn("ask-ext", "External", "Codex", external, true, cx)),
                                    ),
                            )
                            .child(
                                div()
                                    .pl_2()
                                    .border_l_2()
                                    .border_color(rgb(0xab47bc))
                                    .text_sm()
                                    .italic()
                                    .text_color(rgb(0xc8c9cc))
                                    .id("ask-quote")
                                    .max_h(px(150.))
                                    .overflow_y_scroll()
                                    .child(ask.quote.clone()),
                            ),
                    )
                    // question + mode
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_start()
                            .gap_3()
                            .min_h(px(50.))
                            .px_4()
                            .py_2p5()
                            .border_t_1()
                            .border_b_1()
                            .border_color(rgb(C_BORDER))
                            .child(
                                div()
                                    .id("ask-question")
                                    .flex_1()
                                    .min_w_0()
                                    .max_h(px(180.))
                                    .overflow_y_scroll()
                                    .pt_0p5()
                                    .text_base()
                                    .when(ask.question.is_empty(), |d| {
                                        d.flex().child(div().text_color(rgb(0xb98ad0)).child("▏")).child(div().text_color(rgb(C_FAINT)).child("What does this mean for us?"))
                                    })
                                    .when(!ask.question.is_empty(), |d| {
                                        let c = ask.caret.min(ask.question.len());
                                        let shown = format!("{}▏{}", &ask.question[..c], &ask.question[c..]);
                                        let caret_at = c..c + "▏".len();
                                        d.child(gpui::StyledText::new(shown).with_highlights([(
                                            caret_at,
                                            gpui::HighlightStyle { color: Some(rgb(0xb98ad0).into()), ..Default::default() },
                                        )]))
                                    }),
                            )
                    )
                    .when(external, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .px_4()
                                .py_1()
                                .bg(with_alpha(0xff9f1a, 0.12))
                                .text_xs()
                                .text_color(rgb(0xffc266))
                                .child("External sends this passage and your question to OpenAI."),
                        )
                    })
                    // answer
                    .child(
                        div()
                            .id("ask-answer")
                            .flex_1()
                            .min_h(px(60.))
                            .overflow_y_scroll()
                            .px_4()
                            .py_3()
                            .when_some(ask.asked.clone().filter(|_| has_answer || running), |d, q| {
                                d.child(div().mb_1p5().text_xs().text_color(rgb(C_FAINT)).child(format!("Q: {q}")))
                            })
                            .child(status),
                    )
                    // footer
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(rgb(C_BORDER))
                            .text_xs()
                            .text_color(rgb(C_FAINT))
                            .child(div().flex().gap_1().items_center().child(kbd("↵")).child(if has_answer { "ask again" } else { "ask" }))
                            .child(div().flex().gap_1().items_center().child(kbd("⇧↵")).child("new line"))
                            .child(div().flex().gap_1().items_center().child(kbd("⇥")).child("internal / external"))
                            .child(div().flex().gap_1().items_center().child(kbd("esc")).child(if running { "stop" } else { "close" }))
                            .child(div().flex_1())
                            .when(has_answer && !running, |d| {
                                d.child(
                                    plain_button("ask-copy")
                                        .text_color(rgb(C_TEXT))
                                        .child("Copy")
                                        .child(div().text_color(rgb(C_FAINT)).child("⌘C"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(a) = &this.ask {
                                                cx.write_to_clipboard(ClipboardItem::new_string(a.answer.trim().to_string()));
                                            }
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .id("ask-note")
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .h(px(28.))
                                    .px_3()
                                    .rounded_md()
                                    .text_color(rgb(0xffffff))
                                    .font_weight(FontWeight::MEDIUM)
                                    .when(can_save, |d| d.cursor_pointer().bg(rgb(C_ACCENT)).hover(|d| d.bg(rgb(C_ACCENT_HOVER))))
                                    .when(!can_save, |d| d.bg(rgb(C_SEG)).text_color(rgb(C_FAINT)))
                                    .child(icon("note").text_color(rgb(if can_save { 0xffffff } else { C_FAINT })))
                                    .child("Add as note")
                                    .child(div().opacity(0.7).child("⌘↵"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ask_to_note();
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    fn active_color(&self) -> u32 {
        match self.tool {
            Tool::Mark(k) => self.tool_colors.get(&k).copied().unwrap_or(PALETTE[0]),
            Tool::Select | Tool::Ask => PALETTE[0],
        }
    }

    fn save_settings(&self) {
        let s = Settings { sidebar_w: self.sidebar_w, minimap: self.minimap, name: self.reviewer_name.clone(), owl: self.owl_setting.clone() };
        if let Some(dir) = settings_file().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(settings_file(), serde_json::to_string_pretty(&s).unwrap());
    }

    /// Annotations panel width: what the user dragged it to, else ~30% of the window.
    fn sidebar_width(&self) -> f32 {
        let max = (self.win_w * 0.55).max(300.0);
        self.sidebar_w.unwrap_or(self.win_w * 0.3).clamp(300.0, max)
    }

    // ---------- undo ----------

    fn checkpoint(&mut self) {
        self.undo.push(self.anns.clone());
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo(&mut self, forward: bool) {
        let (from, to) = if forward { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
        let Some(state) = from.pop() else {
            self.status = if forward { "Nothing to redo" } else { "Nothing to undo" }.into();
            return;
        };
        to.push(std::mem::replace(&mut self.anns, state));
        self.editing = false;
        self.edit_checkpoint = None;
        if self.selected.as_ref().is_some_and(|id| !self.anns.iter().any(|a| &a.id == id)) {
            self.selected = None;
        }
        self.status = if forward { "Redone" } else { "Undone" }.into();
        self.save();
    }

    // ---------- positions, jumps, marks ----------

    /// Top of the viewport as (page, y in PDF points from that page's top).
    fn viewport_pos(&self) -> (usize, f32) {
        let vb = self.scroll.bounds();
        let top = vb.top() + px(1.);
        let pb = self.page_bounds.borrow();
        let page = pb
            .iter()
            .filter(|(p, _)| **p <= self.pages.len())
            .find(|(_, b)| b.top() <= top && top <= b.bottom() + px(24.))
            .map(|(p, _)| *p)
            .unwrap_or_else(|| self.current_page());
        let y = pb.get(&page).map(|b| f32::from(vb.top() - b.top()) / self.zoom).unwrap_or(0.0);
        (page, y.max(0.0))
    }

    fn goto_pos(&mut self, (page, y): (usize, f32)) {
        let Some(b) = self.page_bounds.borrow().get(&page).copied() else { return };
        let vb = self.scroll.bounds();
        let off = self.scroll.offset();
        let content_y = b.top() - vb.top() - off.y + px(y * self.zoom);
        self.set_scroll_y(-content_y);
    }

    fn set_scroll_y(&mut self, y: Pixels) {
        let max = self.scroll.max_offset().height;
        let off = self.scroll.offset();
        self.scroll.set_offset(point(off.x, y.min(px(0.)).max(-max)));
    }

    fn scroll_by(&mut self, dy: Pixels) {
        let y = self.scroll.offset().y - dy;
        self.set_scroll_y(y);
    }

    /// A "big" move: remember where we were so '' can come back.
    fn jump(&mut self, f: impl FnOnce(&mut Self)) {
        self.last_jump = Some(self.viewport_pos());
        f(self);
    }

    fn run_vim(&mut self, cmd: vim::Cmd) {
        use vim::Cmd::*;
        let screen = self.scroll.bounds().size.height;
        let n = self.pages.len().max(1);
        match cmd {
            Lines(d) => self.scroll_by(px(60.) * d as f32),
            HalfScreen(d) => self.scroll_by(screen * (0.5 * d as f32)),
            Screen(d) => self.scroll_by((screen - px(48.)) * d as f32),
            Page(d) => {
                let p = (self.current_page() as i64 + d as i64).clamp(1, n as i64) as usize;
                self.scroll_to_page(p);
            }
            FirstPage => self.jump(|s| s.scroll_to_page(1)),
            LastPage => self.jump(|s| s.scroll_to_page(n)),
            GotoPage(p) => self.jump(|s| s.scroll_to_page(p.min(n))),
            SetMark(c) => {
                let pos = self.viewport_pos();
                self.marks.insert(c, pos);
                self.status = format!("Mark {c} set on page {}", pos.0).into();
                self.save();
            }
            JumpMark(c) => match self.marks.get(&c).copied() {
                Some(pos) => self.jump(|s| s.goto_pos(pos)),
                None => self.status = format!("Mark {c} is not set").into(),
            },
            JumpBack => {
                if let Some(back) = self.last_jump {
                    let here = self.viewport_pos();
                    self.goto_pos(back);
                    self.last_jump = Some(here);
                }
            }
            SearchStart => {
                self.editing = false;
                self.search = Some(SearchState { query: String::new(), typing: true, matches: vec![], cur: 0 });
            }
            SearchNext | SearchPrev => {
                let Some(s) = &mut self.search else { return };
                if s.matches.is_empty() {
                    self.status = format!("Pattern not found: {}", s.query).into();
                    return;
                }
                let len = s.matches.len();
                s.cur = if cmd == SearchNext { (s.cur + 1) % len } else { (s.cur + len - 1) % len };
                self.jump(|s| s.reveal_hit());
            }
        }
    }

    // ---------- search ----------

    fn run_search(&mut self) {
        let Some(s) = &mut self.search else { return };
        let mut matches = vec![];
        for (i, pv) in self.pages.iter().enumerate() {
            for (a, b) in search::find(&pv.flat, &s.query) {
                let (spans, _) = pv.range(pv.flat_pos[a], pv.flat_pos[b]);
                if !spans.is_empty() {
                    let (before, matched, after) = context(&pv.flat, a, b);
                    matches.push(SearchHit { page: i + 1, spans, before, matched, after });
                }
            }
        }
        s.matches = matches;
        // Like Vim's incsearch: first match at or after the top of the view.
        let (page, y) = self.viewport_pos();
        let s = self.search.as_mut().unwrap();
        s.cur = s.matches.iter().position(|m| m.page > page || (m.page == page && m.spans[0].y >= y)).unwrap_or(0);
        self.search_scroll.scroll_to_item(s.cur);
        if !s.matches.is_empty() {
            self.reveal_hit();
        }
    }

    /// Scroll the current match into the middle of the view.
    fn reveal_hit(&mut self) {
        let Some((page, r)) = self.search.as_ref().and_then(|s| s.matches.get(s.cur)).map(|m| (m.page, m.spans[0])) else { return };
        let Some(b) = self.page_bounds.borrow().get(&page).copied() else { return };
        let vb = self.scroll.bounds();
        let off = self.scroll.offset();
        let content_y = b.top() - vb.top() - off.y + px((r.y + r.h / 2.0) * self.zoom);
        self.set_scroll_y(vb.size.height / 2.0 - content_y);
    }

    fn search_key(&mut self, k: &gpui::Keystroke) {
        let Some(s) = &mut self.search else { return };
        match k.key.as_str() {
            "escape" => {
                self.search = None;
                return;
            }
            "enter" => {
                s.typing = false;
                if s.query.trim().is_empty() {
                    self.search = None;
                } else if s.matches.is_empty() {
                    self.status = format!("Pattern not found: {}", s.query).into();
                } else {
                    self.last_jump = Some(self.viewport_pos());
                }
                return;
            }
            "up" | "down" => {
                let len = s.matches.len();
                if len > 0 {
                    s.cur = if k.key == "down" { (s.cur + 1) % len } else { (s.cur + len - 1) % len };
                    self.search_scroll.scroll_to_item(s.cur);
                    self.reveal_hit();
                }
                return;
            }
            "backspace" => {
                if s.query.pop().is_none() {
                    self.search = None;
                    return;
                }
            }
            _ => match k.key_char.clone().filter(|_| !k.modifiers.control && !k.modifiers.platform) {
                Some(t) => s.query.push_str(&t),
                None => return,
            },
        }
        self.run_search();
    }

    /// Scroll the viewer so the annotation sits in the middle of the view.
    fn scroll_to_ann(&mut self, id: &str) {
        let Some(a) = self.anns.iter().find(|a| a.id == id) else { return };
        let Some(b) = self.page_bounds.borrow().get(&a.page).copied() else { return };
        let vb = self.scroll.bounds();
        let off = self.scroll.offset();
        let mid = px((a.rect.y + a.rect.h / 2.0) * self.zoom);
        let content_y = b.top() - vb.top() - off.y + mid;
        self.scroll.set_offset(point(off.x, (vb.size.height / 2.0 - content_y).min(px(0.))));
    }

    fn scroll_to_page(&mut self, page: usize) {
        let Some(b) = self.page_bounds.borrow().get(&page).copied() else { return };
        let vb = self.scroll.bounds();
        let off = self.scroll.offset();
        let content_y = b.top() - vb.top() - off.y;
        self.scroll.set_offset(point(off.x, (px(24.) - content_y).min(px(0.))));
    }

    fn hit(&self, page: usize, p: (f32, f32)) -> Option<String> {
        self.anns
            .iter()
            .rev()
            .find(|a| {
                a.page == page
                    && if a.kind == Kind::Note {
                        self.note_rect(a).is_some_and(|r| r.contains(p, 3.0))
                    } else if a.spans.is_empty() {
                        a.rect.contains(p, 4.0)
                    } else {
                        a.spans.iter().any(|r| r.contains(p, 2.0))
                    }
            })
            .map(|a| a.id.clone())
    }

    fn delete_selected(&mut self) {
        if let Some(id) = self.selected.take() {
            self.checkpoint();
            self.anns.retain(|a| a.id != id);
            self.editing = false;
            self.save();
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if self.picker.is_some() {
            self.picker_key(k, cx);
            cx.notify();
            return;
        }
        if self.ask.is_some() {
            self.ask_key(k, cx);
            cx.notify();
            return;
        }
        if self.name_prompt.is_some() {
            self.name_key(k);
            cx.notify();
            return;
        }
        if self.search.as_ref().is_some_and(|s| s.typing) && !shortcut(k) {
            self.search_key(k);
            cx.notify();
            return;
        }
        let cmd = shortcut(k);
        if cmd {
            match k.key.as_str() {
                "z" if k.modifiers.shift => self.undo(true),
                "z" => self.undo(false),
                "f" => self.run_vim(vim::Cmd::SearchStart),
                "o" => self.show_picker(),
                "e" if k.modifiers.shift => self.send_review(),
                "e" => self.export_md(cx),
                "=" | "+" => self.zoom_to(self.zoom + 0.25, None),
                "-" => self.zoom_to(self.zoom - 0.25, None),
                "0" => self.zoom_to(1.25, None),
                "v" if self.editing => {
                    if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        self.edit(|n, c| note_edit::insert(n, c, &t));
                    }
                }
                "left" if self.editing => self.edit(|n, c| *c = note_edit::line_start(n, *c)),
                "right" if self.editing => self.edit(|n, c| *c = note_edit::line_end(n, *c)),
                "up" if self.editing => self.edit(|_, c| *c = 0),
                "down" if self.editing => self.edit(|n, c| *c = n.len()),
                "backspace" if self.editing => self.edit(|n, c| {
                    let s = note_edit::line_start(n, *c);
                    note_edit::remove(n, c, s);
                }),
                "a" if self.editing => {}
                "q" => cx.quit(),
                "w" => window.remove_window(),
                _ => return,
            }
            cx.notify();
            return;
        }
        if self.editing {
            let alt = k.modifiers.alt;
            match k.key.as_str() {
                "escape" => self.editing = false,
                "backspace" if alt => self.edit(|n, c| {
                    let s = note_edit::word_left(n, *c);
                    note_edit::remove(n, c, s);
                }),
                "backspace" => self.edit(|n, c| {
                    let s = note_edit::prev(n, *c);
                    note_edit::remove(n, c, s);
                }),
                "delete" => self.edit(|n, c| {
                    let e = note_edit::next(n, *c);
                    note_edit::remove(n, c, e);
                }),
                "left" if alt => self.edit(|n, c| *c = note_edit::word_left(n, *c)),
                "right" if alt => self.edit(|n, c| *c = note_edit::word_right(n, *c)),
                "left" => self.edit(|n, c| *c = note_edit::prev(n, *c)),
                "right" => self.edit(|n, c| *c = note_edit::next(n, *c)),
                "up" => self.edit(|n, c| *c = note_edit::up(n, *c)),
                "down" => self.edit(|n, c| *c = note_edit::down(n, *c)),
                "home" => self.edit(|n, c| *c = note_edit::line_start(n, *c)),
                "end" => self.edit(|n, c| *c = note_edit::line_end(n, *c)),
                "enter" => self.edit(|n, c| note_edit::insert(n, c, "\n")),
                _ => {
                    if let Some(t) = k.key_char.clone().filter(|_| !k.modifiers.control) {
                        self.edit(|n, c| note_edit::insert(n, c, &t));
                    }
                }
            }
            cx.notify();
            return;
        }
        let key = k.key.to_lowercase();
        if !k.modifiers.alt {
            match self.vim.feed(&key, k.modifiers.shift, k.modifiers.control) {
                vim::Feed::Cmd(c) => {
                    self.run_vim(c);
                    cx.notify();
                    return;
                }
                vim::Feed::Pending => {
                    cx.notify();
                    return;
                }
                vim::Feed::Unhandled => {}
            }
        }
        if k.modifiers.control {
            return;
        }
        self.tool = match key.as_str() {
            "escape" if self.search.is_some() => {
                self.search = None;
                self.tool
            }
            "escape" if self.palette_open => {
                self.palette_open = false;
                self.tool
            }
            "v" | "escape" => Tool::Select,
            "h" if k.modifiers.shift => Tool::Mark(Kind::Highlight),
            "h" => Tool::Mark(Kind::TextHighlight),
            "u" => Tool::Mark(Kind::Underline),
            "s" => Tool::Mark(Kind::Strike),
            "b" => Tool::Mark(Kind::Box),
            "p" => Tool::Mark(Kind::Pen),
            "c" => Tool::Mark(Kind::Note),
            "o" if self.owl_path.is_some() => Tool::Ask,
            // Enter on a selected annotation edits it; ⇧Enter asks Owl about it.
            "enter" if k.modifiers.shift && self.owl_path.is_some() && self.selected.is_some() => {
                if let Some(id) = self.selected.clone() {
                    self.ask_about_ann(&id);
                }
                self.tool
            }
            "1" | "2" | "3" | "4" | "5" => {
                if let Tool::Mark(kind) = self.tool {
                    self.tool_colors.insert(kind, PALETTE[key.parse::<usize>().unwrap() - 1]);
                }
                self.tool
            }
            "backspace" | "delete" => {
                self.delete_selected();
                self.tool
            }
            "enter" if self.selected.is_some() => {
                self.begin_edit();
                self.tool
            }
            "tab" | "]" | "[" => {
                self.step_task(key == "]" || (key == "tab" && !k.modifiers.shift));
                self.tool
            }
            "x" => {
                if let Some(id) = self.current_task.clone() {
                    self.toggle_task(&id);
                    if self.tasks.iter().any(|t| t.id == id && t.done) {
                        self.step_task(true);
                    }
                }
                self.tool
            }
            "r" => {
                self.side_tab = if self.side_tab == SideTab::Review { SideTab::Annotations } else { SideTab::Review };
                self.tool
            }
            "t" => {
                self.minimap = !self.minimap;
                self.save_settings();
                self.tool
            }
            _ => return,
        };
        cx.notify();
    }

    /// Apply an edit or caret move to the selected note, then save and keep the card visible.
    fn edit(&mut self, f: impl FnOnce(&mut String, &mut usize)) {
        let Some(id) = self.selected.clone() else { return };
        let Some(a) = self.anns.iter_mut().find(|a| a.id == id) else { return };
        let before = a.note.clone();
        self.caret = self.caret.min(a.note.len());
        let mut note = before.clone();
        f(&mut note, &mut self.caret);
        if note != before {
            if self.edit_checkpoint.as_deref() != Some(id.as_str()) {
                self.checkpoint();
                self.edit_checkpoint = Some(id.clone());
            }
            if let Some(a) = self.anns.iter_mut().find(|a| a.id == id) {
                a.note = note;
            }
            self.save();
        }
        self.reveal_card();
    }

    // ---------- views ----------

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // Grouped by purpose: marks that follow text, free shapes, comments.
        // Select is the default: Esc, V, or clicking the active tool again.
        let groups: [&[(&'static str, &'static str, &'static str, &'static str, Kind)]; 3] = [
            &[
                ("t-hl", "highlight", "Highlight", "H", Kind::TextHighlight),
                ("t-ul", "underline", "Underline", "U", Kind::Underline),
                ("t-st", "strike", "Strike", "S", Kind::Strike),
            ],
            &[
                ("t-bhl", "box-highlight", "Box highlight", "⇧H", Kind::Highlight),
                ("t-bx", "box", "Box", "B", Kind::Box),
                ("t-pen", "pen", "Pen", "P", Kind::Pen),
            ],
            &[("t-note", "note", "Note", "C", Kind::Note)],
        ];
        let title = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "PDF Review".into());
        let subtitle = match self.anns.len() {
            _ if self.pages.is_empty() => String::new(),
            1 => "1 annotation".to_string(),
            n => format!("{n} annotations"),
        };
        let palette_open = self.palette_open;

        let tool_group = |items: &[(&'static str, &'static str, &'static str, &'static str, Kind)], cx: &mut Context<Self>| {
            segment().children(items.iter().map(|&(id, icon_name, label, key, kind)| {
                let active = self.tool == Tool::Mark(kind);
                let color = self.tool_colors.get(&kind).copied().unwrap_or(PALETTE[0]);
                div()
                    .relative()
                    .h_full()
                    .child(
                        div()
                            .id(id)
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .h_full()
                            .px_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(active, |d| d.bg(with_alpha(C_ACCENT, 0.22)).text_color(rgb(0xcfe0ff)))
                            .when(!active, |d| d.text_color(rgb(C_MUTED)).hover(|d| d.bg(rgb(C_SEG_ACTIVE)).text_color(rgb(C_TEXT))))
                            // svg() does not inherit text colour, so set it on the icon itself
                            .child(icon(icon_name).size(px(17.)).text_color(rgb(if active { 0xcfe0ff } else { C_MUTED })))
                            .when(active, |d| {
                                d.child(label).child(
                                    div()
                                        .id(SharedString::from(format!("{id}-color")))
                                        .ml_0p5()
                                        .size(px(12.))
                                        .rounded_full()
                                        .bg(rgb(color))
                                        .border_1()
                                        .border_color(rgba(0xffffff66))
                                        .hover(|d| d.border_color(rgb(0xffffff)))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.palette_open = !this.palette_open;
                                            cx.notify();
                                        })),
                                )
                            })
                            .tooltip(Tip::view(label, Some(key)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tool = if this.tool == Tool::Mark(kind) { Tool::Select } else { Tool::Mark(kind) };
                                this.palette_open = false;
                                this.editing = false;
                                cx.notify();
                            })),
                    )
                    .when(active && palette_open, |d| {
                        d.child(
                            div().absolute().top(px(34.)).left_0().child(deferred(
                                anchored().snap_to_window().child(
                                    div()
                                        .occlude()
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .p_2p5()
                                        .rounded_lg()
                                        .bg(rgb(C_CARD))
                                        .border_1()
                                        .border_color(rgb(C_BORDER))
                                        .shadow_lg()
                                        .text_xs()
                                        .text_color(rgb(C_MUTED))
                                        .child(format!("{label} colour"))
                                        .child(div().flex().gap_2().children(PALETTE.into_iter().enumerate().map(|(i, c)| {
                                            div()
                                                .id(("swatch", i))
                                                .size(px(22.))
                                                .rounded_full()
                                                .cursor_pointer()
                                                .bg(rgb(c))
                                                .border_2()
                                                .border_color(if c == color { rgb(0xffffff) } else { rgb(C_CARD) })
                                                .hover(|d| d.border_color(rgba(0xffffffaa)))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.tool_colors.insert(kind, c);
                                                    this.palette_open = false;
                                                    cx.notify();
                                                }))
                                        })))
                                        .child(div().text_color(rgb(C_FAINT)).child("Keys 1–5 also set it")),
                                ),
                            )),
                        )
                    })
            }))
        };
        let mut groups_el: Vec<_> = groups.iter().map(|g| tool_group(g, cx)).collect();
        if self.owl_path.is_some() {
            let active = self.tool == Tool::Ask;
            groups_el.push(
                segment().child(
                    div()
                        .id("t-ask")
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .h_full()
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .when(active, |d| d.bg(with_alpha(0xab47bc, 0.25)).text_color(rgb(0xf0d4ff)))
                        .when(!active, |d| d.text_color(rgb(C_MUTED)).hover(|d| d.bg(rgb(C_SEG_ACTIVE)).text_color(rgb(C_TEXT))))
                        .child(icon("owl").size(px(17.)).text_color(rgb(if active { 0xf0d4ff } else { C_MUTED })))
                        .when(active, |d| d.child("Ask Owl"))
                        .tooltip(Tip::view("Ask Owl about text (drag, or ⌥-click a sentence)", Some("O")))
                        .into_any_element(),
                )
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.tool = if this.tool == Tool::Ask { Tool::Select } else { Tool::Ask };
                    this.editing = false;
                    cx.notify();
                })),
            );
        }

        div()
            .id("titlebar")
            .h(px(46.))
            .flex_none()
            .flex()
            .items_center()
            .bg(rgb(C_PANEL))
            .border_b_1()
            .border_color(rgb(C_BORDER))
            .text_sm()
            .on_click(|ev, window, _| {
                if ev.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            // Document column: title left, tools centred over the pages.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pl(px(if cfg!(target_os = "macos") { 84. } else { 12. })) // room for the traffic lights
                    .pr_3()
                    .child(
                        div().flex_1().flex_basis(px(0.)).min_w_0().flex().child(
                            div()
                                .id("doc-title")
                                .flex()
                                .min_w_0()
                                .items_baseline()
                                .gap_2()
                                .max_w_full()
                                .px_1p5()
                                .py_0p5()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|d| d.bg(rgb(C_SEG)))
                                .tooltip(Tip::view("Open another document", Some("⌘O")))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    window.focus(&this.focus);
                                    this.show_picker();
                                    cx.notify();
                                }))
                                .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                                .child(div().text_xs().text_color(rgb(C_MUTED)).flex_none().child(subtitle)),
                        ),
                    )
                    .child(div().flex_none().flex().items_center().gap_2().children(groups_el))
                    .child(div().flex_1().flex_basis(px(0.))),
            )
            // Over the annotations panel: document actions.
            .child(
                div()
                    .w(px(self.sidebar_width()))
                    .flex_none()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_3()
                    .child(
                        plain_button("undo")
                            .px_2()
                            .child(icon("undo").text_color(rgb(if self.undo.is_empty() { C_FAINT } else { C_MUTED })))
                            .tooltip(Tip::view("Undo", Some("⌘Z")))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.undo(false);
                                cx.notify();
                            })),
                    )
                    .child(
                        plain_button("open")
                            .child(icon("open").text_color(rgb(C_MUTED)))
                            .child("Open")
                            .tooltip(Tip::view("Open document", Some("⌘O")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.focus);
                                this.show_picker();
                                cx.notify();
                            })),
                    )
                    .child(
                        plain_button("send")
                            .child("Send review")
                            .tooltip(Tip::view("Save your comments, with your name, to email back", Some("⇧⌘E")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.focus);
                                this.send_review();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("md")
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .h(px(28.))
                            .px_3()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(rgb(C_ACCENT))
                            .hover(|d| d.bg(rgb(C_ACCENT_HOVER)))
                            .text_color(rgb(0xffffff))
                            .font_weight(FontWeight::MEDIUM)
                            .child(icon("export").text_color(rgb(0xffffff)))
                            .child("Export")
                            .tooltip(Tip::view("Export review as Markdown", Some("⌘E")))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_md(cx);
                                cx.notify();
                            })),
                    ),
            )
    }

    /// Under the pages: view controls, page position, and the `/` search line.
    fn bottom_bar(&self, current: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let n = self.pages.len();
        let mid: gpui::AnyElement = if let Some(s) = self.search.as_ref().filter(|s| !s.typing) {
            let count = if s.query.trim().is_empty() {
                String::new()
            } else if s.matches.is_empty() {
                "no matches".into()
            } else {
                format!("{} of {}", s.cur + 1, s.matches.len())
            };
            div()
                .flex()
                .items_center()
                .gap_3()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .min_w_0()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .when(s.typing, |d| d.bg(rgb(C_BG)).border_1().border_color(rgb(C_ACCENT)))
                        .child(div().text_color(rgb(C_ACCENT)).child("/"))
                        .child(div().truncate().child(s.query.clone()))
                        .when(s.typing, |d| d.child(div().text_color(rgb(C_ACCENT)).child("▏"))),
                )
                .child(div().flex_none().text_color(rgb(if s.matches.is_empty() && !s.query.is_empty() { 0xef5350 } else { C_MUTED })).child(count))
                .child(div().flex_none().text_color(rgb(C_FAINT)).child(if s.typing { "↵ confirm · esc cancel" } else { "n next · N prev · esc clear" }))
                .into_any_element()
        } else if let Some(p) = self.vim.pending() {
            div().text_color(rgb(C_TEXT)).child(p).into_any_element()
        } else {
            div()
                .truncate()
                .text_color(rgb(C_FAINT))
                .child("/ search · j k scroll · J K page · gg G g<n> · m<a> '<a> marks · ⌘Z undo")
                .into_any_element()
        };
        div()
            .flex_none()
            .h(px(34.))
            .flex()
            .items_center()
            .gap_3()
            .px_2()
            .bg(rgb(C_PANEL))
            .border_t_1()
            .border_color(rgb(C_BORDER))
            .text_xs()
            .child(
                seg_button("minimap", self.minimap)
                    .h(px(26.))
                    .child(icon("pages").text_color(rgb(if self.minimap { C_TEXT } else { C_MUTED })))
                    .tooltip(Tip::view("Page strip", Some("T")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.minimap = !this.minimap;
                        this.save_settings();
                        cx.notify();
                    })),
            )
            .child(div().flex_1().min_w_0().overflow_hidden().child(mid))
            .child(
                segment()
                    .h(px(26.))
                    .child(seg_button("zo", false).child(icon("minus").size(px(13.)).text_color(rgb(C_MUTED))).tooltip(Tip::view("Zoom out", Some("⌘−"))).on_click(cx.listener(|this, _, _, cx| {
                        this.zoom_to(this.zoom - 0.25, None);
                        cx.notify();
                    })))
                    .child(div().w(px(40.)).flex().justify_center().text_color(rgb(C_MUTED)).child(format!("{:.0}%", self.zoom * 100.0)))
                    .child(seg_button("zi", false).child(icon("plus").size(px(13.)).text_color(rgb(C_MUTED))).tooltip(Tip::view("Zoom in", Some("⌘+"))).on_click(cx.listener(|this, _, _, cx| {
                        this.zoom_to(this.zoom + 0.25, None);
                        cx.notify();
                    }))),
            )
            .child(
                div()
                    .flex_none()
                    .min_w(px(78.))
                    .flex()
                    .justify_end()
                    .pr_1()
                    .text_color(rgb(C_MUTED))
                    .child(if n == 0 { String::new() } else { format!("Page {current} / {n}") }),
            )
    }

    fn page(&self, i: usize, pv: &PageView, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let page = i + 1;
        let (w, h) = (pv.w * self.zoom, pv.h * self.zoom);
        let anns: Vec<Ann> = self.anns.iter().filter(|a| a.page == page).cloned().collect();
        let selected = self.selected.clone();
        let drag = self.drag.as_ref().filter(|d| d.page == page).map(|d| (d.start, d.cur, d.points.clone()));
        let tool = self.tool;
        let preview_spans: Vec<R> = match (&drag, tool) {
            (Some((s, c, _)), Tool::Mark(Kind::TextHighlight) | Tool::Ask) => match (pv.nearest_char(*s), pv.nearest_char(*c)) {
                (Some(a), Some(b)) => pv.range(a, b).0,
                _ => vec![],
            },
            _ => vec![],
        };
        let color = self.active_color();
        let zoom = self.zoom;
        let bounds_store = self.page_bounds.clone();
        let notes: Vec<_> = self
            .anns
            .iter()
            .filter(|a| a.page == page && a.kind == Kind::Note)
            .map(|a| {
                let sel = self.selected.as_deref() == Some(a.id.as_str());
                let text = if sel && self.editing {
                    let c = self.caret.min(a.note.len());
                    format!("{}▏{}", &a.note[..c], &a.note[c..])
                } else {
                    a.note.clone()
                };
                (a.id.clone(), a.rect.x, a.rect.y, text, parse_hex(&a.color), sel)
            })
            .collect();
        let order: Vec<String> = self.sorted_tasks().iter().map(|t| t.id.clone()).collect();
        let task_marks: Vec<(usize, Vec<R>, bool, bool, String, tasks::Task)> = self
            .tasks
            .iter()
            .filter(|t| !t.done || self.current_task.as_deref() == Some(t.id.as_str()))
            .filter_map(|t| {
                let (p, spans) = self.task_anchor.get(&t.id)?;
                (*p == page).then(|| {
                    let num = order.iter().position(|x| *x == t.id).map(|i| i + 1).unwrap_or(0);
                    (num, spans.clone(), self.current_task.as_deref() == Some(t.id.as_str()), t.done, t.id.clone(), t.clone())
                })
            })
            .collect();
        let task_rects: Vec<(Vec<R>, bool)> = task_marks.iter().map(|(_, s, cur, _, _, _)| (s.clone(), *cur)).collect();
        // Invisible hover zones over each item's quoted text, showing the same tooltip as the badge.
        // They have no mouse handlers, so clicks and drags still reach the page for selecting text.
        let quote_els: Vec<_> = task_marks
            .iter()
            .flat_map(|(_, spans, _, _, id, task)| {
                spans.iter().enumerate().map(move |(i, r)| {
                    let task = task.clone();
                    div()
                        .id(SharedString::from(format!("quote-{id}-{i}")))
                        .absolute()
                        .left(px(r.x * zoom))
                        .top(px(r.y * zoom))
                        .w(px(r.w * zoom))
                        .h(px(r.h * zoom))
                        .tooltip(move |_, cx| cx.new(|_| TaskTip(task.clone())).into())
                        // GPUI only hides a tooltip when the window repaints; nothing else
                        // repaints on leaving plain text, so the tooltip stuck until a click.
                        .on_hover(|_, window, _| window.refresh())
                })
            })
            .collect();
        let badge_els: Vec<_> = task_marks
            .into_iter()
            .map(|(num, spans, cur, done, id, task)| {
                let y = spans.first().map(|r| r.y).unwrap_or(12.0);
                div()
                    .id(SharedString::from(format!("badge-{id}")))
                    .absolute()
                    .left(px(8.0 * zoom))
                    .top(px(y * zoom))
                    .min_w(px(18.))
                    .h(px(18.))
                    .px_1()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(if done { 0x4caf50 } else { 0xab47bc }))
                    .when(cur, |d| d.border_2().border_color(rgb(0xffffff)))
                    .shadow_md()
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(0xffffff))
                    .cursor_pointer()
                    .child(if done { "✓".to_string() } else { num.to_string() })
                    .tooltip(move |_, cx| cx.new(|_| TaskTip(task.clone())).into())
                    .on_hover(|_, window, _| window.refresh())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.goto_task(&id);
                        cx.notify();
                    }))
            })
            .collect();
        // While typing the comment for a mark, show it in a bubble right under the mark,
        // so it is visible whatever the sidebar shows.
        let bubble = self
            .editing
            .then(|| self.selected.as_ref().and_then(|id| self.anns.iter().find(|a| &a.id == id)))
            .flatten()
            .filter(|a| a.page == page && a.kind != Kind::Note)
            .map(|a| {
                let (x, bottom) = match (a.spans.first(), a.spans.last()) {
                    (Some(f), Some(l)) => (f.x, l.y + l.h),
                    _ => (a.rect.x, a.rect.y + a.rect.h),
                };
                let c = self.caret.min(a.note.len());
                let empty = a.note.is_empty();
                let shown = format!("{}▏{}", &a.note[..c], &a.note[c..]);
                let caret_at = c..c + "▏".len();
                let left = (x * zoom).min(w - 340.0).max(4.0);
                div().absolute().left(px(left)).top(px(bottom * zoom + 8.0)).child(deferred(
                    div()
                        .id("note-bubble")
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .w(px(330.))
                        .flex()
                        .flex_col()
                        .gap_1p5()
                        .p_2p5()
                        .rounded_lg()
                        .bg(rgb(C_PANEL))
                        .border_1()
                        .border_color(rgb(C_ACCENT))
                        .shadow_2xl()
                        .font_family(".SystemUIFont")
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .text_xs()
                                .text_color(rgb(C_MUTED))
                                .child(div().size(px(8.)).rounded_full().bg(rgb(parse_hex(&a.color))))
                                .child(format!("{} · note", a.kind.label())),
                        )
                        .child(
                            div()
                                .id("note-bubble-text")
                                .max_h(px(220.))
                                .overflow_y_scroll()
                                .text_sm()
                                .text_color(rgb(C_TEXT))
                                .when(empty, |d| d.flex().child(div().text_color(rgb(C_ACCENT)).child("▏")).child(div().text_color(rgb(C_FAINT)).child("Type a note…")))
                                .when(!empty, |d| {
                                    d.child(gpui::StyledText::new(shown).with_highlights([(
                                        caret_at,
                                        gpui::HighlightStyle { color: Some(rgb(C_ACCENT).into()), ..Default::default() },
                                    )]))
                                }),
                        )
                        .child(div().text_xs().text_color(rgb(C_FAINT)).child("↵ new line · esc or click elsewhere to finish")),
                ))
            });
        let note_els: Vec<_> = notes
            .into_iter()
            .map(|(id, x, y, text, c, sel)| {
                let store = self.note_bounds.clone();
                let empty = text.is_empty();
                div()
                    .absolute()
                    .left(px(x * zoom - 3.0))
                    .top(px(y * zoom - 2.0))
                    .max_w(px(NOTE_MAX_W * zoom))
                    .px(px(3.0))
                    .py(px(1.0))
                    .rounded_sm()
                    .border_1()
                    .border_dashed()
                    .border_color(if sel { rgba(0x0088ffcc) } else { rgba(0x00000000) })
                    .text_size(px(NOTE_FONT * zoom))
                    .line_height(px(NOTE_FONT * 1.3 * zoom))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if empty { with_alpha(c, 0.45) } else { ink(c) })
                    .child(if empty { "Note".to_string() } else { text })
                    .child(
                        canvas(
                            move |b, _, _| {
                                store.borrow_mut().insert(id, b);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
            })
            .collect();
        let hits: Vec<(Vec<R>, bool)> = self
            .search
            .as_ref()
            .map(|s| s.matches.iter().enumerate().filter(|(_, m)| m.page == page).map(|(i, m)| (m.spans.clone(), i == s.cur)).collect())
            .unwrap_or_default();

        div()
            .id(("page", i))
            .relative()
            .w(px(w))
            .h(px(h))
            .flex_none()
            .bg(gpui::white())
            .shadow_lg()
            .when(tool != Tool::Select, |d| d.cursor_crosshair())
            .child(img(pv.image.clone()).w(px(w)).h(px(h)))
            .children(note_els)
            .children(quote_els)
            .children(badge_els)
            .children(bubble)
            .child(
                canvas(
                    move |b, _, _| {
                        bounds_store.borrow_mut().insert(page, b);
                    },
                    move |b, _, window, _| {
                        let o = b.origin;
                        let tr = |r: R| Bounds::new(point(o.x + px(r.x * zoom), o.y + px(r.y * zoom)), size(px(r.w * zoom), px(r.h * zoom)));
                        for a in &anns {
                            let c = parse_hex(&a.color);
                            if a.kind == Kind::TextHighlight {
                                for r in &a.spans {
                                    window.paint_quad(fill(tr(*r), with_alpha(c, 0.35)));
                                }
                            } else {
                                paint_ann(window, a.kind, a.rect, &a.points, c, o, zoom);
                            }
                            if selected.as_deref() == Some(a.id.as_str()) {
                                let mut r = a.rect;
                                r.x -= 3.0;
                                r.y -= 3.0;
                                r.w += 6.0;
                                r.h += 6.0;
                                if a.kind != Kind::Note {
                                    window.paint_quad(outline(tr(r), rgba(0x0088ffcc), BorderStyle::Dashed));
                                }
                            }
                        }
                        for (spans, current) in &task_rects {
                            for r in spans {
                                let mut r = *r;
                                r.x -= 2.0;
                                r.y -= 1.0;
                                r.w += 4.0;
                                r.h += 2.0;
                                if *current {
                                    window.paint_quad(fill(tr(r), with_alpha(0xab47bc, 0.18)));
                                }
                                window.paint_quad(outline(tr(r), with_alpha(0xab47bc, if *current { 0.95 } else { 0.55 }), BorderStyle::Dashed));
                            }
                        }
                        for (spans, current) in &hits {
                            for r in spans {
                                let mut r = *r;
                                r.x -= 1.0;
                                r.w += 2.0;
                                window.paint_quad(fill(tr(r), with_alpha(0xff9f1a, if *current { 0.55 } else { 0.28 })));
                                if *current {
                                    window.paint_quad(outline(tr(r), with_alpha(0xff7a00, 1.0), BorderStyle::Solid));
                                }
                            }
                        }
                        if drag.is_some() && tool == Tool::Ask {
                            for r in &preview_spans {
                                window.paint_quad(fill(tr(*r), with_alpha(0xab47bc, 0.3)));
                            }
                        }
                        if let (Some((s, c, pts)), Tool::Mark(kind)) = (&drag, tool) {
                            if kind == Kind::Pen {
                                paint_ann(window, kind, R::default(), pts, color, o, zoom);
                            } else if kind == Kind::TextHighlight {
                                for r in &preview_spans {
                                    window.paint_quad(fill(tr(*r), with_alpha(color, 0.35)));
                                }
                            } else {
                                let r = R::from_points(*s, *c);
                                window.paint_quad(fill(tr(r), rgba(0x0078ff22)));
                                window.paint_quad(outline(tr(r), with_alpha(color, 1.0), BorderStyle::Solid));
                            }
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    // Handled here; keep the root's "click elsewhere ends typing" from undoing a new mark.
                    cx.stop_propagation();
                    window.focus(&this.focus);
                    let Some(p) = this.to_pdf(page, ev.position) else { return };
                    match this.tool {
                        Tool::Select => {
                            this.selected = this.hit(page, p);
                            this.editing = false;
                            this.reveal_card();
                        }
                        Tool::Mark(Kind::Note) => {
                            // Clicking an existing note edits it instead of stacking a new one.
                            let on_note = this.hit(page, p).filter(|id| this.anns.iter().any(|a| &a.id == id && a.kind == Kind::Note));
                            if let Some(id) = on_note {
                                this.selected = Some(id);
                                this.begin_edit();
                            } else {
                                this.add(page, Kind::Note, R { x: p.0, y: p.1, w: 0.0, h: 0.0 }, vec![]);
                            }
                        }
                        // Alt+click with a highlight tool: highlight the sentence under the cursor.
                        Tool::Mark(Kind::TextHighlight | Kind::Highlight) if ev.modifiers.alt => {
                            let hit = this.pages.get(page - 1).and_then(|pv| pv.nearest_char(p).map(|c| pv.sentence(c)));
                            if let Some((a, b)) = hit {
                                this.add_text_highlight(page, a, b);
                            }
                        }
                        Tool::Ask if ev.modifiers.alt => {
                            let hit = this.pages.get(page - 1).and_then(|pv| pv.nearest_char(p).map(|c| pv.sentence(c)));
                            if let Some((a, b)) = hit {
                                this.ask_about_range(page, a, b);
                            }
                        }
                        Tool::Mark(_) | Tool::Ask => this.drag = Some(Drag { page, start: p, cur: p, points: vec![p] }),
                    }
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                if this.drag.as_ref().map(|d| d.page) != Some(page) {
                    return;
                }
                let Some(p) = this.to_pdf(page, ev.position) else { return };
                let d = this.drag.as_mut().unwrap();
                d.cur = p;
                d.points.push(p);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    let Some(d) = this.drag.take() else { return };
                    if this.tool == Tool::Ask {
                        let moved = (d.cur.0 - d.start.0).abs() + (d.cur.1 - d.start.1).abs() > 3.0;
                        let ends = this.pages.get(d.page - 1).and_then(|pv| Some((pv.nearest_char(d.start)?, pv.nearest_char(d.cur)?)));
                        if let (true, Some((a, b))) = (moved, ends) {
                            this.ask_about_range(d.page, a, b);
                        }
                        cx.notify();
                        return;
                    }
                    let Tool::Mark(kind) = this.tool else { return };
                    if kind == Kind::TextHighlight {
                        let moved = (d.cur.0 - d.start.0).abs() + (d.cur.1 - d.start.1).abs() > 3.0;
                        let ends = this.pages.get(d.page - 1).and_then(|pv| Some((pv.nearest_char(d.start)?, pv.nearest_char(d.cur)?)));
                        if let (true, Some((a, b))) = (moved, ends) {
                            this.add_text_highlight(d.page, a, b);
                        }
                    } else if kind == Kind::Pen {
                        if d.points.len() > 2 {
                            let r = bbox(&d.points);
                            let pts = d.points.iter().map(|(x, y)| ((x * 10.0).round() / 10.0, (y * 10.0).round() / 10.0)).collect();
                            this.add(d.page, kind, r, pts);
                        }
                    } else {
                        let r = R::from_points(d.start, d.cur);
                        if r.w > 3.0 && r.h > 2.0 {
                            this.add(d.page, kind, r, vec![]);
                        }
                    }
                    cx.notify();
                }),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut people: Vec<String> = self.anns.iter().map(|a| a.author.clone()).filter(|a| !a.is_empty()).collect();
        people.sort();
        people.dedup();
        let filter = self.author_filter.clone();
        let items: Vec<Ann> = self.sorted().into_iter().filter(|a| filter.as_ref().is_none_or(|f| &a.author == f)).collect();
        let count = items.len();
        // One chip per reviewer once others' reviews are imported.
        let chips = (!people.is_empty()).then(|| {
            let me = self.reviewer_name.clone().unwrap_or_else(|| "Me".into());
            let opts: Vec<(Option<String>, String)> = [(None, "Everyone".to_string()), (Some(String::new()), me)]
                .into_iter()
                .chain(people.into_iter().map(|p| (Some(p.clone()), p)))
                .collect();
            div()
                .flex_none()
                .flex()
                .flex_wrap()
                .gap_1()
                .px_3()
                .pt_3()
                .children(opts.into_iter().enumerate().map(|(i, (f, label))| {
                    let n = self.anns.iter().filter(|a| f.as_ref().is_none_or(|f| &a.author == f)).count();
                    let active = self.author_filter == f;
                    div()
                        .id(("who", i))
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .text_xs()
                        .cursor_pointer()
                        .bg(rgb(if active { C_ACCENT } else { C_SEG }))
                        .text_color(rgb(if active { 0xffffff } else { C_MUTED }))
                        .hover(|d| d.text_color(rgb(0xffffff)))
                        .child(format!("{label} {n}"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.author_filter = f.clone();
                            cx.notify();
                        }))
                }))
        });
        let caret = self.caret;
        let empty_msg = if self.pages.is_empty() {
            "Open a PDF to start reviewing."
        } else {
            "No annotations yet. Pick a tool and drag across the page."
        };

        let list = div()
            .id("ann-list")
            .track_scroll(&self.list_scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .when(count == 0, |d| {
                d.child(
                    div()
                        .mt_10()
                        .px_6()
                        .w_full()
                        .flex()
                        .justify_center()
                        .text_center()
                        .text_color(rgb(C_MUTED))
                        .child(empty_msg),
                )
            })
            .children(items.into_iter().map(|a| {
                let sel = self.selected.as_deref() == Some(a.id.as_str());
                let editing = sel && self.editing;
                let id = a.id.clone();
                let del_id = a.id.clone();
                let color = parse_hex(&a.color);
                div()
                    .id(SharedString::from(a.id.clone()))
                    .group("card")
                    .w_full()
                    .flex_none()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(if sel { C_CARD_SEL } else { C_CARD }))
                    .border_1()
                    .border_color(rgb(if sel { C_ACCENT } else { C_BORDER }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        window.focus(&this.focus);
                        if this.selected.as_deref() != Some(id.as_str()) || !this.editing {
                            this.selected = Some(id.clone());
                            this.begin_edit();
                            this.scroll_to_ann(&id);
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(div().size(px(8.)).rounded_full().bg(rgb(color)))
                            .child(div().font_weight(FontWeight::MEDIUM).child(a.kind.label()))
                            .child(div().text_color(rgb(C_MUTED)).child(format!("Page {}", a.page)))
                            .when(!a.author.is_empty(), |d| {
                                d.child(div().px_1p5().rounded_sm().bg(rgb(C_SEG)).text_color(rgb(C_TEXT)).child(a.author.clone()))
                            })
                            .child(div().flex_1())
                            .when(self.owl_path.is_some() && !a.text.is_empty(), |d| {
                                let ask_id = a.id.clone();
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("ask-{}", a.id)))
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .px_1()
                                        .rounded_sm()
                                        .text_color(rgb(C_FAINT))
                                        .invisible()
                                        .group_hover("card", |d| d.visible())
                                        .hover(|d| d.text_color(rgb(0xd9a8f0)).bg(rgb(C_SEG)))
                                        .child(icon("owl").size(px(12.)).text_color(rgb(0xb98ad0)))
                                        .child("Ask")
                                        .tooltip(Tip::view("Ask Owl about this passage", Some("⇧↵")))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.ask_about_ann(&ask_id);
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .id(SharedString::from(format!("del-{}", a.id)))
                                    .px_1()
                                    .rounded_sm()
                                    .text_color(rgb(C_FAINT))
                                    .invisible()
                                    .group_hover("card", |d| d.visible())
                                    .hover(|d| d.text_color(rgb(0xef5350)).bg(rgb(C_SEG)))
                                    .child("Delete")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.selected = Some(del_id.clone());
                                        this.delete_selected();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(!a.text.is_empty(), |d| {
                        d.child(
                            div()
                                .mt_2()
                                .pl_2()
                                .border_l_2()
                                .border_color(rgb(color))
                                .text_sm()
                                .text_color(rgb(0xc8c9cc))
                                .line_clamp(6)
                                .child(a.text.clone()),
                        )
                    })
                    .child(
                        div()
                            .mt_2()
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .min_h(px(34.))
                            .text_sm()
                            .bg(rgb(C_BG))
                            .border_1()
                            .border_color(rgb(if editing { C_ACCENT } else { C_BORDER }))
                            .when(a.note.is_empty() && !editing, |d| {
                                d.text_color(rgb(C_FAINT)).child("Add a note…")
                            })
                            .when(!a.note.is_empty() || editing, |d| {
                                d.child(if editing {
                                    let c = caret.min(a.note.len());
                                    format!("{}▏{}", &a.note[..c], &a.note[c..])
                                } else {
                                    a.note.clone()
                                })
                            }),
                    )
            }));

        let width = self.sidebar_width();
        let footer = if self.editing {
            SharedString::from("Typing a note · Esc to finish, then shortcuts work again")
        } else if !self.status.is_empty() {
            self.status.clone()
        } else {
            format!("{} pages · {} annotations", self.pages.len(), count).into()
        };
        div()
            .relative()
            .w(px(width))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(C_PANEL))
            .border_l_1()
            .border_color(rgb(C_BORDER))
            // drag the left edge to resize
            .child(
                div()
                    .id("sidebar-resize")
                    .absolute()
                    .top_0()
                    .left(px(-3.))
                    .w(px(6.))
                    .h_full()
                    .cursor_col_resize()
                    .hover(|d| d.bg(rgba(0x3b82f655)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.resizing = Some((ev.position.x, width));
                        }),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .h(px(40.))
                    .border_b_1()
                    .border_color(rgb(C_BORDER))
                    .px_2()
                    .child(
                        segment().h(px(30.)).text_sm().children([
                            (SideTab::Annotations, "tab-ann", "Annotations", self.anns.len().to_string()),
                            (SideTab::Review, "tab-review", "Review", {
                                let open = self.tasks.iter().filter(|t| !t.done).count();
                                if self.tasks.is_empty() { "0".into() } else { format!("{open}/{}", self.tasks.len()) }
                            }),
                        ]
                        .into_iter()
                        .map(|(tab, id, label, badge)| {
                            let on = self.side_tab == tab;
                            let alert = tab == SideTab::Review && self.tasks.iter().any(|t| !t.done);
                            seg_button(id, on)
                                .child(label)
                                .child(
                                    div()
                                        .px_1p5()
                                        .rounded_md()
                                        .text_xs()
                                        .bg(if alert { with_alpha(0xab47bc, 0.35) } else { rgb(C_BG).into() })
                                        .text_color(rgb(if alert { 0xf0d4ff } else { C_MUTED }))
                                        .child(badge),
                                )
                                .tooltip(Tip::view(if tab == SideTab::Review { "Review items from agents · Tab next · x done" } else { "Your annotations" }, Some("R")))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.side_tab = tab;
                                    cx.notify();
                                }))
                        })),
                    ),
            )
            .child(if self.side_tab == SideTab::Review {
                div().id("review-list").flex_1().min_h_0().overflow_y_scroll().p_3().child(self.review_view(cx)).into_any_element()
            } else {
                div().flex_1().min_h_0().flex().flex_col().children(chips).child(list).into_any_element()
            })
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(rgb(C_BORDER))
                    .text_xs()
                    .text_color(rgb(C_MUTED))
                    .truncate()
                    .child(footer),
            )
    }
}

const C_BG: u32 = 0x161719;
const C_PANEL: u32 = 0x1f2023;
const C_BORDER: u32 = 0x2c2e33;
const C_SEG: u32 = 0x2a2c31;
const C_SEG_ACTIVE: u32 = 0x3b3e45;
const C_CARD: u32 = 0x26282c;
const C_CARD_SEL: u32 = 0x262c38;
const C_TEXT: u32 = 0xe6e7ea;
const C_MUTED: u32 = 0x8d9097;
const C_FAINT: u32 = 0x62656c;
const C_ACCENT: u32 = 0x3b82f6;
const C_ACCENT_HOVER: u32 = 0x5593f7;

fn segment() -> gpui::Div {
    div().flex().items_center().h(px(30.)).p(px(2.)).gap(px(2.)).rounded_lg().bg(rgb(C_SEG))
}

fn seg_button(id: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1p5()
        .h_full()
        .px_2p5()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(if active { C_TEXT } else { C_MUTED }))
        .when(active, |d| d.bg(rgb(C_SEG_ACTIVE)).shadow_sm())
        .when(!active, |d| d.hover(|d| d.text_color(rgb(C_TEXT))))
}

fn plain_button(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1p5()
        .h(px(28.))
        .px_3()
        .rounded_md()
        .cursor_pointer()
        .border_1()
        .border_color(rgb(C_BORDER))
        .hover(|d| d.bg(rgb(C_SEG)))
}

fn bbox(pts: &[(f32, f32)]) -> R {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in pts {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    R { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

fn paint_ann(window: &mut Window, kind: Kind, r: R, pts: &[(f32, f32)], color: u32, o: Point<Pixels>, zoom: f32) {
    let p = |x: f32, y: f32| point(o.x + px(x * zoom), o.y + px(y * zoom));
    let rect = |x: f32, y: f32, w: f32, h: f32| Bounds::new(p(x, y), size(px(w * zoom), px(h * zoom)));
    let line_w = (1.5 / zoom).max(1.0);
    match kind {
        Kind::Highlight | Kind::TextHighlight => window.paint_quad(fill(rect(r.x, r.y, r.w, r.h), with_alpha(color, 0.35))),
        Kind::Underline => window.paint_quad(fill(rect(r.x, r.y + r.h - line_w, r.w, line_w), with_alpha(color, 1.0))),
        Kind::Strike => window.paint_quad(fill(rect(r.x, r.y + r.h / 2.0 - line_w / 2.0, r.w, line_w), with_alpha(color, 1.0))),
        Kind::Box => window.paint_quad(outline(rect(r.x, r.y, r.w, r.h), with_alpha(color, 1.0), BorderStyle::Solid)),
        Kind::Note => {} // drawn as text by `Reviewer::page`
        Kind::Pen => {
            if pts.len() < 2 {
                return;
            }
            let mut b = PathBuilder::stroke(px(2.0 * zoom.max(1.0) / 1.25));
            b.move_to(p(pts[0].0, pts[0].1));
            for &(x, y) in &pts[1..] {
                b.line_to(p(x, y));
            }
            if let Ok(path) = b.build() {
                window.paint_path(path, with_alpha(color, 1.0));
            }
        }
    }
}

fn is_pua(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

/// Some fonts (e.g. Inter's case-sensitive forms) emit private-use code points for
/// alternate glyphs such as raised parentheses, and embedded subsets have no cmap to look
/// them up. Recover the real character: run each page through a device that records glyph
/// ids per font, then match the private-use glyph's outline (up to translation) against the
/// outlines of ordinary characters drawn in the same font.
fn resolve_pua(doc: &Document) -> HashMap<(String, char), char> {
    #[derive(Default)]
    struct Seen {
        fonts: HashMap<String, mupdf::Font>,
        known: HashMap<String, HashMap<i32, char>>, // font -> gid -> char
        pua: HashMap<(String, char), i32>,          // (font, char) -> gid
    }
    struct Collector(Rc<RefCell<Seen>>);
    impl mupdf::NativeDevice for Collector {
        fn fill_text(&mut self, text: &mupdf::Text, _: Matrix, _: &Colorspace, _: &[f32], _: f32, _: mupdf::ColorParams) {
            let mut seen = self.0.borrow_mut();
            for span in text.spans() {
                let font = span.font();
                let name = font.name().to_string();
                for item in span.items() {
                    let Some(ch) = char::from_u32(item.ucs() as u32) else { continue };
                    if item.gid() <= 0 {
                        continue;
                    }
                    if is_pua(ch) {
                        seen.pua.insert((name.clone(), ch), item.gid());
                    } else if !ch.is_whitespace() {
                        seen.known.entry(name.clone()).or_default().entry(item.gid()).or_insert(ch);
                    }
                }
                seen.fonts.entry(name).or_insert(font);
            }
        }
    }

    let seen = Rc::new(RefCell::new(Seen::default()));
    for i in 0..doc.page_count().unwrap_or(0) {
        let (Ok(page), Ok(dev)) = (doc.load_page(i), mupdf::Device::from_native(Collector(seen.clone()))) else { continue };
        let _ = page.run(&dev, &Matrix::IDENTITY);
    }
    let seen = seen.borrow();
    let mut out = HashMap::new();
    for ((name, ch), gid) in &seen.pua {
        let (Some(font), Some(shape)) = (seen.fonts.get(name), seen.fonts.get(name).and_then(|f| glyph_shape(f, *gid))) else { continue };
        // Exact outline in the same font first.
        let found = seen.known.get(name).and_then(|known| {
            known.iter().find(|(g, _)| glyph_shape(font, **g).is_some_and(|s| same_shape(&s, &shape))).map(|(_, c)| *c)
        });
        // Otherwise the closest size-normalised outline in another weight of the same family.
        let found = found.or_else(|| {
            let fam = family(name);
            let target = normalize(&shape)?;
            let mut best: Option<(f32, char)> = None;
            for (other, known) in seen.known.iter().filter(|(n, _)| family(n) == fam) {
                let Some(f) = seen.fonts.get(other) else { continue };
                for (g, c) in known {
                    let Some(cand) = glyph_shape(f, *g).and_then(|s| normalize(&s)) else { continue };
                    if cand.len() != target.len() {
                        continue;
                    }
                    let d = cand.iter().zip(&target).map(|(a, b)| (a.0 - b.0).abs().max((a.1 - b.1).abs())).fold(0.0, f32::max);
                    if d < 0.12 && best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, *c));
                    }
                }
            }
            best.map(|(_, c)| c)
        });
        out.insert((name.clone(), *ch), found.unwrap_or('\u{FFFD}'));
    }
    out
}

/// Outline points of glyph `gid`, in 1/1000 em, relative to the first point.
fn glyph_shape(font: &mupdf::Font, gid: i32) -> Option<Vec<(i32, i32)>> {
    struct W(Vec<(f32, f32)>);
    impl mupdf::PathWalker for W {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push((x, y));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push((x, y));
        }
        fn curve_to(&mut self, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) {
            self.0.extend([(a, b), (c, d), (e, f)]);
        }
        fn close(&mut self) {}
    }
    let path = font.outline_glyph(gid).ok().flatten()?;
    let mut w = W(vec![]);
    path.walk(&mut w).ok()?;
    let (x0, y0) = *w.0.first()?;
    Some(w.0.iter().map(|(x, y)| (((x - x0) * 1000.0).round() as i32, ((y - y0) * 1000.0).round() as i32)).collect())
}

/// "ABCDEF+Inter-SemiBold" -> "Inter"
fn family(font_name: &str) -> &str {
    let n = font_name.split_once('+').map(|(_, r)| r).unwrap_or(font_name);
    n.split(['-', ',']).next().unwrap_or(n)
}

/// Outline scaled so its bounding box's larger side is 1.
fn normalize(shape: &[(i32, i32)]) -> Option<Vec<(f32, f32)>> {
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for &(x, y) in shape {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let size = (x1 - x0).max(y1 - y0) as f32;
    (size > 0.0).then(|| shape.iter().map(|&(x, y)| ((x - x0) as f32 / size, (y - y0) as f32 / size)).collect())
}

fn same_shape(a: &[(i32, i32)], b: &[(i32, i32)]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(p, q)| (p.0 - q.0).abs() <= 2 && (p.1 - q.1).abs() <= 2)
}

fn load_pages(path: &PathBuf) -> anyhow::Result<Vec<PageView>> {
    let doc = Document::open(path.to_str().unwrap_or_default())?;
    let mut pages = vec![];
    let pua = resolve_pua(&doc);
    for i in 0..doc.page_count()? {
        let page = doc.load_page(i)?;
        let b = page.bounds()?;
        let pix = page.to_pixmap(&Matrix::new_scale(RENDER_SCALE, RENDER_SCALE), &Colorspace::device_rgb(), false, true)?;
        let (pw, ph, n) = (pix.width(), pix.height(), pix.n() as usize);
        let stride = pix.stride() as usize;
        let src = pix.samples();
        // GPUI wants BGRA
        let mut bgra = Vec::with_capacity((pw * ph * 4) as usize);
        for y in 0..ph as usize {
            for x in 0..pw as usize {
                let o = y * stride + x * n;
                bgra.extend_from_slice(&[src[o + 2], src[o + 1], src[o], 255]);
            }
        }
        let buf = image::RgbaImage::from_raw(pw, ph, bgra).ok_or_else(|| anyhow::anyhow!("bad pixmap"))?;
        let image = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(buf)]));

        let mut lines = vec![];
        let tp = page.to_text_page(TextPageFlags::empty())?;
        for (bi, block) in tp.blocks().enumerate() {
            for line in block.lines() {
                let lb = line.bounds();
                let chars = line
                    .chars()
                    .filter_map(|c| {
                        let q = c.quad();
                        let ch = c.char()?;
                        let ch = if is_pua(ch) {
                            c.font().and_then(|f| pua.get(&(f.name().to_string(), ch)).copied()).unwrap_or('\u{FFFD}')
                        } else {
                            ch
                        };
                        Some((ch, q.ul.x.min(q.ll.x) - b.x0, q.ur.x.max(q.lr.x) - b.x0))
                    })
                    .collect();
                lines.push(TextLine { block: bi, bbox: R { x: lb.x0 - b.x0, y: lb.y0 - b.y0, w: lb.x1 - lb.x0, h: lb.y1 - lb.y0 }, chars });
            }
        }
        let (flat, flat_pos) = flatten(&lines);
        pages.push(PageView { image, w: b.x1 - b.x0, h: b.y1 - b.y0, lines, flat, flat_pos });
    }
    Ok(pages)
}

impl Render for Reviewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.win_w = f32::from(window.viewport_size().width);
        let mut pages = vec![];
        for (i, pv) in self.pages.iter().enumerate() {
            pages.push(self.page(i, pv, cx));
        }
        let overlay = self.picker.is_some().then(|| self.picker_view(cx));
        let search_overlay = self.search.as_ref().is_some_and(|s| s.typing).then(|| self.search_view(cx));
        let ask_overlay = self.ask.is_some().then(|| self.ask_view(cx));
        let name_overlay = self.name_prompt.is_some().then(|| self.name_view());
        let current = self.current_page();
        if current != self.last_page {
            self.last_page = current;
            self.minimap_scroll.scroll_to_item(current - 1);
        }
        let minimap = (self.minimap && !self.pages.is_empty()).then(|| self.minimap_view(current, cx));
        let bottom = self.bottom_bar(current, cx);
        let weak = cx.entity().downgrade();
        let focus = self.focus.clone();
        // Any click gives keyboard focus back to the root, so shortcuts keep working.
        let refocus = canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                window.on_mouse_event(move |_: &MouseDownEvent, phase, window, _| {
                    if phase == DispatchPhase::Capture {
                        window.focus(&focus);
                    }
                });
            },
        )
        .absolute()
        .size_0();
        // Ctrl/⌘ + scroll zooms. Capture phase so the viewer does not also scroll.
        let zoom_wheel = canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, _window, cx| {
                    if phase != DispatchPhase::Capture || !(ev.modifiers.control || ev.modifiers.platform) {
                        return;
                    }
                    cx.stop_propagation();
                    let dy = match ev.delta {
                        ScrollDelta::Pixels(p) => f32::from(p.y),
                        ScrollDelta::Lines(l) => l.y * 20.0,
                    };
                    let pos = ev.position;
                    weak.update(cx, |this, cx| {
                        this.zoom_to(this.zoom * (dy * 0.01).exp(), Some(pos));
                        cx.notify();
                    })
                    .ok();
                });
            },
        )
        .absolute()
        .size_0();
        div()
            .relative()
            .child(zoom_wheel)
            .child(refocus)
            // A click on anything that does not handle it itself (viewer background, panels)
            // ends note typing.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.editing || this.palette_open {
                        this.editing = false;
                        this.palette_open = false;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                this.mouse = ev.position;
                if let Some((x0, w0)) = this.resizing {
                    this.sidebar_w = Some(w0 + f32::from(x0 - ev.position.x));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    if this.resizing.take().is_some() {
                        this.sidebar_w = Some(this.sidebar_width());
                        this.save_settings();
                    }
                }),
            )
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(C_BG))
            .text_color(rgb(C_TEXT))
            .font_family(".SystemUIFont")
            .child(self.toolbar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .children(minimap)
                    .child(
                        div().flex_1().h_full().min_w_0().flex().flex_col().child(
                        div()
                            .id("viewer")
                            .track_scroll(&self.scroll)
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .overflow_scroll()
                            .child(
                                div()
                                    .min_w_full()
                                    .py_8()
                                    .px_8()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap_6()
                                    .children(pages),
                            ),
                        )
                        .child(bottom),
                    )
                    .child(self.sidebar(cx)),
            )
            .children(search_overlay)
            .children(ask_overlay)
            .children(name_overlay)
            .children(overlay)
    }
}

/// Toolbar icons, compiled into the binary.
struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let bytes: &'static [u8] = match path {
        "icons/select.svg" => include_bytes!("../assets/icons/select.svg"),
        "icons/highlight.svg" => include_bytes!("../assets/icons/highlight.svg"),
        "icons/box-highlight.svg" => include_bytes!("../assets/icons/box-highlight.svg"),
        "icons/underline.svg" => include_bytes!("../assets/icons/underline.svg"),
        "icons/strike.svg" => include_bytes!("../assets/icons/strike.svg"),
        "icons/box.svg" => include_bytes!("../assets/icons/box.svg"),
        "icons/owl.svg" => include_bytes!("../assets/icons/owl.svg"),
        "icons/undo.svg" => include_bytes!("../assets/icons/undo.svg"),
        "icons/pen.svg" => include_bytes!("../assets/icons/pen.svg"),
        "icons/note.svg" => include_bytes!("../assets/icons/note.svg"),
        "icons/pages.svg" => include_bytes!("../assets/icons/pages.svg"),
        "icons/open.svg" => include_bytes!("../assets/icons/open.svg"),
        "icons/export.svg" => include_bytes!("../assets/icons/export.svg"),
        "icons/minus.svg" => include_bytes!("../assets/icons/minus.svg"),
        "icons/plus.svg" => include_bytes!("../assets/icons/plus.svg"),
            _ => return Ok(None),
        };
        Ok(Some(std::borrow::Cow::Borrowed(bytes)))
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(vec![])
    }
}

fn icon(name: &str) -> gpui::Svg {
    gpui::svg().path(format!("icons/{name}.svg")).size(px(15.)).flex_none()
}

/// Small hover label: name plus shortcut.
struct Tip {
    label: SharedString,
    key: Option<SharedString>,
}

impl Tip {
    fn view(label: &'static str, key: Option<&'static str>) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
        move |_, cx| cx.new(|_| Tip { label: label.into(), key: key.map(Into::into) }).into()
    }
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .mt_1()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(C_CARD))
            .border_1()
            .border_color(rgb(C_BORDER))
            .shadow_md()
            .text_xs()
            .text_color(rgb(C_TEXT))
            .font_family(".SystemUIFont")
            .child(self.label.clone())
            .children(self.key.clone().map(|k| div().px_1().rounded_sm().bg(rgb(C_SEG)).text_color(rgb(C_MUTED)).child(k)))
    }
}

/// Hover card for a review badge: the item's title, detail and origin.
struct TaskTip(tasks::Task);

impl Render for TaskTip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = &self.0;
        let meta = [t.priority.as_str(), t.from.as_str(), t.list.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        div()
            .mt_1()
            // A fixed width so wrapped lines are measured correctly; with only max_w the
            // tooltip was sized for unwrapped text and the lines overlapped.
            .w(px(340.))
            .flex()
            .flex_col()
            .gap_1()
            .line_height(px(16.))
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(C_CARD))
            .border_1()
            .border_color(rgb(C_BORDER))
            .shadow_md()
            .text_xs()
            .text_color(rgb(C_TEXT))
            .font_family(".SystemUIFont")
            .child(div().w_full().font_weight(FontWeight::SEMIBOLD).child(if t.done { format!("✓ {}", t.title) } else { t.title.clone() }))
            .when(!t.detail.is_empty(), |d| d.child(div().w_full().child(t.detail.clone())))
            .child(div().w_full().text_color(rgb(C_MUTED)).child(meta))
    }
}

#[cfg(target_os = "macos")]
/// Dock icon when running the bare binary (cargo run); a bundled .app uses AppIcon.icns.
fn set_dock_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;
    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(include_bytes!("../assets/icon.png"));
    if let Some(img) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&img)) };
    }
}

fn main() {
    // `pdf-review --text file.pdf`: print each page's text exactly as the app searches it,
    // so agents can copy quotes for review lists (see REVIEW_LISTS.md).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--text") {
        let Some(file) = args.get(2) else {
            eprintln!("usage: pdf-review --text <file.pdf>");
            std::process::exit(2);
        };
        match load_pages(&PathBuf::from(file)) {
            Ok(pages) => {
                for (i, pv) in pages.iter().enumerate() {
                    println!("=== page {} ===", i + 1);
                    println!("{}", pv.flat.iter().collect::<String>());
                }
            }
            Err(e) => {
                eprintln!("could not read {file}: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let arg = std::env::args().nth(1).map(PathBuf::from);
    // Files opened from Finder / "Open With" arrive as file:// URLs, possibly before launch finishes.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<PathBuf>();
    let app = Application::new().with_assets(Assets);
    app.on_open_urls(move |urls| {
        for u in urls {
            if let Some(p) = u.strip_prefix("file://") {
                open_tx.unbounded_send(PathBuf::from(percent_decode(p))).ok();
            }
        }
    });
    app.run(move |cx: &mut App| {
        #[cfg(target_os = "macos")]
        set_dock_icon();
        let bounds = Bounds::centered(None, size(px(1400.), px(950.)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("PDF Review".into()),
                    // macOS draws its traffic lights over our titlebar; Windows needs its native one.
                    appears_transparent: cfg!(target_os = "macos"),
                    traffic_light_position: Some(point(px(14.), px(15.))),
                }),
                window_background: WindowBackgroundAppearance::Opaque,
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| Reviewer::new(arg, cx));
                window.focus(&view.read(cx).focus);
                view
            },
        )
        .unwrap();
        cx.spawn(async move |cx| {
            use futures::StreamExt;
            while let Some(path) = open_rx.next().await {
                window
                    .update(cx, |view, _, cx| {
                        view.open(path);
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        // Closing the window (red button or ⌘W) quits, like a single-document app.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_urls() {
        assert_eq!(percent_decode("/Users/x/My%20Doc%C3%A5.pdf"), "/Users/x/My Docå.pdf");
        assert_eq!(percent_decode("/a/100%"), "/a/100%");
    }

    #[test]
    fn search_spans_lines_in_pdf() {
        let pages = load_pages(&PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sentences.pdf"))).unwrap();
        let pv = &pages[0];
        // "get in" ends one line, "through" starts the next
        let hits = search::find(&pv.flat, "get in through");
        assert_eq!(hits.len(), 1);
        let (a, b) = hits[0];
        let (spans, text) = pv.range(pv.flat_pos[a], pv.flat_pos[b]);
        assert_eq!(text, "get in through");
        assert_eq!(spans.len(), 2);
        // smartcase: "brp" matches BRP, "Brp" does not
        assert_eq!(search::find(&pv.flat, "brp").len(), 1);
        assert_eq!(search::find(&pv.flat, "Brp").len(), 0);
    }

    #[test]
    fn search_context() {
        let flat: Vec<char> = "Members get in\nthrough BRP and payment.".chars().collect();
        let a = flat.iter().collect::<String>().find("BRP").unwrap(); // ASCII, so byte index == char index
        assert_eq!(flat[a..a + 3].iter().collect::<String>(), "BRP");
        let (b, m, af) = context(&flat, a, a + 2);
        assert_eq!((b.as_str(), m.as_str(), af.as_str()), ("Members get in through ", "BRP", " and payment."));
        // at the very start/end: no stray spaces, no panics
        let (b, m, af) = context(&flat, 0, 6);
        assert_eq!((b.as_str(), m.as_str()), ("", "Members"));
        assert!(af.starts_with(" get in"));
        let n = flat.len();
        let (_, m, af) = context(&flat, n - 8, n - 1);
        assert_eq!((m.as_str(), af.as_str()), ("payment.", ""));
    }

    #[test]
    fn note_caret() {
        use note_edit::*;
        let mut s = String::from("hello wörld\nab");
        let mut c = s.len();
        assert_eq!(prev(&s, c), c - 1);
        c = up(&s, c); // col 2 on line 1
        assert_eq!(&s[..c], "he");
        c = down(&s, c);
        assert_eq!(c, s.len());
        c = line_start(&s, c);
        c = up(&s, c);
        assert_eq!(c, 0);
        c = word_right(&s, c);
        c = word_right(&s, c);
        assert_eq!(&s[..c], "hello wörld");
        assert_eq!(&s[..word_left(&s, c)], "hello ");
        let mut c = 5;
        insert(&mut s, &mut c, ",");
        assert_eq!(&s[..c], "hello,");
        let p = prev(&s, c);
        remove(&mut s, &mut c, p);
        assert_eq!(s, "hello wörld\nab");
        let mut c = 9; // after "ö"? ö is 2 bytes at 7..9
        let p = prev(&s, c);
        remove(&mut s, &mut c, p);
        assert_eq!(s, "hello wrld\nab");
    }

    #[test]
    fn sentence_and_range() {
        let pages = load_pages(&PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sentences.pdf"))).unwrap();
        let pv = &pages[0];
        for l in &pv.lines {
            let t: String = l.chars.iter().map(|c| c.0).collect();
            println!("block {} y {:.0}: {t}", l.block, l.bbox.y);
        }
        // click on "fails" (inside the 2nd sentence of the paragraph)
        let li = pv.lines.iter().position(|l| l.chars.iter().map(|c| c.0).collect::<String>().contains("fails")).unwrap();
        let line: String = pv.lines[li].chars.iter().map(|c| c.0).collect();
        let ci = line.chars().collect::<Vec<_>>().iter().position(|_| false).unwrap_or(line.find("fails").unwrap());
        let (a, b) = pv.sentence((li, ci));
        let (spans, text) = pv.range(a, b);
        println!("sentence: {text:?} spans {}", spans.len());
        assert_eq!(text, "If this fails, members cannot get in.");
        assert_eq!(spans.len(), 2);
        // click on "people" -> sentence spanning two lines
        let li = pv.lines.iter().position(|l| l.chars.iter().map(|c| c.0).collect::<String>().contains("people")).unwrap();
        let line: String = pv.lines[li].chars.iter().map(|c| c.0).collect();
        let (a, b) = pv.sentence((li, line.find("people").unwrap()));
        let (spans, text) = pv.range(a, b);
        println!("sentence: {text:?} spans {}", spans.len());
        assert_eq!(text, "This policy holds the people who protect it accountable.");
        assert_eq!(spans.len(), 2);
    }
}

