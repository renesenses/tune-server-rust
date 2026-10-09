//! Les balises d'une piste extraite (#2466), écrites par `lofty` — la
//! bibliothèque que le scan de Tune emploie pour les relire.
//!
//! FLAC : commentaires Vorbis et bloc PICTURE. WAV : étiquette ID3v2 (chunk
//! `id3 `), la seule qui porte numéro de disque, MBIDs et image.
//!
//! Les clés sont celles que le scan lit (`metadata/mod.rs`) :
//! `MUSICBRAINZ_ALBUMID` (sortie), `MUSICBRAINZ_TRACKID` (enregistrement),
//! `MUSICBRAINZ_RELEASETRACKID` (piste de la sortie), `MUSICBRAINZ_ARTISTID`,
//! `MUSICBRAINZ_ALBUMARTISTID`.

use std::path::Path;

use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{ItemKey, ItemValue, Tag, TagItem, TagType};

use super::Format;

/// Ce qu'on écrit sur une piste.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Balises {
    pub titre: String,
    pub artiste: String,
    pub album: String,
    pub artiste_album: String,
    pub numero: u32,
    pub total_pistes: u32,
    pub disque: u32,
    pub disques: u32,
    pub date: Option<String>,
    pub release_id: Option<String>,
    pub recording_id: Option<String>,
    pub piste_id: Option<String>,
    pub artiste_ids: Vec<String>,
    pub artiste_album_ids: Vec<String>,
}

/// Le type d'une image d'après ses premiers octets. `None` : ni JPEG ni PNG,
/// l'image n'est pas écrite.
pub fn type_d_image(octets: &[u8]) -> Option<MimeType> {
    if octets.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(MimeType::Jpeg)
    } else if octets.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(MimeType::Png)
    } else {
        None
    }
}

/// Écrit `b` (et la pochette, si elle est JPEG ou PNG) dans `chemin`, dont
/// le format est lu dans le CONTENU : le fichier peut encore porter son nom
/// provisoire `.part`.
pub fn ecrire(
    chemin: &Path,
    format: Format,
    b: &Balises,
    pochette: Option<&[u8]>,
) -> Result<(), String> {
    let mut tag = Tag::new(match format {
        Format::Flac => TagType::VorbisComments,
        Format::Wav => TagType::Id3v2,
    });
    let mut texte = |cle: ItemKey, v: &str| {
        if !v.is_empty() {
            tag.push(TagItem::new(cle, ItemValue::Text(v.to_string())));
        }
    };
    texte(ItemKey::TrackTitle, &b.titre);
    texte(ItemKey::TrackArtist, &b.artiste);
    texte(ItemKey::AlbumTitle, &b.album);
    texte(ItemKey::AlbumArtist, &b.artiste_album);
    texte(ItemKey::TrackNumber, &b.numero.to_string());
    if b.total_pistes > 0 {
        texte(ItemKey::TrackTotal, &b.total_pistes.to_string());
    }
    texte(ItemKey::DiscNumber, &b.disque.max(1).to_string());
    texte(ItemKey::DiscTotal, &b.disques.max(1).to_string());
    if let Some(d) = &b.date {
        texte(ItemKey::RecordingDate, d);
    }
    for (cle, v) in [
        (ItemKey::MusicBrainzReleaseId, &b.release_id),
        (ItemKey::MusicBrainzRecordingId, &b.recording_id),
        (ItemKey::MusicBrainzTrackId, &b.piste_id),
    ] {
        if let Some(v) = v {
            texte(cle, v);
        }
    }
    for id in &b.artiste_ids {
        texte(ItemKey::MusicBrainzArtistId, id);
    }
    for id in &b.artiste_album_ids {
        texte(ItemKey::MusicBrainzReleaseArtistId, id);
    }
    let image = pochette.and_then(|octets| {
        type_d_image(octets).map(|mime| {
            Picture::unchecked(octets.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(mime)
                .build()
        })
    });
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(chemin)
        .map_err(|e| format!("ouverture pour les balises : {e}"))?;
    match format {
        Format::Flac => {
            if let Some(i) = image {
                tag.push_picture(i);
            }
            tag.save_to(&mut f, WriteOptions::default())
        }
        Format::Wav => {
            id3_avec_identifiants(tag, b, image).save_to(&mut f, WriteOptions::default())
        }
    }
    .map_err(|e| format!("écriture des balises : {e}"))
}

/// MESURÉ (lofty 0.24) : le `Tag` générique écrit en ID3v2 garde
/// `MUSICBRAINZ_ARTISTID` mais PERD l'identifiant de sortie, de piste et
/// d'enregistrement, et la pochette. Ils sont posés ici dans les trames que
/// le lecteur de lofty relit : `TXXX` par description, `UFID` du
/// propriétaire MusicBrainz pour l'enregistrement, `APIC` pour l'image.
fn id3_avec_identifiants(
    tag: Tag,
    b: &Balises,
    image: Option<Picture>,
) -> lofty::id3::v2::Id3v2Tag {
    use lofty::id3::v2::{Frame, Id3v2Tag, UniqueFileIdentifierFrame};
    let mut id3 = Id3v2Tag::from(tag);
    let mut txxx = |description: &str, valeur: Option<String>| {
        if let Some(v) = valeur.filter(|v| !v.is_empty())
            && id3.get_user_text(description).is_none()
        {
            id3.insert_user_text(description.to_string(), v);
        }
    };
    txxx("MusicBrainz Album Id", b.release_id.clone());
    txxx("MusicBrainz Release Track Id", b.piste_id.clone());
    txxx("MusicBrainz Artist Id", b.artiste_ids.first().cloned());
    txxx(
        "MusicBrainz Album Artist Id",
        b.artiste_album_ids.first().cloned(),
    );
    if let Some(rec) = b.recording_id.as_ref().filter(|r| !r.is_empty()) {
        id3.insert(Frame::UniqueFileIdentifier(UniqueFileIdentifierFrame::new(
            "http://musicbrainz.org",
            rec.as_bytes().to_vec(),
        )));
    }
    if let Some(i) = image {
        id3.insert_picture(i);
    }
    id3
}
