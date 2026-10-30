//! Import playlist files (`.m3u` / `.m3u8` / `.pls`) found while scanning music
//! directories, turning each into a local Tune playlist (Bertrand: "les
//! playlists rencontrées au scan doivent être ajoutées en playlists locales").
//!
//! Reuses the existing parse + match engine (`library::importer`): each playlist
//! file's entries are resolved (relative paths against the playlist's own folder)
//! and matched against the library by file path first, then a title/artist/album
//! fuzzy fallback. Audirvana Qobuz identities are kept as service entries.
//! Only playlists with at least one local match or service entry are created;
//! an existing playlist name is skipped so a re-scan never duplicates it.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tracing::{info, warn};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::db::backend::DbBackend;
use crate::db::playlist_repo::{EntryContent, PlaylistRepo};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::library::importer::{ImportedTrack, match_tracks, parse_m3u, parse_pls};

/// Default profile a scan-imported playlist is attached to (Default profile).
const DEFAULT_PROFILE_ID: i64 = 1;

#[derive(Debug, Default)]
pub struct PlaylistScanReport {
    pub playlists_created: usize,
    pub tracks_added: usize,
    /// #7503 — ce que le journal dit de chaque sortie, tenu aussi ici pour
    /// que les essais le vérifient : fichiers de playlist trouvés sous les
    /// racines…
    pub files_found: usize,
    /// … ceux écartés parce qu'une playlist du même nom existe déjà…
    pub skipped_name_taken: Vec<String>,
    /// … et ceux dont aucune ligne ne s'est rapprochée de la bibliothèque.
    pub no_match: Vec<String>,
}

/// Canonical comparison key for a file path: forward slashes + NFC. Both the
/// library index and the resolved playlist entries go through this so paths
/// compare equal regardless of separator/normalisation origin.
fn path_key(p: &str) -> String {
    p.replace('\\', "/").nfc().collect()
}

/// Lexically resolve `..` / `.` without touching the filesystem (the referenced
/// file may have moved; we only want a comparable absolute-ish path).
fn lexical_clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve one playlist entry to a comparison key, or `None` for URLs / empties.
fn resolve_entry(entry: &str, playlist_dir: &Path) -> Option<String> {
    let e = entry.trim();
    if e.is_empty() || e.starts_with("http://") || e.starts_with("https://") {
        return None;
    }
    let p = Path::new(e);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        playlist_dir.join(p)
    };
    Some(path_key(&lexical_clean(&abs).to_string_lossy()))
}

/// Clé de réglage qui coupe cet import. Absente ⇒ import actif : le
/// comportement d'origine reste celui par défaut, on n'en prive personne
/// silencieusement.
pub const SETTING_IMPORT_PLAYLISTS: &str = "scan_import_playlists";

/// L'import des playlists rencontrées au scan est-il actif ?
///
/// Opt-out explicite : seule la valeur `"false"` le désactive. Une clé absente,
/// vide ou illisible laisse l'import en place.
pub fn playlist_import_enabled(db: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(SETTING_IMPORT_PLAYLISTS)
        .ok()
        .flatten()
        .as_deref()
        != Some("false")
}

/// Scan `dirs` for playlist files and create a local playlist for each one that
/// resolves to at least one library track. Idempotent by playlist name.
pub fn import_local_playlists(db: &Arc<dyn DbBackend>, dirs: &[String]) -> PlaylistScanReport {
    let mut report = PlaylistScanReport::default();

    // Le garde-fou est ICI, et pas chez les appelants.
    //
    // Deux endroits déclenchent cet import — le scan automatique et le scan
    // manuel. Le poser à chaque appel, c'était garantir qu'un troisième
    // appelant l'oublie un jour. JP Borderies a vu arriver des playlists
    // « Cabrel » après avoir ajouté deux dossiers qui n'ont rien à voir : le
    // scan balaie TOUS les répertoires configurés, pas seulement le nouveau,
    // et y ramasse de vieux fichiers .m3u oubliés. Comportement voulu, mais
    // sans moyen de dire non.
    if !playlist_import_enabled(db) {
        info!("playlist_scan_disabled_by_setting");
        return report;
    }

    let track_repo = TrackRepo::with_backend(db.clone());
    let playlist_repo = PlaylistRepo::with_backend(db.clone());

    // Existing playlist names (default profile) — skip to stay idempotent.
    let existing: HashSet<String> = playlist_repo
        .list(DEFAULT_PROFILE_ID, 100_000, 0)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.name)
        .collect();

    // #6168 — les fichiers à importer AVANT l'index de la bibliothèque. L'index
    // était bâti à chaque fin de scan, même sans aucun fichier de playlist, ou
    // quand tous portaient déjà une playlist du même nom : c'était le pic de
    // mémoire de la fin du scan (Shrek, 121 000 pistes : de 330 à 885 Mo).
    // Même filtre qu'avant, sur le même `existing` lu une fois.
    //
    // #7503 — chaque fichier écarté l'est À VOIX HAUTE. Un `.m3u` posé dans le
    // dossier de musique, un scan qui finit, et aucune ligne `playlist_scan_*`
    // au journal : l'utilisateur ne pouvait pas savoir si le fichier n'avait
    // pas été vu, ou s'il avait été ignoré parce qu'une playlist du même nom
    // existait déjà — dans le profil 1, qu'il ne regarde peut-être pas.
    let fichiers = find_playlist_files(dirs);
    report.files_found = fichiers.len();
    info!(
        found = fichiers.len(),
        dirs = ?dirs,
        profile_id = DEFAULT_PROFILE_ID,
        "playlist_scan_files_found"
    );
    let mut a_importer: Vec<(PathBuf, String)> = Vec::new();
    for pf in fichiers {
        let Some(name) = nom_de_playlist(&pf) else {
            warn!(path = %pf.display(), "playlist_scan_skipped_unreadable_name");
            continue;
        };
        if existing.contains(&name) {
            info!(
                name = %name,
                path = %pf.display(),
                profile_id = DEFAULT_PROFILE_ID,
                "playlist_scan_skipped_name_taken — une playlist de ce nom existe déjà dans ce profil ; renommer le fichier ou supprimer la playlist pour le réimporter"
            );
            report.skipped_name_taken.push(name);
            continue;
        }
        a_importer.push((pf, name));
    }
    if a_importer.is_empty() {
        return report;
    }

    // Build the match indexes once from the freshly-scanned library — page by
    // page, from the five columns they compare (#6168), never from whole
    // `Track` rows.
    let mut path_index: HashMap<String, i64> = HashMap::new();
    let mut fuzzy_index: HashMap<(String, String, String), i64> = HashMap::new();
    if let Err(e) = track_repo.pour_chaque_cle_de_playlist(|t| {
        if let Some(fp) = &t.file_path {
            path_index.insert(path_key(fp), t.id);
        }
        let key = (
            t.title.to_lowercase(),
            t.artist_name.unwrap_or_default().to_lowercase(),
            t.album_title.unwrap_or_default().to_lowercase(),
        );
        fuzzy_index.entry(key).or_insert(t.id);
    }) {
        warn!(error = %e, "playlist_scan_load_tracks_failed");
        return report;
    }

    for (pf, name) in a_importer {
        let raw = match std::fs::read(&pf) {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %pf.display(), error = %e, "playlist_file_read_failed");
                continue;
            }
        };
        // #7503 — le décodage de l'import manuel : BOM retiré, repli Latin-1.
        // `from_utf8_lossy` seul changeait chaque accent d'un .m3u en
        // Windows-1252 en U+FFFD (aucun chemin accentué ne se rapprochait), et
        // laissait le BOM collé à la première ligne.
        let text = super::m3u_parser::decode(&raw, false);
        let is_pls = pf
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pls"));
        let mut imported: Vec<ImportedTrack> = if is_pls {
            parse_pls(&text, Some(&name))
        } else {
            parse_m3u(&text, Some(&name))
        };

        // Resolve each entry's file_path (relative → absolute key). Entries that
        // are URLs (radio streams) drop their file_path so only the title/artist
        // fuzzy fallback can match them (usually it won't — that's fine).
        // Les liens Qobuz portent une identité de service : le rapprochement
        // local ne doit ni les perdre ni leur substituer un titre homonyme.
        let services: Vec<_> = imported
            .iter()
            .map(|it| {
                let chemin = it.file_path.as_deref().unwrap_or("");
                let est_qobuz = chemin
                    .split_once("://")
                    .is_some_and(|(s, _)| s.eq_ignore_ascii_case("qobuz"));
                let contenu = super::m3u_parser::entree_qobuz_audirvana(
                    chemin,
                    it.title.as_deref(),
                    it.artist.as_deref(),
                    None,
                )
                .map(EntryContent::Service);
                (est_qobuz, contenu)
            })
            .collect();
        let playlist_dir = pf.parent().unwrap_or_else(|| Path::new(""));
        for it in &mut imported {
            it.file_path = it
                .file_path
                .as_deref()
                .and_then(|fp| resolve_entry(fp, playlist_dir));
        }

        let results = match_tracks(&imported, &path_index, &fuzzy_index);
        // #7503 — les lignes sans correspondance, et la première d'entre elles
        // telle qu'elle a été cherchée : c'est ce qui permet de voir d'un coup
        // d'œil un chemin qui ne ressemble pas à celui de la bibliothèque.
        let mut non_rapprochees = 0usize;
        let mut exemple: Option<String> = None;
        for ((r, it), (est_qobuz, service)) in results.iter().zip(&imported).zip(&services) {
            let trouvee = service.is_some() || (!*est_qobuz && r.tune_track_id.is_some());
            if !trouvee {
                non_rapprochees += 1;
                if exemple.is_none() {
                    exemple = Some(
                        it.file_path
                            .clone()
                            .unwrap_or_else(|| r.imported_title.clone()),
                    );
                }
            }
        }
        let exemple = exemple.unwrap_or_default();
        // L'ordre mixte est celui du fichier ; le dépôt dédoublonne les
        // identités locales et de service dans la même transaction.
        let entrees: Vec<EntryContent> = results
            .iter()
            .zip(services)
            .filter_map(|(r, (est_qobuz, service))| {
                service.or_else(|| {
                    if est_qobuz {
                        None
                    } else {
                        r.tune_track_id.map(EntryContent::Local)
                    }
                })
            })
            .collect();
        if entrees.is_empty() {
            warn!(
                name = %name,
                path = %pf.display(),
                total = imported.len(),
                unmatched = non_rapprochees,
                exemple = %exemple,
                "playlist_scan_no_match — aucune ligne du fichier ne correspond à une piste de la bibliothèque ; playlist non créée"
            );
            report.no_match.push(name);
            continue;
        }
        match playlist_repo.create_with_entries(
            &name,
            Some("Importée au scan"),
            DEFAULT_PROFILE_ID,
            &entrees,
        ) {
            Ok((_, ajoutees)) => {
                report.playlists_created += 1;
                report.tracks_added += ajoutees.len();
                info!(name = %name, matched = ajoutees.len(), total = imported.len(),
                    unmatched = non_rapprochees, exemple = %exemple,
                    profile_id = DEFAULT_PROFILE_ID, "playlist_scan_imported");
            }
            Err(e) => warn!(name = %name, error = %e, "playlist_scan_create_failed"),
        }
    }

    if report.playlists_created > 0 {
        info!(
            playlists = report.playlists_created,
            tracks = report.tracks_added,
            "playlist_scan_complete"
        );
    }
    report
}

/// Walk the scan dirs for `.m3u` / `.m3u8` / `.pls` files.
fn find_playlist_files(dirs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        // #7503 — une erreur de parcours (droits, lien cassé, boucle) jetait
        // en silence tout ce qu'il y avait dessous : on la compte et on cite
        // la première.
        let mut erreurs = 0usize;
        let mut premiere: Option<String> = None;
        for entry in WalkDir::new(dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|r| {
                r.map_err(|e| {
                    erreurs += 1;
                    if premiere.is_none() {
                        premiere = Some(e.to_string());
                    }
                })
                .ok()
            })
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_lowercase().as_str(), "m3u" | "m3u8" | "pls"))
            {
                out.push(path.to_path_buf());
            }
        }
        if erreurs > 0 {
            warn!(
                dir = %dir,
                errors = erreurs,
                premiere = %premiere.unwrap_or_default(),
                "playlist_scan_walk_errors"
            );
        }
    }
    out
}

/// Nom de la playlist créée pour un fichier : son nom sans extension, rogné.
/// `None` pour un nom vide ou qui n'est pas de l'UTF-8.
fn nom_de_playlist(pf: &Path) -> Option<String> {
    pf.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Arc<dyn DbBackend> {
        use crate::db::migrations;
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().expect("base memoire");
        db.init_schema().expect("schema");
        migrations::run_migrations(&db).expect("migrations");
        Arc::new(db)
    }

    /// Sans reglage, l'import reste actif : on ne prive personne du
    /// comportement d'origine en ajoutant un interrupteur.
    #[test]
    fn playlist_import_is_on_when_the_setting_is_absent() {
        assert!(playlist_import_enabled(&mem_db()));
    }

    /// Seule la chaine « false » coupe. Demande par JP Borderies, qui voyait
    /// arriver des playlists sans rapport avec les dossiers qu'il venait
    /// d'ajouter — le scan balaie TOUS les repertoires configures.
    #[test]
    fn playlist_import_is_off_only_for_false() {
        let db = mem_db();
        let settings = SettingsRepo::with_backend(db.clone());

        settings.set(SETTING_IMPORT_PLAYLISTS, "false").unwrap();
        assert!(!playlist_import_enabled(&db), "« false » doit couper");

        settings.set(SETTING_IMPORT_PLAYLISTS, "true").unwrap();
        assert!(playlist_import_enabled(&db));

        // Une valeur inattendue ne doit pas couper par accident : en cas de
        // doute on garde le comportement d'origine.
        settings.set(SETTING_IMPORT_PLAYLISTS, "").unwrap();
        assert!(playlist_import_enabled(&db));
        settings.set(SETTING_IMPORT_PLAYLISTS, "0").unwrap();
        assert!(playlist_import_enabled(&db));
    }

    /// Le garde-fou coupe le travail AVANT de lire la bibliotheque : sinon on
    /// paierait le balayage complet des repertoires pour rien.
    #[test]
    fn disabled_import_returns_an_empty_report() {
        let db = mem_db();
        SettingsRepo::with_backend(db.clone())
            .set(SETTING_IMPORT_PLAYLISTS, "false")
            .unwrap();

        let report = import_local_playlists(&db, &["/nexiste/pas".to_string()]);
        assert_eq!(report.playlists_created, 0);
        assert_eq!(report.tracks_added, 0);
    }

    /// Une piste en base, au chemin donné (titre neutre : le rapprochement
    /// testé ici est celui du CHEMIN, pas le repli titre/artiste).
    fn piste(db: &Arc<dyn DbBackend>, id: i64, chemin: &str) {
        use crate::db::backend::ToSqlValue;
        let titre = format!("Piste {id}");
        let params: [&dyn ToSqlValue; 3] = [&id, &titre, &chemin];
        db.execute(
            "INSERT INTO tracks (id, title, file_path) VALUES (?, ?, ?)",
            &params,
        )
        .unwrap();
    }

    fn racine(dir: &tempfile::TempDir) -> Vec<String> {
        vec![dir.path().to_string_lossy().into_owned()]
    }

    /// #7503 — un fichier ignoré parce que son nom est déjà pris l'est À
    /// VOIX HAUTE : le rapport (et le journal) le nomme. Avant, rien ne
    /// distinguait ce cas d'un fichier jamais vu.
    #[test]
    fn nom_deja_pris_est_rapporte_7503() {
        let db = mem_db();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Mix.m3u"), "#EXTM3U\n/x/a.flac\n").unwrap();
        PlaylistRepo::with_backend(db.clone())
            .create("Mix", None, DEFAULT_PROFILE_ID)
            .unwrap();

        let r = import_local_playlists(&db, &racine(&dir));
        assert_eq!(r.files_found, 1);
        assert_eq!(r.skipped_name_taken, vec!["Mix".to_string()]);
        assert_eq!(r.playlists_created, 0);
    }

    /// #7503 — zéro ligne rapprochée : la playlist n'est pas créée, mais le
    /// rapport le dit (et le journal cite un chemin introuvable).
    #[test]
    fn zero_rapprochement_est_rapporte_7503() {
        let db = mem_db();
        piste(&db, 1, "/ailleurs/autre.flac");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Vide.m3u"), "#EXTM3U\n/nulle/part.flac\n").unwrap();

        let r = import_local_playlists(&db, &racine(&dir));
        assert_eq!(r.files_found, 1);
        assert_eq!(r.no_match, vec!["Vide".to_string()]);
        assert_eq!(r.playlists_created, 0);
    }

    /// #7503 — la forme exacte du fichier du fil 2550 : `#EXTINF:-1, - Titre`,
    /// chemins ABSOLUS avec apostrophes, `&`, parenthèses, `_`, espaces ; ici
    /// en plus un BOM et des fins de ligne CRLF. Tout se rapproche par le
    /// chemin.
    #[test]
    fn chemins_du_fil_2550_se_rapprochent_7503() {
        let db = mem_db();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_string_lossy().into_owned();
        let chemins = [
            format!("{base}/Pop Rock/The '59 Sound/1-02 The '59 Sound.flac"),
            format!("{base}/Pop Rock/Agents of Fortune/1-03 (Don't Fear) The Reaper.flac"),
            format!("{base}/Pop Rock/Boys Don't Cry/1-03 10_15 Saturday Night.flac"),
            format!(
                "{base}/Pop Rock/Abacab (embossed RBYG _ red_blue_yellow_grey cover)/1-01 Abacab.flac"
            ),
            format!("{base}/Pop Rock/Northern Lights - Southern Cross/004 A & B.flac"),
        ];
        let mut m3u = String::from("\u{feff}#EXTM3U\r\n");
        for (i, c) in chemins.iter().enumerate() {
            piste(&db, i as i64 + 1, c);
            m3u.push_str(&format!("#EXTINF:-1, - Titre {i}\r\n{c}\r\n"));
        }
        std::fs::write(dir.path().join("Ma liste.m3u"), m3u).unwrap();

        let r = import_local_playlists(&db, &racine(&dir));
        assert_eq!(r.playlists_created, 1, "{r:?}");
        assert_eq!(r.tracks_added, chemins.len());
        assert!(r.no_match.is_empty());
    }

    /// #7503 — un .m3u en Latin-1 (Windows-1252) : ses accents se lisent comme
    /// à l'import manuel. Contre-épreuve : avec `String::from_utf8_lossy`, `é`
    /// (0xE9) devenait U+FFFD et le chemin ne se rapprochait plus.
    #[test]
    fn m3u_latin1_se_rapproche_7503() {
        let db = mem_db();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_string_lossy().into_owned();
        let chemin = format!("{base}/Caf\u{e9}/01 \u{c9}t\u{e9}.flac");
        piste(&db, 1, &chemin);
        let mut octets = b"#EXTM3U\n".to_vec();
        octets.extend(chemin.chars().map(|c| u8::try_from(u32::from(c)).unwrap()));
        octets.push(b'\n');
        assert!(
            std::str::from_utf8(&octets).is_err(),
            "le fichier doit être du Latin-1"
        );
        std::fs::write(dir.path().join("Latin.m3u"), octets).unwrap();

        let r = import_local_playlists(&db, &racine(&dir));
        assert_eq!(r.playlists_created, 1, "{r:?}");
        assert_eq!(r.tracks_added, 1);
    }

    /// #7503 — le BOM UTF-8 ne devient plus une « entrée » de chemin.
    #[test]
    fn bom_ne_devient_pas_une_entree_7503() {
        let octets = "\u{feff}#EXTM3U\r\n/a/b.flac\r\n".as_bytes();
        let texte = crate::library::m3u_parser::decode(octets, false);
        let lignes = parse_m3u(&texte, None);
        assert_eq!(lignes.len(), 1);
        assert_eq!(lignes[0].file_path.as_deref(), Some("/a/b.flac"));
    }

    #[test]
    fn resolve_relative_and_absolute_and_url() {
        let dir = Path::new("/music/My Playlist");
        // Relative → joined against the playlist dir, `..` collapsed.
        assert_eq!(
            resolve_entry("01 - Song.flac", dir).as_deref(),
            Some("/music/My Playlist/01 - Song.flac")
        );
        assert_eq!(
            resolve_entry("../Album/02 - T.flac", dir).as_deref(),
            Some("/music/Album/02 - T.flac")
        );
        // Absolute kept as-is (slash-normalised).
        assert_eq!(
            resolve_entry("/music/x/y.flac", dir).as_deref(),
            Some("/music/x/y.flac")
        );
        // Backslash entry normalised to slashes.
        assert_eq!(
            resolve_entry(r"sub\z.flac", dir).as_deref(),
            Some("/music/My Playlist/sub/z.flac")
        );
        // URLs / empties dropped.
        assert_eq!(resolve_entry("http://radio.example/stream", dir), None);
        assert_eq!(resolve_entry("   ", dir), None);
    }
}
