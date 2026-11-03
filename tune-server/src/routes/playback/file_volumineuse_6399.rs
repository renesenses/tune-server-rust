//! #6399 — l'interface lente quand la file d'une zone compte des milliers de
//! titres (Nguyen Thanh Ha, fils 2289/2290 : file HQPlayer > 39 000 entrées).
//!
//! Banc : une file factice de 3 000 titres locaux (300 albums de 10 pistes),
//! une zone à sortie factice, aucune sortie réelle.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

pub(super) const LONGUEUR: usize = 3_000;

pub(super) struct Banc {
    pub app: axum::Router,
    pub state: crate::state::AppState,
    pub zone_id: i64,
}

/// Une zone dont la file compte `longueur` titres locaux, curseur au rang 10.
pub(super) async fn monter(longueur: usize) -> Banc {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("HQPlayer", Some("local"), None)
        .unwrap();
    let pistes = TrackRepo::with_backend(state.backend.clone());
    let albums = AlbumRepo::with_backend(state.backend.clone());
    let artistes = ArtistRepo::with_backend(state.backend.clone());
    let mut file = Vec::with_capacity(longueur);
    let mut album_courant = (0, 0);
    for i in 0..longueur {
        if i % 10 == 0 {
            let artiste = artistes
                .create(&Artist::new(format!("Artiste numéro {}", i / 10)))
                .unwrap();
            let mut album = Album::new(format!("Un album assez long, volume {}", i / 10));
            album.artist_id = Some(artiste);
            album_courant = (albums.create(&album).unwrap(), artiste);
        }
        let mut t = Track::new(format!("Titre de la piste {i} (version remastérisée)"));
        t.album_id = Some(album_courant.0);
        t.artist_id = Some(album_courant.1);
        t.track_number = (i % 10) as i32 + 1;
        t.disc_number = 1;
        t.duration_ms = 240_000;
        t.format = Some("flac".into());
        t.sample_rate = Some(96_000);
        t.bit_depth = Some(24);
        t.file_path = Some(format!(
            "/aucun/chemin/6399/Artiste {}/Album {}/{:02} - Titre {i}.flac",
            i / 10,
            i / 10,
            i % 10 + 1
        ));
        file.push(QueueInput::Local {
            track_id: pistes.create(&t).unwrap(),
        });
    }
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(zone_id, &file)
        .unwrap();
    state
        .playback
        .update_queue_info(zone_id, 10, longueur as i64)
        .await;
    Banc {
        app: crate::routes::router(state.clone()),
        state,
        zone_id,
    }
}

impl Banc {
    /// Statut, corps brut.
    pub async fn get(&self, uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        self.get_avec(uri, None).await
    }

    pub async fn get_avec(
        &self,
        uri: &str,
        if_none_match: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut req = Request::get(uri);
        if let Some(etag) = if_none_match {
            req = req.header("If-None-Match", etag);
        }
        let rep = self
            .app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let statut = rep.status();
        let entetes = rep.headers().clone();
        let corps = axum::body::to_bytes(rep.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (statut, entetes, corps)
    }

    /// Comme un navigateur : `Accept-Encoding: gzip`, et `If-None-Match`.
    pub async fn get_gzip(
        &self,
        uri: &str,
        if_none_match: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut req = Request::get(uri).header("Accept-Encoding", "gzip");
        if let Some(etag) = if_none_match {
            req = req.header("If-None-Match", etag);
        }
        let rep = self
            .app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let statut = rep.status();
        let entetes = rep.headers().clone();
        let corps = axum::body::to_bytes(rep.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (statut, entetes, corps)
    }

    pub async fn json(&self, uri: &str) -> Value {
        let (statut, _, corps) = self.get(uri).await;
        assert_eq!(statut, StatusCode::OK, "{uri}");
        serde_json::from_slice(&corps).unwrap()
    }
}

fn ids(v: &Value) -> Vec<i64> {
    v["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect()
}

/// Le témoin de la pagination : une fenêtre de 50 lignes rend 50 lignes, la
/// longueur ENTIÈRE et le rang global de la ligne en cours. Sans le
/// correctif, `offset`/`limit` sont ignorés et la route rend les 300.
#[tokio::test]
async fn une_fenetre_de_file_ne_rend_que_ses_lignes_6399() {
    let banc = monter(300).await;
    let v = banc
        .json(&format!(
            "/api/v1/zones/{}/queue?offset=100&limit=50",
            banc.zone_id
        ))
        .await;
    assert_eq!(
        v["tracks"].as_array().map(Vec::len),
        Some(50),
        "la fenêtre offset=100&limit=50 doit rendre 50 lignes, pas la file entière (#6399)"
    );
    assert_eq!(
        v["length"], 300,
        "la longueur reste celle de la file entière"
    );
    assert_eq!(v["offset"], 100);
    assert_eq!(v["limit"], 50);
}

/// La fenêtre est EXACTEMENT la tranche de la file entière : mêmes lignes,
/// même `gapless_next` (la dernière ligne lit sa suivante hors fenêtre),
/// même `position`.
#[tokio::test]
async fn la_fenetre_est_la_tranche_de_la_file_entiere_6399() {
    let banc = monter(300).await;
    let entiere = banc
        .json(&format!("/api/v1/zones/{}/queue", banc.zone_id))
        .await;
    let fenetre = banc
        .json(&format!(
            "/api/v1/zones/{}/queue?offset=290&limit=20",
            banc.zone_id
        ))
        .await;
    assert_eq!(entiere["tracks"].as_array().unwrap().len(), 300);
    assert_eq!(
        fenetre["tracks"].as_array().unwrap()[..],
        entiere["tracks"].as_array().unwrap()[290..],
        "la fenêtre en bout de file s'arrête à la dernière ligne"
    );
    let milieu = banc
        .json(&format!(
            "/api/v1/zones/{}/queue?offset=40&limit=10",
            banc.zone_id
        ))
        .await;
    assert_eq!(
        milieu["tracks"].as_array().unwrap()[..],
        entiere["tracks"].as_array().unwrap()[40..50]
    );
    assert_eq!(ids(&milieu), ids(&entiere)[40..50].to_vec());
    assert_eq!(milieu["position"], entiere["position"]);
    assert_eq!(milieu["length"], entiere["length"]);
}

/// Sans paramètre, la réponse est celle d'avant : la file entière, sans
/// champ de fenêtre.
#[tokio::test]
async fn sans_parametre_la_file_entiere_comme_avant_6399() {
    let banc = monter(120).await;
    let v = banc
        .json(&format!("/api/v1/zones/{}/queue", banc.zone_id))
        .await;
    assert_eq!(v["tracks"].as_array().unwrap().len(), 120);
    assert_eq!(v["length"], 120);
    assert!(v.get("offset").is_none() && v.get("limit").is_none(), "{v}");
}

/// Le témoin de la revalidation : une file inchangée répond `304` sans
/// corps à `If-None-Match` ; une file modifiée change d'ETag. Sans le
/// correctif, aucune réponse ne porte d'ETag.
#[tokio::test]
async fn une_file_inchangee_se_revalide_en_304_6399() {
    let banc = monter(200).await;
    let uri = format!("/api/v1/zones/{}/queue", banc.zone_id);
    let (statut, entetes, _) = banc.get(&uri).await;
    assert_eq!(statut, StatusCode::OK);
    let etag = entetes
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .expect("GET /queue doit porter un ETag (#6399)");

    let (statut, _, corps) = banc.get_avec(&uri, Some(&etag)).await;
    assert_eq!(statut, StatusCode::NOT_MODIFIED, "file inchangée : 304");
    assert!(corps.is_empty(), "un 304 n'a pas de corps");

    PlayQueueRepo::with_backend(banc.state.backend.clone())
        .remove_pos(banc.zone_id, 0)
        .unwrap();
    let (statut, entetes, _) = banc.get_avec(&uri, Some(&etag)).await;
    assert_eq!(statut, StatusCode::OK, "file modifiée : le corps repart");
    assert_ne!(
        entetes.get("etag").and_then(|v| v.to_str().ok()),
        Some(etag.as_str())
    );
}

#[test]
fn if_none_match_comparaison_faible_6399() {
    use super::{etag_correspond, etag_de_file};
    let e = etag_de_file(b"{}");
    assert!(e.starts_with("W/\""));
    assert!(etag_correspond(Some(&e), &e));
    assert!(etag_correspond(Some(e.trim_start_matches("W/")), &e));
    assert!(etag_correspond(Some(&format!("\"x\", {e}")), &e));
    assert!(etag_correspond(Some("*"), &e));
    assert!(!etag_correspond(Some("\"x\""), &e));
    assert!(!etag_correspond(None, &e));
}

/// Mesure, pas un témoin : `cargo test -p tune-server --release
/// mesure_file_6399 -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "mesure #6399, à lancer à la main"]
async fn mesure_file_6399() {
    let banc = monter(LONGUEUR).await;
    let _ = &banc.state;
    let routes = [
        format!("/api/v1/zones/{}/queue", banc.zone_id),
        format!("/api/v1/zones/{}/queue?offset=0&limit=100", banc.zone_id),
        format!("/api/v1/zones/{}/queue?offset=2900&limit=100", banc.zone_id),
        format!("/api/v1/zones/{}/status", banc.zone_id),
        "/api/v1/zones".to_string(),
    ];
    for uri in routes {
        let mut durees = Vec::new();
        let mut octets = 0;
        let mut statut = StatusCode::OK;
        for _ in 0..15 {
            let t0 = std::time::Instant::now();
            let (s, _, corps) = banc.get_gzip(&uri, None).await;
            durees.push(t0.elapsed());
            octets = corps.len();
            statut = s;
        }
        durees.sort();
        let (_, entetes, brut) = banc.get(&uri).await;
        println!("MESURE_6399 {uri} octets_bruts={}", brut.len());
        if let Some(etag) = entetes.get("etag").and_then(|v| v.to_str().ok()) {
            let mut d304 = Vec::new();
            let mut o304 = 0;
            for _ in 0..15 {
                let t0 = std::time::Instant::now();
                let (s, _, c) = banc.get_gzip(&uri, Some(etag)).await;
                d304.push(t0.elapsed());
                o304 = c.len();
                assert_eq!(s, StatusCode::NOT_MODIFIED);
            }
            d304.sort();
            println!(
                "MESURE_6399 {uri} revalidation=304 mediane={:?} octets={o304}",
                d304[d304.len() / 2]
            );
        }
        println!(
            "MESURE_6399 {uri} statut={} mediane={:?} min={:?} octets_gzip={octets}",
            statut.as_u16(),
            durees[durees.len() / 2],
            durees[0]
        );
    }
}
