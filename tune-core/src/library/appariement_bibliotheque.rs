//! Apparier un titre CONNU (titre, artiste, ISRC, durée) sur la bibliothèque
//! LOCALE — le pendant de [`crate::streaming::matching`] pour le sens
//! service → bibliothèque.
//!
//! # Pourquoi (#4716, tranche 1 de l'épique #4715)
//!
//! L'interface hôte WASM n'exposait que le sens bibliothèque → service : un
//! convertisseur de playlists pouvait lire une playlist locale et l'écrire
//! chez Qobuz, mais pas l'inverse — le greffon « Playlists converter » refusait
//! explicitement « transférer vers la bibliothèque » faute de savoir chercher
//! et apparier localement.
//!
//! # Rien n'est réécrit
//!
//! Les deux gestes existaient déjà, dispersés : la recherche est celle de
//! [`TrackRepo::search`] (l'index plein texte de la bibliothèque), et
//! l'appariement celui de [`crate::library::track_matcher`] — le même que les
//! favoris radio locaux (`routes/radios.rs`) enchaînent à la main depuis
//! #1235. Ce module les NOMME ensemble, comme
//! [`crate::streaming::matching::apparier_chez_le_service`] l'a fait pour le
//! sens sortant.

use crate::db::models::Track;
use crate::db::track_repo::TrackRepo;
use crate::library::track_matcher::{MatchCandidate, classer_candidats};
use crate::streaming::matching::{LIMITE_RECHERCHE_APPARIEMENT, MATCH_APPROX_SCORE};

/// Les pistes locales, traduites en candidats d'appariement.
///
/// L'INDICE sert d'identifiant de candidat : une piste peut n'avoir pas encore
/// d'`id` (fiche en cours d'import), et deux pistes peuvent partager un titre.
fn candidats(pistes: &[Track]) -> Vec<MatchCandidate> {
    pistes
        .iter()
        .enumerate()
        .map(|(i, t)| MatchCandidate {
            title: t.title.clone(),
            artist_name: t.artist_name.clone().unwrap_or_default(),
            album_title: t.album_title.clone().unwrap_or_default(),
            source_id: i.to_string(),
            duration_ms: t.duration_ms,
            // Quand la piste locale porte un ISRC, le chemin RAPIDE de
            // `find_best_match` peut trancher avant tout score approché.
            isrc: t.isrc.clone().unwrap_or_default(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        })
        .collect()
}

/// Classer des pistes LOCALES déjà trouvées pour un titre connu, le verdict en
/// tête et au plus `max` candidats.
///
/// Comme côté service, plusieurs candidats sont rendus : l'appelant qui
/// applique ensuite sa propre règle (la tolérance de durée du greffon) doit
/// pouvoir redescendre d'un cran au lieu de conclure « introuvable ».
pub fn classer_en_bibliotheque<'a>(
    title: &str,
    artist: &str,
    isrc: &str,
    duration_ms: i64,
    pistes: &'a [Track],
    max: usize,
) -> Vec<(&'a Track, f64)> {
    if pistes.is_empty() {
        return Vec::new();
    }
    let candidats = candidats(pistes);
    classer_candidats(
        title,
        artist,
        isrc,
        duration_ms,
        &candidats,
        MATCH_APPROX_SCORE,
        max,
    )
    .into_iter()
    .filter_map(|(i, score)| pistes.get(i).map(|t| (t, score)))
    .collect()
}

/// Chercher un titre connu EN BIBLIOTHÈQUE, puis l'apparier — les deux gestes
/// enchaînés, comme [`crate::streaming::matching::apparier_chez_le_service`]
/// les enchaîne chez un service.
///
/// La requête est celle des favoris radio locaux (`"{artiste} {titre}"`,
/// l'artiste seul étant facultatif) et la profondeur celle du transfert de
/// playlist ([`LIMITE_RECHERCHE_APPARIEMENT`]) : un seul jeu de valeurs pour
/// les deux sens.
pub fn apparier_en_bibliotheque(
    repo: &TrackRepo,
    title: &str,
    artist: &str,
    isrc: &str,
    duration_ms: i64,
    max: usize,
) -> Result<Vec<(Track, f64)>, String> {
    let requete = if artist.is_empty() {
        title.to_string()
    } else {
        format!("{artist} {title}")
    };
    let pistes = repo
        .search(&requete, LIMITE_RECHERCHE_APPARIEMENT as i64)
        .map_err(|e| e.to_string())?;
    Ok(
        classer_en_bibliotheque(title, artist, isrc, duration_ms, &pistes, max)
            .into_iter()
            .map(|(t, score)| (t.clone(), score))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piste(id: i64, titre: &str, artiste: &str, duree_ms: i64) -> Track {
        Track {
            id: Some(id),
            title: titre.into(),
            album_id: None,
            album_title: None,
            artist_id: None,
            artist_name: Some(artiste.into()),
            album_artist: None,
            disc_number: 1,
            disc_subtitle: None,
            track_number: 1,
            duration_ms: duree_ms,
            file_path: None,
            format: None,
            sample_rate: None,
            bit_depth: None,
            channels: 2,
            file_mtime: None,
            file_size: None,
            audio_hash: None,
            source: "local".into(),
            source_id: None,
            isrc: None,
            genre: None,
            genres: None,
            composer: None,
            year: None,
            bpm: None,
            label: None,
            musicbrainz_recording_id: None,
            cover_path: None,
            comments: None,
            cue_media_path: None,
            cue_start_ms: None,
            cue_end_ms: None,
        }
    }

    /// 🔴 Le manque que cette tranche répare, côté bibliothèque : le meilleur
    /// candidat rate la tolérance de durée du greffon (±3 s), et un autre
    /// l'aurait tenue. Un seul verdict rendu, et le titre est perdu.
    #[test]
    fn un_second_candidat_local_sauve_le_titre_quand_le_premier_rate_la_duree() {
        let source_ms = 150_000i64;
        let pistes = vec![
            piste(1, "La Bohème", "Charles Aznavour", 210_000),
            piste(2, "La Bohème", "Charles Aznavour", 150_500),
        ];
        let classes =
            classer_en_bibliotheque("La Boheme", "Charles Aznavour", "", source_ms, &pistes, 5);
        assert_eq!(classes.len(), 2, "{classes:?}");
        assert_eq!(classes[0].0.id, Some(1), "le verdict reste le verdict");
        let retenu = classes
            .iter()
            .find(|(t, _)| (t.duration_ms - source_ms).abs() <= 3_000)
            .expect("un candidat doit tenir la tolérance de durée");
        assert_eq!(retenu.0.id, Some(2));
    }

    /// Rien de plausible en bibliothèque : aucun candidat de dépit.
    #[test]
    fn aucune_piste_plausible_rend_un_classement_vide() {
        let pistes = vec![piste(1, "Totalement autre chose", "Quelqu'un", 200_000)];
        assert!(
            classer_en_bibliotheque("La Boheme", "Charles Aznavour", "", 0, &pistes, 5).is_empty()
        );
        assert!(classer_en_bibliotheque("La Boheme", "", "", 0, &[], 5).is_empty());
    }

    /// L'ISRC tranche ici aussi, même quand le titre local est mal étiqueté.
    #[test]
    fn l_isrc_local_prime_sur_le_titre() {
        let mut porteuse = piste(2, "Titre mal etiquete", "V.A.", 200_000);
        porteuse.isrc = Some("FRUM71600123".into());
        let pistes = vec![piste(1, "La Bohème", "Charles Aznavour", 210_000), porteuse];
        let classes = classer_en_bibliotheque(
            "La Boheme",
            "Charles Aznavour",
            "FRUM71600123",
            210_000,
            &pistes,
            5,
        );
        assert_eq!(classes[0].0.id, Some(2), "{classes:?}");
        assert_eq!(classes[0].1, 1.0);
    }
}
