//! #8066 — un FAUX service (Qobuz, TIDAL…) pour éprouver les routes de
//! remplacement sans réseau ni compte réel. N'existe qu'en test ou sous la
//! feature `services-factices` (cibles de test de `tune-server` et
//! `tune-streaming-http`).
//!
//! * `catalogue` : ce que la recherche rend et ce que `get_track` connaît ;
//! * `playlist` : la playlist du service, que `replace_playlist_track`
//!   réécrit au rang demandé ;
//! * `modifiable` : la réponse de `playlist_modifiable` ;
//! * `ecritures` : le nombre d'écritures reçues (un refus doit en laisser 0).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::TuneError;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

pub struct ServiceFactice {
    nom: String,
    pub catalogue: Vec<StreamTrack>,
    pub playlist: Arc<Mutex<Vec<StreamTrack>>>,
    pub modifiable: Option<bool>,
    pub ecritures: Arc<AtomicUsize>,
}

impl ServiceFactice {
    pub fn new(nom: &str, catalogue: Vec<StreamTrack>, playlist: Vec<StreamTrack>) -> Self {
        Self {
            nom: nom.into(),
            catalogue,
            playlist: Arc::new(Mutex::new(playlist)),
            modifiable: Some(true),
            ecritures: Arc::default(),
        }
    }

    pub fn ecritures(&self) -> usize {
        self.ecritures.load(Ordering::SeqCst)
    }
}

/// Une piste de service pour les bancs d'essai.
pub fn piste(
    id: &str,
    titre: &str,
    artiste: &str,
    album: &str,
    duree_ms: u64,
    disponible: Option<bool>,
) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: Some(album.to_string()).filter(|a| !a.is_empty()),
        album_id: Some(format!("alb-{id}")),
        duration_ms: duree_ms,
        cover_path: Some(format!("https://images.invalid/{id}.jpg")),
        track_number: None,
        disc_number: None,
        explicit: false,
        quality: None,
        isrc: None,
        disponible,
        composer: None,
        artist_id: None,
    }
}

fn hors_sujet<T>() -> Result<T, TuneError> {
    Err(TuneError::Streaming(
        "hors sujet pour le service factice".into(),
    ))
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
        &self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _e: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
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
        Ok(SearchResults {
            tracks: self.catalogue.clone(),
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        })
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        let playlist = self.playlist.lock().unwrap().clone();
        self.catalogue
            .iter()
            .chain(playlist.iter())
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| TuneError::NotFound(id.into()))
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
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        hors_sujet()
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        hors_sujet()
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(self.playlist.lock().unwrap().clone())
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
    fn supports_write(&self) -> bool {
        true
    }
    async fn playlist_modifiable(&self, _p: &str) -> Result<Option<bool>, TuneError> {
        Ok(self.modifiable)
    }
    async fn replace_playlist_track(
        &self,
        _playlist_id: &str,
        rang: usize,
        ancien: &str,
        nouveau: &str,
    ) -> Result<(), TuneError> {
        self.ecritures.fetch_add(1, Ordering::SeqCst);
        let remplacant = self
            .catalogue
            .iter()
            .find(|p| p.id == nouveau)
            .cloned()
            .ok_or_else(|| TuneError::NotFound(nouveau.into()))?;
        let mut liste = self.playlist.lock().unwrap();
        match liste.get_mut(rang) {
            Some(p) if p.id == ancien => {
                *p = remplacant;
                Ok(())
            }
            _ => Err(TuneError::Other("la playlist a changé".into())),
        }
    }
}
