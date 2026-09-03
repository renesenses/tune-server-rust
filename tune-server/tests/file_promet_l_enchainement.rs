//! Le badge « Gapless » de la file : ce que le serveur promet vraiment (#2934).
//!
//! Jean Valjean, fil forum 631, 15/06/2026, v0.8.114 : « je n'ai pas de Gapless
//! affiché ». Ce n'était pas un défaut d'affichage chez lui — le badge ne
//! s'affichait JAMAIS, pour personne, sur aucune zone.
//!
//! Le client lit `queueTrack.gapless_next` (`QueueView.svelte`) sur les lignes
//! de `GET /zones/{id}/queue`. Côté serveur, `gapless_next` n'était un champ
//! d'AUCUNE structure sérialisée : ni `QueueEntry` (ce que la route rend), ni
//! `QueueTrack`, ni `Track`. Les treize occurrences du mot dans le code Rust
//! étaient toutes des noms de journaux ou de fonctions
//! (`local_audio_gapless_next_met`, `resolve_gapless_next_local_file`…). Le
//! champ ne survivait que dans `docs/contrat-web.json`, hérité du serveur
//! Python : un contrat que le serveur ne tenait pas.
//!
//! Ce que ce fichier cloue :
//!
//! 1. le champ est bien rendu, et il vaut vrai QUAND l'enchaînement aura
//!    réellement lieu, faux quand il n'aura pas lieu — des deux côtés ;
//! 2. la réponse DÉPEND DE LA SORTIE : la même file promet sur une sortie qui
//!    chaîne et ne promet pas quand la piste suivante est du DSD sur un
//!    renderer DLNA (#402) ;
//! 3. le témoin : le reste de la réponse ne change pas de forme, `gapless_next`
//!    est la SEULE clef nouvelle — un client 0.9.131 déjà installé continue de
//!    fonctionner.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

fn app_et_etat() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Une zone posée EN BASE : `POST /zones` refuse une sortie dont l'appareil
/// n'existe pas sur la machine, et ce qui est éprouvé ici n'est pas la
/// création.
fn zone(state: &AppState, nom: &str, output_type: &str, device_id: &str) -> i64 {
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some(output_type), Some(device_id))
        .expect("creation de zone")
}

/// Enregistre une sortie factice. `MockOutput` déclare `can_gapless = true` :
/// c'est une sortie qui sait chaîner depuis sa propre boucle.
async fn brancher_sortie(state: &AppState, device_id: &str, output_type: &str) {
    state.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(device_id, "Sortie d'essai")
            .with_type(output_type),
    ));
}

fn piste(state: &AppState, titre: &str, format: &str) -> i64 {
    let mut t = Track::new(titre.into());
    t.format = Some(format.into());
    t.file_path = Some(format!("/musique/{titre}.{format}"));
    t.duration_ms = 240_000;
    TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insertion de piste")
}

async fn enfiler(app: &axum::Router, zone_id: i64, pistes: &[i64]) {
    let (status, corps) = poster(
        app,
        &format!("/api/v1/zones/{zone_id}/queue/add"),
        json!({ "track_ids": pistes }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mise en file : {corps}");
}

/// `gapless_next` de chaque ligne, dans l'ordre de la file.
async fn promesses(app: &axum::Router, zone_id: i64) -> Vec<Value> {
    let (status, corps) = lire(app, &format!("/api/v1/zones/{zone_id}/queue")).await;
    assert_eq!(status, StatusCode::OK, "lecture de la file : {corps}");
    corps["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .map(|t| t["gapless_next"].clone())
        .collect()
}

/// Le cas de Jean Valjean : deux FLAC locaux, une sortie qui chaîne, le
/// réglage de zone actif. La première ligne promet, la dernière ne promet rien
/// — elle n'a personne à qui s'enchaîner.
#[tokio::test]
async fn la_file_promet_l_enchainement_quand_il_aura_lieu() {
    let (app, state) = app_et_etat();
    let zid = zone(&state, "Salon", "mock", "sortie-salon");
    brancher_sortie(&state, "sortie-salon", "mock").await;
    let a = piste(&state, "A", "flac");
    let b = piste(&state, "B", "flac");
    let c = piste(&state, "C", "flac");
    enfiler(&app, zid, &[a, b, c]).await;

    assert_eq!(
        promesses(&app, zid).await,
        vec![json!(true), json!(true), json!(false)],
        "les deux premieres lignes s'enchainent, la derniere n'a pas de suivant"
    );
}

/// Le réglage de zone `gapless_enabled` commande : le poller n'entre même pas
/// dans sa branche d'armement quand il est à zéro.
#[tokio::test]
async fn zone_sans_gapless_ne_promet_rien() {
    let (app, state) = app_et_etat();
    let zid = zone(&state, "Bureau", "mock", "sortie-bureau");
    brancher_sortie(&state, "sortie-bureau", "mock").await;
    let a = piste(&state, "A", "flac");
    let b = piste(&state, "B", "flac");
    enfiler(&app, zid, &[a, b]).await;

    // Le témoin AVANT : la même file promet quand le réglage est actif.
    assert_eq!(promesses(&app, zid).await, vec![json!(true), json!(false)]);

    ZoneRepo::with_backend(state.backend.clone())
        .update_gapless_enabled(zid, false)
        .expect("reglage de zone");

    assert_eq!(
        promesses(&app, zid).await,
        vec![json!(false), json!(false)],
        "gapless coupe sur la zone : plus rien n'est promis"
    );
}

/// Sortie inconnue — zone navigateur, sortie disparue : on ne promet pas ce
/// qu'on ne peut pas constater. Le commentaire d'`output_capabilities` le dit
/// déjà pour les capacités ; la file le tient.
#[tokio::test]
async fn sortie_non_branchee_ne_promet_rien() {
    let (app, state) = app_et_etat();
    let zid = zone(&state, "Fantome", "dlna", "sortie-absente");
    // AUCUN `brancher_sortie` ici.
    let a = piste(&state, "A", "flac");
    let b = piste(&state, "B", "flac");
    enfiler(&app, zid, &[a, b]).await;

    assert_eq!(promesses(&app, zid).await, vec![json!(false), json!(false)]);
}

/// #402 — un renderer DLNA accepte `SetNextAVTransportURI` pour un flux DSD et
/// ne le consomme jamais (HiFi Rose RS130, Benjithom) : le poller refuse
/// d'armer (`gapless_skipped_dsd_next_dlna`), donc la file ne le promet pas.
///
/// La contre-épreuve est dans le même test : ce n'est ni « DLNA » seul ni
/// « DSD » seul qui refuse, c'est le COUPLE. La même sortie DLNA promet vers un
/// FLAC, et le même DSD est promis sur une sortie locale, qui garde sa chaîne
/// interne.
#[tokio::test]
async fn dsd_sur_dlna_nest_pas_promis_mais_dsd_en_local_lest() {
    let (app, state) = app_et_etat();

    let dlna = zone(&state, "Rose", "dlna", "sortie-rose");
    brancher_sortie(&state, "sortie-rose", "dlna").await;
    let a = piste(&state, "DlnaA", "flac");
    let dsd = piste(&state, "DlnaB", "dsf");
    let c = piste(&state, "DlnaC", "flac");
    // La ligne 0 va vers du DSD (refusée) ; la ligne 1 va vers du FLAC (promise).
    enfiler(&app, dlna, &[a, dsd, c]).await;
    assert_eq!(
        promesses(&app, dlna).await,
        vec![json!(false), json!(true), json!(false)],
        "sur DLNA seul le saut VERS du DSD est refuse"
    );

    let local = zone(&state, "Carte son", "local", "sortie-locale");
    brancher_sortie(&state, "sortie-locale", "local").await;
    let d = piste(&state, "LocA", "flac");
    let e = piste(&state, "LocB", "dsf");
    enfiler(&app, local, &[d, e]).await;
    assert_eq!(
        promesses(&app, local).await,
        vec![json!(true), json!(false)],
        "la meme piste DSD est promise sur une sortie locale"
    );
}

/// LE TÉMOIN — un client 0.9.131 déjà installé continue de fonctionner.
///
/// L'enveloppe garde exactement ses trois clefs, et chaque ligne garde
/// exactement les clefs de `QueueEntry`. `gapless_next` est la SEULE clef
/// nouvelle : rien n'a été retiré, rien n'a changé de type.
#[tokio::test]
async fn temoin_la_reponse_ne_gagne_que_gapless_next() {
    let (app, state) = app_et_etat();
    let zid = zone(&state, "Temoin", "mock", "sortie-temoin");
    brancher_sortie(&state, "sortie-temoin", "mock").await;
    let a = piste(&state, "A", "flac");
    let b = piste(&state, "B", "flac");
    enfiler(&app, zid, &[a, b]).await;

    let (status, corps) = lire(&app, &format!("/api/v1/zones/{zid}/queue")).await;
    assert_eq!(status, StatusCode::OK);

    let mut enveloppe: Vec<&str> = corps
        .as_object()
        .expect("objet")
        .keys()
        .map(String::as_str)
        .collect();
    enveloppe.sort_unstable();
    assert_eq!(
        enveloppe,
        vec!["length", "position", "tracks"],
        "l'enveloppe de la file ne change pas"
    );
    assert_eq!(corps["length"], 2);
    assert_eq!(corps["position"], 0);

    // Les clefs de `QueueEntry` telles qu'un client 0.9.131 les connaît.
    let mut attendues = vec![
        "id",
        "zone_id",
        "track_id",
        "position",
        "is_current",
        "source",
        "source_id",
        "title",
        "artist_name",
        "album_title",
        "duration_ms",
        "file_path",
        "cover_path",
        "format",
        "sample_rate",
        "bit_depth",
        "track_number",
        "disc_number",
        // …plus la seule nouveauté.
        "gapless_next",
    ];
    attendues.sort_unstable();

    for (i, ligne) in corps["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .enumerate()
    {
        let mut clefs: Vec<&str> = ligne
            .as_object()
            .expect("ligne de file")
            .keys()
            .map(String::as_str)
            .collect();
        clefs.sort_unstable();
        assert_eq!(clefs, attendues, "clefs de la ligne {i}");
        assert!(
            ligne["gapless_next"].is_boolean(),
            "gapless_next est un booleen, jamais nul"
        );
    }

    // Les valeurs que le client lisait déjà sont intactes.
    assert_eq!(corps["tracks"][0]["title"], "A");
    assert_eq!(corps["tracks"][1]["title"], "B");
    assert_eq!(corps["tracks"][0]["format"], "flac");
    assert_eq!(corps["tracks"][0]["position"], 0);
    assert_eq!(corps["tracks"][1]["position"], 1);
}
