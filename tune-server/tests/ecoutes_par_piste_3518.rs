//! #3518 — « # Plays » et « Last Played » sur les listes de pistes.
//!
//! La maquette V1 (Levente, 07/09/2026) transforme la liste de pistes d'un
//! album en tableau à colonnes choisies. Dix des douze colonnes étaient
//! couvertes ; `play_count` et `last_played_at` ne l'étaient par AUCUNE route.
//! Le client les déclarait `indisponible` : grisées dans les réglages, écartées
//! du rendu même si un réglage plus ancien les cochait.
//!
//! ## Pourquoi ce test passe par la ROUTE MONTÉE
//!
//! Le défaut le plus fréquent de ce dépôt est « écrit mais pas branché ». Un
//! test qui appellerait `HistoryRepo::plays_for_tracks` directement resterait
//! VERT alors que la route ne l'appelle pas — c'est-à-dire alors que le ticket
//! n'est pas réglé. Ici on monte `tune_server::routes::router(state)` et on
//! envoie de vraies requêtes HTTP : la donnée doit traverser le handler, la
//! sérialisation et la recopie.
//!
//! ## La cause que ces essais gardent
//!
//! `listen_history.track_id` est **toujours NULL** : le seul site qui écrit
//! l'historique (`orchestrator::commun::record_listen`) passe `track_id: None`.
//! Une jointure sur cette colonne rendrait `play_count = 0` pour la
//! bibliothèque entière — la colonne fausse que l'issue refuse. Les écoutes
//! posées ici le sont donc SANS `track_id`, comme en production.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite")
}

/// Un album de deux pistes du même interprète.
fn bibliotheque(state: &AppState) {
    let backend = &state.backend;
    backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis')",
            &[],
        )
        .expect("artiste");
    backend
        .execute(
            "INSERT INTO albums (id, title, artist_id, track_count) \
             VALUES (1, 'Kind of Blue', 1, 2)",
            &[],
        )
        .expect("album");
    for (id, titre, piste) in [(1, "So What", 1), (2, "Blue in Green", 2)] {
        backend
            .execute(
                &format!(
                    "INSERT INTO tracks (id, title, artist_id, album_id, track_number, file_path) \
                     VALUES ({id}, '{titre}', 1, 1, {piste}, '/musique/{id}.flac')"
                ),
                &[],
            )
            .expect("piste");
    }
}

/// Une écoute, **sans `track_id`** — exactement ce que l'orchestrateur écrit.
fn ecoute(state: &AppState, titre: &str, quand: &str) {
    state
        .backend
        .execute(
            &format!(
                "INSERT INTO listen_history \
                   (track_id, title, artist_name, album_title, source, listened_at) \
                 VALUES (NULL, '{titre}', 'Miles Davis', 'Kind of Blue', 'local', '{quand}')"
            ),
            &[],
        )
        .expect("ecoute");
}

async fn json(routeur: Router, chemin: &str) -> Value {
    let r = routeur
        .oneshot(
            Request::builder()
                .uri(chemin)
                .body(Body::empty())
                .expect("requete"),
        )
        .await
        .expect("reponse");
    assert_eq!(r.status(), StatusCode::OK, "chemin: {chemin}");
    let octets = axum::body::to_bytes(r.into_body(), 4 << 20)
        .await
        .expect("corps");
    serde_json::from_slice(&octets).expect("json")
}

/// La vérification que l'issue demande, jouée sur la vraie route :
///
/// ```text
/// curl -s "$TUNE/api/v1/library/albums/1/tracks" | jq '.[0] | {play_count, last_played_at}'
/// ```
///
/// Les deux clés doivent exister, et `play_count` doit refléter les écoutes.
///
/// Contre-épreuve : retirez `attacher_ecoutes(&state, &mut items)` de
/// `album_tracks` et cet essai passe au rouge — la route rend les 31 champs
/// d'avant, sans les deux colonnes.
#[tokio::test]
async fn les_pistes_d_un_album_portent_le_compte_et_la_derniere_ecoute() {
    let state = etat();
    bibliotheque(&state);
    for quand in ["2026-09-01T10:00:00Z", "2026-09-05T21:30:00Z"] {
        ecoute(&state, "So What", quand);
    }
    let corps = json(
        tune_server::routes::router(state),
        "/api/v1/library/albums/1/tracks",
    )
    .await;
    let pistes = corps.as_array().expect("un tableau de pistes");
    assert_eq!(pistes.len(), 2);

    let jouee = pistes
        .iter()
        .find(|p| p["title"] == "So What")
        .expect("la piste jouee");
    assert_eq!(
        jouee["play_count"], 2,
        "deux ecoutes doivent se compter, malgre un track_id NULL en base"
    );
    assert_eq!(
        jouee["last_played_at"], "2026-09-05T21:30:00Z",
        "la DERNIERE ecoute, pas la premiere"
    );

    let jamais = pistes
        .iter()
        .find(|p| p["title"] == "Blue in Green")
        .expect("la piste jamais jouee");
    assert_eq!(
        jamais["play_count"], 0,
        "ici un zero est une information, pas une absence"
    );
    assert!(
        jamais["last_played_at"].is_null(),
        "jamais jouee = null, et la cle doit exister pour que le client \
         puisse afficher « jamais joue » au lieu d'une case vide"
    );
}

/// Les deux clés existent TOUJOURS, y compris sur une bibliothèque sans une
/// seule écoute. C'est ce qui permet au client de basculer un seul drapeau :
/// une clé parfois absente l'obligerait à distinguer « route muette » de
/// « piste jamais jouée ».
#[tokio::test]
async fn les_deux_cles_existent_meme_sans_aucune_ecoute() {
    let state = etat();
    bibliotheque(&state);
    let corps = json(
        tune_server::routes::router(state),
        "/api/v1/library/albums/1/tracks",
    )
    .await;
    for piste in corps.as_array().expect("un tableau") {
        assert_eq!(piste["play_count"], 0);
        assert!(
            piste
                .as_object()
                .expect("un objet")
                .contains_key("last_played_at"),
            "la cle doit etre presente, a null"
        );
    }
}

/// « Idéalement sur la même route que les autres listes de pistes, pour que le
/// tableau ait les mêmes colonnes partout. » `/library/tracks` sert le MÊME
/// tableau dans la maquette : sans cette moitié, l'écran aurait les colonnes
/// sur un album et rien dans la table des titres.
#[tokio::test]
async fn la_table_des_titres_porte_les_memes_colonnes() {
    let state = etat();
    bibliotheque(&state);
    ecoute(&state, "So What", "2026-09-05T21:30:00Z");
    let corps = json(
        tune_server::routes::router(state),
        "/api/v1/library/tracks?limit=10",
    )
    .await;
    let items = corps["items"].as_array().expect("des pistes");
    assert!(
        !items.is_empty(),
        "la table des titres ne doit pas etre vide"
    );
    let jouee = items
        .iter()
        .find(|p| p["title"] == "So What")
        .expect("la piste jouee");
    assert_eq!(jouee["play_count"], 1);
    assert_eq!(jouee["last_played_at"], "2026-09-05T21:30:00Z");
}

/// Une écoute de RADIO ne compte pas. Le titre d'une ligne de radio est un
/// instantané figé qui ne correspond pas à ce qui passait vraiment — même
/// exclusion que `top_tracks` et que la liste d'historique. Sans elle, une
/// webradio ayant annoncé « So What » gonflerait le compte d'une piste que
/// personne n'a jouée.
#[tokio::test]
async fn une_ecoute_de_radio_ne_gonfle_pas_le_compte() {
    let state = etat();
    bibliotheque(&state);
    state
        .backend
        .execute(
            "INSERT INTO listen_history \
               (track_id, title, artist_name, album_title, source, listened_at) \
             VALUES (NULL, 'So What', 'Miles Davis', 'Kind of Blue', 'radio', \
                     '2026-09-05T21:30:00Z')",
            &[],
        )
        .expect("ecoute radio");
    let corps = json(
        tune_server::routes::router(state),
        "/api/v1/library/albums/1/tracks",
    )
    .await;
    let jouee = corps
        .as_array()
        .expect("un tableau")
        .iter()
        .find(|p| p["title"] == "So What")
        .expect("la piste");
    assert_eq!(jouee["play_count"], 0);
    assert!(jouee["last_played_at"].is_null());
}
