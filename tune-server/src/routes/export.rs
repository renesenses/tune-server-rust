use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use tune_http_types::panne_sql::OuDefautJournalise;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tracks.csv", get(export_tracks_csv))
        .route("/albums.csv", get(export_albums_csv))
        .route("/artists.csv", get(export_artists_csv))
        .route("/library-audit.csv", get(export_library_audit_csv))
}

async fn export_tracks_csv(State(state): State<AppState>) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = repo.list(999999, 0).unwrap_or_default();

    let mut csv = String::from(
        "id,title,artist,album,disc,track,duration_ms,format,sample_rate,bit_depth,file_path\n",
    );
    for t in &tracks {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            t.id.unwrap_or(0),
            csv_escape(&t.title),
            csv_escape(t.artist_name.as_deref().unwrap_or("")),
            csv_escape(t.album_title.as_deref().unwrap_or("")),
            t.disc_number,
            t.track_number,
            t.duration_ms,
            t.format.as_deref().unwrap_or(""),
            t.sample_rate.unwrap_or(0),
            t.bit_depth.unwrap_or(0),
            csv_escape(t.file_path.as_deref().unwrap_or("")),
        ));
    }

    csv_response(csv, "tracks.csv")
}

async fn export_albums_csv(State(state): State<AppState>) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let albums = repo.list(999999, 0).unwrap_or_default();

    let mut csv =
        String::from("id,title,artist,year,genre,format,sample_rate,bit_depth,track_count\n");
    for a in &albums {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            a.id.unwrap_or(0),
            csv_escape(&a.title),
            csv_escape(a.artist_name.as_deref().unwrap_or("")),
            a.year.unwrap_or(0),
            csv_escape(a.genre.as_deref().unwrap_or("")),
            a.format.as_deref().unwrap_or(""),
            a.sample_rate.unwrap_or(0),
            a.bit_depth.unwrap_or(0),
            a.track_count.unwrap_or(0),
        ));
    }

    csv_response(csv, "albums.csv")
}

async fn export_artists_csv(State(state): State<AppState>) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artists = repo.list(999999, 0).unwrap_or_default();

    let mut csv = String::from("id,name,sort_name,musicbrainz_id\n");
    for a in &artists {
        csv.push_str(&format!(
            "{},{},{},{}\n",
            a.id.unwrap_or(0),
            csv_escape(&a.name),
            csv_escape(a.sort_name.as_deref().unwrap_or("")),
            a.musicbrainz_id.as_deref().unwrap_or(""),
        ));
    }

    csv_response(csv, "artists.csv")
}

fn csv_response(csv: String, filename: &str) -> Result<impl IntoResponse, AppError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        "Content-Disposition",
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|e| AppError::internal(e.to_string()))?,
    );
    Ok((StatusCode::OK, headers, csv))
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// GET /export/library-audit.csv?dir=/data/music/…
///
/// Le comparatif disque ↔ bibliothèque d'un répertoire, en CSV Excel FR
/// (BOM UTF-8, séparateur `;`). Une ligne par fichier du disque ET par piste
/// en base sous ce répertoire, classée : indexée / désynchronisée /
/// hors bibliothèque / fantôme — les avertissements du walker (montage
/// absent, dossier illisible) voyagent DANS le fichier, pour qu'un rapport
/// tronqué ne fasse jamais passer un montage débranché pour mille fantômes.
///
/// `dir` doit être l'une des racines musique configurées ou un
/// sous-dossier : on n'audite pas hors périmètre (et on ne laisse pas cette
/// route lire un chemin arbitraire du disque). Sans `dir`, toutes les
/// racines sont auditées.
async fn export_library_audit_csv(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use unicode_normalization::UnicodeNormalization;

    let music_dirs = crate::routes::system::get_music_dirs_list(&state.backend);
    let dirs: Vec<String> = match q.get("dir").map(|d| d.trim().trim_end_matches(['/', '\\'])) {
        Some(d) if !d.is_empty() => {
            let autorise = music_dirs
                .iter()
                .any(|root| crate::routes::system::scan::sous_le_dossier(d, root));
            if !autorise {
                return (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "dir hors des racines musique configurées ({})",
                        music_dirs.join(", ")
                    ),
                )
                    .into_response();
            }
            vec![d.to_string()]
        }
        _ => music_dirs.clone(),
    };
    if dirs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "aucune racine musique configurée".to_string(),
        )
            .into_response();
    }

    // Côté disque : le walker du scan, avec ses gardes (NAS gelé, montage
    // absent → missing_dirs, extensions ignorées comptées) et les mêmes
    // exclusions que le scan.
    let excludes = crate::auto_scan::scan_exclude_patterns(&state.backend);
    let liste = match tokio::task::spawn_blocking({
        let dirs = dirs.clone();
        move || tune_core::scanner::walker::list_audio_files_with_excludes(&dirs, &excludes)
    })
    .await
    {
        Ok(l) => l,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("parcours du disque interrompu: {e}"),
            )
                .into_response();
        }
    };

    let mut avertissements: Vec<String> = liste.missing_dir_reasons.clone();
    for e in &liste.error_dirs {
        avertissements.push(format!("{e}: erreur de lecture en cours de parcours"));
    }
    for (ext, n) in &liste.skipped_by_ext {
        let raison = liste
            .skipped_reasons
            .get(ext)
            .map(String::as_str)
            .unwrap_or("extension non lue");
        avertissements.push(format!("{n} fichier(s) .{ext} ignorés ({raison})"));
    }

    let mut disque: Vec<tune_core::library::audit::FichierDisque> = Vec::new();
    for p in &liste.files {
        let chemin: String = p.to_string_lossy().nfc().collect();
        let (taille, mtime) = std::fs::metadata(p)
            .map(|m| {
                (
                    m.len(),
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));
        disque.push(tune_core::library::audit::FichierDisque {
            chemin,
            taille,
            mtime,
        });
    }

    // Côté base : les pistes LOCALES sous ces racines. Les pistes streaming
    // (source non locale) n'ont pas de fichier : hors sujet.
    let mut bdd: Vec<tune_core::library::audit::PisteBdd> = Vec::new();
    for d in &dirs {
        let prefix = format!(
            "{}%",
            d.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let rows = state
            .backend
            .query_many(
                "SELECT t.id, t.file_path, t.title, COALESCE(ar.name, ''), COALESCE(al.title, ''), \
                 COALESCE(t.format, ''), COALESCE(t.file_size, 0), COALESCE(t.file_mtime, 0), \
                 COALESCE(t.audio_hash, '') \
                 FROM tracks t \
                 LEFT JOIN artists ar ON t.artist_id = ar.id \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 WHERE t.file_path LIKE ? ESCAPE '\\' AND (t.source IS NULL OR t.source = '' OR t.source = 'local')",
                &[&prefix],
            )
            .ou_defaut_journalise();
        for r in rows {
            bdd.push(tune_core::library::audit::PisteBdd {
                id: r.first().and_then(|v| v.as_i64()).unwrap_or(0),
                chemin: r
                    .get(1)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .nfc()
                    .collect(),
                titre: r.get(2).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                artiste: r.get(3).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                album: r.get(4).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                format: r.get(5).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                taille: r.get(6).and_then(|v| v.as_i64()).unwrap_or(0) as u64,
                mtime: r.get(7).and_then(|v| v.as_i64()).unwrap_or(0) as u64,
                hash: r.get(8).and_then(|v| v.as_str()).unwrap_or("").to_string(),
            });
        }
    }

    let lignes = tune_core::library::audit::classer(disque, bdd);

    // Réconciliation des déplacés : on ne hache QUE les fichiers hors
    // bibliothèque (64 Ko chacun, à 25 % du fichier — le hash du scan), et
    // seulement s'il existe au moins un fantôme à qui les comparer.
    let a_des_fantomes = lignes
        .iter()
        .any(|l| l.statut == tune_core::library::audit::Statut::Fantome);
    let candidats: Vec<String> = lignes
        .iter()
        .filter(|l| l.statut == tune_core::library::audit::Statut::HorsBibliotheque)
        .map(|l| l.chemin.clone())
        .collect();
    let hashes: std::collections::HashMap<String, String> =
        if a_des_fantomes && !candidats.is_empty() {
            tokio::task::spawn_blocking(move || {
                candidats
                    .into_iter()
                    .filter_map(|c| {
                        tune_core::scanner::hasher::compute_audio_hash_str(&c).map(|h| (c, h))
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
        } else {
            Default::default()
        };
    let lignes = tune_core::library::audit::apparier_deplaces(lignes, &hashes);

    let csv = tune_core::library::audit::rendre_csv(&lignes, &avertissements);
    match csv_response(csv, "audit-bibliotheque.csv") {
        Ok(r) => r.into_response(),
        Err(e) => e.into_response(),
    }
}
