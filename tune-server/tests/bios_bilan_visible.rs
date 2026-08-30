//! #1311 — « Enrichissement : bios non disponibles ».
//!
//! ## Ce que ces tests gardent
//!
//! `tune_core::metadata::bio_batch` range le bilan de chaque passe de
//! biographies dans les réglages, sous `artist_bio_enrich_result` et
//! `album_bio_enrich_result`. Il le faisait déjà avant ce correctif — et
//! **personne ne le relisait** : une recherche de ces deux clés dans tout le
//! dépôt ne rendait qu'une seule ligne chacune, celle de l'écriture.
//!
//! Le serveur savait donc dire pourquoi une passe était rentrée à vide, et ne
//! le disait à personne. C'est le défaut derrière le signalement de Fabien :
//! pas un décompte faux, une absence de retour. `GET /system/enrichment/status`
//! relit désormais les deux bilans sous `bio_last_run`.
//!
//! ## Hermétique : aucun appel réseau
//!
//! Aucune passe d'enrichissement n'est lancée ici. Les bilans sont **semés**
//! dans les réglages, exactement sous la forme que `bilan_de_passe` écrit, et
//! la route est interrogée en lecture seule. Ni MusicBrainz, ni Wikipédia, ni
//! Last.fm, ni `mozaiklabs.fr` ne sont joints.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

/// Sème un bilan de passe sous `cle`, dans la forme exacte que
/// `bio_batch::bilan_de_passe` écrit à la fin de chaque passe.
fn semer_bilan(state: &tune_server::state::AppState, cle: &str, bilan: Value) {
    SettingsRepo::with_backend(state.backend.clone())
        .set(cle, &bilan.to_string())
        .expect("ecriture du bilan de passe");
}

async fn statut(app: &axum::Router) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/system/enrichment/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("appel de /system/enrichment/status");
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("corps de la reponse");
    // Le statut et le corps brut sont dans le message : une reponse vide
    // (route mal prefixee, filtre d'authentification) se lit d'un coup d'oeil
    // au lieu d'un « EOF while parsing a value » muet.
    let json: Value = serde_json::from_slice(&octets).unwrap_or_else(|e| {
        panic!(
            "reponse non JSON (statut {statut}) : {e} — corps brut : {:?}",
            String::from_utf8_lossy(&octets)
        )
    });
    (statut, json)
}

/// Le cas de Fabien : la passe a tourné sur 120 artistes et n'en a enrichi
/// **aucun**, parce que 118 d'entre eux n'avaient ni MBID ni clé Last.fm —
/// donc aucune source à interroger.
///
/// Avant ce correctif, la route ne portait rien de tout cela : le bilan était
/// écrit dans les réglages et aucun appelant ne le relisait. L'utilisateur
/// voyait « bios non disponibles » sans pouvoir distinguer une passe qui n'a
/// rien trouvé d'une passe qui n'avait rien à interroger.
///
/// Contre-épreuve : retirer `"bio_last_run": bio_last_run` de la réponse de
/// `enrichment_status` fait rougir ce test.
#[tokio::test]
async fn le_bilan_de_la_derniere_passe_artistes_est_servi() {
    let (app, state) = app_et_etat();
    semer_bilan(
        &state,
        "artist_bio_enrich_result",
        json!({
            "total": 120,
            "enriched": 0,
            "failed": 120,
            "sans_source": 118,
            "lastfm_configure": false,
            "fini_le": "2026-08-30T09:12:44+00:00",
        }),
    );

    let (code, corps) = statut(&app).await;
    assert_eq!(code, StatusCode::OK);

    let artistes = &corps["bio_last_run"]["artists"];
    assert_eq!(
        artistes["total"], 120,
        "le panneau doit dire sur combien d'artistes la passe a porte"
    );
    assert_eq!(artistes["enriched"], 0);
    assert_eq!(
        artistes["sans_source"], 118,
        "l'echec certain — aucune source a interroger — doit remonter jusqu'a l'ecran"
    );
    assert_eq!(
        artistes["lastfm_configure"], false,
        "l'ecran doit pouvoir nommer le reglage que l'utilisateur peut corriger"
    );
    assert_eq!(artistes["fini_le"], "2026-08-30T09:12:44+00:00");
}

/// Chemin sœur : la passe albums range son bilan sous une autre clé. Corriger
/// l'une sans l'autre laisserait la moitié du panneau muette.
///
/// Contre-épreuve : retirer la seule ligne `"albums": …` du bilan servi fait
/// rougir ce test, et lui seul.
#[tokio::test]
async fn le_bilan_de_la_derniere_passe_albums_est_servi_aussi() {
    let (app, state) = app_et_etat();
    semer_bilan(
        &state,
        "album_bio_enrich_result",
        json!({
            "total": 40,
            "enriched": 3,
            "failed": 37,
            "sans_source": 0,
            "lastfm_configure": true,
            "fini_le": "2026-08-30T09:20:01+00:00",
        }),
    );

    let (code, corps) = statut(&app).await;
    assert_eq!(code, StatusCode::OK);

    let albums = &corps["bio_last_run"]["albums"];
    assert_eq!(albums["total"], 40);
    assert_eq!(albums["enriched"], 3);
    assert_eq!(albums["failed"], 37);
}

/// Aucune passe n'a encore tourné : la clé `bio_last_run` doit tout de même
/// être là, à `null`, plutôt que de disparaître.
///
/// Un champ absent et un champ nul ne se lisent pas pareil côté client : le
/// premier ressemble à une version de serveur trop ancienne, le second dit
/// « aucune passe à ce jour ». C'est la différence entre « je ne sais pas
/// répondre » et « la réponse est : rien encore ».
#[tokio::test]
async fn sans_passe_le_bilan_est_nul_et_non_absent() {
    let (app, _state) = app_et_etat();
    let (code, corps) = statut(&app).await;
    assert_eq!(code, StatusCode::OK);

    assert!(
        corps.get("bio_last_run").is_some(),
        "la cle doit exister meme quand aucune passe n'a tourne"
    );
    assert!(corps["bio_last_run"]["artists"].is_null());
    assert!(corps["bio_last_run"]["albums"].is_null());
}

/// Un bilan illisible — réglage tronqué, écriture concurrente interrompue — ne
/// doit pas emporter tout le panneau : les décomptes de bibliothèque sont
/// servis par la même réponse.
#[tokio::test]
async fn un_bilan_illisible_ne_fait_pas_tomber_le_panneau() {
    let (app, state) = app_et_etat();
    SettingsRepo::with_backend(state.backend.clone())
        .set("artist_bio_enrich_result", "{ceci n'est pas du JSON")
        .expect("ecriture du reglage tronque");

    let (code, corps) = statut(&app).await;
    assert_eq!(code, StatusCode::OK);
    assert!(corps["bio_last_run"]["artists"].is_null());
    assert!(
        corps["stats"]["total_artists"].is_i64(),
        "les decomptes de bibliotheque restent servis"
    );
}

/// Témoin anti-régression : ce que la route servait déjà continue d'être
/// servi, à la même place et sous le même nom. Ce test est vert avant comme
/// après le correctif — il tombe si l'ajout de `bio_last_run` a déplacé ou
/// renommé quoi que ce soit dans `stats`.
#[tokio::test]
async fn temoin_le_contrat_existant_du_panneau_ne_bouge_pas() {
    let (app, _state) = app_et_etat();
    let (code, corps) = statut(&app).await;
    assert_eq!(code, StatusCode::OK);

    for champ in [
        "total_tracks",
        "total_artists",
        "total_albums",
        "artists_with_bio",
        "artists_with_image",
        "artists_with_mbid",
        "albums_with_cover",
        "albums_with_bio",
    ] {
        assert!(
            corps["stats"].get(champ).is_some(),
            "le panneau doit continuer de servir stats.{champ}"
        );
    }
    assert!(
        corps.get("premium").is_some() && corps.get("last_run").is_some(),
        "premium et last_run font partie du contrat existant"
    );
}
