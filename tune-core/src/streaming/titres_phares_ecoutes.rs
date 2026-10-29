//! #7026 — les « Top tracks » d'une fiche artiste de service, classées par
//! nombre total d'écoutes sur Tune.
//!
//! Bertrand, 25/10/2026, fiche Michael Kiwanuka (Qobuz) : « Dans les top
//! tracks, il faudrait prendre en compte le nb total d'écoutes » — puis :
//! « écoutes globales sur Mozaiklabs ! ». Le total est celui de la
//! communauté, agrégé par le site ([`ecoutes_communaute`], #6738) : seuls
//! les comptes qui ont choisi de partager leurs écoutes y entrent, et le site
//! ne rend un nombre qu'à partir de trois comptes distincts.
//!
//! Sur chaque piste :
//! - `play_count` : ce total, posé seulement quand le site l'a rendu, avec
//!   `play_count_scope: "community"` ;
//! - `last_played_at` et `local_play_count` : les écoutes de CE serveur, tous
//!   profils ([`HistoryRepo::ecoutes_de_pistes_de_service`]) — le site n'a pas
//!   de date, et n'en rendra jamais d'individuelle.
//!
//! Puis le tri (#7107) : `play_count`, puis `local_play_count`, puis le rang
//! Last.fm, puis l'ordre du service. Sans compte relié, envoi des écoutes coupé, site
//! en panne ou trop lent : aucun total, le tri passe aux écoutes de ce
//! serveur, puis à Last.fm, puis à l'ordre du service.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::cloud::ecoutes_communaute::{self, CHAMP, Genre};
use crate::db::backend::DbBackend;
use crate::db::history_repo::{HistoryRepo, PisteDeServiceARapprocher};

/// Le total de la communauté, dans le champ que lit la colonne « # Plays ».
pub const CHAMP_TOTAL: &str = "play_count";
/// Les écoutes de ce serveur, dans le champ que lit « Last played ».
pub const CHAMP_DERNIERE: &str = "last_played_at";
/// Le nombre d'écoutes de ce serveur, à côté du total.
pub const CHAMP_LOCAL: &str = "local_play_count";
/// Ce que compte `play_count` ici : la communauté, pas ce serveur — ailleurs
/// (bibliothèque), `play_count` est local. Le client le dit en infobulle.
pub const CHAMP_PORTEE: &str = "play_count_scope";
/// La valeur de [`CHAMP_PORTEE`].
pub const PORTEE_COMMUNAUTE: &str = "community";

/// Pose les écoutes sur les titres phares d'un service, puis les classe.
pub async fn enrichir_et_classer(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    service: &str,
    items: &mut [Value],
) {
    if items.is_empty() {
        return;
    }
    ecoutes_communaute::annoter(backend, http, Genre::PisteDeService, items).await;
    for item in items.iter_mut() {
        if let Some(o) = item.as_object_mut()
            && let Some(n) = o.get(CHAMP).and_then(Value::as_i64)
        {
            o.insert(CHAMP_TOTAL.into(), json!(n));
            o.insert(CHAMP_PORTEE.into(), json!(PORTEE_COMMUNAUTE));
        }
    }
    poser_ecoutes_locales(backend, service, items);
    classer(items);
}

/// `last_played_at` et `local_play_count` depuis l'historique de ce serveur,
/// en une requête. Une erreur de base ne retire rien : la liste part sans.
pub fn poser_ecoutes_locales(backend: &Arc<dyn DbBackend>, service: &str, items: &mut [Value]) {
    let texte = |v: &Value, c: &str| v.get(c).and_then(Value::as_str).unwrap_or("").to_string();
    let champs: Vec<(String, String, String, i64)> = items
        .iter()
        .map(|v| {
            (
                texte(v, "source_id"),
                texte(v, "title"),
                texte(v, "artist_name"),
                v.get("duration_ms").and_then(Value::as_i64).unwrap_or(0),
            )
        })
        .collect();
    let pistes: Vec<PisteDeServiceARapprocher<'_>> = champs
        .iter()
        .map(|(id, titre, artiste, duree)| PisteDeServiceARapprocher {
            source_id: id,
            titre,
            artiste,
            duree_ms: *duree,
        })
        .collect();
    let ecoutes = match HistoryRepo::with_backend(backend.clone())
        .ecoutes_de_pistes_de_service(service, &pistes)
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(service, error = %e, "titres_phares_ecoutes_locales_echec");
            return;
        }
    };
    for (item, ecoute) in items.iter_mut().zip(ecoutes) {
        if let (Some(o), Some((n, derniere))) = (item.as_object_mut(), ecoute) {
            o.insert(CHAMP_LOCAL.into(), json!(n));
            o.insert(CHAMP_DERNIERE.into(), json!(derniere));
        }
    }
}

/// Le classement des titres phares, à quatre niveaux (#7107) :
/// 1. `play_count` décroissant (le total de la communauté) ;
/// 2. `local_play_count` décroissant (les écoutes de ce serveur, locales et
///    du service) ;
/// 3. `lastfm_rank` croissant (la popularité Last.fm, 1 = le plus écouté) ;
/// 4. l'ordre reçu du service (tri stable : à égalité, il décide).
///
/// Un niveau absent classe après tous les présents, et un niveau n'en
/// départage un autre qu'à égalité du précédent.
pub fn classer(items: &mut [Value]) {
    use crate::streaming::titres_phares_lastfm::CHAMP_RANG;
    let nombre = |v: &Value, c: &str| v.get(c).and_then(Value::as_i64);
    items.sort_by_key(|v| {
        (
            std::cmp::Reverse(nombre(v, CHAMP_TOTAL).unwrap_or(-1)),
            std::cmp::Reverse(nombre(v, CHAMP_LOCAL).unwrap_or(0)),
            nombre(v, CHAMP_RANG).unwrap_or(i64::MAX),
        )
    });
}

#[cfg(test)]
#[path = "titres_phares_ecoutes_tests.rs"]
mod tests;
