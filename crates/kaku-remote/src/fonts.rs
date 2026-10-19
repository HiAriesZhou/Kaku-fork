//! The fonts the web page should draw with: the families of Kaku's font
//! config in order, and the files of those the user installed, which a
//! browser on another computer does not have. The page fetches each file
//! once over the session's download path and keeps it in its cache. Only
//! the text weight goes: the page draws bold text at that weight too.

use config::FontAttributes;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use wezterm_font::db::FontDatabase;
use wezterm_font::locator::{new_locator, FontDataSource};
use wezterm_font::parser::ParsedFont;

/// Larger files are left to the page's own fonts: a CJK family can be tens of
/// megabytes per weight, too much to send through the relay.
const MAX_FONT_BYTES: u64 = 32 << 20;
/// The size fonts are matched at; only bitmap fonts care.
const PIXEL_SIZE: u16 = 16;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct FontFile {
    /// The family as Kaku's config names it; the page registers the file
    /// under this name, so its stack reads like Kaku's.
    pub family: String,
    pub path: String,
    pub size: u64,
    /// With the path and size, tells the page whether its cached copy is
    /// still this file.
    pub modified: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub(crate) struct PageFonts {
    pub families: Vec<String>,
    pub files: Vec<FontFile>,
}

/// Families the page has its own copy of, or that must stay out of its
/// stack: named, an emoji font draws Claude Code's status symbols (⏺ ✳) as
/// fixed-color emoji, and the browser's own fallback already finds emoji.
fn page_skips(family: &str) -> bool {
    family.eq_ignore_ascii_case("JetBrains Mono") || family.to_ascii_lowercase().contains("emoji")
}

/// Whether a resolved file is worth sending: one font per file, not part of
/// macOS (a Mac browser has those, and they are large collections), and
/// small enough.
fn sendable(path: &Path, index: u32, size: u64) -> bool {
    let collection = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["ttc", "otc", "dfont"].contains(&e.to_ascii_lowercase().as_str()));
    index == 0 && !collection && !path.starts_with("/System/") && size > 0 && size <= MAX_FONT_BYTES
}

fn file_for(family: &str, path: &Path, index: u32) -> Option<FontFile> {
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    if !sendable(path, index, meta.len()) {
        return None;
    }
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Some(FontFile {
        family: family.to_string(),
        path: path.display().to_string(),
        size: meta.len(),
        modified,
    })
}

fn resolve(cfg: &config::ConfigHandle) -> PageFonts {
    let mut fonts = PageFonts::default();
    let mut seen = HashSet::new();
    for attr in &cfg.font.font {
        if seen.insert(attr.family.to_ascii_lowercase()) {
            fonts.families.push(attr.family.clone());
        }
    }
    let wanted: Vec<&FontAttributes> = cfg
        .font
        .font
        .iter()
        .filter(|a| !page_skips(&a.family))
        .collect();
    if wanted.is_empty() {
        return fonts;
    }
    let locator = new_locator(cfg.font_locator);
    let dirs = FontDatabase::with_font_dirs(cfg).ok();
    let locate = |attr: &FontAttributes| -> Option<(std::path::PathBuf, u32)> {
        let mut found: Vec<ParsedFont> = dirs
            .as_ref()
            .map(|d| d.candidates(attr).into_iter().cloned().collect())
            .unwrap_or_default();
        found.extend(
            locator
                .load_fonts(std::slice::from_ref(attr), &mut HashSet::new(), PIXEL_SIZE)
                .unwrap_or_default(),
        );
        let named: Vec<&ParsedFont> = found.iter().filter(|p| p.matches_name(attr)).collect();
        let best = named[ParsedFont::best_matching_index(attr, &named, PIXEL_SIZE)?];
        match &best.handle.source {
            FontDataSource::OnDisk(path) => Some((path.clone(), best.handle.index)),
            _ => None,
        }
    };
    let mut paths = HashSet::new();
    for attr in wanted {
        let Some((path, index)) = locate(attr) else {
            continue;
        };
        if !paths.insert((path.clone(), index)) {
            continue;
        }
        if let Some(file) = file_for(&attr.family, &path, index) {
            fonts.files.push(file);
        }
    }
    fonts
}

/// Resolved once per config: matching runs CoreText and reads font files.
static CACHE: Mutex<Option<(usize, PageFonts)>> = Mutex::new(None);

pub(crate) fn page_fonts(cfg: &config::ConfigHandle) -> PageFonts {
    let generation = cfg.generation();
    if let Some((gen, fonts)) = &*CACHE.lock() {
        if *gen == generation {
            return fonts.clone();
        }
    }
    let fonts = resolve(cfg);
    log::debug!(
        "kaku-remote web: page fonts {:?}, {} file(s) to send",
        fonts.families,
        fonts.files.len()
    );
    *CACHE.lock() = Some((generation, fonts.clone()));
    fonts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_keeps_its_own_copies_and_no_emoji_font() {
        assert!(page_skips("JetBrains Mono"));
        assert!(page_skips("Apple Color Emoji"));
        assert!(page_skips("Noto Color Emoji"));
        assert!(!page_skips("Maple Mono NF CN"));
        assert!(!page_skips("PingFang SC"));
    }

    #[test]
    fn only_single_user_fonts_of_a_sendable_size_go_to_the_page() {
        let user = Path::new("/Users/me/Library/Fonts/MapleMono-NF-Regular.ttf");
        assert!(sendable(user, 0, 1 << 20));
        assert!(!sendable(user, 0, 0));
        assert!(!sendable(user, 0, MAX_FONT_BYTES + 1));
        assert!(!sendable(user, 1, 1 << 20), "a face inside a collection");
        assert!(!sendable(
            Path::new("/Users/me/Library/Fonts/Iosevka.TTC"),
            0,
            1 << 20
        ));
        assert!(!sendable(
            Path::new("/System/Library/Fonts/Menlo.ttf"),
            0,
            1 << 20
        ));
    }

    #[test]
    fn a_font_file_carries_what_the_page_caches_it_by() {
        let dir = std::env::temp_dir().join(format!("kaku-font-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Mine-Regular.otf");
        std::fs::write(&path, b"OTTO....").unwrap();
        let file = file_for("Mine", &path, 0).unwrap();
        assert_eq!(file.family, "Mine");
        assert_eq!(file.size, 8);
        assert!(file.modified > 0);
        assert_eq!(file.path, path.display().to_string());
        assert!(file_for("Mine", &dir, 0).is_none(), "a folder");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
