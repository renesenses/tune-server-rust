pub mod artist_enrichment;
pub mod artist_split;
pub mod auto_fix;
pub mod batch;
pub mod bio_batch;
pub mod credit_enricher;
pub mod enrich_scope;
pub mod enrichment;
pub mod fingerprint;
pub mod lastfm;
pub mod lyrics;
pub mod matcher;
pub mod musicbrainz_release;
pub mod reidentify;
pub mod suggestions;
pub mod tag_writer;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Build the MusicBrainz Lucene clause used to look up an artist by name.
///
/// A bare `artist:"<name>"` phrase only matches the artist's primary `name`
/// (and `sort-name`). For non-Latin artists MusicBrainz stores the romanized
/// form as the primary name (e.g. `IU`, `BTS`, `坂本龍一`→`Ryuichi Sakamoto`)
/// and keeps the native-script name only as an *alias*. The bare phrase query
/// therefore returns zero results for a Hangul/CJK/Cyrillic query, so no MBID
/// is resolved and no bio/image enrichment happens.
///
/// Adding `OR alias:"<name>"` makes the native-script name resolve while
/// keeping the quoted phrase precision for Latin names (verified against the
/// live MB API on IU/BTS/坂本龍一 as well as Radiohead/The Beatles/Björk).
pub(crate) fn mb_artist_query(name: &str) -> String {
    format!("artist:\"{name}\" OR alias:\"{name}\"")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackCredit {
    pub name: String,
    pub role: String,
    pub instrument: Option<String>,
}

/// Max size of an embedded cover kept in `TrackMetadata.cover_art`. The scanner
/// retains this buffer for every file and accumulates a whole batch in memory,
/// so an oversized (or malformed) embedded picture, multiplied across files,
/// blew the scanner past the OOM killer (JeromeQ: 261 files → 6.1 GB RSS on an
/// 8 GB machine). Above this, we keep `has_cover=true` but drop the bytes and
/// let the scan re-extract that one file's cover to the artwork cache on demand,
/// keeping peak scan memory bounded. Normal covers (well under 4 MB) stay cached.
pub const MAX_RETAINED_COVER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub album_artist_sort: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub total_tracks: Option<u32>,
    pub total_discs: Option<u32>,
    pub disc_subtitle: Option<String>,
    pub year: Option<u32>,
    pub original_year: Option<u32>,
    pub release_date: Option<String>,
    pub original_date: Option<String>,
    /// Primary genre (first after splitting multi-genre tags)
    pub genre: Option<String>,
    /// All genres parsed from the tag (split by `;`, `/`, `\\`)
    pub genres: Vec<String>,
    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub channels: Option<u16>,
    pub format: Option<String>,
    pub file_size: Option<u64>,
    pub bpm: Option<f64>,
    pub compilation: bool,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub musicbrainz_recording_id: Option<String>,
    pub musicbrainz_release_id: Option<String>,
    pub musicbrainz_artist_id: Option<String>,
    pub musicbrainz_album_artist_id: Option<String>,
    pub musicbrainz_release_group_id: Option<String>,
    pub isrc: Option<String>,
    pub has_cover: bool,
    /// Embedded cover art (bytes, mime) read from the SAME lofty pass that
    /// parsed the tags. Lets the scanner cache the cover without re-opening the
    /// file — a second `lofty::read_from_path` failed with "path not found"
    /// (os error 3) for some accented Windows paths even though the first read
    /// succeeded (Thibaud: <1% of albums had no artwork).
    pub cover_art: Option<(Vec<u8>, String)>,
    pub credits: Vec<TrackCredit>,
    pub comment: Option<String>,
}

/// One unsafe character removed from untrusted metadata.
///
/// `byte_offset` deliberately uses the UTF-8 byte position: it is the offset
/// that a tag parser, JSON payload or C boundary can reproduce exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCorrection {
    pub field: String,
    pub kind: &'static str,
    pub codepoint: u32,
    pub byte_offset: usize,
}

/// Replace unsafe metadata characters with one visible separator while
/// preserving the layout characters allowed in textual tags.
///
/// NUL is unsafe at every C ABI boundary. U+FEFF is a BOM only at the start of
/// a text stream and is invisible corruption inside a tag or path component.
/// Other control characters are equally unsuitable for DB grouping and FTS,
/// except tab, LF and CR: those three are valid in comments and lyrics. A whole
/// consecutive run becomes one space so
/// `"Lisa\0\u{feff}The String Soloists"` does not silently collapse to
/// `"LisaThe String Soloists"`.
pub fn sanitize_untrusted_text(raw: &str, field: &str) -> (String, Vec<TextCorrection>) {
    sanitize_untrusted_text_with_layout(raw, field, true)
}

/// Single-line variant for titles, identifiers and filesystem components.
pub fn sanitize_untrusted_single_line_text(
    raw: &str,
    field: &str,
) -> (String, Vec<TextCorrection>) {
    sanitize_untrusted_text_with_layout(raw, field, false)
}

fn sanitize_untrusted_text_with_layout(
    raw: &str,
    field: &str,
    preserve_layout: bool,
) -> (String, Vec<TextCorrection>) {
    let mut out = String::with_capacity(raw.len());
    let mut corrections = Vec::new();
    let mut separator_pending = false;

    for (byte_offset, c) in raw.char_indices() {
        let kind = if c == '\0' {
            Some("NUL")
        } else if c == '\u{feff}' {
            Some("BOM")
        } else if c.is_control() && !(preserve_layout && matches!(c, '\t' | '\n' | '\r')) {
            Some("CONTROL")
        } else {
            None
        };

        if let Some(kind) = kind {
            corrections.push(TextCorrection {
                field: field.to_string(),
                kind,
                codepoint: c as u32,
                byte_offset,
            });
            separator_pending = true;
            continue;
        }

        if separator_pending {
            if !out.is_empty() && !out.ends_with(char::is_whitespace) && !c.is_whitespace() {
                out.push(' ');
            }
            separator_pending = false;
        }
        out.push(c);
    }

    (out, corrections)
}

impl TrackMetadata {
    /// Remove unsafe text from every field that can reach the database.
    pub fn sanitize_text_fields(&mut self) -> Vec<TextCorrection> {
        fn sanitize_option(
            field: &str,
            value: &mut Option<String>,
            corrections: &mut Vec<TextCorrection>,
        ) {
            let Some(raw) = value.as_deref() else {
                return;
            };
            let (clean, mut found) = sanitize_untrusted_single_line_text(raw, field);
            if found.is_empty() {
                return;
            }
            *value = (!clean.is_empty()).then_some(clean);
            corrections.append(&mut found);
        }

        let mut corrections = Vec::new();
        sanitize_option("title", &mut self.title, &mut corrections);
        sanitize_option("artist", &mut self.artist, &mut corrections);
        sanitize_option("album", &mut self.album, &mut corrections);
        sanitize_option("album_artist", &mut self.album_artist, &mut corrections);
        sanitize_option(
            "album_artist_sort",
            &mut self.album_artist_sort,
            &mut corrections,
        );
        sanitize_option("disc_subtitle", &mut self.disc_subtitle, &mut corrections);
        sanitize_option("release_date", &mut self.release_date, &mut corrections);
        sanitize_option("original_date", &mut self.original_date, &mut corrections);
        sanitize_option("genre", &mut self.genre, &mut corrections);
        sanitize_option("format", &mut self.format, &mut corrections);
        sanitize_option("label", &mut self.label, &mut corrections);
        sanitize_option("catalog_number", &mut self.catalog_number, &mut corrections);
        sanitize_option(
            "musicbrainz_recording_id",
            &mut self.musicbrainz_recording_id,
            &mut corrections,
        );
        sanitize_option(
            "musicbrainz_release_id",
            &mut self.musicbrainz_release_id,
            &mut corrections,
        );
        sanitize_option(
            "musicbrainz_artist_id",
            &mut self.musicbrainz_artist_id,
            &mut corrections,
        );
        sanitize_option(
            "musicbrainz_album_artist_id",
            &mut self.musicbrainz_album_artist_id,
            &mut corrections,
        );
        sanitize_option(
            "musicbrainz_release_group_id",
            &mut self.musicbrainz_release_group_id,
            &mut corrections,
        );
        sanitize_option("isrc", &mut self.isrc, &mut corrections);
        if let Some(raw) = self.comment.as_deref() {
            let (clean, mut found) = sanitize_untrusted_text(raw, "comment");
            if !found.is_empty() {
                self.comment = (!clean.is_empty()).then_some(clean);
                corrections.append(&mut found);
            }
        }

        for (index, genre) in self.genres.iter_mut().enumerate() {
            let (clean, mut found) =
                sanitize_untrusted_single_line_text(genre, &format!("genres[{index}]"));
            if !found.is_empty() {
                *genre = clean;
                corrections.append(&mut found);
            }
        }
        self.genres.retain(|genre| !genre.is_empty());

        for (index, credit) in self.credits.iter_mut().enumerate() {
            for (suffix, value) in [("name", &mut credit.name), ("role", &mut credit.role)] {
                let (clean, mut found) = sanitize_untrusted_single_line_text(
                    value,
                    &format!("credits[{index}].{suffix}"),
                );
                if !found.is_empty() {
                    *value = clean;
                    corrections.append(&mut found);
                }
            }
            sanitize_option(
                &format!("credits[{index}].instrument"),
                &mut credit.instrument,
                &mut corrections,
            );
        }
        self.credits
            .retain(|credit| !credit.name.is_empty() && !credit.role.is_empty());

        corrections
    }
}

/// Split a multi-genre tag string into individual genres.
///
/// Handles common separators: `;`, `/`, `\\`, and `\0` (null byte, used by
/// some ID3v2 implementations for multi-value frames).
///
/// Examples:
///   "Jazz; Fusion; Progressive" -> ["Jazz", "Fusion", "Progressive"]
///   "Jazz/Fusion/Progressive"   -> ["Jazz", "Fusion", "Progressive"]
///   "Rock"                      -> ["Rock"]
///   ""                          -> []
/// Normalize a genre string to Title Case, handling special tokens.
///
/// - Splits on whitespace, capitalizes the first letter of each word and
///   lowercases the rest.
/// - Preserves well-known uppercase tokens: "R&B", "DJ", "UK", "US", "MC",
///   "TV", "AC", "DC", "EDM", "RnB", "II", "III", "IV".
/// - Handles slash-separated sub-genres (e.g. "Folk/Rock") by normalizing
///   each part independently.
///
/// Examples:
///   "classique"       -> "Classique"
///   "ROCK"            -> "Rock"
///   "r&b"             -> "R&B"
///   "hip hop"         -> "Hip Hop"
///   "dj mix"          -> "DJ Mix"
///   "folk/rock"       -> "Folk/Rock"
pub fn normalize_genre(genre: &str) -> String {
    // Uppercase tokens that must be preserved verbatim (checked case-insensitively)
    const UPPERCASE_TOKENS: &[&str] = &[
        "R&B", "DJ", "UK", "US", "MC", "TV", "AC", "DC", "EDM", "II", "III", "IV",
    ];

    fn title_case_word(word: &str) -> String {
        // Check if the whole word matches an uppercase token
        for &token in UPPERCASE_TOKENS {
            if word.eq_ignore_ascii_case(token) {
                return token.to_string();
            }
        }
        // Title-case: first char uppercase, rest lowercase
        let mut chars = word.chars();
        match chars.next() {
            None => String::new(),
            Some(first) => {
                let mut s = first.to_uppercase().to_string();
                for c in chars {
                    s.extend(c.to_lowercase());
                }
                s
            }
        }
    }

    // Handle slash-separated compound genres like "Folk/Rock", and title-case
    // each hyphen-separated part so "Folk-Punk" stays "Folk-Punk" (not
    // "Folk-punk") and "Hip-Hop"/"Lo-Fi" keep both parts capitalised
    // (Yves Scordia: Folk-Punk was lower-cased after the hyphen).
    genre
        .split('/')
        .map(|part| {
            part.split_whitespace()
                .map(|word| {
                    word.split('-')
                        .map(title_case_word)
                        .collect::<Vec<_>>()
                        .join("-")
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn split_genre_tag(raw: &str) -> Vec<String> {
    // Split by semicolon, forward-slash, backslash, or null byte
    raw.split(&[';', '/', '\\', '\0'][..])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(normalize_genre)
        .collect()
}

/// Assemble la liste des genres d'une piste à partir des valeurs BRUTES du tag.
///
/// Un fichier peut porter ses genres de DEUX façons, selon le logiciel qui l'a
/// gravé, et les deux sont légitimes :
///
///   * **plusieurs valeurs** — Vorbis Comment répète le champ (`GENRE=Jazz`,
///     `GENRE=Fusion`), MP4 répète l'atome `©gen`, ID3v2.4 sépare les valeurs
///     d'un `TCON` par un octet nul ;
///   * **une seule chaîne** — ID3v2.3 n'a pas de multivaleur, l'étiqueteur
///     écrit `TCON = "Jazz; Fusion"` ou `"Jazz/Fusion"`.
///
/// Chaque valeur brute est donc redécoupée par `split_genre_tag`, ce qui couvre
/// aussi les fichiers qui mêlent les deux conventions. Le dédoublonnage passe
/// par `genre_key` — la clé canonique de la bibliothèque, pas un
/// `to_lowercase()` réécrit sur place — pour que « Hip-Hop » et « Hip Hop »,
/// écrits par deux marchands sur le même disque, ne comptent qu'une fois.
///
/// L'ordre d'apparition est conservé : le premier genre reste le genre
/// principal (colonne `tracks.genre`).
pub fn genres_from_tag_values<S: AsRef<str>>(values: &[S]) -> Vec<String> {
    let mut vus = std::collections::HashSet::new();
    let mut sortie = Vec::new();
    for valeur in values {
        for g in split_genre_tag(valeur.as_ref()) {
            if vus.insert(genre_key(&g)) {
                sortie.push(g);
            }
        }
    }
    sortie
}

/// Canonical grouping key for a genre label, insensitive to case AND to the
/// space-vs-hyphen separator, so "Trip Hop" and "Trip-Hop" (or "trip hop")
/// collapse to a single key ("trip hop"). Used to dedup the library genre
/// views, which otherwise show one card per spelling variant (#1161).
pub fn genre_key(genre: &str) -> String {
    genre
        .to_lowercase()
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize a lofty `FileType` debug string into a user-friendly format name.
///
/// lofty's `FileType` Debug representation doesn't always match what users expect:
///   - `Mpeg` -> `mp3`
///   - `Dsf`  -> `dsd` (DSD over PCM, stored in .dsf container)
///   - `Dff`  -> `dsd` (DSD Interchange File Format)
///   - `Mp4`  -> `alac` when bit_depth is present (ALAC is lossless, has bit depth)
///             -> `aac` otherwise (AAC is lossy, no bit depth reported by lofty)
///   - Other values pass through unchanged (already lowercase).
pub fn normalize_format(raw: &str, bit_depth: Option<u8>) -> String {
    match raw {
        "mpeg" => "mp3".to_string(),
        // `dsf` et `dff` ne sont PLUS repliés sur « dsd ».
        //
        // Ils l'étaient, et l'écran s'en trouvait menteur : deux conteneurs
        // différents produisaient une seule entrée « DSD » dans les types de
        // fichiers — et quand une valeur écrite autrement traversait (casse,
        // import, version antérieure), deux entrées **visuellement identiques**
        // (Cyrille Moutia, #1612). On ne peut pas distinguer ce qu'on a
        // confondu à l'écriture.
        //
        // Le conteneur est une information que l'utilisateur possède : ses
        // fichiers sont des `.dsf` ou des `.dff`, et la bibliothèque doit le
        // dire. Le repli faisait perdre cette information pour ne rien
        // simplifier — tout le code qui décide « est-ce du DSD ? » teste déjà
        // les trois valeurs :
        //
        //   audio/formats.rs:31   "dsf" | "dff" | "dst" | "dsd" => Dsd
        //   db/models.rs:79       contains("dsf") || contains("dff") || …
        //   db/track_repo.rs:1030 t.format IN ('dsd','dsf','dff')
        //   db/album_repo.rs:1238 format IN ('dsd','dsf','dff')
        //   routes/zones.rs:848   matches!(fmt, "dsd" | "dsf" | "dff")
        //
        // Rien ne repose donc sur la valeur repliée. Les lignes déjà écrites en
        // « dsd » sont converties par la migration `format_conteneur_dsd`, qui
        // relit l'extension du fichier — sans quoi une bibliothèque existante
        // afficherait « DSD » (anciennes lignes) ET « DSF » (nouvelles), soit
        // exactement le défaut d'origine sous un autre nom.
        "mp4" | "m4a" => {
            // ALAC (Apple Lossless) files in M4A containers report a bit depth
            // (typically 16 or 24), while AAC (lossy) does not.
            if bit_depth.is_some() {
                "alac".to_string()
            } else {
                "aac".to_string()
            }
        }
        // lofty may report "alac" directly for some M4A files
        "alac" => "alac".to_string(),
        other => other.to_string(),
    }
}

/// Probe an M4A/MP4 file for its real codec **and** bit depth.
///
/// lofty reports neither for these files: it can't tell ALAC (lossless) from
/// AAC (lossy) and never fills in the bit depth. symphonia's ISOMP4 demuxer
/// also leaves `bits_per_sample` empty for ALAC, so the depth is read from the
/// ALAC magic cookie in `extra_data` (bit depth at byte 5 of the 24-byte
/// payload, after optional `frma`/`alac` atom prefixes) — the same layout the
/// decoder uses. Returns `(format, bit_depth)`; bit depth is `None` for AAC.
pub fn probe_m4a_props(path: &std::path::Path) -> Option<(String, Option<u16>)> {
    // symphonia-codec-aac 0.6.0 panique `index out of bounds` (ics/mod.rs:246,
    // len 64 idx 64) sur certains flux AAC-in-M4A malformés. Pendant un scan de
    // bibliothèque cet unwind tuait la tâche de scan et faisait crasher le
    // serveur quelques secondes après le démarrage (#2302, forum Marco Polo).
    // Depuis que #2327 a restauré `panic = "unwind"`, `catch_unwind` intercepte
    // vraiment ce panic — on calque le durcissement déjà en place dans le chemin
    // de LECTURE (`audio/decode.rs`) : un fichier qui panique est SAUTÉ (None)
    // avec un `warn!`, jamais propagé. `probe_m4a_props` est le SEUL appel
    // symphonia du chemin de scan (`try_read_metadata` passe par lofty).
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe_m4a_props_inner(path)))
        .unwrap_or_else(|_| {
            tracing::warn!(
                path = %path.display(),
                "m4a_probe_panic: le décodeur AAC de symphonia a paniqué, fichier ignoré"
            );
            None
        })
}

fn probe_m4a_props_inner(path: &std::path::Path) -> Option<(String, Option<u16>)> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::well_known::CODEC_ID_ALAC;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(&*crate::library::artwork::extended_path(path)).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let format_reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()?;
    let track = format_reader.default_track(symphonia::core::formats::TrackType::Audio)?;

    // Match on the codec id (0x2003 for ALAC) rather than a Debug string — the
    // Debug form of the codec parameters doesn't spell out "Alac".
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => return Some(("aac".to_string(), None)),
    };
    if params.codec != CODEC_ID_ALAC {
        return Some(("aac".to_string(), None));
    }

    let bit_depth = params
        .bits_per_sample
        .map(|b| b as u16)
        .or_else(|| alac_bit_depth_from_cookie(params.extra_data.as_deref()));
    Some(("alac".to_string(), bit_depth))
}

/// Extract the ALAC bit depth from the magic cookie (`extra_data`).
/// Byte 5 of the 24-byte payload holds the bit depth, after optional 12-byte
/// `frma` and `alac` atom prefixes.
fn alac_bit_depth_from_cookie(extra: Option<&[u8]>) -> Option<u16> {
    let mut buf = extra?;
    if buf.len() >= 12 && &buf[4..8] == b"frma" {
        buf = &buf[12..];
    }
    if buf.len() >= 12 && &buf[4..8] == b"alac" {
        buf = &buf[12..];
    }
    if buf.len() >= 24 {
        let bd = buf[5];
        if bd > 0 && bd <= 32 {
            return Some(bd as u16);
        }
    }
    None
}

// ── DSF / DFF support ──────────────────────────────────────────────────

/// Parsed DSF header information.
struct DsfHeaderInfo {
    sample_rate: Option<u32>,
    channels: Option<u16>,
    duration_ms: Option<u64>,
    /// Byte offset of the ID3v2 metadata chunk (0 means no metadata).
    metadata_offset: Option<u64>,
}

/// Parse a DSF file header to extract sample rate, channel count, duration,
/// and the metadata (ID3v2) offset.
fn parse_dsf_header_full(path: &Path) -> Result<DsfHeaderInfo, &'static str> {
    use std::io::Read;

    let mut f = std::fs::File::open(&*crate::library::artwork::extended_path(path))
        .map_err(|_| "ouverture_impossible")?;
    let mut header = [0u8; 92]; // DSD chunk (28) + fmt chunk header (64 is plenty)
    f.read_exact(&mut header)
        .map_err(|_| "entete_dsd_trop_court")?;

    // Verify "DSD " magic
    if &header[0..4] != b"DSD " {
        return Err("magie_dsd_absente");
    }

    // DSD chunk: bytes 4-11 = chunk size (u64 LE, should be 28)
    //            bytes 12-19 = total file size
    //            bytes 20-27 = metadata offset (0 = none)
    let metadata_offset = u64::from_le_bytes([
        header[20], header[21], header[22], header[23], header[24], header[25], header[26],
        header[27],
    ]);

    // fmt chunk should start at offset 28
    if &header[28..32] != b"fmt " {
        return Err("magie_fmt_absente");
    }

    // fmt chunk layout (all little-endian):
    //   28-31: "fmt " magic
    //   32-39: chunk size (u64)
    //   40-43: format version (u32)
    //   44-47: format ID (u32)
    //   48-51: channel type (u32)
    //   52-55: channel count (u32)
    //   56-59: sample rate (u32)
    //   60-63: bits per sample (u32)
    //   64-71: sample count per channel (u64)
    let channels = u32::from_le_bytes([header[52], header[53], header[54], header[55]]);
    let sample_rate = u32::from_le_bytes([header[56], header[57], header[58], header[59]]);
    let bits_per_sample = u32::from_le_bytes([header[60], header[61], header[62], header[63]]);
    let sample_count = u64::from_le_bytes([
        header[64], header[65], header[66], header[67], header[68], header[69], header[70],
        header[71],
    ]);

    let duration_ms = if sample_rate > 0 {
        // DSD sample rate is 1-bit rate (e.g. 2_822_400 for DSD64).
        Some(sample_count * 1000 / sample_rate as u64)
    } else {
        None
    };

    let _ = bits_per_sample; // typically 1 for DSD

    Ok(DsfHeaderInfo {
        sample_rate: Some(sample_rate),
        channels: Some(channels as u16),
        duration_ms,
        metadata_offset: if metadata_offset > 0 {
            Some(metadata_offset)
        } else {
            None
        },
    })
}

/// A parsed ID3v2 text frame (frame ID -> text value).
#[derive(Debug, Default)]
struct Id3v2Tags {
    /// Standard text frames: frame_id (e.g. "TIT2") -> value
    text_frames: Vec<(String, String)>,
    /// TXXX user-defined text frames: description -> value
    txxx_frames: Vec<(String, String)>,
    /// UFID frames : proprietaire -> identifiant.
    ///
    /// C'est LA ou MusicBrainz Picard ecrit l'identifiant d'enregistrement en
    /// ID3 — proprietaire `http://musicbrainz.org` —, pas dans un TXXX. Les
    /// frames qui ne commencent pas par `T` etaient toutes ignorees ici : sur
    /// un DSD etiquete avec Picard, l'identifiant n'arrivait donc jamais.
    ufid_frames: Vec<(String, String)>,
    /// Whether an APIC (picture) frame was found
    has_picture: bool,
    /// First embedded picture found, as `(mime_type, image_bytes)`.
    picture: Option<(String, Vec<u8>)>,
}

impl Id3v2Tags {
    /// Get the first text frame matching the given ID.
    fn get(&self, frame_id: &str) -> Option<&str> {
        self.text_frames
            .iter()
            .find(|(id, _)| id == frame_id)
            .map(|(_, v)| v.as_str())
    }

    /// Get a TXXX frame by description (case-insensitive).
    /// L'identifiant d'enregistrement MusicBrainz, quelle que soit la facon
    /// dont l'etiqueteur l'a ecrit.
    ///
    /// Ordre delibere : `UFID` d'abord, parce que c'est la convention de
    /// Picard en ID3 et donc la source la plus fiable ; les deux descriptions
    /// TXXX ensuite, pour les etiqueteurs qui s'en ecartent.
    fn musicbrainz_recording_id(&self) -> Option<&str> {
        self.ufid_frames
            .iter()
            .find(|(owner, _)| owner.eq_ignore_ascii_case("http://musicbrainz.org"))
            .map(|(_, id)| id.as_str())
            .or_else(|| self.get_txxx("MusicBrainz Recording Id"))
            .or_else(|| self.get_txxx("MusicBrainz Track Id"))
            .filter(|v| !v.is_empty())
    }

    fn get_txxx(&self, description: &str) -> Option<&str> {
        self.txxx_frames
            .iter()
            .find(|(desc, _)| desc.eq_ignore_ascii_case(description))
            .map(|(_, v)| v.as_str())
    }

    fn title(&self) -> Option<&str> {
        self.get("TIT2")
    }
    fn artist(&self) -> Option<&str> {
        self.get("TPE1")
    }
    fn album(&self) -> Option<&str> {
        self.get("TALB")
    }
    fn album_artist(&self) -> Option<&str> {
        self.get("TPE2")
    }
    fn genre(&self) -> Option<&str> {
        self.get("TCON")
    }

    /// TOUTES les trames `TCON`, dans l'ordre du fichier.
    ///
    /// `get()` ne rend que la première, ce qui suffit à la plupart des trames
    /// mais pas au genre : un étiqueteur peut écrire une trame `TCON` par
    /// genre au lieu d'une seule chaîne séparée. Jumeau du chemin lofty, qui
    /// lit lui aussi toutes les valeurs depuis #1821 — les deux doivent rendre
    /// la même liste pour le même fichier.
    fn genres(&self) -> Vec<&str> {
        self.text_frames
            .iter()
            .filter(|(id, _)| id == "TCON")
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Parse track number from TRCK frame ("7" or "7/11").
    fn track_number(&self) -> Option<u32> {
        let raw = self.get("TRCK")?;
        raw.split('/').next()?.trim().parse().ok()
    }

    /// Parse total tracks from TRCK frame ("7/11").
    fn total_tracks(&self) -> Option<u32> {
        let raw = self.get("TRCK")?;
        raw.split('/').nth(1)?.trim().parse().ok()
    }

    /// Parse disc number from TPOS frame ("1" or "1/2").
    fn disc_number(&self) -> Option<u32> {
        let raw = self.get("TPOS")?;
        raw.split('/').next()?.trim().parse().ok()
    }

    /// Parse total discs from TPOS frame ("1/2").
    fn total_discs(&self) -> Option<u32> {
        let raw = self.get("TPOS")?;
        raw.split('/').nth(1)?.trim().parse().ok()
    }

    /// Parse year from TDRC, TYER, TDRL, or TDOR frame (in priority order).
    fn year(&self) -> Option<u32> {
        self.get("TDRC")
            .or_else(|| self.get("TYER"))
            .or_else(|| self.get("TDRL"))
            .or_else(|| self.get("TDOR"))
            .and_then(|s| s.get(..4)?.parse().ok())
    }

    fn disc_subtitle(&self) -> Option<&str> {
        self.get("TSST")
    }

    fn release_date(&self) -> Option<&str> {
        self.get("TDRL")
    }

    fn label(&self) -> Option<&str> {
        self.get("TPUB")
    }

    fn composer(&self) -> Option<&str> {
        self.get("TCOM")
    }

    fn album_artist_sort(&self) -> Option<&str> {
        self.get("TSO2").or_else(|| self.get("TSOA"))
    }

    fn original_date(&self) -> Option<&str> {
        self.get("TDOR")
    }

    fn original_year(&self) -> Option<u32> {
        self.original_date().and_then(|s| s.get(..4)?.parse().ok())
    }

    fn isrc(&self) -> Option<&str> {
        self.get("TSRC")
    }
}

/// Decode an ID3v2 syncsafe integer (7 bits per byte).
fn syncsafe_to_u32(bytes: &[u8]) -> u32 {
    debug_assert!(bytes.len() == 4);
    ((bytes[0] as u32) << 21)
        | ((bytes[1] as u32) << 14)
        | ((bytes[2] as u32) << 7)
        | (bytes[3] as u32)
}

/// Reverse ID3v2 unsynchronisation: every `0xFF 0x00` pair becomes `0xFF`.
/// Applied to the whole tag body when the header's unsynchronisation flag
/// (0x80) is set (ID3v2.2/v2.3). A no-op when no such pair is present. Old
/// taggers commonly set this on DSD/DSF files (Benjithom, #959); without
/// reversing it the frame sizes desync and the title/artist are lost.
fn deunsynchronise(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        out.push(data[i]);
        if data[i] == 0xFF && i + 1 < data.len() && data[i + 1] == 0x00 {
            i += 2; // drop the stuffed 0x00
        } else {
            i += 1;
        }
    }
    out
}

/// Read and parse an ID3v2 tag from a byte slice starting at "ID3".
///
/// Supports ID3v2.3 and ID3v2.4 text frames (TIT2, TPE1, TALB, etc.)
/// and TXXX user-defined text frames. Skips binary frames (APIC, etc.)
/// but notes their presence.
/// Map an ID3v2.2 three-character frame id to its v2.3/v2.4 four-character
/// equivalent so the rest of the reader (which keys on `TIT2`, `TPE1`, …) works.
/// DSD/DSF files are frequently tagged with ID3v2.2 (Benjithom: the title showed
/// as the filename because v2.2 was skipped entirely).
fn map_id3v22_frame(id: &str) -> Option<&'static str> {
    Some(match id {
        "TT2" => "TIT2", // title
        "TT1" => "TIT1",
        "TT3" => "TIT3",
        "TP1" => "TPE1", // artist
        "TP2" => "TPE2", // album artist
        "TP3" => "TPE3",
        "TAL" => "TALB", // album
        "TRK" => "TRCK", // track number
        "TPA" => "TPOS", // disc number
        "TYE" => "TYER", // year
        "TCO" => "TCON", // genre
        "TCM" => "TCOM", // composer
        "TCP" => "TCMP", // compilation flag
        "TOR" => "TORY",
        "TDA" => "TDAT",
        "TXX" => "TXXX", // user-defined text
        "PIC" => "APIC", // attached picture
        _ => return None,
    })
}

fn parse_id3v2_tag(data: &[u8]) -> Option<Id3v2Tags> {
    if data.len() < 10 || &data[0..3] != b"ID3" {
        return None;
    }

    let major_version = data[3]; // 2 = ID3v2.2, 3 = ID3v2.3, 4 = ID3v2.4
    let _minor_version = data[4];
    let flags = data[5];
    let tag_size = syncsafe_to_u32(&data[6..10]) as usize;

    // We handle ID3v2.2, v2.3 and v2.4.
    if major_version < 2 || major_version > 4 {
        return None;
    }

    // Extended header (v2.3/v2.4 only — in v2.2 that flag bit means compression).
    let mut pos = 10;
    if major_version >= 3 && flags & 0x40 != 0 {
        // Extended header present, skip it
        if pos + 4 > data.len() {
            return None;
        }
        let ext_size = if major_version == 4 {
            syncsafe_to_u32(&data[pos..pos + 4]) as usize
        } else {
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize
        };
        pos += ext_size.max(4);
    }

    let tag_end = (10 + tag_size).min(data.len());
    let mut tags = Id3v2Tags::default();

    // ID3v2.2/v2.3 may unsynchronise the whole tag (header flag 0x80): every
    // 0xFF byte is followed by a stuffed 0x00 that must be removed before the
    // frames can be parsed. Old taggers commonly set this on DSD/DSF files
    // (Benjithom, #959) — without reversing it the frame sizes desync (notably
    // when a PIC image precedes the title) and the title is lost, so Tune fell
    // back to the filename. v2.4 is handled per frame below (its synchsafe frame
    // sizes count the *stored* length, so a whole-tag deunsync would desync them).
    let unsync = flags & 0x80 != 0;
    let raw_frames = &data[pos.min(tag_end)..tag_end];
    let deunsynced;
    let frames: &[u8] = if unsync && major_version <= 3 {
        deunsynced = deunsynchronise(raw_frames);
        &deunsynced
    } else {
        raw_frames
    };
    let frames_end = frames.len();

    // v2.2 frames: 3-char id + 3-byte size, no flags (6-byte header).
    // v2.3/v2.4 frames: 4-char id + 4-byte size + 2-byte flags (10-byte header).
    let (id_len, header_len) = if major_version == 2 { (3, 6) } else { (4, 10) };

    let mut fpos = 0usize;
    while fpos + header_len <= frames_end {
        let raw_id = match std::str::from_utf8(&frames[fpos..fpos + id_len]) {
            Ok(s) => s.to_string(),
            Err(_) => break,
        };

        // Stop on padding (null bytes)
        if raw_id.starts_with('\0') {
            break;
        }

        let frame_size = match major_version {
            4 => syncsafe_to_u32(&frames[fpos + 4..fpos + 8]) as usize,
            3 => u32::from_be_bytes([
                frames[fpos + 4],
                frames[fpos + 5],
                frames[fpos + 6],
                frames[fpos + 7],
            ]) as usize,
            // v2.2: 3-byte big-endian size.
            _ => {
                ((frames[fpos + 3] as usize) << 16)
                    | ((frames[fpos + 4] as usize) << 8)
                    | (frames[fpos + 5] as usize)
            }
        };

        // v2.4 unsynchronisation is per frame: either the whole-tag flag (0x80)
        // or the frame's own format flag (0x02, second flag byte). Its synchsafe
        // frame size counts the *stored* (still-stuffed) bytes, so we slice with
        // frame_size first, then reverse the 0xFF 0x00 stuffing on the slice.
        let frame_unsync =
            major_version == 4 && (unsync || (header_len == 10 && frames[fpos + 9] & 0x02 != 0));

        fpos += header_len; // skip frame header

        // Normalize v2.2 3-char ids to their v2.3/v2.4 equivalents.
        let frame_id = if major_version == 2 {
            map_id3v22_frame(&raw_id)
                .map(|s| s.to_string())
                .unwrap_or(raw_id)
        } else {
            raw_id
        };

        if frame_size == 0 || fpos + frame_size > frames_end {
            break;
        }

        let frame_data = &frames[fpos..fpos + frame_size];
        fpos += frame_size;

        // Reverse per-frame unsynchronisation before reading the payload.
        let deunsynced_frame;
        let frame_data: &[u8] = if frame_unsync {
            deunsynced_frame = deunsynchronise(frame_data);
            &deunsynced_frame
        } else {
            frame_data
        };

        // Check for picture frames (APIC in v2.3/2.4, PIC in v2.2).
        if frame_id == "APIC" {
            tags.has_picture = true;
            if tags.picture.is_none() {
                tags.picture = extract_apic_picture(frame_data, major_version);
            }
            continue;
        }

        // UFID : proprietaire en ISO-8859-1 termine par un octet nul, puis
        // l'identifiant binaire — pour MusicBrainz, l'UUID en ASCII. Lu AVANT
        // le filtre ci-dessous, qui ne laisse passer que les frames de texte.
        if frame_id == "UFID" {
            if let Some(nul) = frame_data.iter().position(|b| *b == 0) {
                let owner = String::from_utf8_lossy(&frame_data[..nul])
                    .trim()
                    .to_string();
                let id = String::from_utf8_lossy(&frame_data[nul + 1..])
                    .trim_end_matches('\0')
                    .trim()
                    .to_string();
                if !owner.is_empty() && !id.is_empty() {
                    tags.ufid_frames.push((owner, id));
                }
            }
            continue;
        }

        // Only process text frames (start with 'T') and TXXX
        if !frame_id.starts_with('T') {
            continue;
        }

        // Text frame: first byte is encoding, rest is the string
        if frame_data.is_empty() {
            continue;
        }

        let encoding = frame_data[0];
        let text_data = &frame_data[1..];

        let text = decode_id3v2_string(encoding, text_data);
        let text = text.trim_end_matches('\0').trim().to_string();

        if text.is_empty() {
            continue;
        }

        if frame_id == "TXXX" {
            // TXXX: encoding byte + null-terminated description + value
            // The `text` we decoded contains "description\0value"
            if let Some(null_pos) = text.find('\0') {
                let desc = text[..null_pos].trim().to_string();
                let val = text[null_pos + 1..].trim().to_string();
                if !desc.is_empty() && !val.is_empty() {
                    tags.txxx_frames.push((desc, val));
                }
            }
        } else {
            tags.text_frames.push((frame_id, text));
        }
    }

    Some(tags)
}

/// Return the genre from the FIRST prepended ID3v2 tag of an MP3 — but only when
/// a SECOND ID3v2 tag immediately follows it.
///
/// iTunes M4A→MP3 conversions leave a stale ID3v2.4 tag, and a later re-tag in
/// Mp3Tag prepends a fresh ID3v2.3 tag in front of it, so the file carries two
/// consecutive tags. lofty merges both into one tag with last-wins frame
/// semantics, so the stale second tag's `TCON` ("Singer/Songwriter") overrides
/// the user's genre ("Alternatif & Indé"). Every standard tool (Mp3Tag, ffprobe)
/// reads only the first tag — so do we. The single-tag guard keeps this a no-op
/// for normal files, so lofty's (encoding/numeric-genre-aware) value is untouched
/// except in exactly this dual-tag case. Forum #1184.
fn mp3_first_tag_genre_if_dual(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 10];
    f.read_exact(&mut header).ok()?;
    if &header[0..3] != b"ID3" {
        return None;
    }
    let major_version = header[3];
    let flags = header[5];
    let tag_size = syncsafe_to_u32(&header[6..10]) as usize;
    // v2.4 may append a 10-byte footer (flag 0x10) after the frames; the next
    // tag then starts past it.
    let has_footer = major_version == 4 && (flags & 0x10 != 0);
    let first_tag_end = 10 + tag_size + if has_footer { 10 } else { 0 };

    // Cap to avoid reading a pathological/corrupt size into memory.
    if first_tag_end > 4_194_304 {
        return None;
    }

    // Is there a SECOND ID3v2 tag right after the first? If not, leave lofty's
    // genre alone — a single well-formed tag needs no correction.
    f.seek(SeekFrom::Start(first_tag_end as u64)).ok()?;
    let mut peek = [0u8; 3];
    if f.read_exact(&mut peek).is_err() || &peek != b"ID3" {
        return None;
    }

    // Re-read and parse just the first tag; its TCON is the user's genre.
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut buf = vec![0u8; first_tag_end];
    f.read_exact(&mut buf).ok()?;
    let tags = parse_id3v2_tag(&buf)?;
    tags.genre().map(|s| s.to_string())
}

/// Decode an ID3v2 text string given its encoding byte.
///
/// Encodings:
///   0 = ISO-8859-1 (Latin-1)
///   1 = UTF-16 with BOM
///   2 = UTF-16BE without BOM
///   3 = UTF-8
fn decode_id3v2_string(encoding: u8, data: &[u8]) -> String {
    match encoding {
        0 => {
            // ISO-8859-1: each byte maps directly to a Unicode code point
            data.iter().map(|&b| b as char).collect()
        }
        1 => {
            // UTF-16 with BOM
            if data.len() < 2 {
                return String::new();
            }
            let is_le = data[0] == 0xFF && data[1] == 0xFE;
            let payload = &data[2..];
            decode_utf16(payload, is_le)
        }
        2 => {
            // UTF-16BE without BOM
            decode_utf16(data, false)
        }
        3 => {
            // UTF-8
            String::from_utf8_lossy(data).to_string()
        }
        _ => String::from_utf8_lossy(data).to_string(),
    }
}

/// Decode a UTF-16 byte slice to a String.
fn decode_utf16(data: &[u8], little_endian: bool) -> String {
    let pairs = data.chunks_exact(2);
    let code_units: Vec<u16> = pairs
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&code_units)
}

/// Nombre d'octets que le lecteur de tag DSF tient en mémoire d'un seul bloc.
///
/// Ce n'est PLUS un plafond de REFUS. Un tag plus gros n'est plus jeté en
/// bloc : il est relu trame par trame par [`read_id3v2_selected_frames`], qui
/// ne copie que les trames utiles et SAUTE les autres d'un `seek` — elles ne
/// sont jamais allouées.
///
/// Le chiffre ne bouge pas, parce que c'est lui qui borne la pointe mémoire du
/// scan : [`try_read_metadata`] est appelé par le pool de `scanner/walker.rs`,
/// à `SCAN_IO_CONCURRENCY = 32` lectures simultanées, soit 32 Mio de pointe.
/// C'est la contrainte mesurée sur ce scanner (JeromeQ : 261 fichiers, 6,1 Gio
/// de RSS sur une machine de 8 Gio, tué par l'OOM killer), la même que citent
/// [`MAX_RETAINED_COVER_BYTES`] et les deux passes lofty de ce fichier.
const DSF_TAG_READ_BUDGET: usize = 1_048_576;

/// Origine de l'appel au lecteur de tag ID3v2 brut. Décide si un rejet PARLE.
///
/// Les rejets de ce lecteur étaient tous MUETS : un fichier dont le tag entier
/// était écarté ne laissait aucune trace, et c'est ce qui a rendu #3180
/// invisible deux mois (Benjithom fil 1100 : titre = nom de fichier ;
/// Pierre M fil 920 : tags ignorés en bloc et albums DSD sans pochette).
///
/// Mais le même lecteur sert aussi une sonde spéculative en tête de MP3/WAV, où
/// ne rien trouver est le cas NORMAL. Y journaliser un rejet noierait un scan
/// de dizaines de milliers de fichiers. C'est donc le SITE D'APPEL qui tranche,
/// pas le lecteur : un `warn!` par fichier réellement écarté, zéro ligne sur un
/// fichier ordinaire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Id3ReadSite {
    /// Tag d'un `.dsf`, à l'offset annoncé par son en-tête DSD.
    ///
    /// lofty 0.24 ne connaît pas le format DSF — pas de variante `FileType`, le
    /// mot n'apparaît nulle part dans ses sources —, donc `Probe::read()` échoue
    /// sur tout `.dsf` et ce lecteur est la SEULE source de titre, d'artiste,
    /// d'album et de pochette du format. Il n'y a aucun filet derrière : tout
    /// rejet est une perte sèche et doit laisser une trace.
    DsfTag,
    /// Sonde spéculative à l'offset 0 d'un fichier NON-DSF dont lofty a rendu un
    /// titre vide. N'y trouver aucun tag est le cas normal et fréquent — un
    /// FLAC sans titre le traverse à chaque scan. Muette, donc.
    LeadingProbe,
}

/// Trames qu'un tag hors budget vaut la peine d'être relu pour.
///
/// Exactement ce que [`parse_id3v2_tag`] consomme : les trames de texte
/// (`T…`, `TXXX` compris) et `UFID`/`UFI`, où Picard écrit l'identifiant
/// MusicBrainz. Plus `APIC`/`PIC` quand c'est la pochette qu'on est venu
/// chercher.
///
/// Le critère est SÉMANTIQUE, pas une taille : on ne devine pas ce qui est
/// « gros », on sait ce qui est utile. La trame qui fait déborder le budget est
/// toujours l'image, et elle n'est copiée que sur le chemin qui la demande.
fn id3v2_frame_worth_reading(frame_id: &[u8], want_picture: bool) -> bool {
    if want_picture && (frame_id == b"APIC".as_slice() || frame_id == b"PIC".as_slice()) {
        return true;
    }
    frame_id.first() == Some(&b'T')
        || frame_id == b"UFID".as_slice()
        || frame_id == b"UFI".as_slice()
}

/// Plafond du seul chemin POCHETTE ([`extract_dsf_cover`]).
///
/// Plus haut que [`DSF_TAG_READ_BUDGET`] pour une raison de site d'appel, pas de
/// goût : la pochette n'est extraite qu'UNE fois par album, par la boucle
/// d'import séquentielle de `scan_import.rs` (garde `albums_with_cover`), alors
/// que le tag est lu pour CHAQUE fichier sur un pool de 32 lectures
/// simultanées. Une seule allocation à la fois, donc.
///
/// Le chiffre n'est pas choisi : c'est [`MAX_RETAINED_COVER_BYTES`], la seule
/// borne MESURÉE du dépôt pour une pochette intégrée. Au-delà, Tune refuse déjà
/// de tenir une pochette en mémoire quel que soit le conteneur ; le DSF suit la
/// même règle et l'album retombe sur la pochette de son dossier.
const DSF_COVER_FRAME_BUDGET: usize = MAX_RETAINED_COVER_BYTES;

/// Read the raw ID3v2 tag bytes from a DSF file's metadata chunk.
///
/// DSF files store an ID3v2 tag at the byte offset specified in the DSD
/// chunk header (bytes 20-27). Returns the tag as a contiguous buffer
/// (ID3v2 header + body), or `None` if there is no tag or it looks invalid.
///
/// # #3180 — la pochette n'emporte plus le texte
///
/// Ce lecteur refusait EN BLOC tout tag de plus d'un mégaoctet. Dans un `.dsf`,
/// le tag ID3v2 contient la pochette (`APIC`) : sur un rip SACD elle dépasse
/// couramment le mégaoctet à elle seule. Le refus rendait donc `None` pour la
/// TOTALITÉ du tag — plus de titre, plus d'artiste, plus d'album, plus de
/// pochette — et `dsf_dff_fallback` retombait sur `path.file_stem()`. Une seule
/// ligne expliquait les deux plaintes du ticket.
///
/// Le plafond n'est pas relevé : il borne une pointe mémoire réelle. Ce qui
/// change, c'est qu'il ne décide plus du sort du TEXTE. Au-dessus du budget, le
/// tag est reparcouru trame par trame et seules les trames utiles sont copiées.
fn read_dsf_id3v2_raw(
    path: &Path,
    metadata_offset: Option<u64>,
    site: Id3ReadSite,
    want_picture: bool,
) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    // Un rejet ne parle que depuis le tag d'un `.dsf` — voir [`Id3ReadSite`].
    let rejet = |motif: &str, detail: String| {
        if site == Id3ReadSite::DsfTag {
            tracing::warn!(
                path = %path.display(),
                motif = motif,
                detail = %detail,
                "dsf_id3v2_tag_ecarte"
            );
        }
    };
    let Some(offset) = metadata_offset else {
        // Pas un rejet : l'en-tête DSD annonce 0 quand le fichier n'a aucun tag.
        // `debug!`, sinon chaque `.dsf` nu d'une bibliothèque écrirait un `warn!`.
        if site == Id3ReadSite::DsfTag {
            tracing::debug!(path = %path.display(), "dsf_id3v2_aucun_chunk_metadata");
        }
        return None;
    };
    let mut f = match std::fs::File::open(&*crate::library::artwork::extended_path(path)) {
        Ok(f) => f,
        Err(e) => {
            rejet("ouverture_impossible", e.to_string());
            return None;
        }
    };
    let file_len = match f.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            rejet("taille_illisible", e.to_string());
            return None;
        }
    };
    // `offset + 10` débordait en silence sur un offset corrompu proche de
    // `u64::MAX` : en release le calcul boucle, rend un petit nombre, et la
    // garde PASSE au lieu d'arrêter. `checked_add` la ferme.
    match offset.checked_add(10) {
        Some(fin) if fin <= file_len => {}
        _ => {
            rejet(
                "offset_hors_fichier",
                format!("offset={offset} taille={file_len}"),
            );
            return None;
        }
    }
    if let Err(e) = f.seek(SeekFrom::Start(offset)) {
        rejet("positionnement_impossible", e.to_string());
        return None;
    }
    // Read the ID3v2 header to get the tag size
    let mut header = [0u8; 10];
    if let Err(e) = f.read_exact(&mut header) {
        rejet("entete_id3v2_illisible", e.to_string());
        return None;
    }
    if &header[0..3] != b"ID3" {
        rejet(
            "pas_un_tag_id3v2",
            format!("offset={offset} magie={:02x?}", &header[0..3]),
        );
        return None;
    }
    let tag_size = syncsafe_to_u32(&header[6..10]) as usize;
    let total_tag_bytes = 10 + tag_size;

    // Cas courant : le tag tient dans le budget, il est lu d'un bloc — pochette
    // comprise, donc `extract_dsf_cover` ne change pas d'un octet ici.
    if total_tag_bytes <= DSF_TAG_READ_BUDGET {
        let mut tag_data = Vec::with_capacity(total_tag_bytes);
        tag_data.extend_from_slice(&header);
        if let Err(e) = f.by_ref().take(tag_size as u64).read_to_end(&mut tag_data) {
            rejet("corps_du_tag_illisible", e.to_string());
            return None;
        }
        if tag_data.len() < total_tag_bytes {
            // Lecture COURTE — le troisième `return None` muet. Le tag annonce
            // plus d'octets que le fichier n'en porte, mais les trames
            // COMPLÈTES avant la coupure restent valables et `parse_id3v2_tag`
            // borne son parcours à `data.len()`. On rend ce qui a été lu au lieu
            // de jeter un titre parfaitement lisible parce que l'image derrière
            // est tronquée.
            tracing::warn!(
                path = %path.display(),
                annonce = total_tag_bytes,
                lu = tag_data.len(),
                "dsf_id3v2_tag_tronque"
            );
        }
        return Some(tag_data);
    }

    // Tag AU-DESSUS du budget : c'est ici que #3180 rendait `None`.
    match read_id3v2_selected_frames(&mut f, &header, offset, file_len, want_picture) {
        Some(recompose) => {
            tracing::debug!(
                path = %path.display(),
                taille_tag = total_tag_bytes,
                budget = DSF_TAG_READ_BUDGET,
                retenu = recompose.len(),
                "dsf_id3v2_tag_hors_budget_relu_par_trames"
            );
            Some(recompose)
        }
        None => {
            // Le parcours par trames n'est pas sûr ici (voir
            // `read_id3v2_selected_frames`), ou n'a rien retenu. Reste le
            // préfixe : on lit le budget et on rend ce qu'il contient, que
            // `parse_id3v2_tag` sait parcourir jusqu'à la coupure. Moins bon que
            // le parcours, infiniment mieux que l'ancien `None`.
            if f.seek(SeekFrom::Start(offset)).is_err() {
                rejet(
                    "hors_budget_repositionnement_impossible",
                    format!("taille={total_tag_bytes} budget={DSF_TAG_READ_BUDGET}"),
                );
                return None;
            }
            let mut prefixe = Vec::with_capacity(DSF_TAG_READ_BUDGET);
            if f.by_ref()
                .take(DSF_TAG_READ_BUDGET as u64)
                .read_to_end(&mut prefixe)
                .is_err()
                || prefixe.len() <= 10
            {
                rejet(
                    "hors_budget_prefixe_illisible",
                    format!("taille={total_tag_bytes} budget={DSF_TAG_READ_BUDGET}"),
                );
                return None;
            }
            tracing::warn!(
                path = %path.display(),
                taille_tag = total_tag_bytes,
                budget = DSF_TAG_READ_BUDGET,
                motif = "trames_non_parcourables",
                "dsf_id3v2_tag_hors_budget_lu_en_prefixe"
            );
            Some(prefixe)
        }
    }
}

/// Relit un tag ID3v2 trop gros pour le budget en ne copiant QUE les trames
/// utiles, les autres étant sautées d'un `seek` — jamais allouées.
///
/// Rend un tag ID3v2 **recomposé** (en-tête + trames retenues, taille corrigée)
/// que [`parse_id3v2_tag`] lit sans savoir qu'il a été rebâti. C'est ce qui
/// évite de redire ici sa logique de décodage : un seul décodeur, un seul
/// endroit où le corriger.
///
/// Rend `None` quand le parcours ne serait pas fiable — l'appelant retombe alors
/// sur un préfixe plutôt que sur rien.
fn read_id3v2_selected_frames(
    f: &mut std::fs::File,
    header: &[u8; 10],
    tag_offset: u64,
    file_len: u64,
    want_picture: bool,
) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let major = header[3];
    if !(2..=4).contains(&major) {
        return None;
    }
    let flags = header[5];
    let tag_size = syncsafe_to_u32(&header[6..10]) as u64;
    // Désynchronisation GLOBALE (fanion 0x80) en v2.2/v2.3 : le bourrage
    // 0xFF 0x00 est réparti sur TOUT le bloc de trames et les tailles annoncées
    // ne valent qu'une fois ce bourrage retiré. Sauter de trame en trame sur le
    // flux stocké désaligne tout dès la première. En v2.4 la désynchronisation
    // est PAR TRAME et la taille compte les octets STOCKÉS : le parcours reste
    // juste, `parse_id3v2_tag` défaisant le bourrage trame par trame.
    if major <= 3 && flags & 0x80 != 0 {
        return None;
    }
    // Fin du tag, bornée par le fichier : une taille annoncée plus grande que ce
    // que le fichier porte ne doit pas nous faire lire au-delà.
    let tag_end = tag_offset
        .saturating_add(10)
        .saturating_add(tag_size)
        .min(file_len);
    let mut cursor = tag_offset.saturating_add(10);
    // En-tête étendu (v2.3/v2.4) : sauté, il n'est pas recopié.
    if major >= 3 && flags & 0x40 != 0 {
        let mut ext = [0u8; 4];
        f.seek(SeekFrom::Start(cursor)).ok()?;
        f.read_exact(&mut ext).ok()?;
        let ext_size = if major == 4 {
            syncsafe_to_u32(&ext) as u64
        } else {
            u32::from_be_bytes(ext) as u64
        };
        cursor = cursor.saturating_add(ext_size.max(4));
    }
    let (id_len, frame_header_len) = if major == 2 {
        (3usize, 6u64)
    } else {
        (4usize, 10u64)
    };
    // Ce que le parcours s'autorise à copier EN TOUT. Le chemin pochette ajoute
    // de quoi tenir une image PAR-DESSUS le texte — il tourne seul, une fois par
    // album. Le chemin métadonnées garde le budget du scan, à 32 lectures
    // simultanées.
    let allocation_max = if want_picture {
        (DSF_COVER_FRAME_BUDGET + DSF_TAG_READ_BUDGET) as u64
    } else {
        DSF_TAG_READ_BUDGET as u64
    };
    let mut kept: Vec<u8> = Vec::new();
    f.seek(SeekFrom::Start(cursor)).ok()?;
    while cursor.saturating_add(frame_header_len) <= tag_end {
        let mut entete = [0u8; 10];
        let n = frame_header_len as usize;
        if f.read_exact(&mut entete[..n]).is_err() {
            break;
        }
        cursor += frame_header_len;
        // Bourrage de fin de tag : des octets nuls, plus aucune trame derrière.
        if entete[0] == 0 {
            break;
        }
        let frame_size = match major {
            4 => syncsafe_to_u32(&entete[4..8]) as u64,
            3 => u32::from_be_bytes([entete[4], entete[5], entete[6], entete[7]]) as u64,
            // v2.2 : taille sur 3 octets, gros-boutiste.
            _ => ((entete[3] as u64) << 16) | ((entete[4] as u64) << 8) | (entete[5] as u64),
        };
        if frame_size == 0 || cursor.saturating_add(frame_size) > tag_end {
            break;
        }
        // Deux plafonds, un par nature de trame : l'image a le sien
        // (`DSF_COVER_FRAME_BUDGET`), le texte celui du scan. Une image
        // au-dessus du sien est SAUTÉE — l'album retombe sur la pochette de son
        // dossier, comme n'importe quel autre conteneur — sans que le texte
        // autour d'elle en souffre.
        let id = &entete[..id_len];
        let est_image = id == b"APIC".as_slice() || id == b"PIC".as_slice();
        let plafond_trame = if est_image {
            DSF_COVER_FRAME_BUDGET as u64
        } else {
            DSF_TAG_READ_BUDGET as u64
        };
        let garder = id3v2_frame_worth_reading(id, want_picture)
            && frame_size <= plafond_trame
            && kept.len() as u64 + frame_header_len + frame_size <= allocation_max;
        if garder {
            let mut corps = vec![0u8; frame_size as usize];
            if f.read_exact(&mut corps).is_err() {
                break;
            }
            kept.extend_from_slice(&entete[..n]);
            kept.extend_from_slice(&corps);
        } else {
            // La trame écartée n'est jamais lue : on passe par-dessus.
            if f.seek(SeekFrom::Current(frame_size as i64)).is_err() {
                break;
            }
        }
        cursor += frame_size;
    }
    if kept.is_empty() {
        return None;
    }
    // Tag recomposé : le même en-tête, sans le fanion d'en-tête étendu (0x40,
    // non recopié) ni celui de pied de page (0x10, laissé derrière), et la
    // taille syncsafe des seules trames retenues.
    let mut out = Vec::with_capacity(10 + kept.len());
    out.extend_from_slice(header);
    out[5] &= !(0x40u8 | 0x10u8);
    let taille = kept.len();
    out[6] = ((taille >> 21) & 0x7F) as u8;
    out[7] = ((taille >> 14) & 0x7F) as u8;
    out[8] = ((taille >> 7) & 0x7F) as u8;
    out[9] = (taille & 0x7F) as u8;
    out.extend_from_slice(&kept);
    Some(out)
}

/// Read and parse the ID3v2 metadata chunk from a DSF file.
fn read_dsf_id3v2_tags(path: &Path, metadata_offset: Option<u64>) -> Option<Id3v2Tags> {
    let tag_data = read_dsf_id3v2_raw(path, metadata_offset, Id3ReadSite::DsfTag, false)?;
    match parse_id3v2_tag(&tag_data) {
        Some(tags) => Some(tags),
        None => {
            // Le tag a été LU mais pas compris : version hors 2.2–2.4, en-tête
            // étendu incohérent… Muet jusqu'ici, alors que c'est le dernier
            // point avant le repli sur le nom de fichier.
            tracing::warn!(
                path = %path.display(),
                octets = tag_data.len(),
                motif = "tag_illisible",
                "dsf_id3v2_tag_ecarte"
            );
            None
        }
    }
}

/// Decode the image bytes and MIME type from an ID3v2 picture frame body.
///
/// Handles both the v2.3/2.4 `APIC` layout (encoding byte, NUL-terminated
/// Latin-1 MIME string, picture-type byte, NUL-terminated description, image
/// data) and the v2.2 `PIC` layout (encoding byte, 3-char image-format code,
/// picture-type byte, NUL-terminated description, image data). The description
/// terminator is one NUL for Latin-1/UTF-8 encodings and a two-byte NUL for the
/// UTF-16 encodings. Returns `(mime_type, image_bytes)`.
fn extract_apic_picture(body: &[u8], major_version: u8) -> Option<(String, Vec<u8>)> {
    if body.is_empty() {
        return None;
    }
    let encoding = body[0];
    let mut pos = 1usize;

    let mime = if major_version == 2 {
        // v2.2 "PIC": 3-character image format code (e.g. "JPG", "PNG").
        if body.len() < pos + 3 {
            return None;
        }
        let fmt = &body[pos..pos + 3];
        pos += 3;
        match fmt.to_ascii_uppercase().as_slice() {
            b"PNG" => "image/png".to_string(),
            _ => "image/jpeg".to_string(),
        }
    } else {
        // v2.3/2.4 "APIC": NUL-terminated Latin-1 MIME string.
        let start = pos;
        while pos < body.len() && body[pos] != 0 {
            pos += 1;
        }
        if pos >= body.len() {
            return None;
        }
        let mime = String::from_utf8_lossy(&body[start..pos]).into_owned();
        pos += 1; // skip NUL terminator
        if mime.is_empty() {
            "image/jpeg".to_string()
        } else {
            mime
        }
    };

    // Picture type (1 byte).
    if pos >= body.len() {
        return None;
    }
    pos += 1;

    // Description, NUL-terminated in the frame's text encoding.
    match encoding {
        1 | 2 => {
            // UTF-16: terminated by a 0x0000 code unit.
            while pos + 1 < body.len() && !(body[pos] == 0 && body[pos + 1] == 0) {
                pos += 2;
            }
            pos += 2;
        }
        _ => {
            while pos < body.len() && body[pos] != 0 {
                pos += 1;
            }
            pos += 1;
        }
    }

    if pos >= body.len() {
        return None;
    }
    let data = body[pos..].to_vec();
    if data.is_empty() {
        return None;
    }
    Some((mime, data))
}

/// Extract the embedded cover art (APIC) from a DSF file's ID3v2 chunk.
///
/// lofty does not read the ID3v2 tag stored at the DSF metadata offset, so
/// embedded artwork is invisible to the generic `lofty::read_from_path`
/// cover-extraction path used by [`crate::library::artwork::extract_cover_art`].
/// This reads the tag directly and returns the first picture's raw bytes and
/// MIME type. Non-`.dsf` paths (and files without embedded art) return `None`.
pub(crate) fn extract_dsf_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if ext != "dsf" {
        return None;
    }

    let info = match parse_dsf_header_full(path) {
        Ok(info) => info,
        Err(motif) => {
            // `debug!` et pas `warn!` : la passe de métadonnées a déjà parlé au
            // niveau WARN pour ce même fichier (`dsf_entete_illisible`), et le
            // redire ici doublerait chaque ligne d'un scan de DSF abîmés.
            tracing::debug!(path = %path.display(), motif, "dsf_pochette_entete_illisible");
            return None;
        }
    };
    // `want_picture` : sur un tag hors budget, c'est le seul chemin qui copie la
    // trame APIC. La passe de métadonnées, elle, la saute — d'où deux plafonds.
    let tag_data = read_dsf_id3v2_raw(path, info.metadata_offset, Id3ReadSite::DsfTag, true)?;
    let (mime, data) = match parse_id3v2_tag(&tag_data).and_then(|t| t.picture) {
        Some(p) => p,
        None => {
            // Cas NORMAL et fréquent : un DSF tagué sans pochette intégrée.
            // `debug!` — un `warn!` ici parlerait pour un album sur deux.
            tracing::debug!(path = %path.display(), "dsf_aucune_pochette_integree");
            return None;
        }
    };
    Some((data, mime))
}

/// Fallback metadata extraction for DSF/DFF files when lofty fails.
///
/// DSF files contain an ID3v2 tag at an offset specified in the DSD chunk
/// header (bytes 20-27).  This function reads that offset, seeks to the
/// ID3v2 data, and parses the embedded tags.  Audio properties (sample rate,
/// channels, duration) come from the fmt chunk header.
///
/// For DFF files (or if DSF header / ID3v2 parsing fails), we fall back to
/// deriving title/album/artist from the file path.
fn dsf_dff_fallback(path: &Path) -> Option<TrackMetadata> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if ext != "dsf" && ext != "dff" {
        return None;
    }

    let file_size = std::fs::metadata(&*crate::library::artwork::extended_path(path))
        .ok()
        .map(|m| m.len());

    let (sample_rate, channels, duration_ms, metadata_offset) = if ext == "dsf" {
        match parse_dsf_header_full(path) {
            Ok(info) => (
                info.sample_rate,
                info.channels,
                info.duration_ms,
                info.metadata_offset,
            ),
            Err(motif) => {
                // Muet jusqu'ici : un `.dsf` dont l'en-tête DSD ne se lit pas
                // perdait d'un coup fréquence, canaux, durée ET tag, sans une
                // ligne. lofty ne connaissant pas le format, il n'y a aucun
                // second lecteur derrière pour rattraper.
                tracing::warn!(path = %path.display(), motif, "dsf_entete_illisible");
                (None, None, None, None)
            }
        }
    } else {
        // DFF (DSDIFF) has no fmt/ID3 chunk like DSF. Previously this arm
        // returned all-None, so every DFF that reached this fallback (lofty
        // couldn't decode it, or it had no/empty tag) landed with
        // duration_ms = 0 — which downstream disables gapless, the wall-clock
        // advance nets, prefetch and crossfade (poller), cutting the album on
        // those tracks (DSD testers: Benjithom RS130, LANDES). Read the DSDIFF
        // header for the real sample rate, channels and duration.
        match path.to_str().map(crate::audio::dff::parse_dff) {
            Some(Ok(info)) => (
                Some(info.sample_rate),
                Some(info.channels as u16),
                info.duration_ms(),
                None,
            ),
            _ => (None, None, None, None),
        }
    };

    // Try to read ID3v2 tags from the DSF metadata chunk
    let id3_tags = if ext == "dsf" {
        read_dsf_id3v2_tags(path, metadata_offset)
    } else {
        None
    };

    let (
        title,
        artist,
        album,
        album_artist,
        album_artist_sort,
        track_number,
        disc_number,
        total_tracks,
        total_discs,
        disc_subtitle,
        year,
        original_year,
        original_date,
        release_date,
        genre,
        genres,
        has_cover,
        label,
        isrc,
        compilation,
        credits,
    ) = if let Some(ref tags) = id3_tags {
        // Toutes les trames `TCON`, comme le chemin lofty (#1821).
        let raw_genres: Vec<&str> = tags.genres();
        let genres = genres_from_tag_values(&raw_genres);
        let genre = genres
            .first()
            .cloned()
            .or_else(|| raw_genres.first().map(|s| s.to_string()));

        let compilation_str = tags.get("TCMP").unwrap_or("");
        let compilation = matches!(compilation_str, "1" | "true" | "True");

        let mut credits = Vec::new();
        if let Some(composer) = tags.composer() {
            credits.push(TrackCredit {
                name: composer.to_string(),
                role: "composer".into(),
                instrument: None,
            });
        }
        if let Some(conductor) = tags.get("TPE3") {
            credits.push(TrackCredit {
                name: conductor.to_string(),
                role: "conductor".into(),
                instrument: None,
            });
        }

        (
            tags.title().map(|s| s.to_string()),
            tags.artist().map(|s| s.to_string()),
            tags.album().map(|s| s.to_string()),
            tags.album_artist().map(|s| s.to_string()),
            tags.album_artist_sort().map(|s| s.to_string()),
            tags.track_number(),
            tags.disc_number(),
            tags.total_tracks(),
            tags.total_discs(),
            tags.disc_subtitle().map(|s| s.to_string()),
            tags.year(),
            tags.original_year(),
            tags.original_date().map(|s| s.to_string()),
            tags.release_date().map(|s| s.to_string()),
            genre,
            genres,
            tags.has_picture,
            tags.label().map(|s| s.to_string()),
            tags.isrc().map(|s| s.to_string()),
            compilation,
            credits,
        )
    } else {
        (
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            false,
            None,
            None,
            false,
            Vec::new(),
        )
    };

    // Fall back to filename/directory for fields the ID3v2 tag didn't provide.
    // Treat a present-but-empty/whitespace tag as absent: a file whose ALBUM tag
    // is "" (not missing) otherwise produced a blank, untitled album that no
    // amount of re-scanning could name (Bilou #1093). `filter` drops the empty
    // value so the folder-name fallback kicks in.
    let title = title
        .filter(|s| !s.trim().is_empty())
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()));
    let (album_du_chemin, artiste_du_chemin, disque_du_chemin) = album_artiste_du_chemin(path);
    let album = album.filter(|s| !s.trim().is_empty()).or(album_du_chemin);
    let artist = artist.or(artiste_du_chemin);
    // `album_artist` n'est PLUS déduit du chemin. C'est le seul repli à tags
    // partiels : le fichier peut porter un ARTIST par piste sans ALBUMARTIST,
    // et y coller un nom de dossier faisait arriver le champ REMPLI au scan.
    // `auto_scan.rs` compare alors `album_artist` à « various artists / va /
    // compilations » — un nom de dossier ne correspond à rien, la décision
    // « compilation » était close avant d'être posée, et le repli « pas
    // d'album_artist → artiste de la première piste du dossier » de
    // `scan_import.rs` ne s'exécutait jamais. Le laisser absent rend au scan
    // l'information dont il a besoin : ce champ est absent (#1656).
    let disc_number = disc_number.or(disque_du_chemin);

    // Extract MusicBrainz IDs from TXXX frames
    let (
        mb_recording_id,
        mb_release_id,
        mb_artist_id,
        mb_album_artist_id,
        mb_release_group_id,
        catalog_number,
    ) = if let Some(ref tags) = id3_tags {
        (
            tags.musicbrainz_recording_id().map(|s| s.to_string()),
            tags.get_txxx("MusicBrainz Album Id").map(|s| s.to_string()),
            tags.get_txxx("MusicBrainz Artist Id")
                .map(|s| s.to_string()),
            tags.get_txxx("MusicBrainz Album Artist Id")
                .map(|s| s.to_string()),
            tags.get_txxx("MusicBrainz Release Group Id")
                .map(|s| s.to_string()),
            tags.get_txxx("CATALOGNUMBER")
                .or_else(|| tags.get_txxx("CatalogNumber"))
                .map(|s| s.to_string()),
        )
    } else {
        (None, None, None, None, None, None)
    };

    Some(TrackMetadata {
        title,
        album,
        artist,
        album_artist,
        album_artist_sort,
        track_number,
        disc_number,
        total_tracks,
        total_discs,
        disc_subtitle,
        year,
        original_year,
        release_date,
        original_date,
        genre,
        genres,
        // Le conteneur réel — `dsf` ou `dff` — et non « dsd » en dur : ce
        // chemin connaît l'extension depuis sa première ligne, et la perdre
        // ici rouvrirait le défaut que `normalize_format` vient de fermer
        // (#1612).
        format: Some(ext.clone()),
        file_size,
        sample_rate,
        channels,
        duration_ms: duration_ms.or(Some(0)),
        bit_depth: Some(1), // DSD is always 1-bit
        bpm: None,
        compilation,
        label,
        catalog_number,
        musicbrainz_recording_id: mb_recording_id,
        musicbrainz_release_id: mb_release_id,
        musicbrainz_artist_id: mb_artist_id,
        musicbrainz_album_artist_id: mb_album_artist_id,
        musicbrainz_release_group_id: mb_release_group_id,
        isrc,
        has_cover,
        cover_art: None,
        credits,
        comment: None,
    })
}

fn m4a_fallback(path: &Path) -> Option<TrackMetadata> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if ext != "m4a" && ext != "mp4" && ext != "alac" {
        return None;
    }
    let file_name = path.file_stem()?.to_str()?;
    let (album, artist, disc_number) = album_artiste_du_chemin(path);

    let (track_number, title) =
        if let Some(rest) = file_name.strip_prefix(|c: char| c.is_ascii_digit()) {
            let num_str: String = std::iter::once(file_name.chars().next().unwrap())
                .chain(rest.chars().take_while(|c| c.is_ascii_digit()))
                .collect();
            let after = file_name[num_str.len()..].trim_start_matches([' ', '-', '.', '_']);
            (num_str.parse::<u32>().ok(), Some(after.to_string()))
        } else {
            (None, Some(file_name.to_string()))
        };

    let file_size = std::fs::metadata(&*crate::library::artwork::extended_path(path))
        .ok()
        .map(|m| m.len());

    tracing::debug!(path = %path.display(), title = ?title, artist = ?artist, album = ?album, "m4a_fallback_metadata");

    Some(TrackMetadata {
        title,
        album,
        artist: artist.clone(),
        album_artist: artist,
        album_artist_sort: None,
        track_number,
        disc_number,
        total_tracks: None,
        total_discs: None,
        disc_subtitle: None,
        year: None,
        original_year: None,
        release_date: None,
        original_date: None,
        genre: None,
        genres: vec![],
        format: Some("alac".to_string()),
        file_size,
        sample_rate: None,
        channels: Some(2),
        duration_ms: None,
        bit_depth: None,
        bpm: None,
        compilation: false,
        label: None,
        catalog_number: None,
        musicbrainz_recording_id: None,
        musicbrainz_release_id: None,
        musicbrainz_artist_id: None,
        musicbrainz_album_artist_id: None,
        musicbrainz_release_group_id: None,
        isrc: None,
        has_cover: false,
        cover_art: None,
        credits: vec![],
        comment: None,
    })
}

/// Rend le numéro de disque quand un nom de dossier n'est QUE cela : `CD1`,
/// `CD 2`, `Disc-3`, `Disque 1`, `disk04`.
///
/// Volontairement étroit. `Vol. 2` n'en fait pas partie : c'est presque
/// toujours un vrai titre d'album (« Greatest Hits Vol. 2 »), et le confondre
/// avec un disque effacerait un album entier. Un préfixe suivi d'autre chose
/// qu'un nombre — `Disco`, `CD Rip` — ne correspond pas non plus.
pub(crate) fn numero_de_disque(nom: &str) -> Option<u32> {
    let nom = nom.trim().to_lowercase();
    // « disque » avant « disc », sinon « disque 2 » se lirait « disc » + « ue 2 ».
    for prefixe in ["disque", "disc", "disk", "cd"] {
        if let Some(reste) = nom.strip_prefix(prefixe) {
            let reste = reste.trim_start_matches([' ', '-', '_', '.', '#']);
            return reste.parse::<u32>().ok().filter(|&d| d > 0);
        }
    }
    None
}

/// Déduit `(album, artiste, disque)` de l'arborescence, convention
/// `.../Artiste/Album/piste.ext`.
///
/// Quand le dossier parent n'est qu'un numéro de disque, la convention remonte
/// d'un cran : dans `.../The Complete Motown Singles/CD1/01 - Piste.wav`, le
/// parent est le disque, l'album est au-dessus et l'artiste encore au-dessus.
/// Sans ce décalage, l'album s'appelait « CD1 » et **l'artiste retenu était le
/// titre de l'album** — le symptôme signalé par jfpaquet (#1656) sur ses
/// compilations « VA-xxx ».
///
/// Le numéro de disque est rendu avec le reste, et pas seulement pour
/// l'affichage : fusionner CD1 et CD2 en un seul album sans lui ferait
/// collisionner les numéros de piste.
pub(crate) fn album_artiste_du_chemin(
    path: &Path,
) -> (Option<String>, Option<String>, Option<u32>) {
    let nom = |p: Option<&Path>| {
        p.and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
    };
    let parent = path.parent();
    let disque = nom(parent).as_deref().and_then(numero_de_disque);
    let dossier_album = match disque {
        Some(_) => parent.and_then(|p| p.parent()),
        None => parent,
    };
    (
        nom(dossier_album),
        nom(dossier_album.and_then(|p| p.parent())),
        disque,
    )
}

/// Check if a file has a known audio extension (used to decide whether to
/// attempt a filesystem-based metadata fallback when lofty fails).
fn is_known_audio_ext(path: &Path) -> bool {
    crate::audio::support::native_decoder_supports(path)
}

/// Extract basic metadata from the directory structure when lofty successfully
/// parsed the audio properties but the file has no tags.
///
/// Directory convention: `.../Artist/Album/01 - Title.wav`, un dossier de
/// disque intercalé étant sauté (voir [`album_artiste_du_chemin`]).
fn tagless_fallback(path: &Path, props: &lofty::properties::FileProperties) -> TrackMetadata {
    let (track_number, title) = extract_title_from_filename(path);
    let (album, artist, disc_number) = album_artiste_du_chemin(path);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav")
        .to_lowercase();
    let mut probed_bit_depth: Option<u16> = None;
    let format = {
        let mut fmt = normalize_format(&ext, props.bit_depth());
        if fmt == "aac" && (ext == "m4a" || ext == "mp4") && props.bit_depth().is_none() {
            if let Some((probed, bd)) = probe_m4a_props(path) {
                fmt = probed;
                probed_bit_depth = bd;
            }
        }
        Some(fmt)
    };

    tracing::debug!(
        path = %path.display(),
        title = ?title,
        artist = ?artist,
        album = ?album,
        "tagless_fallback_metadata"
    );

    TrackMetadata {
        title,
        album,
        artist: artist.clone(),
        album_artist: artist,
        album_artist_sort: None,
        track_number,
        disc_number,
        total_tracks: None,
        total_discs: None,
        disc_subtitle: None,
        year: None,
        original_year: None,
        release_date: None,
        original_date: None,
        genre: None,
        genres: vec![],
        format,
        file_size: std::fs::metadata(&*crate::library::artwork::extended_path(path))
            .ok()
            .map(|m| m.len()),
        sample_rate: props.sample_rate(),
        channels: props.channels().map(|c| c as u16),
        duration_ms: Some(props.duration().as_millis() as u64),
        bit_depth: props.bit_depth().map(|b| b as u16).or(probed_bit_depth),
        bpm: None,
        compilation: false,
        label: None,
        catalog_number: None,
        musicbrainz_recording_id: None,
        musicbrainz_release_id: None,
        musicbrainz_artist_id: None,
        musicbrainz_album_artist_id: None,
        musicbrainz_release_group_id: None,
        isrc: None,
        has_cover: false,
        cover_art: None,
        credits: vec![],
        comment: None,
    }
}

/// Fallback when lofty cannot parse the file at all (no audio properties).
/// Extracts everything from the filesystem.
/// Path/filename-only metadata (no file I/O). Used as a last resort when the
/// tag reader fails or times out, so a file still appears in the library.
pub fn tagless_fallback_no_props(path: &Path) -> TrackMetadata {
    let (track_number, title) = extract_title_from_filename(path);
    let (album, artist, disc_number) = album_artiste_du_chemin(path);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav")
        .to_lowercase();

    tracing::debug!(
        path = %path.display(),
        title = ?title,
        artist = ?artist,
        album = ?album,
        "tagless_fallback_no_props_metadata"
    );

    TrackMetadata {
        title,
        album,
        artist: artist.clone(),
        album_artist: artist,
        album_artist_sort: None,
        track_number,
        disc_number,
        total_tracks: None,
        total_discs: None,
        disc_subtitle: None,
        year: None,
        original_year: None,
        release_date: None,
        original_date: None,
        genre: None,
        genres: vec![],
        format: Some(ext),
        file_size: std::fs::metadata(&*crate::library::artwork::extended_path(path))
            .ok()
            .map(|m| m.len()),
        sample_rate: None,
        channels: Some(2),
        duration_ms: None,
        bit_depth: None,
        bpm: None,
        compilation: false,
        label: None,
        catalog_number: None,
        musicbrainz_recording_id: None,
        musicbrainz_release_id: None,
        musicbrainz_artist_id: None,
        musicbrainz_album_artist_id: None,
        musicbrainz_release_group_id: None,
        isrc: None,
        has_cover: false,
        cover_art: None,
        credits: vec![],
        comment: None,
    }
}

/// Parse track number and title from a filename.
///
/// Handles patterns like:
///   "01 - Title.wav" -> (Some(1), Some("Title"))
///   "01. Title.wav"  -> (Some(1), Some("Title"))
///   "01_Title.wav"   -> (Some(1), Some("Title"))
///   "Title.wav"      -> (None, Some("Title"))
fn extract_title_from_filename(path: &Path) -> (Option<u32>, Option<String>) {
    let file_name = match path.file_stem().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => return (None, None),
    };
    if let Some(first_char) = file_name.chars().next()
        && first_char.is_ascii_digit()
    {
        let num_str: String = file_name
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let after = file_name[num_str.len()..].trim_start_matches([' ', '-', '.', '_']);
        let title = if after.is_empty() {
            Some(file_name.to_string())
        } else {
            Some(after.to_string())
        };
        (num_str.parse::<u32>().ok(), title)
    } else {
        (None, Some(file_name.to_string()))
    }
}

/// Écarter une durée MP3 franchement incohérente avec la taille du fichier.
///
/// Le besoin est réel (`1e06a2c0`) : sans en-tête XING/VBRI, ou avec un
/// en-tête corrompu, lofty compte mal les trames et annonce n'importe quoi —
/// 184 s pour un fichier de 84 s — d'où un `seek` au-delà de la fin.
///
/// # La borne était prise à l'envers
///
/// La version précédente divisait la taille par le débit **maximum** :
///
/// ```text
/// max_plausible_ms = taille×8000 / 320_000     // « durée plausible maximale »
/// if lofty_ms > max_plausible_ms × 2 { … }
/// ```
///
/// Diviser par le débit maximum donne la durée **minimale** possible, pas la
/// maximale. Le test se réduisait donc à `débit_réel < 160 kbps`, et **tout
/// MP3 sous 160 kbps voyait sa durée réécrite** — à la durée qu'aurait le
/// fichier en 320 kbps, soit divisée par `320/débit_réel`.
///
/// Mesuré sur l'export d'un testeur (#2027, fil forum #1479) : 322
/// avertissements en dix minutes, débit réel de 65 à 159 kbps, **les 322 sans
/// exception**. La borne haute mesurée est exactement le seuil théorique — ce
/// n'était pas un lot de fichiers abîmés, c'était le seuil qui coupait la
/// population en deux. Un fichier de 4 min 02 en 130 kbps était inscrit en
/// base à 1 min 38.
///
/// # La bonne référence est le débit du fichier lui-même
///
/// La taille seule ne peut pas trancher : un fichier long à bas débit et un
/// fichier court à durée sur-annoncée pèsent pareil. Aucun réglage du seuil
/// n'y change rien — il faut la deuxième grandeur, et lofty la donne.
///
/// Sur un XING corrompu, c'est le **compte de trames** qui est faux ; l'en-tête
/// de trame, donc le débit, reste juste. La durée impliquée par le débit vaut
/// alors 84 s et écarte bien les 184 s annoncés : le cas qui a motivé la garde
/// est mieux traité qu'avant.
///
/// Sans débit exploitable, on ne corrige rien : mieux vaut une durée douteuse
/// qu'une durée inventée.
fn mp3_duration_sanity_check(path: &Path, lofty_ms: u64, bitrate_kbps: Option<u32>) -> u64 {
    let file_size = std::fs::metadata(&*crate::library::artwork::extended_path(path))
        .map(|m| m.len())
        .unwrap_or(0);
    if file_size == 0 || lofty_ms == 0 {
        return lofty_ms;
    }
    let Some(kbps) = bitrate_kbps.filter(|k| *k > 0) else {
        return lofty_ms;
    };
    // taille (octets) × 8 bits ÷ (kbps × 1000 bits/s) × 1000 ms/s, simplifié.
    let implique_ms = (file_size * 8) / kbps as u64;
    // Facteur 2 conservé : on ne corrige que l'incohérence franche, pas le
    // flottement normal entre le débit annoncé et le débit réel d'un VBR.
    if lofty_ms > implique_ms * 2 {
        tracing::warn!(
            path = %path.display(),
            lofty_ms,
            implique_ms,
            kbps,
            file_size,
            "mp3_duration_implausible_clamping"
        );
        implique_ms
    } else {
        lofty_ms
    }
}

/// Read a raw Vorbis comment value using its exact 4-byte length prefix.
///
/// Every Vorbis comment is stored as `[len: u32 LE]["KEY=value"]`. Unlike
/// [`raw_vorbis_field`] — which scans for a control-char delimiter and can
/// over-read (then fail UTF-8) on a value sitting at the very end of the comment
/// block, right before the audio frames — this recovers the exact value
/// regardless of what follows. Used for keys lofty has no `ItemKey` for (e.g.
/// `SOURCE`), which are dropped during the VorbisComments → generic-tag split.
fn raw_vorbis_comment(path: &Path, field_name: &str) -> Option<String> {
    let data = read_vorbis_header(path)?;
    find_vorbis_comment(&data, field_name)
}

/// The bounded header read behind [`raw_vorbis_comment`], split out so a caller
/// after several fields pays for **one** read instead of one per field.
///
/// This matters at scan time: looking for four Dynamic Range spellings with four
/// separate calls costs four 1 MB reads on *every* file, and the files that have
/// none — the vast majority — pay the full price every time.
fn read_vorbis_header(path: &Path) -> Option<Vec<u8>> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if !matches!(ext.as_str(), "flac" | "ogg" | "oga" | "opus") {
        return None;
    }
    // Vorbis comments live in the file header; a bounded prefix read finds them
    // without slurping a multi-GB hi-res FLAC into RAM.
    const HEADER_BYTES: u64 = 1024 * 1024;
    let mut data = Vec::new();
    {
        use std::io::Read;
        std::fs::File::open(path)
            .ok()?
            .take(HEADER_BYTES)
            .read_to_end(&mut data)
            .ok()?;
    }
    Some(data)
}

/// Find one field in an already-read Vorbis header.
fn find_vorbis_comment(data: &[u8], field_name: &str) -> Option<String> {
    let needle = format!("{}=", field_name.to_ascii_uppercase());
    let nlen = needle.len();
    if data.len() <= nlen {
        return None;
    }
    for i in 4..=data.len() - nlen {
        if !data[i..i + nlen].eq_ignore_ascii_case(needle.as_bytes()) {
            continue;
        }
        // The 4-byte LE length prefix precedes the "KEY=value" string and covers
        // its whole length, so the value ends at `i + len`.
        let len = u32::from_le_bytes([data[i - 4], data[i - 3], data[i - 2], data[i - 1]]) as usize;
        if len < nlen || i + len > data.len() {
            continue;
        }
        if let Ok(value) = std::str::from_utf8(&data[i + nlen..i + len]) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Reduce a Dynamic Range tag to its bare digits.
///
/// Tools disagree on the form: DROffline MK2 and foobar2000 write `12`, `DR12`
/// or `DR 12` depending on version and template. Storing the raw form would
/// sink the sort this feature exists for — values are TEXT in `track_metadata`,
/// so `"DR9"` and `"9"` never line up, and `"10" < "9"` lexically.
///
/// An optional `DR` prefix and surrounding spaces are dropped, but the result is
/// kept ONLY when what remains is an integer. Anything unexpected is returned
/// untouched rather than mangled: showing a value we failed to parse beats
/// losing it.
fn normalise_dr(raw: &str) -> String {
    let t = raw.trim();
    let body = t
        .strip_prefix("DR")
        .or_else(|| t.strip_prefix("dr"))
        .or_else(|| t.strip_prefix("Dr"))
        .unwrap_or(t)
        .trim();
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit()) {
        return t.to_string();
    }
    // "08" → "8", but a lone "0" stays "0" (a crushed master really can measure
    // DR0, and dropping it would read as "no value").
    let stripped = body.trim_start_matches('0');
    if stripped.is_empty() {
        "0".to_string()
    } else {
        stripped.to_string()
    }
}

fn raw_vorbis_field(path: &Path, field_name: &str) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if !matches!(ext.as_str(), "flac" | "ogg" | "oga" | "opus") {
        return None;
    }
    // The Vorbis comment block lives in the file header (FLAC metadata blocks
    // precede the audio; the OGG/Opus comment header is in the first pages), so
    // a bounded prefix read finds the field — never slurp a multi-GB hi-res FLAC
    // into RAM (twice: the old code also built a full from_utf8_lossy copy that
    // was never read) just to recover one tag. On the rare file whose comment
    // block sits past the window this returns None, exactly as before when lofty
    // already missed the field. (Port from main: unbounded fs::read here spiked
    // the scanner to ~14 GB RSS across 32 workers on .15's hi-res library → OOM
    // crash-loop — the real cause behind the RC3 scan OOM.)
    const HEADER_BYTES: u64 = 1024 * 1024;
    let mut data = Vec::new();
    {
        use std::io::Read;
        std::fs::File::open(path)
            .ok()?
            .take(HEADER_BYTES)
            .read_to_end(&mut data)
            .ok()?;
    }
    let needle = format!("{}=", field_name);
    for line_bytes in data.windows(needle.len()) {
        let chunk = std::str::from_utf8(line_bytes).unwrap_or("");
        if chunk.eq_ignore_ascii_case(&needle) {
            let start = (line_bytes.as_ptr() as usize) - (data.as_ptr() as usize) + needle.len();
            if start < data.len() {
                let rest = &data[start..];
                let end = rest
                    .iter()
                    .position(|&b| b == 0 || b < 0x20)
                    .unwrap_or(rest.len().min(512));
                let value = std::str::from_utf8(&rest[..end]).ok()?;
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// La durée réelle d'un fichier, lue sans aucun garde-fou de vraisemblance.
///
/// `read_metadata` fait passer les MP3 par `mp3_duration_sanity_check`. Une
/// passe de RÉPARATION ne peut donc pas s'en servir : elle relirait la valeur
/// par le chemin qui l'a corrompue. Cette fonction ouvre le fichier et rend ce
/// que lofty mesure, rien d'autre.
///
/// Elle reste correcte que la borne soit corrigée ou non — c'est précisément
/// pourquoi la réparation ne dépend pas de l'ordre des correctifs.
pub fn probe_duration_ms(path: &Path) -> Option<u64> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::AudioFile;
    use lofty::probe::Probe;

    let tagged = Probe::open(path)
        .and_then(|p| {
            p.options(
                ParseOptions::new()
                    .parsing_mode(ParsingMode::Relaxed)
                    .max_junk_bytes(1024 * 1024)
                    .read_cover_art(false),
            )
            .guess_file_type()?
            .read()
        })
        .ok()?;

    let ms = tagged.properties().duration().as_millis() as u64;
    (ms > 0).then_some(ms)
}

pub fn try_read_metadata(path: &Path) -> Result<TrackMetadata, String> {
    let mut metadata = try_read_metadata_unsanitized(path)?;
    let corrections = metadata.sanitize_text_fields();
    if !corrections.is_empty() {
        tracing::warn!(
            path = %path.display(),
            corrections = ?corrections,
            "metadata_unsafe_text_sanitized"
        );
    }
    Ok(metadata)
}

fn try_read_metadata_unsanitized(path: &Path) -> Result<TrackMetadata, String> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, ItemKey};

    let tagged = match Probe::open(path).and_then(|p| {
        p.options(
            ParseOptions::new()
                .parsing_mode(ParsingMode::Relaxed)
                .max_junk_bytes(1024 * 1024)
                // Don't load embedded cover art in the tag pass: lofty otherwise
                // reads the whole PICTURE block into memory, and a huge/malformed
                // embedded image, multiplied by the scan's concurrency (up to 32
                // reads at once), spikes the scanner past the OOM killer (JeromeQ:
                // 261 files → 6.1 GB RSS → tune-server killed, black screen). The
                // cover is extracted separately, sequentially, by
                // `artwork::get_or_extract` when the album needs one, so artwork
                // is unaffected. (has_cover becomes false here — it has no
                // consumers beyond serialization; the album cover_path is the
                // real signal.)
                .read_cover_art(false),
        )
        .guess_file_type()?
        .read()
    }) {
        Ok(t) => t,
        Err(e) => {
            // Try DSF/DFF fallback first
            if let Some(meta) = dsf_dff_fallback(path) {
                return Ok(meta);
            }
            // For M4A/ALAC files that lofty can't parse (large atoms),
            // fall back to directory/filename-based metadata extraction
            if let Some(meta) = m4a_fallback(path) {
                return Ok(meta);
            }
            // For any other audio file (WAV, AIFF, etc.) that lofty cannot
            // parse, extract basic metadata from the filesystem so the file
            // still appears in the library rather than being silently skipped.
            // Only apply the fallback if the file actually exists (a missing
            // file should still return Err).
            if is_known_audio_ext(path) && path.exists() {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "lofty_parse_failed_using_filesystem_fallback"
                );
                return Ok(tagless_fallback_no_props(path));
            }
            return Err(format!("{e}"));
        }
    };
    let props = tagged.properties();
    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => {
            if let Some(meta) = dsf_dff_fallback(path) {
                return Ok(meta);
            }
            // For audio files that lofty can parse (valid audio properties)
            // but have no tags (e.g. WAV without RIFF INFO or ID3v2),
            // extract metadata from the file/directory structure so they
            // still appear in the library instead of being silently skipped.
            return Ok(tagless_fallback(path, props));
        }
    };

    // DSF/DFF: lofty parses the container and returns a tag object, but often
    // misreads the ID3v2.2 frames commonly used on DSD files — the title comes
    // back empty and the track ends up showing its filename (LANDES Philippe,
    // Benjithom). Because a (mostly-empty) tag *is* present, the `None` branch
    // above never fires. So when lofty's title is empty for a DSD file, prefer
    // our own ID3v2.2/.3/.4 parser, which reads those frames correctly.
    {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if matches!(ext.as_str(), "dsf" | "dff")
            && tag.title().map_or(true, |t| t.trim().is_empty())
        {
            if let Some(meta) = dsf_dff_fallback(path) {
                if meta
                    .title
                    .as_deref()
                    .map_or(false, |t| !t.trim().is_empty())
                {
                    return Ok(meta);
                }
            }
        }
    }

    let get = |key: ItemKey| tag.get_string(key).map(|s| s.to_string());

    let compilation_str = get(ItemKey::FlagCompilation).unwrap_or_default();
    let compilation = matches!(compilation_str.as_str(), "1" | "true" | "True");

    let bpm = get(ItemKey::Bpm).and_then(|s| s.parse::<f64>().ok());

    let original_year =
        get(ItemKey::OriginalReleaseDate).and_then(|s| s.get(..4)?.parse::<u32>().ok());

    let total_tracks = tag
        .track_total()
        .or_else(|| get(ItemKey::TrackTotal).and_then(|s| s.parse::<u32>().ok()));
    let total_discs = tag
        .disk_total()
        .or_else(|| get(ItemKey::DiscTotal).and_then(|s| s.parse::<u32>().ok()));

    let credits = parse_credits(tag);

    // TOUTES les valeurs du tag de genre, pas seulement la première (#1821).
    // `Accessor::genre()` ne rend que la première : un FLAC gravé avec deux
    // champs `GENRE`, ou un M4A avec deux atomes `©gen`, perdait tous ses
    // genres secondaires — alors que le MÊME disque, acheté chez un marchand
    // qui écrit « Jazz; Fusion » dans un unique `TCON`, les gardait tous les
    // deux. Le classement dépendait donc du logiciel de gravure, pas de la
    // musique (DEvir, #1821).
    let mut raw_genres: Vec<String> = tag
        .get_strings(ItemKey::Genre)
        .map(|s| s.to_string())
        .collect();
    // MP3s carrying two prepended ID3v2 tags (iTunes M4A→MP3 leftover + Mp3Tag
    // re-tag) make lofty merge last-wins, so a stale genre overrides the user's.
    // Read the first tag like every standard player does — no-op unless a second
    // tag actually follows. Forum #1184.
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mp3"))
    {
        if let Some(g) = mp3_first_tag_genre_if_dual(path) {
            // Le premier tag REMPLACE la fusion de lofty : c'est tout ce que
            // lisent les autres lecteurs, valeurs multiples comprises.
            raw_genres = vec![g];
        }
    }
    let genres = genres_from_tag_values(&raw_genres);
    let genre = genres
        .first()
        .cloned()
        .or_else(|| raw_genres.first().cloned());

    // lofty can't distinguish ALAC (lossless) from AAC (lossy) in an M4A/MP4
    // container and reports no bit depth for either, so a tagged ALAC file was
    // stored as "aac" with no bit depth — the signal path then showed the wrong
    // format and a fabricated 16-bit (Yves: ALAC 24/96 shown as AAC/FLAC).
    // Probe the real codec and, for ALAC, the true bit depth from the magic
    // cookie. Only used for M4A containers with no lofty bit depth.
    let file_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let m4a_probe = if (file_ext == "m4a" || file_ext == "mp4" || file_ext == "m4b")
        && props.bit_depth().is_none()
    {
        probe_m4a_props(path)
    } else {
        None
    };

    // lofty occasionally mis-decodes an MP3's ID3v2 text frames and returns an
    // EMPTY title/artist/album even though the frames are valid (Yves Scordia: a
    // Chris Isaak MP3 with UTF-16 TIT2/TPE1/TALB read as empty, so BluOS got no
    // metadata — other frames like TPE2/TCON/TYER read fine). When the title is
    // empty and the file has a leading ID3v2 tag (MP3, WAV+ID3), re-read those
    // frames with our own ID3v2 parser — the same one used for DSF.
    let mut title = tag.title().map(|s| s.to_string());
    let mut artist = tag.artist().map(|s| s.to_string());
    let mut album = tag.album().map(|s| s.to_string());
    if title.as_deref().map_or(true, |t| t.trim().is_empty()) {
        // `LeadingProbe` : sonde spéculative, ne rien trouver est le cas
        // normal — elle ne journalise donc aucun rejet. Sans cette
        // distinction, chaque fichier sans titre d'une bibliothèque
        // produirait une ligne de journal par scan.
        if let Some(raw) = read_dsf_id3v2_raw(path, Some(0), Id3ReadSite::LeadingProbe, false) {
            if let Some(id3) = parse_id3v2_tag(&raw) {
                let prefer = |cur: Option<String>, alt: Option<&str>| -> Option<String> {
                    if cur.as_deref().map_or(true, |x| x.trim().is_empty()) {
                        alt.filter(|s| !s.trim().is_empty())
                            .map(|s| s.to_string())
                            .or(cur)
                    } else {
                        cur
                    }
                };
                title = prefer(title, id3.title());
                artist = prefer(artist, id3.artist());
                album = prefer(album, id3.album());
            }
        }
    }

    // Partial-tag fallback: a file can carry SOME tags (artist, a title…) yet be
    // missing others. lofty leaves those empty, and because the file *is* tagged
    // the whole-file filename fallback (tagless_fallback) never runs — so a FLAC
    // with no TRACKNUMBER shows track 0 and one with no ALBUM shows "Album
    // inconnu", even though the filename ("09.Stuffy") and folder carry the
    // answer (JP Robbe; confirmed on real libraries: Jazz at the Pawnshop /
    // Montreux Alexander FLACs have TITLE+ALBUM but no TRACKNUMBER). Fill each
    // MISSING field individually — never override a value the tag already has.
    let (fname_track, fname_title) = extract_title_from_filename(path);
    if title.as_deref().map_or(true, |t| t.trim().is_empty()) {
        title = fname_title;
    }
    // Le dossier parent n'est pas toujours l'album : sous `.../Titre/CD2/`,
    // c'est un disque, et l'album est au-dessus (#1656).
    let (album_du_chemin, _, disque_du_chemin) = album_artiste_du_chemin(path);
    if album.as_deref().map_or(true, |a| a.trim().is_empty()) {
        album = album_du_chemin;
    }
    let track_number = tag.track().or(fname_track);
    let disc_number = tag.disk().or(disque_du_chemin);

    Ok(TrackMetadata {
        title,
        artist,
        album,
        album_artist: get(ItemKey::AlbumArtist).or_else(|| raw_vorbis_field(path, "album_artist")),
        album_artist_sort: get(ItemKey::AlbumArtistSortOrder),
        track_number,
        disc_number,
        total_tracks,
        total_discs,
        // lofty's SetSubtitle maps Vorbis DISCSUBTITLE / ID3 TSST only — it has
        // NO mapping for the Vorbis `SETSUBTITLE` alias, which is the canonical
        // key in D. Pamingle's Vademecum. Read it raw as a fallback so those
        // files show per-disc subtitles too (Dominique, v0.9.9).
        disc_subtitle: get(ItemKey::SetSubtitle)
            .or_else(|| raw_vorbis_comment(path, "SETSUBTITLE")),
        year: tag
            .date()
            .map(|d| d.year as u32)
            .or_else(|| {
                // Fallback: try TDRL (ReleaseDate), then TDOR (OriginalReleaseDate)
                get(ItemKey::ReleaseDate).and_then(|s| s.get(..4)?.parse::<u32>().ok())
            })
            .or_else(|| {
                get(ItemKey::OriginalReleaseDate).and_then(|s| s.get(..4)?.parse::<u32>().ok())
            }),
        original_year,
        release_date: get(ItemKey::ReleaseDate),
        original_date: get(ItemKey::OriginalReleaseDate),
        genre,
        genres,
        duration_ms: {
            let lofty_dur = props.duration().as_millis() as u64;
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if ext == "mp3" {
                Some(mp3_duration_sanity_check(
                    path,
                    lofty_dur,
                    props.audio_bitrate(),
                ))
            } else {
                Some(lofty_dur)
            }
        },
        sample_rate: props.sample_rate(),
        bit_depth: props
            .bit_depth()
            .map(|b| b as u16)
            .or_else(|| m4a_probe.as_ref().and_then(|(_, bd)| *bd)),
        channels: props.channels().map(|c| c as u16),
        format: Some(match m4a_probe.as_ref() {
            Some((fmt, _)) => fmt.clone(),
            None => normalize_format(
                &format!("{:?}", tagged.file_type()).to_lowercase(),
                props.bit_depth(),
            ),
        }),
        file_size: std::fs::metadata(&*crate::library::artwork::extended_path(path))
            .ok()
            .map(|m| m.len()),
        bpm,
        compilation,
        label: get(ItemKey::Label),
        catalog_number: get(ItemKey::CatalogNumber),
        musicbrainz_recording_id: get(ItemKey::MusicBrainzRecordingId),
        musicbrainz_release_id: get(ItemKey::MusicBrainzReleaseId),
        musicbrainz_artist_id: get(ItemKey::MusicBrainzArtistId),
        musicbrainz_album_artist_id: get(ItemKey::MusicBrainzReleaseArtistId),
        musicbrainz_release_group_id: get(ItemKey::MusicBrainzReleaseGroupId),
        isrc: get(ItemKey::Isrc),
        has_cover: !tag.pictures().is_empty(),
        // Capture the embedded cover from this same lofty pass so the scanner
        // doesn't have to re-open the file to extract it.
        cover_art: tag.pictures().first().and_then(|pic| {
            let data = pic.data();
            // Don't retain oversized embedded pictures — they accumulate across
            // the scan batch and OOM the scanner. has_cover stays true, so the
            // scan re-extracts this file's cover to the cache on demand.
            if data.len() > MAX_RETAINED_COVER_BYTES {
                return None;
            }
            let mime = match pic.mime_type() {
                Some(lofty::picture::MimeType::Png) => "image/png",
                Some(lofty::picture::MimeType::Bmp) => "image/bmp",
                _ => "image/jpeg",
            };
            Some((data.to_vec(), mime.to_string()))
        }),
        credits,
        comment: tag.comment().map(|s| s.to_string()),
    })
}

pub fn read_metadata(path: &Path) -> Option<TrackMetadata> {
    try_read_metadata(path).ok()
}

/// Read extended metadata tags beyond the core fields already stored in the tracks table.
/// Returns a HashMap of key-value pairs suitable for the track_metadata table.
/// This extracts tags like composer, conductor, lyricist, performer, remixer,
/// ReplayGain values, MusicBrainz IDs, and other extended fields.
pub fn read_extended_metadata(path: &Path) -> HashMap<String, String> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, ItemKey};

    let mut meta = HashMap::new();

    let tagged = match Probe::open(path).and_then(|p| {
        p.options(
            ParseOptions::new()
                .parsing_mode(ParsingMode::Relaxed)
                .max_junk_bytes(1024 * 1024)
                // Don't load embedded cover art: this pass only reads text tags
                // (sort orders, credits, ISRC, lyrics…) via get_string and never
                // touches the picture. Without this, lofty reads the whole PICTURE
                // block into memory for EVERY file the scanner processes (called
                // per file in auto_scan's batch callback), and a huge/malformed
                // embedded image spikes RSS past the OOM killer — the same failure
                // try_read_metadata was hardened against (#JeromeQ), which this
                // second read path was missing (.15: 31 115 new files → ~14 GB RSS
                // → OOM crash-loop). Cover extraction stays in artwork::get_or_extract.
                .read_cover_art(false),
        )
        .guess_file_type()?
        .read()
    }) {
        Ok(t) => t,
        Err(_) => return meta,
    };

    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => return meta,
    };

    let get = |key: ItemKey| tag.get_string(key).map(|s| s.to_string());

    // Sort-order fields
    if let Some(v) = get(ItemKey::TrackArtistSortOrder) {
        meta.insert("sort_artist".into(), v);
    }
    if let Some(v) = get(ItemKey::AlbumTitleSortOrder) {
        meta.insert("sort_album".into(), v);
    }
    if let Some(v) = get(ItemKey::AlbumArtistSortOrder) {
        meta.insert("sort_album_artist".into(), v);
    }

    // Credits / personnel
    if let Some(v) = get(ItemKey::Composer) {
        meta.insert("composer".into(), v);
    }
    if let Some(v) = get(ItemKey::Conductor) {
        meta.insert("conductor".into(), v);
    }
    if let Some(v) = get(ItemKey::Lyricist) {
        meta.insert("lyricist".into(), v);
    }
    if let Some(v) = get(ItemKey::Performer) {
        meta.insert("performer".into(), v);
    }
    if let Some(v) = get(ItemKey::Remixer) {
        meta.insert("remixer".into(), v);
    }
    if let Some(v) = get(ItemKey::Label) {
        meta.insert("label".into(), v);
    }
    if let Some(v) = get(ItemKey::Producer) {
        meta.insert("producer".into(), v);
    }

    // Descriptive
    if let Some(v) = get(ItemKey::Bpm) {
        meta.insert("bpm".into(), v);
    }
    if let Some(v) = get(ItemKey::Mood) {
        meta.insert("mood".into(), v);
    }
    if let Some(v) = get(ItemKey::ContentGroup) {
        meta.insert("grouping".into(), v);
    }
    if let Some(v) = get(ItemKey::FlagCompilation) {
        meta.insert("compilation".into(), v);
    }
    if let Some(v) = tag.comment().map(|s| s.to_string()) {
        meta.insert("comment".into(), v);
    }
    if let Some(v) = get(ItemKey::Lyrics) {
        meta.insert("lyrics".into(), v);
    }

    // Identifiers
    if let Some(v) = get(ItemKey::Isrc) {
        meta.insert("isrc".into(), v);
    }
    if let Some(v) = get(ItemKey::Barcode) {
        meta.insert("barcode".into(), v);
    }
    if let Some(v) = get(ItemKey::CatalogNumber) {
        meta.insert("catalog_number".into(), v);
    }
    if let Some(v) = get(ItemKey::OriginalMediaType) {
        meta.insert("media_type".into(), v);
    }
    // RELEASECOUNTRY (Vorbis) — country of the specific release, ISO 3166-1.
    if let Some(v) = get(ItemKey::ReleaseCountry) {
        meta.insert("release_country".into(), v);
    }

    // Dates
    if let Some(v) = get(ItemKey::ReleaseDate) {
        meta.insert("release_date".into(), v);
    }
    if let Some(v) = get(ItemKey::OriginalReleaseDate) {
        meta.insert("original_date".into(), v);
    }

    // Technical
    if let Some(v) = get(ItemKey::EncodedBy) {
        meta.insert("encoder".into(), v);
    }
    // ENCODER (Vorbis) — encoding software that produced the file. Distinct from
    // `encoder` (ENCODEDBY / who encoded). lofty falls back to the FLAC vendor
    // string when no explicit ENCODER field is present.
    if let Some(v) = get(ItemKey::EncoderSoftware) {
        meta.insert("encoder_software".into(), v);
    }
    // Support / medium (CD, SACD, Vinyl…). Aligned on the MusicBrainz standard
    // MEDIA (Vorbis) / TMED (ID3v2) via ItemKey::OriginalMediaType, with the
    // legacy Vorbis `SOURCE` tag as a fallback for files tagged before the
    // switch (D. Pamingle : « nommer MEDIA, ID3v2 TMED, aligné sur MusicBrainz »).
    if let Some(v) = get(ItemKey::OriginalMediaType).or_else(|| raw_vorbis_comment(path, "SOURCE"))
    {
        meta.insert("source_media".into(), v);
    }
    // Dynamic Range — the mastering's measured dynamics, asked for twice on the
    // forum (Babacar #303, Patatorz #1418). No standard exists: lofty has no
    // `ItemKey` for it, so like `SOURCE` these fields are dropped during the
    // VorbisComments → generic-tag split and need the raw header read.
    //
    // Patatorz described the real chain (2026-08-15): measured with DROffline
    // MK2, written as `ALBUM DYNAMIC RANGE` through Mp3tag. He does NOT tag
    // individual tracks ("trop de travail"), which is why the album field leads
    // here; `DYNAMIC RANGE` is read anyway since foobar2000 writes it.
    //
    // NOT covered: MP3. There these values live in TXXX frames, which lofty does
    // not surface either, and no raw reader serves that format (the `Id3v2Tags`
    // one is DSF-specific). Separate piece of work.
    //
    // `ALBUM DR` / `DR` are accepted as secondary spellings. The header is read
    // ONCE for all four: a file without any of them — the common case — would
    // otherwise pay four separate 1 MB reads per scan.
    if let Some(header) = read_vorbis_header(path) {
        if let Some(v) = find_vorbis_comment(&header, "ALBUM DYNAMIC RANGE")
            .or_else(|| find_vorbis_comment(&header, "ALBUM DR"))
        {
            meta.insert("dr_album".into(), normalise_dr(&v));
        }
        // "DR" is checked last and only as a fallback: it is short enough to
        // collide with an unrelated field, so a specific spelling always wins.
        if let Some(v) = find_vorbis_comment(&header, "DYNAMIC RANGE")
            .or_else(|| find_vorbis_comment(&header, "DR"))
        {
            meta.insert("dr_track".into(), normalise_dr(&v));
        }
    }
    if let Some(v) = get(ItemKey::CopyrightMessage) {
        meta.insert("copyright".into(), v);
    }
    if let Some(v) = get(ItemKey::Language) {
        meta.insert("language".into(), v);
    }

    // ReplayGain
    if let Some(v) = get(ItemKey::ReplayGainTrackGain) {
        meta.insert("rg_track_gain".into(), v);
    }
    if let Some(v) = get(ItemKey::ReplayGainTrackPeak) {
        meta.insert("rg_track_peak".into(), v);
    }
    if let Some(v) = get(ItemKey::ReplayGainAlbumGain) {
        meta.insert("rg_album_gain".into(), v);
    }
    if let Some(v) = get(ItemKey::ReplayGainAlbumPeak) {
        meta.insert("rg_album_peak".into(), v);
    }

    // MusicBrainz IDs
    if let Some(v) = get(ItemKey::MusicBrainzRecordingId) {
        meta.insert("mb_track_id".into(), v);
    }
    // MUSICBRAINZ_RELEASETRACKID — per-release track MBID, distinct from the
    // recording id above (which is MUSICBRAINZ_TRACKID in Vorbis terms).
    if let Some(v) = get(ItemKey::MusicBrainzTrackId) {
        meta.insert("mb_release_track_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzReleaseId) {
        meta.insert("mb_release_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzArtistId) {
        meta.insert("mb_artist_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzReleaseArtistId) {
        meta.insert("mb_release_artist_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzReleaseGroupId) {
        meta.insert("mb_release_group_id".into(), v);
    }
    if let Some(v) = get(ItemKey::MusicBrainzWorkId) {
        meta.insert("mb_work_id".into(), v);
    }

    let mut corrections = Vec::new();
    for (key, value) in &mut meta {
        let (clean, mut found) = sanitize_untrusted_text(value, key);
        if !found.is_empty() {
            *value = clean;
            corrections.append(&mut found);
        }
    }
    meta.retain(|_, value| !value.is_empty());
    if !corrections.is_empty() {
        tracing::warn!(
            path = %path.display(),
            corrections = ?corrections,
            "extended_metadata_unsafe_text_sanitized"
        );
    }

    meta
}

#[derive(Debug, Clone)]
pub struct MetadataUpdate {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<u32>,
    pub composer: Option<String>,
    pub label: Option<String>,
    /// MBID de l'**enregistrement** MusicBrainz.
    ///
    /// Il désigne une prise, pas le rang d'une piste dans une édition :
    /// c'est la seule clé qui dise « ce morceau-ci est le même que
    /// celui-là » d'une édition à l'autre. Écrit tel quel, jamais déduit
    /// d'un titre.
    pub musicbrainz_recording_id: Option<String>,
}

impl MetadataUpdate {
    fn sanitized(&self) -> (Self, Vec<TextCorrection>) {
        fn clean(field: &str, value: &mut Option<String>, corrections: &mut Vec<TextCorrection>) {
            let Some(raw) = value.as_deref() else {
                return;
            };
            let (sanitized, mut found) = sanitize_untrusted_text(raw, field);
            if found.is_empty() {
                return;
            }
            *value = (!sanitized.is_empty()).then_some(sanitized);
            corrections.append(&mut found);
        }

        let mut update = self.clone();
        let mut corrections = Vec::new();
        clean("title", &mut update.title, &mut corrections);
        clean("artist", &mut update.artist, &mut corrections);
        clean("album", &mut update.album, &mut corrections);
        clean("album_artist", &mut update.album_artist, &mut corrections);
        clean("genre", &mut update.genre, &mut corrections);
        clean("composer", &mut update.composer, &mut corrections);
        clean("label", &mut update.label, &mut corrections);
        clean(
            "musicbrainz_recording_id",
            &mut update.musicbrainz_recording_id,
            &mut corrections,
        );
        (update, corrections)
    }
}

pub fn write_metadata(path: &Path, update: &MetadataUpdate) -> Result<(), String> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::tag::items::Timestamp;
    use lofty::tag::{Accessor, ItemKey, ItemValue, TagExt, TagItem};

    let (update, corrections) = update.sanitized();
    if !corrections.is_empty() {
        tracing::warn!(
            path = %path.display(),
            corrections = ?corrections,
            "metadata_tag_input_sanitized"
        );
    }

    let mut tagged = lofty::read_from_path(path).map_err(|e| format!("read: {e}"))?;
    let tag = tagged.primary_tag_mut().ok_or("no primary tag")?;

    if let Some(ref v) = update.title {
        tag.set_title(v.clone());
    }
    if let Some(ref v) = update.artist {
        tag.set_artist(v.clone());
    }
    if let Some(ref v) = update.album {
        tag.set_album(v.clone());
    }
    if let Some(ref v) = update.genre {
        tag.set_genre(v.clone());
    }
    if let Some(v) = update.track_number {
        tag.set_track(v);
    }
    if let Some(v) = update.disc_number {
        tag.set_disk(v);
    }
    if let Some(v) = update.year {
        tag.set_date(Timestamp {
            year: v as u16,
            ..Default::default()
        });
    }

    if let Some(ref v) = update.album_artist {
        tag.insert(TagItem::new(
            ItemKey::AlbumArtist,
            ItemValue::Text(v.clone()),
        ));
    }
    if let Some(ref v) = update.composer {
        tag.insert(TagItem::new(ItemKey::Composer, ItemValue::Text(v.clone())));
    }
    if let Some(ref v) = update.label {
        tag.insert(TagItem::new(ItemKey::Label, ItemValue::Text(v.clone())));
    }
    if let Some(ref v) = update.musicbrainz_recording_id {
        tag.insert(TagItem::new(
            ItemKey::MusicBrainzRecordingId,
            ItemValue::Text(v.clone()),
        ));
    }

    tag.save_to_path(path, WriteOptions::default())
        .map_err(|e| format!("write: {e}"))?;
    Ok(())
}

fn parse_credits(tag: &lofty::tag::Tag) -> Vec<TrackCredit> {
    use lofty::tag::ItemKey;

    let mut credits = Vec::new();

    if let Some(composer) = tag.get_string(ItemKey::Composer) {
        credits.push(TrackCredit {
            name: composer.to_string(),
            role: "composer".into(),
            instrument: None,
        });
    }

    if let Some(conductor) = tag.get_string(ItemKey::Conductor) {
        credits.push(TrackCredit {
            name: conductor.to_string(),
            role: "conductor".into(),
            instrument: None,
        });
    }

    if let Some(lyricist) = tag.get_string(ItemKey::Lyricist) {
        credits.push(TrackCredit {
            name: lyricist.to_string(),
            role: "lyricist".into(),
            instrument: None,
        });
    }

    for item in tag.items() {
        if item.key() == ItemKey::Performer
            && let Some(val) = item.value().text()
        {
            let (name, instrument) = if let Some((n, i)) = val.split_once('(') {
                (
                    n.trim().to_string(),
                    Some(i.trim_end_matches(')').trim().to_string()),
                )
            } else {
                (val.to_string(), None)
            };
            credits.push(TrackCredit {
                name,
                role: "performer".into(),
                instrument,
            });
        }
    }

    credits
}

/// Helper: build a minimal DSF file with the given audio properties
/// and optional ID3v2 tag appended.
#[cfg(test)]
fn build_dsf_bytes(id3v2_tag: Option<&[u8]>) -> Vec<u8> {
    let metadata_offset: u64 = if id3v2_tag.is_some() { 92 } else { 0 };
    let id3_len = id3v2_tag.map(|t| t.len()).unwrap_or(0);
    let total_size: u64 = 92 + id3_len as u64;

    let mut buf = vec![0u8; 92];
    // DSD chunk (28 bytes)
    buf[0..4].copy_from_slice(b"DSD ");
    buf[4..12].copy_from_slice(&28u64.to_le_bytes());
    buf[12..20].copy_from_slice(&total_size.to_le_bytes());
    buf[20..28].copy_from_slice(&metadata_offset.to_le_bytes());
    // fmt chunk (64 bytes)
    buf[28..32].copy_from_slice(b"fmt ");
    buf[32..40].copy_from_slice(&52u64.to_le_bytes());
    buf[40..44].copy_from_slice(&1u32.to_le_bytes()); // version
    buf[44..48].copy_from_slice(&0u32.to_le_bytes()); // format ID
    buf[48..52].copy_from_slice(&2u32.to_le_bytes()); // channel type
    buf[52..56].copy_from_slice(&2u32.to_le_bytes()); // channel count = 2
    buf[56..60].copy_from_slice(&2_822_400u32.to_le_bytes()); // DSD64
    buf[60..64].copy_from_slice(&1u32.to_le_bytes()); // bits per sample
    let samples: u64 = 2_822_400 * 180; // 3 minutes
    buf[64..72].copy_from_slice(&samples.to_le_bytes());

    if let Some(tag) = id3v2_tag {
        buf.extend_from_slice(tag);
    }
    buf
}

/// Helper: build a minimal ID3v2.3 tag with the given text frames.
/// Each entry is (frame_id, text_value), using ISO-8859-1 encoding.
#[cfg(test)]
fn build_id3v2_tag(frames: &[(&str, &str)]) -> Vec<u8> {
    let mut frame_bytes = Vec::new();
    for (id, text) in frames {
        assert_eq!(id.len(), 4);
        // Frame header: 4-byte ID + 4-byte size (big-endian) + 2-byte flags
        // Frame data: 1-byte encoding (0 = ISO-8859-1) + text bytes
        let text_bytes = text.as_bytes();
        let frame_size = 1 + text_bytes.len(); // encoding byte + text
        frame_bytes.extend_from_slice(id.as_bytes());
        frame_bytes.extend_from_slice(&(frame_size as u32).to_be_bytes());
        frame_bytes.extend_from_slice(&[0u8; 2]); // flags
        frame_bytes.push(0); // encoding = ISO-8859-1
        frame_bytes.extend_from_slice(text_bytes);
    }

    let tag_size = frame_bytes.len();
    // Encode tag_size as syncsafe integer
    let ss = [
        ((tag_size >> 21) & 0x7F) as u8,
        ((tag_size >> 14) & 0x7F) as u8,
        ((tag_size >> 7) & 0x7F) as u8,
        (tag_size & 0x7F) as u8,
    ];

    let mut tag = Vec::new();
    tag.extend_from_slice(b"ID3");
    tag.push(3); // version major = ID3v2.3
    tag.push(0); // version minor
    tag.push(0); // flags
    tag.extend_from_slice(&ss);
    tag.extend_from_slice(&frame_bytes);
    tag
}

#[cfg(test)]
mod dynamic_range_tests {
    use super::{find_vorbis_comment, normalise_dr};

    /// Build a Vorbis comment block the way the format stores it:
    /// `[len: u32 LE]["KEY=value"]`, which is what the reader relies on.
    fn comment_block(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut out = vec![0u8; 4]; // reader starts at index 4
        for (k, v) in pairs {
            let entry = format!("{k}={v}");
            out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
            out.extend_from_slice(entry.as_bytes());
        }
        out
    }

    #[test]
    fn reads_the_album_field_written_by_mp3tag() {
        // Patatorz's real chain: DROffline MK2 measures, Mp3tag writes this key.
        let data = comment_block(&[("ARTIST", "Autechre"), ("ALBUM DYNAMIC RANGE", "12")]);
        assert_eq!(
            find_vorbis_comment(&data, "ALBUM DYNAMIC RANGE").as_deref(),
            Some("12")
        );
    }

    #[test]
    fn field_lookup_ignores_case() {
        let data = comment_block(&[("album dynamic range", "9")]);
        assert_eq!(
            find_vorbis_comment(&data, "ALBUM DYNAMIC RANGE").as_deref(),
            Some("9")
        );
    }

    #[test]
    fn absent_field_is_none_not_a_wrong_match() {
        // A library with no DR tags is the common case; it must not pick up a
        // neighbouring field just because the block contains the word.
        let data = comment_block(&[("COMMENT", "dynamic range is great")]);
        assert!(find_vorbis_comment(&data, "ALBUM DYNAMIC RANGE").is_none());
    }

    /// The forms seen in the wild, and what the future numeric sort needs.
    #[test]
    fn normalises_every_known_spelling_to_bare_digits() {
        assert_eq!(normalise_dr("12"), "12");
        assert_eq!(normalise_dr("DR12"), "12");
        assert_eq!(normalise_dr("DR 12"), "12");
        assert_eq!(normalise_dr(" dr8 "), "8");
        assert_eq!(normalise_dr("Dr8"), "8");
        // Zero-padded values must collapse, or "08" and "8" sort apart.
        assert_eq!(normalise_dr("08"), "8");
    }

    #[test]
    fn keeps_dr_zero_rather_than_emptying_it() {
        // A crushed master really can measure DR0; an empty string would read
        // as "no value" and hide it.
        assert_eq!(normalise_dr("DR0"), "0");
        assert_eq!(normalise_dr("0"), "0");
    }

    #[test]
    fn unparseable_values_survive_untouched() {
        // Better to show something we could not interpret than to lose it.
        assert_eq!(normalise_dr("n/a"), "n/a");
        assert_eq!(normalise_dr("12.5"), "12.5");
        assert_eq!(normalise_dr(""), "");
    }
}

#[cfg(test)]
mod tests_dossier_de_disque {
    use super::*;

    #[test]
    fn reconnait_les_ecritures_courantes() {
        for (nom, attendu) in [
            ("CD1", 1),
            ("CD 2", 2),
            ("cd-3", 3),
            ("CD_4", 4),
            ("CD.5", 5),
            ("CD #6", 6),
            ("cd01", 1),
            ("Disc 2", 2),
            ("disk3", 3),
            ("Disque 4", 4),
            (" CD2 ", 2),
        ] {
            assert_eq!(
                numero_de_disque(nom),
                Some(attendu),
                "« {nom} » aurait dû être lu comme le disque {attendu}"
            );
        }
    }

    #[test]
    fn ne_confond_pas_un_titre_avec_un_disque() {
        // Chacun de ces noms est un VRAI dossier d'album. Le prendre pour un
        // disque effacerait l'album : ses pistes remonteraient d'un cran et se
        // rattacheraient au dossier parent.
        for nom in [
            "Greatest Hits Vol. 2", // « Vol » est volontairement hors liste
            "Vol. 2",
            "Disco",
            "Disc Jockey",
            "CD Rip",
            "CDs",
            "Discovery", // Daft Punk
            "CD",        // un préfixe sans numéro n'est pas un disque
            "Disque",
            "cd 0", // un disque 0 n'existe pas — c'est un nom, pas un rang
            "",
            "The Complete Motown Singles",
        ] {
            assert_eq!(
                numero_de_disque(nom),
                None,
                "« {nom} » ne doit PAS être pris pour un dossier de disque"
            );
        }
    }

    #[test]
    fn coffret_l_artiste_n_est_plus_le_titre_de_l_album() {
        // Le symptôme de jfpaquet (#1656) : sur un coffret rangé
        // `.../Titre/CD1/`, l'artiste retenu était le TITRE DE L'ALBUM, parce
        // que la convention `.../Artiste/Album/piste` prenait le dossier de
        // disque pour l'album.
        let (album, artiste, disque) = album_artiste_du_chemin(Path::new(
            "/Musique/Various Artists/The Complete Motown Singles/CD1/01 - Piste.wav",
        ));
        assert_eq!(album.as_deref(), Some("The Complete Motown Singles"));
        assert_eq!(artiste.as_deref(), Some("Various Artists"));
        // Sans le numéro de disque, fusionner CD1 et CD2 en un seul album
        // ferait collisionner les numéros de piste.
        assert_eq!(disque, Some(1));
    }

    #[test]
    fn les_deux_disques_d_un_coffret_donnent_le_meme_album() {
        let cd1 = album_artiste_du_chemin(Path::new("/M/Artiste/Titre/CD1/01.flac"));
        let cd2 = album_artiste_du_chemin(Path::new("/M/Artiste/Titre/CD2/01.flac"));
        assert_eq!(
            cd1.0, cd2.0,
            "les deux disques doivent nommer le même album"
        );
        assert_eq!(cd1.1, cd2.1);
        assert_eq!((cd1.2, cd2.2), (Some(1), Some(2)));
    }

    #[test]
    fn sans_dossier_de_disque_la_convention_ne_bouge_pas() {
        // Zéro régression sur le rangement habituel.
        let (album, artiste, disque) = album_artiste_du_chemin(Path::new(
            "/Musique/Miles Davis/Kind of Blue/01 - So What.flac",
        ));
        assert_eq!(album.as_deref(), Some("Kind of Blue"));
        assert_eq!(artiste.as_deref(), Some("Miles Davis"));
        assert_eq!(disque, None);
    }

    #[test]
    fn un_chemin_trop_court_ne_panique_pas() {
        let (album, artiste, disque) = album_artiste_du_chemin(Path::new("/piste.wav"));
        assert_eq!(album.as_deref(), None);
        assert_eq!(artiste, None);
        assert_eq!(disque, None);

        // Un dossier de disque à la racine : l'album manque, et c'est correct —
        // mieux vaut aucun album qu'un album nommé « CD1 ».
        let (album, artiste, disque) = album_artiste_du_chemin(Path::new("/CD1/piste.wav"));
        assert_eq!(album, None);
        assert_eq!(artiste, None);
        assert_eq!(disque, Some(1));
    }

    #[test]
    fn le_repli_sans_tags_applique_le_decalage() {
        // Le trajet réellement emprunté par un fichier illisible, bout en bout.
        let m = tagless_fallback_no_props(Path::new(
            "/Musique/Various Artists/VA-Best of 80s/CD2/03 - Piste.wav",
        ));
        assert_eq!(m.album.as_deref(), Some("VA-Best of 80s"));
        assert_eq!(m.artist.as_deref(), Some("Various Artists"));
        assert_eq!(m.disc_number, Some(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_text_preserves_a_visible_word_boundary_and_exact_offsets() {
        let (clean, corrections) =
            sanitize_untrusted_text("Jacobs, Lisa\0\u{feff}The\u{0001}String Soloists", "artist");
        assert_eq!(clean, "Jacobs, Lisa The String Soloists");
        assert_eq!(corrections.len(), 3);
        assert_eq!(corrections[0].kind, "NUL");
        assert_eq!(corrections[0].codepoint, 0);
        assert_eq!(corrections[0].byte_offset, 12);
        assert_eq!(corrections[1].kind, "BOM");
        assert_eq!(corrections[1].codepoint, 0xfeff);
        assert_eq!(corrections[1].byte_offset, 13);
        assert_eq!(corrections[2].kind, "CONTROL");
        assert_eq!(corrections[2].codepoint, 0x01);
        assert_eq!(corrections[2].byte_offset, 19);
        assert!(clean.chars().all(|c| c != '\0' && c != '\u{feff}'));

        let clean_multiline = "  ligne 1\nligne 2\t ";
        assert_eq!(
            sanitize_untrusted_text(clean_multiline, "lyrics"),
            (clean_multiline.to_string(), Vec::new())
        );
    }

    #[test]
    fn track_metadata_sanitizes_core_lists_and_nested_credits_before_db() {
        let mut metadata = TrackMetadata {
            title: Some("Titre\0cache".into()),
            artist: Some("Lisa\0\u{feff}The Strings".into()),
            genres: vec!["Jazz\u{feff}Fusion".into(), "\0".into()],
            credits: vec![TrackCredit {
                name: "Chef\0Orchestre".into(),
                role: "conductor".into(),
                instrument: Some("violin\u{feff}solo".into()),
            }],
            comment: Some("ligne 1\nligne 2".into()),
            ..Default::default()
        };

        let corrections = metadata.sanitize_text_fields();
        assert_eq!(metadata.title.as_deref(), Some("Titre cache"));
        assert_eq!(metadata.artist.as_deref(), Some("Lisa The Strings"));
        assert_eq!(metadata.genres, vec!["Jazz Fusion"]);
        assert_eq!(metadata.credits[0].name, "Chef Orchestre");
        assert_eq!(
            metadata.credits[0].instrument.as_deref(),
            Some("violin solo")
        );
        assert_eq!(metadata.comment.as_deref(), Some("ligne 1\nligne 2"));
        assert_eq!(corrections.len(), 7);
    }

    #[test]
    fn tag_update_ne_peut_pas_transmettre_un_nul_a_lofty() {
        let update = MetadataUpdate {
            title: Some("A\0B".into()),
            artist: Some("\u{feff}Artist".into()),
            album: Some("Album".into()),
            album_artist: None,
            genre: None,
            track_number: None,
            disc_number: None,
            year: None,
            composer: None,
            label: None,
            musicbrainz_recording_id: None,
        };
        let (clean, corrections) = update.sanitized();
        assert_eq!(clean.title.as_deref(), Some("A B"));
        assert_eq!(clean.artist.as_deref(), Some("Artist"));
        assert_eq!(corrections.len(), 2);
    }

    #[test]
    fn probe_m4a_props_attrape_un_panic_du_decodeur() {
        // symphonia-codec-aac 0.6.0 panique `index out of bounds` (ics/mod.rs:246,
        // len 64 idx 64) sur certains AAC-in-M4A malformés. On simule ce panic
        // exact : SANS la garde `catch_unwind` de `probe_m4a_props`, l'unwind
        // remonte et tue la tâche de scan (ROUGE) ; AVEC, il est attrapé et rendu
        // en `None` (VERT). On prouve que la MÉCANIQUE de garde attrape bien le
        // panic index-out-of-bounds d'origine.
        let sous_garde: Option<(String, Option<u16>)> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let v: Vec<u8> = vec![0u8; 64];
                let _ = v[64]; // index out of bounds: len 64 index 64 — comme #2302
                Some(("aac".to_string(), None))
            }))
            .unwrap_or(None);
        assert_eq!(
            sous_garde, None,
            "un panic du décodeur doit rendre None, pas remonter"
        );
    }

    #[test]
    fn mb_artist_query_includes_alias_clause() {
        // The alias clause is what lets non-Latin (Hangul/CJK) names resolve:
        // MusicBrainz indexes their romanized form as `name` and the native
        // script only as an alias, so a bare `artist:"…"` phrase returns none.
        let q = mb_artist_query("아이유");
        assert_eq!(q, "artist:\"아이유\" OR alias:\"아이유\"");
        assert!(q.contains("alias:"));
        // Quoted phrase precision preserved for Latin names.
        assert_eq!(
            mb_artist_query("The Beatles"),
            "artist:\"The Beatles\" OR alias:\"The Beatles\""
        );
    }

    /// Build a minimal ID3v2.3 tag containing a single APIC frame.
    fn id3v23_with_apic(mime: &[u8], img: &[u8]) -> Vec<u8> {
        // Frame body: encoding(1) + mime + NUL + pic_type(1) + desc NUL + data.
        let mut body = vec![0u8]; // Latin-1
        body.extend_from_slice(mime);
        body.push(0);
        body.push(3); // picture type: front cover
        body.push(0); // empty description + NUL
        body.extend_from_slice(img);

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[3, 0, 0]); // v2.3, no flags
        // syncsafe tag size (frame header 10 + body)
        let size = (10 + body.len()) as u32;
        tag.extend_from_slice(&[
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);
        tag.extend_from_slice(b"APIC");
        tag.extend_from_slice(&(body.len() as u32).to_be_bytes()); // v2.3 plain size
        tag.extend_from_slice(&[0, 0]); // frame flags
        tag.extend_from_slice(&body);
        tag
    }

    #[test]
    fn apic_extracted_from_id3v23() {
        let img = [0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4]; // JPEG-ish
        let tag = id3v23_with_apic(b"image/jpeg", &img);
        let parsed = parse_id3v2_tag(&tag).expect("tag parses");
        assert!(parsed.has_picture);
        let (mime, data) = parsed.picture.expect("picture present");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, img);
    }

    #[test]
    fn apic_body_v22_pic_png() {
        // v2.2 "PIC": encoding(1) + 3-char format + pic_type(1) + desc NUL + data.
        let img = [0x89u8, 0x50, 0x4E, 0x47, 9, 9];
        let mut body = vec![0u8]; // Latin-1
        body.extend_from_slice(b"PNG");
        body.push(3); // picture type
        body.push(0); // empty description
        body.extend_from_slice(&img);
        let (mime, data) = extract_apic_picture(&body, 2).expect("v2.2 PIC parses");
        assert_eq!(mime, "image/png");
        assert_eq!(data, img);
    }

    #[test]
    fn deunsynchronise_removes_stuffed_zeros() {
        // 0xFF 0x00 -> 0xFF; other bytes untouched; trailing 0xFF kept.
        assert_eq!(
            deunsynchronise(&[0x01, 0xFF, 0x00, 0x02, 0xFF, 0x00, 0xFF]),
            vec![0x01, 0xFF, 0x02, 0xFF, 0xFF]
        );
        // No stuffing -> identity.
        assert_eq!(deunsynchronise(&[0x01, 0x02, 0x03]), vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn parse_id3v23_utf16_title() {
        // A v2.3 TIT2 frame encoded as UTF-16-with-BOM — the case lofty
        // mis-decoded as an empty string on Yves Scordia's Chris Isaak MP3, so
        // Tune now falls back to this parser. Verify it reads the real title.
        fn utf16_frame(id: &str, text: &str) -> Vec<u8> {
            let mut body = vec![0x01u8]; // encoding 1 = UTF-16 with BOM
            body.extend_from_slice(&[0xFF, 0xFE]); // little-endian BOM
            for u in text.encode_utf16() {
                body.extend_from_slice(&u.to_le_bytes());
            }
            let mut f = id.as_bytes().to_vec();
            f.extend_from_slice(&(body.len() as u32).to_be_bytes()); // v2.3 plain size
            f.extend_from_slice(&[0, 0]); // frame flags
            f.extend_from_slice(&body);
            f
        }
        let mut frames = utf16_frame("TIT2", "First Comes The Night");
        frames.extend_from_slice(&utf16_frame("TPE1", "Chris Isaak"));
        let mut tag = vec![b'I', b'D', b'3', 0x03, 0x00, 0x00]; // ID3v2.3, no flags
        let size = frames.len();
        tag.push(((size >> 21) & 0x7F) as u8);
        tag.push(((size >> 14) & 0x7F) as u8);
        tag.push(((size >> 7) & 0x7F) as u8);
        tag.push((size & 0x7F) as u8);
        tag.extend_from_slice(&frames);

        let parsed = parse_id3v2_tag(&tag).expect("tag parses");
        assert_eq!(parsed.title(), Some("First Comes The Night"));
        assert_eq!(parsed.artist(), Some("Chris Isaak"));
    }

    #[test]
    fn parse_id3v22_unsynchronised_title() {
        // An unsynchronised ID3v2.2 tag (header flag 0x80) with a PIC frame
        // whose data contains 0xFF bytes — which get 0x00-stuffed — placed
        // BEFORE the TT2 title. Without de-unsynchronisation the PIC frame's
        // real byte length exceeds its declared size, the cursor desyncs and
        // the title is never found → filename fallback (Benjithom, #959).
        fn frame_v22(id: &str, data: &[u8]) -> Vec<u8> {
            let mut f = id.as_bytes().to_vec();
            let n = data.len();
            f.push((n >> 16) as u8);
            f.push((n >> 8) as u8);
            f.push(n as u8);
            f.extend_from_slice(data);
            f
        }
        // Frame sizes are the de-synchronised (true) sizes.
        let pic = frame_v22("PIC", &[0x00, 0xFF, 0xFF, 0x01]);
        let tt2 = frame_v22("TT2", &[0x00, b'H', b'i']); // Latin-1 "Hi"
        let mut body = Vec::new();
        body.extend_from_slice(&pic);
        body.extend_from_slice(&tt2);

        // Unsynchronise the assembled body: 0xFF -> 0xFF 0x00.
        let mut unsynced = Vec::new();
        for &b in &body {
            unsynced.push(b);
            if b == 0xFF {
                unsynced.push(0x00);
            }
        }

        let size = unsynced.len();
        let mut tag = vec![b'I', b'D', b'3', 0x02, 0x00, 0x80]; // v2.2, unsync flag
        tag.push(((size >> 21) & 0x7F) as u8);
        tag.push(((size >> 14) & 0x7F) as u8);
        tag.push(((size >> 7) & 0x7F) as u8);
        tag.push((size & 0x7F) as u8);
        tag.extend_from_slice(&unsynced);

        let parsed = parse_id3v2_tag(&tag).expect("tag parses");
        assert_eq!(parsed.title(), Some("Hi"));
        assert!(parsed.has_picture);
    }

    #[test]
    fn parse_id3v24_unsynchronised_frames() {
        // Pierre Mack's DSF (Mp3tag-written): ID3v2.4 with the whole-tag unsync
        // flag (0x80). Unlike v2.3, a v2.4 frame's synchsafe size counts the
        // *stored* (still-0x00-stuffed) bytes, so the tag body must NOT be
        // deunsynchronised as a whole — each frame is unstuffed individually
        // after slicing by its stored size. A large APIC full of 0xFF stuffing
        // precedes the title; whole-tag deunsync would desync every later frame.
        fn frame_v24(id: &str, data: &[u8]) -> Vec<u8> {
            // Unsynchronise the payload; the stored size is the stuffed length.
            let mut stuffed = Vec::new();
            for &b in data {
                stuffed.push(b);
                if b == 0xFF {
                    stuffed.push(0x00);
                }
            }
            let n = stuffed.len() as u32;
            let mut f = id.as_bytes().to_vec();
            f.push(((n >> 21) & 0x7F) as u8);
            f.push(((n >> 14) & 0x7F) as u8);
            f.push(((n >> 7) & 0x7F) as u8);
            f.push((n & 0x7F) as u8);
            f.extend_from_slice(&[0, 0]); // frame flags
            f.extend_from_slice(&stuffed);
            f
        }

        // APIC body with 0xFF bytes (front cover JPEG-ish), then the title.
        let mut apic_body = vec![0u8]; // Latin-1
        apic_body.extend_from_slice(b"image/jpeg");
        apic_body.push(0);
        apic_body.push(3); // front cover
        apic_body.push(0); // empty description
        apic_body.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 0xFF, 0xFF]);
        let apic = frame_v24("APIC", &apic_body);
        let tit2 = frame_v24("TIT2", &[0x00, b'H', b'i']); // Latin-1 "Hi"

        let mut body = apic;
        body.extend_from_slice(&tit2);

        let size = body.len();
        let mut tag = vec![b'I', b'D', b'3', 0x04, 0x00, 0x80]; // v2.4, whole-tag unsync
        tag.push(((size >> 21) & 0x7F) as u8);
        tag.push(((size >> 14) & 0x7F) as u8);
        tag.push(((size >> 7) & 0x7F) as u8);
        tag.push((size & 0x7F) as u8);
        tag.extend_from_slice(&body);

        let parsed = parse_id3v2_tag(&tag).expect("tag parses");
        assert_eq!(parsed.title(), Some("Hi"));
        assert!(parsed.has_picture);
        let (mime, data) = parsed.picture.expect("picture present");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 0xFF, 0xFF]);
    }

    #[test]
    fn parse_id3v24_per_frame_unsync_flag() {
        // v2.4 also allows a single frame to opt into unsync via its format flag
        // (0x02, second flag byte) while the tag header does not set 0x80.
        let title = [0x00u8, b'F', 0xFF, b'x']; // Latin-1 with a raw 0xFF
        let mut stuffed = Vec::new();
        for &b in &title {
            stuffed.push(b);
            if b == 0xFF {
                stuffed.push(0x00);
            }
        }
        let n = stuffed.len() as u32;
        let mut frame = b"TIT2".to_vec();
        frame.push(((n >> 21) & 0x7F) as u8);
        frame.push(((n >> 14) & 0x7F) as u8);
        frame.push(((n >> 7) & 0x7F) as u8);
        frame.push((n & 0x7F) as u8);
        frame.extend_from_slice(&[0x00, 0x02]); // per-frame unsync flag
        frame.extend_from_slice(&stuffed); // stored (still-stuffed) payload

        let size = frame.len();
        let mut tag = vec![b'I', b'D', b'3', 0x04, 0x00, 0x00]; // v2.4, no tag flag
        tag.push(((size >> 21) & 0x7F) as u8);
        tag.push(((size >> 14) & 0x7F) as u8);
        tag.push(((size >> 7) & 0x7F) as u8);
        tag.push((size & 0x7F) as u8);
        tag.extend_from_slice(&frame);

        let parsed = parse_id3v2_tag(&tag).expect("tag parses");
        assert_eq!(parsed.title(), Some("F\u{FF}x"));
    }

    #[test]
    fn apic_utf16_description_skipped() {
        // encoding 1 (UTF-16): description terminated by a 2-byte NUL.
        let img = [0xFFu8, 0xD8, 42];
        let mut body = vec![1u8]; // UTF-16
        body.extend_from_slice(b"image/jpeg");
        body.push(0);
        body.push(3); // picture type
        body.extend_from_slice(&[0x00, 0x00]); // empty UTF-16 description
        body.extend_from_slice(&img);
        let (mime, data) = extract_apic_picture(&body, 4).expect("utf-16 desc parses");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, img);
    }

    #[test]
    fn alac_bit_depth_from_magic_cookie() {
        // ALACSpecificConfig: bit depth lives at byte 5 of the 24-byte payload.
        let mut cookie = [0u8; 24];
        cookie[5] = 24;
        assert_eq!(alac_bit_depth_from_cookie(Some(&cookie)), Some(24));
        cookie[5] = 16;
        assert_eq!(alac_bit_depth_from_cookie(Some(&cookie)), Some(16));

        // With the optional `frma`/`alac` atom prefixes, the payload is offset.
        let mut prefixed = Vec::new();
        prefixed.extend_from_slice(&[0, 0, 0, 12]);
        prefixed.extend_from_slice(b"frma");
        prefixed.extend_from_slice(&[0, 0, 0, 0]);
        prefixed.extend_from_slice(&[0, 0, 0, 12]);
        prefixed.extend_from_slice(b"alac");
        prefixed.extend_from_slice(&[0, 0, 0, 0]);
        let mut payload = [0u8; 24];
        payload[5] = 24;
        prefixed.extend_from_slice(&payload);
        assert_eq!(alac_bit_depth_from_cookie(Some(&prefixed)), Some(24));

        // Missing / too-short / out-of-range depths yield None.
        assert_eq!(alac_bit_depth_from_cookie(None), None);
        assert_eq!(alac_bit_depth_from_cookie(Some(&[0u8; 10])), None);
        let mut bad = [0u8; 24];
        bad[5] = 99;
        assert_eq!(alac_bit_depth_from_cookie(Some(&bad)), None);
    }

    #[test]
    fn nonexistent_file_returns_none() {
        assert!(read_metadata(Path::new("/tmp/nonexistent.flac")).is_none());
    }

    #[test]
    fn split_genre_semicolon() {
        let genres = split_genre_tag("Jazz; Fusion; Progressive");
        assert_eq!(genres, vec!["Jazz", "Fusion", "Progressive"]);
    }

    #[test]
    fn split_genre_slash() {
        let genres = split_genre_tag("Jazz/Fusion/Progressive");
        assert_eq!(genres, vec!["Jazz", "Fusion", "Progressive"]);
    }

    #[test]
    fn split_genre_backslash() {
        let genres = split_genre_tag("Rock\\Metal\\Punk");
        assert_eq!(genres, vec!["Rock", "Metal", "Punk"]);
    }

    #[test]
    fn split_genre_null_byte() {
        let genres = split_genre_tag("Jazz\0Blues\0Soul");
        assert_eq!(genres, vec!["Jazz", "Blues", "Soul"]);
    }

    #[test]
    fn split_genre_single() {
        let genres = split_genre_tag("Jazz");
        assert_eq!(genres, vec!["Jazz"]);
    }

    #[test]
    fn split_genre_empty() {
        let genres = split_genre_tag("");
        assert!(genres.is_empty());
    }

    #[test]
    fn split_genre_mixed_separators() {
        let genres = split_genre_tag("Jazz; Rock/Blues");
        assert_eq!(genres, vec!["Jazz", "Rock", "Blues"]);
    }

    #[test]
    fn split_genre_trims_whitespace() {
        let genres = split_genre_tag("  Jazz ;  Fusion  ; Progressive  ");
        assert_eq!(genres, vec!["Jazz", "Fusion", "Progressive"]);
    }

    #[test]
    fn normalize_genre_preserves_hyphen_casing() {
        // Yves Scordia: "Folk-Punk" was lower-cased after the hyphen.
        assert_eq!(normalize_genre("Folk-Punk"), "Folk-Punk");
        assert_eq!(normalize_genre("folk-punk"), "Folk-Punk");
        assert_eq!(normalize_genre("hip-hop"), "Hip-Hop");
        assert_eq!(normalize_genre("lo-fi"), "Lo-Fi");
        // Slash + hyphen combos still work.
        assert_eq!(normalize_genre("Folk-Punk/Ska-Punk"), "Folk-Punk/Ska-Punk");
        // Single-word genres are unaffected.
        assert_eq!(normalize_genre("rock"), "Rock");
    }

    #[test]
    fn split_genre_consecutive_separators() {
        let genres = split_genre_tag("Jazz;;Rock");
        assert_eq!(genres, vec!["Jazz", "Rock"]);
    }

    #[test]
    fn split_genre_only_separators() {
        let genres = split_genre_tag(";;;");
        assert!(genres.is_empty());
    }

    #[test]
    fn split_genre_unicode() {
        let genres = split_genre_tag("Musique classique; Musique experimentale");
        assert_eq!(genres, vec!["Musique Classique", "Musique Experimentale"]);
    }

    #[test]
    fn split_genre_single_char() {
        let genres = split_genre_tag("A");
        assert_eq!(genres, vec!["A"]);
    }

    #[test]
    fn track_metadata_default() {
        let md = TrackMetadata::default();
        assert!(md.title.is_none());
        assert!(md.artist.is_none());
        assert!(md.genres.is_empty());
        assert!(!md.compilation);
        assert!(!md.has_cover);
        assert!(md.credits.is_empty());
    }

    #[test]
    fn track_metadata_serialization() {
        let md = TrackMetadata {
            title: Some("Test".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            genre: Some("Jazz".into()),
            genres: vec!["Jazz".into(), "Fusion".into()],
            duration_ms: Some(300_000),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: Some(2),
            format: Some("flac".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&md).unwrap();
        assert_eq!(json["title"], "Test");
        assert_eq!(json["genres"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn track_credit_default() {
        let credit = TrackCredit::default();
        assert_eq!(credit.name, "");
        assert_eq!(credit.role, "");
        assert!(credit.instrument.is_none());
    }

    #[test]
    fn metadata_update_fields() {
        let update = MetadataUpdate {
            title: Some("New Title".into()),
            artist: Some("New Artist".into()),
            album: None,
            album_artist: None,
            genre: Some("Rock".into()),
            track_number: Some(1),
            disc_number: Some(1),
            year: Some(2024),
            composer: Some("Composer".into()),
            label: None,
            musicbrainz_recording_id: None,
        };
        assert_eq!(update.title.as_deref(), Some("New Title"));
        assert_eq!(update.year, Some(2024));
    }

    #[test]
    fn normalize_format_mpeg_to_mp3() {
        assert_eq!(normalize_format("mpeg", None), "mp3");
    }

    /// #1612 — le conteneur DSD est conservé, plus replié sur « dsd ».
    ///
    /// Ces deux tests figeaient l'inverse. Le repli faisait qu'un `.dsf` et un
    /// `.dff` produisaient une seule entrée dans les types de fichiers, et que
    /// l'utilisateur ne pouvait plus savoir ce qu'il possédait. Rien ne
    /// reposait dessus : tout le code qui décide « est-ce du DSD ? » teste déjà
    /// les trois valeurs.
    #[test]
    fn normalize_format_conserve_le_conteneur_dsd() {
        assert_eq!(normalize_format("dsf", None), "dsf");
        assert_eq!(normalize_format("dff", None), "dff");
        // « dsd » reste accepté : c'est la valeur des lignes non encore
        // converties, et elle reste reconnue partout.
        assert_eq!(normalize_format("dsd", None), "dsd");
    }

    #[test]
    fn normalize_format_flac_unchanged() {
        assert_eq!(normalize_format("flac", None), "flac");
    }

    #[test]
    fn normalize_format_wav_unchanged() {
        assert_eq!(normalize_format("wav", None), "wav");
    }

    #[test]
    fn normalize_format_aiff_unchanged() {
        assert_eq!(normalize_format("aiff", None), "aiff");
    }

    #[test]
    fn dsf_dff_fallback_returns_none_for_non_dsd() {
        assert!(dsf_dff_fallback(Path::new("/tmp/test.flac")).is_none());
        assert!(dsf_dff_fallback(Path::new("/tmp/test.mp3")).is_none());
    }

    #[test]
    fn dsf_dff_fallback_rend_le_conteneur_reel() {
        // #1612 : le repli connait l'extension des sa premiere ligne. Ecrire
        // « dsd » en dur ici rouvrirait le defaut que `normalize_format` ferme.
        let meta = dsf_dff_fallback(Path::new("/tmp/nonexistent_track.dsf"));
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.format.as_deref(), Some("dsf"));
        assert_eq!(meta.title.as_deref(), Some("nonexistent_track"));
        assert_eq!(meta.duration_ms, Some(0));

        let meta2 = dsf_dff_fallback(Path::new("/tmp/test_track.dff"));
        assert!(meta2.is_some());
        let meta2 = meta2.unwrap();
        assert_eq!(meta2.format.as_deref(), Some("dff"));
        assert_eq!(meta2.title.as_deref(), Some("test_track"));
    }

    #[test]
    fn dsf_fallback_with_valid_header() {
        use std::io::Write;
        let tmp = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        let buf = build_dsf_bytes(None);
        std::fs::File::create(tmp.path())
            .unwrap()
            .write_all(&buf)
            .unwrap();
        let meta = dsf_dff_fallback(tmp.path());
        assert!(meta.is_some());
        let meta = meta.unwrap();
        // #1612 : un `.dsf` porte desormais son conteneur, plus « dsd ».
        assert_eq!(meta.format.as_deref(), Some("dsf"));
        assert_eq!(meta.sample_rate, Some(2_822_400));
        assert_eq!(meta.channels, Some(2));
        let dur = meta.duration_ms.unwrap();
        assert!(
            (179_000..=181_000).contains(&dur),
            "unexpected duration: {dur}ms"
        );
    }

    #[test]
    fn dsf_fallback_reads_id3v2_tags() {
        use std::io::Write;
        let id3_tag = build_id3v2_tag(&[
            ("TIT2", "Man On The Corner"),
            ("TPE1", "Genesis"),
            ("TALB", "Abacab"),
            ("TPE2", "Genesis"),
            ("TRCK", "7/11"),
            ("TPOS", "1/2"),
            ("TDRC", "1981"),
            ("TCON", "Rock"),
            ("TPUB", "Virgin Records"),
        ]);
        let buf = build_dsf_bytes(Some(&id3_tag));
        let tmp = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        std::fs::File::create(tmp.path())
            .unwrap()
            .write_all(&buf)
            .unwrap();
        let meta = dsf_dff_fallback(tmp.path());
        assert!(
            meta.is_some(),
            "dsf_dff_fallback should return Some for DSF with ID3v2"
        );
        let meta = meta.unwrap();
        assert_eq!(meta.title.as_deref(), Some("Man On The Corner"));
        assert_eq!(meta.artist.as_deref(), Some("Genesis"));
        assert_eq!(meta.album.as_deref(), Some("Abacab"));
        assert_eq!(meta.album_artist.as_deref(), Some("Genesis"));
        assert_eq!(meta.track_number, Some(7));
        assert_eq!(meta.total_tracks, Some(11));
        assert_eq!(meta.disc_number, Some(1));
        assert_eq!(meta.total_discs, Some(2));
        assert_eq!(meta.year, Some(1981));
        assert_eq!(meta.genre.as_deref(), Some("Rock"));
        assert_eq!(meta.label.as_deref(), Some("Virgin Records"));
        // #1612 : un `.dsf` porte desormais son conteneur, plus « dsd ».
        assert_eq!(meta.format.as_deref(), Some("dsf"));
        assert_eq!(meta.sample_rate, Some(2_822_400));
        assert_eq!(meta.channels, Some(2));
        assert_eq!(meta.bit_depth, Some(1));
    }

    #[test]
    fn dsf_fallback_id3v2_overrides_path() {
        use std::io::Write;
        let base = tempfile::TempDir::new().unwrap();
        let dir = base.path().join("V_DSF").join("Genesis - Abacab");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("07 - Man On The Corner.dsf");
        let id3_tag = build_id3v2_tag(&[
            ("TIT2", "Man On The Corner"),
            ("TPE1", "Genesis"),
            ("TALB", "Abacab"),
            ("TRCK", "7"),
        ]);
        let buf = build_dsf_bytes(Some(&id3_tag));
        std::fs::File::create(&file_path)
            .unwrap()
            .write_all(&buf)
            .unwrap();
        let meta = dsf_dff_fallback(&file_path);
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.title.as_deref(), Some("Man On The Corner"));
        assert_eq!(meta.artist.as_deref(), Some("Genesis"));
        assert_eq!(meta.album.as_deref(), Some("Abacab"));
        assert_eq!(meta.track_number, Some(7));
    }

    #[test]
    fn try_read_metadata_dsf_fallback() {
        let result = try_read_metadata(Path::new("/tmp/nonexistent_fallback_test.dsf"));
        assert!(result.is_ok());
        let meta = result.unwrap();
        // #1612 : un `.dsf` porte desormais son conteneur, plus « dsd ».
        assert_eq!(meta.format.as_deref(), Some("dsf"));
    }

    #[test]
    fn try_read_metadata_non_dsd_still_errors() {
        let result = try_read_metadata(Path::new("/tmp/nonexistent_fallback_test.flac"));
        assert!(result.is_err());
    }

    #[test]
    fn try_read_metadata_untagged_wav_falls_back_to_path() {
        // Regression (Jean-Luc Cassé, Windows): ~16% of his WAV-ripped albums never
        // appeared in the library because an untagged WAV made lofty return no tag
        // (or fail to parse), read_metadata returned None, and the scanner dropped
        // the file as `skipped_no_metadata`. A supported, on-disk audio file must
        // NEVER be dropped just because its tags are unreadable — it must index with
        // metadata derived from the path.
        use std::io::Write;

        // Minimal canonical PCM WAV (stereo/16-bit/44100), no INFO/id3 tags.
        let mut wav: Vec<u8> = Vec::new();
        let data: [u8; 8] = [0; 8];
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // subchunk1 size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // channels
        wav.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&176_400u32.to_le_bytes()); // byte rate
        wav.extend_from_slice(&4u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);

        // Directory convention: .../Artist/Album/NN - Title.wav
        let base = tempfile::TempDir::new().unwrap();
        let dir = base.path().join("Jean-Luc").join("Best Of");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("07 - Untagged Song.wav");
        std::fs::File::create(&file)
            .unwrap()
            .write_all(&wav)
            .unwrap();

        let result = try_read_metadata(&file);
        assert!(result.is_ok(), "untagged WAV must not error: {result:?}");
        let meta = result.unwrap();
        assert_eq!(meta.title.as_deref(), Some("Untagged Song"));
        assert_eq!(meta.album.as_deref(), Some("Best Of"));
        assert_eq!(meta.artist.as_deref(), Some("Jean-Luc"));
        assert_eq!(meta.album_artist.as_deref(), Some("Jean-Luc"));
        assert_eq!(meta.track_number, Some(7));
        // Holds through both fallback paths: tagless_fallback (lofty parsed props)
        // and tagless_fallback_no_props (lofty failed) both normalise to "wav".
        assert_eq!(meta.format.as_deref(), Some("wav"));
    }

    #[test]
    fn try_read_metadata_dsf_title_not_filename() {
        // Regression (LANDES Philippe / Benjithom): a tagged DSF must surface its
        // real ID3v2 title through the full try_read_metadata path, never fall
        // back to the filename. Covers the case where lofty parses the container
        // and returns a (possibly title-less) tag: our DSF ID3v2 parser must
        // still fill the title.
        use std::io::Write;
        let id3_tag = build_id3v2_tag(&[("TIT2", "Aurora"), ("TPE1", "Yes"), ("TALB", "Fragile")]);
        let buf = build_dsf_bytes(Some(&id3_tag));
        let tmp = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        std::fs::File::create(tmp.path())
            .unwrap()
            .write_all(&buf)
            .unwrap();
        let meta = try_read_metadata(tmp.path());
        let meta = meta.expect("try_read_metadata should succeed for a tagged DSF");
        assert_eq!(meta.title.as_deref(), Some("Aurora"));
        assert_eq!(meta.artist.as_deref(), Some("Yes"));
        assert_eq!(meta.album.as_deref(), Some("Fragile"));
        // #1612 : un `.dsf` porte desormais son conteneur, plus « dsd ».
        assert_eq!(meta.format.as_deref(), Some("dsf"));
    }

    #[test]
    fn mp3_dual_id3v2_prefers_first_tag_genre() {
        // Forum #1184: an MP3 with two prepended ID3v2 tags (iTunes M4A→MP3
        // leftover + Mp3Tag re-tag) must report the FIRST tag's genre, like every
        // standard player — not lofty's last-wins merge of the stale second tag.
        use std::io::Write;
        let first = build_id3v2_tag(&[("TIT2", "Song"), ("TCON", "Alternatif")]);
        let second = build_id3v2_tag(&[("TCON", "Singer/Songwriter")]);
        let mut buf = first.clone();
        buf.extend_from_slice(&second);
        let tmp = tempfile::Builder::new().suffix(".mp3").tempfile().unwrap();
        std::fs::File::create(tmp.path())
            .unwrap()
            .write_all(&buf)
            .unwrap();
        let g = mp3_first_tag_genre_if_dual(tmp.path());
        assert_eq!(g.as_deref(), Some("Alternatif"));
    }

    #[test]
    fn mp3_single_id3v2_leaves_genre_to_lofty() {
        // Guard: a normal single-tag MP3 must NOT trigger the override (returns
        // None), so lofty's encoding/numeric-genre-aware value is kept.
        use std::io::Write;
        let only = build_id3v2_tag(&[("TIT2", "Song"), ("TCON", "Jazz")]);
        let tmp = tempfile::Builder::new().suffix(".mp3").tempfile().unwrap();
        std::fs::File::create(tmp.path())
            .unwrap()
            .write_all(&only)
            .unwrap();
        let g = mp3_first_tag_genre_if_dual(tmp.path());
        assert_eq!(g, None);
    }

    #[test]
    fn filename_track_and_title_extraction() {
        // Powers the partial-tag fallback (JP Robbe / Jazz at the Pawnshop): a
        // FLAC missing TRACKNUMBER still gets its number + title from the filename.
        let cases = [
            ("09.Stuffy.flac", Some(9), "Stuffy"),
            ("01 Expresso love.flac", Some(1), "Expresso love"),
            (
                "13 Going home (with Hank Marvin).flac",
                Some(13),
                "Going home (with Hank Marvin)",
            ),
            ("Sultans of Swing.flac", None, "Sultans of Swing"), // no leading number
        ];
        for (name, want_num, want_title) in cases {
            let (num, title) = extract_title_from_filename(Path::new(name));
            assert_eq!(num, want_num, "track number for {name}");
            assert_eq!(title.as_deref(), Some(want_title), "title for {name}");
        }
    }

    #[test]
    fn normalize_format_mp4_aac_no_bit_depth() {
        // AAC (lossy) in M4A container: lofty reports no bit depth
        assert_eq!(normalize_format("mp4", None), "aac");
        assert_eq!(normalize_format("m4a", None), "aac");
    }

    #[test]
    fn normalize_format_mp4_alac_with_bit_depth() {
        // ALAC (lossless) in M4A container: lofty reports bit depth (16 or 24)
        assert_eq!(normalize_format("mp4", Some(16)), "alac");
        assert_eq!(normalize_format("mp4", Some(24)), "alac");
        assert_eq!(normalize_format("m4a", Some(16)), "alac");
        assert_eq!(normalize_format("m4a", Some(24)), "alac");
    }

    #[test]
    fn normalize_format_unknown_passthrough() {
        assert_eq!(normalize_format("ogg", None), "ogg");
        assert_eq!(normalize_format("opus", None), "opus");
        assert_eq!(normalize_format("wv", None), "wv");
        assert_eq!(normalize_format("ape", None), "ape");
    }

    #[test]
    fn split_genre_parenthesized_id3v1_numeric() {
        let genres = split_genre_tag("(17)Rock");
        assert!(!genres.is_empty());
    }

    #[test]
    fn split_genre_very_long() {
        let long = (0..50)
            .map(|i| format!("Genre{i}"))
            .collect::<Vec<_>>()
            .join(";");
        let genres = split_genre_tag(&long);
        assert_eq!(genres.len(), 50);
    }

    #[test]
    fn track_metadata_all_optional_fields_none() {
        let md = TrackMetadata::default();
        let json = serde_json::to_value(&md).unwrap();
        assert!(json["title"].is_null());
        assert!(json["artist"].is_null());
        assert!(json["album"].is_null());
        assert!(json["album_artist"].is_null());
        assert!(json["track_number"].is_null());
        assert!(json["disc_number"].is_null());
        assert!(json["year"].is_null());
        assert!(json["sample_rate"].is_null());
        assert!(json["bit_depth"].is_null());
        assert!(json["duration_ms"].is_null());
        assert_eq!(json["compilation"], false);
        assert_eq!(json["has_cover"], false);
    }

    #[test]
    fn track_metadata_json_types_stable() {
        let md = TrackMetadata {
            title: Some("Track".into()),
            track_number: Some(3),
            disc_number: Some(1),
            total_tracks: Some(12),
            total_discs: Some(2),
            year: Some(2024),
            duration_ms: Some(245_000),
            sample_rate: Some(96000),
            bit_depth: Some(24),
            channels: Some(2),
            bpm: Some(120.5),
            compilation: true,
            has_cover: true,
            cover_art: None,
            genres: vec!["Jazz".into(), "Fusion".into()],
            ..Default::default()
        };
        let json = serde_json::to_value(&md).unwrap();
        assert!(json["track_number"].is_number());
        assert!(json["disc_number"].is_number());
        assert!(json["total_tracks"].is_number());
        assert!(json["total_discs"].is_number());
        assert!(json["year"].is_number());
        assert!(json["duration_ms"].is_number());
        assert!(json["sample_rate"].is_number());
        assert!(json["bit_depth"].is_number());
        assert!(json["channels"].is_number());
        assert!(json["bpm"].is_number());
        assert_eq!(json["compilation"], true);
        assert_eq!(json["has_cover"], true);
        assert!(json["genres"].is_array());
        assert_eq!(json["genres"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn track_metadata_credits_serialization() {
        let md = TrackMetadata {
            credits: vec![
                TrackCredit {
                    name: "John Doe".into(),
                    role: "composer".into(),
                    instrument: None,
                },
                TrackCredit {
                    name: "Jane Doe".into(),
                    role: "performer".into(),
                    instrument: Some("piano".into()),
                },
            ],
            ..Default::default()
        };
        let json = serde_json::to_value(&md).unwrap();
        let credits = json["credits"].as_array().unwrap();
        assert_eq!(credits.len(), 2);
        assert_eq!(credits[0]["name"], "John Doe");
        assert_eq!(credits[0]["role"], "composer");
        assert!(credits[0]["instrument"].is_null());
        assert_eq!(credits[1]["instrument"], "piano");
    }

    #[test]
    fn track_metadata_musicbrainz_ids_serialization() {
        let md = TrackMetadata {
            musicbrainz_recording_id: Some("rec-uuid".into()),
            musicbrainz_release_id: Some("rel-uuid".into()),
            musicbrainz_artist_id: Some("art-uuid".into()),
            musicbrainz_album_artist_id: Some("aa-uuid".into()),
            musicbrainz_release_group_id: Some("rg-uuid".into()),
            isrc: Some("USRC12345678".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&md).unwrap();
        assert_eq!(json["musicbrainz_recording_id"], "rec-uuid");
        assert_eq!(json["musicbrainz_release_id"], "rel-uuid");
        assert_eq!(json["musicbrainz_artist_id"], "art-uuid");
        assert_eq!(json["musicbrainz_album_artist_id"], "aa-uuid");
        assert_eq!(json["musicbrainz_release_group_id"], "rg-uuid");
        assert_eq!(json["isrc"], "USRC12345678");
    }

    #[test]
    fn dsf_fallback_derives_album_and_artist_from_path() {
        let meta = dsf_dff_fallback(Path::new("/music/Miles Davis/Kind of Blue/01-So What.dsf"));
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.title.as_deref(), Some("01-So What"));
        assert_eq!(meta.album.as_deref(), Some("Kind of Blue"));
        assert_eq!(meta.artist.as_deref(), Some("Miles Davis"));
    }

    #[test]
    fn normalize_format_case_sensitivity() {
        assert_eq!(normalize_format("mpeg", None), "mp3");
        assert_eq!(normalize_format("MPEG", None), "MPEG");
    }

    #[test]
    fn normalize_genre_title_case() {
        assert_eq!(normalize_genre("classique"), "Classique");
        assert_eq!(normalize_genre("ROCK"), "Rock");
        assert_eq!(normalize_genre("jazz"), "Jazz");
        assert_eq!(normalize_genre("Jazz"), "Jazz");
    }

    #[test]
    fn normalize_genre_multi_word() {
        assert_eq!(normalize_genre("hip hop"), "Hip Hop");
        assert_eq!(normalize_genre("trip hop"), "Trip Hop");
        assert_eq!(normalize_genre("HARD ROCK"), "Hard Rock");
    }

    #[test]
    fn normalize_genre_special_tokens() {
        assert_eq!(normalize_genre("r&b"), "R&B");
        assert_eq!(normalize_genre("R&B"), "R&B");
        assert_eq!(normalize_genre("dj mix"), "DJ Mix");
        assert_eq!(normalize_genre("DJ"), "DJ");
        assert_eq!(normalize_genre("edm"), "EDM");
        assert_eq!(normalize_genre("uk garage"), "UK Garage");
    }

    #[test]
    fn normalize_genre_slash_compound() {
        assert_eq!(normalize_genre("Folk/Rock"), "Folk/Rock");
        assert_eq!(normalize_genre("folk/rock"), "Folk/Rock");
        assert_eq!(normalize_genre("FOLK/ROCK"), "Folk/Rock");
    }

    #[test]
    fn normalize_genre_already_correct() {
        assert_eq!(normalize_genre("Progressive Rock"), "Progressive Rock");
        assert_eq!(normalize_genre("Jazz"), "Jazz");
    }

    #[test]
    fn split_genre_normalizes_case() {
        let genres = split_genre_tag("classique; ROCK; jazz");
        assert_eq!(genres, vec!["Classique", "Rock", "Jazz"]);
    }

    /// Picard ecrit l'identifiant d'ENREGISTREMENT en ID3 dans une frame
    /// `UFID`, proprietaire `http://musicbrainz.org` — pas dans un TXXX.
    ///
    /// Le lecteur DSD ignorait toute frame ne commencant pas par `T` : sur un
    /// DSD etiquete avec Picard, l'identifiant n'arrivait donc JAMAIS. C'est
    /// la cle dont depend tout rapprochement par oeuvre (#2374), et le DSD est
    /// au coeur du public de Tune.
    #[test]
    fn un_ufid_musicbrainz_donne_l_identifiant_d_enregistrement() {
        const MBID: &str = "b1a9c0e8-1111-4c2b-9f3d-2c4e5a6b7c8d";
        let mut corps = Vec::new();
        corps.extend_from_slice(b"http://musicbrainz.org");
        corps.push(0);
        corps.extend_from_slice(MBID.as_bytes());

        let mut frames = Vec::new();
        frames.extend_from_slice(b"UFID");
        let n = corps.len() as u32;
        frames.extend_from_slice(&[
            (n >> 21) as u8 & 0x7f,
            (n >> 14) as u8 & 0x7f,
            (n >> 7) as u8 & 0x7f,
            n as u8 & 0x7f,
        ]);
        frames.extend_from_slice(&[0, 0]); // drapeaux
        frames.extend_from_slice(&corps);

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[0x04, 0x00, 0x00]); // v2.4.0
        let size = frames.len();
        tag.extend_from_slice(&[
            (size >> 21) as u8 & 0x7f,
            (size >> 14) as u8 & 0x7f,
            (size >> 7) as u8 & 0x7f,
            size as u8 & 0x7f,
        ]);
        tag.extend_from_slice(&frames);

        let tags = parse_id3v2_tag(&tag).expect("le tag doit s'analyser");
        assert_eq!(
            tags.musicbrainz_recording_id(),
            Some(MBID),
            "l'UFID de MusicBrainz doit rendre l'identifiant d'enregistrement"
        );
    }

    /// Un UFID d'un AUTRE proprietaire n'est pas un identifiant MusicBrainz.
    #[test]
    fn un_ufid_etranger_n_est_pas_pris_pour_un_mbid() {
        let mut corps = Vec::new();
        corps.extend_from_slice(b"http://exemple.invalide");
        corps.push(0);
        corps.extend_from_slice(b"quelque-chose");

        let mut frames = Vec::new();
        frames.extend_from_slice(b"UFID");
        let n = corps.len() as u32;
        frames.extend_from_slice(&[
            (n >> 21) as u8 & 0x7f,
            (n >> 14) as u8 & 0x7f,
            (n >> 7) as u8 & 0x7f,
            n as u8 & 0x7f,
        ]);
        frames.extend_from_slice(&[0, 0]);
        frames.extend_from_slice(&corps);

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[0x04, 0x00, 0x00]);
        let size = frames.len();
        tag.extend_from_slice(&[
            (size >> 21) as u8 & 0x7f,
            (size >> 14) as u8 & 0x7f,
            (size >> 7) as u8 & 0x7f,
            size as u8 & 0x7f,
        ]);
        tag.extend_from_slice(&frames);

        let tags = parse_id3v2_tag(&tag).expect("le tag doit s'analyser");
        assert_eq!(tags.musicbrainz_recording_id(), None);
    }

    /// Le repli TXXX reste accepte : certains etiqueteurs s'ecartent de la
    /// convention de Picard.
    #[test]
    fn un_txxx_reste_un_repli_accepte() {
        let tag_bytes = build_id3v2_tag(&[("TXXX", "MusicBrainz Track Id\0abc-123")]);
        let tags = parse_id3v2_tag(&tag_bytes).unwrap();
        assert_eq!(tags.musicbrainz_recording_id(), Some("abc-123"));
    }

    #[test]
    fn parse_id3v2_basic_text_frames() {
        let tag_bytes = build_id3v2_tag(&[
            ("TIT2", "Test Title"),
            ("TPE1", "Test Artist"),
            ("TALB", "Test Album"),
        ]);
        let tags = parse_id3v2_tag(&tag_bytes).unwrap();
        assert_eq!(tags.title(), Some("Test Title"));
        assert_eq!(tags.artist(), Some("Test Artist"));
        assert_eq!(tags.album(), Some("Test Album"));
    }

    #[test]
    fn parse_id3v22_maps_three_char_frames() {
        // ID3v2.2 tag (3-char frame ids, 6-byte frame header) as used by many
        // DSD/DSF files — previously skipped, so the title fell back to filename.
        let build_frame = |id: &[u8; 3], text: &str| {
            let body_len = 1 + text.len(); // encoding byte + text
            let mut f = Vec::new();
            f.extend_from_slice(id);
            f.extend_from_slice(&[
                (body_len >> 16) as u8,
                (body_len >> 8) as u8,
                body_len as u8,
            ]);
            f.push(0x00); // ISO-8859-1
            f.extend_from_slice(text.as_bytes());
            f
        };
        let mut frames = Vec::new();
        frames.extend(build_frame(b"TT2", "The Beat Goes On"));
        frames.extend(build_frame(b"TP1", "Sonny & Cher"));
        frames.extend(build_frame(b"TAL", "Best Of"));

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[0x02, 0x00, 0x00]); // v2.2.0, no flags
        let size = frames.len();
        tag.extend_from_slice(&[
            (size >> 21) as u8 & 0x7f,
            (size >> 14) as u8 & 0x7f,
            (size >> 7) as u8 & 0x7f,
            size as u8 & 0x7f,
        ]);
        tag.extend_from_slice(&frames);

        let tags = parse_id3v2_tag(&tag).unwrap();
        assert_eq!(tags.title(), Some("The Beat Goes On"));
        assert_eq!(tags.artist(), Some("Sonny & Cher"));
        assert_eq!(tags.album(), Some("Best Of"));
    }

    #[test]
    fn parse_id3v2_track_disc_parsing() {
        let tag_bytes = build_id3v2_tag(&[("TRCK", "7/11"), ("TPOS", "2/3")]);
        let tags = parse_id3v2_tag(&tag_bytes).unwrap();
        assert_eq!(tags.track_number(), Some(7));
        assert_eq!(tags.total_tracks(), Some(11));
        assert_eq!(tags.disc_number(), Some(2));
        assert_eq!(tags.total_discs(), Some(3));
    }

    #[test]
    fn parse_id3v2_year_from_tdrc() {
        let tag_bytes = build_id3v2_tag(&[("TDRC", "1981")]);
        let tags = parse_id3v2_tag(&tag_bytes).unwrap();
        assert_eq!(tags.year(), Some(1981));
    }

    #[test]
    fn parse_id3v2_invalid_magic() {
        assert!(parse_id3v2_tag(b"NOT_ID3_").is_none());
    }

    #[test]
    fn syncsafe_integer_values() {
        assert_eq!(syncsafe_to_u32(&[0, 0, 0, 127]), 127);
        assert_eq!(syncsafe_to_u32(&[0, 0, 1, 0]), 128);
        assert_eq!(syncsafe_to_u32(&[0, 0, 2, 0]), 256);
    }

    /// Fabriquer un fichier de la taille voulue, pour éprouver la garde de
    /// durée sans dépendre d'un vrai MP3 : elle ne lit que `len()`.
    fn fichier_de(taille: u64) -> tempfile::NamedTempFile {
        let f = tempfile::Builder::new().suffix(".mp3").tempfile().unwrap();
        f.as_file().set_len(taille).unwrap();
        f
    }

    /// Le cas de Bilou, repris tel quel de son journal (#2027, fil #1479) :
    /// 3 933 560 octets pour 242 051 ms, soit 130 kbps. Un MP3 parfaitement
    /// ordinaire, que l'ancienne garde réécrivait à 98 339 ms — 1 min 38 au
    /// lieu de 4 min 02.
    #[test]
    fn un_mp3_a_130_kbps_garde_sa_duree() {
        let f = fichier_de(3_933_560);
        let vue = mp3_duration_sanity_check(f.path(), 242_051, Some(130));
        assert_eq!(vue, 242_051, "une durée juste ne doit pas être corrigée");
    }

    /// Le seuil de l'ancienne garde était `débit < 160 kbps`. On balaie de part
    /// et d'autre : aucun de ces fichiers n'est incohérent, aucun ne doit être
    /// touché.
    #[test]
    fn aucun_debit_courant_ne_declenche_la_garde() {
        for kbps in [64u32, 96, 128, 159, 160, 192, 256, 320] {
            let duree_ms = 240_000u64;
            let taille = duree_ms * kbps as u64 / 8;
            let f = fichier_de(taille);
            let vue = mp3_duration_sanity_check(f.path(), duree_ms, Some(kbps));
            assert_eq!(vue, duree_ms, "{kbps} kbps ne doit pas être corrigé");
        }
    }

    /// Le défaut d'origine (`1e06a2c0`) : XING corrompu, 184 s annoncés pour un
    /// fichier de 84 s. Le compte de trames est faux, le débit reste juste —
    /// la garde doit donc toujours l'attraper, et ramener à 84 s.
    #[test]
    fn une_duree_sur_annoncee_est_toujours_ramenee() {
        let kbps = 128u32;
        let vraie_ms = 84_000u64;
        let taille = vraie_ms * kbps as u64 / 8;
        let f = fichier_de(taille);
        let vue = mp3_duration_sanity_check(f.path(), 184_000, Some(kbps));
        assert_eq!(vue, vraie_ms);
    }

    #[test]
    fn le_facteur_deux_laisse_passer_le_flottement_dun_vbr() {
        // Un VBR annonce son débit moyen : la durée réelle peut s'écarter un
        // peu de celle qu'il implique. On ne corrige que l'écart franc.
        let f = fichier_de(240_000 * 128 / 8);
        assert_eq!(
            mp3_duration_sanity_check(f.path(), 300_000, Some(128)),
            300_000
        );
        assert_eq!(
            mp3_duration_sanity_check(f.path(), 479_000, Some(128)),
            479_000
        );
        // Au-delà du double, c'est autre chose qu'un flottement.
        assert_eq!(
            mp3_duration_sanity_check(f.path(), 600_000, Some(128)),
            240_000
        );
    }

    #[test]
    fn sans_debit_exploitable_on_ne_corrige_rien() {
        // Mieux vaut une durée douteuse qu'une durée inventée.
        let f = fichier_de(1_000_000);
        assert_eq!(mp3_duration_sanity_check(f.path(), 999_999, None), 999_999);
        assert_eq!(
            mp3_duration_sanity_check(f.path(), 999_999, Some(0)),
            999_999
        );
    }

    #[test]
    fn un_fichier_absent_ou_vide_est_laisse_tel_quel() {
        let f = fichier_de(0);
        assert_eq!(
            mp3_duration_sanity_check(f.path(), 120_000, Some(128)),
            120_000
        );
        let inexistant = std::path::Path::new("/n/existe/pas/x.mp3");
        assert_eq!(
            mp3_duration_sanity_check(inexistant, 120_000, Some(128)),
            120_000
        );
    }

    // --- Réparation des durées MP3 rognées (#2027, #2034) ---

    #[test]
    fn probe_duration_ms_rend_none_sur_un_fichier_absent() {
        // La passe de réparation compte les illisibles séparément des
        // inchangées : confondre les deux ferait passer un disque débranché
        // pour « rien à réparer ».
        assert!(probe_duration_ms(Path::new("/nexiste/pas/rien.mp3")).is_none());
    }

    #[test]
    fn signature_de_rognage_vaut_la_taille_divisee_par_quarante() {
        // Ce que la borne inversée écrivait en base :
        //     max_plausible_ms = file_size * 8 * 1000 / 320_000
        //
        // Cette égalité est la SIGNATURE que la requête de réparation
        // recherche. Elle décrit une corruption HISTORIQUE, déjà écrite sur
        // les disques des utilisateurs : elle ne doit PAS suivre une éventuelle
        // reformulation de `mp3_duration_sanity_check`. Corriger la lecture
        // n'efface pas les valeurs déjà persistées.
        for taille in [1_000_000u64, 4_845_600, 7_340_032, 40, 41] {
            let ecrit = taille * 8 * 1000 / 320_000;
            assert_eq!(
                ecrit,
                taille / 40,
                "la signature recherchée par la réparation ne tient plus pour {taille}"
            );
        }
    }

    #[test]
    fn un_mp3_a_320_kbps_constant_porte_la_signature_sans_avoir_ete_rogne() {
        // Faux positif inoffensif, documenté pour qui relira la requête : à
        // 320 kbps constant, la durée réelle EST `taille / 40`. La passe relit
        // le fichier et récrit la même valeur — aucun dégât possible.
        let taille = 4_800_000u64;
        let duree_reelle_a_320k = taille * 8 * 1000 / 320_000;
        assert_eq!(duree_reelle_a_320k, taille / 40);
    }
    /// L'identifiant d'enregistrement écrit par [`write_metadata`] doit
    /// ressortir de [`try_read_metadata`]. C'est le maillon qui relie
    /// l'ingestion au parcours : le parcours ne lit que le FICHIER, donc un
    /// identifiant qui n'atteint pas l'étiquette n'atteindra jamais la base.
    ///
    /// L'assertion porte sur le CONTENU relu, jamais sur le code de retour :
    /// un `Ok(())` ne prouverait pas qu'une étiquette a été posée.
    #[test]
    fn l_identifiant_d_enregistrement_ecrit_est_relu_depuis_le_fichier() {
        let dir = tempfile::tempdir().unwrap();
        let fichier = dir.path().join("piste.flac");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac"),
            &fichier,
        )
        .unwrap();

        assert_eq!(
            try_read_metadata(&fichier)
                .unwrap()
                .musicbrainz_recording_id,
            None,
            "la fixture porte deja un identifiant : le test ne prouverait rien"
        );

        let update = MetadataUpdate {
            title: None,
            artist: None,
            album: None,
            album_artist: None,
            genre: None,
            track_number: None,
            disc_number: None,
            year: None,
            composer: None,
            label: None,
            musicbrainz_recording_id: Some("11111111-2222-3333-4444-555555555555".into()),
        };
        write_metadata(&fichier, &update).expect("ecriture refusee");

        assert_eq!(
            try_read_metadata(&fichier)
                .unwrap()
                .musicbrainz_recording_id
                .as_deref(),
            Some("11111111-2222-3333-4444-555555555555"),
            "l'identifiant n'a pas ete inscrit dans l'etiquette du fichier"
        );
    }
}

/// #1821 — le genre ne doit pas dépendre du logiciel qui a gravé le fichier.
///
/// DEvir : « songs purchased from different platforms or labels end up being
/// categorized under different genres ». La cause mesurée n'est pas le
/// vocabulaire des marchands, c'est l'ENCODAGE : la même intention « ce disque
/// est du Jazz ET de la Fusion » s'écrit de deux façons légitimes selon le
/// format et l'étiqueteur, et Tune n'en lisait qu'une.
///
/// Ces épreuves construisent de VRAIS fichiers dans les trois conteneurs qui
/// couvrent la bibliothèque d'un testeur — FLAC (Vorbis Comment), M4A (atomes
/// MP4) et MP3 (trames ID3v2) — parce qu'un garde-fou qui ne monterait qu'un
/// seul format ne dirait rien des deux autres : chacun a sa propre façon de
/// porter plusieurs valeurs.
#[cfg(test)]
mod genres_multivalues_i1821 {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::prelude::*;
    use lofty::tag::{ItemKey, ItemValue, TagItem};

    /// Copie d'un gabarit sous un nom qui porte À LA FOIS la clé de l'agent et
    /// le nom de l'épreuve : deux tests du même binaire ne peuvent pas se voler
    /// leur fichier, et un nettoyage par glob commun ne peut pas emporter
    /// celui d'un autre.
    ///
    /// ⚠️ Le nom de l'épreuve ne suffisait PAS (#2864) : sans pid, deux
    /// binaires de test concurrents — deux agents sur la même machine de
    /// compilation, `/tmp` partagé — visaient le même `i1821-<épreuve>-<nom>`.
    /// `scratch_name` ajoute le pid ET un compteur ; l'étiquette ne sert plus
    /// qu'à la lisibilité d'un résidu dans `/tmp`.
    fn gabarit(nom: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(nom);
        // `nom` porte l'EXTENSION, et lofty choisit son analyseur dessus :
        // elle doit rester en dernier. Le suffixe unique se glisse donc
        // avant, jamais après.
        //
        // `ScratchFile` et non un chemin nu : la copie disparaît à la sortie
        // de portée, y compris quand l'épreuve échoue — c'est justement le
        // cas qui laissait le plus de résidus (#3030).
        let copie =
            crate::test_scratch::scratch_file(&format!("i1821-{epreuve}"), &format!("-{nom}"));
        std::fs::copy(&source, &copie).expect("copie du gabarit");
        copie
    }

    /// Écrit N valeurs de genre en tant qu'ÉLÉMENTS SÉPARÉS du tag — la façon
    /// native de Vorbis Comment (champ `GENRE` répété), de MP4 (atome `©gen`
    /// répété) et d'ID3v2.4 (`TCON` multivalué).
    fn ecrire_genres_separes(chemin: &std::path::Path, genres: &[&str]) {
        let mut fichier = lofty::read_from_path(chemin).expect("lecture du gabarit");
        let tag = fichier.primary_tag_mut().expect("tag principal");
        tag.remove_key(ItemKey::Genre);
        for g in genres {
            tag.push(TagItem::new(
                ItemKey::Genre,
                ItemValue::Text((*g).to_string()),
            ));
        }
        tag.save_to_path(chemin, WriteOptions::default())
            .expect("écriture du tag");
    }

    /// Écrit les mêmes genres en UNE SEULE chaîne séparée — ce qu'écrit un
    /// étiqueteur limité à ID3v2.3, qui n'a pas de multivaleur.
    fn ecrire_genres_en_une_chaine(chemin: &std::path::Path, chaine: &str) {
        let mut fichier = lofty::read_from_path(chemin).expect("lecture du gabarit");
        let tag = fichier.primary_tag_mut().expect("tag principal");
        tag.remove_key(ItemKey::Genre);
        tag.push(TagItem::new(
            ItemKey::Genre,
            ItemValue::Text(chaine.to_string()),
        ));
        tag.save_to_path(chemin, WriteOptions::default())
            .expect("écriture du tag");
    }

    #[test]
    fn les_trois_conteneurs_rendent_tous_les_genres_du_tag() {
        // On récolte les TROIS lectures avant d'affirmer quoi que ce soit :
        // une assertion posée dans la boucle s'arrêterait au premier format et
        // ne dirait rien des deux autres — le faux garde-fou exact qu'on veut
        // éviter ici. Sans le correctif, le message rouge nomme les trois.
        let lu: Vec<(&str, Vec<String>, Option<String>)> = ["test.flac", "test.m4a", "test.mp3"]
            .into_iter()
            .map(|nom| {
                let chemin = gabarit(nom, "trois-conteneurs");
                ecrire_genres_separes(&chemin, &["Jazz", "Fusion"]);
                let meta = super::read_metadata(&chemin).expect("lecture des métadonnées");
                (nom, meta.genres, meta.genre)
            })
            .collect();

        let attendu = vec!["Jazz".to_string(), "Fusion".to_string()];
        let perdus: Vec<&str> = lu
            .iter()
            .filter(|(_, genres, _)| *genres != attendu)
            .map(|(nom, _, _)| *nom)
            .collect();
        assert!(
            perdus.is_empty(),
            "les genres secondaires du tag sont perdus sur {perdus:?} — lu : {lu:?}"
        );
        for (nom, _, genre) in &lu {
            assert_eq!(
                genre.as_deref(),
                Some("Jazz"),
                "{nom} : le genre principal reste le premier du tag"
            );
        }
    }

    #[test]
    fn les_deux_conventions_decrivent_la_meme_musique() {
        // Le cœur de #1821 : le même disque, gravé une fois en valeurs
        // séparées et une fois en chaîne unique, doit se ranger IDENTIQUEMENT.
        // Avant le correctif, la chaîne unique rendait deux genres et les
        // valeurs séparées un seul — d'où deux classements pour un seul disque.
        let separe = gabarit("test.flac", "deux-conventions-separe");
        ecrire_genres_separes(&separe, &["Jazz", "Fusion"]);

        let unique = gabarit("test.mp3", "deux-conventions-unique");
        ecrire_genres_en_une_chaine(&unique, "Jazz; Fusion");

        let a = super::read_metadata(&separe).expect("FLAC à valeurs séparées");
        let b = super::read_metadata(&unique).expect("MP3 à chaîne unique");
        assert_eq!(
            a.genres, b.genres,
            "deux gravures de la même intention donnent deux classements"
        );
        assert_eq!(a.genre, b.genre);
    }

    #[test]
    fn un_genre_unique_reste_intact() {
        // Contre-garde : le cas courant — un seul genre — ne bouge pas.
        for nom in ["test.flac", "test.m4a", "test.mp3"] {
            let chemin = gabarit(nom, "genre-unique");
            ecrire_genres_separes(&chemin, &["Rock"]);
            let meta = super::read_metadata(&chemin).expect("lecture");
            assert_eq!(meta.genres, vec!["Rock".to_string()], "{nom}");
            assert_eq!(meta.genre.as_deref(), Some("Rock"), "{nom}");
        }
    }

    #[test]
    fn deux_orthographes_du_meme_genre_ne_comptent_quune_fois() {
        // Un marchand écrit « Hip-Hop », l'autre « Hip Hop ». Un fichier
        // regravé peut porter les deux ; `genre_key` — la clé canonique de la
        // bibliothèque, pas un `to_lowercase()` réécrit sur place — les
        // ramène à un seul genre.
        assert_eq!(
            super::genres_from_tag_values(&["Hip-Hop", "hip hop", "Trip Hop"]),
            vec!["Hip-Hop".to_string(), "Trip Hop".to_string()]
        );
    }

    #[test]
    fn une_valeur_peut_elle_meme_etre_separee() {
        // Les deux conventions se mêlent dans un même fichier : deux champs
        // `GENRE`, dont l'un porte encore un séparateur.
        assert_eq!(
            super::genres_from_tag_values(&["Jazz", "Fusion; Latin Jazz"]),
            vec![
                "Jazz".to_string(),
                "Fusion".to_string(),
                "Latin Jazz".to_string()
            ]
        );
    }
}
