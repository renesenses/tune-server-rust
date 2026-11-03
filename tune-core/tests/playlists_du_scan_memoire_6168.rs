//! #6168 — la fin d'un scan bâtissait un index de TOUTE la bibliothèque pour
//! importer ses playlists, qu'il y en ait ou non.
//!
//! `import_local_playlists` (appelé à la fin du scan manuel et du scan de
//! démarrage) lisait `list_all_local` : les 35 colonnes de chaque piste, puis
//! la `Track` complète de chaque piste, toutes vivantes en même temps, avant
//! même de chercher un fichier `.m3u`. Mesuré sur Shrek, bibliothèque
//! synthétique de 121 000 pistes sans aucune playlist : le pic du processus
//! passait de 330 à 885 Mo dans les quatre dernières secondes du scan. Au
//! rythme mesuré, près de 3 Go pour les 611 905 pistes du fil 2231.
//!
//! Ce test éprouve la PROPRIÉTÉ, pas seulement le résultat : un allocateur
//! global suit le pic d'octets vivants pendant l'appel de PRODUCTION.
//! - sans playlist à importer, le pic ne dépend pas du nombre de pistes ;
//! - avec une playlist neuve, il croît d'au plus quelques centaines d'octets
//!   par piste (l'index lui-même), jamais de la ligne complète ;
//! - et l'import rend les mêmes playlists qu'avant (chemin, puis titre et
//!   interprète, et rien de plus au second passage).
//!
//! Binaire de test à lui seul, et UN seul `#[test]` : l'allocateur est global
//! au processus, un test voisin qui allouerait en parallèle fausserait le pic.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tune_core::db::backend::DbBackend;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::library::playlist_scan::import_local_playlists;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct PeakTracking;

#[inline]
fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for PeakTracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        out
    }
}

#[global_allocator]
static ALLOC: PeakTracking = PeakTracking;

/// Pic d'octets vivants AU-DESSUS du niveau d'entrée pendant `f`.
fn pic_pendant<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let socle = LIVE.load(Ordering::Relaxed);
    PEAK.store(socle, Ordering::Relaxed);
    let r = f();
    (r, PEAK.load(Ordering::Relaxed).saturating_sub(socle))
}

/// Une base de `n` pistes locales, écrites par SQLite lui-même (`WITH
/// RECURSIVE`) : la préparation n'alloue rien côté Rust, seule la lecture
/// compte. Vingt pistes par album, un interprète par album.
fn bibliotheque(n: usize) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    db.execute_batch(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {albums} - 1) \
         INSERT INTO artists (id, name) SELECT i + 1, 'Artiste ' || i FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {albums} - 1) \
         INSERT INTO albums (id, title, artist_id) SELECT i + 1, 'Album ' || i, i + 1 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {n}) \
         INSERT INTO tracks (title, file_path, source, album_id, artist_id) \
           SELECT 'Titre ' || i, \
                  '/bibliotheque synthetique/Album ' || ((i - 1) / 20) || '/piste ' || i || '.flac', \
                  'local', (i - 1) / 20 + 1, (i - 1) / 20 + 1 \
           FROM n;",
        albums = n.div_ceil(20)
    ))
    .expect("pistes synthétiques");
    Arc::new(db)
}

const N: usize = 20_000;

#[test]
fn l_import_des_playlists_ne_tient_plus_la_bibliotheque_entiere_6168() {
    let sans_playlist = tempfile::TempDir::new().expect("dossier sans playlist");
    let dirs_vides = vec![sans_playlist.path().to_string_lossy().into_owned()];

    // 1. Aucun fichier de playlist : le pic ne suit PAS la bibliothèque.
    let petite = bibliotheque(N / 4);
    let grande = bibliotheque(N);
    let (r, pic_petite) = pic_pendant(|| import_local_playlists(&petite, &dirs_vides));
    assert_eq!(r.playlists_created, 0);
    let (r, pic_grande) = pic_pendant(|| import_local_playlists(&grande, &dirs_vides));
    assert_eq!(r.playlists_created, 0);
    eprintln!(
        "sans playlist : pic {pic_petite} o pour {} pistes, {pic_grande} o pour {N}",
        N / 4
    );
    assert!(
        pic_grande < 512 * 1024,
        "#6168 — sans aucun fichier de playlist, la fin du scan a tenu {pic_grande} octets \
         pour {N} pistes : la bibliothèque entière est encore chargée pour rien"
    );
    assert!(
        pic_grande < pic_petite + 64 * 1024,
        "#6168 — sans playlist, le pic suit le nombre de pistes : {pic_petite} o pour {} \
         pistes, {pic_grande} o pour {N}",
        N / 4
    );

    // 2. Une playlist neuve : l'index se construit, et seulement lui.
    let avec_playlist = tempfile::TempDir::new().expect("dossier avec playlist");
    std::fs::write(
        avec_playlist.path().join("Ma selection.m3u"),
        "#EXTM3U\n\
         /bibliotheque synthetique/Album 0/piste 3.flac\n\
         #EXTINF:180,Artiste 2 - Titre 57\n\
         /ailleurs/introuvable.flac\n\
         /bibliotheque synthetique/Album 999/piste 19999.flac\n\
         /nulle/part.flac\n",
    )
    .expect("écrire la playlist");
    let dirs = vec![avec_playlist.path().to_string_lossy().into_owned()];
    let (r, pic_index) = pic_pendant(|| import_local_playlists(&grande, &dirs));
    eprintln!(
        "playlist neuve : pic {pic_index} o pour {N} pistes ({} o par piste)",
        pic_index / N
    );
    assert_eq!(r.playlists_created, 1, "la playlist neuve doit être créée");
    assert_eq!(r.tracks_added, 3, "chemin, titre + interprète, chemin");
    assert!(
        pic_index < N * 600,
        "#6168 — l'index des playlists a tenu {pic_index} octets pour {N} pistes \
         ({} par piste) : des lignes complètes sont encore chargées, pas les seules clés",
        pic_index / N
    );

    // 3. Même résultat qu'avant : l'ordre de la playlist, les pistes trouvées
    //    par le chemin puis par titre + interprète, rien pour les introuvables.
    let repo = PlaylistRepo::with_backend(grande.clone());
    let playlists = repo.list(1, 100, 0).expect("playlists");
    let p = playlists
        .iter()
        .find(|p| p.name == "Ma selection")
        .expect("playlist « Ma selection »");
    let ids = repo
        .get_track_ids(p.id.expect("identifiant de playlist"))
        .expect("pistes de la playlist");
    let id_de = |chemin: &str| -> i64 {
        grande
            .query_one(
                "SELECT id FROM tracks WHERE file_path = ?1",
                &[&chemin.to_string() as &dyn tune_core::db::backend::ToSqlValue],
            )
            .expect("lecture")
            .and_then(|c| c.first().and_then(|v| v.as_i64()))
            .expect("piste")
    };
    let titre_57 = grande
        .query_one("SELECT id FROM tracks WHERE title = 'Titre 57'", &[])
        .expect("lecture")
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .expect("Titre 57");
    assert_eq!(
        ids,
        vec![
            id_de("/bibliotheque synthetique/Album 0/piste 3.flac"),
            titre_57,
            id_de("/bibliotheque synthetique/Album 999/piste 19999.flac"),
        ]
    );

    // 4. Idempotent : le second passage ne crée rien, et ne charge plus rien.
    let (r, pic_second) = pic_pendant(|| import_local_playlists(&grande, &dirs));
    assert_eq!(r.playlists_created, 0);
    assert!(
        pic_second < 512 * 1024,
        "#6168 — une playlist déjà importée a encore fait charger {pic_second} octets"
    );
}
