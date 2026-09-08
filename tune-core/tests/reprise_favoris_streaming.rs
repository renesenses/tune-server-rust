//! Témoin de la reprise des favoris posés CHEZ le service (#3419).
//!
//! Le défaut, mesuré sur le .18 le 05/09/2026 : 14 pistes et 3 albums en
//! favori chez Qobuz, et `streaming_favorites` — la table que lisent l'écran
//! Favoris ET les règles de collection intelligente — n'en contenait aucun.
//! Deux magasins, aucune passerelle. Ce que Bertrand voyait : « j'ai 3 pistes
//! Qobuz en favori !! » et une règle « Favori · est · Piste » qui rend 0.
//!
//! Ce que ce fichier fixe :
//!   1. ce que le service dit entre dans la table, les trois types ;
//!   2. le passage est idempotent — une seconde reprise n'ajoute rien, et ne
//!      compte pas non plus ce qu'elle n'a pas fait ;
//!   3. un favori posé DANS Tune sur un objet que le service ne connaît pas
//!      survit à la reprise (elle ajoute, elle ne réconcilie pas) ;
//!   4. l'échec d'un type n'emporte pas les deux autres.

use std::sync::Arc;

use tune_core::TuneError;
use tune_core::db::backend::DbBackend;
use tune_core::db::migrations::run_migrations;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::streaming_favorites_repo::StreamingFavoritesRepo;
use tune_core::streaming::favorites_import::reprendre_les_favoris_du_service;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const SERVICE: &str = "qobuz";
const PROFIL: i64 = 1;

fn non_prevu(quoi: &str) -> TuneError {
    TuneError::Streaming(format!("le service factice ne sert pas : {quoi}"))
}

fn piste(id: &str, titre: &str, artiste: &str, album: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: Some(album.into()),
        album_id: None,
        duration_ms: 180_000,
        cover_path: Some(format!("https://cover/{id}.jpg")),
        track_number: None,
        disc_number: None,
        explicit: false,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

fn album(id: &str, titre: &str, artiste: &str) -> StreamAlbum {
    StreamAlbum {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        artist_id: None,
        cover_path: Some(format!("https://cover/{id}.jpg")),
        year: None,
        track_count: 10,
        quality: None,
    }
}

/// Un service dont on choisit exactement ce qu'il rend, y compris l'échec.
struct ServiceFactice {
    pistes: Result<Vec<StreamTrack>, &'static str>,
    albums: Result<Vec<StreamAlbum>, &'static str>,
    artistes: Result<Vec<StreamArtist>, &'static str>,
}

impl Default for ServiceFactice {
    fn default() -> Self {
        Self {
            pistes: Ok(vec![]),
            albums: Ok(vec![]),
            artistes: Ok(vec![]),
        }
    }
}

#[async_trait::async_trait]
impl StreamingService for ServiceFactice {
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
    async fn authenticate(
        &mut self,
        _credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        Err(non_prevu("authenticate"))
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Err(non_prevu("search"))
    }
    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, TuneError> {
        Err(non_prevu("get_track"))
    }
    async fn get_track_url(
        &self,
        _track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err(non_prevu("get_track_url"))
    }
    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_prevu("get_album"))
    }
    async fn get_album_tracks(&self, _album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_prevu("get_album_tracks"))
    }
    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_prevu("get_artist"))
    }
    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_prevu("get_playlist"))
    }
    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_prevu("get_playlist_tracks"))
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        self.pistes
            .clone()
            .map_err(|e| TuneError::Streaming(e.to_string()))
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        self.albums
            .clone()
            .map_err(|e| TuneError::Streaming(e.to_string()))
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        self.artistes
            .clone()
            .map_err(|e| TuneError::Streaming(e.to_string()))
    }
}

fn base() -> Arc<dyn DbBackend> {
    let sqlite = SqliteDb::open_in_memory().expect("base mémoire");
    sqlite.init_schema().expect("schéma");
    run_migrations(&sqlite).expect("migrations");
    Arc::new(sqlite)
}

/// Le défaut, dans le sens où on le constate : ce que le service dit doit
/// entrer dans la table de Tune.
///
/// L'assertion est prise DANS la table (`StreamingFavoritesRepo::list`), pas
/// sur le compte rendu du passage : c'est cette table que joignent
/// `track_favorites_sub` et l'écran Favoris. Un compte rendu vert sur une
/// table vide serait le faux vert exact de ce ticket.
#[tokio::test]
async fn les_favoris_du_service_entrent_dans_la_table() {
    let db = base();
    let svc = ServiceFactice {
        pistes: Ok(vec![
            piste("t1", "Midnight Junction", "Aaron Zed", "Bis"),
            piste("t2", "Volume 10", "Éric Zimmer", "Anthologie"),
        ]),
        albums: Ok(vec![album("a1", "Live with the Orchestra", "Erik Satie")]),
        artistes: Ok(vec![StreamArtist {
            id: "ar1".into(),
            name: "Erik Satie".into(),
            image_path: None,
            bio: None,
        }]),
    };

    let stats = reprendre_les_favoris_du_service(&svc, PROFIL, &db).await;
    assert_eq!(stats.lus, 4, "{stats:?}");
    assert_eq!(stats.ajoutes, 4, "{stats:?}");
    assert_eq!(stats.echecs, 0, "{stats:?}");

    let repo = StreamingFavoritesRepo::with_backend(db.clone());
    let pistes = repo.list(PROFIL, Some("track")).expect("lecture");
    assert_eq!(pistes.len(), 2, "{pistes:?}");
    let une = pistes
        .iter()
        .find(|f| f.service_id == "t1")
        .expect("la piste reprise doit être là");
    assert_eq!(une.service, SERVICE);
    assert_eq!(une.title.as_deref(), Some("Midnight Junction"));
    assert_eq!(une.artist.as_deref(), Some("Aaron Zed"));
    assert_eq!(une.album.as_deref(), Some("Bis"));

    // Titre + artiste sont exactement ce que joint `track_favorites_sub`
    // (`lower(trim(...))` des deux) : sans eux, la reprise remplirait la table
    // sans rien rendre aux règles.
    assert!(
        pistes
            .iter()
            .all(|f| f.title.is_some() && f.artist.is_some()),
        "le rapprochement des règles se fait sur titre + artiste : {pistes:?}"
    );

    assert_eq!(repo.list(PROFIL, Some("album")).unwrap().len(), 1);
    assert_eq!(repo.list(PROFIL, Some("artist")).unwrap().len(), 1);
}

/// Repasser ne double rien, et le dit.
///
/// La reprise tourne au démarrage : si elle n'était pas idempotente, chaque
/// redémarrage gonflerait la table (ou, à tout le moins, mentirait dans le
/// journal en annonçant des ajouts qu'elle n'a pas faits).
#[tokio::test]
async fn une_seconde_reprise_n_ajoute_rien() {
    let db = base();
    let svc = ServiceFactice {
        pistes: Ok(vec![piste("t1", "Midnight Junction", "Aaron Zed", "Bis")]),
        ..Default::default()
    };

    let premier = reprendre_les_favoris_du_service(&svc, PROFIL, &db).await;
    assert_eq!(premier.ajoutes, 1, "{premier:?}");

    let second = reprendre_les_favoris_du_service(&svc, PROFIL, &db).await;
    assert_eq!(second.lus, 1, "{second:?}");
    assert_eq!(second.ajoutes, 0, "{second:?}");
    assert_eq!(second.deja_presents, 1, "{second:?}");

    let repo = StreamingFavoritesRepo::with_backend(db.clone());
    assert_eq!(repo.list(PROFIL, Some("track")).unwrap().len(), 1);
}

/// Le témoin de ce que la reprise NE FAIT PAS : elle n'efface rien.
///
/// Un favori posé dans Tune sur un objet que le service ne rend pas — parce
/// qu'il a été retiré chez lui, ou parce qu'il n'a jamais été connu de lui —
/// doit survivre. C'est le point laissé à l'arbitrage : une réconciliation
/// « ce que le service dit fait foi » détruirait cette ligne, et la table ne
/// distingue pas encore les deux origines.
#[tokio::test]
async fn la_reprise_ne_supprime_pas_un_favori_pose_dans_tune() {
    let db = base();
    let repo = StreamingFavoritesRepo::with_backend(db.clone());
    repo.add(
        PROFIL,
        "track",
        SERVICE,
        "pose-dans-tune",
        Some("Un titre à nous"),
        Some("Un artiste"),
        None,
        None,
    )
    .expect("ajout");

    let svc = ServiceFactice {
        pistes: Ok(vec![piste("t1", "Midnight Junction", "Aaron Zed", "Bis")]),
        ..Default::default()
    };
    reprendre_les_favoris_du_service(&svc, PROFIL, &db).await;

    let pistes = repo.list(PROFIL, Some("track")).expect("lecture");
    let ids: Vec<&str> = pistes.iter().map(|f| f.service_id.as_str()).collect();
    assert!(
        ids.contains(&"pose-dans-tune"),
        "un favori posé dans Tune ne doit pas disparaître ({ids:?})"
    );
    assert!(ids.contains(&"t1"), "{ids:?}");
}

/// Un type illisible ne fait pas perdre les autres.
///
/// Les trois lectures sont indépendantes chez le service ; les enchaîner dans
/// un seul `?` aurait fait qu'une panne sur les pistes emporte silencieusement
/// les albums et les artistes — et le passage aurait rendu « 0 » sans dire
/// qu'il avait échoué.
#[tokio::test]
async fn l_echec_d_un_type_n_emporte_pas_les_autres() {
    let db = base();
    let svc = ServiceFactice {
        pistes: Err("HTTP 502"),
        albums: Ok(vec![album("a1", "Live with the Orchestra", "Erik Satie")]),
        artistes: Ok(vec![]),
    };

    let stats = reprendre_les_favoris_du_service(&svc, PROFIL, &db).await;
    assert_eq!(stats.echecs, 1, "{stats:?}");
    assert_eq!(stats.ajoutes, 1, "{stats:?}");

    let repo = StreamingFavoritesRepo::with_backend(db.clone());
    assert_eq!(repo.list(PROFIL, Some("album")).unwrap().len(), 1);
    assert_eq!(repo.list(PROFIL, Some("track")).unwrap().len(), 0);
}
