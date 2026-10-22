//! #6467 — « Lire » un artiste : jusqu'à 50 de ses titres, au hasard.
//!
//! Fil forum 2302 (FabienM), points 10 et 12 : « quand on joue un artiste
//! (clic sur lecture sur la vignette de l'artiste), ça joue seulement un
//! album local alors que ça devrait jouer un ensemble de titres aléatoire de
//! l'artiste » ; « on peut fixer à 50 titres aléatoires max la lecture d'un
//! artiste ».
//!
//! `POST /zones/{id}/play` accepte désormais `artist_id` (un artiste de la
//! BIBLIOTHÈQUE) : le serveur tire au hasard au plus [`LIMITE_PAR_DEFAUT`]
//! titres parmi ceux de l'artiste — la même liste que `GET
//! /library/artists/{id}/tracks`, dédoublonnée de la même façon, titres
//! bannis du profil écartés — et les joue, contexte `artist`. Un client peut
//! demander moins (`artist_limit`), jamais plus.
//!
//! Tirer côté serveur plutôt que dans le client web : l'appli Android et les
//! clients tiers ont le même bouton à brancher, et une liste de 50 ids n'a
//! pas à traverser le réseau pour revenir telle quelle.

use tune_core::db::track_repo::{TrackRepo, dedup_display_tracks};

use crate::state::AppState;

/// Le plafond demandé par le testeur (point 12).
pub(super) const LIMITE_PAR_DEFAUT: usize = 50;

/// Un tirage sans remise de `limite` éléments, dans un ordre aléatoire
/// (Fisher-Yates partiel sur xorshift64). Déterministe pour une `graine`
/// donnée, pour les tests ; la route la prend à l'horloge.
pub(super) fn tirer(mut ids: Vec<i64>, limite: usize, graine: u64) -> Vec<i64> {
    let mut etat = graine | 1;
    let n = ids.len();
    let k = limite.min(n);
    for i in 0..k {
        etat ^= etat << 13;
        etat ^= etat >> 7;
        etat ^= etat << 17;
        let j = i + (etat % (n - i) as u64) as usize;
        ids.swap(i, j);
    }
    ids.truncate(k);
    ids
}

fn graine_de_l_horloge() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

/// Les titres à jouer pour « Lire » `artist_id` : au plus `limite` (bornée à
/// [`LIMITE_PAR_DEFAUT`]), tirés au hasard, sans les titres bannis du profil.
/// Vide quand l'artiste n'a rien de jouable : la route répond alors comme
/// pour un album vide.
pub(super) fn pistes_d_artiste_au_hasard(
    state: &AppState,
    artist_id: i64,
    profile_id: i64,
    limite: Option<usize>,
) -> Vec<i64> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let ids: Vec<i64> = dedup_display_tracks(repo.list_by_artist(artist_id).unwrap_or_default())
        .iter()
        .filter_map(|t| t.id)
        .collect();
    let bannis = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone())
        .banned_track_ids(profile_id, &ids)
        .unwrap_or_default();
    let jouables: Vec<i64> = ids.into_iter().filter(|id| !bannis.contains(id)).collect();
    let limite = limite
        .filter(|l| *l > 0)
        .unwrap_or(LIMITE_PAR_DEFAUT)
        .min(LIMITE_PAR_DEFAUT);
    let disponibles = jouables.len();
    let tirees = tirer(jouables, limite, graine_de_l_horloge());
    tracing::info!(
        artist_id,
        disponibles,
        tirees = tirees.len(),
        bannis = bannis.len(),
        "lecture_artiste_au_hasard"
    );
    tirees
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_tirage_est_sans_remise_et_borne() {
        let ids: Vec<i64> = (1..=120).collect();
        let t = tirer(ids.clone(), 50, 42);
        assert_eq!(t.len(), 50);
        let mut uniques = t.clone();
        uniques.sort_unstable();
        uniques.dedup();
        assert_eq!(uniques.len(), 50, "aucun doublon");
        assert!(t.iter().all(|id| ids.contains(id)));
        // Un artiste de 12 titres les joue tous.
        let petit: Vec<i64> = (1..=12).collect();
        let mut tout = tirer(petit.clone(), 50, 7);
        tout.sort_unstable();
        assert_eq!(tout, petit);
    }

    #[test]
    fn le_tirage_melange_vraiment() {
        let ids: Vec<i64> = (1..=120).collect();
        assert_ne!(tirer(ids.clone(), 50, 42), ids[..50].to_vec());
        assert_ne!(tirer(ids.clone(), 50, 42), tirer(ids, 50, 1_000));
    }
}
