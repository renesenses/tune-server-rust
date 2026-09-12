//! #3226 — `GET /search` rendait la bibliothèque locale quel que soit `sources`.
//!
//! Reivax66, forum fil 1647 (02/09/2026), 0.9.130 Windows/SQLite, 17 588
//! pistes, un seul service authentifié (Qobuz) : « si on clique sur Tous c'est
//! bon, si on clique sur Local c'est bon. Mais si on clique sur Qobuz, c'est le
//! même résultat que Tous. »
//!
//! Ce n'était pas une illusion d'optique. `sources` était lu APRÈS les quatre
//! recherches locales et ne filtrait que la boucle des services : le bloc
//! `local` et `radios` partaient dans toutes les réponses. « Qobuz » rendait
//! donc `local + qobuz`, et chez quelqu'un dont c'est le seul service
//! authentifié, c'est mot pour mot ce que rend « Tous ». La pilule « Local »,
//! elle, semblait marcher parce qu'elle EXCLUAIT le service — pas parce
//! qu'elle incluait le local.
//!
//! # Ce que ces essais mesurent
//!
//! Le CORPS JSON de la réponse, et rien d'autre. Aucune assertion ne rappelle
//! la condition de filtrage écrite dans `routes/search.rs` : un essai qui
//! recopie la condition du code ne la garde pas, il la duplique. Ici on pose
//! une URL et on regarde ce qui sort.
//!
//! La base porte, pour le même mot, une piste, un album, un artiste ET une
//! station de radio ; le registre porte un service authentifié qui répond à ce
//! même mot. Les deux moitiés sont donc non vides en même temps, et « vide »
//! ne peut jamais vouloir dire « il n'y avait rien à trouver ».
//!
//!   1. [`la_pilule_d_un_service_ne_rend_plus_la_bibliotheque`] — le défaut.
//!   2. [`la_pilule_locale_ne_rend_aucun_service`] — l'inverse, qui interdit de
//!      « corriger » en vidant le local dans tous les cas.
//!   3. [`le_temoin_sans_sources_rend_les_deux_moities`] — LE TÉMOIN. « Tous »
//!      est le seul cas qui marchait déjà ; il doit rester intact.
//!   4. [`local_et_service_ensemble_rendent_les_deux`] — la liste est une
//!      union, pas un choix exclusif.
//!   5. [`le_joker_all_vaut_tout`] — `all` ne rend pas moins que l'absence.
//!   6. [`une_source_inconnue_ne_selectionne_rien`] — le contrat des bords.
//!   7. [`la_forme_lue_par_le_client_reste_entiere_quand_le_local_est_ecarte`]
//!      — la clé `local` reste PRÉSENTE avec des tableaux vides. Un champ
//!      absent et un champ vide ne se comportent pas pareil en JavaScript, et
//!      `federatedSearch` (`tune-web-client/src/lib/api.ts`) fait
//!      `if (result.local) result.local.tracks = mapStreamingTracks(result.local.tracks)`.
//!      Rendre `local` sans `tracks` planterait l'écran au lieu de le corriger.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::TuneError;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_core::db::track_repo::TrackRepo;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

/// Le mot cherché. Il ne peut correspondre à rien de semé par les migrations —
/// le catalogue de radios livré en est un (migration 90).
const MOT: &str = "Reivax";

/// Le service factice. Un nom qui ne peut pas collider avec un service réel du
/// registre, pour que « ce bloc est là » ne soit jamais une coïncidence.
const SERVICE: &str = "banc-essai";

// --- un service authentifié qui répond ---------------------------------

/// Le seul point de ce faux service : être AUTHENTIFIÉ et rendre un résultat
/// non vide. Sans lui, `services` serait `{}` dans tous les cas et l'essai ne
/// pourrait pas distinguer « le service a été filtré » de « aucun service
/// n'était joignable ».
struct ServiceDeBanc;

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

    async fn search(&self, query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: vec![StreamTrack {
                id: "banc-1".into(),
                title: format!("{query} en streaming"),
                artist: format!("{query} Trio"),
                album: Some(format!("{query} Sessions")),
                album_id: Some("banc-album-1".into()),
                duration_ms: 180_000,
                cover_path: None,
                track_number: Some(1),
                disc_number: Some(1),
                explicit: false,
                quality: None,
                isrc: None,
                composer: None,
                artist_id: None,
            }],
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }

    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, TuneError> {
        Err(TuneError::NotFound("banc".into()))
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

// --- socle -------------------------------------------------------------

/// Une base où `MOT` correspond dans les QUATRE familles locales, et un
/// registre où un service authentifié y répond aussi.
async fn banc() -> tune_server::state::AppState {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();

    let tracks = TrackRepo::with_backend(state.backend.clone());
    let mut t = Track::new(format!("{MOT} Leaves"));
    t.file_path = Some(format!("/musique/{MOT}.flac"));
    tracks.create(&t).expect("insert piste");

    AlbumRepo::with_backend(state.backend.clone())
        .create(&Album::new(format!("{MOT} Sessions")))
        .expect("insert album");

    ArtistRepo::with_backend(state.backend.clone())
        .create(&Artist::new(format!("{MOT} Trio")))
        .expect("insert artiste");

    RadioRepo::with_backend(state.backend.clone())
        .create(&RadioStation {
            id: None,
            name: format!("Radio {MOT}"),
            url: "http://example.invalid/stream".into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .expect("insert station");

    state
        .services
        .lock()
        .await
        .register(Box::new(ServiceDeBanc));

    state
}

/// `GET /api/v1/search?q=MOT`, avec le paramètre `sources` tel qu'il est passé
/// ici — `None` veut dire « pas de paramètre du tout », pas « paramètre vide ».
async fn chercher(state: &tune_server::state::AppState, sources: Option<&str>) -> Value {
    let app = tune_server::routes::router(state.clone());
    let mut url = format!("/api/v1/search?q={MOT}&limit=50");
    if let Some(s) = sources {
        url.push_str(&format!("&sources={s}"));
    }
    let reponse = app
        .oneshot(Request::get(&url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    assert_eq!(statut, StatusCode::OK, "GET {url} — corps : {corps}");
    corps
}

/// Longueur d'un tableau du corps, avec un message qui montre le corps entier
/// quand le chemin n'est pas un tableau.
fn combien(corps: &Value, chemin: &[&str]) -> usize {
    let mut cur = corps;
    for cle in chemin {
        cur = cur
            .get(*cle)
            .unwrap_or_else(|| panic!("clé « {cle} » absente de la réponse : {corps}"));
    }
    cur.as_array()
        .unwrap_or_else(|| panic!("{chemin:?} n'est pas un tableau : {cur}"))
        .len()
}

/// Les quatre familles locales, dans l'ordre `artists, albums, tracks, radios`.
fn moitie_locale(corps: &Value) -> [usize; 4] {
    [
        combien(corps, &["local", "artists"]),
        combien(corps, &["local", "albums"]),
        combien(corps, &["local", "tracks"]),
        combien(corps, &["radios"]),
    ]
}

/// Le service factice est-il dans la réponse, et avec quelque chose dedans ?
fn pistes_du_service(corps: &Value) -> usize {
    match corps["services"].get(SERVICE) {
        None => 0,
        Some(bloc) => bloc["tracks"]
            .as_array()
            .unwrap_or_else(|| panic!("services.{SERVICE}.tracks n'est pas un tableau : {bloc}"))
            .len(),
    }
}

/// Le socle tient-il ? Si l'un des deux côtés était vide dès le départ, tous
/// les essais ci-dessous seraient verts contre rien.
async fn verifier_le_socle(state: &tune_server::state::AppState) {
    let tout = chercher(state, None).await;
    assert_eq!(
        moitie_locale(&tout),
        [1, 1, 1, 1],
        "le socle doit poser une correspondance locale dans CHAQUE famille : {tout}"
    );
    assert_eq!(
        pistes_du_service(&tout),
        1,
        "le socle doit poser un service authentifié qui répond : {tout}"
    );
}

// --- 1. le défaut ------------------------------------------------------

#[tokio::test]
async fn la_pilule_d_un_service_ne_rend_plus_la_bibliotheque() {
    let state = banc().await;
    verifier_le_socle(&state).await;

    let corps = chercher(&state, Some(SERVICE)).await;

    assert_eq!(
        moitie_locale(&corps),
        [0, 0, 0, 0],
        "« {SERVICE} » demandé seul : artistes, albums, pistes locaux et radios \
         doivent être vides — la base contient pourtant « {MOT} » dans les quatre. \
         Corps : {corps}"
    );
    assert_eq!(
        pistes_du_service(&corps),
        1,
        "et le bloc du service demandé, lui, doit être là : {corps}"
    );
    assert_eq!(
        corps["local"]["totals"]["tracks"], 0,
        "un total non nul sous zéro ligne ferait afficher un compteur menteur : {corps}"
    );
    assert_eq!(corps["local"]["totals"]["albums"], 0, "{corps}");
    assert_eq!(corps["local"]["totals"]["artists"], 0, "{corps}");
}

// --- 2. l'inverse ------------------------------------------------------

#[tokio::test]
async fn la_pilule_locale_ne_rend_aucun_service() {
    let state = banc().await;
    verifier_le_socle(&state).await;

    let corps = chercher(&state, Some("local")).await;

    assert_eq!(
        moitie_locale(&corps),
        [1, 1, 1, 1],
        "« local » demandé seul : les quatre familles locales doivent être servies : {corps}"
    );
    assert_eq!(
        pistes_du_service(&corps),
        0,
        "et aucun service ne doit paraître : {corps}"
    );
    assert_eq!(
        corps["services"],
        serde_json::json!({}),
        "`services` doit rester un objet, vide : {corps}"
    );
    assert_eq!(corps["local"]["totals"]["tracks"], 1, "{corps}");
}

// --- 3. LE TÉMOIN ------------------------------------------------------

/// « Tous » n'envoie aucun `sources`. C'est le seul cas qui marchait avant
/// #3226, et le seul dont le comportement ne doit PAS bouger. Il reste vert
/// même si le filtrage du local est saboté — c'est précisément son rôle :
/// interdire de faire passer les essais 1 et 2 en vidant le local partout.
#[tokio::test]
async fn le_temoin_sans_sources_rend_les_deux_moities() {
    let state = banc().await;

    let corps = chercher(&state, None).await;

    assert_eq!(
        moitie_locale(&corps),
        [1, 1, 1, 1],
        "sans `sources`, la bibliothèque et les radios partent comme avant : {corps}"
    );
    assert_eq!(
        pistes_du_service(&corps),
        1,
        "sans `sources`, tous les services authentifiés partent aussi : {corps}"
    );
}

// --- 4, 5, 6. le contrat des bords -------------------------------------

#[tokio::test]
async fn local_et_service_ensemble_rendent_les_deux() {
    let state = banc().await;
    verifier_le_socle(&state).await;

    let corps = chercher(&state, Some(&format!("local,{SERVICE}"))).await;

    assert_eq!(
        moitie_locale(&corps),
        [1, 1, 1, 1],
        "la liste est une UNION : `local` y figure, le local sort : {corps}"
    );
    assert_eq!(pistes_du_service(&corps), 1, "{corps}");
}

/// Le serveur acceptait déjà `all` pour les services. Il ne peut pas rendre
/// MOINS que l'absence de paramètre, sans quoi le joker serait un piège.
#[tokio::test]
async fn le_joker_all_vaut_tout() {
    let state = banc().await;
    verifier_le_socle(&state).await;

    let corps = chercher(&state, Some("all")).await;

    assert_eq!(moitie_locale(&corps), [1, 1, 1, 1], "{corps}");
    assert_eq!(pistes_du_service(&corps), 1, "{corps}");
}

/// Une sélection explicite qui ne reconnaît rien ne se replie PAS sur « tout ».
/// C'est déjà ce que la boucle des services faisait d'un nom de service
/// inconnu ; la même règle vaut désormais pour le local.
#[tokio::test]
async fn une_source_inconnue_ne_selectionne_rien() {
    let state = banc().await;
    verifier_le_socle(&state).await;

    for valeur in ["napster", "", "local-machin"] {
        let corps = chercher(&state, Some(valeur)).await;
        assert_eq!(
            moitie_locale(&corps),
            [0, 0, 0, 0],
            "`sources={valeur}` ne désigne aucune source connue : rien de local ne sort. \
             Corps : {corps}"
        );
        assert_eq!(
            pistes_du_service(&corps),
            0,
            "`sources={valeur}` : aucun service non plus. Corps : {corps}"
        );
    }
}

// --- 7. ce que le client déréférence -----------------------------------

/// Le correctif ne doit pas faire DISPARAÎTRE `local`.
///
/// `federatedSearch`, côté `tune-web-client`, écrit
/// `if (result.local) result.local.tracks = mapStreamingTracks(result.local.tracks)`,
/// et `SearchView.svelte` lit `results.local?.albums`, `results.local?.tracks`,
/// `results.local?.artists`. Un `local` absent est absorbé par les `?.` ; un
/// `local` présent mais amputé de `tracks` ne l'est pas. La forme complète de
/// #3189 est donc rendue telle quelle, avec des zéros dedans.
#[tokio::test]
async fn la_forme_lue_par_le_client_reste_entiere_quand_le_local_est_ecarte() {
    let state = banc().await;
    verifier_le_socle(&state).await;

    let corps = chercher(&state, Some(SERVICE)).await;

    assert!(
        corps["local"].is_object(),
        "`local` doit rester un objet, pas disparaître : {corps}"
    );
    for famille in ["artists", "albums", "tracks"] {
        assert!(
            corps["local"][famille].is_array(),
            "`local.{famille}` doit rester un tableau (vide), jamais absent : {corps}"
        );
        assert!(
            corps["local"]["totals"][famille].is_i64(),
            "`local.totals.{famille}` doit rester un entier : {corps}"
        );
        assert_eq!(
            corps["local"]["has_more"][famille],
            Value::Bool(false),
            "rien n'a été coupé puisque rien n'a été cherché : {corps}"
        );
        assert_eq!(
            corps["local"]["totals_capped"][famille],
            Value::Bool(false),
            "{corps}"
        );
    }
    assert_eq!(
        corps["local"]["totals"]["tracks_via_metadata"], 0,
        "{corps}"
    );
    assert_eq!(
        corps["local"]["limit"], 50,
        "`limit` reste renvoyé : {corps}"
    );
    assert_eq!(
        corps["local"]["offset"], 0,
        "`offset` reste renvoyé : {corps}"
    );
    assert!(
        corps["radios"].is_array(),
        "`radios` reste un tableau au premier niveau : {corps}"
    );
    assert!(
        corps["services"].is_object(),
        "`services` reste un objet au premier niveau : {corps}"
    );
}
