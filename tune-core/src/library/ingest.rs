//! Ingest — bring a folder of freshly-acquired files into the library.
//!
//! The scanner ([`crate::scanner::walker`]) assumes files already sit inside a
//! configured music directory. This module covers the step *before* that: take
//! an arbitrary folder (a download, a rip, a USB key), work out where each file
//! should live under the library naming scheme, and hand back a plan the caller
//! can show the user before a single byte is moved.
//!
//! Planning is deliberately filesystem-free — existence checks come in through
//! a closure — so the whole placement logic is unit-testable without touching
//! a disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default destination layout.
///
/// Placeholders: `{albumartist}` `{artist}` `{album}` `{title}` `{year}`
/// `{track}` `{disc}` `{genre}`. A `[...]` group is dropped in full when any
/// placeholder inside it resolves empty, which is how a yearless album avoids
/// a `" - Absolution"` folder and a single-disc album avoids a `Disc /` level.
pub const DEFAULT_TEMPLATE: &str =
    "{albumartist}/[{year} - ]{album}/[Disc {disc}/]{track} - {title}";

/// Longest a single path component may get. Chosen well under the 255-byte
/// limit of ext4/APFS/NTFS so that a long title plus an extension still fits.
const MAX_COMPONENT_BYTES: usize = 120;

/// Non-audio files worth carrying over with the album.
const EXTRA_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "bmp", "gif", "cue", "log", "txt", "nfo", "pdf", "m3u", "m3u8",
];

/// What to do with the source files once they are in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileMode {
    /// Move the files (leaves nothing behind, undoable via the job manifest).
    #[default]
    Move,
    /// Copy the files, leaving the source folder untouched.
    Copy,
}

impl FileMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FileMode::Move => "move",
            FileMode::Copy => "copy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "move" => Some(FileMode::Move),
            "copy" => Some(FileMode::Copy),
            _ => None,
        }
    }
}

/// One audio file found in the source folder, reduced to the fields needed to
/// place and describe it. Deliberately not [`crate::metadata::TrackMetadata`]:
/// that carries embedded cover bytes we do not want to hold for a whole album.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceTrack {
    pub source_path: String,
    pub ext: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub duration_ms: Option<u64>,
    pub format: Option<String>,
    pub file_size: u64,
    pub has_cover: bool,
}

impl SourceTrack {
    /// Artist to file the album under: explicit album artist, else the track
    /// artist. A per-track artist is the wrong answer for a compilation, which
    /// is why [`AlbumSummary`] resolves this across the whole folder too.
    pub fn filing_artist(&self) -> Option<&str> {
        self.album_artist
            .as_deref()
            .or(self.artist.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// Album-level view of the source folder, as guessed from the files' tags.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlbumSummary {
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub track_count: usize,
    pub disc_count: u32,
    /// Track artists differ across the folder — likely a compilation, so
    /// filing under a single artist would be wrong.
    pub is_compilation: bool,
    pub formats: Vec<String>,
    pub total_bytes: u64,
    /// At least one file already carries embedded artwork.
    pub has_cover: bool,
    /// Machine-readable codes (`missing_year`, `mixed_albums`, …) for the UI to
    /// translate. Never user-facing prose: this crate has no locale.
    pub warnings: Vec<String>,
}

/// User corrections applied on top of the guessed summary before planning.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlbumOverrides {
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub genre: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Audio,
    /// Cover, cue sheet, rip log — carried into the album folder verbatim.
    Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conflict {
    /// A file already exists at the destination.
    DestinationExists,
    /// Two source files render to the same destination — usually missing or
    /// duplicate track numbers.
    DuplicateTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    pub source_path: String,
    /// Absolute destination.
    pub dest_path: String,
    /// Destination relative to `dest_root`, for a compact UI preview.
    pub relative_path: String,
    pub kind: EntryKind,
    pub conflict: Option<Conflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFile {
    pub source_path: String,
    /// Machine-readable code: `unsupported_extension`, `unreadable`.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestPlan {
    pub source_path: String,
    pub dest_root: String,
    /// Common destination folder of all audio entries, when there is one —
    /// what to hand to the targeted scan once the files are in place.
    pub album_dir: Option<String>,
    pub template: String,
    pub mode: FileMode,
    pub entries: Vec<PlanEntry>,
    pub skipped: Vec<SkippedFile>,
    pub warnings: Vec<String>,
}

impl IngestPlan {
    pub fn audio_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::Audio)
            .count()
    }

    pub fn conflicts(&self) -> usize {
        self.entries.iter().filter(|e| e.conflict.is_some()).count()
    }

    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(|e| e.conflict.is_some())
    }
}

// -- Summarising a source folder --

/// Most frequent non-empty value, ties broken by first appearance.
fn majority<T, F>(tracks: &[SourceTrack], get: F) -> Option<T>
where
    T: Clone + std::hash::Hash + Eq,
    F: Fn(&SourceTrack) -> Option<T>,
{
    let mut counts: HashMap<T, usize> = HashMap::new();
    let mut order: Vec<T> = Vec::new();
    for t in tracks {
        if let Some(v) = get(t) {
            if !counts.contains_key(&v) {
                order.push(v.clone());
            }
            *counts.entry(v).or_insert(0) += 1;
        }
    }
    order
        .into_iter()
        .max_by_key(|v| counts.get(v).copied().unwrap_or(0))
}

fn clean(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Guess the album-level fields from a folder's worth of tracks, and flag what
/// is missing or inconsistent so the caller can ask the user before moving
/// anything.
pub fn summarize(tracks: &[SourceTrack]) -> AlbumSummary {
    let mut s = AlbumSummary {
        track_count: tracks.len(),
        ..Default::default()
    };
    if tracks.is_empty() {
        s.warnings.push("no_audio_files".into());
        return s;
    }

    s.album = majority(tracks, |t| clean(t.album.as_deref()));
    s.year = majority(tracks, |t| t.year);
    s.genre = majority(tracks, |t| clean(t.genre.as_deref()));

    let distinct_artists: Vec<String> = {
        let mut v: Vec<String> = tracks
            .iter()
            .filter_map(|t| clean(t.artist.as_deref()))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let explicit_album_artist = majority(tracks, |t| clean(t.album_artist.as_deref()));
    s.is_compilation = explicit_album_artist.is_none() && distinct_artists.len() > 1;
    s.album_artist = explicit_album_artist.or_else(|| {
        if s.is_compilation {
            None
        } else {
            majority(tracks, |t| clean(t.artist.as_deref()))
        }
    });

    s.disc_count = tracks
        .iter()
        .filter_map(|t| t.disc_number)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut formats: Vec<String> = tracks
        .iter()
        .filter_map(|t| clean(t.format.as_deref()).or_else(|| clean(Some(&t.ext))))
        .collect();
    formats.sort();
    formats.dedup();
    s.formats = formats;

    s.total_bytes = tracks.iter().map(|t| t.file_size).sum();
    s.has_cover = tracks.iter().any(|t| t.has_cover);

    if s.album.is_none() {
        s.warnings.push("missing_album".into());
    }
    if s.album_artist.is_none() {
        s.warnings.push("missing_album_artist".into());
    }
    if s.year.is_none() {
        s.warnings.push("missing_year".into());
    }
    if s.is_compilation {
        s.warnings.push("mixed_artists".into());
    }
    if tracks.iter().any(|t| clean(t.title.as_deref()).is_none()) {
        s.warnings.push("missing_titles".into());
    }
    if tracks.iter().any(|t| t.track_number.is_none()) {
        s.warnings.push("missing_track_numbers".into());
    }
    if !s.has_cover {
        s.warnings.push("no_cover".into());
    }

    // More than one album in the folder means the plan will scatter files
    // across several destinations — worth saying out loud.
    let distinct_albums: Vec<String> = {
        let mut v: Vec<String> = tracks
            .iter()
            .filter_map(|t| clean(t.album.as_deref()))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    if distinct_albums.len() > 1 {
        s.warnings.push("mixed_albums".into());
    }

    s
}

/// Apply user corrections over a guessed summary.
pub fn apply_overrides(summary: &AlbumSummary, overrides: &AlbumOverrides) -> AlbumSummary {
    let mut s = summary.clone();
    if let Some(v) = clean(overrides.album_artist.as_deref()) {
        s.album_artist = Some(v);
        s.warnings.retain(|w| w != "missing_album_artist");
        s.is_compilation = false;
        s.warnings.retain(|w| w != "mixed_artists");
    }
    if let Some(v) = clean(overrides.album.as_deref()) {
        s.album = Some(v);
        s.warnings.retain(|w| w != "missing_album");
    }
    if let Some(y) = overrides.year {
        s.year = Some(y);
        s.warnings.retain(|w| w != "missing_year");
    }
    if let Some(v) = clean(overrides.genre.as_deref()) {
        s.genre = Some(v);
    }
    s
}

// -- Pairing source files with a chosen release --

/// A track of the release the user picked, reduced to what pairing needs.
///
/// Deliberately not the MusicBrainz type: the placement logic has no business
/// knowing where the listing came from, and a plain struct keeps the matcher
/// testable with three lines of setup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReleaseTrack {
    pub disc: u32,
    pub position: u32,
    pub title: String,
}

/// Per-file correction chosen by the user, applied before planning.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackOverride {
    pub source_path: String,
    pub title: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
}

/// What a chosen release would change for one source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackProposal {
    pub source_path: String,
    pub current_title: Option<String>,
    pub current_track_number: Option<u32>,
    pub current_disc_number: Option<u32>,
    pub proposed_title: Option<String>,
    pub proposed_track_number: Option<u32>,
    pub proposed_disc_number: Option<u32>,
    pub matched: bool,
    /// How the pairing was made — `disc_and_number`, `title`, or `order`.
    /// Shown to the user, because a positional guess deserves more suspicion
    /// than a track-number hit.
    pub method: Option<String>,
}

impl TrackProposal {
    /// Would accepting this proposal actually change anything?
    pub fn changes_anything(&self) -> bool {
        let title_differs = match (&self.current_title, &self.proposed_title) {
            (Some(cur), Some(new)) => cur != new,
            (None, Some(_)) => true,
            _ => false,
        };
        let track_differs = self.proposed_track_number.is_some()
            && self.proposed_track_number != self.current_track_number;
        let disc_differs = self.proposed_disc_number.is_some()
            && self.proposed_disc_number != self.current_disc_number;
        title_differs || track_differs || disc_differs
    }
}

/// Loose title comparison: case, punctuation and spacing all differ freely
/// between a rip and MusicBrainz ("Dont Stop Me Now" vs "Don't Stop Me Now").
fn norm_title(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pair each source file with a track of the chosen release.
///
/// Three passes, each release track consumed at most once:
///
/// 1. disc + track number — the reliable signal when the rip is numbered
/// 2. title — catches a rip whose numbering is wrong but whose titles are right
/// 3. order — only when exactly as many files as tracks are left over, so an
///    unnumbered, untitled rip still lines up; reported as `order` so the user
///    knows it is a guess
pub fn match_release_tracks(
    sources: &[SourceTrack],
    release: &[ReleaseTrack],
) -> Vec<TrackProposal> {
    let mut pairing: Vec<Option<usize>> = vec![None; sources.len()];
    let mut used: Vec<bool> = vec![false; release.len()];
    let mut method: Vec<Option<&'static str>> = vec![None; sources.len()];

    // Pass 1 — disc + number.
    for (si, source) in sources.iter().enumerate() {
        let Some(number) = source.track_number else {
            continue;
        };
        let disc = source.disc_number.unwrap_or(1);
        let hit = release
            .iter()
            .enumerate()
            .find(|(ri, r)| !used[*ri] && r.disc == disc && r.position == number)
            .map(|(ri, _)| ri);
        if let Some(ri) = hit {
            pairing[si] = Some(ri);
            method[si] = Some("disc_and_number");
            used[ri] = true;
        }
    }

    // Pass 2 — title.
    for (si, source) in sources.iter().enumerate() {
        if pairing[si].is_some() {
            continue;
        }
        let Some(title) = source
            .title
            .as_deref()
            .map(norm_title)
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        let hit = release
            .iter()
            .enumerate()
            .find(|(ri, r)| !used[*ri] && norm_title(&r.title) == title)
            .map(|(ri, _)| ri);
        if let Some(ri) = hit {
            pairing[si] = Some(ri);
            method[si] = Some("title");
            used[ri] = true;
        }
    }

    // Pass 3 — order, but only if what is left lines up exactly.
    let leftover_sources: Vec<usize> = (0..sources.len())
        .filter(|i| pairing[*i].is_none())
        .collect();
    let leftover_tracks: Vec<usize> = (0..release.len()).filter(|i| !used[*i]).collect();
    if !leftover_sources.is_empty() && leftover_sources.len() == leftover_tracks.len() {
        for (si, ri) in leftover_sources.iter().zip(leftover_tracks.iter()) {
            pairing[*si] = Some(*ri);
            method[*si] = Some("order");
        }
    }

    sources
        .iter()
        .enumerate()
        .map(|(si, source)| {
            let hit = pairing[si].map(|ri| &release[ri]);
            TrackProposal {
                source_path: source.source_path.clone(),
                current_title: source.title.clone(),
                current_track_number: source.track_number,
                current_disc_number: source.disc_number,
                proposed_title: hit.map(|r| r.title.clone()),
                proposed_track_number: hit.map(|r| r.position),
                proposed_disc_number: hit.map(|r| r.disc),
                matched: hit.is_some(),
                method: method[si].map(String::from),
            }
        })
        .collect()
}

/// Apply the corrections the user accepted, matched on source path.
///
/// Only non-empty fields are written, so an override can fix a title without
/// clearing a track number the tags already had right.
pub fn apply_track_overrides(tracks: &mut [SourceTrack], overrides: &[TrackOverride]) {
    for over in overrides {
        let Some(track) = tracks
            .iter_mut()
            .find(|t| t.source_path == over.source_path)
        else {
            continue;
        };
        if let Some(title) = clean(over.title.as_deref()) {
            track.title = Some(title);
        }
        if let Some(n) = over.track_number {
            track.track_number = Some(n);
        }
        if let Some(d) = over.disc_number {
            track.disc_number = Some(d);
        }
    }
}

// -- Filename safety --

/// Make one path component safe on every platform we ship on.
///
/// Windows rejects `\ / : * ? " < > |` and trailing dots/spaces; `.` and `..`
/// would be path traversal; an empty result would collapse the hierarchy. The
/// byte cap keeps us clear of filesystem name limits on long classical titles.
pub fn sanitize_component(raw: &str) -> String {
    let (safe_text, corrections) =
        crate::metadata::sanitize_untrusted_single_line_text(raw, "path_component");
    if !corrections.is_empty() {
        tracing::warn!(
            corrections = ?corrections,
            "ingest_path_component_unsafe_text_sanitized"
        );
    }
    let mut out = String::with_capacity(raw.len());
    for c in safe_text.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }

    let mut s = out.trim().trim_end_matches('.').trim().to_string();

    if s.is_empty() || s == "." || s == ".." {
        s = "Unknown".to_string();
    }

    if s.len() > MAX_COMPONENT_BYTES {
        // Truncate on a char boundary, never mid-codepoint.
        let mut end = MAX_COMPONENT_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s = s.trim().trim_end_matches('.').trim().to_string();
        if s.is_empty() {
            s = "Unknown".to_string();
        }
    }

    s
}

// -- Template rendering --

/// Values a template can reference. Missing or empty entries make any
/// enclosing `[...]` group vanish.
#[derive(Debug, Clone, Default)]
pub struct TemplateFields(HashMap<String, String>);

impl TemplateFields {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) -> &mut Self {
        let v: String = value.into();
        let v = v.trim().to_string();
        if !v.is_empty() {
            self.0.insert(key.to_string(), v);
        }
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

/// Render a destination path template into a `/`-separated relative path, with
/// every component sanitized. The extension is *not* appended — the caller
/// knows the real one and must not let a template rewrite it.
///
/// Unknown placeholders resolve empty rather than erroring: a typo in a
/// user-editable template should degrade, not fail the import.
pub fn render_template(template: &str, fields: &TemplateFields) -> String {
    let rendered = render_groups(template, fields);

    let parts: Vec<String> = rendered
        .split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(sanitize_component)
        .collect();

    parts.join("/")
}

/// Expand placeholders and drop `[...]` groups whose placeholders are empty.
fn render_groups(template: &str, fields: &TemplateFields) -> String {
    let mut out = String::with_capacity(template.len());
    // Stack of open groups: (buffer written so far, whether a placeholder in
    // this group resolved empty). Nesting is supported, so a group only
    // survives if it and all its ancestors are satisfied.
    let mut stack: Vec<(String, bool)> = Vec::new();

    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '[' => {
                stack.push((String::new(), true));
                i += 1;
            }
            ']' => {
                if let Some((buf, ok)) = stack.pop() {
                    let target = match stack.last_mut() {
                        Some((parent, _)) => parent,
                        None => &mut out,
                    };
                    if ok {
                        target.push_str(&buf);
                    }
                } else {
                    // Unbalanced ']' — treat as a literal.
                    let target = match stack.last_mut() {
                        Some((buf, _)) => buf,
                        None => &mut out,
                    };
                    target.push(']');
                }
                i += 1;
            }
            '{' => {
                let close = chars[i..].iter().position(|&c| c == '}').map(|p| i + p);
                match close {
                    Some(end) => {
                        let key: String = chars[i + 1..end].iter().collect();
                        let value = fields.get(key.trim()).unwrap_or("");
                        if value.is_empty()
                            && let Some((_, ok)) = stack.last_mut()
                        {
                            *ok = false;
                        }
                        let target = match stack.last_mut() {
                            Some((buf, _)) => buf,
                            None => &mut out,
                        };
                        target.push_str(value);
                        i = end + 1;
                    }
                    None => {
                        // Unterminated '{' — literal.
                        let target = match stack.last_mut() {
                            Some((buf, _)) => buf,
                            None => &mut out,
                        };
                        target.push('{');
                        i += 1;
                    }
                }
            }
            c => {
                let target = match stack.last_mut() {
                    Some((buf, _)) => buf,
                    None => &mut out,
                };
                target.push(c);
                i += 1;
            }
        }
    }

    // Unclosed groups: flush satisfied ones so a truncated template still
    // yields something usable.
    while let Some((buf, ok)) = stack.pop() {
        let target = match stack.last_mut() {
            Some((parent, _)) => parent,
            None => &mut out,
        };
        if ok {
            target.push_str(&buf);
        }
    }

    out
}

/// Build the template fields for one track under a resolved album summary.
pub fn fields_for(track: &SourceTrack, album: &AlbumSummary) -> TemplateFields {
    let mut f = TemplateFields::new();

    // A compilation must not fall back to the per-track artist: that would
    // scatter one album across a folder per guest artist.
    let album_artist = album.album_artist.clone().unwrap_or_else(|| {
        if album.is_compilation {
            "Various Artists".to_string()
        } else {
            track
                .filing_artist()
                .map(String::from)
                .unwrap_or_else(|| "Unknown Artist".to_string())
        }
    });
    f.set("albumartist", album_artist);

    if let Some(a) = track.artist.as_deref().or(album.album_artist.as_deref()) {
        f.set("artist", a);
    }
    f.set(
        "album",
        album
            .album
            .clone()
            .or_else(|| clean(track.album.as_deref()))
            .unwrap_or_else(|| "Unknown Album".to_string()),
    );
    f.set(
        "title",
        clean(track.title.as_deref()).unwrap_or_else(|| {
            // No title tag: fall back to the source filename so the file stays
            // identifiable instead of becoming "01 - Unknown".
            Path::new(&track.source_path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Title".into())
        }),
    );
    if let Some(y) = album.year.or(track.year) {
        f.set("year", y.to_string());
    }
    if let Some(g) = album.genre.as_deref().or(track.genre.as_deref()) {
        f.set("genre", g);
    }
    if let Some(n) = track.track_number {
        f.set("track", format!("{n:02}"));
    }
    // Only expose {disc} on genuinely multi-disc sets, so the default template
    // does not create a "Disc 1" level for every single-disc album.
    if album.disc_count > 1
        && let Some(d) = track.disc_number
    {
        f.set("disc", d.to_string());
    }
    f.set("ext", track.ext.clone());

    f
}

// -- Planning --

/// Longest common directory prefix of a set of absolute paths.
fn common_dir(paths: &[PathBuf]) -> Option<String> {
    let mut iter = paths.iter().map(|p| p.parent().unwrap_or(Path::new("")));
    let first = iter.next()?;
    let mut common: Vec<&std::ffi::OsStr> = first.iter().collect();
    for p in iter {
        let parts: Vec<&std::ffi::OsStr> = p.iter().collect();
        let keep = common
            .iter()
            .zip(parts.iter())
            .take_while(|(a, b)| a == b)
            .count();
        common.truncate(keep);
        if common.is_empty() {
            return None;
        }
    }
    let mut out = PathBuf::new();
    for c in common {
        out.push(c);
    }
    let s = out.to_string_lossy().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// True when a non-audio file is worth carrying along with the album.
pub fn is_extra_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTRA_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Where an import should land and how.
///
/// Grouped rather than passed positionally on purpose: `dest_root` and
/// `template` are both `&str`, so as arguments they can be swapped without the
/// compiler noticing — and the result would be an album filed under a folder
/// named after the template.
#[derive(Debug, Clone)]
pub struct PlanOptions<'a> {
    pub dest_root: &'a str,
    /// Blank falls back to [`DEFAULT_TEMPLATE`].
    pub template: &'a str,
    pub mode: FileMode,
}

impl<'a> PlanOptions<'a> {
    pub fn new(dest_root: &'a str, template: &'a str, mode: FileMode) -> Self {
        Self {
            dest_root,
            template,
            mode,
        }
    }
}

/// Work out where every file goes, without touching the filesystem.
///
/// `dest_exists` decides whether a destination is already taken — injected so
/// the planner stays pure in tests. Extra files (cover, cue, log) land in the
/// album folder derived from the audio entries; if no audio file could be
/// placed, they are skipped rather than dumped at the library root.
pub fn build_plan(
    source_path: &str,
    tracks: &[SourceTrack],
    extras: &[String],
    album: &AlbumSummary,
    options: &PlanOptions<'_>,
    dest_exists: &dyn Fn(&Path) -> bool,
) -> IngestPlan {
    let template = if options.template.trim().is_empty() {
        DEFAULT_TEMPLATE
    } else {
        options.template.trim()
    };
    let mode = options.mode;
    let dest_root = options.dest_root;
    let root = Path::new(dest_root);

    let mut entries: Vec<PlanEntry> = Vec::new();
    let mut skipped: Vec<SkippedFile> = Vec::new();
    let mut taken: HashMap<String, usize> = HashMap::new();

    for track in tracks {
        let fields = fields_for(track, album);
        let rel_no_ext = render_template(template, &fields);
        if rel_no_ext.is_empty() {
            skipped.push(SkippedFile {
                source_path: track.source_path.clone(),
                reason: "template_rendered_empty".into(),
            });
            continue;
        }

        let ext = track.ext.trim().trim_start_matches('.').to_lowercase();
        let relative_path = if ext.is_empty() {
            rel_no_ext
        } else {
            format!("{rel_no_ext}.{ext}")
        };
        let dest = root.join(&relative_path);
        let dest_str = dest.to_string_lossy().to_string();

        // Two source files rendering to the same name is a data problem the
        // user must see — silently overwriting one with the other would lose
        // a track.
        let dup = taken.contains_key(&dest_str);
        *taken.entry(dest_str.clone()).or_insert(0) += 1;

        let conflict = if dup {
            Some(Conflict::DuplicateTarget)
        } else if dest_exists(&dest) {
            Some(Conflict::DestinationExists)
        } else {
            None
        };

        entries.push(PlanEntry {
            source_path: track.source_path.clone(),
            dest_path: dest_str,
            relative_path,
            kind: EntryKind::Audio,
            conflict,
        });
    }

    let audio_dests: Vec<PathBuf> = entries
        .iter()
        .filter(|e| e.kind == EntryKind::Audio)
        .map(|e| PathBuf::from(&e.dest_path))
        .collect();
    let album_dir = common_dir(&audio_dests);

    if let Some(ref dir) = album_dir {
        for extra in extras {
            let name = match Path::new(extra).file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };
            let dest = Path::new(dir).join(sanitize_component(&name));
            let dest_str = dest.to_string_lossy().to_string();
            let relative_path = dest
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| dest_str.clone());
            // Extras share the `taken` map with the audio entries: a multi-disc
            // set with a cover.jpg in each disc folder collapses both onto one
            // destination, and the user should see that at preview time rather
            // than discover it as a silent skip during the copy.
            let dup = taken.contains_key(&dest_str);
            *taken.entry(dest_str.clone()).or_insert(0) += 1;

            let conflict = if dup {
                Some(Conflict::DuplicateTarget)
            } else if dest_exists(&dest) {
                Some(Conflict::DestinationExists)
            } else {
                None
            };
            entries.push(PlanEntry {
                source_path: extra.clone(),
                dest_path: dest_str,
                relative_path,
                kind: EntryKind::Extra,
                conflict,
            });
        }
    } else {
        for extra in extras {
            skipped.push(SkippedFile {
                source_path: extra.clone(),
                reason: "no_album_folder".into(),
            });
        }
    }

    IngestPlan {
        source_path: source_path.to_string(),
        dest_root: dest_root.to_string(),
        album_dir,
        template: template.to_string(),
        mode,
        entries,
        skipped,
        warnings: album.warnings.clone(),
    }
}

// -- Execution --

/// What to do when a destination is already occupied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Leave the existing file alone and report the source as skipped.
    #[default]
    Skip,
    /// Replace the existing file.
    Overwrite,
    /// Keep both, adding a ` (2)` suffix to the incoming file.
    Rename,
}

/// A file that was actually put in place, kept so the job can be undone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovedFile {
    pub source_path: String,
    pub dest_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestError {
    pub source_path: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestReport {
    pub mode: FileMode,
    pub album_dir: Option<String>,
    pub moved: Vec<MovedFile>,
    pub skipped: Vec<SkippedFile>,
    pub errors: Vec<IngestError>,
    pub bytes: u64,
}

impl IngestReport {
    pub fn files_placed(&self) -> usize {
        self.moved.len()
    }
}

/// First free path of the form `name.ext`, `name (2).ext`, `name (3).ext`, …
pub fn unique_path(dest: &Path, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    if !exists(dest) {
        return dest.to_path_buf();
    }
    let parent = dest.parent().unwrap_or(Path::new(""));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let ext = dest.extension().map(|e| e.to_string_lossy().to_string());

    for n in 2..1000 {
        let name = match ext {
            Some(ref e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !exists(&candidate) {
            return candidate;
        }
    }
    dest.to_path_buf()
}

/// Move a file, falling back to copy+delete across filesystem boundaries.
///
/// `fs::rename` fails with `EXDEV` when source and destination live on
/// different volumes — the normal case here, since downloads sit on the system
/// disk and the library often lives on an external drive or a NAS mount. The
/// copy is only unlinked once it has fully succeeded, so an interrupted move
/// leaves the source intact rather than losing the file.
fn move_file(source: &Path, dest: &Path) -> Result<u64, String> {
    match std::fs::rename(source, dest) {
        Ok(()) => std::fs::metadata(dest).map(|m| m.len()).or(Ok(0)),
        Err(_) => {
            let bytes = std::fs::copy(source, dest).map_err(|e| format!("copy: {e}"))?;
            std::fs::remove_file(source).map_err(|e| format!("remove source: {e}"))?;
            Ok(bytes)
        }
    }
}

/// Carry out a plan against the real filesystem.
///
/// Entries are processed independently: one unreadable file does not abort the
/// rest of the album, it lands in `errors`. Destination directories are created
/// as needed. The returned report is the undo manifest — every path in `moved`
/// was created by this call.
pub fn execute(plan: &IngestPlan, policy: ConflictPolicy) -> IngestReport {
    let exists = |p: &Path| p.exists();
    let mut report = IngestReport {
        mode: plan.mode,
        album_dir: plan.album_dir.clone(),
        skipped: plan.skipped.clone(),
        ..Default::default()
    };

    for entry in &plan.entries {
        let source = Path::new(&entry.source_path);
        if !source.exists() {
            report.errors.push(IngestError {
                source_path: entry.source_path.clone(),
                message: "source file no longer exists".into(),
            });
            continue;
        }

        let mut dest = PathBuf::from(&entry.dest_path);

        // Re-probe rather than trusting the plan: it may have been previewed
        // minutes ago, and a duplicate target only shows up now that the
        // earlier entry of the pair has been written.
        if dest.exists() {
            match policy {
                ConflictPolicy::Skip => {
                    report.skipped.push(SkippedFile {
                        source_path: entry.source_path.clone(),
                        reason: "destination_exists".into(),
                    });
                    continue;
                }
                ConflictPolicy::Rename => dest = unique_path(&dest, &exists),
                ConflictPolicy::Overwrite => {}
            }
        }

        if let Some(parent) = dest.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            report.errors.push(IngestError {
                source_path: entry.source_path.clone(),
                message: format!("create dir: {e}"),
            });
            continue;
        }

        let result = match plan.mode {
            FileMode::Move => move_file(source, &dest),
            FileMode::Copy => std::fs::copy(source, &dest).map_err(|e| format!("copy: {e}")),
        };

        match result {
            Ok(bytes) => {
                report.bytes += bytes;
                report.moved.push(MovedFile {
                    source_path: entry.source_path.clone(),
                    dest_path: dest.to_string_lossy().to_string(),
                });
            }
            Err(e) => report.errors.push(IngestError {
                source_path: entry.source_path.clone(),
                message: e,
            }),
        }
    }

    report
}

/// Reverse a finished job.
///
/// A move is undone by moving the file back; a copy by deleting the copy we
/// made. Only paths recorded in the manifest are touched, and a destination
/// that no longer matches the manifest (edited or replaced since) is reported
/// instead of being clobbered. Emptied album folders are removed.
pub fn undo(report: &IngestReport) -> IngestReport {
    let mut result = IngestReport {
        mode: report.mode,
        album_dir: report.album_dir.clone(),
        ..Default::default()
    };

    for file in &report.moved {
        let dest = Path::new(&file.dest_path);
        if !dest.exists() {
            result.errors.push(IngestError {
                source_path: file.dest_path.clone(),
                message: "already gone".into(),
            });
            continue;
        }

        match report.mode {
            FileMode::Move => {
                let source = Path::new(&file.source_path);
                if source.exists() {
                    result.errors.push(IngestError {
                        source_path: file.dest_path.clone(),
                        message: "source path is occupied again".into(),
                    });
                    continue;
                }
                if let Some(parent) = source.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match move_file(dest, source) {
                    Ok(bytes) => {
                        result.bytes += bytes;
                        result.moved.push(file.clone());
                    }
                    Err(e) => result.errors.push(IngestError {
                        source_path: file.dest_path.clone(),
                        message: e,
                    }),
                }
            }
            FileMode::Copy => match std::fs::remove_file(dest) {
                Ok(()) => result.moved.push(file.clone()),
                Err(e) => result.errors.push(IngestError {
                    source_path: file.dest_path.clone(),
                    message: format!("remove: {e}"),
                }),
            },
        }
    }

    // Clean up the folders the job created: the per-disc folders first, then
    // the album folder. Deliberately no upward walk beyond the album folder —
    // an emptied artist folder is harmless, whereas a walk that keeps going
    // could delete the music directory root itself. `remove_dir` refuses
    // non-empty directories, which is the guard against taking anything that
    // was not ours.
    let mut dirs: Vec<PathBuf> = report
        .moved
        .iter()
        .filter_map(|f| Path::new(&f.dest_path).parent().map(PathBuf::from))
        .collect();
    if let Some(ref album_dir) = report.album_dir {
        dirs.push(PathBuf::from(album_dir));
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for dir in dirs {
        let _ = std::fs::remove_dir(&dir);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, n: u32) -> SourceTrack {
        SourceTrack {
            source_path: format!("/dl/{n:02} {title}.flac"),
            ext: "flac".into(),
            title: Some(title.into()),
            artist: Some("Muse".into()),
            album_artist: Some("Muse".into()),
            album: Some("Absolution".into()),
            year: Some(2003),
            genre: Some("Rock".into()),
            track_number: Some(n),
            disc_number: Some(1),
            file_size: 1000,
            format: Some("FLAC".into()),
            ..Default::default()
        }
    }

    fn never_exists(_: &Path) -> bool {
        false
    }

    #[test]
    fn sanitize_strips_forbidden_and_traversal() {
        assert_eq!(sanitize_component("AC/DC"), "AC_DC");
        assert_eq!(sanitize_component("What?"), "What_");
        assert_eq!(sanitize_component("  spaced  "), "spaced");
        assert_eq!(sanitize_component("trailing."), "trailing");
        assert_eq!(sanitize_component(".."), "Unknown");
        assert_eq!(sanitize_component(""), "Unknown");
        assert_eq!(sanitize_component("a:b*c|d"), "a_b_c_d");
        assert_eq!(
            sanitize_component("Jacobs, Lisa\0\u{feff}The String Soloists"),
            "Jacobs, Lisa The String Soloists"
        );
        assert!(
            !sanitize_component("A\0\u{feff}B")
                .chars()
                .any(|c| c == '\0' || c == '\u{feff}')
        );
    }

    #[test]
    fn sanitize_truncates_on_char_boundary() {
        let long = "é".repeat(200); // 2 bytes each
        let out = sanitize_component(&long);
        assert!(out.len() <= MAX_COMPONENT_BYTES);
        assert!(!out.is_empty());
        // Still valid UTF-8 made of whole chars.
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn template_optional_group_dropped_when_empty() {
        let mut f = TemplateFields::new();
        f.set("album", "Absolution");
        // No year → the "[{year} - ]" group disappears entirely.
        assert_eq!(render_template("[{year} - ]{album}", &f), "Absolution");

        f.set("year", "2003");
        assert_eq!(
            render_template("[{year} - ]{album}", &f),
            "2003 - Absolution"
        );
    }

    #[test]
    fn template_unknown_placeholder_degrades() {
        let mut f = TemplateFields::new();
        f.set("album", "X");
        assert_eq!(render_template("{nope}{album}", &f), "X");
        assert_eq!(render_template("{unterminated", &f), "{unterminated");
    }

    #[test]
    fn template_empty_components_collapse() {
        let f = TemplateFields::new();
        // Every placeholder empty → no stray "/" components.
        assert_eq!(render_template("{albumartist}/{album}/{title}", &f), "");
    }

    #[test]
    fn summarize_reads_album_fields() {
        let tracks = vec![track("Intro", 1), track("Apocalypse Please", 2)];
        let s = summarize(&tracks);
        assert_eq!(s.album.as_deref(), Some("Absolution"));
        assert_eq!(s.album_artist.as_deref(), Some("Muse"));
        assert_eq!(s.year, Some(2003));
        assert_eq!(s.track_count, 2);
        assert_eq!(s.disc_count, 1);
        assert!(!s.is_compilation);
        assert!(!s.warnings.contains(&"missing_album".to_string()));
    }

    #[test]
    fn summarize_flags_missing_fields() {
        let mut t = track("Untitled", 1);
        t.album = None;
        t.year = None;
        t.title = None;
        t.track_number = None;
        let s = summarize(&[t]);
        for code in [
            "missing_album",
            "missing_year",
            "missing_titles",
            "missing_track_numbers",
            "no_cover",
        ] {
            assert!(s.warnings.contains(&code.to_string()), "missing {code}");
        }
    }

    #[test]
    fn summarize_detects_compilation() {
        let mut a = track("One", 1);
        a.album_artist = None;
        a.artist = Some("Artist A".into());
        let mut b = track("Two", 2);
        b.album_artist = None;
        b.artist = Some("Artist B".into());
        let s = summarize(&[a, b]);
        assert!(s.is_compilation);
        assert!(s.album_artist.is_none());
        assert!(s.warnings.contains(&"mixed_artists".to_string()));
    }

    #[test]
    fn summarize_empty_folder() {
        let s = summarize(&[]);
        assert_eq!(s.track_count, 0);
        assert!(s.warnings.contains(&"no_audio_files".to_string()));
    }

    #[test]
    fn overrides_clear_matching_warning() {
        let mut t = track("One", 1);
        t.year = None;
        let s = summarize(&[t]);
        assert!(s.warnings.contains(&"missing_year".to_string()));

        let fixed = apply_overrides(
            &s,
            &AlbumOverrides {
                year: Some(2003),
                ..Default::default()
            },
        );
        assert_eq!(fixed.year, Some(2003));
        assert!(!fixed.warnings.contains(&"missing_year".to_string()));
    }

    #[test]
    fn plan_uses_default_layout() {
        let tracks = vec![track("Intro", 1), track("Apocalypse Please", 2)];
        let album = summarize(&tracks);
        let plan = build_plan(
            "/dl",
            &tracks,
            &[],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );

        assert_eq!(plan.audio_count(), 2);
        assert_eq!(
            plan.entries[0].relative_path,
            "Muse/2003 - Absolution/01 - Intro.flac"
        );
        assert_eq!(
            plan.entries[0].dest_path,
            "/music/Muse/2003 - Absolution/01 - Intro.flac"
        );
        assert_eq!(
            plan.album_dir.as_deref(),
            Some("/music/Muse/2003 - Absolution")
        );
        assert!(!plan.has_conflicts());
    }

    #[test]
    fn plan_adds_disc_level_only_for_multi_disc() {
        let mut a = track("One", 1);
        a.disc_number = Some(1);
        let mut b = track("Two", 1);
        b.source_path = "/dl/d2-01.flac".into();
        b.disc_number = Some(2);
        let tracks = vec![a, b];
        let album = summarize(&tracks);
        assert_eq!(album.disc_count, 2);

        let plan = build_plan(
            "/dl",
            &tracks,
            &[],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );
        assert_eq!(
            plan.entries[0].relative_path,
            "Muse/2003 - Absolution/Disc 1/01 - One.flac"
        );
        assert_eq!(
            plan.entries[1].relative_path,
            "Muse/2003 - Absolution/Disc 2/01 - Two.flac"
        );
        // Album dir is the album folder, not one of the disc folders.
        assert_eq!(
            plan.album_dir.as_deref(),
            Some("/music/Muse/2003 - Absolution")
        );
    }

    #[test]
    fn plan_flags_duplicate_targets() {
        // Same track number and title twice → one destination for two files.
        let tracks = vec![track("Intro", 1), track("Intro", 1)];
        let album = summarize(&tracks);
        let plan = build_plan(
            "/dl",
            &tracks,
            &[],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );
        assert_eq!(plan.entries[0].conflict, None);
        assert_eq!(plan.entries[1].conflict, Some(Conflict::DuplicateTarget));
        assert_eq!(plan.conflicts(), 1);
    }

    #[test]
    fn plan_flags_existing_destination() {
        let tracks = vec![track("Intro", 1)];
        let album = summarize(&tracks);
        let plan = build_plan(
            "/dl",
            &tracks,
            &[],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Copy),
            &|_| true,
        );
        assert_eq!(plan.entries[0].conflict, Some(Conflict::DestinationExists));
        assert_eq!(plan.mode, FileMode::Copy);
    }

    #[test]
    fn plan_places_extras_in_album_folder() {
        let tracks = vec![track("Intro", 1)];
        let album = summarize(&tracks);
        let plan = build_plan(
            "/dl",
            &tracks,
            &["/dl/cover.jpg".into(), "/dl/rip.log".into()],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );
        let extras: Vec<&PlanEntry> = plan
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Extra)
            .collect();
        assert_eq!(extras.len(), 2);
        assert_eq!(
            extras[0].dest_path,
            "/music/Muse/2003 - Absolution/cover.jpg"
        );
    }

    #[test]
    fn plan_flags_two_extras_landing_on_one_name() {
        // Multi-disc set with a cover in each disc folder: both render to
        // <album>/cover.jpg, which the preview must call out.
        let mut a = track("One", 1);
        a.disc_number = Some(1);
        let mut b = track("Two", 1);
        b.source_path = "/dl/d2/01.flac".into();
        b.disc_number = Some(2);
        let tracks = vec![a, b];
        let album = summarize(&tracks);

        let plan = build_plan(
            "/dl",
            &tracks,
            &["/dl/d1/cover.jpg".into(), "/dl/d2/cover.jpg".into()],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );

        let extras: Vec<&PlanEntry> = plan
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Extra)
            .collect();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].conflict, None);
        assert_eq!(extras[1].conflict, Some(Conflict::DuplicateTarget));
    }

    #[test]
    fn plan_skips_extras_without_audio() {
        let album = summarize(&[]);
        let plan = build_plan(
            "/dl",
            &[],
            &["/dl/cover.jpg".into()],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );
        assert!(plan.entries.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, "no_album_folder");
    }

    #[test]
    fn plan_falls_back_to_filename_when_untitled() {
        let mut t = track("Ignored", 1);
        t.title = None;
        t.source_path = "/dl/weird name.flac".into();
        let album = summarize(&[t.clone()]);
        let plan = build_plan(
            "/dl",
            &[t],
            &[],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );
        assert!(
            plan.entries[0]
                .relative_path
                .ends_with("01 - weird name.flac")
        );
    }

    #[test]
    fn plan_unknown_artist_when_nothing_known() {
        let mut t = SourceTrack {
            source_path: "/dl/x.mp3".into(),
            ext: "mp3".into(),
            ..Default::default()
        };
        t.track_number = Some(3);
        let album = summarize(&[t.clone()]);
        let plan = build_plan(
            "/dl",
            &[t],
            &[],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );
        assert_eq!(
            plan.entries[0].relative_path,
            "Unknown Artist/Unknown Album/03 - x.mp3"
        );
    }

    #[test]
    fn plan_compilation_files_under_various_artists() {
        let mut a = track("One", 1);
        a.album_artist = None;
        a.artist = Some("Artist A".into());
        let mut b = track("Two", 2);
        b.album_artist = None;
        b.artist = Some("Artist B".into());
        let tracks = vec![a, b];
        let album = summarize(&tracks);
        let plan = build_plan(
            "/dl",
            &tracks,
            &[],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );
        assert!(
            plan.entries[0]
                .relative_path
                .starts_with("Various Artists/")
        );
    }

    #[test]
    fn plan_custom_template() {
        let tracks = vec![track("Intro", 1)];
        let album = summarize(&tracks);
        let plan = build_plan(
            "/dl",
            &tracks,
            &[],
            &album,
            &PlanOptions::new(
                "/music",
                "{genre}/{albumartist} - {album}/{track}. {title}",
                FileMode::Move,
            ),
            &never_exists,
        );
        assert_eq!(
            plan.entries[0].relative_path,
            "Rock/Muse - Absolution/01. Intro.flac"
        );
    }

    #[test]
    fn plan_blank_template_falls_back_to_default() {
        let tracks = vec![track("Intro", 1)];
        let album = summarize(&tracks);
        let plan = build_plan(
            "/dl",
            &tracks,
            &[],
            &album,
            &PlanOptions::new("/music", "   ", FileMode::Move),
            &never_exists,
        );
        assert_eq!(plan.template, DEFAULT_TEMPLATE);
        assert_eq!(
            plan.entries[0].relative_path,
            "Muse/2003 - Absolution/01 - Intro.flac"
        );
    }

    #[test]
    fn file_mode_round_trip() {
        assert_eq!(FileMode::parse("move"), Some(FileMode::Move));
        assert_eq!(FileMode::parse(" COPY "), Some(FileMode::Copy));
        assert_eq!(FileMode::parse("link"), None);
        assert_eq!(FileMode::Move.as_str(), "move");
    }

    #[test]
    fn extra_file_detection() {
        assert!(is_extra_file(Path::new("/a/cover.jpg")));
        assert!(is_extra_file(Path::new("/a/rip.LOG")));
        assert!(!is_extra_file(Path::new("/a/track.flac")));
        assert!(!is_extra_file(Path::new("/a/noext")));
    }

    // -- Pairing with a chosen release --

    fn rt(disc: u32, position: u32, title: &str) -> ReleaseTrack {
        ReleaseTrack {
            disc,
            position,
            title: title.into(),
        }
    }

    #[test]
    fn pairs_on_disc_and_track_number() {
        let sources = vec![track("Wrong Title", 1), track("Also Wrong", 2)];
        let release = vec![rt(1, 1, "Intro"), rt(1, 2, "Apocalypse Please")];

        let out = match_release_tracks(&sources, &release);
        assert!(out.iter().all(|p| p.matched));
        assert_eq!(out[0].method.as_deref(), Some("disc_and_number"));
        assert_eq!(out[0].proposed_title.as_deref(), Some("Intro"));
        assert_eq!(out[1].proposed_title.as_deref(), Some("Apocalypse Please"));
    }

    #[test]
    fn pairs_on_title_when_numbering_is_wrong() {
        // Numbers all say 1; titles are right apart from punctuation.
        let mut a = track("Apocalypse Please", 1);
        a.source_path = "/dl/a.flac".into();
        let mut b = track("Time Is Running Out", 1);
        b.source_path = "/dl/b.flac".into();
        let release = vec![
            rt(1, 2, "Apocalypse Please"),
            rt(1, 3, "Time Is Running Out!"),
        ];

        let out = match_release_tracks(&[a, b], &release);
        // The first file takes position 1 by number; the second has no number
        // hit left and falls to the title pass.
        assert!(out.iter().all(|p| p.matched));
        assert_eq!(out[1].method.as_deref(), Some("title"));
        assert_eq!(out[1].proposed_track_number, Some(3));
    }

    #[test]
    fn pairs_untagged_files_in_order() {
        let mut a = SourceTrack {
            source_path: "/dl/01.flac".into(),
            ext: "flac".into(),
            ..Default::default()
        };
        let mut b = a.clone();
        b.source_path = "/dl/02.flac".into();
        a.title = None;
        b.title = None;

        let release = vec![rt(1, 1, "Intro"), rt(1, 2, "Apocalypse Please")];
        let out = match_release_tracks(&[a, b], &release);

        assert!(out.iter().all(|p| p.matched));
        assert!(out.iter().all(|p| p.method.as_deref() == Some("order")));
        assert_eq!(out[0].proposed_title.as_deref(), Some("Intro"));
        assert_eq!(out[1].proposed_title.as_deref(), Some("Apocalypse Please"));
    }

    #[test]
    fn refuses_to_guess_by_order_when_counts_disagree() {
        // Two unmatched files, three spare tracks: pairing by order would be a
        // coin toss, so nothing is proposed.
        let a = SourceTrack {
            source_path: "/dl/01.flac".into(),
            ..Default::default()
        };
        let b = SourceTrack {
            source_path: "/dl/02.flac".into(),
            ..Default::default()
        };
        let release = vec![rt(1, 1, "One"), rt(1, 2, "Two"), rt(1, 3, "Three")];

        let out = match_release_tracks(&[a, b], &release);
        assert!(out.iter().all(|p| !p.matched));
        assert!(out.iter().all(|p| p.proposed_title.is_none()));
    }

    #[test]
    fn pairs_multi_disc_by_disc() {
        let mut a = track("x", 1);
        a.disc_number = Some(1);
        let mut b = track("y", 1);
        b.source_path = "/dl/d2-01.flac".into();
        b.disc_number = Some(2);

        let release = vec![rt(1, 1, "Disc one opener"), rt(2, 1, "Disc two opener")];
        let out = match_release_tracks(&[a, b], &release);

        assert_eq!(out[0].proposed_title.as_deref(), Some("Disc one opener"));
        assert_eq!(out[0].proposed_disc_number, Some(1));
        assert_eq!(out[1].proposed_title.as_deref(), Some("Disc two opener"));
        assert_eq!(out[1].proposed_disc_number, Some(2));
    }

    #[test]
    fn never_uses_one_release_track_twice() {
        // Two files both claiming track 1.
        let a = track("One", 1);
        let mut b = track("One", 1);
        b.source_path = "/dl/dup.flac".into();
        let release = vec![rt(1, 1, "Intro")];

        let out = match_release_tracks(&[a, b], &release);
        assert_eq!(out.iter().filter(|p| p.matched).count(), 1);
    }

    #[test]
    fn extra_local_files_stay_unmatched() {
        // A 15-track rip against the 14-track standard edition.
        let sources: Vec<SourceTrack> = (1..=15).map(|n| track(&format!("T{n}"), n)).collect();
        let release: Vec<ReleaseTrack> = (1..=14).map(|n| rt(1, n, &format!("MB {n}"))).collect();

        let out = match_release_tracks(&sources, &release);
        assert_eq!(out.iter().filter(|p| p.matched).count(), 14);
        assert!(!out[14].matched, "the bonus track has nothing to pair with");
    }

    #[test]
    fn empty_release_proposes_nothing() {
        let out = match_release_tracks(&[track("One", 1)], &[]);
        assert_eq!(out.len(), 1);
        assert!(!out[0].matched);
    }

    #[test]
    fn proposal_detects_whether_anything_changes() {
        let sources = vec![track("Intro", 1)];
        let release = vec![rt(1, 1, "Intro")];
        let out = match_release_tracks(&sources, &release);
        assert!(
            !out[0].changes_anything(),
            "same title and number — nothing to apply"
        );

        let release2 = vec![rt(1, 1, "Intro (remastered)")];
        let out2 = match_release_tracks(&sources, &release2);
        assert!(out2[0].changes_anything());
    }

    #[test]
    fn overrides_are_applied_by_source_path() {
        let mut tracks = vec![track("Old", 1), track("Keep", 2)];
        let overrides = vec![TrackOverride {
            source_path: tracks[0].source_path.clone(),
            title: Some("New".into()),
            track_number: Some(7),
            disc_number: None,
        }];

        apply_track_overrides(&mut tracks, &overrides);
        assert_eq!(tracks[0].title.as_deref(), Some("New"));
        assert_eq!(tracks[0].track_number, Some(7));
        // Untouched fields and other files stay as they were.
        assert_eq!(tracks[0].disc_number, Some(1));
        assert_eq!(tracks[1].title.as_deref(), Some("Keep"));
    }

    #[test]
    fn overrides_ignore_blank_values_and_unknown_paths() {
        let mut tracks = vec![track("Keep", 1)];
        let overrides = vec![
            TrackOverride {
                source_path: tracks[0].source_path.clone(),
                title: Some("   ".into()),
                ..Default::default()
            },
            TrackOverride {
                source_path: "/nowhere.flac".into(),
                title: Some("Ghost".into()),
                ..Default::default()
            },
        ];

        apply_track_overrides(&mut tracks, &overrides);
        assert_eq!(tracks[0].title.as_deref(), Some("Keep"));
    }

    #[test]
    fn overridden_titles_reach_the_destination_path() {
        let mut tracks = vec![track("Untitled", 1)];
        let overrides = [TrackOverride {
            source_path: tracks[0].source_path.clone(),
            title: Some("Apocalypse Please".into()),
            track_number: Some(2),
            disc_number: None,
        }];
        apply_track_overrides(&mut tracks, &overrides);

        let album = summarize(&tracks);
        let plan = build_plan(
            "/dl",
            &tracks,
            &[],
            &album,
            &PlanOptions::new("/music", DEFAULT_TEMPLATE, FileMode::Move),
            &never_exists,
        );
        assert_eq!(
            plan.entries[0].relative_path,
            "Muse/2003 - Absolution/02 - Apocalypse Please.flac"
        );
    }

    // -- Execution tests (real filesystem, inside a temp dir) --

    /// Build a source folder of fake audio files and return (dir, tracks).
    fn staged_source(dir: &Path, names: &[(&str, u32)]) -> Vec<SourceTrack> {
        std::fs::create_dir_all(dir).unwrap();
        names
            .iter()
            .map(|(title, n)| {
                let path = dir.join(format!("{n:02} {title}.flac"));
                std::fs::write(&path, b"audio-bytes").unwrap();
                SourceTrack {
                    source_path: path.to_string_lossy().to_string(),
                    ext: "flac".into(),
                    title: Some((*title).into()),
                    artist: Some("Muse".into()),
                    album_artist: Some("Muse".into()),
                    album: Some("Absolution".into()),
                    year: Some(2003),
                    track_number: Some(*n),
                    disc_number: Some(1),
                    file_size: 11,
                    ..Default::default()
                }
            })
            .collect()
    }

    fn plan_for(
        src: &Path,
        dest_root: &Path,
        tracks: &[SourceTrack],
        extras: &[String],
        mode: FileMode,
    ) -> IngestPlan {
        let album = summarize(tracks);
        build_plan(
            &src.to_string_lossy(),
            tracks,
            extras,
            &album,
            &PlanOptions::new(&dest_root.to_string_lossy(), DEFAULT_TEMPLATE, mode),
            &|p: &Path| p.exists(),
        )
    }

    #[test]
    fn unique_path_suffixes_until_free() {
        let taken = |p: &Path| p.to_string_lossy().ends_with("a.flac");
        let out = unique_path(Path::new("/m/a.flac"), &taken);
        assert_eq!(out, PathBuf::from("/m/a (2).flac"));

        let free = |_: &Path| false;
        assert_eq!(
            unique_path(Path::new("/m/a.flac"), &free),
            PathBuf::from("/m/a.flac")
        );
    }

    #[test]
    fn execute_moves_files_into_place() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1), ("Sing for Absolution", 2)]);

        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Move);
        let report = execute(&plan, ConflictPolicy::Skip);

        assert_eq!(report.files_placed(), 2);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            dest_root
                .join("Muse/2003 - Absolution/01 - Intro.flac")
                .exists()
        );
        // Move really moved: nothing left behind.
        assert!(!Path::new(&tracks[0].source_path).exists());
        assert_eq!(report.bytes, 22);
    }

    #[test]
    fn execute_copy_leaves_source_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1)]);

        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Copy);
        let report = execute(&plan, ConflictPolicy::Skip);

        assert_eq!(report.files_placed(), 1);
        assert!(Path::new(&tracks[0].source_path).exists());
        assert!(
            dest_root
                .join("Muse/2003 - Absolution/01 - Intro.flac")
                .exists()
        );
    }

    #[test]
    fn execute_carries_extras_into_album_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1)]);
        let cover = src.join("cover.jpg");
        std::fs::write(&cover, b"jpeg").unwrap();

        let plan = plan_for(
            &src,
            &dest_root,
            &tracks,
            &[cover.to_string_lossy().to_string()],
            FileMode::Move,
        );
        let report = execute(&plan, ConflictPolicy::Skip);

        assert_eq!(report.files_placed(), 2);
        assert!(dest_root.join("Muse/2003 - Absolution/cover.jpg").exists());
    }

    #[test]
    fn execute_skip_policy_keeps_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1)]);

        let occupied = dest_root.join("Muse/2003 - Absolution/01 - Intro.flac");
        std::fs::create_dir_all(occupied.parent().unwrap()).unwrap();
        std::fs::write(&occupied, b"original").unwrap();

        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Move);
        let report = execute(&plan, ConflictPolicy::Skip);

        assert_eq!(report.files_placed(), 0);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(std::fs::read(&occupied).unwrap(), b"original");
        // Source untouched, so nothing is lost.
        assert!(Path::new(&tracks[0].source_path).exists());
    }

    #[test]
    fn execute_overwrite_policy_replaces_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1)]);

        let occupied = dest_root.join("Muse/2003 - Absolution/01 - Intro.flac");
        std::fs::create_dir_all(occupied.parent().unwrap()).unwrap();
        std::fs::write(&occupied, b"original").unwrap();

        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Move);
        let report = execute(&plan, ConflictPolicy::Overwrite);

        assert_eq!(report.files_placed(), 1);
        assert_eq!(std::fs::read(&occupied).unwrap(), b"audio-bytes");
    }

    #[test]
    fn execute_rename_policy_keeps_both() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1)]);

        let occupied = dest_root.join("Muse/2003 - Absolution/01 - Intro.flac");
        std::fs::create_dir_all(occupied.parent().unwrap()).unwrap();
        std::fs::write(&occupied, b"original").unwrap();

        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Move);
        let report = execute(&plan, ConflictPolicy::Rename);

        assert_eq!(report.files_placed(), 1);
        assert_eq!(std::fs::read(&occupied).unwrap(), b"original");
        assert!(
            dest_root
                .join("Muse/2003 - Absolution/01 - Intro (2).flac")
                .exists()
        );
    }

    #[test]
    fn execute_reports_vanished_source() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1)]);
        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Move);

        // Simulate the file disappearing between preview and apply.
        std::fs::remove_file(&tracks[0].source_path).unwrap();
        let report = execute(&plan, ConflictPolicy::Skip);

        assert_eq!(report.files_placed(), 0);
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn undo_move_puts_files_back_and_cleans_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1), ("Stockholm Syndrome", 2)]);

        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Move);
        let report = execute(&plan, ConflictPolicy::Skip);
        assert_eq!(report.files_placed(), 2);

        let undone = undo(&report);
        assert_eq!(undone.moved.len(), 2);
        assert!(undone.errors.is_empty(), "{:?}", undone.errors);
        assert!(Path::new(&tracks[0].source_path).exists());
        assert!(!dest_root.join("Muse/2003 - Absolution").exists());
    }

    #[test]
    fn undo_copy_removes_the_copies_only() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1)]);

        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Copy);
        let report = execute(&plan, ConflictPolicy::Skip);
        let undone = undo(&report);

        assert_eq!(undone.moved.len(), 1);
        // The source copy is what the user still has; it must survive.
        assert!(Path::new(&tracks[0].source_path).exists());
        assert!(
            !dest_root
                .join("Muse/2003 - Absolution/01 - Intro.flac")
                .exists()
        );
    }

    #[test]
    fn undo_refuses_when_source_path_reoccupied() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dl");
        let dest_root = tmp.path().join("music");
        let tracks = staged_source(&src, &[("Intro", 1)]);

        let plan = plan_for(&src, &dest_root, &tracks, &[], FileMode::Move);
        let report = execute(&plan, ConflictPolicy::Skip);

        // Something else now sits where the file came from.
        std::fs::write(&tracks[0].source_path, b"different file").unwrap();
        let undone = undo(&report);

        assert_eq!(undone.moved.len(), 0);
        assert_eq!(undone.errors.len(), 1);
        // Neither copy was destroyed.
        assert_eq!(
            std::fs::read(&tracks[0].source_path).unwrap(),
            b"different file"
        );
        assert!(
            dest_root
                .join("Muse/2003 - Absolution/01 - Intro.flac")
                .exists()
        );
    }
}
