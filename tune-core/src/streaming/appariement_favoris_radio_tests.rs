//! Favoris radio → playlist Qobuz : appariement et création, contre un
//! **faux serveur Qobuz** lié sur `127.0.0.1:0`.
//!
//! Le connecteur est le VRAI `QobuzService` (requête `/catalog/search`,
//! lecture de la réponse, `/playlist/create`, `/playlist/addTracks`) ; seule
//! la base de l'API est redirigée. Aucun appel n'atteint Qobuz, aucun
//! identifiant ni jeton réel n'apparaît ici (le jeton est « jeton-essai »).
//!
//! Le faux moteur de recherche est volontairement STRICT : un titre n'est
//! rendu que si TOUS les mots de la requête figurent dans « titre + artiste »
//! (accents repliés, comme Qobuz). Une requête qui porte encore
//! « (feat. X) » ne rend donc rien — c'est le cas réel de #1235, qui oblige
//! l'appariement à retenter avec le titre tronqué.

use super::*;
use crate::streaming::qobuz::QobuzService;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Ce que le faux serveur a reçu : (chemin, requête ou corps).
type Journal = Arc<Mutex<Vec<(String, String)>>>;

/// Une piste du faux catalogue : (id, titre, interprète).
type Piste = (u64, &'static str, &'static str);

fn replier(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            _ => c,
        })
        .collect()
}

fn mots(s: &str) -> Vec<String> {
    replier(s)
        .split(|c: char| c.is_whitespace())
        .filter(|m| !m.is_empty() && *m != "&" && *m != "-")
        .map(str::to_string)
        .collect()
}

#[derive(Clone, Copy)]
struct Pannes {
    recherche: bool,
    ajout: bool,
}

async fn faux_qobuz(catalogue: Vec<Piste>, pannes: Pannes) -> (QobuzService, Journal) {
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));

    let j = journal.clone();
    let recherche = move |Query(q): Query<HashMap<String, String>>| {
        let j = j.clone();
        let catalogue = catalogue.clone();
        async move {
            let requete = q.get("query").cloned().unwrap_or_default();
            j.lock()
                .unwrap()
                .push(("/catalog/search".into(), requete.clone()));
            if pannes.recherche {
                return (StatusCode::UNAUTHORIZED, "{\"status\":\"error\"}").into_response();
            }
            let demandes = mots(&requete);
            let items: Vec<_> = catalogue
                .iter()
                .filter(|(_, titre, artiste)| {
                    let botte = mots(&format!("{titre} {artiste}"));
                    demandes.iter().all(|m| botte.contains(m))
                })
                .map(|(id, titre, artiste)| {
                    json!({
                        "id": id,
                        "title": titre,
                        "performer": {"name": artiste},
                        "album": {"id": format!("alb-{id}"), "title": "Album d'essai"},
                        "duration": 200,
                    })
                })
                .collect();
            let n = items.len();
            Json(json!({"tracks": {"items": items, "total": n, "limit": 10, "offset": 0}}))
                .into_response()
        }
    };

    let j = journal.clone();
    let creation = move |corps: String| {
        let j = j.clone();
        async move {
            j.lock().unwrap().push(("/playlist/create".into(), corps));
            Json(json!({"id": 987_654, "name": "x"}))
        }
    };

    let j = journal.clone();
    let ajout = move |corps: String| {
        let j = j.clone();
        async move {
            j.lock()
                .unwrap()
                .push(("/playlist/addTracks".into(), corps));
            if pannes.ajout {
                return (StatusCode::BAD_REQUEST, "{\"status\":\"error\"}").into_response();
            }
            Json(json!({"id": 987_654})).into_response()
        }
    };

    // La playlist existante 555 contient déjà 101 (La Bohème).
    let j = journal.clone();
    let lecture = move |Query(q): Query<HashMap<String, String>>| {
        let j = j.clone();
        async move {
            let id = q.get("playlist_id").cloned().unwrap_or_default();
            j.lock().unwrap().push(("/playlist/get".into(), id.clone()));
            if id != "555" {
                return (StatusCode::NOT_FOUND, "{\"status\":\"error\"}").into_response();
            }
            Json(json!({
                "id": 555,
                "name": "Ma playlist",
                "tracks": {"items": [{"id": 101, "title": "La Bohème", "performer": {"name": "Charles Aznavour"}}], "total": 1, "limit": 50, "offset": 0}
            }))
            .into_response()
        }
    };

    let app = Router::new()
        .route("/playlist/get", get(lecture))
        .route("/catalog/search", get(recherche))
        .route("/playlist/create", post(creation))
        .route("/playlist/addTracks", post(ajout));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (
        QobuzService::connecte_a_base_forcee(format!("http://{adresse}")),
        journal,
    )
}

const SANS_PANNE: Pannes = Pannes {
    recherche: false,
    ajout: false,
};

fn catalogue() -> Vec<Piste> {
    vec![
        (101, "La Bohème", "Charles Aznavour"),
        (102, "Under The Strikes", "Yannis & The Yaw"),
        (103, "Heroes (2017 Remaster)", "David Bowie"),
        (104, "Heroes", "David Bowie"),
        (105, "Hallelujah", "Jeff Buckley"),
        (106, "Hallelujah", "Leonard Cohen"),
        (107, "Ashes to Ashes (2017 Remaster)", "David Bowie"),
    ]
}

/// Corps `application/x-www-form-urlencoded` → champs décodés.
fn champs_de(corps: &str) -> HashMap<String, String> {
    corps
        .split('&')
        .filter_map(|paire| paire.split_once('='))
        .map(|(k, v)| {
            let d = |s: &str| {
                urlencoding::decode(&s.replace('+', " "))
                    .map(|c| c.into_owned())
                    .unwrap_or_default()
            };
            (d(k), d(v))
        })
        .collect()
}

fn recherches(j: &Journal) -> Vec<String> {
    j.lock()
        .unwrap()
        .iter()
        .filter(|(c, _)| c == "/catalog/search")
        .map(|(_, q)| q.clone())
        .collect()
}

#[tokio::test]
async fn accents_absents_a_l_antenne_retrouvent_le_titre_accentue() {
    // La radio écrit « LA BOHEME », Qobuz « La Bohème ».
    let (svc, _j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let a = apparier_favori_radio(&svc, "qobuz", "LA BOHEME", "Charles Aznavour").await;
    assert_eq!(a.statut(), StatutAppariement::Matched, "{a:?}");
    assert_eq!(a.meilleur.as_ref().map(|(t, _)| t.id.as_str()), Some("101"));
}

#[tokio::test]
async fn feat_dans_le_titre_capte_retente_sans_le_suffixe() {
    // #1235 : la requête complète ne rend rien chez Qobuz ; le titre tronqué
    // avant « ( » trouve la piste.
    let (svc, j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let a = apparier_favori_radio(
        &svc,
        "qobuz",
        "Under the Strikes (feat. Tony Allen)",
        "Yannis & The Yaw",
    )
    .await;
    assert_eq!(a.statut(), StatutAppariement::Matched, "{a:?}");
    assert_eq!(a.meilleur.as_ref().map(|(t, _)| t.id.as_str()), Some("102"));
    let r = recherches(&j);
    assert_eq!(
        r.last().map(String::as_str),
        Some("Yannis & The Yaw Under the Strikes"),
        "la dernière requête doit être le titre SANS « (feat. …) » : {r:?}"
    );
    assert_eq!(r.len(), 3, "trois requêtes, la troisième trouve : {r:?}");
}

#[tokio::test]
async fn version_remasterisee_seule_au_catalogue_est_retenue() {
    // Le favori dit « Ashes to Ashes », Qobuz n'a que la réédition.
    let (svc, _j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let a = apparier_favori_radio(&svc, "qobuz", "Ashes to Ashes", "David Bowie").await;
    assert_eq!(
        a.meilleur.as_ref().map(|(t, _)| t.id.as_str()),
        Some("107"),
        "{a:?}"
    );
    assert_eq!(a.statut(), StatutAppariement::Matched, "{a:?}");
}

#[tokio::test]
async fn original_et_remaster_ne_font_pas_une_ambiguite() {
    // « Heroes » et « Heroes (2017 Remaster) » : le même morceau du même
    // artiste. Le signaler « ambigu » ferait valider à la main chaque titre
    // réédité, sans rien apprendre à l'auditeur.
    let (svc, _j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let a = apparier_favori_radio(&svc, "qobuz", "Heroes", "David Bowie").await;
    let mut ids: Vec<&str> = a
        .meilleur
        .iter()
        .chain(a.alternatives.iter())
        .map(|(t, _)| t.id.as_str())
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec!["103", "104"],
        "les deux éditions sont proposées, l'une retenue, l'autre en alternative : {a:?}"
    );
    assert_eq!(a.statut(), StatutAppariement::Matched, "{a:?}");
}

#[tokio::test]
async fn meme_titre_deux_artistes_sans_artiste_capte_est_ambigu() {
    let (svc, _j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let a = apparier_favori_radio(&svc, "qobuz", "Hallelujah", "").await;
    assert_eq!(a.statut(), StatutAppariement::Ambiguous, "{a:?}");
    let ids: Vec<&str> = std::iter::once(a.meilleur.as_ref().map(|(t, _)| t.id.as_str()))
        .flatten()
        .chain(a.alternatives.iter().map(|(t, _)| t.id.as_str()))
        .collect();
    assert!(ids.contains(&"105") && ids.contains(&"106"), "{ids:?}");
}

#[tokio::test]
async fn l_artiste_capte_leve_l_ambiguite() {
    // Contre-épreuve du précédent : avec l'artiste, plus d'hésitation.
    let (svc, _j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let a = apparier_favori_radio(&svc, "qobuz", "Hallelujah", "Jeff Buckley").await;
    assert_eq!(a.statut(), StatutAppariement::Matched, "{a:?}");
    assert_eq!(a.meilleur.as_ref().map(|(t, _)| t.id.as_str()), Some("105"));
}

#[tokio::test]
async fn titre_introuvable_est_signale_et_non_force() {
    let (svc, j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let a = apparier_favori_radio(
        &svc,
        "qobuz",
        "Un titre que Qobuz n a jamais eu",
        "Personne du tout",
    )
    .await;
    assert_eq!(a.statut(), StatutAppariement::NotFound, "{a:?}");
    assert!(a.meilleur.is_none());
    assert!(!recherches(&j).is_empty(), "la recherche doit être PARTIE");
}

#[tokio::test]
async fn recherche_en_panne_n_est_pas_un_introuvable() {
    let (svc, _j) = faux_qobuz(
        catalogue(),
        Pannes {
            recherche: true,
            ajout: false,
        },
    )
    .await;
    let a = apparier_favori_radio(&svc, "qobuz", "Heroes", "David Bowie").await;
    assert_eq!(a.statut(), StatutAppariement::SearchFailed, "{a:?}");
}

#[test]
fn titre_icy_composite_est_scinde() {
    assert_eq!(
        scinder_titre_icy("David Bowie - Heroes", ""),
        ("Heroes".to_string(), "David Bowie".to_string())
    );
    // Un artiste présent : on ne touche à rien.
    assert_eq!(
        scinder_titre_icy("Heroes - Single Version", "David Bowie"),
        (
            "Heroes - Single Version".to_string(),
            "David Bowie".to_string()
        )
    );
}

#[tokio::test]
async fn la_playlist_est_creee_avec_les_titres_retenus_sans_doublon() {
    let (svc, j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let ids: Vec<String> = ["101", "104", "101", " "]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let r = creer_playlist_avec_titres(&svc, "Favoris radio — 14/10/2026", &ids)
        .await
        .expect("création");
    assert_eq!(r.playlist_id, "987654");
    assert_eq!(r.ajoutes, 2);
    assert_eq!(r.doublons, 2, "un doublon et un identifiant vide écartés");
    assert!(r.erreur_ajout.is_none());

    let journal = j.lock().unwrap().clone();
    let creation = journal
        .iter()
        .find(|(c, _)| c == "/playlist/create")
        .expect("/playlist/create appelé");
    let champs: HashMap<String, String> = champs_de(&creation.1);
    assert_eq!(
        champs.get("name").map(String::as_str),
        Some("Favoris radio — 14/10/2026"),
        "le nom choisi part tel quel, tiret long compris"
    );
    assert_eq!(champs.get("is_public").map(String::as_str), Some("false"));
    let ajout = journal
        .iter()
        .find(|(c, _)| c == "/playlist/addTracks")
        .expect("/playlist/addTracks appelé");
    let champs: HashMap<String, String> = champs_de(&ajout.1);
    assert_eq!(
        champs.get("playlist_id").map(String::as_str),
        Some("987654")
    );
    assert_eq!(champs.get("track_ids").map(String::as_str), Some("101,104"));
}

#[tokio::test]
async fn un_ajout_refuse_est_rendu_et_non_avale() {
    let (svc, _j) = faux_qobuz(
        catalogue(),
        Pannes {
            recherche: false,
            ajout: true,
        },
    )
    .await;
    let r = creer_playlist_avec_titres(&svc, "Favoris radio", &["101".to_string()])
        .await
        .expect("la création elle-même réussit");
    assert_eq!(r.playlist_id, "987654");
    assert_eq!(r.ajoutes, 0);
    assert!(r.erreur_ajout.is_some(), "{r:?}");
}

#[tokio::test]
async fn ajout_a_une_playlist_existante_saute_les_titres_deja_presents() {
    // Décision du 14/10 : un titre déjà dans la playlist cible n'y entre pas
    // deux fois. 101 y est déjà ; seul 104 doit partir.
    let (svc, j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let ids: Vec<String> = ["101", "104", "104"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let r = ajouter_a_playlist_existante(&svc, "555", &ids)
        .await
        .expect("ajout");
    assert_eq!(r.playlist_id, "555");
    assert_eq!(r.ajoutes, 1, "{r:?}");
    assert_eq!(r.deja_presents, 1, "{r:?}");
    assert_eq!(r.doublons, 1, "{r:?}");
    let journal = j.lock().unwrap().clone();
    assert!(
        journal.iter().all(|(c, _)| c != "/playlist/create"),
        "aucune playlist ne doit être créée : {journal:?}"
    );
    let ajout = journal
        .iter()
        .find(|(c, _)| c == "/playlist/addTracks")
        .expect("/playlist/addTracks appelé");
    let champs = champs_de(&ajout.1);
    assert_eq!(champs.get("playlist_id").map(String::as_str), Some("555"));
    assert_eq!(
        champs.get("track_ids").map(String::as_str),
        Some("104"),
        "101 est déjà dans la playlist cible : il ne doit pas être renvoyé"
    );
}

#[tokio::test]
async fn tout_deja_present_n_appelle_pas_qobuz_pour_rien() {
    let (svc, j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    let r = ajouter_a_playlist_existante(&svc, "555", &["101".to_string()])
        .await
        .expect("ajout");
    assert_eq!((r.ajoutes, r.deja_presents), (0, 1), "{r:?}");
    assert!(
        j.lock()
            .unwrap()
            .iter()
            .all(|(c, _)| c != "/playlist/addTracks")
    );
}

#[tokio::test]
async fn playlist_cible_illisible_n_ajoute_rien() {
    let (svc, j) = faux_qobuz(catalogue(), SANS_PANNE).await;
    ajouter_a_playlist_existante(&svc, "999", &["104".to_string()])
        .await
        .expect_err("sans lire la cible, on ne sait pas éviter les doublons");
    assert!(
        j.lock()
            .unwrap()
            .iter()
            .all(|(c, _)| c != "/playlist/addTracks")
    );
}
