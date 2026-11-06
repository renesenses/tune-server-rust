//! #7695 — retirer de `tracks` les photos et vidéos qu'une indexation UPnP y
//! avait rangées comme des pistes.
//!
//! # Le défaut
//!
//! La synchronisation d'une source UPnP (`routes/indexation_upnp.rs`,
//! `recolter`) retenait chaque `<item>` doté d'un titre et d'une URL. Le
//! partage multimédia de Windows d'un portable publie aussi les photos du
//! poste : des centaines de « pistes » DNG sans artiste sont entrées dans la
//! bibliothèque et dans Oxygen.
//!
//! La récolte les écarte désormais. Mais celles qui sont déjà en base n'en
//! sortent pas seules : la synchronisation ne retire que ce qu'une passe
//! COMPLÈTE ne revoit plus, après confirmation, et rien ne se passe tant que
//! le portable est éteint.
//!
//! # Ce que la passe retire, et rien d'autre
//!
//! Une ligne n'est retirée que si les TROIS conditions tiennent :
//!
//! 1. `source = 'upnp'` ;
//! 2. `file_path IS NULL` (jamais une piste locale adoptée) ;
//! 3. et son format est celui d'une image ou d'une vidéo
//!    (`FORMATS_NON_AUDIO` — le format rangé est le sous-type du MIME,
//!    `image/x-adobe-dng` → `adobe-dng`), **ou**, faute de format rangé, son
//!    URL de lecture se termine par une extension de photo ou de vidéo
//!    (`EXTENSIONS_NON_AUDIO`).
//!
//! Comme à la récolte, un format rangé autre (donc un MIME audio) l'emporte
//! sur l'extension : une piste `flac` dont l'URL finit en `.avi` reste.
//!
//! Les sous-types ambigus (`mpeg`, `mp4`, `webm`, `3gpp`, `matroska`, `raw`,
//! qui existent aussi en `audio/…` — `audio/x-matroska` est servi par Tune
//! lui-même) ne sont PAS des critères, ni les extensions `.mp4`, `.mkv`,
//! `.3gp` : un vrai fichier audio ne doit pas partir.
//!
//! Une ligne qu'un favori (`item_type = 'track'`) ou une playlist référence est
//! **laissée** et comptée : la passe ne supprime jamais ce que l'utilisateur a
//! rangé lui-même (doctrine de `synchronisation_upnp::remove_missing`). Les
//! albums distants que la passe a vidés sont retirés, sauf en favori.
//!
//! Idempotente : sur une base déjà passée, une lecture et rien d'écrit.

use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};

/// Les sous-types MIME (sans `x-`, en minuscules) qui ne sont jamais du son.
const FORMATS_NON_AUDIO: &[&str] = &[
    "jpeg",
    "jpg",
    "pjpeg",
    "png",
    "gif",
    "bmp",
    "ms-bmp",
    "tiff",
    "heic",
    "heif",
    "webp",
    "dng",
    "adobe-dng",
    "dcraw",
    "canon-cr2",
    "canon-cr3",
    "nikon-nef",
    "sony-arw",
    "olympus-orf",
    "panasonic-rw2",
    "fuji-raf",
    "samsung-srw",
    "pentax-pef",
    "msvideo",
    "quicktime",
    "ms-wmv",
    "m4v",
];

/// Les extensions d'URL qui ne sont jamais du son. Même liste que la récolte
/// (`EXTENSIONS_NON_AUDIO`, `routes/indexation_upnp.rs`) ; `.mp4`, `.mkv` et
/// `.3gp` n'y sont pas.
const EXTENSIONS_NON_AUDIO: &[&str] = &[
    "dng", "jpg", "jpeg", "jpe", "png", "gif", "bmp", "tif", "tiff", "heic", "heif", "webp", "raw",
    "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf", "srw", "pef", "avi", "mov", "wmv", "m4v",
    "mpg", "mpeg", "mts", "m2ts",
];

/// Les candidates : les trois conditions de l'en-tête — l'extension ne compte
/// que faute de format rangé. Les listes sont des
/// constantes du module, jamais une saisie : les inscrire dans le texte SQL
/// n'ouvre rien.
fn candidates() -> String {
    let formats = FORMATS_NON_AUDIO
        .iter()
        .map(|f| format!("'{f}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let extensions = EXTENSIONS_NON_AUDIO
        .iter()
        .map(|e| format!("LOWER(m.value) LIKE '%.{e}' OR LOWER(m.value) LIKE '%.{e}?%'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        "SELECT t.id, t.album_id FROM tracks t \
         WHERE t.source = 'upnp' AND t.file_path IS NULL \
           AND (LOWER(COALESCE(t.format, '')) IN ({formats}) \
                OR (COALESCE(t.format, '') = '' \
                    AND EXISTS (SELECT 1 FROM track_metadata m \
                                 WHERE m.track_id = t.id AND m.key = 'upnp_res_url' \
                                   AND ({extensions}))))"
    )
}

/// Une candidate qu'un favori de piste ou une playlist référence.
const LIEE: &str = "SELECT 1 FROM tracks t WHERE t.id = ? AND ( \
       EXISTS (SELECT 1 FROM playlist_tracks pt WHERE pt.track_id = t.id) \
       OR EXISTS (SELECT 1 FROM favorites f \
                   WHERE f.item_type = 'track' AND f.item_id = CAST(t.id AS TEXT)))";

/// Un album distant que la passe a vidé, et que rien ne retient.
const ALBUM_VIDE: &str = "DELETE FROM albums WHERE id = ? AND source = 'upnp' \
       AND NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id) \
       AND NOT EXISTS (SELECT 1 FROM favorites f WHERE f.item_type = 'album' \
                        AND f.item_id = CAST(albums.id AS TEXT))";

/// Ce que la passe a fait.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Bilan {
    /// Photos et vidéos retirées de `tracks`.
    pub retirees: usize,
    /// Lignes laissées parce qu'un favori ou une playlist les référence.
    pub gardees_liees: Vec<i64>,
    /// Albums distants vidés par la passe, retirés.
    pub albums_vides_retires: usize,
}

/// Retire les photos et vidéos indexées comme des pistes. Voir l'en-tête du
/// module.
pub fn retirer_les_photos_indexees(db: &Arc<dyn DbBackend>) -> Result<Bilan, String> {
    let lignes: Vec<(i64, Option<i64>)> = db
        .query_many(&candidates(), &[])?
        .iter()
        .filter_map(|r| {
            let id = r.first().and_then(|v| v.as_i64())?;
            Some((id, r.get(1).and_then(|v| v.as_i64())))
        })
        .collect();
    let mut bilan = Bilan::default();
    if lignes.is_empty() {
        return Ok(bilan);
    }
    db.write_tx(&mut |tx| {
        // Relu dans la transaction : un favori a pu être posé depuis la lecture.
        let (mut retirees, mut gardees, mut albums) = (0usize, Vec::new(), Vec::new());
        for (id, album) in &lignes {
            let p: [&dyn ToSqlValue; 1] = [id];
            if tx.query_one(LIEE, &p)?.is_some() {
                gardees.push(*id);
                continue;
            }
            tx.execute("DELETE FROM upnp_library_members WHERE track_id = ?", &p)?;
            retirees += tx.execute(
                "DELETE FROM tracks WHERE id = ? AND source = 'upnp' AND file_path IS NULL",
                &p,
            )?;
            if let Some(a) = album
                && !albums.contains(a)
            {
                albums.push(*a);
            }
        }
        let mut albums_vides = 0usize;
        for a in &albums {
            let p: [&dyn ToSqlValue; 1] = [a];
            albums_vides += tx.execute(ALBUM_VIDE, &p)?;
        }
        bilan = Bilan {
            retirees,
            gardees_liees: gardees,
            albums_vides_retires: albums_vides,
        };
        Ok(())
    })?;
    Ok(bilan)
}

/// La passe du démarrage : journalise, n'interrompt jamais le démarrage.
pub fn passe_journalisee(db: &Arc<dyn DbBackend>) {
    match retirer_les_photos_indexees(db) {
        Ok(b) if b.retirees == 0 && b.gardees_liees.is_empty() => {}
        Ok(b) => tracing::info!(
            retirees = b.retirees,
            gardees_liees = ?b.gardees_liees,
            albums_vides_retires = b.albums_vides_retires,
            "photos_indexees_upnp_7695_retirees_des_pistes"
        ),
        Err(e) => tracing::warn!(error = %e, "photos_indexees_upnp_7695_passe_echouee"),
    }
}

#[cfg(test)]
#[path = "photos_indexees_upnp_7695_tests.rs"]
mod tests;
