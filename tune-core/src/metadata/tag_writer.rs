//! Lecture et écriture des étiquettes d'un fichier audio.
//!
//! ⚠️ **Tout chemin qui entre ici vient de `tracks.file_path`**, donc du
//! scanner, donc normalisé en **NFC**. Le disque, lui, peut tenir le nom en
//! NFD (macOS, SMB/CIFS) ou en graphie mixte : `lofty::read_from_path` rend
//! alors `ENOENT` sur un fichier bel et bien présent (#1865). Aucune fonction
//! de ce module ne doit donner à `lofty` — ni à `exists()` — la chaîne lue en
//! base : elle passe d'abord par `graphie_sur_disque`.
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
    /// TOUS les genres de la piste, dans l'ordre (#6635) — le premier reste
    /// le genre principal. Non vide, il l'emporte sur [`Self::genre`] et
    /// chaque genre devient une VALEUR SÉPARÉE, comme pour « Écrire dans les
    /// fichiers » ([`BalisesEdition::genres`], #5314) : champ `GENRE` répété
    /// en Vorbis, `TCON` multivalué en ID3v2.4, atome `©gen` répété en MP4.
    /// Sans lui, « Graver tags » n'écrivait que `tracks.genre`, le premier
    /// genre seul : « Pop/Rock » devenait « Pop » dans le fichier.
    #[serde(default)]
    pub genres: Vec<String>,
    pub composer: Option<String>,
    pub year: Option<i32>,
    pub comment: Option<String>,
    pub isrc: Option<String>,
    pub bpm: Option<i32>,
    pub label: Option<String>,
    pub lyrics: Option<String>,
    /// Artiste d'album (`ALBUMARTIST`, `TPE2`, `aART`) — la modification en
    /// masse d'Oxygen (#7539). `None` : ne pas toucher.
    #[serde(default)]
    pub album_artist: Option<String>,
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
        update.genres = std::mem::take(&mut update.genres)
            .into_iter()
            .filter_map(|g| {
                let mut g = Some(g);
                clean("genre", &mut g, &mut corrections);
                g.map(|g| g.trim().to_string()).filter(|g| !g.is_empty())
            })
            .collect();
        clean("composer", &mut update.composer, &mut corrections);
        clean("isrc", &mut update.isrc, &mut corrections);
        clean("label", &mut update.label, &mut corrections);
        clean("album_artist", &mut update.album_artist, &mut corrections);
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
/// Motif du refus d'écrire dans un fichier rangé dans une image ISO (#5299).
pub const MOTIF_IMAGE_ISO_LECTURE_SEULE: &str =
    "fichier dans une image ISO : l'image est en lecture seule";

fn graphie_sur_disque(file_path: &str) -> Result<String, String> {
    // #5299 — une image ISO est en lecture seule : un fichier qu'elle contient
    // ne se réécrit pas. Le dire, plutôt que d'échouer plus loin sur un chemin
    // que lofty ne sait pas ouvrir.
    if crate::audio::iso9660::est_chemin_virtuel(file_path) {
        return Err(MOTIF_IMAGE_ISO_LECTURE_SEULE.into());
    }
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

/// Applique une modification au `Tag` GÉNÉRIQUE d'un fichier et l'enregistre,
/// sans perdre ce que ce `Tag` ne sait pas représenter.
///
/// 🔴 MESURÉ le 16/09/2026 : `lofty::read_from_path` → `Tag` → `save_to`
/// EFFAÇAIT tout champ Vorbis hors catalogue d'un FLAC à chaque écriture —
/// `DYNAMIC RANGE`, `ALBUM DYNAMIC RANGE`, `SOURCE`, les champs maison des
/// testeurs (Patatorz tague ses DR via Mp3tag, fil 1683). Un FLAC gravé
/// `DYNAMIC RANGE=12`, puis un compositeur écrit par `write_tags` : le DR
/// avait disparu. lofty 0.24 n'a pas d'`ItemKey::Unknown` : la conversion
/// `VorbisComments → Tag` range ces champs dans un *reste* que `From` jette.
///
/// Pour FLAC / Ogg / Opus, on passe donc par le fichier CONCRET et par le
/// couple `split_tag` / `merge_tag` que lofty prévoit exactement pour ça :
/// le reste garde les champs inconnus (et le vendor), le `Tag` reçoit ce que
/// l'appelant sait éditer, et la fusion réunit les deux. `FlacFile::save_to`
/// réenchaîne aussi les blocs PICTURE, que `VorbisComments::save_to` seul
/// retirerait. Les autres conteneurs (MP3, M4A, AIFF) gardent le chemin
/// générique : lofty y conserve les trames inconnues (TXXX, atomes libres)
/// dans le `Tag` lui-même.
///
/// `appliquer` rend le nombre de champs touchés ; à zéro, rien n'est écrit.
fn ecrire_par_tag_generique(
    file_path: &str,
    appliquer: impl FnOnce(&mut lofty::tag::Tag) -> usize,
) -> Result<usize, String> {
    ecrire_par_tag(file_path, false, appliquer)
}

/// [`ecrire_par_tag_generique`], avec une option : `promouvoir`.
///
/// Un WAV ou un AIFF qui ne porte QUE des balises secondaires (RIFF INFO,
/// blocs texte AIFF) n'a ni ALBUMARTIST, ni DISCNUMBER, ni DISCSUBTITLE, ni
/// COMPILATION : lofty y refuse ces clés sans le dire. Avec `promouvoir`,
/// l'étiquette PRIMAIRE du conteneur (ID3v2) est créée à partir de la
/// secondaire — tous ses champs et ses images recopiés — et c'est elle qui est
/// éditée ; la secondaire reste telle quelle. Le lecteur du scan lit la
/// primaire d'abord : il retrouve les anciens champs ET les nouveaux.
/// Réservé à « Écrire dans les fichiers » (tranche 4), qui VÉRIFIE ensuite
/// la copie ; les autres écrivains gardent leur comportement.
fn ecrire_par_tag(
    file_path: &str,
    promouvoir: bool,
    appliquer: impl FnOnce(&mut lofty::tag::Tag) -> usize,
) -> Result<usize, String> {
    use std::io::Seek;

    use lofty::config::ParseOptions;
    use lofty::flac::FlacFile;
    use lofty::ogg::{OpusFile, VorbisComments, VorbisFile};
    use lofty::tag::{MergeTag, SplitTag};

    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let ouvrir = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(file_path)
            .map_err(|e| format!("open: {e}"))
    };

    /// Le tour commun aux trois conteneurs Vorbis : séparer, éditer, réunir.
    fn tour(
        vc: VorbisComments,
        appliquer: impl FnOnce(&mut lofty::tag::Tag) -> usize,
    ) -> (Option<VorbisComments>, usize) {
        let (reste, mut tag) = vc.split_tag();
        let n = appliquer(&mut tag);
        if n == 0 {
            return (None, 0);
        }
        (Some(reste.merge_tag(tag)), n)
    }

    match ext.as_str() {
        "flac" => {
            let mut file = ouvrir()?;
            let mut f = FlacFile::read_from(&mut file, ParseOptions::new())
                .map_err(|e| format!("lofty read: {e}"))?;
            let vc = f
                .vorbis_comments_mut()
                .map(std::mem::take)
                .unwrap_or_default();
            let (fusion, n) = tour(vc, appliquer);
            let Some(fusion) = fusion else { return Ok(0) };
            f.set_vorbis_comments(fusion);
            file.rewind().map_err(|e| format!("rewind: {e}"))?;
            f.save_to(&mut file, WriteOptions::default())
                .map_err(|e| format!("lofty save: {e}"))?;
            Ok(n)
        }
        "opus" => {
            let mut file = ouvrir()?;
            let mut f = OpusFile::read_from(&mut file, ParseOptions::new())
                .map_err(|e| format!("lofty read: {e}"))?;
            let vc = std::mem::take(f.vorbis_comments_mut());
            let (fusion, n) = tour(vc, appliquer);
            let Some(fusion) = fusion else { return Ok(0) };
            *f.vorbis_comments_mut() = fusion;
            file.rewind().map_err(|e| format!("rewind: {e}"))?;
            f.save_to(&mut file, WriteOptions::default())
                .map_err(|e| format!("lofty save: {e}"))?;
            Ok(n)
        }
        "ogg" | "oga" => {
            let mut file = ouvrir()?;
            let mut f = VorbisFile::read_from(&mut file, ParseOptions::new())
                .map_err(|e| format!("lofty read: {e}"))?;
            let vc = std::mem::take(f.vorbis_comments_mut());
            let (fusion, n) = tour(vc, appliquer);
            let Some(fusion) = fusion else { return Ok(0) };
            *f.vorbis_comments_mut() = fusion;
            file.rewind().map_err(|e| format!("rewind: {e}"))?;
            f.save_to(&mut file, WriteOptions::default())
                .map_err(|e| format!("lofty save: {e}"))?;
            Ok(n)
        }
        _ => {
            let mut tagged =
                lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
            let tag_type = tagged.primary_tag_type();
            if tagged.primary_tag().is_none() {
                let nouvelle = match tagged.first_tag() {
                    Some(secondaire) if promouvoir => {
                        let mut primaire = lofty::tag::Tag::new(tag_type);
                        for item in secondaire.items() {
                            primaire.insert(item.clone());
                        }
                        for image in secondaire.pictures() {
                            primaire.push_picture(image.clone());
                        }
                        Some(primaire)
                    }
                    Some(_) => None,
                    None => Some(lofty::tag::Tag::new(tag_type)),
                };
                if let Some(t) = nouvelle {
                    tagged.insert_tag(t);
                }
            }
            let has_primary = tagged.primary_tag().is_some();
            let tag = if has_primary {
                tagged.primary_tag_mut().unwrap()
            } else {
                tagged.first_tag_mut().unwrap()
            };
            let n = appliquer(tag);
            if n == 0 {
                return Ok(0);
            }
            let mut file = ouvrir()?;
            tagged
                .save_to(&mut file, WriteOptions::default())
                .map_err(|e| format!("lofty save: {e}"))?;
            Ok(n)
        }
    }
}

fn write_tags_lofty(file_path: &str, update: &TagUpdate) -> Result<WriteResult, String> {
    let count = ecrire_par_tag_generique(file_path, |tag| appliquer_update(tag, update))?;
    if count > 0 {
        info!(file = file_path, fields = count, "tags_written_lofty");
    }
    Ok(WriteResult {
        file_path: file_path.into(),
        fields_written: count,
    })
}

/// Pose les champs d'un [`TagUpdate`] sur un `Tag` générique ; rend combien.
fn appliquer_update(tag: &mut lofty::tag::Tag, update: &TagUpdate) -> usize {
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
    if !update.genres.is_empty() {
        // Une valeur par genre (#6635), même pose que `poser` (#5314).
        tag.remove_key(ItemKey::Genre);
        for g in &update.genres {
            tag.push(lofty::tag::TagItem::new(
                ItemKey::Genre,
                lofty::tag::ItemValue::Text(g.clone()),
            ));
        }
        count += 1;
    } else if let Some(ref v) = update.genre {
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
    for (cle, valeur) in [
        (ItemKey::Composer, update.composer.clone()),
        (ItemKey::Label, update.label.clone()),
        (ItemKey::Isrc, update.isrc.clone()),
        (ItemKey::Bpm, update.bpm.map(|b| b.to_string())),
        (ItemKey::Lyrics, update.lyrics.clone()),
        (ItemKey::AlbumArtist, update.album_artist.clone()),
    ] {
        if let Some(v) = valeur {
            tag.insert(lofty::tag::TagItem::new(
                cle,
                lofty::tag::ItemValue::Text(v),
            ));
            count += 1;
        }
    }
    count
}

/// La clé Vorbis du Dynamic Range d'une PISTE, telle que foobar2000 (DR
/// Meter) l'écrit et telle que le scan la RELIT.
///
/// 🔴 C'est la contrainte de tout ce bloc : « cette clé doit pouvoir être
/// relue » (Bertrand, 16/09/2026). Le scan lit `DYNAMIC RANGE=` dans l'en-tête
/// Vorbis (`read_vorbis_header` + `find_vorbis_comment`, dans `mod.rs`) et
/// pose alors `dr_track` + `dr_source = "tag"`. Graver autre chose — une autre
/// graphie, une autre famille de conteneur — produirait un tag que Tune
/// lui-même ne verrait plus au scan suivant.
pub const CLE_VORBIS_DR: &str = "DYNAMIC RANGE";

/// Ce qu'une gravure de DR a fait du fichier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GravureDr {
    /// La clé a été écrite avec la valeur demandée.
    Ecrite,
    /// Le fichier portait DÉJÀ une valeur non vide : elle est rendue telle
    /// quelle, et rien n'a été écrit. Le tag du disque fait foi — même règle
    /// que `replaygain::peut_ecrire_le_dr` dans l'autre sens.
    DejaPresente(String),
}

/// Les conteneurs dont le scan relit l'en-tête Vorbis. La liste est celle de
/// `read_vorbis_header`, et doit le rester : graver un format absent d'ici
/// serait écrire ce que personne ne relit.
pub fn format_dr_relu(file_path: &str) -> bool {
    matches!(
        Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("flac" | "ogg" | "oga" | "opus")
    )
}

/// Grave `DYNAMIC RANGE=<dr>` dans un FLAC, un Ogg Vorbis ou un Opus.
///
/// Passe par le `VorbisComments` CONCRET du fichier, jamais par le `Tag`
/// générique de `write_tags` : lofty 0.24 n'a pas de `ItemKey` pour cette
/// clé, et la conversion en `Tag` la laisserait tomber (c'est ce que dit le
/// commentaire du lecteur, dans `mod.rs`). Et par `FlacFile::save_to`, jamais
/// par `VorbisComments::save_to` seul : le premier réenchaîne les blocs
/// PICTURE du fichier, le second les retirerait — un FLAC gravé perdrait sa
/// pochette.
///
/// `dr` est écrit tel quel : c'est à l'appelant de donner l'entier normalisé
/// (`"12"`), pas `"DR12"`.
pub async fn graver_dr(file_path: &str, dr: &str) -> Result<GravureDr, String> {
    if !format_dr_relu(file_path) {
        return Err("unsupported format for DYNAMIC RANGE".into());
    }
    let dr = dr.trim();
    if dr.is_empty() || !dr.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid DR value: {dr:?}"));
    }
    let path = graphie_sur_disque(file_path)?;
    let dr = dr.to_string();
    tokio::task::spawn_blocking(move || graver_dr_lofty(&path, &dr))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn graver_dr_lofty(path: &str, dr: &str) -> Result<GravureDr, String> {
    use std::io::Seek;

    use lofty::config::ParseOptions;
    use lofty::flac::FlacFile;
    use lofty::ogg::{OpusFile, VorbisComments, VorbisFile};

    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open: {e}"))?;

    // Une valeur déjà là, non vide, l'emporte : rendue à l'appelant pour qu'il
    // aligne la base sur le disque plutôt que l'inverse.
    fn deja(vc: &VorbisComments) -> Option<String> {
        vc.get(CLE_VORBIS_DR)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    }

    // `ParseOptions::new()` LIT les pochettes — il le faut : elles sont
    // réécrites à partir de ce qui a été lu.
    let opts = ParseOptions::new();
    let resultat = match ext.as_str() {
        "flac" => {
            let mut f =
                FlacFile::read_from(&mut file, opts).map_err(|e| format!("lofty read: {e}"))?;
            if f.vorbis_comments().is_none() {
                f.set_vorbis_comments(VorbisComments::default());
            }
            let vc = f.vorbis_comments_mut().expect("posé juste au-dessus");
            if let Some(v) = deja(vc) {
                return Ok(GravureDr::DejaPresente(v));
            }
            vc.insert(CLE_VORBIS_DR.to_string(), dr.to_string());
            file.rewind().map_err(|e| format!("rewind: {e}"))?;
            f.save_to(&mut file, WriteOptions::default())
        }
        "opus" => {
            let mut f =
                OpusFile::read_from(&mut file, opts).map_err(|e| format!("lofty read: {e}"))?;
            if let Some(v) = deja(f.vorbis_comments()) {
                return Ok(GravureDr::DejaPresente(v));
            }
            f.vorbis_comments_mut()
                .insert(CLE_VORBIS_DR.to_string(), dr.to_string());
            file.rewind().map_err(|e| format!("rewind: {e}"))?;
            f.save_to(&mut file, WriteOptions::default())
        }
        "ogg" | "oga" => {
            let mut f =
                VorbisFile::read_from(&mut file, opts).map_err(|e| format!("lofty read: {e}"))?;
            if let Some(v) = deja(f.vorbis_comments()) {
                return Ok(GravureDr::DejaPresente(v));
            }
            f.vorbis_comments_mut()
                .insert(CLE_VORBIS_DR.to_string(), dr.to_string());
            file.rewind().map_err(|e| format!("rewind: {e}"))?;
            f.save_to(&mut file, WriteOptions::default())
        }
        _ => return Err("unsupported format for DYNAMIC RANGE".into()),
    };
    resultat.map_err(|e| format!("lofty save: {e}"))?;
    info!(file = path, dr, "dr_grave_lofty");
    Ok(GravureDr::Ecrite)
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
    // #5299 — un fichier rangé dans une image ISO : l'image est en lecture
    // seule, quel que soit le format.
    matches!(ext.as_str(), "dff") || crate::audio::iso9660::est_chemin_virtuel(file_path)
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
    let count = ecrire_par_tag_generique(file_path, |tag| {
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
        count
    })?;
    if count > 0 {
        info!(file = file_path, fields = count, "extended_tags_written");
    }
    Ok(WriteResult {
        file_path: file_path.into(),
        fields_written: count,
    })
}

// ---------------------------------------------------------------------------
// « Écrire dans les fichiers » — tranche 4 du chantier « édition des albums,
// compilations et coffrets » (GO de Bertrand du 25/09/2026).
//
// Reporte dans les BALISES ce que le mode « Modifier » de la fiche album a
// posé EN BASE. Même écrivain que tout le reste du module
// (`ecrire_par_tag_generique` : Vorbis par `split_tag`/`merge_tag`, pochettes
// et champs maison conservés ; ID3v2 / MP4 / APE par le `Tag` générique),
// mais JAMAIS en place : sur une copie, dans le même dossier, qui ne remplace
// l'original qu'après avoir prouvé que son audio est identique octet pour
// octet ([`super::empreinte_audio`]).
// ---------------------------------------------------------------------------

/// Préfixe des copies de travail : le scanner et le surveillant les écartent
/// (`scanner::is_tune_temp_file`), elles ne deviennent jamais des pistes.
/// Caché (point en tête) depuis #6635 : un explorateur de fichiers ne le
/// montre pas pendant l'écriture.
pub const PREFIXE_COPIE_DE_TRAVAIL: &str = ".tune-balises-";

/// Le préfixe visible d'avant #6635. Le scan l'écarte toujours, et le
/// nettoyage des copies orphelines le retire.
pub const PREFIXE_COPIE_DE_TRAVAIL_ANCIEN: &str = "tune-balises-";

/// Les valeurs EFFECTIVES d'une piste, telles que la base les tient.
///
/// `None` sur un champ facultatif : « ne pas toucher » — sauf
/// [`Self::nom_disque`], où `None` RETIRE la balise (un disque sans nom en
/// base ne doit pas garder, dans le fichier, un nom que l'utilisateur a
/// effacé).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BalisesEdition {
    pub album: String,
    pub artiste_album: Option<String>,
    pub disque: u32,
    pub disques: u32,
    pub nom_disque: Option<String>,
    pub piste: u32,
    pub pistes: u32,
    pub titre: String,
    pub artiste: Option<String>,
    /// `Some(true)` : `COMPILATION=1` ; `Some(false)` : pas une compilation
    /// (voir [`DrapeauAEcrire`]) ; `None` : ne pas toucher.
    pub compilation: Option<DrapeauAEcrire>,
    /// GENRE (#5314, décision de Bertrand du 28/09/2026) : les genres de la
    /// piste EN BASE (`tracks.genres`, sinon `tracks.genre`), dans l'ordre —
    /// le premier reste le genre principal. Écrits en VALEURS SÉPARÉES, la
    /// convention multivaleur que le lecteur du scan relit déjà
    /// (`metadata::genres_from_tag_values`, #1821) : champ `GENRE` répété en
    /// Vorbis, atome `©gen` répété en MP4, `TCON` multivalué en ID3v2.4.
    ///
    /// Vide : « ne pas toucher » — une piste sans genre en base ne vide
    /// JAMAIS la balise GENRE que porte son fichier.
    #[serde(default)]
    pub genres: Vec<String>,
}

/// Ce que devient la balise COMPILATION.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrapeauAEcrire {
    /// `1` (`TCMP=1`, `cpil=true`, `COMPILATION=1`).
    Vrai,
    /// `0` : l'album a plusieurs artistes mais l'utilisateur dit « pas une
    /// compilation » — le `0` porte cette intention jusqu'au scan suivant, qui
    /// sinon la redécouvrirait d'après les artistes.
    Faux,
    /// Retrait : un album ordinaire n'a pas besoin de drapeau.
    Retrait,
}

/// Un champ qui change : nom stable (celui de la balise Vorbis), avant, après.
/// `apres: None` = balise retirée.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Changement {
    pub champ: &'static str,
    pub avant: Option<String>,
    pub apres: Option<String>,
}

/// Les extensions que « Écrire dans les fichiers » sait écrire ET garder :
/// celles que lofty écrit et dont [`super::empreinte_audio`] sait isoler
/// l'audio. DSF et DFF n'y sont pas : lofty 0.24 ne connaît pas le DSF (la
/// lecture passe par un lecteur ID3 maison, voir `metadata::mod`), et aucun
/// écrivain du dépôt n'y écrit.
pub fn format_balises_edition(file_path: &str) -> bool {
    let p = Path::new(file_path);
    super::empreinte_audio::format_empreinte_gere(p) && !is_unsupported_format(file_path)
}

fn texte(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Les clés de genre (`genre_key`) de valeurs brutes, dans l'ordre, après le
/// même découpage que le scan : « Jazz; Fusion » en une chaîne et deux
/// valeurs `Jazz` / `Fusion` décrivent la même musique.
fn cles_de_genres<S: AsRef<str>>(valeurs: &[S]) -> Vec<String> {
    super::genres_from_tag_values(valeurs)
        .iter()
        .map(|g| super::genre_key(g))
        .collect()
}

/// Ce qui changerait sur ce `Tag` pour qu'il porte `b`.
fn changements(tag: &lofty::tag::Tag, b: &BalisesEdition) -> Vec<Changement> {
    let mut v = Vec::new();
    let mut texte_requis = |champ: &'static str, avant: Option<String>, apres: Option<&str>| {
        let apres = texte(apres);
        if apres.is_some() && avant != apres {
            v.push(Changement {
                champ,
                avant,
                apres,
            });
        }
    };
    texte_requis("ALBUM", texte(tag.album().as_deref()), Some(&b.album));
    texte_requis(
        "ALBUMARTIST",
        texte(tag.get_string(ItemKey::AlbumArtist)),
        b.artiste_album.as_deref(),
    );
    texte_requis("TITLE", texte(tag.title().as_deref()), Some(&b.titre));
    texte_requis(
        "ARTIST",
        texte(tag.artist().as_deref()),
        b.artiste.as_deref(),
    );
    let mut nombre = |champ: &'static str, avant: Option<u32>, apres: u32| {
        if apres > 0 && avant != Some(apres) {
            v.push(Changement {
                champ,
                avant: avant.map(|n| n.to_string()),
                apres: Some(apres.to_string()),
            });
        }
    };
    nombre("DISCNUMBER", tag.disk(), b.disque);
    nombre("DISCTOTAL", tag.disk_total(), b.disques);
    nombre("TRACKNUMBER", tag.track(), b.piste);
    nombre("TRACKTOTAL", tag.track_total(), b.pistes);
    let nom_avant = texte(tag.get_string(ItemKey::SetSubtitle));
    let nom_apres = texte(b.nom_disque.as_deref());
    if nom_avant != nom_apres {
        v.push(Changement {
            champ: "DISCSUBTITLE",
            avant: nom_avant,
            apres: nom_apres,
        });
    }
    let brut = texte(tag.get_string(ItemKey::FlagCompilation));
    let lu = super::lire_drapeau_compilation(brut.as_deref()).tri_etat();
    let voulu = match b.compilation {
        Some(DrapeauAEcrire::Vrai) if lu != Some(true) => Some(Some("1".to_string())),
        Some(DrapeauAEcrire::Faux) if lu != Some(false) => Some(Some("0".to_string())),
        Some(DrapeauAEcrire::Retrait) if brut.is_some() => Some(None),
        _ => None,
    };
    if let Some(apres) = voulu {
        v.push(Changement {
            champ: "COMPILATION",
            avant: brut,
            apres,
        });
    }
    // GENRE : seulement si la base en a un (#5314). Comparé par clés de
    // genre, pour ne pas réécrire un fichier qui dit déjà la même chose
    // autrement (casse, « ; » au lieu de valeurs séparées).
    // Une valeur qui ne donne aucun genre une fois découpée (« / ») vaut
    // « pas de genre » : on ne touche pas.
    let voulus = cles_de_genres(&b.genres);
    if !voulus.is_empty() {
        let bruts: Vec<String> = tag
            .get_strings(ItemKey::Genre)
            .filter_map(|s| texte(Some(s)))
            .collect();
        if cles_de_genres(&bruts) != voulus {
            v.push(Changement {
                champ: "GENRE",
                avant: (!bruts.is_empty()).then(|| bruts.join("; ")),
                apres: Some(b.genres.join("; ")),
            });
        }
    }
    v
}

/// Pose `changements` sur le `Tag`. Rend combien de champs ont été posés.
/// GENRE prend ses valeurs dans `b.genres`, une par élément du `Tag`.
fn poser(tag: &mut lofty::tag::Tag, changements: &[Changement], b: &BalisesEdition) -> usize {
    for c in changements {
        let nombre = || c.apres.as_deref().and_then(|s| s.parse::<u32>().ok());
        match (c.champ, c.apres.as_deref()) {
            ("ALBUM", Some(s)) => tag.set_album(s.to_string()),
            ("ALBUMARTIST", Some(s)) => {
                tag.insert_text(ItemKey::AlbumArtist, s.to_string());
            }
            ("TITLE", Some(s)) => tag.set_title(s.to_string()),
            ("ARTIST", Some(s)) => tag.set_artist(s.to_string()),
            ("DISCNUMBER", _) => {
                if let Some(n) = nombre() {
                    tag.set_disk(n)
                }
            }
            ("DISCTOTAL", _) => {
                if let Some(n) = nombre() {
                    tag.set_disk_total(n)
                }
            }
            ("TRACKNUMBER", _) => {
                if let Some(n) = nombre() {
                    tag.set_track(n)
                }
            }
            ("TRACKTOTAL", _) => {
                if let Some(n) = nombre() {
                    tag.set_track_total(n)
                }
            }
            ("DISCSUBTITLE", Some(s)) => {
                tag.insert_text(ItemKey::SetSubtitle, s.to_string());
            }
            ("DISCSUBTITLE", None) => tag.remove_key(ItemKey::SetSubtitle),
            ("COMPILATION", Some(s)) => {
                tag.insert_text(ItemKey::FlagCompilation, s.to_string());
            }
            ("COMPILATION", None) => tag.remove_key(ItemKey::FlagCompilation),
            ("GENRE", Some(_)) => {
                tag.remove_key(ItemKey::Genre);
                for g in &b.genres {
                    tag.push(lofty::tag::TagItem::new(
                        ItemKey::Genre,
                        lofty::tag::ItemValue::Text(g.clone()),
                    ));
                }
            }
            _ => {}
        }
    }
    changements.len()
}

/// Le `Tag` que l'écrivain éditerait, lu sans rien écrire : même choix que
/// `ecrire_par_tag_generique` (Vorbis pour FLAC/Ogg/Opus, sinon l'étiquette
/// primaire, sinon la première).
fn tag_lu(file_path: &str) -> Result<lofty::tag::Tag, String> {
    let tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    Ok(tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .cloned()
        .unwrap_or_else(|| lofty::tag::Tag::new(tagged.primary_tag_type())))
}

/// Le plan d'une piste : ce que l'écriture changerait, sans rien écrire.
///
/// `file_path` : la graphie de la base ; la graphie du disque est retrouvée
/// ici (#1865).
pub fn plan_balises_edition(
    file_path: &str,
    b: &BalisesEdition,
) -> Result<Vec<Changement>, String> {
    if !format_balises_edition(file_path) {
        return Err("unsupported tag format".into());
    }
    let path = graphie_sur_disque(file_path)?;
    Ok(changements(&tag_lu(&path)?, &b.sanitized()))
}

impl BalisesEdition {
    /// Même assainissement que [`TagUpdate`] : aucun caractère de contrôle
    /// n'atteint un fichier.
    fn sanitized(&self) -> Self {
        let mut b = self.clone();
        for (champ, v) in [("album_title", &mut b.album), ("title", &mut b.titre)] {
            let (propre, _) = super::sanitize_untrusted_single_line_text(v, champ);
            *v = propre;
        }
        for (champ, v) in [
            ("album_artist", &mut b.artiste_album),
            ("artist_name", &mut b.artiste),
            ("disc_subtitle", &mut b.nom_disque),
        ] {
            if let Some(brut) = v.as_deref() {
                let (propre, _) = super::sanitize_untrusted_single_line_text(brut, champ);
                *v = (!propre.is_empty()).then_some(propre);
            }
        }
        b.genres = b
            .genres
            .iter()
            .map(|g| super::sanitize_untrusted_single_line_text(g, "genre").0)
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect();
        b
    }
}

/// Écrit `b` dans le fichier, ATOMIQUEMENT. Rend les changements écrits
/// (vide : le fichier portait déjà tout, il n'a pas été touché).
///
/// 1. l'empreinte audio de l'original est prise ;
/// 2. l'original est COPIÉ dans son propre dossier, sous un nom
///    [`PREFIXE_COPIE_DE_TRAVAIL`] que le scan ignore (droits recopiés,
///    propriétaire recopié quand le système le permet) ;
/// 3. l'écrivain du module écrit dans la COPIE ;
/// 4. la copie est relue : empreinte audio identique, sinon abandon ;
///    l'original n'a pas bougé depuis l'étape 1 (taille, date), sinon abandon ;
/// 5. la copie est synchronisée sur le disque puis RENOMMÉE sur l'original —
///    un renommage dans un même dossier est atomique : un lecteur voit
///    l'ancien fichier entier ou le nouveau entier, jamais un mélange.
///
/// Tout abandon retire la copie et laisse l'original tel quel.
///
/// Synchrone : à appeler dans `spawn_blocking`.
pub fn ecrire_balises_edition(
    file_path: &str,
    b: &BalisesEdition,
) -> Result<Vec<Changement>, String> {
    if !format_balises_edition(file_path) {
        return Err("unsupported tag format".into());
    }
    let path = graphie_sur_disque(file_path)?;
    let b = b.sanitized();
    let prevus = changements(&tag_lu(&path)?, &b);
    if prevus.is_empty() {
        return Ok(prevus);
    }
    let mut ecrits: Vec<Changement> = Vec::new();
    ecrire_atomiquement(
        Path::new(&path),
        true,
        |tag| {
            // Recalculé sur le `Tag` que l'écrivain édite vraiment.
            ecrits = changements(tag, &b);
            poser(tag, &ecrits, &b)
        },
        // La copie RELUE doit porter chaque valeur : un conteneur qui en
        // refuse une (un WAV qui n'a que des balises RIFF INFO, sans
        // ALBUMARTIST ni DISCSUBTITLE) ne remplace pas l'original.
        |copie| {
            let restants = changements(&tag_lu(copie)?, &b);
            if restants.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "champs non retenus par le conteneur : {}",
                    restants
                        .iter()
                        .map(|c| c.champ)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        },
    )?;
    info!(
        file = %path,
        champs = ecrits.len(),
        "balises_edition_ecrites"
    );
    Ok(ecrits)
}

/// Le jeton du PROCESSUS, dans le nom de chaque copie de travail : le pid et
/// l'instant du premier appel, en hexadécimal, sans tiret.
///
/// Une copie qui porte un AUTRE jeton a été laissée par un processus qui
/// n'existe plus — tué entre l'écriture de la copie et son renommage (#6635) :
/// elle est retirée au prochain passage dans son dossier
/// ([`nettoyer_les_copies_orphelines`]). Le pid seul ne suffit pas : dans un
/// conteneur, le serveur redémarre sous le même pid.
fn jeton_de_processus() -> String {
    #[cfg(test)]
    if let Some(j) = essais::JETON.with(|j| j.borrow().clone()) {
        return j;
    }
    static JETON: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    JETON
        .get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{:x}x{:x}", std::process::id(), nanos)
        })
        .clone()
}

/// La copie de travail : `<dossier>/.tune-balises-<jeton>-<n>.<ext>`. Cachée
/// (point en tête), dans le MÊME dossier que l'original, donc sur le même
/// système de fichiers : le `rename` final est atomique.
fn chemin_de_copie(original: &Path) -> Result<std::path::PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COMPTEUR: AtomicU64 = AtomicU64::new(0);
    let dossier = original
        .parent()
        .ok_or_else(|| "dossier parent introuvable".to_string())?;
    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("tmp");
    let n = COMPTEUR.fetch_add(1, Ordering::Relaxed);
    Ok(dossier.join(format!(
        "{PREFIXE_COPIE_DE_TRAVAIL}{}-{n}.{ext}",
        jeton_de_processus()
    )))
}

/// Retire, dans `dossier`, les copies de travail qu'un processus disparu a
/// laissées : celles d'un autre jeton, et toutes celles de l'ancien préfixe
/// visible (versions d'avant #6635). Les copies du processus courant ne sont
/// jamais touchées : une écriture voisine peut être en cours. Rend combien
/// ont été retirées.
pub(crate) fn nettoyer_les_copies_orphelines(dossier: &Path) -> usize {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return 0;
    };
    let jeton = jeton_de_processus();
    let mut retirees = 0;
    for e in entrees.flatten() {
        let nom = e.file_name().to_string_lossy().into_owned();
        let orpheline = if let Some(reste) = nom.strip_prefix(PREFIXE_COPIE_DE_TRAVAIL) {
            reste.split_once('-').map(|(j, _)| j) != Some(jeton.as_str())
        } else {
            nom.starts_with(PREFIXE_COPIE_DE_TRAVAIL_ANCIEN)
        };
        if !orpheline || !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        match std::fs::remove_file(e.path()) {
            Ok(()) => {
                retirees += 1;
                tracing::warn!(copie = %e.path().display(), "balises_copie_orpheline_retiree");
            }
            Err(err) => {
                tracing::warn!(copie = %e.path().display(), erreur = %err, "balises_copie_orpheline_non_retiree");
            }
        }
    }
    retirees
}

/// Retire la copie de travail quoi qu'il arrive, sauf si elle a été renommée.
struct Copie {
    chemin: std::path::PathBuf,
    renommee: bool,
}

impl Drop for Copie {
    fn drop(&mut self) {
        if !self.renommee {
            let _ = std::fs::remove_file(&self.chemin);
        }
    }
}

/// Motif du refus d'écrire un fichier en lecture seule (droits du fichier).
pub const MOTIF_FICHIER_LECTURE_SEULE: &str =
    "fichier en lecture seule : écriture refusée, original intact";
/// Motif du refus d'écrire sur un volume monté en lecture seule.
pub const MOTIF_VOLUME_LECTURE_SEULE: &str =
    "volume monté en lecture seule : écriture refusée, original intact";
/// Motif du refus quand le dossier n'accepte pas la copie de travail.
pub const MOTIF_DOSSIER_LECTURE_SEULE: &str =
    "dossier en lecture seule : écriture refusée, original intact";
/// Motif du refus d'un format dont l'audio ne peut pas être contrôlé.
pub const MOTIF_FORMAT_SANS_EMPREINTE: &str = "format sans empreinte audio : écriture refusée";

fn motif_io(e: &std::io::Error, motif_droits: &str, quoi: &str) -> String {
    match e.kind() {
        std::io::ErrorKind::ReadOnlyFilesystem => MOTIF_VOLUME_LECTURE_SEULE.to_string(),
        std::io::ErrorKind::PermissionDenied => motif_droits.to_string(),
        _ => format!("{quoi}: {e}"),
    }
}

/// Le fichier se laisse-t-il écrire ? Refuse un fichier en lecture seule
/// (droits, ou droits refusés au service) et un volume monté en lecture
/// seule, AVANT toute copie. L'ouverture en écriture, sans troncature, ne
/// modifie ni le contenu ni la date du fichier.
fn refuser_si_lecture_seule(original: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.permissions().readonly() {
        return Err(MOTIF_FICHIER_LECTURE_SEULE.into());
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(original)
        .map(drop)
        .map_err(|e| motif_io(&e, MOTIF_FICHIER_LECTURE_SEULE, "open"))
}

/// La copie porte-t-elle les droits et le propriétaire de l'original ? Les
/// droits sont recopiés par `fs::copy` ; le propriétaire est reposé par
/// `chown` quand le système le permet. Si, après cela, la copie ne porte pas
/// les mêmes, le renommage changerait le propriétaire ou les droits du
/// fichier de l'utilisateur : refus. (Sur un partage dont le montage impose
/// un propriétaire unique, copie et original le portent tous deux : la
/// comparaison passe.)
#[cfg(unix)]
fn conserver_droits_et_proprietaire(
    copie: &Path,
    meta_original: &std::fs::Metadata,
) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let _ = std::os::unix::fs::chown(copie, Some(meta_original.uid()), Some(meta_original.gid()));
    let mut m = std::fs::metadata(copie).map_err(|e| format!("stat copie: {e}"))?;
    if m.mode() & 0o7777 != meta_original.mode() & 0o7777 {
        let _ = std::fs::set_permissions(
            copie,
            std::fs::Permissions::from_mode(meta_original.mode() & 0o7777),
        );
        m = std::fs::metadata(copie).map_err(|e| format!("stat copie: {e}"))?;
    }
    if m.uid() != meta_original.uid() || m.gid() != meta_original.gid() {
        return Err(
            "propriétaire du fichier impossible à conserver : écriture refusée, original intact"
                .into(),
        );
    }
    if m.mode() & 0o7777 != meta_original.mode() & 0o7777 {
        return Err(
            "droits du fichier impossibles à conserver : écriture refusée, original intact".into(),
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn conserver_droits_et_proprietaire(
    _copie: &Path,
    _meta_original: &std::fs::Metadata,
) -> Result<(), String> {
    Ok(())
}

/// Synchronise un dossier sur le disque (son entrée de répertoire). Sans
/// effet hors Unix, où un dossier ne s'ouvre pas ainsi.
fn synchroniser_le_dossier(dossier: Option<&Path>) {
    #[cfg(unix)]
    if let Some(d) = dossier {
        let _ = std::fs::File::open(d).and_then(|d| d.sync_all());
    }
    #[cfg(not(unix))]
    let _ = dossier;
}

/// L'étape commune : copier, écrire la copie, prouver l'audio, renommer.
/// Rend le nombre de champs posés (0 : rien écrit, la copie est jetée).
///
/// 1. le fichier doit se laisser écrire (sinon refus, rien n'est créé) ;
/// 2. les copies orphelines du dossier sont retirées ;
/// 3. l'empreinte audio de l'original est prise (un format sans empreinte
///    est refusé) ;
/// 4. l'original est COPIÉ, dans son dossier, sous un nom caché ; la copie
///    porte ses droits et son propriétaire, sinon refus ;
/// 5. les balises sont écrites dans la COPIE, que `verifier` relit ;
/// 6. l'empreinte audio de la copie doit être celle de l'original, et
///    l'original ne doit pas avoir bougé (taille, date) ;
/// 7. `fsync` de la copie, puis du dossier, puis `rename` sur l'original,
///    puis `fsync` du dossier.
///
/// Tout abandon retire la copie et laisse l'original tel quel. Un processus
/// tué en cours de route laisse au pire la copie cachée, que l'étape 2 du
/// passage suivant retire.
///
/// `promouvoir` : voir [`ecrire_par_tag`].
pub(crate) fn ecrire_atomiquement(
    original: &Path,
    promouvoir: bool,
    appliquer: impl FnOnce(&mut lofty::tag::Tag) -> usize,
    verifier: impl FnOnce(&str) -> Result<(), String>,
) -> Result<usize, String> {
    use super::empreinte_audio::empreinte_audio;

    let meta_avant = std::fs::metadata(original).map_err(|e| format!("stat: {e}"))?;
    refuser_si_lecture_seule(original, &meta_avant)?;
    if let Some(dossier) = original.parent() {
        nettoyer_les_copies_orphelines(dossier);
    }
    let avant =
        empreinte_audio(original)?.ok_or_else(|| MOTIF_FORMAT_SANS_EMPREINTE.to_string())?;
    let mut copie = Copie {
        chemin: chemin_de_copie(original)?,
        renommee: false,
    };
    if copie.chemin.exists() {
        return Err("copie de travail déjà présente".into());
    }
    std::fs::copy(original, &copie.chemin)
        .map_err(|e| motif_io(&e, MOTIF_DOSSIER_LECTURE_SEULE, "copie"))?;
    conserver_droits_et_proprietaire(&copie.chemin, &meta_avant)?;
    let copie_str = copie
        .chemin
        .to_str()
        .ok_or_else(|| "chemin de copie non UTF-8".to_string())?
        .to_string();
    let n = ecrire_par_tag(&copie_str, promouvoir, appliquer)?;
    if n == 0 {
        return Ok(0);
    }
    #[cfg(test)]
    if let Some(sabotage) = essais::APRES_ECRITURE.with(|s| s.get()) {
        sabotage(&copie.chemin);
    }
    // Les champs d'abord, l'audio ENSUITE : la preuve de l'audio est la
    // dernière chose vue avant le renommage.
    verifier(&copie_str)?;
    let apres = empreinte_audio(&copie.chemin)?
        .ok_or_else(|| "empreinte audio de la copie illisible".to_string())?;
    if apres != avant {
        tracing::error!(
            file = %original.display(),
            "balises_edition_audio_modifie_abandon"
        );
        return Err("l'écriture aurait modifié l'audio : abandon, original intact".into());
    }
    let meta_now = std::fs::metadata(original).map_err(|e| format!("stat: {e}"))?;
    if meta_now.len() != meta_avant.len() || meta_now.modified().ok() != meta_avant.modified().ok()
    {
        return Err("le fichier a changé pendant l'écriture : abandon".into());
    }
    std::fs::File::open(&copie.chemin)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("sync: {e}"))?;
    synchroniser_le_dossier(original.parent());
    #[cfg(test)]
    if essais::COUPURE_AVANT_RENOMMAGE.with(|c| c.get()) {
        // Le processus « meurt » ici : la copie reste sur le disque, comme
        // si aucun `Drop` ne passait.
        copie.renommee = true;
        return Err("coupure simulée avant le renommage".into());
    }
    std::fs::rename(&copie.chemin, original).map_err(|e| format!("rename: {e}"))?;
    copie.renommee = true;
    synchroniser_le_dossier(original.parent());
    Ok(n)
}

/// « Graver tags » (`POST /library/write-tags`, #6635) : le [`TagUpdate`]
/// écrit par le chemin SÛR — copie cachée dans le même dossier, contrôle de
/// l'audio, remplacement atomique ([`ecrire_atomiquement`]) — jamais en place
/// comme [`write_tags`].
///
/// Un format dont l'audio ne peut pas être contrôlé est refusé
/// ([`MOTIF_FORMAT_SANS_EMPREINTE`]), comme un fichier en lecture seule.
pub async fn write_tags_atomique(
    file_path: &str,
    update: &TagUpdate,
) -> Result<WriteResult, String> {
    if !format_balises_edition(file_path) {
        return Err(MOTIF_FORMAT_SANS_EMPREINTE.into());
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
    tokio::task::spawn_blocking(move || write_tags_atomique_sync(&path, &update))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn write_tags_atomique_sync(path: &str, update: &TagUpdate) -> Result<WriteResult, String> {
    let count = ecrire_atomiquement(
        Path::new(path),
        false,
        |tag| appliquer_update(tag, update),
        |_| Ok(()),
    )?;
    if count > 0 {
        info!(file = path, fields = count, "tags_written_atomique");
    }
    Ok(WriteResult {
        file_path: path.into(),
        fields_written: count,
    })
}

/// Crochets d'essai de l'écriture atomique : propres au fil qui les pose.
#[cfg(test)]
pub(crate) mod essais {
    use std::cell::{Cell, RefCell};
    thread_local! {
        /// Appelé sur la copie juste après l'écriture des balises.
        pub static APRES_ECRITURE: Cell<Option<fn(&std::path::Path)>> = const { Cell::new(None) };
        /// Le processus « meurt » entre l'écriture et le renommage.
        pub static COUPURE_AVANT_RENOMMAGE: Cell<bool> = const { Cell::new(false) };
        /// Un autre jeton de processus (celui d'un processus disparu).
        pub static JETON: RefCell<Option<String>> = const { RefCell::new(None) };
    }
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
            //
            // `dr_source` (#3924) has no mapping either, and must not get one:
            // it is not a tag at all. It is Tune's own note of WHICH of the two
            // producers wrote `dr_track` — the file tag, or the analysis pass —
            // and writing it into the file would invent a `DR_SOURCE` field
            // that no tagger reads and that would come back as a tag on the
            // next scan.
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
        // #5519 : le gain approché depuis `foo_dr.txt` n'est JAMAIS écrit
        // dans un fichier — sa clé n'a pas de correspondance, l'écrivain la
        // saute (`tag_writer_unknown_key_skipped`).
        assert!(
            tune_key_to_lofty(crate::audio::replaygain::gain_approche::CLE_MESURE_FOO_DR).is_none()
        );
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
    // ------------------------------------------------------------------
    // Graver le Dynamic Range — « cette clé doit pouvoir être relue »
    // (Bertrand, 16/09/2026). Chaque témoin relit par le VRAI lecteur du
    // scan, `read_extended_metadata`, jamais par lofty : c'est lui qui pose
    // `dr_track` et `dr_source` en base, et c'est donc lui qui décide si la
    // gravure existe.
    // ------------------------------------------------------------------

    fn fixture(nom: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(nom)
    }

    fn relu(chemin: &std::path::Path) -> (Option<String>, Option<String>) {
        let m = crate::metadata::read_extended_metadata(chemin);
        (m.get("dr_track").cloned(), m.get("dr_source").cloned())
    }

    #[tokio::test]
    async fn dr_grave_est_relu_par_le_scan_sur_les_trois_conteneurs() {
        for nom in ["test.flac", "test.opus", "test_vorbis.ogg"] {
            let dir = tempfile::tempdir().unwrap();
            let cible = dir.path().join(nom);
            std::fs::copy(fixture(nom), &cible).unwrap();
            assert_eq!(
                relu(&cible),
                (None, None),
                "{nom} : la fixture porte deja un DR"
            );

            let r = graver_dr(cible.to_str().unwrap(), "12").await.unwrap();
            assert_eq!(r, GravureDr::Ecrite, "{nom}");

            assert_eq!(
                relu(&cible),
                (Some("12".into()), Some("tag".into())),
                "{nom} : le scan ne relit pas ce qui vient d'etre grave"
            );
        }
    }

    #[tokio::test]
    async fn dr_deja_present_dans_le_fichier_fait_foi() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("test.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();
        let chemin = cible.to_str().unwrap();

        graver_dr(chemin, "9").await.unwrap();
        // Seconde gravure, valeur differente : refusee, et l'ancienne rendue.
        let r = graver_dr(chemin, "14").await.unwrap();
        assert_eq!(r, GravureDr::DejaPresente("9".into()));
        assert_eq!(relu(&cible).0.as_deref(), Some("9"));
    }

    #[tokio::test]
    async fn dr_grave_conserve_la_pochette_du_flac() {
        use lofty::config::ParseOptions;
        use lofty::flac::FlacFile;
        use lofty::ogg::OggPictureStorage;
        use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};

        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("test.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();

        // Une pochette posee par lofty lui-meme : un PNG minimal suffit, on ne
        // compte que les blocs PICTURE.
        let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        {
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&cible)
                .unwrap();
            let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
            let pic = Picture::unchecked(png)
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build();
            flac.insert_picture(pic, Some(PictureInformation::default()))
                .unwrap();
            use std::io::Seek;
            f.rewind().unwrap();
            flac.save_to(&mut f, WriteOptions::default()).unwrap();
        }
        let pochettes = |p: &std::path::Path| {
            let mut f = std::fs::File::open(p).unwrap();
            FlacFile::read_from(&mut f, ParseOptions::new())
                .unwrap()
                .pictures()
                .len()
        };
        assert_eq!(pochettes(&cible), 1, "la pochette temoin n'a pas ete posee");

        graver_dr(cible.to_str().unwrap(), "11").await.unwrap();

        assert_eq!(
            pochettes(&cible),
            1,
            "la gravure du DR a retire la pochette"
        );
        assert_eq!(relu(&cible).0.as_deref(), Some("11"));
    }

    #[tokio::test]
    async fn dr_refuse_un_format_que_le_scan_ne_relit_pas() {
        // MP3 : `read_vorbis_header` ne le lit pas, donc on n'y grave pas —
        // ecrire ce que personne ne relit serait un faux « fait ».
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("test.mp3");
        std::fs::copy(fixture("test.mp3"), &cible).unwrap();
        let r = graver_dr(cible.to_str().unwrap(), "12").await;
        assert!(r.is_err(), "{r:?}");
        assert!(!format_dr_relu("x.mp3") && !format_dr_relu("x.m4a") && format_dr_relu("x.FLAC"));
    }

    #[tokio::test]
    async fn dr_refuse_une_valeur_qui_nest_pas_un_entier() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("test.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();
        let chemin = cible.to_str().unwrap();
        assert!(graver_dr(chemin, "").await.is_err());
        assert!(graver_dr(chemin, "DR12").await.is_err());
        assert_eq!(
            relu(&cible),
            (None, None),
            "une valeur refusee ne doit rien ecrire"
        );
    }
    // ------------------------------------------------------------------
    // 🔴 Le graveur d'étiquettes ne doit plus EFFACER les champs Vorbis
    // qu'il ne connaît pas. Mesuré le 16/09/2026 : DR=12 avant, None après
    // une simple écriture de compositeur. Ces témoins relisent par le
    // lecteur du scan, comme ceux du DR.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn write_tags_conserve_le_dynamic_range_sur_les_trois_conteneurs() {
        for nom in ["test.flac", "test.opus", "test_vorbis.ogg"] {
            let dir = tempfile::tempdir().unwrap();
            let cible = dir.path().join(nom);
            std::fs::copy(fixture(nom), &cible).unwrap();
            let chemin = cible.to_str().unwrap();
            graver_dr(chemin, "12").await.unwrap();

            let upd = TagUpdate {
                composer: Some("Sonde".into()),
                title: Some("Titre".into()),
                ..Default::default()
            };
            let r = write_tags(chemin, &upd).await.unwrap();
            assert_eq!(r.fields_written, 2, "{nom}");

            let m = crate::metadata::read_extended_metadata(&cible);
            assert_eq!(
                m.get("dr_track").map(String::as_str),
                Some("12"),
                "{nom} : write_tags a efface le DYNAMIC RANGE"
            );
            assert_eq!(m.get("dr_source").map(String::as_str), Some("tag"), "{nom}");
            // Et ce qu'on a demande est bien la.
            assert_eq!(
                m.get("composer").map(String::as_str),
                Some("Sonde"),
                "{nom}"
            );
            let t = crate::metadata::read_metadata(&cible).unwrap();
            assert_eq!(t.title.as_deref(), Some("Titre"), "{nom}");
        }
    }

    #[test]
    fn l_editeur_de_tags_conserve_aussi_le_dynamic_range() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("test.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();
        let chemin = cible.to_str().unwrap();
        // Gravure synchrone : meme fonction que le chemin asynchrone.
        assert_eq!(graver_dr_lofty(chemin, "9").unwrap(), GravureDr::Ecrite);

        let mut champs = HashMap::new();
        champs.insert("composer".to_string(), "Sonde".to_string());
        champs.insert("isrc".to_string(), String::new()); // suppression d'un champ
        let r = write_metadata_to_file_sync(chemin, &champs).unwrap();
        assert_eq!(r.fields_written, 2);

        let m = crate::metadata::read_extended_metadata(&cible);
        assert_eq!(
            m.get("dr_track").map(String::as_str),
            Some("9"),
            "l'editeur a efface le DYNAMIC RANGE"
        );
        assert_eq!(m.get("composer").map(String::as_str), Some("Sonde"));
    }

    #[tokio::test]
    async fn write_tags_conserve_la_pochette_et_les_champs_maison_du_flac() {
        use lofty::config::ParseOptions;
        use lofty::flac::FlacFile;
        use lofty::ogg::OggPictureStorage;
        use lofty::picture::{MimeType, Picture, PictureType};

        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("test.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();
        {
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&cible)
                .unwrap();
            let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
            let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
            let pic = Picture::unchecked(png)
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build();
            flac.insert_picture(pic, Some(Default::default())).unwrap();
            if flac.vorbis_comments().is_none() {
                flac.set_vorbis_comments(Default::default());
            }
            flac.vorbis_comments_mut()
                .unwrap()
                .insert("MON CHAMP".into(), "valeur".into());
            use std::io::Seek;
            f.rewind().unwrap();
            flac.save_to(&mut f, WriteOptions::default()).unwrap();
        }
        let upd = TagUpdate {
            genre: Some("Jazz".into()),
            ..Default::default()
        };
        write_tags(cible.to_str().unwrap(), &upd).await.unwrap();

        let mut f = std::fs::File::open(&cible).unwrap();
        let flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
        assert_eq!(flac.pictures().len(), 1, "la pochette a saute");
        let vc = flac.vorbis_comments().unwrap();
        assert_eq!(
            vc.get("MON CHAMP"),
            Some("valeur"),
            "un champ maison a saute"
        );
        assert_eq!(vc.get("GENRE"), Some("Jazz"));
    }

    #[tokio::test]
    async fn write_tags_sans_rien_a_ecrire_ne_touche_pas_au_fichier() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("test.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();
        let avant = std::fs::read(&cible).unwrap();
        let r = write_tags(cible.to_str().unwrap(), &TagUpdate::default())
            .await
            .unwrap();
        assert_eq!(r.fields_written, 0);
        assert_eq!(std::fs::read(&cible).unwrap(), avant);
    }

    // ------------------------------------------------------------------
    // « Écrire dans les fichiers » (édition d'album, tranche 4) : l'écriture
    // atomique et ses deux gardes. Contre-épreuves : une copie dont l'audio a
    // bougé, une copie qui ne porte pas les champs — l'original ne bouge pas,
    // et aucune copie de travail ne traîne.
    // ------------------------------------------------------------------

    fn copies_de_travail(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(PREFIXE_COPIE_DE_TRAVAIL))
            .collect()
    }

    #[test]
    fn ecriture_atomique_temoin_ecrit_et_ne_laisse_rien() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("t.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();
        let n = ecrire_atomiquement(
            &cible,
            true,
            |tag| {
                tag.set_title("Temoin".into());
                1
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(n, 1);
        let t = crate::metadata::read_metadata(&cible).unwrap();
        assert_eq!(t.title.as_deref(), Some("Temoin"));
        assert!(copies_de_travail(dir.path()).is_empty());
    }

    #[test]
    fn ecriture_atomique_refuse_une_copie_dont_l_audio_a_bouge() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("t.flac");
        std::fs::copy(fixture("test.flac"), &cible).unwrap();
        let avant = std::fs::read(&cible).unwrap();
        let r = ecrire_atomiquement(
            &cible,
            true,
            |tag| {
                tag.set_title("Abime".into());
                1
            },
            // Simule un écrivain qui toucherait au son : un octet des trames
            // FLAC (le milieu du fichier) change dans la COPIE.
            |copie| {
                let mut o = std::fs::read(copie).unwrap();
                let m = o.len() / 2;
                o[m] ^= 0x01;
                std::fs::write(copie, o).unwrap();
                Ok(())
            },
        );
        let e = r.unwrap_err();
        assert!(e.contains("audio"), "{e}");
        assert_eq!(std::fs::read(&cible).unwrap(), avant, "l'original a bougé");
        assert!(copies_de_travail(dir.path()).is_empty(), "copie laissée");
    }

    #[test]
    fn ecriture_atomique_verification_refusee_laisse_l_original() {
        let dir = tempfile::tempdir().unwrap();
        let cible = dir.path().join("t.mp3");
        std::fs::copy(fixture("test.mp3"), &cible).unwrap();
        let avant = std::fs::read(&cible).unwrap();
        let r = ecrire_atomiquement(
            &cible,
            true,
            |tag| {
                tag.set_title("Refuse".into());
                1
            },
            |_| Err("champs non retenus par le conteneur : TITLE".into()),
        );
        assert!(r.is_err());
        assert_eq!(std::fs::read(&cible).unwrap(), avant);
        assert!(copies_de_travail(dir.path()).is_empty());
    }

    #[test]
    fn dsf_et_dff_ne_sont_pas_des_formats_d_edition() {
        assert!(!format_balises_edition("/m/a.dsf"));
        assert!(!format_balises_edition("/m/a.dff"));
        assert!(!format_balises_edition("/m/a.wma"));
        for ok in [
            "a.flac", "a.MP3", "a.m4a", "a.ogg", "a.opus", "a.wav", "a.aiff",
        ] {
            assert!(format_balises_edition(ok), "{ok}");
        }
    }

    // ------------------------------------------------------------------
    // #6635 — « Graver tags » passe par l'écriture SÛRE
    // (`write_tags_atomique`) : copie cachée dans le même dossier, contrôle de
    // l'audio, `rename` atomique ; refus de la lecture seule ; une coupure
    // entre l'écriture et le renommage ne laisse que l'original intact et une
    // copie cachée, retirée au passage suivant ; droits conservés.
    // Fichiers dans `crate::test_scratch` (garde `aucune_fuite_de_temporaires`).
    // ------------------------------------------------------------------

    /// Remet les crochets d'essai à zéro, même si le test panique.
    struct Crochets;
    impl Drop for Crochets {
        fn drop(&mut self) {
            essais::APRES_ECRITURE.with(|s| s.set(None));
            essais::COUPURE_AVANT_RENOMMAGE.with(|c| c.set(false));
            essais::JETON.with(|j| *j.borrow_mut() = None);
        }
    }

    fn banc_6635(nom: &str, src: &str) -> (crate::test_scratch::ScratchDir, std::path::PathBuf) {
        let dir = crate::test_scratch::scratch_dir(nom);
        let cible = dir.path().join(format!(
            "piste.{}",
            std::path::Path::new(src)
                .extension()
                .unwrap()
                .to_string_lossy()
        ));
        std::fs::copy(fixture(src), &cible).unwrap();
        (dir, cible)
    }

    fn maj_pop_rock() -> TagUpdate {
        TagUpdate {
            title: Some("Graver".into()),
            genre: Some("Pop".into()),
            genres: vec!["Pop".into(), "Rock".into()],
            ..Default::default()
        }
    }

    fn genres_vorbis(p: &std::path::Path) -> Vec<String> {
        use lofty::config::ParseOptions;
        use lofty::flac::FlacFile;
        let f =
            FlacFile::read_from(&mut std::fs::File::open(p).unwrap(), ParseOptions::new()).unwrap();
        f.vorbis_comments()
            .map(|vc| vc.get_all("GENRE").map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn empreinte(p: &std::path::Path) -> crate::metadata::empreinte_audio::Empreinte {
        crate::metadata::empreinte_audio::empreinte_audio(p)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn graver_tags_6635_ecrit_les_genres_et_garde_l_audio() {
        for src in ["test.flac", "test.mp3", "test.m4a"] {
            let (dir, cible) = banc_6635("tune-6635-sur", src);
            let audio_avant = empreinte(&cible);
            let r = write_tags_atomique_sync(cible.to_str().unwrap(), &maj_pop_rock()).unwrap();
            assert!(r.fields_written > 0, "{src}");
            assert_eq!(
                crate::metadata::read_metadata(&cible).unwrap().genres,
                ["Pop", "Rock"],
                "#6635 : {src} doit porter tous les genres"
            );
            assert_eq!(
                empreinte(&cible),
                audio_avant,
                "#6635 : l'audio de {src} doit rester identique"
            );
            assert!(
                copies_de_travail(dir.path()).is_empty(),
                "{src} : copie laissée"
            );
        }
    }

    #[test]
    fn graver_tags_6635_controle_audio_en_echec_laisse_l_original_sans_copie() {
        let _c = Crochets;
        let (dir, cible) = banc_6635("tune-6635-audio", "test.flac");
        let avant = std::fs::read(&cible).unwrap();
        // L'écrivain « abîme » un octet des trames de la COPIE.
        fn abimer(copie: &std::path::Path) {
            let mut o = std::fs::read(copie).unwrap();
            let m = o.len() - 64;
            o[m] ^= 0x01;
            std::fs::write(copie, o).unwrap();
        }
        essais::APRES_ECRITURE.with(|s| s.set(Some(abimer)));
        let e = write_tags_atomique_sync(cible.to_str().unwrap(), &maj_pop_rock()).unwrap_err();
        assert!(
            e.contains("audio"),
            "#6635 : le contrôle audio doit refuser — {e}"
        );
        assert_eq!(
            std::fs::read(&cible).unwrap(),
            avant,
            "#6635 : l'original doit rester intact octet pour octet"
        );
        assert!(
            copies_de_travail(dir.path()).is_empty(),
            "#6635 : la copie doit être supprimée"
        );
    }

    #[cfg(unix)]
    #[test]
    fn graver_tags_6635_fichier_en_lecture_seule_refuse() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, cible) = banc_6635("tune-6635-ro", "test.flac");
        std::fs::set_permissions(&cible, std::fs::Permissions::from_mode(0o444)).unwrap();
        let avant = std::fs::read(&cible).unwrap();
        let e = write_tags_atomique_sync(cible.to_str().unwrap(), &maj_pop_rock()).unwrap_err();
        assert_eq!(e, MOTIF_FICHIER_LECTURE_SEULE, "#6635 : refus attendu");
        assert_eq!(std::fs::read(&cible).unwrap(), avant);
        assert!(copies_de_travail(dir.path()).is_empty());
        std::fs::set_permissions(&cible, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn graver_tags_6635_dossier_en_lecture_seule_refuse() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, cible) = banc_6635("tune-6635-dro", "test.flac");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        // Sous root, les droits du dossier ne s'appliquent pas : rien à prouver.
        let sonde = dir.path().join(".sonde");
        if std::fs::write(&sonde, b"x").is_ok() {
            let _ = std::fs::remove_file(&sonde);
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let avant = std::fs::read(&cible).unwrap();
        let r = write_tags_atomique_sync(cible.to_str().unwrap(), &maj_pop_rock());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r.unwrap_err(), MOTIF_DOSSIER_LECTURE_SEULE);
        assert_eq!(std::fs::read(&cible).unwrap(), avant);
        assert!(copies_de_travail(dir.path()).is_empty());
    }

    #[test]
    fn graver_tags_6635_coupure_avant_le_renommage_puis_nettoyage() {
        let _c = Crochets;
        let (dir, cible) = banc_6635("tune-6635-coupure", "test.flac");
        let avant = std::fs::read(&cible).unwrap();
        // Un processus « mort » (autre jeton) est tué après l'écriture de la
        // copie, avant le `rename`.
        essais::JETON.with(|j| *j.borrow_mut() = Some("mort".into()));
        essais::COUPURE_AVANT_RENOMMAGE.with(|c| c.set(true));
        let e = write_tags_atomique_sync(cible.to_str().unwrap(), &maj_pop_rock()).unwrap_err();
        assert!(e.contains("coupure"), "{e}");
        assert_eq!(
            std::fs::read(&cible).unwrap(),
            avant,
            "#6635 : après une coupure, l'original doit être intact"
        );
        let restes = copies_de_travail(dir.path());
        assert_eq!(
            restes.len(),
            1,
            "la coupure laisse au pire une copie : {restes:?}"
        );
        assert!(
            restes[0].starts_with('.'),
            "la copie est cachée : {restes:?}"
        );
        // Une copie de l'ANCIEN préfixe visible, laissée par une version
        // d'avant : retirée aussi.
        let ancienne = dir
            .path()
            .join(format!("{PREFIXE_COPIE_DE_TRAVAIL_ANCIEN}1-2-3.flac"));
        std::fs::write(&ancienne, b"x").unwrap();

        // Le processus suivant (vrai jeton) repasse : il nettoie, puis écrit.
        essais::COUPURE_AVANT_RENOMMAGE.with(|c| c.set(false));
        essais::JETON.with(|j| *j.borrow_mut() = None);
        // Une copie du processus COURANT (écriture voisine en cours) : intacte.
        let voisine = dir.path().join(format!(
            "{PREFIXE_COPIE_DE_TRAVAIL}{}-999.flac",
            jeton_de_processus()
        ));
        std::fs::write(&voisine, b"x").unwrap();
        write_tags_atomique_sync(cible.to_str().unwrap(), &maj_pop_rock()).unwrap();
        assert_eq!(genres_vorbis(&cible), ["Pop", "Rock"]);
        assert!(
            !ancienne.exists(),
            "#6635 : l'ancienne copie doit être retirée"
        );
        assert!(
            voisine.exists(),
            "#6635 : une copie du processus vivant ne se retire pas"
        );
        std::fs::remove_file(&voisine).unwrap();
        assert!(
            copies_de_travail(dir.path()).is_empty(),
            "#6635 : la copie orpheline doit être nettoyée au passage suivant"
        );
    }

    #[cfg(unix)]
    #[test]
    fn graver_tags_6635_droits_et_proprietaire_conserves() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let (_dir, cible) = banc_6635("tune-6635-droits", "test.flac");
        std::fs::set_permissions(&cible, std::fs::Permissions::from_mode(0o640)).unwrap();
        let avant = std::fs::metadata(&cible).unwrap();
        write_tags_atomique_sync(cible.to_str().unwrap(), &maj_pop_rock()).unwrap();
        let apres = std::fs::metadata(&cible).unwrap();
        assert_ne!(avant.ino(), apres.ino(), "le fichier a bien été remplacé");
        assert_eq!(apres.mode() & 0o7777, 0o640, "#6635 : droits conservés");
        assert_eq!((apres.uid(), apres.gid()), (avant.uid(), avant.gid()));
    }

    #[test]
    fn graver_tags_6635_format_sans_empreinte_refuse() {
        let dir = crate::test_scratch::scratch_dir("tune-6635-dsf");
        let cible = dir.path().join("a.dsf");
        std::fs::write(&cible, b"DSD ").unwrap();
        let e = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(write_tags_atomique(
                cible.to_str().unwrap(),
                &maj_pop_rock(),
            ))
            .unwrap_err();
        assert_eq!(e, MOTIF_FORMAT_SANS_EMPREINTE);
    }
}
