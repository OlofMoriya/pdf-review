//! Renders assets/icon.svg to assets/icon.png (1024px) and assets/AppIcon.icns.
//! Run with: cargo run --example make_icon

use std::{path::Path, process::Command};

fn render(svg: &resvg::usvg::Tree, size: u32, out: &Path) {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
    let scale = size as f32 / svg.size().width();
    resvg::render(svg, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.save_png(out).unwrap();
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let data = std::fs::read(root.join("icon.svg")).unwrap();
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).unwrap();

    render(&tree, 1024, &root.join("icon.png"));

    let set = std::env::temp_dir().join("pdf-review-AppIcon.iconset");
    let _ = std::fs::remove_dir_all(&set);
    std::fs::create_dir_all(&set).unwrap();
    for base in [16, 32, 128, 256, 512] {
        render(&tree, base, &set.join(format!("icon_{base}x{base}.png")));
        render(&tree, base * 2, &set.join(format!("icon_{base}x{base}@2x.png")));
    }
    let status = Command::new("iconutil")
        .args(["-c", "icns", "-o"])
        .arg(root.join("AppIcon.icns"))
        .arg(&set)
        .status()
        .expect("iconutil");
    assert!(status.success(), "iconutil failed");
    println!("wrote {}", root.display());
}
