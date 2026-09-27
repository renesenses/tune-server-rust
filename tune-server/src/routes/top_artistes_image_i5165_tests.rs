//! #5165 — « Top Artistes » sans photo pour un artiste écouté SEULEMENT sur
//! un service.
//!
//! Alex Campbell, 26/09/2026, accueil de l'app iPad : aucun des huit « Top
//! Artistes » n'avait de photo, dont plusieurs écoutés sur Qobuz seulement.
//! `GET /library/history/top-artists` rendait `artist_id: null` pour un
//! artiste absent de la table `artists`, et jamais d'`image_path` — champ que
//! l'app lit pourtant (`TopArtistEntry.imagePath`, clé `image_path`).
//!
//! Un service simulé, sans réseau : la piste `t-neil` nomme « Neil Young » et
//! porte son identifiant d'artiste chez le service, dont la fiche porte une
//! image. Chaque témoin prend un nom d'artiste qui lui est propre : la mémoire
//! des images est commune au processus.

use axum::Json;
use axum::extract::{Query, State};
use serde_json::{Value, json};
use tune_core::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

use super::{HistoryParams, top_artists};
use crate::state::AppState;

const IMAGE_SERVICE: &str = "https://static.qobuz.com/images/artists/neil-young.jpg";

/// Un service simulé : chaque piste `t-<x>` nomme l'artiste `<nom>` et porte
/// l'identifiant `a-<x>` ; seul `a-neil` a une image.
struct ServiceTemoin {
    nom: &'static str,
    actif: bool,
    /// (identifiant de piste, nom d'artiste NOMMÉ par la piste, id d'artiste)
    pistes: Vec<(&'static str, &'static str, &'static str)>,
}

fn piste(id: &str, artiste: &str, artiste_id: &str) -> StreamTrack {
    serde_json::from_value(json!({
        "id": id,
        "title": "Harvest Moon",
        "artist": artiste,
        "duration_ms": 300_000,
        "explicit": false,
        "artist_id": artiste_id,
    }))
    .unwrap()
}

#[async_trait::async_trait]
impl StreamingService for ServiceTemoin {
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
        self.actif
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.actif = enabled;
    }
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
        Err("hors sujet".into())
    }
    async fn get_track(&self, t: &str) -> Result<StreamTrack, TuneError> {
        self.pistes
            .iter()
            .find(|(id, _, _)| *id == t)
            .map(|(id, artiste, artiste_id)| piste(id, artiste, artiste_id))
            .ok_or_else(|| "piste inconnue".into())
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
    async fn get_artist(&self, a: &str) -> Result<StreamArtist, TuneError> {
        let (_, nom, _) = self
            .pistes
            .iter()
            .find(|(_, _, id)| *id == a)
            .ok_or_else(|| TuneError::from("artiste inconnu"))?;
        Ok(StreamArtist {
            id: a.to_string(),
            name: nom.to_string(),
            image_path: (a == "a-neil").then(|| IMAGE_SERVICE.to_string()),
            bio: None,
        })
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

async fn serveur(pistes: Vec<(&'static str, &'static str, &'static str)>, actif: bool) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .services
        .lock()
        .await
        .register(Box::new(ServiceTemoin {
            nom: "qobuz",
            actif,
            pistes,
        }));
    state
}

fn ecoute(state: &AppState, artiste: &str, source: &str, source_id: &str) {
    state
        .backend
        .execute(
            &format!(
                "INSERT INTO listen_history (title, artist_name, source, source_id, duration_ms) \
                 VALUES ('Harvest Moon', '{artiste}', '{source}', '{source_id}', 300000)"
            ),
            &[],
        )
        .unwrap();
}

async fn top(state: AppState) -> Vec<Value> {
    let Json(corps) = top_artists(
        State(state),
        Query(HistoryParams {
            limit: Some(20),
            offset: None,
            period: None,
        }),
    )
    .await;
    corps.as_array().expect("un tableau").clone()
}

fn ligne<'a>(top: &'a [Value], nom: &str) -> &'a Value {
    top.iter()
        .find(|l| l["name"] == nom)
        .unwrap_or_else(|| panic!("{nom} absent du classement : {top:?}"))
}

/// ⭐ La capture d'Alex : un artiste joué sur Qobuz seulement, sans fiche de
/// bibliothèque, reçoit l'image de sa fiche Qobuz.
#[tokio::test]
async fn un_artiste_de_service_seulement_recoit_l_image_du_service() {
    let state = serveur(vec![("t-neil", "Neil Young 5165", "a-neil")], true).await;
    ecoute(&state, "Neil Young 5165", "qobuz", "t-neil");
    ecoute(&state, "Neil Young 5165", "qobuz", "t-neil");

    let top = top(state).await;
    let neil = ligne(&top, "Neil Young 5165");
    assert_eq!(
        neil["artist_id"],
        Value::Null,
        "pas de fiche de bibliothèque"
    );
    assert_eq!(
        neil["image_path"], IMAGE_SERVICE,
        "l'artiste de service n'a pas d'image dans Top Artistes (#5165) : {neil}"
    );
    // Le contrat d'avant tient : les champs existants ne bougent pas.
    assert_eq!(neil["plays"], 2);
    assert_eq!(neil["artist_name"], "Neil Young 5165");
}

/// L'artiste de BIBLIOTHÈQUE garde l'image de sa fiche, sans que le service
/// soit interrogé à sa place.
#[tokio::test]
async fn l_artiste_de_bibliotheque_rend_l_image_de_sa_fiche() {
    let state = serveur(vec![("t-neil", "Roger Waters 5165", "a-neil")], true).await;
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name, image_path) VALUES (77, 'Roger Waters 5165', 'abc123')",
            &[],
        )
        .unwrap();
    ecoute(&state, "Roger Waters 5165", "qobuz", "t-neil");

    let top = top(state).await;
    let roger = ligne(&top, "Roger Waters 5165");
    assert_eq!(roger["artist_id"], 77);
    assert_eq!(roger["image_path"], "abc123");
}

/// Rien n'est deviné : une piste qui nomme un AUTRE artiste que celui de
/// l'historique ne prête pas l'image de ce dernier.
#[tokio::test]
async fn une_piste_qui_nomme_un_autre_artiste_ne_donne_aucune_image() {
    let state = serveur(vec![("t-neil", "Crazy Horse", "a-neil")], true).await;
    ecoute(&state, "Muddy Waters 5165", "qobuz", "t-neil");

    let top = top(state).await;
    let muddy = ligne(&top, "Muddy Waters 5165");
    assert_eq!(muddy["image_path"], Value::Null, "{muddy}");
}

/// Un service désactivé n'est pas interrogé (#5103) ; une écoute locale ne
/// sert jamais d'ancre.
#[tokio::test]
async fn un_service_desactive_ou_une_ecoute_locale_ne_donnent_aucune_image() {
    let state = serveur(vec![("t-neil", "Daine 5165", "a-neil")], false).await;
    ecoute(&state, "Daine 5165", "qobuz", "t-neil");
    ecoute(&state, "NEU! 5165", "local", "t-neil");

    let top = top(state).await;
    assert_eq!(ligne(&top, "Daine 5165")["image_path"], Value::Null);
    assert_eq!(ligne(&top, "NEU! 5165")["image_path"], Value::Null);
}
