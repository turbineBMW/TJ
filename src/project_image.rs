//! A project's picture: an icon found in the project, or one the user picks.
//!
//! [`find_icon`] looks through the top few levels of a project for the file most likely to be its
//! icon or logo: one named like `icon`, `logo`, `favicon`, `ic_launcher` or the project itself, or
//! sitting in an `icons` folder. Square, large raster images win; tiny ones, banners and
//! wordmarks lose. Build output, dependencies and tests are skipped.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
};

/// How many folders down to look (Android's `app/src/main/res/mipmap-*` is five).
const MAX_DEPTH: usize = 5;
/// Stop after this many directory entries, so a huge project can't stall the search.
const MAX_ENTRIES: usize = 5_000;
/// Files bigger than this aren't icons.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Folders that hold build output, dependencies or test data rather than the project's own art.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "out",
    "venv",
    "__pycache__",
    "pods",
    "deriveddata",
    "coverage",
    "test",
    "tests",
    "__tests__",
    "spec",
    "fixtures",
    "examples",
    "third_party",
    "third-party",
    "deps",
    "subprojects",
    "external",
    "extern",
    "thirdparty",
    "3rdparty",
    "site-packages",
];
/// Hidden folders that can still hold a project icon (JetBrains keeps one in `.idea/icon.svg`).
const HIDDEN_DIRS_ALLOWED: &[&str] = &[".github", ".idea"];
/// Folders whose images are icons whatever they're called.
const ICON_DIRS: &[&str] = &["icon", "icons", "logo", "logos", "brand", "branding"];
/// Names that mark an image as something other than the icon.
const NOT_ICON: &[&str] =
    &["banner", "screenshot", "wordmark", "horizontal", "social", "preview", "symbolic", "tray", "badge", "splash"];

/// Image formats TJ can draw, and how much each is preferred. A large PNG edges out an SVG: it
/// looks the same at badge size and is much cheaper to draw (app SVGs often just wrap a PNG).
fn format_score(ext: &str) -> Option<i32> {
    Some(match ext {
        "svg" => 9,
        "png" => 10,
        "webp" => 6,
        "jpg" | "jpeg" => 4,
        "ico" => 3,
        "gif" => 2,
        _ => return None,
    })
}

/// The best icon in `root`, if it has one. `name` is the project's name, so `tj.svg` counts in TJ.
pub fn find_icon(root: &Path, name: &str) -> Option<PathBuf> {
    let name = normalize(name);
    let mut best: Option<(Candidate, PathBuf)> = None;
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut seen = 0;
    // Breadth first, so the shallow folders are covered before the entry budget runs out.
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            seen += 1;
            if seen > MAX_ENTRIES {
                return best.map(|(_, path)| path);
            }
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().to_lowercase();
            if kind.is_dir() {
                let hidden = file_name.starts_with('.') && !HIDDEN_DIRS_ALLOWED.contains(&file_name.as_str());
                if depth < MAX_DEPTH && !hidden && !SKIP_DIRS.contains(&file_name.as_str()) {
                    queue.push_back((path, depth + 1));
                }
            } else if kind.is_file() {
                let relative = path.strip_prefix(root).unwrap_or(&path);
                if let Some(candidate) = score(&path, relative, &name)
                    && best.as_ref().is_none_or(|(b, _)| candidate > *b)
                {
                    best = Some((candidate, path));
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Whether `path` is an image TJ can show (checked by reading it, not by its name alone).
pub fn is_image(path: &Path) -> bool {
    let ext = extension(path);
    if ext == "svg" {
        return svg_size(path).is_some();
    }
    format_score(&ext).is_some() && image::image_dimensions(path).is_ok()
}

/// Ordered by score, then by pixel count, then preferring the shorter path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Candidate {
    score: i32,
    area: u64,
    shortness: i64,
}

fn score(path: &Path, relative: &Path, project: &str) -> Option<Candidate> {
    let ext = extension(path);
    let format = format_score(&ext)?;
    let stem = path.file_stem()?.to_string_lossy().to_lowercase();
    let dirs: Vec<String> = relative
        .parent()
        .map(|p| p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect())
        .unwrap_or_default();

    let first_word = stem.split(|c: char| !c.is_alphanumeric()).find(|w| !w.is_empty()).unwrap_or("");
    let named_for_project = project.len() >= 2 && (normalize(&stem) == project || first_word == project);
    let mut score = if stem.contains("logo") {
        30
    } else if stem.contains("icon") || stem.starts_with("ic_launcher") {
        28
    } else if named_for_project {
        24
    } else if dirs
        .iter()
        .any(|d| ICON_DIRS.contains(&d.as_str()) || d.ends_with(".appiconset") || d.starts_with("mipmap"))
    {
        12
    } else {
        return None;
    };
    if stem.contains("favicon") {
        // Usually a small, simplified version of the real icon.
        score -= 6;
    }
    if NOT_ICON.iter().any(|w| stem.contains(w)) {
        score -= 15;
    }
    score += format;
    score -= 2 * dirs.len() as i32;

    if fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let (width, height) = if ext == "svg" { svg_size(path)? } else { image::image_dimensions(path).ok()? };
    let (short, long) = (width.min(height).max(1), width.max(height));
    if long as f32 / short as f32 > 1.3 {
        // A banner or a wordmark, not an icon.
        score -= 25;
    }
    let size = match short {
        0..32 => -15,
        32..64 => -5,
        64..128 => 0,
        128..256 => 5,
        _ => 8,
    };
    // An SVG's size is only its design size, so a small one isn't held against it.
    score += if ext == "svg" { size.max(0) } else { size };
    let shortness = -(relative.as_os_str().len() as i64);
    Some(Candidate { score, area: width as u64 * height as u64, shortness })
}

/// An SVG's width and height, from its `viewBox` or its `width`/`height` attributes; `None` if
/// the file isn't an SVG.
fn svg_size(path: &Path) -> Option<(u32, u32)> {
    if fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let start = text.find("<svg")?;
    let tag = &text[start..start + text[start..].find('>')?];
    let attr = |name: &str| -> Option<&str> {
        let at = tag.find(&format!(" {name}=")).or_else(|| tag.find(&format!("\n{name}=")))? + name.len() + 2;
        let quote = tag[at..].chars().next()?;
        let value = &tag[at + 1..];
        Some(&value[..value.find(quote)?])
    };
    let number = |s: &str| -> Option<f32> { s.trim().trim_end_matches("px").parse::<f32>().ok().filter(|n| *n > 0.) };
    let from_view_box = attr("viewBox").and_then(|v| {
        let parts: Vec<f32> = v.split([' ', ',']).filter_map(|p| p.trim().parse().ok()).collect();
        (parts.len() == 4 && parts[2] > 0. && parts[3] > 0.).then(|| (parts[2], parts[3]))
    });
    let from_attrs = attr("width").and_then(number).zip(attr("height").and_then(number));
    // No size at all: usvg draws it at 100×100.
    let (w, h) = from_view_box.or(from_attrs).unwrap_or((100., 100.));
    Some((w.round().max(1.) as u32, h.round().max(1.) as u32))
}

fn extension(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// "Nautilus-Omarchy" -> "nautilusomarchy".
fn normalize(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::{find_icon, is_image};
    use std::path::{Path, PathBuf};

    fn project(files: &[(&str, (u32, u32))]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tj-icon-{}", crate::store::new_id()));
        for (path, (w, h)) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if path.extension().is_some_and(|e| e == "svg") {
                std::fs::write(&path, format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}"/>"#))
                    .unwrap();
            } else if path.extension().is_some_and(|e| e == "png") && *w > 0 {
                image::RgbaImage::new(*w, *h).save(&path).unwrap();
            } else {
                std::fs::write(&path, "not an image").unwrap();
            }
        }
        root
    }

    fn found(root: &Path, name: &str) -> Option<String> {
        find_icon(root, name).map(|p| p.strip_prefix(root).unwrap().display().to_string())
    }

    #[test]
    fn picks_a_large_icon_named_for_the_project() {
        let root = project(&[
            ("data/icons/tj-16.png", (16, 16)),
            ("data/icons/tj-512.png", (512, 512)),
            ("data/icons/tj-64.png", (64, 64)),
            ("README.md", (0, 0)),
        ]);
        assert_eq!(found(&root, "TJ").as_deref(), Some("data/icons/tj-512.png"));
    }

    #[test]
    fn prefers_a_logo_and_skips_dependencies_tests_and_banners() {
        let root = project(&[
            ("node_modules/pkg/logo.png", (512, 512)),
            ("tests/fixtures/icon.png", (512, 512)),
            ("docs/banner-logo.png", (1200, 300)),
            ("public/favicon.png", (32, 32)),
            ("public/logo.svg", (24, 24)),
        ]);
        assert_eq!(found(&root, "site").as_deref(), Some("public/logo.svg"));
    }

    #[test]
    fn finds_tauri_and_android_icons() {
        let tauri = project(&[("src-tauri/icons/128x128.png", (128, 128)), ("src-tauri/icons/icon.png", (512, 512))]);
        assert_eq!(found(&tauri, "app").as_deref(), Some("src-tauri/icons/icon.png"));
        let android = project(&[("app/src/main/res/mipmap-xxxhdpi/ic_launcher.png", (192, 192))]);
        assert_eq!(found(&android, "app").as_deref(), Some("app/src/main/res/mipmap-xxxhdpi/ic_launcher.png"));
    }

    #[test]
    fn nothing_when_there_is_no_icon() {
        let root = project(&[("src/main.rs", (0, 0)), ("assets/photo.png", (800, 600)), ("icon.png", (0, 0))]);
        assert_eq!(found(&root, "demo"), None, "a photo isn't an icon, and a broken icon.png is skipped");
    }

    #[test]
    fn checks_images_by_their_contents() {
        let root = project(&[("a.png", (8, 8)), ("b.svg", (10, 10)), ("fake.png", (0, 0))]);
        assert!(is_image(&root.join("a.png")));
        assert!(is_image(&root.join("b.svg")));
        assert!(!is_image(&root.join("fake.png")));
        assert!(!is_image(&root.join("missing.png")));
    }
}
