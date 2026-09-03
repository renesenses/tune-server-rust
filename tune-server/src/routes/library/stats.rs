use axum::Json;
use axum::extract::{Query, State};
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::track_repo::TrackRepo;

use super::Pagination;

/// La ventilation par source des compteurs de bibliothèque (#2147).
///
/// ## Pourquoi cette réponse, et pas un filtre
///
/// Le testeur voyait deux nombres qui ne se rejoignaient pas : le compte de
/// pistes de l'écran Réglages → Bibliothèque et le `total_files` du rapport de
/// scan, 142 de moins. Ce ne sont pas deux mesures de la même chose. Le
/// rapport de scan compte des FICHIERS TROUVÉS SUR LE DISQUE ; le compteur,
/// lui, compte les LIGNES de `tracks`, où cohabitent le local et tout ce que
/// Qobuz, Tidal, la radio, les podcasts et Bandcamp ont posé. Une piste
/// Qobuz n'a aucun fichier à trouver : elle ne peut pas figurer dans un
/// rapport de scan, et pourtant elle est bien dans la bibliothèque.
///
/// Deux voies s'offraient :
///
/// **(a)** faire compter au tableau de bord la même population que le scan —
/// `WHERE source = 'local'`. Les deux écrans deviendraient comparables d'un
/// coup d'œil, mais le compte de pistes CHUTERAIT chez tout utilisateur de
/// streaming, en retirant de l'affichage des pistes qu'il possède vraiment et
/// qu'il peut jouer. On répondrait à « pourquoi deux nombres ? » en faisant
/// disparaître l'un des deux, et une bibliothèque de 12 000 pistes annoncerait
/// soudain 11 858 sans que rien n'ait été perdu. C'est un mensonge par
/// soustraction, et il coûterait un signalement de perte de données.
///
/// **(b) — retenue.** Garder le total, et NOMMER ce qu'il contient. Le total
/// reste juste ; s'y ajoute la ventilation qui explique l'écart. Le testeur
/// n'a plus deux nombres inexplicables mais une décomposition : « 12 000
/// pistes, dont 11 858 locales, 98 Qobuz, 40 Tidal, 4 radio » — et 11 858 est
/// exactement ce que le rapport de scan lui montre. Rien n'est retiré, aucun
/// champ existant ne change de valeur, et la question posée reçoit sa réponse.
///
/// Ce choix s'appuie sur un constat mesuré : **aucune route du serveur
/// n'exposait de décompte par source**. `GROUP BY source` n'avait qu'une seule
/// occurrence dans tout le dépôt (`history_repo.rs`, l'historique d'écoute,
/// sans rapport). Ce n'était pas un filtre qui manquait, c'était le chiffre.
///
/// ## Ce que cette structure n'est pas
///
/// Ce n'est pas un canal d'état de plus : c'est la réponse de la route qui
/// porte le compte, à l'endroit exact où le client lit déjà `tracks` et
/// `albums`. L'ajout est PUREMENT ADDITIF — `tracks`, `albums`, `artists`
/// gardent la valeur qu'ils avaient hier.
pub(crate) struct VentilationParSource {
    pistes: Vec<(String, i64)>,
    albums: Vec<(String, i64)>,
}

impl VentilationParSource {
    /// Lit les deux ventilations. Une erreur SQL ne fait pas tomber la route :
    /// elle rend une ventilation VIDE, et les champs existants continuent de
    /// répondre. Un tableau de bord qui perd sa décomposition reste lisible ;
    /// un tableau de bord en 500 ne l'est pas.
    pub(crate) fn lire(state: &AppState) -> Self {
        Self {
            pistes: TrackRepo::with_backend(state.backend.clone())
                .count_by_source()
                .unwrap_or_default(),
            albums: AlbumRepo::with_backend(state.backend.clone())
                .count_by_source()
                .unwrap_or_default(),
        }
    }

    fn objet(seaux: &[(String, i64)]) -> Value {
        Value::Object(
            seaux
                .iter()
                .map(|(source, compte)| (source.clone(), json!(compte)))
                .collect(),
        )
    }

    fn local(seaux: &[(String, i64)]) -> i64 {
        seaux
            .iter()
            .find(|(source, _)| source == "local")
            .map(|(_, compte)| *compte)
            .unwrap_or(0)
    }

    /// Les quatre champs à épingler dans la réponse d'une route de comptage.
    /// Un seul point de vérité : `/library/stats` et `/system/stats` affichent
    /// les mêmes compteurs sur deux écrans, ils doivent les ventiler pareil.
    pub(crate) fn champs(&self) -> Vec<(String, Value)> {
        vec![
            ("tracks_by_source".to_string(), Self::objet(&self.pistes)),
            ("tracks_local".to_string(), json!(Self::local(&self.pistes))),
            ("albums_by_source".to_string(), Self::objet(&self.albums)),
            ("albums_local".to_string(), json!(Self::local(&self.albums))),
        ]
    }
}

/// Ajoute les champs de ventilation à un objet JSON déjà construit.
/// Sans effet si la valeur n'est pas un objet — la route répond quand même.
pub(crate) fn ajouter_ventilation(cible: &mut Value, ventilation: &VentilationParSource) {
    if let Some(objet) = cible.as_object_mut() {
        for (nom, valeur) in ventilation.champs() {
            objet.insert(nom, valeur);
        }
    }
}

pub(super) async fn library_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let b = &state.backend;
    let (dur_col, size_col) = match b.engine() {
        tune_core::db::engine::Engine::Sqlite => ("duration_ms", "file_size"),
        tune_core::db::engine::Engine::Postgres => {
            ("CAST(duration_ms AS bigint)", "CAST(file_size AS bigint)")
        }
    };
    let sql = format!(
        "SELECT \
         (SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)), \
         (SELECT COUNT(*) FROM albums), \
         (SELECT COUNT(*) FROM tracks), \
         (SELECT COUNT(*) FROM zones), \
         COALESCE(CAST((SELECT SUM({dur_col}) FROM tracks) AS bigint), 0), \
         COALESCE(CAST((SELECT SUM({size_col}) FROM tracks WHERE file_size IS NOT NULL) AS bigint), 0)"
    );
    let row = b
        .query_one(&sql, &[])
        .map_err(|e| AppError::internal(e))?
        .unwrap_or_default();

    let artists = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let albums = row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
    let tracks = row.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
    let zones = row.get(3).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_duration_ms = row.get(4).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_size_bytes = row.get(5).and_then(|v| v.as_i64()).unwrap_or(0);

    let listens = b
        .query_one("SELECT COUNT(*) FROM listen_history", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);

    // `tracks` et `albums` restent le compte ENTIER de la table, toutes
    // sources confondues — c'est la taille réelle de la bibliothèque, et la
    // changer retirerait de l'écran des pistes que l'utilisateur possède
    // (#2147). Ce qui manquait n'était pas un filtre mais la décomposition qui
    // rend l'écart au rapport de scan lisible : elle s'ajoute ici.
    let mut corps = json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
        "listens": listens,
        "zones": zones,
        "total_duration_ms": total_duration_ms,
        "total_size_bytes": total_size_bytes,
    });
    ajouter_ventilation(&mut corps, &VentilationParSource::lire(&state));
    Ok(Json(corps))
}

pub(super) async fn completeness_stats(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // Go through state.backend, NOT state.db: state.db is always the raw
    // SQLite handle, which on a PostgreSQL deployment is a leftover empty
    // database — every count read 0, so the four Metadata cards all showed
    // the same "0/0 = 100%" figure (Fabien-4, v0.9.13). Same engine routing
    // as library_stats above; the SQL itself is dialect-neutral.
    let b = &state.backend;
    let sql = "SELECT \
         (SELECT COUNT(*) FROM tracks), \
         (SELECT COUNT(*) FROM tracks WHERE genre IS NOT NULL AND genre != ''), \
         (SELECT COUNT(*) FROM tracks WHERE year IS NOT NULL), \
         (SELECT COUNT(*) FROM tracks WHERE artist_id IS NOT NULL), \
         (SELECT COUNT(*) FROM tracks WHERE album_id IS NOT NULL), \
         (SELECT COUNT(DISTINCT a.id) FROM albums a WHERE a.cover_path IS NOT NULL AND a.cover_path != ''), \
         (SELECT COUNT(*) FROM albums), \
         (SELECT COUNT(*) FROM tracks WHERE musicbrainz_recording_id IS NOT NULL AND musicbrainz_recording_id != ''), \
         (SELECT COUNT(*) FROM albums WHERE genre IS NOT NULL AND genre != ''), \
         (SELECT COUNT(*) FROM albums WHERE year IS NOT NULL AND year > 0), \
         (SELECT COUNT(*) FROM albums al LEFT JOIN artists ar ON ar.id = al.artist_id \
          WHERE al.artist_id IS NULL OR ar.name IS NULL OR ar.name = '' OR ar.name = 'Unknown Artist'), \
         (SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)), \
         (SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL) \
          AND (image_path IS NULL OR image_path = ''))";
    let row = b
        .query_one(sql, &[])
        .map_err(AppError::internal)?
        .unwrap_or_default();
    let get = |i: usize| row.get(i).and_then(|v| v.as_i64()).unwrap_or(0);

    // Column comments preserved from the per-query version:
    // - albums_with_genre/year count the ALBUM's own genre/year column — the
    //   exact field the Metadata view displays, edits and filters on
    //   (Reivax66, #1091: track-based counts never matched the album list).
    // - albums_without_artist shares the total_albums denominator so the four
    //   cards show consistent X/Y (Fabien, v0.9.4).
    // - total_artists counts only album-artists — the set the library shows
    //   (Bilou: 1808 vs 1505 real artists).
    let total_tracks = get(0);
    let with_genre = get(1);
    let with_year = get(2);
    let with_artist = get(3);
    let with_album = get(4);
    let with_cover = get(5);
    let total_albums = get(6);
    let with_mbid = get(7);
    let albums_with_genre = get(8);
    let albums_with_year = get(9);
    let albums_without_artist = get(10);
    let total_artists = get(11);
    let artists_without_image = get(12);
    // Le client affiche ce nombre dans la pastille « Métadonnées douteuses ».
    // Réutiliser le compteur de la route `/metadata/doubtful` garantit que la
    // pastille et la liste comptent exactement la même population (#1897).
    let doubtful_count = TrackRepo::with_backend(state.backend.clone())
        .count_doubtful()
        .map_err(|erreur| AppError::internal(erreur.to_string()))?;

    let genre_pct = if total_tracks > 0 {
        with_genre as f64 / total_tracks as f64 * 100.0
    } else {
        0.0
    };
    let year_pct = if total_tracks > 0 {
        with_year as f64 / total_tracks as f64 * 100.0
    } else {
        0.0
    };
    let artist_pct = if total_tracks > 0 {
        with_artist as f64 / total_tracks as f64 * 100.0
    } else {
        0.0
    };
    let cover_pct = if total_albums > 0 {
        with_cover as f64 / total_albums as f64 * 100.0
    } else {
        0.0
    };
    let mbid_pct = if total_tracks > 0 {
        with_mbid as f64 / total_tracks as f64 * 100.0
    } else {
        0.0
    };

    // Weighted health score: cover(30%) + genre(25%) + year(20%) + mbid(15%) + artist(10%)
    let health_score = (cover_pct * 0.30
        + genre_pct * 0.25
        + year_pct * 0.20
        + mbid_pct * 0.15
        + artist_pct * 0.10)
        .round();

    let grade = match health_score as u32 {
        90..=100 => "A",
        75..=89 => "B",
        50..=74 => "C",
        25..=49 => "D",
        _ => "F",
    };

    Ok(Json(json!({
        "total_tracks": total_tracks,
        "total_albums": total_albums,
        "total_artists": total_artists,
        "with_genre": with_genre,
        "with_year": with_year,
        "with_artist": with_artist,
        "with_album": with_album,
        "with_cover": with_cover,
        "with_musicbrainz_id": with_mbid,
        "albums_without_cover": total_albums - with_cover,
        "albums_without_genre": total_albums - albums_with_genre,
        "albums_without_year": total_albums - albums_with_year,
        "tracks_without_artist": total_tracks - with_artist,
        "albums_without_artist": albums_without_artist,
        "artists_without_image": artists_without_image,
        "doubtful_count": doubtful_count,
        "genre_pct": genre_pct.round(),
        "year_pct": year_pct.round(),
        "artist_pct": artist_pct.round(),
        "album_pct": if total_tracks > 0 { (with_album as f64 / total_tracks as f64 * 100.0).round() } else { 0.0 },
        "cover_pct": cover_pct.round(),
        "mbid_pct": mbid_pct.round(),
        "health_score": health_score,
        "health_grade": grade,
    })))
}

pub(super) async fn library_activity(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items = repo.recent(limit).unwrap_or_default();
    Json(json!(items))
}
