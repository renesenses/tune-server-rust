//! #4261 — `routes/playback.rs` : une panne ne se présente jamais comme un
//! résultat (famille #2861, point 3 de #3270).
//!
//! Six `if let Ok(...)` sans branche `Err` faisaient continuer le chemin
//! nominal comme si la lecture avait rendu « rien » : une file illisible ou
//! dont la lecture échoue, une zone illisible, une dernière piste dont la
//! lecture échoue — tout se lisait « 400 nothing to resume », c'est-à-dire un
//! choix de l'auditeur ; un service de streaming qui ne répond pas devenait un
//! titre « Unknown » indistinguable d'un titre résolu.
//!
//! # Comment l'échec est injecté
//!
//! - **Panne de base** : un `DbBackend` qui délègue tout à la vraie base
//!   SQLite mais refuse UNE lecture, reconnue à son texte SQL (le compte de la
//!   file, la zone par id, les pistes d'une playlist). Déterministe, sans
//!   horloge. Seules les routes voient ce backend : l'orchestrateur garde le
//!   vrai, ce qui isole le site du handler.
//! - **Refus de l'orchestrateur** : une zone sans sortie (« orpheline »), dont
//!   la lecture est refusée par la sentinelle `zone_no_output_device` — le
//!   409 que les autres chemins de `play` rendent déjà.
//! - **Service qui ne répond pas** : un service factice enregistré dont
//!   `get_track` échoue.
//!
//! # Contre-épreuve permanente
//!
//! [`contre_epreuve_l_injection_fait_bien_echouer_les_lectures`] prouve que
//! le backend d'injection mord et que l'application saine rend bien la réponse
//! nominale : sans elle, les témoins ci-dessous pourraient être verts parce
//! que l'échec n'a jamais été injecté (cf. PR #2798).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::db::backend::{DbBackend, DbTxHandle, SqlValue, ToSqlValue};
use tune_core::db::engine::Engine;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::NowPlaying;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

// --- injection d'échec : une lecture refusée --------------------------

/// Le fragment SQL du compte de la file (`PlayQueueRepo::count_all`).
const LECTURE_LONGUEUR_FILE: &str = "SELECT COUNT(*) FROM queue_items";
/// Le fragment SQL de la zone par id (`ZoneRepo::get`).
const LECTURE_ZONE: &str = "FROM zones WHERE id =";
/// Le fragment SQL des pistes d'une playlist (`PlaylistRepo::get_track_ids`).
const LECTURE_PISTES_PLAYLIST: &str = "SELECT track_id FROM playlist_tracks";

const REFUS: &str = "echec injecte: lecture refusee (#4261)";

/// Délègue tout à `inner`, sauf les lectures dont le SQL contient `motif`.
struct RefuseUneLecture {
    inner: Arc<dyn DbBackend>,
    motif: &'static str,
}

impl RefuseUneLecture {
    fn refuse(&self, sql: &str) -> Result<(), String> {
        if sql.contains(self.motif) {
            return Err(format!("{REFUS} : {}", self.motif));
        }
        Ok(())
    }
}

impl DbBackend for RefuseUneLecture {
    fn engine(&self) -> Engine {
        self.inner.engine()
    }

    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        self.inner.execute(sql, params)
    }

    fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        self.refuse(sql)?;
        self.inner.query_one(sql, params)
    }

    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        self.refuse(sql)?;
        self.inner.query_many(sql, params)
    }

    fn query_one_strong(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        self.refuse(sql)?;
        self.inner.query_one_strong(sql, params)
    }

    fn query_many_strong(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        self.refuse(sql)?;
        self.inner.query_many_strong(sql, params)
    }

    fn execute_batch(&self, sql: &str) -> Result<(), String> {
        self.inner.execute_batch(sql)
    }

    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        self.inner.write_tx(f)
    }
}

// --- un service de streaming qui ne répond pas ------------------------

const SERVICE_MUET: &str = "banc-4261-muet";
const SERVICE_QUI_REPOND: &str = "banc-4261-repond";
const MOTIF_SERVICE: &str = "le service ne repond pas (#4261)";

/// `get_track` échoue quand `repond` est faux ; rend un titre quand il est vrai.
struct ServiceDeBanc {
    nom: &'static str,
    repond: bool,
}

#[async_trait::async_trait]
impl StreamingService for ServiceDeBanc {
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

    async fn authenticate(&mut self, _credentials: &Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            username: Some("banc".into()),
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }

    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        if !self.repond {
            return Err(TuneError::Streaming(MOTIF_SERVICE.into()));
        }
        Ok(StreamTrack {
            id: track_id.into(),
            title: format!("Titre resolu {track_id}"),
            artist: "Artiste resolu".into(),
            album: None,
            album_id: None,
            duration_ms: 180_000,
            cover_path: None,
            track_number: Some(3),
            disc_number: Some(1),
            explicit: false,
            disponible: None,
            quality: None,
            isrc: None,
            composer: None,
            artist_id: None,
        })
    }
    async fn get_track_url(
        &self,
        _track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_album_tracks(&self, _album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

// --- outillage ---------------------------------------------------------

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appli(state: &AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

/// Même base, mais la lecture dont le SQL contient `motif` échoue — pour les
/// ROUTES seulement : l'orchestrateur garde le vrai backend.
fn appli_en_panne(state: &AppState, motif: &'static str) -> axum::Router {
    let mut casse = state.clone();
    casse.backend = Arc::new(RefuseUneLecture {
        inner: state.backend.clone(),
        motif,
    });
    tune_server::routes::router(casse)
}

/// Une zone EN BASE sans sortie : l'orchestrateur refuse d'y lire quoi que ce
/// soit (`zone_no_output_device`, 409). C'est le refus qu'on veut voir sortir.
fn zone_orpheline(state: &AppState, nom: &str) -> i64 {
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("local"), None)
        .expect("creation de zone")
}

fn piste(state: &AppState, titre: &str) -> i64 {
    let mut t = Track::new(titre.into());
    t.file_path = Some(format!("/musique/{titre}.flac"));
    TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insertion de piste")
}

fn mettre_en_file(state: &AppState, zone_id: i64, track_id: i64) {
    PlayQueueRepo::with_backend(state.backend.clone())
        .append_tracks(zone_id, &[track_id])
        .expect("mise en file");
}

fn poser_derniere_piste(state: &AppState, zone_id: i64, track_id: i64) {
    ZoneRepo::with_backend(state.backend.clone())
        .save_playback_position(zone_id, 12_000, Some(track_id), None, None)
        .expect("derniere piste");
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&bytes).to_string();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, json, texte)
}

/// `POST /zones/{id}/play` SANS corps : le geste « Lecture » après Stop.
async fn lecture_sans_corps(app: &axum::Router, zone_id: i64) -> (StatusCode, Value, String) {
    reponse(
        app,
        Request::post(format!("/api/v1/zones/{zone_id}/play"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn poste_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value, String) {
    reponse(
        app,
        Request::post(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

/// `POST /zones/{id}/play` avec un corps JSON qui ne désigne rien (`{}`).
async fn lecture_corps_vide(app: &axum::Router, zone_id: i64) -> (StatusCode, Value, String) {
    poste_json(app, &format!("/api/v1/zones/{zone_id}/play"), json!({})).await
}

fn verifier_panne_de_base(statut: StatusCode, corps: &Value, texte: &str, site: &str) {
    assert_eq!(
        statut,
        StatusCode::INTERNAL_SERVER_ERROR,
        "une panne de base doit rendre 500, pas « {texte} »"
    );
    assert_eq!(
        corps["error"],
        json!("playback_lecture_base_echouee"),
        "corps : {texte}"
    );
    assert_eq!(
        corps["site"],
        json!(site),
        "le site doit être nommé : {texte}"
    );
    assert!(
        corps["message"].as_str().unwrap_or("").contains(REFUS),
        "le motif de la base doit être rendu : {texte}"
    );
}

fn verifier_refus_zone_sans_sortie(statut: StatusCode, corps: &Value, texte: &str) {
    assert_eq!(
        statut,
        StatusCode::CONFLICT,
        "le refus de l'orchestrateur doit sortir tel quel (409), pas « {texte} »"
    );
    assert_eq!(
        corps["error"],
        json!("zone_no_output_device"),
        "corps : {texte}"
    );
}

// --- contre-épreuve permanente ----------------------------------------

/// L'injection mord, et l'application SAINE rend bien la réponse nominale
/// que les témoins refusent. Sans ce test, un témoin vert pourrait l'être
/// parce que rien n'a jamais échoué.
#[tokio::test]
async fn contre_epreuve_l_injection_fait_bien_echouer_les_lectures() {
    let state = etat();
    let zid = zone_orpheline(&state, "Contre-épreuve");
    let tid = piste(&state, "Contre");
    mettre_en_file(&state, zid, tid);
    let pl = PlaylistRepo::with_backend(state.backend.clone());
    let plid = pl.create("Contre", None, 1).unwrap();
    pl.add_tracks(plid, &[tid], None).unwrap();

    // Sain : les trois lectures passent.
    assert_eq!(
        PlayQueueRepo::with_backend(state.backend.clone()).count_all(zid),
        Ok(1)
    );
    assert!(
        ZoneRepo::with_backend(state.backend.clone())
            .get(zid)
            .unwrap()
            .is_some()
    );
    assert_eq!(pl.get_track_ids(plid), Ok(vec![tid]));

    // En panne : chacune échoue, et SEULEMENT elle.
    for (motif, file, zone, playlist) in [
        (LECTURE_LONGUEUR_FILE, true, false, false),
        (LECTURE_ZONE, false, true, false),
        (LECTURE_PISTES_PLAYLIST, false, false, true),
    ] {
        let casse: Arc<dyn DbBackend> = Arc::new(RefuseUneLecture {
            inner: state.backend.clone(),
            motif,
        });
        assert_eq!(
            PlayQueueRepo::with_backend(casse.clone())
                .count_all(zid)
                .is_err(),
            file,
            "motif {motif} : compte de la file"
        );
        assert_eq!(
            ZoneRepo::with_backend(casse.clone()).get(zid).is_err(),
            zone,
            "motif {motif} : zone"
        );
        assert_eq!(
            PlaylistRepo::with_backend(casse)
                .get_track_ids(plid)
                .is_err(),
            playlist,
            "motif {motif} : pistes de playlist"
        );
    }

    // L'application saine, sur une zone où il n'y a VRAIMENT rien, rend le
    // 400 nominal — c'est cette réponse-là qu'une panne ne doit pas imiter.
    let vide = zone_orpheline(&state, "Vraiment vide");
    let (statut, _, texte) = lecture_sans_corps(&appli(&state), vide).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{texte}");
    assert!(texte.contains("nothing to resume"), "{texte}");
    let (statut, _, texte) = lecture_corps_vide(&appli(&state), vide).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{texte}");
    assert!(texte.contains("no track source specified"), "{texte}");
}

// --- corps absent : file, zone, dernière piste -------------------------

/// Site `play_sans_source_longueur_file` — l.1448 au tag v0.9.151.
#[tokio::test]
async fn une_file_illisible_ne_se_lit_pas_comme_nothing_to_resume() {
    let state = etat();
    let zid = zone_orpheline(&state, "File illisible");
    let (statut, corps, texte) =
        lecture_sans_corps(&appli_en_panne(&state, LECTURE_LONGUEUR_FILE), zid).await;
    verifier_panne_de_base(statut, &corps, &texte, "play_sans_source_longueur_file");
}

/// Site l.1453 — la file existe, sa lecture est refusée par l'orchestrateur :
/// le refus sort, au lieu d'un 400 qui prétend qu'il n'y avait rien.
#[tokio::test]
async fn une_file_dont_la_lecture_echoue_rend_le_motif_de_l_orchestrateur() {
    let state = etat();
    let zid = zone_orpheline(&state, "File refusée");
    let tid = piste(&state, "En file");
    mettre_en_file(&state, zid, tid);
    let (statut, corps, texte) = lecture_sans_corps(&appli(&state), zid).await;
    verifier_refus_zone_sans_sortie(statut, &corps, &texte);
}

/// Site `play_sans_source_derniere_piste` — l.1463 au tag v0.9.151.
#[tokio::test]
async fn une_zone_illisible_ne_se_lit_pas_comme_nothing_to_resume() {
    let state = etat();
    let zid = zone_orpheline(&state, "Zone illisible");
    let (statut, corps, texte) =
        lecture_sans_corps(&appli_en_panne(&state, LECTURE_ZONE), zid).await;
    verifier_panne_de_base(statut, &corps, &texte, "play_sans_source_derniere_piste");
}

/// Site l.1485 — une dernière piste existe, sa lecture est refusée : le refus
/// sort, au lieu d'un 400 « nothing to resume » alors qu'il y avait à reprendre.
#[tokio::test]
async fn une_derniere_piste_dont_la_lecture_echoue_rend_le_motif() {
    let state = etat();
    let zid = zone_orpheline(&state, "Dernière piste");
    let tid = piste(&state, "Derniere");
    poser_derniere_piste(&state, zid, tid);
    let (statut, corps, texte) = lecture_sans_corps(&appli(&state), zid).await;
    verifier_refus_zone_sans_sortie(statut, &corps, &texte);
}

// --- corps `{}` : le jumeau ---------------------------------------------

/// Site `play_corps_sans_source_longueur_file` — l.1949 au tag v0.9.151.
#[tokio::test]
async fn un_corps_sans_source_sur_une_file_illisible_rend_500() {
    let state = etat();
    let zid = zone_orpheline(&state, "Corps vide, file illisible");
    let (statut, corps, texte) =
        lecture_corps_vide(&appli_en_panne(&state, LECTURE_LONGUEUR_FILE), zid).await;
    verifier_panne_de_base(
        statut,
        &corps,
        &texte,
        "play_corps_sans_source_longueur_file",
    );
}

/// Site l.1954 au tag v0.9.151.
#[tokio::test]
async fn un_corps_sans_source_dont_la_file_echoue_rend_le_motif() {
    let state = etat();
    let zid = zone_orpheline(&state, "Corps vide, file refusée");
    let tid = piste(&state, "En file aussi");
    mettre_en_file(&state, zid, tid);
    let (statut, corps, texte) = lecture_corps_vide(&appli(&state), zid).await;
    verifier_refus_zone_sans_sortie(statut, &corps, &texte);
}

// --- reprise ratée puis repli raté (l.1432) ------------------------------

/// Site l.1432 — la reprise de `now_playing` échoue, le repli sur la file
/// échoue aussi. La réponse était déjà une erreur (le motif de la reprise) :
/// le correctif n'y change que le journal, ce témoin fixe le contrat — un
/// double échec ne devient jamais un 200 — et ne rougit pas si l'on retire le
/// `warn!`.
#[tokio::test]
async fn un_repli_qui_echoue_apres_une_reprise_ratee_reste_une_erreur() {
    let state = etat();
    let zid = zone_orpheline(&state, "Reprise ratée");
    let tid = piste(&state, "Reprise");
    mettre_en_file(&state, zid, tid);
    state
        .playback
        .play(
            zid,
            NowPlaying {
                track_id: Some(tid),
                title: "Reprise".into(),
                duration_ms: 240_000,
                source: "local".into(),
                ..Default::default()
            },
        )
        .await;
    // En pause : une zone qui JOUE la même piste depuis moins de quelques
    // secondes verrait son « Lecture » coalescé (dédoublonnage du re-tap,
    // #1271) et rendrait 200 sans passer par l'orchestrateur.
    state.playback.pause(zid).await;
    let (statut, _, texte) = lecture_sans_corps(&appli(&state), zid).await;
    assert_eq!(statut, StatusCode::INTERNAL_SERVER_ERROR, "{texte}");
    assert!(
        texte.contains("zone_no_output_device"),
        "le motif de la reprise doit être rendu : {texte}"
    );
}

// --- jumelle : les pistes d'une playlist (l.1896) -------------------------

/// `repo.get_track_ids(...).unwrap_or_default()` : une playlist ILLISIBLE
/// devenait une playlist vide, donc un 400 « no tracks to play ».
#[tokio::test]
async fn une_playlist_illisible_ne_se_lit_pas_comme_no_tracks_to_play() {
    let state = etat();
    let zid = zone_orpheline(&state, "Playlist illisible");
    let tid = piste(&state, "Dans la playlist");
    let pl = PlaylistRepo::with_backend(state.backend.clone());
    let plid = pl.create("Illisible", None, 1).unwrap();
    pl.add_tracks(plid, &[tid], None).unwrap();

    let (statut, corps, texte) = poste_json(
        &appli_en_panne(&state, LECTURE_PISTES_PLAYLIST),
        &format!("/api/v1/zones/{zid}/play"),
        json!({ "playlist_id": plid }),
    )
    .await;
    verifier_panne_de_base(statut, &corps, &texte, "play_pistes_de_playlist");
}

// --- service de streaming qui ne répond pas (l.2763) -----------------------

async fn enregistrer(state: &AppState, svc: ServiceDeBanc) {
    state.services.lock().await.register(Box::new(svc));
}

async fn enfiler_sans_titre(
    app: &axum::Router,
    zid: i64,
    source: &str,
) -> (StatusCode, Value, String) {
    poste_json(
        app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        json!({ "source": source, "source_id": "p-4261" }),
    )
    .await
}

/// Site l.2763 — `svc.get_track` échoue : la piste est enfilée sous
/// « Unknown » (pas de refus), mais la réponse NOMME la piste non résolue et
/// le motif du service. Un « Unknown » n'est plus indistinguable d'un titre.
#[tokio::test]
async fn un_service_qui_ne_repond_pas_est_nomme_dans_la_reponse_d_enfilage() {
    let state = etat();
    enregistrer(
        &state,
        ServiceDeBanc {
            nom: SERVICE_MUET,
            repond: false,
        },
    )
    .await;
    let zid = zone_orpheline(&state, "Service muet");

    let (statut, corps, texte) = enfiler_sans_titre(&appli(&state), zid, SERVICE_MUET).await;
    assert_eq!(statut, StatusCode::CREATED, "{texte}");
    assert_eq!(corps["added"], json!(1), "{texte}");
    assert_eq!(corps["items"][0]["title"], json!("Unknown"), "{texte}");
    let non_resolues = corps["unresolved"]
        .as_array()
        .unwrap_or_else(|| panic!("`unresolved` doit être un tableau : {texte}"));
    assert_eq!(non_resolues.len(), 1, "{texte}");
    assert_eq!(non_resolues[0]["source"], json!(SERVICE_MUET), "{texte}");
    assert_eq!(non_resolues[0]["source_id"], json!("p-4261"), "{texte}");
    assert!(
        non_resolues[0]["error"]
            .as_str()
            .unwrap_or("")
            .contains(MOTIF_SERVICE),
        "le motif du service doit être rendu : {texte}"
    );
}

/// Le témoin nominal : quand le service répond, rien n'est « non résolu » et
/// le titre est le sien. C'est cette réponse-là que le test précédent refuse.
#[tokio::test]
async fn un_service_qui_repond_ne_laisse_rien_de_non_resolu() {
    let state = etat();
    enregistrer(
        &state,
        ServiceDeBanc {
            nom: SERVICE_QUI_REPOND,
            repond: true,
        },
    )
    .await;
    let zid = zone_orpheline(&state, "Service qui répond");

    let (statut, corps, texte) = enfiler_sans_titre(&appli(&state), zid, SERVICE_QUI_REPOND).await;
    assert_eq!(statut, StatusCode::CREATED, "{texte}");
    assert_eq!(
        corps["items"][0]["title"],
        json!("Titre resolu p-4261"),
        "{texte}"
    );
    assert_eq!(corps["unresolved"], json!([]), "{texte}");
}

/// Un service ABSENT du registre est nommé lui aussi — avec un motif qui le
/// distingue d'un service qui a échoué.
#[tokio::test]
async fn un_service_absent_est_nomme_dans_la_reponse_d_enfilage() {
    let state = etat();
    let zid = zone_orpheline(&state, "Service absent");
    let (statut, corps, texte) = enfiler_sans_titre(&appli(&state), zid, "banc-4261-absent").await;
    assert_eq!(statut, StatusCode::CREATED, "{texte}");
    assert_eq!(corps["items"][0]["title"], json!("Unknown"), "{texte}");
    assert_eq!(
        corps["unresolved"].as_array().map(Vec::len),
        Some(1),
        "{texte}"
    );
    assert!(
        corps["unresolved"][0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("service non enregistré"),
        "{texte}"
    );
}

// #4298 : une panne de lecture n'est pas une file vide.
#[tokio::test]
async fn relance_locale_file_illisible_4298_ne_l_efface_pas() {
    let state = etat();
    let zone = zone_orpheline(&state, "Relance");
    let a = piste(&state, "A");
    let b = piste(&state, "B");
    let repo = PlayQueueRepo::with_backend(state.backend.clone());
    repo.set_queue(zone, &[a, b]).unwrap();
    let before: Vec<_> = repo
        .get_ordered(zone)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    let app = appli_en_panne(&state, "SELECT q.id, q.zone_id, q.track_id");
    let response = app
        .oneshot(
            Request::post(format!("/api/v1/zones/{zone}/play"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"track_id": b}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "la panne doit être nommée"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains(REFUS));
    assert_eq!(
        repo.get_ordered(zone)
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        before,
        "#4298 : une lecture en erreur a effacé la file"
    );
}
