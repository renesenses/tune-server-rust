//! #7778, point 1 — fil 2637 (rc10) : la reprise d'un album Bandcamp depuis
//! « Reprendre l'écoute » échoue, journal `not_found error=dernier titre
//! absent du conteneur`.
//!
//! L'identifiant d'un titre Bandcamp est son URL de flux signée
//! (`plugins/tune-bandcamp/src/service.rs`) ; `ts`, `t` et `token` changent à
//! chaque lecture de la page (#4577). La reprise relit l'album et cherchait le
//! dernier titre par égalité stricte de cette URL : il n'était jamais trouvé.
//!
//! Le banc : un faux Bandcamp dont chaque relecture de l'album rend les mêmes
//! titres avec une signature nouvelle, et la vraie mémorisation de la reprise
//! (`resumes::started` sur la file d'une zone).
//!
//! Contre-épreuve : remplacer `meme_piste_bandcamp(a, ai, bi)` par `false`
//! dans `complete_single_container` fait tomber
//! `la_reprise_d_un_album_bandcamp_retrouve_son_titre_malgre_une_signature_nouvelle_7778`.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::error::TuneError;
use tune_core::orchestrator::ContexteEcoute;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const ALBUM: &str = "https://artiste-7778.bandcamp.com/album/reprise";
const PISTES: [&str; 3] = ["1111111", "2222222", "3333333"];

/// L'URL de flux d'une piste, signée à l'instant `ts`.
fn url_signee(piste: &str, ts: u64) -> String {
    format!("https://t4.bcbits.com/stream/abc/mp3-128/{piste}?p=0&ts={ts}&t=t{ts}&token={ts}_jeton")
}

/// Chaque lecture de la page rend une signature nouvelle, comme Bandcamp.
struct FauxBandcamp {
    lectures: AtomicU64,
}

fn non_servi() -> TuneError {
    TuneError::Streaming("faux Bandcamp : rien d'autre n'est servi".into())
}

#[async_trait::async_trait]
impl StreamingService for FauxBandcamp {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "bandcamp"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Err(non_servi())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err(non_servi())
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(non_servi())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(non_servi())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_servi())
    }
    async fn get_album_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        assert_eq!(id, ALBUM, "la reprise doit relire l'album mémorisé");
        let ts = 1_800_000_000 + self.lectures.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(PISTES
            .iter()
            .enumerate()
            .map(|(i, piste)| StreamTrack {
                id: url_signee(piste, ts),
                title: format!("Titre {}", i + 1),
                artist: "Artiste 7778".into(),
                album: Some("Reprise".into()),
                album_id: Some(ALBUM.into()),
                duration_ms: 200_000,
                cover_path: None,
                track_number: Some(i as u32 + 1),
                disc_number: None,
                explicit: false,
                disponible: None,
                quality: None,
                isrc: None,
                composer: None,
                artist_id: None,
            })
            .collect())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
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

fn source_id(item: &QueueInput) -> &str {
    match item {
        QueueInput::Streaming { source_id, .. } => source_id,
        autre => panic!("titre Bandcamp attendu, reçu {autre:?}"),
    }
}

#[tokio::test]
async fn la_reprise_d_un_album_bandcamp_retrouve_son_titre_malgre_une_signature_nouvelle_7778() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.services.lock().await.register(Box::new(FauxBandcamp {
        lectures: AtomicU64::new(0),
    }));
    let db = &state.backend;
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .create("Zone 7778", None, None)
        .unwrap();
    // La file de l'écoute d'origine : le deuxième titre seul, signé la veille.
    let ancienne = url_signee(PISTES[1], 1_799_900_000);
    let q = PlayQueueRepo::with_backend(db.clone());
    q.append(
        zone,
        &[QueueInput::Streaming {
            source: "bandcamp".into(),
            source_id: ancienne.clone(),
            title: "Titre 2".into(),
            artist: "Artiste 7778".into(),
            album: Some("Reprise".into()),
            cover_url: None,
            duration_ms: 200_000,
            track_number: Some(2),
            disc_number: None,
            album_ref: Some(ALBUM.into()),
            artist_ref: None,
        }],
    )
    .unwrap();
    q.set_current_pos(zone, 0).unwrap();
    resumes::started(
        db,
        zone,
        &ContexteEcoute {
            nature: Some("album"),
            id: Some(ALBUM),
            rang: Some(1),
            service: Some("bandcamp"),
            titre: None,
            pochette: None,
        },
        "bandcamp",
        Some(&ancienne),
        "Titre 2",
        Some("Reprise"),
        None,
    )
    .unwrap();
    let mut r = resumes::recent(db, None, 10).unwrap().remove(0);
    assert_eq!(r.source, "bandcamp");

    let rendu = complete_single_container(&state, &mut r).await;
    assert!(
        rendu.is_ok(),
        "#7778 : la reprise d'un album Bandcamp ne doit pas répondre « dernier titre absent du \
         conteneur » quand l'album relu porte une signature nouvelle"
    );
    assert_eq!(
        r.items.len(),
        3,
        "la reprise doit reconstruire tout l'album"
    );
    assert_eq!(
        r.position, 1,
        "la reprise doit repartir du titre écouté, pas d'un autre titre de l'album"
    );
    let reprise = source_id(&r.items[r.position]);
    assert!(
        reprise.contains("/mp3-128/2222222?") && reprise != ancienne,
        "le titre repris doit garder l'URL fraîche de l'album relu, pas l'ancienne signature : {reprise}"
    );
}

#[test]
fn deux_pistes_bandcamp_differentes_ne_se_confondent_pas_7778() {
    let a = url_signee(PISTES[0], 1);
    let b = url_signee(PISTES[1], 1);
    assert!(!meme_piste_bandcamp("bandcamp", &a, &b));
    assert!(meme_piste_bandcamp(
        "bandcamp",
        &a,
        &url_signee(PISTES[0], 2)
    ));
    // Un autre service garde l'égalité stricte de ses identifiants.
    assert!(!meme_piste_bandcamp("qobuz", &a, &url_signee(PISTES[0], 2)));
}
