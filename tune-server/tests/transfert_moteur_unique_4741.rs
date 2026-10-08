//! #4741 — un seul moteur de transfert de playlists : celui du greffon
//! « Playlists converter ».
//!
//! Joué avec le VRAI `main.wasm` (le fixture que `release.yml` et `docker.yml`
//! livrent), chargé comme au démarrage du serveur, et deux services de
//! streaming SIMULÉS — aucun vrai service n'est touché.
//!
//! Ce que ce fichier prouve, par les routes que le web, l'appli iPad et
//! l'appli Flutter appellent :
//!
//! 1. `POST /playlist-manager/transfer` passe par le greffon : le transfert
//!    devient un lot du greffon, la règle d'appariement est la sienne (le
//!    remaster trop long est écarté, la bonne édition est prise plus bas dans
//!    le classement), et le nom choisi est respecté ;
//! 2. `dry_run` n'écrit rien ;
//! 3. la bibliothèque locale est une cible, sous le profil de l'appelant ;
//! 4. l'historique `GET /playlist-manager/history` montre les lots du
//!    greffon, quel que soit le chemin emprunté ;
//! 5. les deux autres moteurs ont disparu ;
//! 6. sans le greffon, la route le dit (503 `greffon_requis`) au lieu de
//!    transférer par un autre chemin ;
//! 7. le transfert entre services est Premium (Bertrand, 07/10/2026) : un
//!    compte gratuit reçoit `402 premium_required` avec sa raison, et rien
//!    n'est écrit ; la copie « bibliothèque → bibliothèque » reste gratuite.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

const ID: &str = "playlists-converter";

// ---------------------------------------------------------------------------
// Services simulés
// ---------------------------------------------------------------------------

type Journal = Arc<Mutex<Vec<String>>>;

/// Un service simulé : des playlists à lire, une recherche qui rend toujours
/// le même classement, et un journal de TOUTES ses écritures.
struct ServiceSimule {
    nom: &'static str,
    playlists: HashMap<String, (String, Vec<StreamTrack>)>,
    recherche: Vec<StreamTrack>,
    ecritures: Journal,
}

#[async_trait::async_trait]
impl StreamingService for ServiceSimule {
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
            username: Some("simule".into()),
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: self.recherche.clone(),
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_track_url(
        &self,
        _id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_playlist_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.playlists
            .get(id)
            .map(|(_, p)| p.clone())
            .ok_or_else(|| TuneError::NotFound(id.into()))
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(self
            .playlists
            .iter()
            .map(|(id, (nom, pistes))| StreamPlaylist {
                id: id.clone(),
                name: nom.clone(),
                description: None,
                cover_path: None,
                covers: Vec::new(),
                track_count: pistes.len() as _,
                owner: None,
            })
            .collect())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
    async fn create_playlist(
        &self,
        name: &str,
        _description: Option<&str>,
    ) -> Result<String, TuneError> {
        self.ecritures
            .lock()
            .unwrap()
            .push(format!("creation:{name}"));
        Ok("pl-creee".to_string())
    }
    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        self.ecritures
            .lock()
            .unwrap()
            .push(format!("ajout:{playlist_id}:{}", track_ids.join(",")));
        Ok(track_ids.len())
    }
    fn supports_write(&self) -> bool {
        true
    }
}

fn piste(id: &str, titre: &str, artiste: &str, duree_ms: u64) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: None,
        album_id: None,
        duration_ms: duree_ms,
        cover_path: None,
        track_number: Some(1),
        disc_number: Some(1),
        explicit: false,
        disponible: None,
        isrc: None,
        composer: None,
        artist_id: None,
        quality: None,
    }
}

// ---------------------------------------------------------------------------
// Socle
// ---------------------------------------------------------------------------

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugins/playlists-converter")
}

/// Pose les variables d'environnement de l'essai et les rend à la sortie.
struct Environnement {
    anciennes: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl Environnement {
    fn poser(valeurs: &[(&'static str, String)]) -> Self {
        let anciennes = valeurs
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in valeurs {
            unsafe { std::env::set_var(k, v) };
        }
        Self { anciennes }
    }
}
impl Drop for Environnement {
    fn drop(&mut self) {
        for (k, v) in &self.anciennes {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

struct Banc {
    _dossier: tempfile::TempDir,
    _env: Environnement,
    state: AppState,
    app: axum::Router,
    ecritures_source: Journal,
    ecritures_cible: Journal,
}

/// Un serveur : la source « source » porte la playlist `pl-1` (trois titres),
/// la cible « cible » rend un classement où le verdict de tête est un
/// remaster trop long et la bonne édition vient en second. La bibliothèque
/// porte « La Bohème » (piste 1). Le greffon est chargé si `avec_greffon`.
async fn banc(avec_greffon: bool) -> Banc {
    banc_licence(avec_greffon, true).await
}

/// Comme [`banc`], en choisissant la licence : le transfert entre services est
/// Premium (Bertrand, 07/10/2026), la copie dans la bibliothèque ne l'est pas.
async fn banc_licence(avec_greffon: bool, premium: bool) -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let greffons = dossier.path().join("plugins");
    std::fs::create_dir_all(&greffons).unwrap();
    if avec_greffon {
        let cible = greffons.join(ID);
        std::fs::create_dir_all(&cible).unwrap();
        for fichier in ["main.wasm", "manifest.json"] {
            std::fs::copy(fixture().join(fichier), cible.join(fichier)).unwrap();
        }
    }
    let env = Environnement::poser(&[
        ("TUNE_PLUGINS_DIR", greffons.display().to_string()),
        ("TUNE_WASM_PROBE_SKIP", "1".into()),
    ]);

    let db = dossier.path().join("tune.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Charles Aznavour')",
            &[],
        )
        .unwrap();
    state
        .backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) \
             VALUES (?, ?, 1, 1, ?)",
            &[&1i64 as &dyn ToSqlValue, &"La Bohème", &210_000i64],
        )
        .unwrap();
    // Le profil visé par `X-Profile-Id` doit exister.
    let id = tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone())
        .create("voisin", Some("Le voisin"), None)
        .unwrap();
    assert_eq!(id, 2);

    let ecritures_source: Journal = Arc::default();
    let ecritures_cible: Journal = Arc::default();
    {
        let mut services = state.services.lock().await;
        services.register(Box::new(ServiceSimule {
            nom: "source",
            playlists: HashMap::from([(
                "pl-1".to_string(),
                (
                    "Mes classiques".to_string(),
                    vec![
                        piste("s-1", "La Bohème", "Charles Aznavour", 210_000),
                        piste("s-2", "Introuvable ailleurs", "Personne", 200_000),
                    ],
                ),
            )]),
            recherche: Vec::new(),
            ecritures: ecritures_source.clone(),
        }));
        services.register(Box::new(ServiceSimule {
            nom: "cible",
            playlists: HashMap::new(),
            recherche: vec![
                // Le verdict de tête : titre identique une fois « (Remastered) »
                // retiré, mais 9 s de trop.
                piste(
                    "c-remaster",
                    "La Bohème (Remastered 2014)",
                    "Charles Aznavour",
                    219_000,
                ),
                // La bonne édition, plus bas dans le classement.
                piste("c-bonne", "La Bohème", "Charles Aznavour", 210_500),
            ],
            ecritures: ecritures_cible.clone(),
        }));
    }

    tune_server::plugins_host::load_wasm_plugins(&state).await;
    state.license.set_account_premium(premium, None).await;
    let app = tune_server::routes::router(state.clone());
    Banc {
        _dossier: dossier,
        _env: env,
        state,
        app,
        ecritures_source,
        ecritures_cible,
    }
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    appel_profil(app, methode, chemin, corps, None).await
}

async fn appel_profil(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
    profil: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(p) = profil {
        req = req.header("X-Profile-Id", p);
    }
    let body = if corps.is_null() {
        Body::empty()
    } else {
        req = req.header("content-type", "application/json");
        Body::from(corps.to_string())
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn journal(j: &Journal) -> Vec<String> {
    j.lock().unwrap().clone()
}

fn transfert(cible: &str) -> Value {
    json!({
        "source_service": "source",
        "source_playlist_id": "pl-1",
        "target_service": cible,
        // Les clients d'avant envoient encore ces champs : acceptés, ignorés.
        "match_threshold": 0.6,
        "include_approximate": true,
        "create_on_target": true,
    })
}

// ---------------------------------------------------------------------------
// 1. Le transfert passe par le greffon
// ---------------------------------------------------------------------------

/// 🔴 Le témoin du moteur unique. Sur l'ancien moteur : la playlist créée
/// s'appelait « Mes classiques (transferred) » (le nom choisi, envoyé sous
/// `target_name`, était ignoré), le remaster de tête était versé, et aucun lot
/// du greffon n'existait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_transfert_historique_passe_par_le_greffon_4741() {
    let _verrou = crate::lock_environment();
    let b = banc(true).await;

    let mut corps = transfert("cible");
    corps["target_name"] = json!("Mon nom");
    let (st, rendu) = appel(&b.app, "POST", "/api/v1/playlist-manager/transfer", corps).await;
    assert_eq!(st, StatusCode::OK, "{rendu}");

    // La preuve chez le service D'ABORD : une création, au nom choisi, et la
    // BONNE édition versée — pas le remaster que l'ancien moteur prenait.
    assert_eq!(
        journal(&b.ecritures_cible),
        vec![
            "creation:Mon nom".to_string(),
            "ajout:pl-creee:c-bonne".to_string(),
        ],
        "écritures chez la cible : {rendu}"
    );
    assert!(
        journal(&b.ecritures_source).is_empty(),
        "rien n'est écrit à la source"
    );

    // Puis la réponse : la forme d'avant, et le lot du greffon.
    assert_eq!(rendu["lot_id"], "lot-1", "{rendu}");
    assert_eq!(rendu["source_playlist_name"], "Mes classiques");
    assert_eq!(rendu["target_playlist_name"], "Mon nom");
    assert_eq!(rendu["remote_playlist_id"], "pl-creee");
    assert_eq!(rendu["total_tracks"], 2);
    assert_eq!(rendu["matched"], 1);
    assert_eq!(rendu["not_found"], 1);
    assert_eq!(rendu["status"], "completed");
    assert!(
        rendu["snapshot_avant"].is_string(),
        "copie datée avant écriture : {rendu}"
    );
    // Le rapport des titres non trouvés, raison comprise.
    let pistes = rendu["tracks"].as_array().unwrap();
    let manquant = pistes
        .iter()
        .find(|p| p["status"] == "not_found")
        .unwrap_or_else(|| panic!("aucun titre introuvable rapporté : {rendu}"));
    assert_eq!(manquant["title"], "Introuvable ailleurs");
    assert!(manquant["raison"]["code"].is_string(), "{manquant}");

    // L'historique montre le lot, sans dépendre du chemin emprunté.
    let (st, historique) = appel(
        &b.app,
        "GET",
        "/api/v1/playlist-manager/history",
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(historique[0]["lot_id"], "lot-1", "{historique}");
    assert_eq!(
        historique[0]["id"], 1,
        "un entier, que l'appli iPad décode : {historique}"
    );
    assert_eq!(historique[0]["operation"], "transfer");
    assert_eq!(historique[0]["matched"], 1);
    let (st, detail) = appel(
        &b.app,
        "GET",
        "/api/v1/playlist-manager/history/lot-1",
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{detail}");
    assert_eq!(
        detail["details"].as_array().map(Vec::len),
        Some(2),
        "{detail}"
    );

    // Et le même lot, vu par l'onglet Transferts du greffon.
    let (st, lots) = appel(
        &b.app,
        "GET",
        &format!("/api/v1/plugins/{ID}/lots"),
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{lots}");
    assert_eq!(lots["lots"][0]["lot_id"], "lot-1", "{lots}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_dry_run_n_ecrit_rien_4741() {
    let _verrou = crate::lock_environment();
    let b = banc(true).await;
    let mut corps = transfert("cible");
    corps["dry_run"] = json!(true);
    let (st, rendu) = appel(&b.app, "POST", "/api/v1/playlist-manager/transfer", corps).await;
    assert_eq!(st, StatusCode::OK, "{rendu}");
    assert!(
        journal(&b.ecritures_cible).is_empty(),
        "un dry_run a écrit : {rendu}"
    );
    assert_eq!(rendu["status"], "dry_run");
    assert_eq!(rendu["etat"], "apercu");
    assert_eq!(rendu["matched"], 1);
}

// ---------------------------------------------------------------------------
// 2. La bibliothèque locale comme cible, sous le profil de l'appelant
// ---------------------------------------------------------------------------

/// 🔴 L'import d'une playlist de service dans la bibliothèque (le bouton
/// « Importer » du web). Le greffon le refusait ; la playlist est maintenant
/// créée, avec la piste locale appariée, chez le profil de l'APPELANT
/// (`X-Profile-Id`), pas chez le profil actif global.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l_import_dans_la_bibliotheque_passe_par_le_greffon_4741() {
    let _verrou = crate::lock_environment();
    let b = banc(true).await;
    let mut corps = transfert("local");
    corps["target_name"] = json!("Importée");
    let (st, rendu) = appel_profil(
        &b.app,
        "POST",
        "/api/v1/playlist-manager/transfer",
        corps,
        Some("2"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rendu}");

    let repo = PlaylistRepo::with_backend(b.state.backend.clone());
    let chez_2 = repo.list(2, 100, 0).unwrap();
    let importee = chez_2
        .iter()
        .find(|p| p.name == "Importée")
        .unwrap_or_else(|| {
            panic!(
                "la playlist importée n'est pas chez le profil 2 : {:?} / profil 1 : {:?}",
                chez_2.iter().map(|p| &p.name).collect::<Vec<_>>(),
                repo.list(1, 100, 0)
                    .unwrap()
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>()
            )
        });
    let id = importee.id.unwrap();
    assert_eq!(repo.get_track_ids(id).unwrap(), vec![1], "{rendu}");
    assert_eq!(rendu["local_playlist_id"], id, "{rendu}");
    assert_eq!(rendu["matched"], 1);
    assert_eq!(rendu["not_found"], 1);
    assert!(journal(&b.ecritures_cible).is_empty());
}

// ---------------------------------------------------------------------------
// 3. Les deux autres moteurs ont disparu
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_deux_autres_moteurs_ont_disparu_4741() {
    let _verrou = crate::lock_environment();
    let b = banc(true).await;
    for chemin in [
        "/api/v1/playlist-transfer/transfer",
        "/api/v1/playlist-transfer/preview",
        "/api/v1/playlist-manager/batch-transfer",
    ] {
        let (st, corps) = appel(
            &b.app,
            "POST",
            chemin,
            json!({
                "source_service": "source",
                "target_service": "cible",
                "source_playlist_id": "pl-1",
                "playlist_ids": ["pl-1"],
            }),
        )
        .await;
        assert!(
            st == StatusCode::NOT_FOUND || st == StatusCode::METHOD_NOT_ALLOWED,
            "{chemin} répond encore {st} : {corps}"
        );
    }
    assert!(journal(&b.ecritures_cible).is_empty());
}

// ---------------------------------------------------------------------------
// 4. Sans le greffon
// ---------------------------------------------------------------------------

/// Sans greffon chargé, aucun transfert par un autre chemin : un refus qui
/// nomme ce qui manque.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sans_le_greffon_la_route_le_dit_4741() {
    let _verrou = crate::lock_environment();
    let b = banc(false).await;
    let (st, rendu) = appel(
        &b.app,
        "POST",
        "/api/v1/playlist-manager/transfer",
        transfert("cible"),
    )
    .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{rendu}");
    assert_eq!(rendu["error"], "greffon_requis", "{rendu}");
    assert!(journal(&b.ecritures_cible).is_empty());
}

// ---------------------------------------------------------------------------
// 5. Premium (Bertrand, 07/10/2026)
// ---------------------------------------------------------------------------

/// 🔴 Un compte gratuit : refus clair, et RIEN d'écrit — ni chez le service,
/// ni dans la bibliothèque, ni dans les lots du greffon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_compte_gratuit_est_refuse_avec_sa_raison_4741() {
    let _verrou = crate::lock_environment();
    let b = banc_licence(true, false).await;
    for cible in ["cible", "local"] {
        let (st, rendu) = appel(
            &b.app,
            "POST",
            "/api/v1/playlist-manager/transfer",
            transfert(cible),
        )
        .await;
        assert_eq!(st, StatusCode::PAYMENT_REQUIRED, "vers {cible} : {rendu}");
        assert_eq!(rendu["error"], "premium_required", "{rendu}");
        assert_eq!(rendu["code"], "playlist_transfer", "{rendu}");
        assert!(
            rendu["raison"]
                .as_str()
                .is_some_and(|r| r.contains("gratuit")),
            "{rendu}"
        );
        assert!(rendu["upgrade_url"].is_string(), "{rendu}");
    }
    assert!(
        journal(&b.ecritures_cible).is_empty(),
        "un refus a écrit chez le service"
    );
    let repo = PlaylistRepo::with_backend(b.state.backend.clone());
    assert!(
        repo.list(1, 100, 0).unwrap().is_empty(),
        "un refus a créé une playlist locale"
    );
    let (_, historique) = appel(
        &b.app,
        "GET",
        "/api/v1/playlist-manager/history",
        Value::Null,
    )
    .await;
    assert_eq!(
        historique,
        json!([]),
        "un refus a laissé un lot : {historique}"
    );
}

/// Témoin : le même compte, passé Premium, transfère.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_compte_premium_transfere_4741() {
    let _verrou = crate::lock_environment();
    let b = banc_licence(true, false).await;
    b.state.license.set_account_premium(true, None).await;
    let (st, rendu) = appel(
        &b.app,
        "POST",
        "/api/v1/playlist-manager/transfer",
        transfert("cible"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rendu}");
    assert_eq!(rendu["matched"], 1, "{rendu}");
    assert_eq!(journal(&b.ecritures_cible).len(), 2);
}

/// « Dupliquer » (bibliothèque → bibliothèque) reste gratuit : ce n'est pas un
/// transfert entre services, et il ne passe pas par le greffon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dupliquer_dans_la_bibliotheque_reste_gratuit_4741() {
    let _verrou = crate::lock_environment();
    let b = banc_licence(false, false).await;
    let repo = PlaylistRepo::with_backend(b.state.backend.clone());
    let id = repo.create("Ma locale", None, 1).unwrap();
    repo.add_tracks(id, &[1], None).unwrap();
    let (st, rendu) = appel(
        &b.app,
        "POST",
        "/api/v1/playlist-manager/transfer",
        json!({
            "source_service": "local",
            "source_playlist_id": id.to_string(),
            "target_service": "local",
            "target_name": "Ma locale (copie)",
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rendu}");
    let copie = repo
        .list(1, 100, 0)
        .unwrap()
        .into_iter()
        .find(|p| p.name == "Ma locale (copie)")
        .unwrap_or_else(|| panic!("aucune copie : {rendu}"));
    assert_eq!(repo.get_track_ids(copie.id.unwrap()).unwrap(), vec![1]);
}

// ---------------------------------------------------------------------------
// 6. Décisions de Bertrand du 08/10/2026 (#5966)
// ---------------------------------------------------------------------------

const BASE_GREFFON: &str = "/api/v1/plugins/playlists-converter";

/// 🔴 Les sauvegardes de l'écran v2 restent GRATUITES, et une restauration
/// recrée la playlist DANS TUNE :
///
/// - un compte gratuit prend une copie datée d'une playlist de service, la
///   liste, puis la restaure ;
/// - la playlist est recréée dans la bibliothèque, avec la piste que la
///   bibliothèque possède, et l'autre est rendue dans `introuvables` ;
/// - elle l'est sous le profil de l'APPELANT (`X-Profile-Id: 2`), pas sous
///   le profil actif global : l'en-tête est transmis au greffon ;
/// - rien n'est écrit chez le service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_sauvegarde_v2_est_gratuite_et_se_restaure_dans_tune_5966() {
    let _verrou = crate::lock_environment();
    let b = banc_licence(true, false).await;

    let (st, rendu) = appel_profil(
        &b.app,
        "POST",
        &format!("{BASE_GREFFON}/snapshot"),
        json!({ "service": "source", "playlist_id": "pl-1" }),
        Some("2"),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "copie datée refusée à un compte gratuit : {rendu}"
    );
    let snapshot_id = rendu["snapshot"]["snapshot_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (st, liste) = appel_profil(
        &b.app,
        "GET",
        &format!("{BASE_GREFFON}/snapshots"),
        Value::Null,
        Some("2"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{liste}");

    let (st, apercu) = appel_profil(
        &b.app,
        "POST",
        &format!("{BASE_GREFFON}/snapshot/restauration/apercu"),
        json!({ "snapshot_id": snapshot_id, "mode": "recreer" }),
        Some("2"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{apercu}");
    assert_eq!(apercu["plan"]["a_rajouter_ids"], json!(["1"]), "{apercu}");
    assert_eq!(
        apercu["introuvables"][0]["titre"], "Introuvable ailleurs",
        "{apercu}"
    );

    let (st, fait) = appel_profil(
        &b.app,
        "POST",
        &format!("{BASE_GREFFON}/snapshot/restauration"),
        json!({ "plan_id": apercu["plan"]["plan_id"], "accord": true }),
        Some("2"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{fait}");
    assert_eq!(fait["plan"]["etat"], "termine", "{fait}");

    let repo = PlaylistRepo::with_backend(b.state.backend.clone());
    let noms = |profil: i64| -> Vec<String> {
        repo.list(profil, 100, 0)
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect()
    };
    let recreee = repo
        .list(2, 100, 0)
        .unwrap()
        .into_iter()
        .find(|p| p.name == "Mes classiques")
        .unwrap_or_else(|| {
            panic!(
                "la playlist restaurée doit être dans Tune, chez le profil 2 : {:?} / profil 1 : {:?}",
                noms(2),
                noms(1)
            )
        });
    assert_eq!(repo.get_track_ids(recreee.id.unwrap()).unwrap(), vec![1]);
    assert!(
        !noms(1).contains(&"Mes classiques".to_string()),
        "pas chez le profil actif global"
    );
    assert!(
        journal(&b.ecritures_source).is_empty() && journal(&b.ecritures_cible).is_empty(),
        "une restauration n'écrit rien chez un service"
    );
}

/// Témoin de l'exception : seules les copies datées sont gratuites. Les liens
/// de synchronisation restent Premium.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_liens_du_greffon_restent_premium_5966() {
    let _verrou = crate::lock_environment();
    let b = banc_licence(true, false).await;
    let (st, refus) = appel(&b.app, "GET", &format!("{BASE_GREFFON}/liens"), Value::Null).await;
    assert_eq!(st, StatusCode::PAYMENT_REQUIRED, "{refus}");
}
