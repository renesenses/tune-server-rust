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
//!
//! Avant le tri, [`ajouter_mes_plus_ecoutes`] (#7416) ajoute à la liste du
//! service les titres de l'artiste les plus écoutés sur ce serveur, toutes
//! sources confondues.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::cloud::ecoutes_communaute::{self, CHAMP, Genre};
use crate::db::backend::DbBackend;
use crate::db::history_repo::{
    EcouteDInterprete, HistoryRepo, PisteDeServiceARapprocher, TOLERANCE_DUREE_ECOUTE_LOCALE_MS,
    nom_rapproche,
};

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

/// Nombre de titres de l'historique ajoutés au plus — autant que la fiche en
/// montre (`TITRES_PHARES_MAX` du client web).
pub const MES_PLUS_ECOUTES_MAX: usize = 10;

/// #7416 — les titres phares d'un service ne sont que SA liste : sur la fiche
/// bibliothèque de Michael Kiwanuka, le top Qobuz n'a que des titres de
/// « FUDGE », et « Love & Hate » (106 écoutes chez Bertrand, surtout en TIDAL)
/// n'y figurait pas. Aucun compteur n'y pouvait rien : le titre n'était pas
/// dans la liste.
///
/// Décision de Bertrand (30/10/2026, option a) : la liste AJOUTE les titres
/// de l'artiste les plus écoutés dans l'historique, toutes sources confondues
/// (fichiers et services), puis [`enrichir_et_classer`] classe l'ensemble.
///
/// L'artiste est celui que nomment le plus souvent les titres du service. Les
/// écoutes se regroupent en MORCEAUX selon la règle du compteur
/// ([`HistoryRepo::ecoutes_de_pistes_de_service`]) : titre et interprète
/// normalisés, durées à [`TOLERANCE_DUREE_ECOUTE_LOCALE_MS`] près. Un morceau
/// déjà dans la liste (même identifiant chez ce service, ou mêmes noms à la
/// même durée) n'est pas ajouté une seconde fois. Chaque ajout est désigné
/// par l'enregistrement le plus jouable : ce service d'abord, puis un autre
/// service, puis le fichier de la bibliothèque ; un morceau qu'aucun ne
/// désigne plus (fichier disparu) n'est pas ajouté. Il porte sa `source` :
/// le client ne tamponne que les pistes qui n'en ont pas.
pub fn ajouter_mes_plus_ecoutes(
    backend: &Arc<dyn DbBackend>,
    service: &str,
    items: &mut Vec<Value>,
) {
    let Some(artiste) = artiste_de_la_liste(items) else {
        return;
    };
    let ecoutes = match HistoryRepo::with_backend(backend.clone()).ecoutes_d_un_interprete(&artiste)
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(service, error = %e, "titres_phares_historique_echec");
            return;
        }
    };
    let ajouts: Vec<Value> = morceaux(ecoutes)
        .into_iter()
        .filter(|m| !deja_dans_la_liste(m, service, items))
        .filter_map(|m| piste_jouable(&m, service))
        .take(MES_PLUS_ECOUTES_MAX)
        .collect();
    items.extend(ajouts);
}

/// L'interprète que nomment le plus souvent les titres (à égalité, le
/// premier rencontré) — `None` pour une liste vide.
fn artiste_de_la_liste(items: &[Value]) -> Option<String> {
    let mut comptes: Vec<(String, String, usize)> = Vec::new();
    for v in items {
        let nom = v.get("artist_name").and_then(Value::as_str).unwrap_or("");
        let cle = nom_rapproche(nom);
        if cle.is_empty() {
            continue;
        }
        match comptes.iter_mut().find(|(c, _, _)| *c == cle) {
            Some((_, _, n)) => *n += 1,
            None => comptes.push((cle, nom.trim().to_string(), 1)),
        }
    }
    let max = comptes.iter().map(|(_, _, n)| *n).max()?;
    comptes
        .into_iter()
        .find(|(_, _, n)| *n == max)
        .map(|(_, nom, _)| nom)
}

/// Un morceau écouté : ses enregistrements, du plus écouté au moins écouté,
/// et le total de leurs écoutes.
struct Morceau {
    lignes: Vec<EcouteDInterprete>,
    compte: i64,
}

fn durees_compatibles(a: i64, b: i64) -> bool {
    a <= 0 || b <= 0 || (a - b).abs() <= TOLERANCE_DUREE_ECOUTE_LOCALE_MS
}

/// Les écoutes regroupées en morceaux, du plus écouté au moins écouté (à
/// égalité, le plus récemment écouté d'abord).
fn morceaux(mut ecoutes: Vec<EcouteDInterprete>) -> Vec<Morceau> {
    ecoutes.sort_by(|a, b| b.compte.cmp(&a.compte).then(b.derniere.cmp(&a.derniere)));
    let mut morceaux: Vec<(String, Morceau)> = Vec::new();
    for e in ecoutes {
        let titre = nom_rapproche(&e.titre);
        if titre.is_empty() {
            continue;
        }
        let place = morceaux
            .iter_mut()
            .find(|(t, m)| *t == titre && durees_compatibles(m.lignes[0].duree_ms, e.duree_ms));
        match place {
            Some((_, m)) => {
                m.compte += e.compte;
                m.lignes.push(e);
            }
            None => morceaux.push((
                titre,
                Morceau {
                    compte: e.compte,
                    lignes: vec![e],
                },
            )),
        }
    }
    let mut morceaux: Vec<Morceau> = morceaux.into_iter().map(|(_, m)| m).collect();
    morceaux.sort_by(|a, b| {
        b.compte
            .cmp(&a.compte)
            .then(b.lignes[0].derniere.cmp(&a.lignes[0].derniere))
    });
    morceaux
}

/// Le morceau est-il déjà un titre de la liste ? Même règle que le compteur.
fn deja_dans_la_liste(m: &Morceau, service: &str, items: &[Value]) -> bool {
    let texte = |v: &Value, c: &str| v.get(c).and_then(Value::as_str).unwrap_or("").to_string();
    items.iter().any(|v| {
        let id = texte(v, "source_id");
        let titre = nom_rapproche(&texte(v, "title"));
        let duree = v.get("duration_ms").and_then(Value::as_i64).unwrap_or(0);
        m.lignes.iter().any(|l| {
            (l.source == service && !l.source_id.is_empty() && l.source_id == id.trim())
                || (!titre.is_empty()
                    && nom_rapproche(&l.titre) == titre
                    && durees_compatibles(l.duree_ms, duree))
        })
    })
}

/// Le titre ajouté, désigné par l'enregistrement le plus jouable du morceau.
fn piste_jouable(m: &Morceau, service: &str) -> Option<Value> {
    let rang = |l: &EcouteDInterprete| -> Option<u8> {
        if l.source == "local" {
            l.track_id.map(|_| 2)
        } else if l.source_id.is_empty() {
            None
        } else if l.source == service {
            Some(0)
        } else {
            Some(1)
        }
    };
    // `min_by_key` garde le premier à égalité : le plus écouté.
    let l = m
        .lignes
        .iter()
        .filter_map(|l| rang(l).map(|r| (r, l)))
        .min_by_key(|(r, _)| *r)?
        .1;
    let mut piste = json!({
        "source": l.source,
        "title": l.titre,
        "artist_name": l.artiste,
        "album_title": l.album,
        "duration_ms": l.duree_ms.max(0),
        "cover_path": l.pochette,
    });
    if l.source == "local" {
        piste["id"] = json!(l.track_id);
    } else {
        piste["source_id"] = json!(l.source_id);
    }
    Some(piste)
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
