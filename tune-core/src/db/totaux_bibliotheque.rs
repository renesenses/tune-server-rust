//! #8625 (fil 2787, Pierre M) — LE comptage de la bibliothèque, partagé par
//! tous les écrans (décision de Bertrand du 13/11 : « totaux cohérents »).
//!
//! Sur la même base, Statistiques disait 156 008 pistes / 10 403 albums,
//! Titres 155 539, Répertoire 155 513, Albums « Tout » 9 494 ou 10 427 :
//! Statistiques comptait les TABLES entières (`COUNT(*) FROM tracks`), les
//! listes comptaient ce qu'elles montrent (le socle de la vue). Désormais :
//!
//! - `pistes` = [`TrackRepo::count_visible`] : le socle de la vue des pistes
//!   (albums masqués, album distant doublé par un local #4146, copie de
//!   moindre qualité #4101 exclus) — le `total` de `GET /library/tracks` ;
//! - `albums` = [`AlbumRepo::count_visible`] : albums masqués et albums
//!   distants doublés exclus — le `total` de `GET /library/albums`.
//!
//! `GET /library/stats` le sert sous `library_totals`, avec le compte des
//! tables (`stored_*`) et l'écart, que l'écran explique en infobulle. Les
//! clés historiques `tracks` / `albums` gardent le compte des tables (#2147).

use std::sync::Arc;

use serde::Serialize;

use super::album_repo::AlbumRepo;
use super::backend::DbBackend;
use super::track_repo::TrackRepo;
use crate::TuneError;

/// Les totaux de la bibliothèque, tels que les listes les montrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TotauxBibliotheque {
    /// Pistes visibles : le `total` de `GET /library/tracks` sans filtre.
    pub tracks: i64,
    /// Albums visibles : le `total` de `GET /library/albums` sans filtre.
    pub albums: i64,
    /// Lignes de la table `tracks`, toutes comprises.
    pub stored_tracks: i64,
    /// Lignes de la table `albums`, toutes comprises.
    pub stored_albums: i64,
    /// Pistes enregistrées mais non montrées : album masqué, album distant
    /// doublé par un local, copie de moindre qualité d'une même piste.
    pub hidden_or_duplicate_tracks: i64,
    /// Albums enregistrés mais non montrés : masqués, ou distants doublés
    /// par un album local.
    pub hidden_or_duplicate_albums: i64,
}

impl TotauxBibliotheque {
    /// Lit les quatre comptes. Mêmes requêtes que les routes de liste : un
    /// écran qui affiche `tracks` affiche le `total` de la liste des titres.
    pub fn lire(db: &Arc<dyn DbBackend>) -> Result<Self, TuneError> {
        let pistes = TrackRepo::with_backend(db.clone());
        let albums = AlbumRepo::with_backend(db.clone());
        let tracks = pistes.count_visible()?;
        let stored_tracks = pistes.count()?;
        let albums_visibles = albums.count_visible()?;
        let stored_albums = albums.count()?;
        Ok(Self {
            tracks,
            albums: albums_visibles,
            stored_tracks,
            stored_albums,
            hidden_or_duplicate_tracks: (stored_tracks - tracks).max(0),
            hidden_or_duplicate_albums: (stored_albums - albums_visibles).max(0),
        })
    }
}
