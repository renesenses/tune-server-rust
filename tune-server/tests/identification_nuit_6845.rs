//! #6845 — une nuit simulée de la passe de nuit d'identification, contre un
//! faux MusicBrainz, l'horloge donnée par l'essai.
//!
//! **Hermétique : aucun appel réel à MusicBrainz.** Doublure locale
//! (127.0.0.1, port éphémère) comme `identification_choix_4805.rs`, qui note
//! chaque recherche. Le limiteur partagé reste en place : l'essai dure
//! quelques secondes réelles, la nuit simulée, elle, n'attend rien.
//!
//! La veille tourne toutes les 5 minutes de 1 h 50 à 3 h 10, sur une fenêtre
//! réglée de 2 h à 3 h et un délai réglé de 20 minutes entre deux albums.
//! Cinq albums sont dans la liste des refus. On attend :
//! - aucun départ avant 2 h, un seul départ à 2 h ;
//! - trois albums traités (2 h 00, 2 h 20, 2 h 40), dans l'ordre de la liste ;
//! - l'arrêt à 3 h, heure de fin : les deux derniers ne sont pas envoyés à
//!   MusicBrainz et restent dans la liste ;
//! - l'album refusé de nouveau (`503`) reste dans la liste, l'album identifié
//!   et l'album introuvable en sortent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::routes::library::{Horloge, un_tour_de_veille_avec};

/// La date de la nuit simulée.
const NUIT: &str = "2026-10-27";

/// Les cinq albums, dans l'ordre de la liste des refus, et ce que le faux
/// MusicBrainz répond à leur recherche.
const ALBUMS: [(i64, &str); 5] = [
    (1, "Nefertiti"),         // un pressage sûr : identifié
    (2, "Kind of Blue"),      // 503 : refusé de nouveau
    (3, "Bitches Brew"),      // aucune release : introuvable
    (4, "Sketches of Spain"), // un pressage sûr, mais après 3 h
    (5, "Milestones"),        // un pressage sûr, mais après 3 h
];

/// L'horloge de l'essai : des secondes depuis minuit, une seule date.
/// `attendre` avance l'horloge sans rien attendre.
struct HorlogeSimulee(AtomicU64);

impl HorlogeSimulee {
    fn a(heure: u64, minute: u64) -> Self {
        Self(AtomicU64::new(heure * 3600 + minute * 60))
    }
    fn hh_mm(&self) -> String {
        let s = self.0.load(Ordering::SeqCst);
        format!("{:02}:{:02}", s / 3600, (s % 3600) / 60)
    }
}

impl Horloge for HorlogeSimulee {
    fn heure(&self) -> u8 {
        ((self.0.load(Ordering::SeqCst) / 3600) % 24) as u8
    }
    fn date(&self) -> String {
        NUIT.into()
    }
    fn attendre(&self, duree: Duration) -> impl std::future::Future<Output = ()> + Send {
        self.0.fetch_add(duree.as_secs(), Ordering::SeqCst);
        std::future::ready(())
    }
}

/// Les recherches reçues, chacune notée par le titre d'album qu'elle porte,
/// avec l'heure simulée à laquelle elle est partie.
type Journal = Arc<Mutex<Vec<(String, String)>>>;

fn hit(id: &str, titre: &str) -> Value {
    json!({
        "id": id,
        "score": 100,
        "title": titre,
        "status": "Official",
        "track-count": 2,
        "artist-credit": [{ "name": "Miles Davis", "joinphrase": "" }],
        "release-group": { "id": format!("rg-{id}") },
    })
}

fn release(id: &str) -> Option<Value> {
    let titre = ALBUMS
        .iter()
        .find(|(n, _)| format!("rel-{n}") == id)
        .map(|(_, t)| *t)?;
    Some(json!({
        "id": id,
        "title": titre,
        "status": "Official",
        "release-group": { "id": format!("rg-{id}") },
        "artist-credit": [{ "name": "Miles Davis", "joinphrase": "" }],
        "media": [{ "position": 1, "tracks": [
            { "position": 1, "title": "Un", "recording": { "id": format!("rec-{id}-1") } },
            { "position": 2, "title": "Deux", "recording": { "id": format!("rec-{id}-2") } }
        ]}],
    }))
}

async fn faux_musicbrainz(horloge: Arc<HorlogeSimulee>) -> Journal {
    let journal: Journal = Arc::default();
    let j = journal.clone();
    let app = axum::Router::new()
        .route(
            "/release",
            axum::routing::get(move |Query(q): Query<HashMap<String, String>>| {
                let j = j.clone();
                let horloge = horloge.clone();
                async move {
                    let requete = q.get("query").cloned().unwrap_or_default();
                    let Some(&(n, titre)) = ALBUMS.iter().find(|(_, t)| requete.contains(t)) else {
                        return axum::Json(json!({ "releases": [] })).into_response();
                    };
                    j.lock().unwrap().push((titre.to_string(), horloge.hh_mm()));
                    match n {
                        2 => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                        3 => axum::Json(json!({ "releases": [] })).into_response(),
                        _ => axum::Json(json!({ "releases": [hit(&format!("rel-{n}"), titre)] }))
                            .into_response(),
                    }
                }
            }),
        )
        .route(
            "/release/{id}",
            axum::routing::get(move |Path(id): Path<String>| async move {
                match release(&id) {
                    Some(v) => axum::Json(v).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    tune_core::metadata::musicbrainz_release::remplacer_la_base_musicbrainz(Some(format!(
        "http://{adresse}"
    )));
    journal
}

async fn appeler(app: &axum::Router, path: &str) -> Value {
    let requete = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(requete).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{path}");
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

fn liste_des_refus(reglages: &SettingsRepo) -> Vec<i64> {
    serde_json::from_str(
        &reglages
            .get("identification_refus_musicbrainz")
            .unwrap()
            .unwrap_or_else(|| "[]".into()),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_nuit_traite_la_liste_garde_les_refus_et_s_arrete_a_l_heure_de_fin() {
    let horloge = Arc::new(HorlogeSimulee::a(1, 50));
    let journal = faux_musicbrainz(horloge.clone()).await;
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    for (id, titre) in ALBUMS {
        state
            .backend
            .execute(
                "INSERT INTO albums (id, title, source) VALUES (?, ?, 'local')",
                &[&id as &dyn ToSqlValue, &titre as &dyn ToSqlValue],
            )
            .unwrap();
        for (rang, nom) in [(1i64, "Un"), (2, "Deux")] {
            let piste = id * 10 + rang;
            state
                .backend
                .execute(
                    "INSERT INTO tracks (id, title, album_id, source, track_number, disc_number) \
                     VALUES (?, ?, ?, 'local', ?, 1)",
                    &[
                        &piste as &dyn ToSqlValue,
                        &nom as &dyn ToSqlValue,
                        &id as &dyn ToSqlValue,
                        &rang as &dyn ToSqlValue,
                    ],
                )
                .unwrap();
        }
    }
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    // Un lot passé a vu MusicBrainz refuser les cinq albums.
    reglages
        .set("identification_refus_musicbrainz", "[1,2,3,4,5]")
        .unwrap();
    reglages.set("identification_nuit_active", "true").unwrap();
    // La fenêtre et le délai réglés (#6845).
    reglages
        .set("identification_nuit_heure_debut", "2")
        .unwrap();
    reglages.set("identification_nuit_heure_fin", "3").unwrap();
    reglages.set("identification_nuit_delai_s", "1200").unwrap();
    let app = tune_server::routes::router(state.clone());
    let avant = appeler(&app, "/api/v1/library/identify-all/nuit").await;
    assert_eq!(avant["fenetre"], json!({ "debut": 2, "fin": 3 }), "{avant}");
    assert_eq!(avant["delai_entre_albums_s"], 1200, "{avant}");
    assert_eq!(avant["refus_en_attente"], 5, "{avant}");

    // La veille, toutes les 5 minutes, de 1 h 50 à 3 h 10.
    let mut tours = Vec::new();
    let mut fin_de_la_nuit = None;
    while horloge.hh_mm().as_str() <= "03:10" {
        let a = horloge.hh_mm();
        let issue = un_tour_de_veille_avec(&state, horloge.as_ref()).await;
        if issue.is_ok() {
            fin_de_la_nuit = Some(horloge.hh_mm());
        }
        tours.push((a, issue));
        horloge.attendre(Duration::from_secs(300)).await;
    }
    let departs: Vec<&str> = tours
        .iter()
        .filter(|(_, issue)| issue.is_ok())
        .map(|(a, _)| a.as_str())
        .collect();
    assert_eq!(departs, vec!["02:00"], "un seul départ, à 2 h : {tours:?}");
    assert!(
        tours
            .iter()
            .filter(|(_, issue)| issue.is_err())
            .all(|(_, issue)| *issue == Err("hors_fenetre")),
        "hors de la fenêtre, la veille ne part pas : {tours:?}"
    );
    assert_eq!(
        fin_de_la_nuit.as_deref(),
        Some("03:00"),
        "la nuit doit s'arrêter à l'heure de fin"
    );

    // Les albums traités : les trois premiers, à 20 minutes d'écart.
    assert_eq!(
        *journal.lock().unwrap(),
        vec![
            ("Nefertiti".to_string(), "02:00".to_string()),
            ("Kind of Blue".to_string(), "02:20".to_string()),
            ("Bitches Brew".to_string(), "02:40".to_string()),
        ],
        "la passe a continué après l'heure de fin, ou n'a pas suivi le délai réglé"
    );

    // Les refus retenus : l'album refusé de nouveau, et les deux albums que
    // la fin de la fenêtre n'a pas laissé partir.
    assert_eq!(
        liste_des_refus(&reglages),
        vec![2, 4, 5],
        "la liste des refus est fausse après la nuit"
    );
    let pressage = state
        .backend
        .query_one(
            "SELECT musicbrainz_release_id FROM albums WHERE id = 1",
            &[],
        )
        .unwrap()
        .and_then(|ligne| ligne[0].as_string());
    assert_eq!(
        pressage.as_deref(),
        Some("rel-1"),
        "l'album 1 est identifié"
    );

    // Le bilan que lit l'écran.
    let apres = appeler(&app, "/api/v1/library/identify-all/nuit").await;
    let bilan = &apres["derniere_nuit"];
    assert_eq!(bilan["raison"], "fin_de_fenetre", "{apres}");
    assert_eq!(bilan["date"], NUIT, "{apres}");
    assert_eq!(bilan["total"], 5, "{apres}");
    assert_eq!(bilan["traites"], 3, "{apres}");
    assert_eq!(bilan["identifies"], 1, "{apres}");
    assert_eq!(bilan["encore_refuses"], 1, "{apres}");
    assert_eq!(apres["refus_en_attente"], 3, "{apres}");
    assert_eq!(
        reglages
            .get("identification_nuit_derniere")
            .unwrap()
            .as_deref(),
        Some(NUIT)
    );

    tune_core::metadata::musicbrainz_release::remplacer_la_base_musicbrainz(None);
}
