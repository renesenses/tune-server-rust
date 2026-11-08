//! #8129 — les fiches Qobuz (artiste, présentation d'album) rendent
//! `bio_segments` : liens vers les fiches Qobuz, liens externes des sources,
//! noms de la bibliothèque. `bio` / `description` restent du texte.

use super::*;
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AuthStatus, HtmlEditorial, SearchResults, StreamAlbum, StreamAlbumDescription, StreamArtist,
    StreamPlaylist, StreamTrack, StreamUrl,
};

const BIO: &str = r#"<p>Miles Davis joue avec <a href="https://www.qobuz.com/fr-fr/interpreter/john-coltrane/16422">John Coltrane</a> sur <a href="https://open.qobuz.com/album/0886443853839"><em>Kind of Blue</em></a>. Bill Evans y tient le piano &amp; plus. Voir <a href="https://fr.wikipedia.org/wiki/Miles_Davis">Wikipédia</a> ou <a href="javascript:alert(1)">ceci</a>.</p>"#;

struct FauxQobuz {
    nom: String,
}

fn hors_sujet<T>() -> Result<T, TuneError> {
    Err(TuneError::Streaming("hors sujet".into()))
}

#[async_trait::async_trait]
impl StreamingService for FauxQobuz {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        &self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _e: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        hors_sujet()
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        hors_sujet()
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        hors_sujet()
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        hors_sujet()
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        hors_sujet()
    }
    async fn get_artist(&self, a: &str) -> Result<StreamArtist, TuneError> {
        Ok(self.get_artist_avec_bio_html(a).await?.0)
    }
    async fn get_artist_avec_bio_html(
        &self,
        a: &str,
    ) -> Result<(StreamArtist, Option<String>), TuneError> {
        Ok((
            StreamArtist {
                id: a.to_string(),
                name: "Miles Davis".into(),
                image_path: None,
                bio: Some("texte brut d'origine".into()),
            },
            Some(BIO.to_string()),
        ))
    }
    async fn get_album_description(
        &self,
        _a: &str,
    ) -> Result<Option<StreamAlbumDescription>, TuneError> {
        Ok(Some(StreamAlbumDescription {
            description: "texte brut d'origine".into(),
            language: Some("fr".into()),
        }))
    }
    async fn presentation_album_html(&self, _a: &str) -> Result<Option<HtmlEditorial>, TuneError> {
        Ok(Some(HtmlEditorial {
            html: BIO.to_string(),
            noms: vec!["Kind of Blue".into(), "Miles Davis".into()],
        }))
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        hors_sujet()
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        hors_sujet()
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        hors_sujet()
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        hors_sujet()
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        hors_sujet()
    }
}

async fn appel(nom: &str, chemin: &str) -> Value {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (3, 'Bill Evans'), (1, 'Miles Davis')",
            &[],
        )
        .expect("artistes de la bibliotheque");
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(FauxQobuz {
        nom: nom.to_string(),
    }));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    let app = Router::new().nest("/api/v1/streaming", router().with_state(etat));
    let r = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/v1/streaming/{nom}/{chemin}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&octets).unwrap()
}

fn segment<'a>(v: &'a Value, texte: &str) -> &'a Value {
    v["bio_segments"]
        .as_array()
        .unwrap_or_else(|| panic!("bio_segments absents : {v}"))
        .iter()
        .find(|s| s["texte"] == texte)
        .unwrap_or_else(|| panic!("segment {texte:?} absent : {v}"))
}

fn verifier(v: &Value, champ: &str) {
    let texte = v[champ].as_str().unwrap();
    let recompose: String = v["bio_segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["texte"].as_str().unwrap())
        .collect();
    assert_eq!(recompose, texte, "les segments recomposent `{champ}`");
    assert!(
        !texte.contains('<') && texte.contains("piano & plus"),
        "{texte:?}"
    );
    assert_eq!(
        segment(v, "John Coltrane")["lien_service"],
        json!({"service": "qobuz", "type": "artiste", "id": "16422"})
    );
    assert_eq!(
        segment(v, "Wikipédia")["url"],
        "https://fr.wikipedia.org/wiki/Miles_Davis"
    );
    assert_eq!(
        segment(v, "Bill Evans")["lien"],
        json!({"type": "artiste", "id": 3})
    );
    assert!(!v["bio_segments"].to_string().contains("javascript"), "{v}");
}

#[tokio::test]
async fn la_fiche_artiste_qobuz_porte_ses_liens_8129() {
    let v = appel("qobuz", "artists/1234").await;
    verifier(&v, "bio");
    assert_eq!(
        segment(&v, "Kind of Blue")["lien_service"],
        json!({"service": "qobuz", "type": "album", "id": "0886443853839"})
    );
    // L'artiste de la page n'est pas lié vers son homonyme local.
    assert!(
        v["bio_segments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| !(s["texte"].as_str().unwrap().contains("Miles Davis")
                && s.get("lien").is_some())),
        "{v}"
    );
}

#[tokio::test]
async fn la_presentation_d_album_qobuz_porte_ses_liens_8129() {
    let v = appel("qobuz", "albums/0886443853839/description").await;
    verifier(&v, "description");
    assert_eq!(v["language"], "fr");
    // L'album de la page : son propre lien reste du texte.
    assert!(
        segment(&v, "Kind of Blue").get("lien_service").is_none(),
        "{v}"
    );
}

#[tokio::test]
async fn un_autre_service_ne_change_pas_de_forme_8129() {
    let v = appel("autre", "artists/1234").await;
    assert!(v.get("bio_segments").is_none(), "{v}");
    assert_eq!(v["bio"], "texte brut d'origine");
}
