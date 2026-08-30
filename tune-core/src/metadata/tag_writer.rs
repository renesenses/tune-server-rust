//! Lecture et écriture des étiquettes d'un fichier audio.
//!
//! ⚠️ **Tout chemin qui entre ici vient de `tracks.file_path`**, donc du
//! scanner, donc normalisé en **NFC**. Le disque, lui, peut tenir le nom en
//! NFD (macOS, SMB/CIFS) ou en graphie mixte : `lofty::read_from_path` rend
//! alors `ENOENT` sur un fichier bel et bien présent (#1865). Aucune fonction
//! de ce module ne doit donner à `lofty` — ni à `exists()` — la chaîne lue en
//! base : elle passe d'abord par [`graphie_sur_disque`].
//!
//! Mesure sur `.18` le 30/08/2026 : **147 pistes sur 46 877** ont un chemin
//! stocké qui ne désigne aucun fichier tel quel (135 retrouvées en NFD global,
//! 12 seulement par le parcours composant par composant de #1837). Pour ces
//! 147, `read_tags` rendait « lofty read: … », `write_tags` rendait « file not
//! found », et la passe de paroles les comptait en `skipped_no_body` — alors
//! que le corps existait et que le fichier était là.
use std::collections::HashMap;
use std::path::Path;

use crate::library::local_path::{LocalPath, resolve_local_path};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::*;
use lofty::tag::ItemKey;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TagUpdate {
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub year: Option<i32>,
    pub comment: Option<String>,
    pub isrc: Option<String>,
    pub bpm: Option<i32>,
    pub label: Option<String>,
    pub lyrics: Option<String>,
}

impl TagUpdate {
    fn sanitized(&self) -> (Self, Vec<super::TextCorrection>) {
        fn clean(
            field: &str,
            value: &mut Option<String>,
            corrections: &mut Vec<super::TextCorrection>,
        ) {
            let Some(raw) = value.as_deref() else {
                return;
            };
            let (sanitized, mut found) = super::sanitize_untrusted_single_line_text(raw, field);
            if found.is_empty() {
                return;
            }
            *value = (!sanitized.is_empty()).then_some(sanitized);
            corrections.append(&mut found);
        }

        let mut update = self.clone();
        let mut corrections = Vec::new();
        clean("title", &mut update.title, &mut corrections);
        clean("artist_name", &mut update.artist_name, &mut corrections);
        clean("album_title", &mut update.album_title, &mut corrections);
        clean("genre", &mut update.genre, &mut corrections);
        clean("composer", &mut update.composer, &mut corrections);
        clean("isrc", &mut update.isrc, &mut corrections);
        clean("label", &mut update.label, &mut corrections);
        for (field, value) in [
            ("comment", &mut update.comment),
            ("lyrics", &mut update.lyrics),
        ] {
            let Some(raw) = value.as_deref() else {
                continue;
            };
            let (sanitized, mut found) = super::sanitize_untrusted_text(raw, field);
            if !found.is_empty() {
                *value = (!sanitized.is_empty()).then_some(sanitized);
                corrections.append(&mut found);
            }
        }
        (update, corrections)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagFormat {
    Id3,
    Vorbis,
    Mp4,
    Unknown,
}

pub fn detect_format(file_path: &str) -> TagFormat {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "mp3" => TagFormat::Id3,
        "flac" | "ogg" | "oga" | "opus" => TagFormat::Vorbis,
        "m4a" | "aac" | "alac" | "mp4" => TagFormat::Mp4,
        "aiff" | "aif" => TagFormat::Id3,
        _ => TagFormat::Unknown,
    }
}

/// La graphie que le système de fichiers accepte pour un chemin lu en base.
///
/// Remplace le `Path::new(file_path).exists()` qui gardait autrefois chaque
/// entrée du module : il répondait faux sur les 147 pistes de `.18` dont le
/// nom est décomposé sur le disque, et le module rendait « file not found »
/// pour un fichier présent (#1865).
///
/// Rend la chaîne **telle que le disque l'a reconnue** — jamais une forme
/// normalisée par nos soins. C'est elle qu'il faut donner à `lofty`, aussi
/// bien en lecture qu'en écriture : lofty réécrit le fichier qu'il a ouvert,
/// donc ouvrir la bonne graphie, c'est écrire dans le bon fichier.
///
/// Le message d'erreur reste `file not found`, mot pour mot : les routes le
/// remontent tel quel et un client peut le comparer.
fn graphie_sur_disque(file_path: &str) -> Result<String, String> {
    match resolve_local_path(file_path) {
        LocalPath::Found(reel) => Ok(reel),
        LocalPath::Missing => Err("file not found".into()),
    }
}

pub async fn write_tags(file_path: &str, update: &TagUpdate) -> Result<WriteResult, String> {
    let format = detect_format(file_path);
    if format == TagFormat::Unknown {
        return Err("unsupported tag format".into());
    }
    let path = graphie_sur_disque(file_path)?;
    let (update, corrections) = update.sanitized();
    if !corrections.is_empty() {
        tracing::warn!(
            path = file_path,
            corrections = ?corrections,
            "tag_writer_unsafe_text_sanitized"
        );
    }
    tokio::task::spawn_blocking(move || write_tags_lofty(&path, &update))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn write_tags_lofty(file_path: &str, update: &TagUpdate) -> Result<WriteResult, String> {
    let mut tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    let tag_type = tagged.primary_tag_type();

    if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
        tagged.insert_tag(lofty::tag::Tag::new(tag_type));
    }

    let has_primary = tagged.primary_tag().is_some();
    let tag = if has_primary {
        tagged.primary_tag_mut().unwrap()
    } else {
        tagged.first_tag_mut().unwrap()
    };

    let mut count = 0usize;
    if let Some(ref v) = update.title {
        tag.set_title(v.clone());
        count += 1;
    }
    if let Some(ref v) = update.artist_name {
        tag.set_artist(v.clone());
        count += 1;
    }
    if let Some(ref v) = update.album_title {
        tag.set_album(v.clone());
        count += 1;
    }
    if let Some(v) = update.track_number {
        tag.set_track(v as u32);
        count += 1;
    }
    if let Some(v) = update.disc_number {
        tag.set_disk(v as u32);
        count += 1;
    }
    if let Some(ref v) = update.genre {
        tag.set_genre(v.clone());
        count += 1;
    }
    if let Some(v) = update.year {
        tag.set_date(lofty::tag::items::Timestamp {
            year: v as u16,
            ..Default::default()
        });
        count += 1;
    }
    if let Some(ref v) = update.comment {
        tag.set_comment(v.clone());
        count += 1;
    }
    if let Some(ref v) = update.composer {
        tag.insert(lofty::tag::TagItem::new(
            ItemKey::Composer,
            lofty::tag::ItemValue::Text(v.clone()),
        ));
        count += 1;
    }
    if let Some(ref v) = update.label {
        tag.insert(lofty::tag::TagItem::new(
            ItemKey::Label,
            lofty::tag::ItemValue::Text(v.clone()),
        ));
        count += 1;
    }
    if let Some(ref v) = update.isrc {
        tag.insert(lofty::tag::TagItem::new(
            ItemKey::Isrc,
            lofty::tag::ItemValue::Text(v.clone()),
        ));
        count += 1;
    }
    if let Some(v) = update.bpm {
        tag.insert(lofty::tag::TagItem::new(
            ItemKey::Bpm,
            lofty::tag::ItemValue::Text(v.to_string()),
        ));
        count += 1;
    }
    if let Some(ref v) = update.lyrics {
        tag.insert(lofty::tag::TagItem::new(
            ItemKey::Lyrics,
            lofty::tag::ItemValue::Text(v.clone()),
        ));
        count += 1;
    }

    if count == 0 {
        return Ok(WriteResult {
            file_path: file_path.into(),
            fields_written: 0,
        });
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file_path)
        .map_err(|e| format!("open: {e}"))?;
    tagged
        .save_to(&mut file, WriteOptions::default())
        .map_err(|e| format!("lofty save: {e}"))?;

    info!(file = file_path, fields = count, "tags_written_lofty");
    Ok(WriteResult {
        file_path: file_path.into(),
        fields_written: count,
    })
}

pub async fn read_tags(file_path: &str) -> Result<HashMap<String, String>, String> {
    let path = graphie_sur_disque(file_path)?;
    tokio::task::spawn_blocking(move || read_tags_lofty(&path))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn read_tags_lofty(file_path: &str) -> Result<HashMap<String, String>, String> {
    let tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    let mut tags = HashMap::new();
    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => return Ok(tags),
    };
    if let Some(v) = tag.title() {
        tags.insert("title".into(), v.to_string());
    }
    if let Some(v) = tag.artist() {
        tags.insert("artist".into(), v.to_string());
    }
    if let Some(v) = tag.album() {
        tags.insert("album".into(), v.to_string());
    }
    if let Some(v) = tag.genre() {
        tags.insert("genre".into(), v.to_string());
    }
    if let Some(v) = tag.date() {
        tags.insert("date".into(), v.to_string());
    }
    if let Some(v) = tag.track() {
        tags.insert("tracknumber".into(), v.to_string());
    }
    if let Some(v) = tag.disk() {
        tags.insert("discnumber".into(), v.to_string());
    }
    if let Some(v) = tag.comment() {
        tags.insert("comment".into(), v.to_string());
    }
    debug!(file = file_path, count = tags.len(), "tags_read_lofty");
    Ok(tags)
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteResult {
    pub file_path: String,
    pub fields_written: usize,
}

// --- Extended metadata writing (HashMap-based) ---

/// Map a Tune metadata field name to the corresponding lofty `ItemKey`.
/// These keys match the ones used in `read_extended_metadata`.
fn tune_key_to_lofty(key: &str) -> Option<ItemKey> {
    match key {
        // Credits / personnel
        "composer" => Some(ItemKey::Composer),
        "conductor" => Some(ItemKey::Conductor),
        "lyricist" => Some(ItemKey::Lyricist),
        "performer" => Some(ItemKey::Performer),
        "remixer" => Some(ItemKey::Remixer),
        "label" => Some(ItemKey::Label),
        "producer" => Some(ItemKey::Producer),

        // Descriptive
        "bpm" => Some(ItemKey::Bpm),
        "mood" => Some(ItemKey::Mood),
        "comment" => Some(ItemKey::Comment),
        "lyrics" => Some(ItemKey::Lyrics),
        "grouping" => Some(ItemKey::ContentGroup),
        "compilation" => Some(ItemKey::FlagCompilation),

        // Identifiers
        "isrc" => Some(ItemKey::Isrc),
        "barcode" => Some(ItemKey::Barcode),
        "catalog_number" => Some(ItemKey::CatalogNumber),
        "media_type" => Some(ItemKey::OriginalMediaType),
        "release_country" => Some(ItemKey::ReleaseCountry),

        // Dates
        "release_date" => Some(ItemKey::ReleaseDate),
        "original_date" => Some(ItemKey::OriginalReleaseDate),

        // Technical
        "copyright" => Some(ItemKey::CopyrightMessage),
        "language" => Some(ItemKey::Language),
        "encoder" => Some(ItemKey::EncodedBy),
        // ENCODER (Vorbis) — encoding software, distinct from `encoder`/ENCODEDBY.
        "encoder_software" => Some(ItemKey::EncoderSoftware),

        // Sort order
        "sort_artist" => Some(ItemKey::TrackArtistSortOrder),
        "sort_album" => Some(ItemKey::AlbumTitleSortOrder),
        "sort_album_artist" => Some(ItemKey::AlbumArtistSortOrder),

        // Core fields (album_artist written via ItemKey)
        "album_artist" => Some(ItemKey::AlbumArtist),

        // MusicBrainz IDs
        "mb_track_id" => Some(ItemKey::MusicBrainzRecordingId),
        "mb_release_id" => Some(ItemKey::MusicBrainzReleaseId),
        "mb_artist_id" => Some(ItemKey::MusicBrainzArtistId),
        "mb_release_artist_id" => Some(ItemKey::MusicBrainzReleaseArtistId),
        "mb_release_group_id" => Some(ItemKey::MusicBrainzReleaseGroupId),
        "mb_release_track_id" => Some(ItemKey::MusicBrainzTrackId),
        "mb_work_id" => Some(ItemKey::MusicBrainzWorkId),

        // ReplayGain (read-only typically, but allow writing)
        "rg_track_gain" => Some(ItemKey::ReplayGainTrackGain),
        "rg_track_peak" => Some(ItemKey::ReplayGainTrackPeak),
        "rg_album_gain" => Some(ItemKey::ReplayGainAlbumGain),
        "rg_album_peak" => Some(ItemKey::ReplayGainAlbumPeak),

        _ => None,
    }
}

/// Returns true if the file extension is not supported for tag writing.
pub(crate) fn is_unsupported_format(file_path: &str) -> bool {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    // DFF has no standard tag support
    matches!(ext.as_str(), "dff")
}

/// Write extended metadata fields to an audio file's tags.
///
/// For each key in `fields`:
/// - If the value is empty, the corresponding tag item is removed.
/// - If the value is non-empty, the tag item is inserted/replaced.
///
/// Skips unsupported formats (DFF) and missing files gracefully.
pub async fn write_metadata_to_file(
    file_path: &str,
    fields: &HashMap<String, String>,
) -> Result<WriteResult, String> {
    if is_unsupported_format(file_path) {
        return Err("unsupported format for tag writing".into());
    }
    let path = graphie_sur_disque(file_path)?;
    let fields = fields.clone();
    tokio::task::spawn_blocking(move || write_metadata_to_file_sync(&path, &fields))
        .await
        .map_err(|e| format!("join: {e}"))?
}

/// ⚠️ Point d'entrée **appelé directement** par la passe de paroles
/// (`library::lyrics_pass::write_one_tag`), qui ne passe donc PAS par
/// [`write_metadata_to_file`] : la résolution de graphie doit être ici, sinon
/// la moitié des appelants reste aveugle au NFD (#1865).
pub(crate) fn write_metadata_to_file_sync(
    file_path: &str,
    fields: &HashMap<String, String>,
) -> Result<WriteResult, String> {
    let file_path = &graphie_sur_disque(file_path)?;
    let mut tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    let tag_type = tagged.primary_tag_type();

    // Ensure we have a tag to write to
    if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
        tagged.insert_tag(lofty::tag::Tag::new(tag_type));
    }

    let has_primary = tagged.primary_tag().is_some();
    let tag = if has_primary {
        tagged.primary_tag_mut().unwrap()
    } else {
        tagged.first_tag_mut().unwrap()
    };

    let mut count = 0usize;
    for (key, value) in fields {
        let Some(item_key) = tune_key_to_lofty(key) else {
            debug!(key = key.as_str(), "tag_writer_unknown_key_skipped");
            continue;
        };

        if value.is_empty() {
            // Remove the tag item
            tag.remove_key(item_key);
        } else {
            // Insert/replace the tag item
            tag.insert_text(item_key, value.clone());
        }
        count += 1;
    }

    if count == 0 {
        return Ok(WriteResult {
            file_path: file_path.into(),
            fields_written: 0,
        });
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file_path)
        .map_err(|e| format!("open: {e}"))?;
    tagged
        .save_to(&mut file, WriteOptions::default())
        .map_err(|e| format!("lofty save: {e}"))?;

    info!(file = file_path, fields = count, "extended_tags_written");
    Ok(WriteResult {
        file_path: file_path.into(),
        fields_written: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_mp3() {
        assert_eq!(detect_format("/music/song.mp3"), TagFormat::Id3);
    }
    #[test]
    fn detect_flac() {
        assert_eq!(detect_format("/music/song.flac"), TagFormat::Vorbis);
    }
    #[test]
    fn detect_m4a() {
        assert_eq!(detect_format("/music/song.m4a"), TagFormat::Mp4);
    }
    #[test]
    fn detect_unknown() {
        assert_eq!(detect_format("/music/song.xyz"), TagFormat::Unknown);
    }

    #[test]
    fn tag_writer_sanitizes_every_text_field_before_the_file_boundary() {
        let dirty = TagUpdate {
            title: Some("A\0B".into()),
            artist_name: Some("Lisa\u{feff}Strings".into()),
            lyrics: Some("line one\nline two".into()),
            ..Default::default()
        };
        let (clean, corrections) = dirty.sanitized();
        assert_eq!(clean.title.as_deref(), Some("A B"));
        assert_eq!(clean.artist_name.as_deref(), Some("Lisa Strings"));
        assert_eq!(clean.lyrics.as_deref(), Some("line one\nline two"));
        assert_eq!(corrections.len(), 2);
    }

    #[test]
    fn tune_key_mapping_covers_all_extended_fields() {
        // Verify all keys from read_extended_metadata have a mapping
        let keys = [
            "composer",
            "conductor",
            "lyricist",
            "performer",
            "remixer",
            "label",
            "producer",
            "bpm",
            "mood",
            "comment",
            "lyrics",
            "grouping",
            "compilation",
            "isrc",
            "barcode",
            "catalog_number",
            "media_type",
            "release_date",
            "original_date",
            "copyright",
            "language",
            "encoder",
            "encoder_software",
            "release_country",
            "sort_artist",
            "sort_album",
            "sort_album_artist",
            "album_artist",
            "mb_track_id",
            "mb_release_id",
            "mb_artist_id",
            "mb_release_artist_id",
            "mb_release_group_id",
            "mb_release_track_id",
            "mb_work_id",
            // Note: `source_media` (Vorbis SOURCE) is intentionally read-only —
            // it has no lofty ItemKey and is read via raw_vorbis_field.
            // Same for `dr_album` / `dr_track` (Vorbis DYNAMIC RANGE): a measured
            // value produced by an external analyser, not something Tune should
            // let a user overwrite from the tag editor.
            "rg_track_gain",
            "rg_track_peak",
            "rg_album_gain",
            "rg_album_peak",
        ];
        for key in keys {
            assert!(
                tune_key_to_lofty(key).is_some(),
                "missing mapping for key: {key}"
            );
        }
    }

    #[test]
    fn tune_key_mapping_returns_none_for_unknown() {
        assert!(tune_key_to_lofty("unknown_field").is_none());
        assert!(tune_key_to_lofty("").is_none());
    }

    #[test]
    fn unsupported_format_dff() {
        assert!(is_unsupported_format("/music/track.dff"));
        assert!(is_unsupported_format("/music/track.DFF"));
    }

    #[test]
    fn supported_formats_not_blocked() {
        assert!(!is_unsupported_format("/music/track.flac"));
        assert!(!is_unsupported_format("/music/track.mp3"));
        assert!(!is_unsupported_format("/music/track.m4a"));
        assert!(!is_unsupported_format("/music/track.dsf"));
    }

    // ------------------------------------------------------------------
    // #1865 — le chemin vient de la base (NFC), le disque porte le NFD.
    //
    // Les deux graphies s'affichent à l'identique : écrites en clair dans le
    // source, un éditeur ou un filtre `git` pourrait les re-normaliser toutes
    // les deux et le test comparerait alors deux fois la même chaîne sans le
    // dire. D'où les échappements explicites — et l'assertion d'inégalité qui
    // suit, qui refuse de tourner si la précaution a sauté.
    // ------------------------------------------------------------------

    /// « Décollage » précomposé : `é` = U+00E9. C'est ce que la base tient.
    const EN_BASE_NFC: &str = "10. D\u{00e9}collage.flac";
    /// Le même, décomposé : `e` + U+0301. C'est ce que macOS pose sur le
    /// disque, et ce qu'un partage SMB/CIFS rend.
    const SUR_DISQUE_NFD: &str = "10. De\u{0301}collage.flac";

    fn fixture_flac() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac")
    }

    /// Le compositeur réellement inscrit dans le fichier, lu par lofty depuis
    /// la graphie du DISQUE. Passer par le contenu, et pas par le code de
    /// retour, est délibéré : un `Ok(_)` ne prouverait pas dans quel fichier
    /// on a écrit.
    ///
    /// Compositeur et non titre : `write_metadata_to_file_sync` passe par
    /// [`tune_key_to_lofty`], qui ne connaît pas `title` — un test écrivant un
    /// titre par cette porte serait vert sans rien écrire du tout.
    fn compositeur_dans(chemin: &std::path::Path) -> Option<String> {
        let tagged = lofty::read_from_path(chemin).ok()?;
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
        tag.get_string(ItemKey::Composer).map(|s| s.to_string())
    }

    #[test]
    fn les_deux_graphies_sont_bien_distinctes() {
        assert_ne!(
            EN_BASE_NFC, SUR_DISQUE_NFD,
            "les constantes ont ete re-normalisees : le test ne prouverait plus rien"
        );
    }

    #[test]
    fn ecriture_atteint_le_fichier_decompose_du_disque() {
        let dir = tempfile::tempdir().unwrap();
        let sur_disque = dir.path().join(SUR_DISQUE_NFD);
        std::fs::copy(fixture_flac(), &sur_disque).unwrap();

        // Ce que la base donnerait aux passes : la forme NFC, qui ne désigne
        // aucun fichier telle quelle.
        let en_base = dir.path().join(EN_BASE_NFC);
        assert!(
            !en_base.exists(),
            "le systeme de fichiers a replie les graphies : le cas #1865 n'est pas reproduit ici"
        );

        let champs = HashMap::from([("composer".to_string(), "Decollage".to_string())]);
        let r = write_metadata_to_file_sync(en_base.to_str().unwrap(), &champs);

        assert!(r.is_ok(), "ecriture refusee : {r:?}");
        assert_eq!(
            compositeur_dans(&sur_disque).as_deref(),
            Some("Decollage"),
            "l'etiquette n'a pas atteint le fichier reellement present sur le disque"
        );
    }

    /// Les deux portes `async` du module — `write_tags` puis `read_tags` — sur
    /// le même fichier décomposé, désigné des deux côtés par la graphie de la
    /// base. C'est le trajet exact de `PUT /metadata/tracks/{id}` suivi d'un
    /// affichage.
    #[tokio::test]
    async fn ecriture_puis_lecture_async_sur_fichier_decompose() {
        let dir = tempfile::tempdir().unwrap();
        let sur_disque = dir.path().join(SUR_DISQUE_NFD);
        std::fs::copy(fixture_flac(), &sur_disque).unwrap();

        let en_base = dir.path().join(EN_BASE_NFC);
        assert!(!en_base.exists());

        let update = TagUpdate {
            title: Some("Decollage".to_string()),
            ..Default::default()
        };
        let ecrit = write_tags(en_base.to_str().unwrap(), &update).await;
        assert!(ecrit.is_ok(), "ecriture refusee : {ecrit:?}");

        let tags = read_tags(en_base.to_str().unwrap()).await;
        assert!(tags.is_ok(), "lecture refusee : {tags:?}");
        assert_eq!(
            tags.unwrap().get("title").map(String::as_str),
            Some("Decollage")
        );
    }

    /// Témoin anti-régression : un chemin purement ASCII n'a qu'une graphie et
    /// marchait déjà. Il doit continuer — le repli ne doit rien changer là où
    /// il n'y avait rien à réparer.
    #[test]
    fn temoin_ascii_toujours_ecrit() {
        let dir = tempfile::tempdir().unwrap();
        let ascii = dir.path().join("10. Takeoff.flac");
        std::fs::copy(fixture_flac(), &ascii).unwrap();

        let champs = HashMap::from([("composer".to_string(), "Takeoff".to_string())]);
        write_metadata_to_file_sync(ascii.to_str().unwrap(), &champs).unwrap();

        assert_eq!(compositeur_dans(&ascii).as_deref(), Some("Takeoff"));
    }

    /// Et le garde-fou n'a pas été retiré au passage : un fichier qu'AUCUNE
    /// graphie ne trouve rend toujours le même « file not found », mot pour
    /// mot, que les routes remontent au client.
    #[test]
    fn absent_rend_toujours_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let nulle_part = dir.path().join("Ho\u{0300}tel de personne.flac");

        let r = write_metadata_to_file_sync(
            nulle_part.to_str().unwrap(),
            &HashMap::from([("composer".to_string(), "x".to_string())]),
        );

        assert_eq!(r.unwrap_err(), "file not found");
    }
}
