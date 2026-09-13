//! Folder → local playlist discovery, run after a scan (Frédéric Fongarnand).
//!
//! A directory whose files span SEVERAL library albums is treated as a
//! hand-made compilation ("dossier-playlist") and mirrored into a local
//! playlist named after the directory. Regular album folders (all tracks on
//! one album, disc subfolders included) never qualify, so a clean library
//! grows zero playlists. The sync is idempotent: the playlist is keyed by its
//! description (`Dossier : <path>`) and its contents are replaced on every
//! scan to mirror the directory.
//!
//! Gated by the `scan_folder_playlists` setting (default OFF).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::playlist_repo::PlaylistRepo;
use crate::db::settings_repo::SettingsRepo;
use tracing::{info, warn};

/// Default profile a scan-discovered playlist is attached to (Default profile).
const DEFAULT_PROFILE_ID: i64 = 1;

const SETTING_KEY: &str = "scan_folder_playlists";
const DESC_PREFIX: &str = "Dossier : ";
/// A folder needs at least this many direct tracks to become a playlist.
const MIN_TRACKS: usize = 3;
/// … and its tracks must span at least this many distinct albums.
const MIN_DISTINCT_ALBUMS: usize = 2;

pub fn folder_playlists_enabled(db: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(SETTING_KEY)
        .ok()
        .flatten()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

/// One library track as read from the DB: (id, chemin ouvrable, album_id,
/// début de la tranche CUE en ms — `0` pour une piste ordinaire).
type TrackRow = (i64, String, Option<i64>, i64);

/// Pure decision logic, unit-tested: group tracks by parent directory and
/// keep the directories that look like hand-made compilations. Track ids are
/// ordered by file name so the playlist follows the on-disk ordering.
///
/// 🔴 Le rang secondaire, c'est le début de la tranche CUE. Quinze pistes
/// d'une même image partagent le MÊME nom de fichier : sur ce seul critère
/// elles sont toutes ex æquo, et l'ordre de la playlist retomberait sur
/// l'ordre où le moteur a rendu les lignes — c'est-à-dire, sur PostgreSQL,
/// sur rien du tout. Le début de tranche les remet dans l'ordre du disque.
fn candidate_dirs(rows: &[TrackRow]) -> Vec<(String, String, Vec<i64>)> {
    let mut by_dir: BTreeMap<String, Vec<(&str, i64, Option<i64>, i64)>> = BTreeMap::new();
    for (id, path, album_id, debut_ms) in rows {
        let p = std::path::Path::new(path);
        let (Some(parent), Some(file)) = (p.parent(), p.file_name()) else {
            continue;
        };
        let Some(file) = file.to_str() else { continue };
        by_dir
            .entry(parent.to_string_lossy().into_owned())
            .or_default()
            .push((file, *id, *album_id, *debut_ms));
    }

    let mut out = Vec::new();
    for (dir, mut tracks) in by_dir {
        if tracks.len() < MIN_TRACKS {
            continue;
        }
        let mut albums: Vec<Option<i64>> = tracks.iter().map(|(_, _, a, _)| *a).collect();
        albums.sort_unstable();
        albums.dedup();
        if albums.len() < MIN_DISTINCT_ALBUMS {
            continue;
        }
        let Some(name) = std::path::Path::new(&dir)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        tracks.sort_by(|a, b| a.0.cmp(b.0).then(a.3.cmp(&b.3)));
        let ids = tracks.iter().map(|(_, id, _, _)| *id).collect();
        out.push((dir, name, ids));
    }
    out
}

/// Discover folder playlists and mirror them into local playlists.
/// Called at the end of manual and automatic scans when the setting is on.
pub fn sync_folder_playlists(db: &Arc<dyn DbBackend>) {
    // 🔴 `COALESCE(file_path, cue_media_path)`, et pas `file_path` seul. Une
    // piste découpée par une feuille CUE porte `file_path = NULL` par
    // construction : sur `file_path IS NOT NULL`, un dossier-playlist qui
    // mélange des fichiers ordinaires et une image CUE perdait en silence
    // toutes les pistes de l'image. Voir `db::track_repo::sql::A_UN_FICHIER`.
    let chemin = crate::db::track_repo::sql::CHEMIN_OUVRABLE;
    let a_un_fichier = crate::db::track_repo::sql::A_UN_FICHIER;
    let rows = match db.query_many(
        &format!(
            "SELECT t.id, {chemin}, t.album_id, COALESCE(t.cue_start_ms, 0) FROM tracks t \
             WHERE t.source = 'local' AND {a_un_fichier}"
        ),
        &[],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "folder_playlists_track_query_failed");
            return;
        }
    };
    let tracks: Vec<TrackRow> = rows
        .iter()
        .filter_map(|cols| {
            Some((
                cols.first()?.as_i64()?,
                cols.get(1)?.as_str()?.to_owned(),
                cols.get(2).and_then(|v| v.as_i64()),
                cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
            ))
        })
        .collect();

    let candidates = candidate_dirs(&tracks);
    if candidates.is_empty() {
        return;
    }

    let repo = PlaylistRepo::with_backend(db.clone());
    let existing = match repo.list(DEFAULT_PROFILE_ID, 10_000, 0) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "folder_playlists_list_failed");
            return;
        }
    };

    let mut created = 0usize;
    let mut updated = 0usize;
    for (dir, name, ids) in candidates {
        let desc = format!("{DESC_PREFIX}{dir}");
        let found = existing
            .iter()
            .find(|p| p.description.as_deref() == Some(desc.as_str()));
        let playlist_id = match found {
            Some(p) => match p.id {
                Some(id) => id,
                None => continue,
            },
            None => match repo.create(&name, Some(&desc), DEFAULT_PROFILE_ID) {
                Ok(id) => {
                    created += 1;
                    id
                }
                Err(e) => {
                    warn!(dir = %dir, error = %e, "folder_playlist_create_failed");
                    continue;
                }
            },
        };
        if repo.get_track_ids(playlist_id).ok().as_deref() == Some(ids.as_slice()) {
            continue;
        }
        match repo.set_tracks(playlist_id, &ids) {
            Ok(()) => {
                if found.is_some() {
                    updated += 1;
                }
            }
            Err(e) => warn!(dir = %dir, error = %e, "folder_playlist_sync_failed"),
        }
    }
    if created > 0 || updated > 0 {
        info!(created, updated, "folder_playlists_synced");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, path: &str, album: Option<i64>) -> TrackRow {
        (id, path.to_owned(), album, 0)
    }

    /// Une tranche de feuille CUE : le chemin est celui de l'IMAGE, partagé
    /// par toutes les pistes du disque, et le rang vient du début de tranche.
    fn row_cue(id: i64, image: &str, album: Option<i64>, debut_ms: i64) -> TrackRow {
        (id, image.to_owned(), album, debut_ms)
    }

    #[test]
    fn album_folder_is_not_a_playlist() {
        // One album spread over a folder (and a disc subfolder) → no candidate.
        let rows = vec![
            row(1, "/music/Artist/Album/01.flac", Some(7)),
            row(2, "/music/Artist/Album/02.flac", Some(7)),
            row(3, "/music/Artist/Album/03.flac", Some(7)),
            row(4, "/music/Artist/Album/CD2/01.flac", Some(7)),
            row(5, "/music/Artist/Album/CD2/02.flac", Some(7)),
            row(6, "/music/Artist/Album/CD2/03.flac", Some(7)),
        ];
        assert!(candidate_dirs(&rows).is_empty());
    }

    #[test]
    fn compilation_folder_becomes_playlist_in_filename_order() {
        let rows = vec![
            row(10, "/music/Playlists/Soirée/03 - C.flac", Some(1)),
            row(11, "/music/Playlists/Soirée/01 - A.flac", Some(2)),
            row(12, "/music/Playlists/Soirée/02 - B.flac", None),
        ];
        let out = candidate_dirs(&rows);
        assert_eq!(out.len(), 1);
        let (dir, name, ids) = &out[0];
        assert_eq!(dir, "/music/Playlists/Soirée");
        assert_eq!(name, "Soirée");
        assert_eq!(ids, &vec![11, 12, 10]);
    }

    #[test]
    fn too_few_tracks_or_albums_is_skipped() {
        // 2 tracks over 2 albums: below MIN_TRACKS.
        let rows = vec![
            row(1, "/m/d/a.flac", Some(1)),
            row(2, "/m/d/b.flac", Some(2)),
        ];
        assert!(candidate_dirs(&rows).is_empty());
    }

    /// 🔴 UN DOSSIER-PLAYLIST QUI CONTIENT UNE IMAGE CUE.
    ///
    /// Les pistes d'une feuille CUE portent `file_path = NULL` par
    /// construction. Le tri par nom de fichier les met toutes ex æquo (elles
    /// partagent l'image) : sans rang secondaire, la playlist les rangeait
    /// dans l'ordre où le moteur avait rendu les lignes — non spécifié, et
    /// arbitraire sur PostgreSQL.
    #[test]
    fn les_tranches_d_une_image_cue_suivent_l_ordre_du_disque() {
        let rows = vec![
            row(10, "/music/Soirée/01 - A.flac", Some(1)),
            // L'image est ARRIVÉE dans le désordre, comme une base peut la rendre.
            row_cue(22, "/music/Soirée/gould.flac", Some(2), 180_000),
            row_cue(21, "/music/Soirée/gould.flac", Some(2), 0),
            row_cue(23, "/music/Soirée/gould.flac", Some(2), 300_000),
        ];
        let out = candidate_dirs(&rows);
        assert_eq!(out.len(), 1, "le dossier mixte est bien une playlist");
        let (_, _, ids) = &out[0];
        assert_eq!(
            ids,
            &vec![10, 21, 22, 23],
            "les trois tranches de l'image doivent suivre leur ordre de disque"
        );
    }

    /// Le témoin de bout en bout : la requête, pas seulement le tri.
    ///
    /// 🔴 La fixture a `file_path = NULL` — c'est tout l'enjeu. Une piste avec
    /// un `file_path` renseigné décrirait le cas qui marchait déjà.
    #[test]
    fn une_piste_cue_entre_dans_la_playlist_de_dossier() {
        use crate::db::album_repo::AlbumRepo;
        use crate::db::artist_repo::ArtistRepo;
        use crate::db::backend::DbBackend;
        use crate::db::models::{Artist, Track};
        use crate::db::playlist_repo::PlaylistRepo;
        use crate::db::settings_repo::SettingsRepo;
        use crate::db::sqlite::SqliteDb;
        use crate::db::track_repo::TrackRepo;

        let sqlite = SqliteDb::open_in_memory().unwrap();
        sqlite.init_schema().unwrap();
        crate::db::migrations::run_migrations(&sqlite).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(sqlite);
        SettingsRepo::with_backend(db.clone())
            .set(SETTING_KEY, "true")
            .unwrap();

        let artiste = ArtistRepo::with_backend(db.clone())
            .create(&Artist::new("Glenn Gould".into()))
            .unwrap();
        let albums = AlbumRepo::with_backend(db.clone());
        let album = |titre: &str| {
            albums
                .get_or_create(titre, artiste, Some(1981))
                .unwrap()
                .id
                .unwrap()
        };
        let (a1, a2, a3) = (album("Un"), album("Deux"), album("Goldberg"));

        let repo = TrackRepo::with_backend(db.clone());
        // Deux fichiers ordinaires, d'albums différents…
        for (n, album_id) in [(1i64, a1), (2, a2)] {
            let mut t = Track::new(format!("piste {n}"));
            t.source = "local".into();
            t.album_id = Some(album_id);
            t.file_path = Some(format!("/music/Soirée/0{n} - piste.flac"));
            repo.create(&t).unwrap();
        }
        // … et UNE image CUE posée dans le même dossier.
        let mut cue = Track::new("Aria".into());
        cue.source = "local".into();
        cue.album_id = Some(a3);
        cue.file_path = None;
        cue.cue_media_path = Some("/music/Soirée/gould.flac".into());
        cue.cue_start_ms = Some(0);
        let aria = repo.create(&cue).unwrap();

        sync_folder_playlists(&db);

        let playlists = PlaylistRepo::with_backend(db.clone())
            .list(DEFAULT_PROFILE_ID, 100, 0)
            .unwrap();
        let dossier = playlists
            .iter()
            .find(|p| p.description.as_deref() == Some("Dossier : /music/Soirée"))
            .expect("le dossier mixte doit donner une playlist");
        let ids = PlaylistRepo::with_backend(db.clone())
            .get_track_ids(dossier.id.unwrap())
            .unwrap();
        assert!(
            ids.contains(&aria),
            "la piste CUE (file_path = NULL) doit entrer dans la playlist de \
             dossier ; contenu rendu : {ids:?}"
        );
        assert_eq!(ids.len(), 3, "les trois pistes du dossier, CUE comprise");
    }
}
