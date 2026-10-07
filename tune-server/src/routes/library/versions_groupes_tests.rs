//! `GET /library/tracks/{id}/versions/groups` et la règle de choix, sur le
//! vrai routeur, avec deux services factices (#2264).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::AppState;
use tune_core::TuneError;
use tune_core::db::backend::ToSqlValue;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};

struct Piste {
    id: &'static str,
    titre: &'static str,
    album: &'static str,
    duree_ms: u64,
    hires: Option<(u32, u16)>,
    disponible: Option<bool>,
}

fn piste(p: &Piste) -> StreamTrack {
    StreamTrack {
        id: p.id.to_string(),
        title: p.titre.to_string(),
        artist: "Michael Jackson".to_string(),
        album: Some(p.album.to_string()),
        album_id: Some(format!("alb-{}", p.id)),
        duration_ms: p.duree_ms,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        disponible: p.disponible,
        quality: p.hires.map(|(sr, bd)| StreamQuality {
            codec: "FLAC".into(),
            sample_rate: sr,
            bit_depth: bd,
            bitrate: None,
            channels: 2,
        }),
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

/// Un service qui répond à toute recherche la même liste.
struct Doublure {
    nom: &'static str,
    pistes: Vec<Piste>,
}

#[async_trait::async_trait]
impl StreamingService for Doublure {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: self.pistes.iter().map(piste).collect(),
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        })
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
}

fn inserer_album(state: &AppState, titre: &str, artiste: i64) -> i64 {
    state
        .backend
        .execute(
            "INSERT INTO albums (title, artist_id) VALUES (?1, ?2)",
            &[&titre as &dyn ToSqlValue, &artiste],
        )
        .unwrap();
    state.backend.last_insert_rowid()
}

#[allow(clippy::too_many_arguments)]
fn inserer_piste(
    state: &AppState,
    titre: &str,
    album: i64,
    artiste: i64,
    duree: i64,
    format: &str,
    isrc: Option<&str>,
    mbid: Option<&str>,
    chemin: &str,
) -> i64 {
    state
        .backend
        .execute(
            "INSERT INTO tracks (title, album_id, artist_id, duration_ms, format, sample_rate, \
             bit_depth, isrc, musicbrainz_recording_id, file_path, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, 44100, 16, ?6, ?7, ?8, 'local')",
            &[
                &titre as &dyn ToSqlValue,
                &album,
                &artiste,
                &duree,
                &format,
                &isrc,
                &mbid,
                &chemin,
            ],
        )
        .unwrap();
    state.backend.last_insert_rowid()
}

struct Banc {
    state: AppState,
    reference: i64,
    number_ones: i64,
    live_local: i64,
    par_isrc: i64,
    par_mbid: i64,
    singles: i64,
    mj: i64,
}

/// La bibliothèque, Qobuz et Tidal.
async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute("INSERT INTO artists (name) VALUES ('Michael Jackson')", &[])
        .unwrap();
    let mj = state.backend.last_insert_rowid();
    let thriller = inserer_album(&state, "Thriller", mj);
    let number_ones = inserer_album(&state, "Number Ones", mj);
    let bad_tour = inserer_album(&state, "Bad Tour", mj);
    let singles = inserer_album(&state, "Singles", mj);

    let reference = inserer_piste(
        &state,
        "Billie Jean",
        thriller,
        mj,
        294_000,
        "flac",
        Some("USSM18200001"),
        Some("0b1c-mbid"),
        "/2264/thriller.flac",
    );
    // Même master en MP3, sans identifiant : l'heuristique (titre, artiste,
    // durée à 0,5 s).
    let number_ones = inserer_piste(
        &state,
        "Billie Jean",
        number_ones,
        mj,
        294_500,
        "mp3",
        None,
        None,
        "/2264/number-ones.mp3",
    );
    // Le live : titre suffixé, 26 s de plus.
    let live_local = inserer_piste(
        &state,
        "Billie Jean (Live)",
        bad_tour,
        mj,
        320_000,
        "flac",
        None,
        None,
        "/2264/live.flac",
    );
    // Un titre que le rapprochement par le titre ne voit PAS (« Billie-Jean »)
    // et une autre durée : seul l'ISRC, écrit avec ses tirets, le relie.
    let par_isrc = inserer_piste(
        &state,
        "Billie-Jean",
        singles,
        mj,
        280_000,
        "flac",
        Some("us-sm1-82-00001"),
        None,
        "/2264/single.flac",
    );
    // Seul le MBID, en majuscules, relie celle-ci.
    let par_mbid = inserer_piste(
        &state,
        "BJ",
        singles,
        mj,
        250_000,
        "flac",
        None,
        Some("0B1C-MBID"),
        "/2264/bj.flac",
    );

    {
        let mut registre = state.services.lock().await;
        registre.register(Box::new(Doublure {
            nom: "qobuz",
            pistes: vec![
                Piste {
                    id: "q-25",
                    titre: "Billie Jean",
                    album: "Thriller 25 Super Deluxe Edition",
                    duree_ms: 294_900,
                    hires: Some((192_000, 24)),
                    disponible: None,
                },
                Piste {
                    id: "q-remaster",
                    titre: "Billie Jean - 2008 Remaster",
                    album: "Thriller 25",
                    duree_ms: 294_000,
                    hires: Some((44_100, 16)),
                    disponible: None,
                },
                Piste {
                    id: "q-wembley",
                    titre: "Billie Jean",
                    album: "Live at Wembley July 16, 1988",
                    duree_ms: 294_300,
                    hires: Some((44_100, 16)),
                    disponible: None,
                },
            ],
        }));
        registre.register(Box::new(Doublure {
            nom: "tidal",
            pistes: vec![Piste {
                id: "t-thriller",
                titre: "Billie Jean",
                album: "Thriller",
                duree_ms: 295_000,
                hires: Some((96_000, 24)),
                disponible: Some(false),
            }],
        }));
    }

    Banc {
        state,
        reference,
        number_ones,
        live_local,
        par_isrc,
        par_mbid,
        singles,
        mj,
    }
}

async fn appeler(
    state: &AppState,
    methode: &str,
    url: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let app = crate::routes::router(state.clone());
    let mut req = Request::builder().method(methode).uri(url);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// L'identité d'un membre : `l:<track_id>` ou `<service>:<source_id>`.
fn cle(m: &Value) -> String {
    match m["track_id"].as_i64() {
        Some(t) => format!("l:{t}"),
        None => format!(
            "{}:{}",
            m["source"].as_str().unwrap_or_default(),
            m["source_id"].as_str().unwrap_or_default()
        ),
    }
}

fn groupes(corps: &Value) -> Vec<Vec<String>> {
    corps["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|g| {
            let mut v: Vec<String> = g["members"].as_array().unwrap().iter().map(cle).collect();
            v.sort();
            v
        })
        .collect()
}

fn defaut_du_premier(corps: &Value) -> String {
    let g = &corps["groups"][0];
    let i = g["default"].as_u64().expect("un défaut") as usize;
    cle(&g["members"][i])
}

#[tokio::test]
async fn la_reference_reunit_ses_exemplaires_et_laisse_live_et_remaster_a_part() {
    let b = banc().await;
    let (statut, corps) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/tracks/{}/versions/groups", b.reference),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let g = groupes(&corps);

    let mut attendu = vec![
        format!("l:{}", b.reference),
        format!("l:{}", b.number_ones),
        format!("l:{}", b.par_isrc),
        format!("l:{}", b.par_mbid),
        "qobuz:q-25".to_string(),
        "tidal:t-thriller".to_string(),
    ];
    attendu.sort();
    assert_eq!(g[0], attendu, "le groupe de la référence : {corps:#}");
    assert_eq!(corps["groups"][0]["contains_reference"], json!(true));

    // Les faux positifs, chacun SEUL dans son groupe.
    for seul in [
        format!("l:{}", b.live_local),
        "qobuz:q-remaster".to_string(),
        "qobuz:q-wembley".to_string(),
    ] {
        assert!(
            g.iter().any(|x| x == &vec![seul.clone()]),
            "{seul} devrait être seul dans son groupe : {g:?}"
        );
    }

    // Les liens publiés.
    let membres = corps["groups"][0]["members"].as_array().unwrap();
    let lien = |c: &str| {
        membres
            .iter()
            .find(|m| cle(m) == c)
            .map(|m| m["link"].clone())
            .unwrap()
    };
    assert_eq!(lien(&format!("l:{}", b.reference)), Value::Null);
    assert_eq!(lien(&format!("l:{}", b.par_isrc)), json!("isrc"));
    assert_eq!(lien(&format!("l:{}", b.par_mbid)), json!("mbid"));
    assert_eq!(lien("qobuz:q-25"), json!("title_artist_duration"));
    assert_eq!(
        corps["groups"][0]["identity"],
        json!("title_artist_duration")
    );
    assert_eq!(corps["rule"], json!("local"));
    assert_eq!(corps["rule_origin"], json!("default"));
}

#[tokio::test]
async fn la_regle_designe_la_version_jouee() {
    let b = banc().await;
    let url = |r: &str| {
        format!(
            "/api/v1/library/tracks/{}/versions/groups?rule={r}",
            b.reference
        )
    };

    let (_, local) = appeler(&b.state, "GET", &url("local"), None).await;
    // La référence (FLAC) bat le MP3 local à règle « local ».
    assert_eq!(defaut_du_premier(&local), format!("l:{}", b.reference));
    assert_eq!(local["rule_origin"], json!("query"));

    let (_, qualite) = appeler(&b.state, "GET", &url("quality"), None).await;
    assert_eq!(defaut_du_premier(&qualite), "qobuz:q-25");

    // Tidal est préféré, mais indisponible : la bibliothèque reprend la main.
    let (_, tidal) = appeler(&b.state, "GET", &url("service:tidal"), None).await;
    assert_eq!(defaut_du_premier(&tidal), format!("l:{}", b.reference));

    let (_, qobuz) = appeler(&b.state, "GET", &url("service:qobuz"), None).await;
    assert_eq!(defaut_du_premier(&qobuz), "qobuz:q-25");

    // Un seul `is_default` par groupe, et il désigne l'indice `default`.
    for g in qobuz["groups"].as_array().unwrap() {
        let marques: Vec<usize> = g["members"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, m)| m["is_default"] == json!(true))
            .map(|(i, _)| i)
            .collect();
        match g["default"].as_u64() {
            Some(d) => assert_eq!(marques, vec![d as usize]),
            None => assert!(marques.is_empty()),
        }
    }
}

#[tokio::test]
async fn la_regle_se_regle_se_relit_et_refuse_l_illisible() {
    let b = banc().await;
    let (s, v) = appeler(&b.state, "GET", "/api/v1/library/versions/rule", None).await;
    assert_eq!(
        (s, v),
        (
            StatusCode::OK,
            json!({"rule": "local", "origin": "default"})
        )
    );

    let (s, v) = appeler(
        &b.state,
        "PUT",
        "/api/v1/library/versions/rule",
        Some(json!({"rule": "quality"})),
    )
    .await;
    assert_eq!(
        (s, v),
        (
            StatusCode::OK,
            json!({"rule": "quality", "origin": "setting"})
        )
    );

    // Sans `rule`, la route de groupes applique le réglage.
    let (_, corps) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/tracks/{}/versions/groups", b.reference),
        None,
    )
    .await;
    assert_eq!(corps["rule"], json!("quality"));
    assert_eq!(corps["rule_origin"], json!("setting"));
    assert_eq!(defaut_du_premier(&corps), "qobuz:q-25");

    // Une règle illisible : 400, et le réglage n'a pas bougé.
    let (s, _) = appeler(
        &b.state,
        "PUT",
        "/api/v1/library/versions/rule",
        Some(json!({"rule": "best"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, v) = appeler(&b.state, "GET", "/api/v1/library/versions/rule", None).await;
    assert_eq!(v["rule"], json!("quality"));

    let (s, v) = appeler(
        &b.state,
        "PUT",
        "/api/v1/library/versions/rule",
        Some(json!({"rule": null})),
    )
    .await;
    assert_eq!(
        (s, v),
        (
            StatusCode::OK,
            json!({"rule": "local", "origin": "default"})
        )
    );

    let (s, _) = appeler(
        &b.state,
        "GET",
        &format!(
            "/api/v1/library/tracks/{}/versions/groups?rule=service:",
            b.reference
        ),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = appeler(
        &b.state,
        "GET",
        "/api/v1/library/tracks/999999/versions/groups",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// `sources=local` : aucun service, et les rapprochements par identifiant
/// restent ; `sources=qobuz` : plus aucune piste locale autre que la
/// référence, qui est l'ENTRÉE de la question.
#[tokio::test]
async fn le_filtre_de_sources_s_applique_aux_groupes() {
    let b = banc().await;
    let (_, local) = appeler(
        &b.state,
        "GET",
        &format!(
            "/api/v1/library/tracks/{}/versions/groups?sources=local",
            b.reference
        ),
        None,
    )
    .await;
    let tout: Vec<String> = groupes(&local).into_iter().flatten().collect();
    assert!(tout.iter().all(|c| c.starts_with("l:")), "{tout:?}");
    assert!(tout.contains(&format!("l:{}", b.par_isrc)));

    let (_, qobuz) = appeler(
        &b.state,
        "GET",
        &format!(
            "/api/v1/library/tracks/{}/versions/groups?sources=qobuz",
            b.reference
        ),
        None,
    )
    .await;
    let tout: Vec<String> = groupes(&qobuz).into_iter().flatten().collect();
    let locales: Vec<&String> = tout.iter().filter(|c| c.starts_with("l:")).collect();
    assert_eq!(locales, vec![&format!("l:{}", b.reference)], "{tout:?}");
    assert!(!tout.iter().any(|c| c.starts_with("tidal:")), "{tout:?}");
}

/// Contre-épreuve du veto : même titre, même durée, mais un AUTRE ISRC — un
/// remaster réédité. Il ne rejoint pas la référence, et il rend AMBIGUS les
/// exemplaires sans identifiant qui concordent avec les deux : ils restent
/// seuls au lieu d'être rangés au hasard.
#[tokio::test]
async fn un_autre_isrc_ferme_le_groupe_et_rend_les_muets_ambigus() {
    let b = banc().await;
    let autre_isrc = inserer_piste(
        &b.state,
        "Billie Jean",
        b.singles,
        b.mj,
        294_000,
        "flac",
        Some("USSM10800999"),
        None,
        "/2264/remaster-isrc.flac",
    );
    let (_, corps) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/tracks/{}/versions/groups", b.reference),
        None,
    )
    .await;
    let g = groupes(&corps);
    let mut attendu = vec![
        format!("l:{}", b.reference),
        format!("l:{}", b.par_isrc),
        format!("l:{}", b.par_mbid),
    ];
    attendu.sort();
    assert_eq!(g[0], attendu, "{corps:#}");
    for seul in [
        format!("l:{autre_isrc}"),
        format!("l:{}", b.number_ones),
        "qobuz:q-25".to_string(),
        "tidal:t-thriller".to_string(),
    ] {
        assert!(
            g.iter().any(|x| x == &vec![seul.clone()]),
            "{seul} seul : {g:?}"
        );
    }
}
