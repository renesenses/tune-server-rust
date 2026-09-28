//! Tune Circle, étape T5 (#5328) : les playlists collaboratives, par
//! références.
//!
//! Contre le faux mozaiklabs de `commun` (contrat de l'issue) : relais fidèle,
//! construction des références (jamais un chemin), résolution chez
//! l'appelant (identifiant de service, puis ISRC, puis texte), aucune
//! écriture chez un service, et la révocation vue au premier appel qui suit.

mod commun;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tune_circle::lecture::Lecture;
use tune_circle::playlists::{self, Collaboratif};
use tune_circle::relais::Relais;
use tune_circle::resolution::Resolveur;
use tune_core::TuneError;
use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::QueueInput;
use tune_core::streaming::ServiceRegistry;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

use commun::*;

// Un faux service de streaming : lit, ne sait pas écrire ---------------------

struct FauxService {
    nom: &'static str,
    connecte: bool,
    pistes: Vec<StreamTrack>,
    /// Toute tentative d'écriture (playlist, favori) : le test exige zéro.
    ecritures: Arc<AtomicUsize>,
    /// Identifiants demandés à `get_track`.
    lus: Arc<Mutex<Vec<String>>>,
}

fn refuse(ecritures: &AtomicUsize) -> TuneError {
    ecritures.fetch_add(1, Ordering::SeqCst);
    TuneError::Streaming("écriture interdite dans ce banc".into())
}

fn piste(id: &str, titre: &str, artiste: &str, duree_ms: u64, isrc: Option<&str>) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: Some("Kind of Blue".into()),
        album_id: None,
        duration_ms: duree_ms,
        cover_path: None,
        track_number: Some(1),
        disc_number: Some(1),
        explicit: false,
        disponible: None,
        isrc: isrc.map(str::to_string),
        composer: None,
        artist_id: None,
        quality: None,
    }
}

#[async_trait]
impl StreamingService for FauxService {
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
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Err(refuse(&self.ecritures))
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.connecte,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Err(refuse(&self.ecritures))
    }
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: self.pistes.clone(),
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        })
    }
    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        self.lus.lock().unwrap().push(track_id.to_string());
        self.pistes
            .iter()
            .find(|p| p.id == track_id)
            .cloned()
            .ok_or_else(|| TuneError::Streaming("introuvable".into()))
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(TuneError::Streaming("pas de flux dans ce banc".into()))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(TuneError::Streaming("non".into()))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![])
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(TuneError::Streaming("non".into()))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(TuneError::Streaming("non".into()))
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![])
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
    async fn create_playlist(&self, _n: &str, _d: Option<&str>) -> Result<String, TuneError> {
        Err(refuse(&self.ecritures))
    }
    async fn add_tracks_to_playlist(&self, _p: &str, _t: &[String]) -> Result<usize, TuneError> {
        Err(refuse(&self.ecritures))
    }
    async fn delete_playlist(&self, _p: &str) -> Result<(), TuneError> {
        Err(refuse(&self.ecritures))
    }
    async fn remove_tracks_from_playlist(
        &self,
        _p: &str,
        _t: &[String],
    ) -> Result<usize, TuneError> {
        Err(refuse(&self.ecritures))
    }
    async fn add_favorite(&mut self, _t: &str, _i: &str) -> Result<(), TuneError> {
        Err(refuse(&self.ecritures))
    }
    async fn remove_favorite(&mut self, _t: &str, _i: &str) -> Result<(), TuneError> {
        Err(refuse(&self.ecritures))
    }
}

// Une fausse lecture : note la file demandée ---------------------------------

#[derive(Default)]
struct FausseLecture {
    files: Mutex<Vec<(i64, Vec<QueueInput>)>>,
}

#[async_trait]
impl Lecture for FausseLecture {
    async fn jouer(&self, zone_id: i64, elements: Vec<QueueInput>) -> Result<usize, String> {
        let n = elements.len();
        self.files.lock().unwrap().push((zone_id, elements));
        Ok(n)
    }
}

// Le banc --------------------------------------------------------------------

const CHEMIN: &str = "/Users/secret-5328/Musique/Miles Davis/Kind of Blue/01 So What.flac";

struct Banc {
    faux: Serveur,
    app: Router,
    backend: Arc<dyn DbBackend>,
    ecritures: Arc<AtomicUsize>,
    lus: Arc<Mutex<Vec<String>>>,
    lecture: Arc<FausseLecture>,
}

/// `services` : (nom, connecté, pistes) de chaque faux service inscrit.
async fn banc(services: Vec<(&'static str, bool, Vec<StreamTrack>)>) -> Banc {
    let faux = demarrer().await;
    let backend = base(&faux.base, Some(JETON));
    let ecritures = Arc::new(AtomicUsize::new(0));
    let lus = Arc::new(Mutex::new(Vec::new()));
    let mut registre = ServiceRegistry::new();
    for (nom, connecte, pistes) in services {
        registre.register(Box::new(FauxService {
            nom,
            connecte,
            pistes,
            ecritures: ecritures.clone(),
            lus: lus.clone(),
        }));
    }
    let lecture = Arc::new(FausseLecture::default());
    let relais = Arc::new(Relais::new(backend.clone()));
    let app = playlists::router(Arc::new(Collaboratif {
        relais,
        resolveur: Resolveur::new(backend.clone(), Arc::new(tokio::sync::Mutex::new(registre))),
        lecture: lecture.clone(),
    }));
    Banc {
        faux,
        app,
        backend,
        ecritures,
        lus,
        lecture,
    }
}

/// Une piste LOCALE dont `file_path` ET `source_id` portent un chemin connu,
/// liée à Qobuz par `track_source_links`.
fn semer_une_piste_locale(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (20, 'Miles Davis');\
             INSERT INTO albums (id, title, artist_id, cover_path) \
             VALUES (10, 'Kind of Blue', 20, '/Users/secret-5328/cover.jpg');\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, duration_ms, \
                                 source, source_id, isrc, musicbrainz_recording_id, cover_path) \
             VALUES (1, 'So What', 10, 20, '{p}', 'flac', 562000, 'local', '{p}', \
                     'USSM15900113', 'mbid-rec-1', '/Users/secret-5328/so-what.jpg');\
             INSERT INTO track_source_links (track_id, service, service_track_id, confidence) \
             VALUES (1, 'qobuz', 'q-so-what', 1.0);",
            p = CHEMIN
        ))
        .unwrap();
    1
}

// 1. Relais ------------------------------------------------------------------

#[tokio::test]
async fn les_routes_de_playlist_relaient_statut_et_corps() {
    let b = banc(vec![]).await;
    let r = appel(&b.app, "GET", "/playlists", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()[0]["id"], 5);
    assert_eq!(r.json()[0]["count"], 3);

    let r = appel(&b.app, "GET", "/playlists/5", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()["items"][0]["item_id"], 51);

    let r = appel(
        &b.app,
        "POST",
        "/playlists",
        Some(json!({ "circle_id": 1, "name": "Soirée", "owner": 999, "extra": true })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::CREATED);
    // Seuls les champs du contrat partent.
    assert_eq!(
        b.faux.etat.lock().unwrap().ecritures_de_playlist[0].1,
        json!({ "circle_id": 1, "name": "Soirée" })
    );

    let r = appel(
        &b.app,
        "PATCH",
        "/playlists/5",
        Some(json!({ "name": "Lundi", "version": 3, "circle_id": 2 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()["name"], "Lundi");
    assert_eq!(
        b.faux.etat.lock().unwrap().ecritures_de_playlist[1].1,
        json!({ "name": "Lundi", "version": 3 })
    );

    let r = appel(&b.app, "DELETE", "/playlists/5/items/52?version=4", None).await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    assert_eq!(
        b.faux.etat.lock().unwrap().requete_de_retrait.as_deref(),
        Some("version=4")
    );

    let r = appel(
        &b.app,
        "PUT",
        "/playlists/5/order",
        Some(json!({ "item_ids": [53, 51], "version": 5 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    assert_eq!(r.json()["items"][0]["item_id"], 53);

    // Pas une permutation exacte : le 422 du cloud, relayé.
    let r = appel(
        &b.app,
        "PUT",
        "/playlists/5/order",
        Some(json!({ "item_ids": [53], "version": 6 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);

    let r = appel(&b.app, "DELETE", "/playlists/5", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));
}

/// Deux ajouts sur la même version : le second reçoit le 409 du cloud, avec
/// l'état courant, octet pour octet.
#[tokio::test]
async fn un_conflit_de_version_est_relaye_avec_l_etat_courant() {
    let b = banc(vec![]).await;
    let ajout = json!({ "items": [{ "title": "Freddie Freeloader" }], "version": 3 });
    let r = appel(&b.app, "POST", "/playlists/5/items", Some(ajout.clone())).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()["version"], 4);
    let r = appel(&b.app, "POST", "/playlists/5/items", Some(ajout)).await;
    assert_eq!(r.statut, StatusCode::CONFLICT);
    assert_eq!(r.json()["error"], "version_conflict");
    assert_eq!(r.json()["playlist"]["version"], 4);
    assert_eq!(r.json()["playlist"]["items"].as_array().unwrap().len(), 4);
}

/// Sans session SSO : 412 « non connecté », et rien ne part.
#[tokio::test]
async fn sans_session_les_routes_de_playlist_rendent_412() {
    let b = banc(vec![]).await;
    tune_core::db::settings_repo::SettingsRepo::with_backend(b.backend.clone())
        .set("mozaik_access_token", "")
        .unwrap();
    for (m, chemin, corps) in [
        ("GET", "/playlists", None),
        ("GET", "/playlists/5", None),
        ("POST", "/playlists/5/resolve", None),
        ("POST", "/playlists/5/play", Some(json!({ "zone_id": 1 }))),
    ] {
        let r = appel(&b.app, m, chemin, corps).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {chemin}");
        assert_eq!(r.json()["code"], "circle.not_connected");
    }
    assert_eq!(b.faux.etat.lock().unwrap().appels, 0);
}

// 2. Références : jamais un chemin -------------------------------------------

/// LA garde de #5328 : une piste locale dont `file_path` et `source_id`
/// valent un chemin connu devient une référence où ce chemin n'est NULLE PART
/// — ni la moindre clé hors de la liste blanche.
#[tokio::test]
async fn track_ids_devient_une_reference_sans_aucun_chemin() {
    let b = banc(vec![]).await;
    let tid = semer_une_piste_locale(&b.backend);
    let r = appel(
        &b.app,
        "POST",
        "/playlists/5/items",
        Some(json!({ "track_ids": [tid], "version": 3 })),
    )
    .await;
    // Le corps reçu par le cloud d'abord : c'est LUI qui ne doit porter aucun
    // chemin, quel que soit le verdict du cloud.
    let envoye = b
        .faux
        .etat
        .lock()
        .unwrap()
        .ecritures_de_playlist
        .first()
        .map(|e| e.1.clone())
        .unwrap_or_else(|| panic!("rien n'est parti vers le cloud : {} {}", r.statut, r.json()));
    let brut = envoye.to_string();
    for interdit in [
        CHEMIN,
        "/Users",
        "secret-5328",
        "file_path",
        "source_id",
        "cover_path",
        "mbid-rec-1",
    ] {
        assert!(
            !brut.contains(interdit),
            "« {interdit} » est parti : {brut}"
        );
    }
    assert_eq!(
        envoye,
        json!({
            "items": [{
                "title": "So What", "artist_name": "Miles Davis", "album_title": "Kind of Blue",
                "duration_ms": 562000, "isrc": "USSM15900113", "qobuz_id": "q-so-what"
            }],
            "version": 3
        })
    );
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
}

/// Une piste désignée qui n'existe pas : 422 nommé, et RIEN ne part.
#[tokio::test]
async fn une_piste_inconnue_n_envoie_rien() {
    let b = banc(vec![]).await;
    let r = appel(
        &b.app,
        "POST",
        "/playlists/5/items",
        Some(json!({ "track_ids": [999], "version": 3 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "circle.unknown_track");
    assert_eq!(b.faux.etat.lock().unwrap().appels, 0);
}

/// Un titre de service connecté : la référence porte son identifiant sous la
/// clé du service, et l'ISRC que le service expose.
#[tokio::test]
async fn un_titre_de_service_devient_une_reference_avec_son_isrc() {
    let b = banc(vec![(
        "tidal",
        true,
        vec![piste(
            "t-42",
            "Blue in Green",
            "Miles Davis",
            337_000,
            Some("USSM15900115"),
        )],
    )])
    .await;
    let r = appel(
        &b.app,
        "POST",
        "/playlists/5/items",
        Some(
            json!({ "service_tracks": [{ "source": "tidal", "source_id": "t-42" }], "version": 3 }),
        ),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    assert_eq!(
        b.faux.etat.lock().unwrap().ecritures_de_playlist[0].1["items"][0],
        json!({ "title": "Blue in Green", "artist_name": "Miles Davis",
                "album_title": "Kind of Blue", "duration_ms": 337000,
                "isrc": "USSM15900115", "tidal_id": "t-42" })
    );
    assert_eq!(b.ecritures.load(Ordering::SeqCst), 0);
}

/// Une référence brute portant un chemin part telle quelle, et c'est le cloud
/// qui la refuse (422) : rien n'est écrit.
#[tokio::test]
async fn une_reference_brute_hors_liste_blanche_est_refusee_par_le_cloud() {
    let b = banc(vec![]).await;
    let r = appel(
        &b.app,
        "POST",
        "/playlists/5/items",
        Some(json!({ "items": [{ "title": "X", "file_path": "/tmp/x.flac" }], "version": 3 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        b.faux.etat.lock().unwrap().playlists[0]["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

// 3. Résolution chez l'appelant ----------------------------------------------

fn ligne_de(r: &Value, item_id: i64) -> Value {
    r.as_array()
        .unwrap()
        .iter()
        .find(|l| l["item_id"] == item_id)
        .cloned()
        .unwrap()
}

/// Un utilisateur Qobuz : `qobuz_id` est retenu tel quel, sans recherche.
#[tokio::test]
async fn un_utilisateur_qobuz_rejoue_par_l_identifiant_qobuz() {
    let b = banc(vec![(
        "qobuz",
        true,
        vec![piste("q-so-what", "So What", "Miles Davis", 562_000, None)],
    )])
    .await;
    let r = appel(&b.app, "POST", "/playlists/5/resolve", None).await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    assert_eq!(
        ligne_de(&r.json(), 51),
        json!({ "item_id": 51, "status": "matched", "source": "qobuz",
                "source_id": "q-so-what", "method": "service_id" })
    );
    // La forme que lit l'écran (web#1731) : un tableau, dans l'ordre.
    let ids: Vec<Value> = r
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["item_id"].clone())
        .collect();
    assert_eq!(ids, vec![json!(51), json!(52), json!(53)]);
    assert_eq!(b.ecritures.load(Ordering::SeqCst), 0);
}

/// Un utilisateur Tidal SEUL, pour un morceau qui ne porte que `qobuz_id` et
/// l'ISRC : le repli ISRC trouve chez Tidal le titre même mal étiqueté, et
/// le préfère à un homonyme par le texte. L'identifiant Qobuz n'est jamais
/// demandé : l'utilisateur n'a pas Qobuz.
#[tokio::test]
async fn sans_le_service_de_la_reference_le_repli_isrc_trouve_ailleurs() {
    let b = banc(vec![
        (
            "tidal",
            true,
            vec![
                piste("t-homonyme", "Blue in Green", "Miles Davis", 337_000, None),
                piste(
                    "t-bon",
                    "Blue In Green (Remastered)",
                    "Miles Davis",
                    337_000,
                    Some("USSM15900115"),
                ),
            ],
        ),
        (
            "qobuz",
            false,
            vec![piste(
                "q-bleu",
                "Blue in Green",
                "Miles Davis",
                337_000,
                None,
            )],
        ),
    ])
    .await;
    b.faux.etat.lock().unwrap().playlists[0]["items"][1]["qobuz_id"] = json!("q-bleu");
    let r = appel(&b.app, "POST", "/playlists/5/resolve", None).await;
    assert_eq!(
        ligne_de(&r.json(), 52),
        json!({ "item_id": 52, "status": "matched", "source": "tidal",
                "source_id": "t-bon", "method": "isrc" })
    );
    assert!(
        !b.lus.lock().unwrap().iter().any(|id| id == "q-bleu"),
        "un service déconnecté ne doit pas être interrogé"
    );
}

/// Ni identifiant, ni ISRC : titre, artiste et durée. Et `not_found` quand
/// rien ne correspond.
#[tokio::test]
async fn repli_texte_puis_introuvable() {
    let b = banc(vec![(
        "deezer",
        true,
        vec![piste("d-9", "So What", "Miles Davis", 562_000, None)],
    )])
    .await;
    {
        let mut f = b.faux.etat.lock().unwrap();
        let item = &mut f.playlists[0]["items"][0];
        let o = item.as_object_mut().unwrap();
        o.remove("isrc");
        o.remove("qobuz_id");
        o.remove("tidal_id");
    }
    let r = appel(&b.app, "POST", "/playlists/5/resolve", None).await;
    let j = r.json();
    assert_eq!(
        ligne_de(&j, 51),
        json!({ "item_id": 51, "status": "matched", "source": "deezer",
                "source_id": "d-9", "method": "text" })
    );
    assert_eq!(
        ligne_de(&j, 53),
        json!({ "item_id": 53, "status": "not_found", "source": null, "source_id": null })
    );
    let introuvables = j
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["status"] == "not_found")
        .count();
    assert_eq!(introuvables, 2, "{j}");
    assert_eq!(b.ecritures.load(Ordering::SeqCst), 0);
}

/// La bibliothèque de l'appelant : son fichier, désigné par son identifiant
/// de ligne — jamais par la colonne `source_id`, qui porte ici un chemin.
#[tokio::test]
async fn une_piste_de_la_bibliotheque_est_rendue_sans_son_chemin() {
    let b = banc(vec![]).await;
    semer_une_piste_locale(&b.backend);
    let r = appel(&b.app, "POST", "/playlists/5/resolve", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        ligne_de(&r.json(), 51),
        json!({ "item_id": 51, "status": "matched", "source": "local",
                "source_id": "1", "track_id": 1, "method": "isrc" })
    );
    assert!(!String::from_utf8_lossy(&r.octets).contains("/Users"));
}

// 4. Lecture -----------------------------------------------------------------

#[tokio::test]
async fn jouer_met_en_file_ce_qui_est_trouve_et_nomme_ce_qui_manque() {
    let b = banc(vec![(
        "qobuz",
        true,
        vec![piste("q-so-what", "So What", "Miles Davis", 562_000, None)],
    )])
    .await;
    let r = appel(
        &b.app,
        "POST",
        "/playlists/5/play",
        Some(json!({ "zone_id": 3 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    assert_eq!(r.json()["queued"], 1);
    assert_eq!(r.json()["missing"], json!([52, 53]));
    let files = b.lecture.files.lock().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, 3);
    assert!(matches!(
        &files[0].1[..],
        [QueueInput::Streaming { source, source_id, .. }] if source == "qobuz" && source_id == "q-so-what"
    ));
    assert_eq!(b.ecritures.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn jouer_sans_zone_ou_sans_rien_de_jouable_ne_lance_rien() {
    let b = banc(vec![]).await;
    let r = appel(&b.app, "POST", "/playlists/5/play", Some(json!({}))).await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "circle.zone_required");
    let r = appel(
        &b.app,
        "POST",
        "/playlists/5/play",
        Some(json!({ "zone_id": 3 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "circle.nothing_playable");
    assert!(b.lecture.files.lock().unwrap().is_empty());
}

// 5. Révocation --------------------------------------------------------------

/// Le contact est retiré (révocation, retrait du cercle, suppression) entre
/// deux appels : le cloud passe à 404, et le greffon rend 404 au second, sur
/// chaque route, sans rien servir de mémoire — ni résolution, ni lecture.
#[tokio::test]
async fn apres_revocation_chaque_route_rend_404_sans_rien_servir_de_memoire() {
    let b = banc(vec![(
        "qobuz",
        true,
        vec![piste("q-so-what", "So What", "Miles Davis", 562_000, None)],
    )])
    .await;
    let r = appel(&b.app, "POST", "/playlists/5/resolve", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    let r = appel(
        &b.app,
        "POST",
        "/playlists/5/play",
        Some(json!({ "zone_id": 3 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);

    b.faux.etat.lock().unwrap().playlists_coupees = true;
    let lectures_avant = b.faux.etat.lock().unwrap().lectures_de_playlist;
    for (m, chemin, corps) in [
        ("GET", "/playlists/5", None),
        (
            "PATCH",
            "/playlists/5",
            Some(json!({ "name": "x", "version": 3 })),
        ),
        (
            "POST",
            "/playlists/5/items",
            Some(json!({ "items": [{ "title": "x" }], "version": 3 })),
        ),
        ("DELETE", "/playlists/5/items/51?version=3", None),
        (
            "PUT",
            "/playlists/5/order",
            Some(json!({ "item_ids": [51, 52, 53], "version": 3 })),
        ),
        ("POST", "/playlists/5/resolve", None),
        ("POST", "/playlists/5/play", Some(json!({ "zone_id": 3 }))),
        ("DELETE", "/playlists/5", None),
    ] {
        let r = appel(&b.app, m, chemin, corps).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{m} {chemin}");
        assert_eq!(r.json(), json!({ "error": "not_found" }), "{m} {chemin}");
    }
    assert_eq!(
        b.faux.etat.lock().unwrap().lectures_de_playlist,
        lectures_avant + 3,
        "resolve et play relisent la playlist au cloud à chaque appel"
    );
    let r = appel(&b.app, "GET", "/playlists", None).await;
    assert_eq!(r.json(), json!([]));
    assert_eq!(
        b.lecture.files.lock().unwrap().len(),
        1,
        "aucune lecture lancée après la révocation"
    );
}

/// Un identifiant qui n'en est pas un : refus local, aucun appel.
#[tokio::test]
async fn un_identifiant_vide_ou_point_point_est_refuse_sans_appel() {
    let b = banc(vec![]).await;
    for chemin in [
        "/playlists/..",
        "/playlists/../resolve",
        "/playlists/5/items/..",
    ] {
        let m = if chemin.ends_with("resolve") {
            "POST"
        } else if chemin.contains("items") {
            "DELETE"
        } else {
            "GET"
        };
        let r = appel(&b.app, m, chemin, None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{chemin}");
    }
    assert_eq!(b.faux.etat.lock().unwrap().appels, 0);
}

// 6. Versions : ETag et If-Match (site-mozaiklabs#236) -------------------------

async fn appel_avec_if_match(
    app: &Router,
    methode: &str,
    chemin: &str,
    if_match: &str,
    corps: Value,
) -> Rendu {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method(methode)
        .uri(chemin)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::IF_MATCH, if_match)
        .body(axum::body::Body::from(corps.to_string()))
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let statut = r.status();
    let entetes = r.headers().clone();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Rendu {
        statut,
        entetes,
        octets,
    }
}

/// L'`ETag` du cloud est relayé, sur une lecture comme sur un 409 ; et sans
/// `version` dans le corps, l'`If-Match` du client part à sa place — aucune
/// clé `version` nulle n'est inventée.
#[tokio::test]
async fn l_etag_est_relaye_et_if_match_part_quand_la_version_manque() {
    let b = banc(vec![]).await;
    let r = appel(&b.app, "GET", "/playlists/5", None).await;
    assert_eq!(r.entetes.get("etag").unwrap(), "\"3\"");

    let r = appel_avec_if_match(
        &b.app,
        "POST",
        "/playlists/5/items",
        "\"3\"",
        json!({ "items": [{ "title": "Freddie Freeloader" }] }),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    assert_eq!(r.entetes.get("etag").unwrap(), "\"4\"");
    {
        let f = b.faux.etat.lock().unwrap();
        assert_eq!(f.if_match_recus.last().unwrap().as_deref(), Some("\"3\""));
        assert!(
            f.ecritures_de_playlist[0].1.get("version").is_none(),
            "{:?}",
            f.ecritures_de_playlist[0].1
        );
    }

    // If-Match périmé : 409, corps et ETag de l'état courant relayés.
    let r = appel_avec_if_match(
        &b.app,
        "PUT",
        "/playlists/5/order",
        "\"3\"",
        json!({ "item_ids": [51, 52, 53, 103] }),
    )
    .await;
    assert_eq!(r.statut, StatusCode::CONFLICT);
    assert_eq!(r.entetes.get("etag").unwrap(), "\"4\"");
    assert_eq!(r.json()["playlist"]["version"], 4);
}

#[tokio::test]
async fn plus_de_cent_ajouts_sont_refuses_sans_rien_chercher() {
    let b = banc(vec![]).await;
    let ids: Vec<i64> = (1..=101).collect();
    let r = appel(
        &b.app,
        "POST",
        "/playlists/5/items",
        Some(json!({ "track_ids": ids, "version": 3 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "circle.too_many_items");
    assert_eq!(b.faux.etat.lock().unwrap().appels, 0);
}

// 7. Récupération à la suppression du cercle ----------------------------------

#[tokio::test]
async fn les_recuperables_sont_relayees() {
    let b = banc(vec![]).await;
    let r = appel(&b.app, "GET", "/recoverable-playlists", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()[0]["id"], 8);
    assert_eq!(r.json()[0]["count"], 3);
    let r = appel(&b.app, "GET", "/recoverable-playlists/8", None).await;
    assert_eq!(r.json()["items"][1]["qobuz_id"], "q-bleu");
    let r = appel(&b.app, "DELETE", "/recoverable-playlists/8", None).await;
    assert_eq!(r.json(), json!({ "ok": true }));
    let r = appel(&b.app, "GET", "/recoverable-playlists/8", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
}

/// La copie : une playlist LOCALE, chaque morceau rejoué chez l'appelant
/// (sa bibliothèque par l'ISRC, Qobuz par l'identifiant), l'introuvable
/// nommé ; puis le droit de récupération rendu au cloud.
#[tokio::test]
async fn copier_une_recuperable_ecrit_une_playlist_locale_puis_rend_le_droit() {
    let b = banc(vec![(
        "qobuz",
        true,
        vec![piste(
            "q-bleu",
            "Blue in Green",
            "Miles Davis",
            337_000,
            None,
        )],
    )])
    .await;
    let tid = semer_une_piste_locale(&b.backend);
    let r = appel(&b.app, "POST", "/recoverable-playlists/8/copy", None).await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    let j = r.json();
    assert_eq!(j["copied"], 2);
    assert_eq!(j["missing"], json!([83]));
    assert_eq!(j["released"], true);
    assert_eq!(j["name"], "Jazz du samedi");
    assert_eq!(j["playlist"]["name"], "Jazz du samedi");
    assert_eq!(j["playlist"]["id"], j["playlist_id"]);
    assert_eq!(
        j["not_copied"],
        json!([{ "item_id": 83, "title": "Un titre que personne n'a", "artist_name": "Inconnu" }])
    );
    assert_eq!(
        b.faux.etat.lock().unwrap().renonciations,
        vec!["8".to_string()]
    );

    let repo = tune_core::db::playlist_repo::PlaylistRepo::with_backend(b.backend.clone());
    let pid = j["playlist_id"].as_i64().unwrap();
    assert_eq!(
        repo.get_for_profile(pid, 1).unwrap().unwrap().name,
        "Jazz du samedi"
    );
    let lignes: Vec<_> = repo
        .get_entries(pid)
        .unwrap()
        .into_iter()
        .map(|l| l.content)
        .collect();
    assert_eq!(lignes.len(), 2);
    assert_eq!(
        lignes[0],
        tune_core::db::playlist_repo::EntryContent::Local(tid)
    );
    match &lignes[1] {
        tune_core::db::playlist_repo::EntryContent::Service(s) => {
            assert_eq!(
                (s.source.as_str(), s.source_id.as_str()),
                ("qobuz", "q-bleu")
            );
        }
        autre => panic!("un titre Qobuz était attendu : {autre:?}"),
    }
    assert_eq!(b.ecritures.load(Ordering::SeqCst), 0);
}

/// Un droit perdu (révocation après la suppression du cercle, ou déjà
/// exercé) : 404 relayé, AUCUNE playlist locale écrite, rien rendu.
#[tokio::test]
async fn sans_droit_de_recuperation_aucune_copie_n_est_ecrite() {
    let b = banc(vec![]).await;
    b.faux.etat.lock().unwrap().recuperables.clear();
    let r = appel(&b.app, "POST", "/recoverable-playlists/8/copy", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "error": "not_found" }));
    let repo = tune_core::db::playlist_repo::PlaylistRepo::with_backend(b.backend.clone());
    assert!(repo.list(1, 100, 0).unwrap().is_empty());
    assert!(b.faux.etat.lock().unwrap().renonciations.is_empty());
}
