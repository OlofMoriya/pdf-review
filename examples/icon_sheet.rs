//! Renders assets/icons/*.svg side by side (light on dark) to the path given as argv[1].
use resvg::{tiny_skia, usvg};

fn main() {
    let out = std::env::args().nth(1).expect("output path");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icons");
    let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
    names.sort();
    let (cell, scale) = (72u32, 2.0f32);
    let mut sheet = tiny_skia::Pixmap::new(cell * names.len() as u32, cell).unwrap();
    sheet.fill(tiny_skia::Color::from_rgba8(0x1f, 0x20, 0x23, 255));
    for (i, p) in names.iter().enumerate() {
        let svg = std::fs::read_to_string(p).unwrap().replace("#000", "#e6e7ea");
        let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).unwrap();
        let off = (cell as f32 - 24.0 * scale) / 2.0;
        let t = tiny_skia::Transform::from_scale(scale, scale).post_translate(i as f32 * cell as f32 + off, off);
        resvg::render(&tree, t, &mut sheet.as_mut());
        println!("{i}: {}", p.file_stem().unwrap().to_string_lossy());
    }
    sheet.save_png(out).unwrap();
}
