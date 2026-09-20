//! Ce que `tune-smart-http` demande aux services, fourni par le registre.
//!
//! #4473 — Bertrand, 19/09/2026 : *« Serait-il possible d'étendre les
//! source = Qobuz à l'intégralité du catalogue ? »*
//!
//! `tune-smart-http` ne connaît que la base : c'est délibéré, et ça permet à
//! Cargo de recompiler ces routes sans invalider le reste de `tune-server`. Le
//! module y déclare donc **ce dont il a besoin** (`CatalogueDistant`), et c'est
//! ICI qu'on le branche sur le registre des services.
//!
//! ## Comment on trouve un artiste
//!
//! Un service n'expose pas « les albums de l'artiste nommé X » : il expose
//! `search` puis `get_artist_albums(id)`. On cherche donc l'artiste, on retient
//! celui dont le nom correspond EXACTEMENT (sans tenir compte de la casse), et
//! on demande ses albums.
//!
//! 🔴 L'exactitude est volontaire. Chercher « Coltrane » rend aussi « Alice
//! Coltrane », « Ravi Coltrane » et des hommages : prendre le premier venu
//! remplirait la collection de quelqu'un d'autre. Faute de correspondance
//! exacte, on rend une liste VIDE — la règle n'a pas trouvé son artiste, et
//! c'est une réponse, pas une panne.
use std::sync::Arc;

use tokio::sync::Mutex;
use tune_core::streaming::ServiceRegistry;
use tune_smart_http::catalogue::{AlbumDistant, CatalogueDistant, PisteDistante};

/// Combien de résultats on demande au service pour retrouver un artiste ou un
/// album par son nom. Au-delà, ce ne sont plus des homonymes mais du bruit.
const CANDIDATS: usize = 20;

/// Combien d'ÉDITIONS d'un même titre d'album on développe en pistes. Au-delà,
/// ce sont des rééditions et des compilations, et chacune coûte un appel.
const ALBUMS_DEVELOPPES: usize = 3;

pub struct CatalogueDuRegistre {
    services: Arc<Mutex<ServiceRegistry>>,
}

impl CatalogueDuRegistre {
    pub fn new(services: Arc<Mutex<ServiceRegistry>>) -> Self {
        Self { services }
    }

    async fn service(
        &self,
        nom: &str,
    ) -> Option<Arc<tokio::sync::RwLock<Box<dyn tune_core::streaming::StreamingService>>>> {
        self.services.lock().await.get(nom)
    }
}

fn meme_nom(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

#[async_trait::async_trait]
impl CatalogueDistant for CatalogueDuRegistre {
    async fn albums_par_artiste(&self, service: &str, nom: &str) -> Vec<AlbumDistant> {
        let Some(s) = self.service(service).await else {
            return Vec::new();
        };
        let garde = s.read().await;
        let Ok(res) = garde.search(nom, CANDIDATS).await else {
            return Vec::new();
        };
        let Some(artiste) = res.artists.iter().find(|a| meme_nom(&a.name, nom)) else {
            return Vec::new();
        };
        let Ok(albums) = garde.get_artist_albums(&artiste.id).await else {
            return Vec::new();
        };
        albums
            .into_iter()
            .map(|a| AlbumDistant {
                service: service.to_string(),
                source_id: a.id,
                title: a.title,
                artist: a.artist,
                cover_url: a.cover_path,
                year: a.year.map(i64::from),
            })
            .collect()
    }

    async fn albums_par_titre(&self, service: &str, titre: &str) -> Vec<AlbumDistant> {
        let Some(s) = self.service(service).await else {
            return Vec::new();
        };
        let garde = s.read().await;
        let Ok(res) = garde.search(titre, CANDIDATS).await else {
            return Vec::new();
        };
        res.albums
            .into_iter()
            .filter(|a| meme_nom(&a.title, titre))
            .map(|a| AlbumDistant {
                service: service.to_string(),
                source_id: a.id,
                title: a.title,
                artist: a.artist,
                cover_url: a.cover_path,
                year: a.year.map(i64::from),
            })
            .collect()
    }

    async fn pistes_par_artiste(&self, service: &str, nom: &str) -> Vec<PisteDistante> {
        let Some(s) = self.service(service).await else {
            return Vec::new();
        };
        let garde = s.read().await;
        let Ok(res) = garde.search(nom, CANDIDATS).await else {
            return Vec::new();
        };
        let Some(artiste) = res.artists.iter().find(|a| meme_nom(&a.name, nom)) else {
            return Vec::new();
        };
        // Les titres phares, pas « toutes ses pistes » : ce dernier ensemble
        // demanderait un aller-retour par album, et le service ne le propose
        // pas d'un bloc.
        let Ok(pistes) = garde.get_artist_top_tracks(&artiste.id).await else {
            return Vec::new();
        };
        pistes.into_iter().map(|t| piste(service, t)).collect()
    }

    /// Les pistes de l'album de ce titre — #4473, second volet.
    ///
    /// Même marche que [`CatalogueDuRegistre::albums_par_titre`] : on cherche,
    /// on ne garde que les correspondances EXACTES de titre, puis on demande
    /// leurs pistes.
    ///
    /// 🔴 Borné à [`ALBUMS_DEVELOPPES`] : un titre courant (« Live », « Greatest
    /// Hits ») rapproche des dizaines d'éditions, et chacune coûterait un
    /// aller-retour réseau avant que l'écran n'affiche quoi que ce soit.
    async fn pistes_par_album(&self, service: &str, titre: &str) -> Vec<PisteDistante> {
        let Some(s) = self.service(service).await else {
            return Vec::new();
        };
        let garde = s.read().await;
        let Ok(res) = garde.search(titre, CANDIDATS).await else {
            return Vec::new();
        };
        let mut pistes = Vec::new();
        for a in res
            .albums
            .iter()
            .filter(|a| meme_nom(&a.title, titre))
            .take(ALBUMS_DEVELOPPES)
        {
            let Ok(p) = garde.get_album_tracks(&a.id).await else {
                continue;
            };
            pistes.extend(p.into_iter().map(|t| piste(service, t)));
        }
        pistes
    }
}

fn piste(service: &str, t: tune_core::streaming::StreamTrack) -> PisteDistante {
    PisteDistante {
        service: service.to_string(),
        source_id: t.id,
        title: t.title,
        artist: t.artist,
        album: t.album.unwrap_or_default(),
        cover_url: t.cover_path,
        duration_ms: i64::try_from(t.duration_ms).ok(),
    }
}
