//! What the prompt box needs besides text: `@file` mentions, `/` commands and attached images.
//!
//! A mention is the text `@path`, which Claude Code itself expands into the file's contents
//! (also in `-p` stream-json mode), so TJ only has to help pick the path. Images go to the
//! agent as image blocks next to the text.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::Context as _;

use crate::agent::{PromptImage, SlashCommand};

/// Stop listing a huge tree here; the picker only ever shows a few.
pub const MAX_FILES: usize = 50_000;
/// How many matches the picker shows.
pub const SHOWN: usize = 8;

/// The project's files, relative to `dir`: what git tracks plus untracked files it doesn't
/// ignore, or (outside git) a walk that skips hidden folders and build output.
pub fn list_files(dir: &Path) -> Vec<String> {
    let git = Command::new("git")
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .current_dir(dir)
        .output();
    if let Ok(out) = git
        && out.status.success()
    {
        let mut files: Vec<String> = out
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .filter_map(|p| std::str::from_utf8(p).ok().map(str::to_owned))
            .take(MAX_FILES)
            .collect();
        files.sort();
        files.dedup();
        return files;
    }
    let mut files = Vec::new();
    walk(dir, dir, 0, &mut files);
    files.sort();
    files
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if depth > 8 || out.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(root, &path, depth + 1, out);
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// The mention being typed: an `@` at the start of the text or after a space, then no
/// spaces up to the cursor. Gives the byte range from the `@` to the cursor and the query.
/// An `@` inside an existing token (a mention already picked) doesn't count.
pub fn query_at(text: &str, cursor: usize, tokens: &[Range<usize>]) -> Option<(Range<usize>, String)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let starts_word = before[..at].chars().next_back().is_none_or(|c| c.is_whitespace() || c == '(');
    let query = &before[at + 1..];
    if !starts_word || query.chars().any(char::is_whitespace) || tokens.iter().any(|t| t.start <= at && at < t.end) {
        return None;
    }
    Some((at..cursor, query.to_string()))
}

/// The best matches for `query`, best first: file names that start with it, then file names
/// that contain it, then paths that contain it, then paths that contain its letters in
/// order. Shorter paths win ties. An empty query lists the shallowest files.
pub fn rank(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|file| {
            let path = file.to_lowercase();
            let name = path.rsplit('/').next().unwrap_or(&path);
            let score = if query.is_empty() {
                path.matches('/').count().min(9) as u8
            } else if name.starts_with(&query) {
                0
            } else if name.contains(&query) {
                1
            } else if path.contains(&query) {
                2
            } else if is_subsequence(&query, &path) {
                3
            } else {
                return None;
            };
            Some((score, file.len(), file))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(limit).map(|(_, _, f)| f.clone()).collect()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle.chars().all(|c| rest.any(|h| h == c))
}

/// The `/command` being typed: the prompt starts with `/` and the cursor is still in that
/// first word. Returns the word's range (to replace) and what's typed after the slash.
/// Claude Code only runs a command at the very start of a message, so nowhere else counts.
pub fn command_query(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let rest = text.strip_prefix('/')?;
    let end = 1 + rest.find(char::is_whitespace).unwrap_or(rest.len());
    if cursor == 0 || cursor > end {
        return None;
    }
    let word = &text[1..end];
    // `/src/main.rs` is a path, not a command.
    if word.contains('/') || word.contains('\\') {
        return None;
    }
    Some((0..end, text[1..cursor].to_string()))
}

/// The commands matching `query`, best first: names that start with it, then names with a
/// part that does (`review` finds `code-review`), then names that contain it. From three
/// letters on, descriptions with a word that starts with it and names that have its letters
/// in order count too; shorter, they'd match nearly everything.
pub fn rank_commands(commands: &[SlashCommand], query: &str, limit: usize) -> Vec<SlashCommand> {
    let query = query.to_lowercase();
    let loose = query.chars().count() >= 3;
    let mut scored: Vec<(u8, usize, usize)> = commands
        .iter()
        .enumerate()
        .filter_map(|(ix, c)| {
            let name = c.name.to_lowercase();
            let score = if name.starts_with(&query) {
                0
            } else if name.split([':', '-', '_']).any(|part| part.starts_with(&query)) {
                1
            } else if name.contains(&query) {
                2
            } else if loose
                && c.description.to_lowercase().split(|ch: char| !ch.is_alphanumeric()).any(|w| w.starts_with(&query))
            {
                3
            } else if loose && is_subsequence(&query, &name) {
                4
            } else {
                return None;
            };
            Some((score, if query.is_empty() { 0 } else { name.len() }, ix))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(limit).map(|(_, _, ix)| commands[ix].clone()).collect()
}

/// The text a mention inserts. Paths with spaces are quoted, as Claude Code expects.
pub fn mention_text(path: &str) -> String {
    if path.chars().any(char::is_whitespace) { format!("@\"{path}\"") } else { format!("@{path}") }
}

/// How a dropped or picked file is mentioned: relative to the project when it's inside it.
pub fn mention_path(file: &Path, project: &Path) -> String {
    file.strip_prefix(project)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| file.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Whether a file is an image TJ attaches (rather than mentions), by its extension.
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"))
}

/// The API's limit is 5 MB per image (base64); stay under it with room to spare.
const MAX_IMAGE_BYTES: usize = 3_500_000;
/// Larger images are scaled down to this on their long side, which is also what the model
/// sees at most without losing detail.
const MAX_IMAGE_SIDE: u32 = 1568;

/// An image ready to send: PNG, JPEG, GIF and WebP pass through when small enough; anything
/// else, or anything too large, is decoded, scaled down and re-encoded (PNG, or JPEG when
/// the PNG is still too big).
pub fn prepare_image(bytes: Vec<u8>) -> anyhow::Result<PromptImage> {
    let format = image::guess_format(&bytes).context("not an image")?;
    let media_type = match format {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::WebP => Some("image/webp"),
        _ => None,
    };
    if let Some(media_type) = media_type
        && bytes.len() <= MAX_IMAGE_BYTES
        && image::load_from_memory(&bytes).map(|i| i.width().max(i.height()) <= MAX_IMAGE_SIDE * 2).unwrap_or(false)
    {
        return Ok(PromptImage { media_type: media_type.into(), data: Arc::new(bytes) });
    }
    let decoded = image::load_from_memory(&bytes).context("could not read the image")?;
    let scaled = if decoded.width().max(decoded.height()) > MAX_IMAGE_SIDE {
        decoded.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let mut png = Vec::new();
    scaled.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    if png.len() <= MAX_IMAGE_BYTES {
        return Ok(PromptImage { media_type: "image/png".into(), data: Arc::new(png) });
    }
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85).encode_image(&scaled.to_rgb8())?;
    Ok(PromptImage { media_type: "image/jpeg".into(), data: Arc::new(jpeg) })
}

/// The file extension for a media type, for saving attached images with the transcript.
pub fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// Where a thread's attached images are kept: next to its transcript (`<id>.files/`).
pub fn files_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("files")
}

#[cfg(test)]
mod tests {
    use super::{command_query, is_image, mention_path, mention_text, prepare_image, query_at, rank, rank_commands};
    use crate::agent::SlashCommand;

    #[test]
    fn a_command_is_the_first_word_of_the_prompt() {
        assert_eq!(command_query("/", 1), Some((0..1, String::new())));
        assert_eq!(command_query("/comp", 5), Some((0..5, "comp".into())));
        assert_eq!(command_query("/compact keep the plan", 3), Some((0..8, "co".into())), "the whole word is replaced");
        assert_eq!(command_query("/compact keep", 12), None, "past the name it's the arguments");
        assert_eq!(command_query("look /comp", 10), None, "only at the start");
        assert_eq!(command_query("/src/main.rs", 4), None, "a path");
        assert_eq!(command_query("/x", 0), None, "the cursor is before the slash");
    }

    #[test]
    fn command_names_that_start_with_the_query_come_first() {
        let commands: Vec<SlashCommand> = [
            ("code-review", "Review the diff"),
            ("compact", "Clear history but keep a summary"),
            ("context", "Show context usage"),
            ("plugin:commit", "Make a commit"),
            ("clear", "Start over"),
        ]
        .map(|(name, description)| SlashCommand {
            name: name.into(),
            description: description.into(),
            argument_hint: String::new(),
        })
        .to_vec();
        let names = |query: &str| rank_commands(&commands, query, 8).into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names("co"), ["compact", "context", "code-review", "plugin:commit"]);
        assert_eq!(names("review"), ["code-review"]);
        assert_eq!(names("commit"), ["plugin:commit"]);
        assert_eq!(names("summary"), ["compact"], "descriptions count too");
        assert_eq!(names("us"), Vec::<String>::new(), "but not for a letter or two (\"usage\" in a description)");
        assert_eq!(names("ont"), ["context"], "a word inside a description has to start with it");
        assert_eq!(names("").len(), 5, "everything, in Claude's order");
        assert_eq!(names("")[0], "code-review");
    }
    use std::path::Path;

    #[test]
    fn a_mention_starts_at_an_at_sign_after_a_space() {
        assert_eq!(query_at("look at @src/ma", 15, &[]), Some((8..15, "src/ma".into())));
        assert_eq!(query_at("@", 1, &[]), Some((0..1, String::new())));
        assert_eq!(query_at("mail me@home", 12, &[]), None, "an @ inside a word is not a mention");
        assert_eq!(query_at("@src/main.rs and", 16, &[]), None, "a space ends it");
        let token = 4..9;
        assert_eq!(query_at("see @a.rs", 9, std::slice::from_ref(&token)), None, "a picked mention is a token");
    }

    #[test]
    fn names_that_start_with_the_query_come_first() {
        let files: Vec<String> =
            ["src/thread.rs", "docs/threading.md", "src/workspace.rs", "tests/fixtures/thread.json", "README.md"]
                .map(String::from)
                .to_vec();
        assert_eq!(rank(&files, "thread", 3), vec!["src/thread.rs", "docs/threading.md", "tests/fixtures/thread.json"]);
        assert_eq!(rank(&files, "swk", 5), vec!["src/workspace.rs"], "letters in order");
        assert_eq!(rank(&files, "", 2), vec!["README.md", "src/thread.rs"], "no query: shallowest first");
        assert!(rank(&files, "zzz", 5).is_empty());
    }

    #[test]
    fn mentions_are_relative_and_quoted_when_needed() {
        assert_eq!(mention_path(Path::new("/p/src/a.rs"), Path::new("/p")), "src/a.rs");
        assert_eq!(mention_path(Path::new("/elsewhere/b.rs"), Path::new("/p")), "/elsewhere/b.rs");
        assert_eq!(mention_text("src/a.rs"), "@src/a.rs");
        assert_eq!(mention_text("My Notes.md"), "@\"My Notes.md\"");
        assert!(is_image(Path::new("shot.PNG")) && !is_image(Path::new("main.rs")));
    }

    #[test]
    fn large_images_are_scaled_down_and_small_ones_pass_through() {
        let encode = |w: u32, h: u32| {
            let mut bytes = Vec::new();
            image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]))
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };
        let small = encode(40, 30);
        let kept = prepare_image(small.clone()).unwrap();
        assert_eq!((kept.media_type.as_str(), kept.data.as_slice()), ("image/png", small.as_slice()));

        // Wider than twice the limit, so it must be scaled (kept thin: debug builds are slow).
        let big = prepare_image(encode(3200, 20)).unwrap();
        let decoded = image::load_from_memory(&big.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1568, 10));

        assert!(prepare_image(b"not an image".to_vec()).is_err());
    }
}
