//! Les partages SMB montés que la bibliothèque ne lit pas (#6175, fil 2145).
//!
//! Côté serveur, monter un partage (`POST /network/smb/mount`) et le déclarer
//! dans les dossiers de musique (`POST /system/music-dirs/add`) sont deux
//! appels indépendants. Un assistant interrompu entre les deux, une racine
//! retirée ou perdue, et il reste un partage « Monté » à chaque démarrage que
//! le scan ne visite jamais. C'est voulu — un partage peut servir à autre
//! chose qu'à la bibliothèque — mais le serveur le taisait partout :
//!
//! - le rapport de bug de Daniel LEVY (rc2) disait `Music dirs:` vide, sans un
//!   mot des deux partages montés que la capture montrait ;
//! - le scan sans dossier sortait sur `scan_aborted_no_dirs` et un événement
//!   `{"no_dirs": true}`, le scan de démarrage sur `auto_scan_skipped_no_dirs`,
//!   tous deux muets sur le partage qui aurait suffi à remplir la bibliothèque.
//!
//! Ce module dit lesquels. Il ne déclare rien de lui-même : ajouter une racine
//! reste le geste de l'utilisateur (« Ajouter à la bibliothèque »).

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;

/// Un partage SMB enregistré, que l'utilisateur garde monté, sous lequel — et
/// au-dessus duquel — aucune racine de la bibliothèque ne vit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartageSansRacine {
    /// Le point de montage, tel qu'enregistré dans `network_mounts`.
    pub point: String,
    /// Le constat de l'instant : le point est-il monté ? Un partage dont le
    /// remontage a échoué reste listé — le déclarer ne suffirait pas, et
    /// l'explication doit le dire.
    pub monte: bool,
}

/// Une racine couvre le point de montage si elle vit sur le partage (le point
/// lui-même ou un dossier en dessous) ou si le partage vit sous elle
/// (`/mnt` déclaré, partage monté sur `/mnt/nas`).
fn couvert(music_dirs: &[String], point: &str) -> bool {
    !crate::routes::network::racines_dependantes(music_dirs, point).is_empty()
        || music_dirs.iter().any(|racine| {
            !crate::routes::network::racines_dependantes(&[point.to_string()], racine).is_empty()
        })
}

/// Parmi les points de montage `points`, ceux qu'aucune racine ne couvre.
/// Fonction pure : `est_monte` est le constat du système de fichiers,
/// injecté pour que les tests n'aient pas à monter un partage.
pub fn parmi(
    music_dirs: &[String],
    points: &[String],
    est_monte: impl Fn(&Path) -> bool,
) -> Vec<PartageSansRacine> {
    let mut vus: Vec<String> = Vec::new();
    let mut sans_racine = Vec::new();
    for point in points {
        let normalise = tune_core::scanner::walker::normalize_path(point);
        if normalise.is_empty() || vus.contains(&normalise) {
            continue;
        }
        vus.push(normalise);
        if couvert(music_dirs, point) {
            continue;
        }
        sans_racine.push(PartageSansRacine {
            point: point.clone(),
            monte: est_monte(Path::new(point)),
        });
    }
    sans_racine
}

/// Les points de montage des partages SMB que l'utilisateur garde actifs —
/// ceux que le démarrage remonte (`startup.rs`, même filtre).
fn points_smb_actifs(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    match backend.query_many(
        "SELECT mount_path FROM network_mounts \
         WHERE mount_type = 'smb' AND COALESCE(active, 1) = 1 ORDER BY id",
        &[],
    ) {
        Ok(lignes) => lignes
            .into_iter()
            .filter_map(|r| r.first().and_then(|v| v.as_string()))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "partages_sans_racine_lecture_echouee");
            Vec::new()
        }
    }
}

/// Les partages SMB actifs qu'aucune racine de `music_dirs` ne couvre, avec le
/// constat de montage du système.
pub fn releve(backend: &Arc<dyn DbBackend>, music_dirs: &[String]) -> Vec<PartageSansRacine> {
    releve_avec(backend, music_dirs, crate::smb::est_un_point_de_montage)
}

/// [`releve`] avec un constat de montage injecté (tests).
pub fn releve_avec(
    backend: &Arc<dyn DbBackend>,
    music_dirs: &[String],
    est_monte: impl Fn(&Path) -> bool,
) -> Vec<PartageSansRacine> {
    parmi(music_dirs, &points_smb_actifs(backend), est_monte)
}

/// La forme JSON, commune au rapport de bug et à l'événement de fin de scan.
pub fn en_json(partages: &[PartageSansRacine]) -> Value {
    Value::Array(
        partages
            .iter()
            .map(|p| json!({ "mount_path": p.point, "mounted": p.monte }))
            .collect(),
    )
}

/// L'explication en une phrase, ou `None` quand il n'y a rien à expliquer.
///
/// Sans aucune racine, c'est LA cause de la bibliothèque vide, et la phrase
/// le dit. Avec d'autres racines, le partage peut servir à autre chose : la
/// phrase se borne à constater qu'il n'est pas lu.
pub fn explication(music_dirs: &[String], partages: &[PartageSansRacine]) -> Option<String> {
    if partages.is_empty() {
        return None;
    }
    let liste = partages
        .iter()
        .map(|p| {
            if p.monte {
                p.point.clone()
            } else {
                format!("{} (non monté)", p.point)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let pluriel = partages.len() > 1;
    let phrase = match (music_dirs.is_empty(), pluriel) {
        (true, false) => format!(
            "Aucun dossier de musique déclaré : la bibliothèque reste vide. Le partage SMB \
             {liste} est enregistré mais pas déclaré : déclarez-le (Emplacements › Ajouter \
             à la bibliothèque) pour que le scan le lise."
        ),
        (true, true) => format!(
            "Aucun dossier de musique déclaré : la bibliothèque reste vide. Les partages SMB \
             {liste} sont enregistrés mais pas déclarés : déclarez-les (Emplacements › \
             Ajouter à la bibliothèque) pour que le scan les lise."
        ),
        (false, false) => format!(
            "Le partage SMB {liste} n'est pas déclaré dans les dossiers de musique : le scan \
             ne le lit pas."
        ),
        (false, true) => format!(
            "Les partages SMB {liste} ne sont pas déclarés dans les dossiers de musique : le \
             scan ne les lit pas."
        ),
    };
    Some(phrase)
}

#[cfg(test)]
#[path = "partages_sans_racine_tests_6175.rs"]
mod tests;
