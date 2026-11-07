//! #8088 — témoins de la garde des contenus explicites à la lecture : une
//! piste locale marquée par sa balise et une piste de service marquée par le
//! service sont refusées pour un profil dont la politique les masque ; une
//! piste sans marquage passe ; un profil sans la case ne change rien.
//!
//! Services simulés (aucun appel réel), aucune sortie : la garde est jouée
//! avant le choix de la sortie, et seule sa sentinelle est examinée.
use std::sync::Arc;

use super::contenu_explicite_8088::sentinelle_explicite;
use super::{PlayRequest, PlaybackOrchestrator};
use crate::TuneError;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::politique_parentale::{CODE_REFUS_EXPLICITE, PolitiqueParentale, poser};
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;
const ZONE: i64 = 80881;
const PISTE_EXPLICITE: i64 = 1;
const PISTE_PROPRE: i64 = 2;
const PISTE_SANS_BALISE: i64 = 3;

/// Un service simulé : `get_track` rend la piste avec le marquage voulu ;
/// l'identifiant `muet` simule un service qui ne répond pas.
struct ServiceSimule {
    nom: &'static str,
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
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        if id == "muet" {
            return Err("service injoignable".into());
        }
        Ok(StreamTrack {
            id: id.to_string(),
            title: "Titre".into(),
            artist: "Artiste".into(),
            album: None,
            album_id: None,
            duration_ms: 180_000,
            cover_path: None,
            track_number: Some(1),
            disc_number: None,
            explicit: id.starts_with("explicite"),
            quality: None,
            isrc: None,
            disponible: None,
            composer: None,
            artist_id: None,
        })
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
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

/// Un orchestrateur sur une base migrée, trois pistes locales (explicite,
/// propre, sans balise) et trois services simulés.
async fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    for (id, marque) in [
        (PISTE_EXPLICITE, Some(1i64)),
        (PISTE_PROPRE, Some(0)),
        (PISTE_SANS_BALISE, None),
    ] {
        db.execute(
            "INSERT INTO tracks (id, title, file_path, explicit) VALUES (?, ?, ?, ?)",
            &[
                &id as &dyn ToSqlValue,
                &format!("Piste {id}"),
                &format!("/m/{id}.flac"),
                &marque,
            ],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO zones (id, name, output_type, output_device_id) \
         VALUES (?, 'Zone 8088', 'local', 'local:banc-8088')",
        &[&ZONE as &dyn ToSqlValue],
    )
    .unwrap();
    let mut services = crate::streaming::registry::ServiceRegistry::new();
    for nom in ["qobuz", "tidal", "spotify"] {
        services.register(Box::new(ServiceSimule { nom }));
    }
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(services)),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.playback.set_session_profile(ZONE, Some(ENFANT)).await;
    orch
}

fn masquer(orch: &PlaybackOrchestrator, masquer_explicite: bool) {
    poser(
        &orch.db,
        ENFANT,
        ADMIN,
        0,
        &PolitiqueParentale {
            masquer_explicite,
            ..Default::default()
        },
    )
    .unwrap();
}

fn locale(id: i64) -> PlayRequest {
    PlayRequest {
        zone_id: ZONE,
        track_id: Some(id),
        ..Default::default()
    }
}

fn service(nom: &str, id: &str) -> PlayRequest {
    PlayRequest {
        zone_id: ZONE,
        source: Some(nom.into()),
        source_id: Some(id.into()),
        title: Some("Titre".into()),
        ..Default::default()
    }
}

/// Les demandes marquées explicites, et celles qui ne le sont pas.
fn demandes() -> (Vec<PlayRequest>, Vec<PlayRequest>) {
    (
        vec![
            locale(PISTE_EXPLICITE),
            service("qobuz", "explicite-1"),
            service("tidal", "explicite-2"),
            service("spotify", "explicite-3"),
        ],
        vec![
            locale(PISTE_PROPRE),
            locale(PISTE_SANS_BALISE),
            service("qobuz", "propre-1"),
            service("tidal", "propre-2"),
            service("spotify", "propre-3"),
            // Un service qui ne répond pas ne dit rien : pas de marquage.
            service("qobuz", "muet"),
        ],
    )
}

#[tokio::test]
async fn la_garde_refuse_l_explicite_et_laisse_le_reste_8088() {
    let orch = orchestrateur().await;
    masquer(&orch, true);
    assert!(orch.masque_l_explicite(ZONE).await);
    let (explicites, autres) = demandes();
    for d in &explicites {
        let e = orch
            .verifier_le_contenu_explicite(d)
            .await
            .expect_err(&format!(
                "{d:?} : marquée explicite, elle doit être refusée"
            ));
        assert!(e.contains(CODE_REFUS_EXPLICITE), "{e}");
    }
    for d in &autres {
        assert_eq!(
            orch.verifier_le_contenu_explicite(d).await,
            Ok(()),
            "{d:?} : sans marquage, elle n'est pas explicite"
        );
    }
}

/// `play()` lui-même refuse, au point d'étranglement de toutes les entrées.
#[tokio::test]
async fn play_refuse_l_explicite_au_point_d_entree_8088() {
    let orch = orchestrateur().await;
    masquer(&orch, true);
    let (explicites, autres) = demandes();
    for d in explicites {
        let e = orch
            .play(d.clone())
            .await
            .err()
            .unwrap_or_else(|| panic!("{d:?} : la lecture devait être refusée"));
        assert_eq!(e, sentinelle_explicite(), "{d:?}");
    }
    for d in autres {
        // Sans sortie, la lecture échoue plus loin : seule compte l'absence
        // du refus parental.
        if let Err(e) = orch.play(d.clone()).await {
            assert!(
                !e.contains(CODE_REFUS_EXPLICITE),
                "{d:?} : refusée à tort : {e}"
            );
        }
    }
}

/// Sans la case, ou sans politique, rien ne change.
#[tokio::test]
async fn sans_la_case_rien_ne_change_8088() {
    let orch = orchestrateur().await;
    let (explicites, _) = demandes();
    assert!(!orch.masque_l_explicite(ZONE).await, "sans politique");
    for d in &explicites {
        assert_eq!(orch.verifier_le_contenu_explicite(d).await, Ok(()), "{d:?}");
    }
    masquer(&orch, false);
    assert!(!orch.masque_l_explicite(ZONE).await, "case décochée");
    for d in &explicites {
        assert_eq!(orch.verifier_le_contenu_explicite(d).await, Ok(()), "{d:?}");
    }
}

/// Un autre profil sur la même base : la politique de l'enfant ne le touche
/// pas.
#[tokio::test]
async fn la_politique_ne_vaut_que_pour_son_profil_8088() {
    let orch = orchestrateur().await;
    masquer(&orch, true);
    orch.playback.set_session_profile(ZONE, Some(ADMIN)).await;
    assert!(!orch.masque_l_explicite(ZONE).await);
    assert_eq!(
        orch.verifier_le_contenu_explicite(&locale(PISTE_EXPLICITE))
            .await,
        Ok(())
    );
}

/// En fin de piste et sur « Suivant », le refus est propre à la piste : la
/// file passe à la suivante au lieu d'épuiser le budget des pannes ; et
/// l'auditeur lit la vraie raison.
#[test]
fn le_refus_est_propre_a_la_piste_et_se_dit_8088() {
    let e = sentinelle_explicite();
    assert!(crate::poller::refus_propre_a_la_piste(&e));
    assert!(crate::poller::refus_propre_a_la_piste(&format!(
        "play failed: {e}"
    )));
    let m = PlaybackOrchestrator::message_piste_non_demarree("suivante", &e);
    assert!(m.contains("explicite"), "{m}");
    assert!(m.contains("contrôle parental"), "{m}");
}
