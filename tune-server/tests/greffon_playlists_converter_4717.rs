//! #4717 — le greffon « Playlists converter », de bout en bout.
//!
//! Le moteur du convertisseur est essayé chez lui, en Rust ordinaire
//! (`plugins/tune-playlists-converter/src/essais.rs`). Ici on garde l'autre
//! moitié, celle qu'on ne peut prouver qu'ici : le **vrai** `main.wasm` chargé
//! dans le bac à sable, monté sous `/api/v1/plugins/playlists-converter/…`, et
//! conduit par HTTP contre un service de banc qui COMPTE ses écritures.
//!
//! Quatre faits, et ce sont les quatre promesses du chantier :
//!
//! 1. **Facultatif** : sans installation, le greffon ne se charge pas — rien
//!    dans le registre, aucune route.
//! 2. **Payant** : installé mais hors Premium, chacune de ses routes répond
//!    402 `premium_required`, et le greffon n'est même pas appelé.
//! 3. **L'aperçu n'écrit rien** : après `POST /apercu`, le service de banc n'a
//!    vu ni création ni ajout. La contre-épreuve est le même compteur après
//!    `POST /executer`.
//! 4. **Une reprise ne duplique pas** : rejouer `/executer` sur un lot terminé
//!    ne recrée ni ne réécrit rien.
#![cfg(feature = "plugins-wasm")]

use std::sync::Arc;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

const GREFFON: &str = "playlists-converter";
const SERVICE: &str = "banc";

/// Le dossier de greffons COMMITÉ, résolu à la compilation : il contient
/// `party/` et `playlists-converter/`.
fn dossier_des_greffons() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("plugins")
}

// ---------------------------------------------------------------------------
// Le service de banc : il note tout ce qu'on lui fait écrire
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Ecritures {
    creees: Vec<String>,
    ajoutees: Vec<(String, Vec<String>)>,
}

impl Ecritures {
    fn total(&self) -> usize {
        self.creees.len() + self.ajoutees.len()
    }
}

struct ServiceDeBanc {
    ecritures: Arc<Mutex<Ecritures>>,
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
        SERVICE
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

    /// Le service connaît « La Bohème » et « Emmenez-moi », et rien d'autre :
    /// « Inédit » restera introuvable, et devra être RAPPORTÉ comme tel.
    async fn search(&self, query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        let mut tracks = Vec::new();
        if query.contains("Bohème") {
            tracks.push(piste("banc-1", "La Bohème (Remastered 2014)"));
        }
        if query.contains("Emmenez") {
            tracks.push(piste("banc-2", "Emmenez-moi"));
        }
        Ok(SearchResults {
            tracks,
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }

    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_track_url(
        &self,
        _id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
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

    async fn create_playlist(
        &self,
        name: &str,
        _description: Option<&str>,
    ) -> Result<String, TuneError> {
        self.ecritures.lock().unwrap().creees.push(name.to_string());
        Ok("pl-neuve".to_string())
    }
    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        self.ecritures
            .lock()
            .unwrap()
            .ajoutees
            .push((playlist_id.to_string(), track_ids.to_vec()));
        Ok(track_ids.len())
    }
    fn supports_write(&self) -> bool {
        true
    }
}

fn piste(id: &str, titre: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: "Charles Aznavour".into(),
        album: None,
        album_id: None,
        duration_ms: 210_000,
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

/// Un serveur avec une playlist locale de trois titres, le service de banc, et
/// le greffon chargé si `installe`.
async fn socle(installe: bool, premium: bool) -> (AppState, Arc<Mutex<Ecritures>>) {
    unsafe {
        std::env::set_var("TUNE_PLUGINS_DIR", dossier_des_greffons());
        // Depuis un binaire libtest, relancer `current_exe` pour sonder
        // wasmtime rejouerait toute la suite.
        std::env::set_var("TUNE_WASM_PROBE_SKIP", "1");
    }

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Charles Aznavour')",
            &[],
        )
        .unwrap();
    for (id, titre) in [(1, "La Bohème"), (2, "Emmenez-moi"), (3, "Inédit")] {
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, artist_id, duration_ms) VALUES (?, ?, 1, 210000)",
                &[&id as &dyn ToSqlValue, &titre],
            )
            .unwrap();
    }
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let playlist = repo.create("Mes classiques", None, 1).unwrap();
    repo.add_tracks(playlist, &[1, 2, 3], None).unwrap();

    let ecritures = Arc::new(Mutex::new(Ecritures::default()));
    state
        .services
        .lock()
        .await
        .register(Box::new(ServiceDeBanc {
            ecritures: ecritures.clone(),
        }));

    if installe {
        // C'est EXACTEMENT ce que `POST /plugins/{id}/enable` écrit : le
        // greffon est facultatif, il faut l'installer pour qu'il se charge.
        SettingsRepo::with_backend(state.backend.clone())
            .set(&format!("plugin_{GREFFON}_enabled"), "true")
            .unwrap();
    }
    if premium {
        state.license.set_account_premium(true, None).await;
    }

    tune_server::plugins_host::load_wasm_plugins(&state).await;
    (state, ecritures)
}

/// L'identifiant de la playlist locale semée par [`socle`].
fn playlist_locale(state: &AppState) -> i64 {
    PlaylistRepo::with_backend(state.backend.clone())
        .list(1, 10, 0)
        .unwrap()
        .first()
        .and_then(|p| p.id)
        .expect("la playlist semée doit exister")
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let requete = if corps.is_null() {
        Request::builder()
            .method(methode)
            .uri(chemin)
            .body(Body::empty())
            .unwrap()
    } else {
        Request::builder()
            .method(methode)
            .uri(chemin)
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap()
    };
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn demande_vers_le_banc(playlist: i64) -> Value {
    json!({
        "sources": [{ "kind": "local", "id": playlist }],
        "cible": { "kind": "service", "service": SERVICE },
    })
}

// ---------------------------------------------------------------------------
// 1. Facultatif
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_greffon_facultatif_ne_se_charge_pas_sans_installation() {
    let _garde = crate::lock_environment();
    let (state, _) = socle(false, true).await;
    let registre = state.wasm_plugins.get().expect("registre publié");
    assert!(
        registre.get(GREFFON).is_none(),
        "`default_enabled: false` : tant que personne ne l'a installé, il dort"
    );
    // Contre-épreuve : le greffon voisin, lui, n'a pas ce marqueur et charge.
    assert!(
        registre.get("party").is_some(),
        "la règle ne doit pas empêcher les greffons ordinaires de charger"
    );

    // Et sa route n'existe pas.
    let app = tune_server::routes::router(state.clone());
    let (statut, _) = appeler(
        &app,
        "POST",
        &format!("/api/v1/plugins/{GREFFON}/apercu"),
        json!({}),
    )
    .await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 2. Payant
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hors_premium_toutes_les_routes_sont_refusees() {
    let _garde = crate::lock_environment();
    let (state, ecritures) = socle(true, false).await;
    assert!(
        state.wasm_plugins.get().unwrap().get(GREFFON).is_some(),
        "installé, le greffon doit charger"
    );
    let app = tune_server::routes::router(state.clone());
    let playlist = playlist_locale(&state);

    for (methode, chemin, corps) in [
        ("GET", "sources", Value::Null),
        ("POST", "apercu", demande_vers_le_banc(playlist)),
        (
            "POST",
            "executer",
            json!({ "transfert_id": "t1", "confirme": true }),
        ),
        ("GET", "transferts", Value::Null),
    ] {
        let (statut, corps_rendu) = appeler(
            &app,
            methode,
            &format!("/api/v1/plugins/{GREFFON}/{chemin}"),
            corps,
        )
        .await;
        assert_eq!(
            statut,
            StatusCode::PAYMENT_REQUIRED,
            "{methode} /{chemin} doit être refusé hors Premium — vu {corps_rendu}"
        );
        assert_eq!(corps_rendu["error"], "premium_required");
    }
    assert_eq!(
        ecritures.lock().unwrap().total(),
        0,
        "un refus ne doit évidemment rien écrire"
    );
}

// ---------------------------------------------------------------------------
// 3. L'aperçu n'écrit rien
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l_apercu_n_ecrit_rien_puis_l_execution_ecrit() {
    let _garde = crate::lock_environment();
    let (state, ecritures) = socle(true, true).await;
    let app = tune_server::routes::router(state.clone());
    let playlist = playlist_locale(&state);

    let (statut, apercu) = appeler(
        &app,
        "POST",
        &format!("/api/v1/plugins/{GREFFON}/apercu"),
        demande_vers_le_banc(playlist),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{apercu}");
    assert_eq!(apercu["etat"], "apercu");
    assert_eq!(apercu["comptes"]["total"], 3);
    assert_eq!(apercu["comptes"]["appariees"], 2);
    assert_eq!(apercu["comptes"]["introuvables"], 1);
    assert_eq!(apercu["ecritures"], 0);
    assert_eq!(
        ecritures.lock().unwrap().total(),
        0,
        "🔴 l'aperçu n'a le droit de toucher NI la bibliothèque NI le service"
    );

    // Le titre que le service ne connaît pas est rapporté, avec sa raison, et
    // aucun identifiant ne lui est inventé.
    let titres = apercu["playlists"][0]["titres"].as_array().unwrap();
    let inedit = titres
        .iter()
        .find(|t| t["titre"] == "Inédit")
        .expect("le titre introuvable doit figurer au rapport");
    assert_eq!(inedit["statut"], "introuvable");
    assert_eq!(inedit["raison"], "aucun_resultat");
    assert!(inedit.get("cible_piste").is_none());

    // Sans accord explicite, rien ne part.
    let transfert = apercu["transfert_id"].as_str().unwrap().to_string();
    let (statut, _) = appeler(
        &app,
        "POST",
        &format!("/api/v1/plugins/{GREFFON}/executer"),
        json!({ "transfert_id": transfert }),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(ecritures.lock().unwrap().total(), 0);

    // Contre-épreuve : avec l'accord, le service voit enfin passer l'écriture.
    let (statut, execute) = appeler(
        &app,
        "POST",
        &format!("/api/v1/plugins/{GREFFON}/executer"),
        json!({ "transfert_id": transfert, "confirme": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{execute}");
    assert_eq!(execute["etat"], "termine");
    assert_eq!(execute["ecritures"], 2);
    {
        let journal = ecritures.lock().unwrap();
        assert_eq!(journal.creees.as_slice(), &["Mes classiques".to_string()]);
        assert_eq!(journal.ajoutees.len(), 1);
        assert_eq!(journal.ajoutees[0].0, "pl-neuve");
        assert_eq!(
            journal.ajoutees[0].1,
            vec!["banc-1".to_string(), "banc-2".to_string()],
            "seuls les titres appariés partent — jamais l'introuvable"
        );
    }

    // 4. Rejouer : la reprise ne duplique rien.
    let (statut, rejoue) = appeler(
        &app,
        "POST",
        &format!("/api/v1/plugins/{GREFFON}/executer"),
        json!({ "transfert_id": transfert, "confirme": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rejoue}");
    let journal = ecritures.lock().unwrap();
    assert_eq!(
        journal.creees.len(),
        1,
        "la playlist cible ne doit être créée qu'une fois"
    );
    assert_eq!(
        journal.ajoutees.len(),
        1,
        "les titres déjà posés ne doivent pas repartir"
    );
}

// ---------------------------------------------------------------------------
// 4. L'état vit dans le stockage cloisonné du greffon
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l_etat_du_lot_vit_dans_le_kv_cloisonne() {
    let _garde = crate::lock_environment();
    let (state, _) = socle(true, true).await;
    let app = tune_server::routes::router(state.clone());
    let playlist = playlist_locale(&state);

    let (_, apercu) = appeler(
        &app,
        "POST",
        &format!("/api/v1/plugins/{GREFFON}/apercu"),
        demande_vers_le_banc(playlist),
    )
    .await;
    let transfert = apercu["transfert_id"].as_str().unwrap().to_string();

    // La clé est celle de l'hôte, préfixée par l'identifiant du greffon : un
    // autre greffon ne peut ni la lire ni l'écraser (#4716).
    let brut = SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("plugin_kv:{GREFFON}:transfert:{transfert}"))
        .unwrap()
        .expect("le plan doit être persisté sous la clé cloisonnée du greffon");
    let plan: Value = serde_json::from_str(&brut).unwrap();
    assert_eq!(plan["etat"], "apercu");
    assert_eq!(plan["blocs"][0]["nom"], "Mes classiques");

    // Et la route de relecture rend le même rapport, sans rien réexécuter.
    let (statut, rapport) = appeler(
        &app,
        "GET",
        &format!("/api/v1/plugins/{GREFFON}/transfert?transfert_id={transfert}"),
        Value::Null,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rapport}");
    assert_eq!(rapport["transfert_id"], transfert);
    assert_eq!(rapport["ecritures"], 0);
}
