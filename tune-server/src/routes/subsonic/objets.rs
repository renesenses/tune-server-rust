//! Les objets que rendent les méthodes de navigation (#7945, étape 2) :
//! `artistID3`, `albumID3` et `child` de l'API Subsonic, avec les champs
//! OpenSubsonic utiles. Un seul constructeur par objet, partagé par toutes
//! les méthodes, pour qu'un même album ou une même piste ait la même forme
//! partout (getAlbum, getAlbumList2, search3…).
//!
//! Seule la bibliothèque LOCALE est exposée : un client Subsonic lit les
//! octets par `stream`, que Tune ne sait servir que pour un fichier. Les
//! albums masqués (#1391) ne paraissent pas.
//!
//! [`Garde`] applique la politique parentale (#7856) du profil de la clé :
//! `/rest` est monté hors de la couche qui filtre les listes de `/api/v1`.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::models::{Album, Track};
use tune_core::db::politique_parentale as pp;

use super::identifiants::{Genre, encoder};
use crate::state::AppState;

/// Une piste exposée : locale et munie d'un fichier.
pub const PISTE_EXPOSEE: &str =
    "COALESCE(t.source, 'local') = 'local' AND t.file_path IS NOT NULL AND t.file_path <> ''";

/// Une piste exposée et visible (alias `t`) : [`PISTE_EXPOSEE`], hors des
/// albums masqués.
pub fn piste_visible() -> String {
    format!(
        "{PISTE_EXPOSEE} AND {}",
        tune_core::db::facet_filter::hidden_tracks_excluded()
    )
}

/// Un album exposé (alias `a`) : local, non masqué, avec au moins une piste
/// exposée.
pub fn album_expose() -> String {
    format!(
        "COALESCE(a.source, 'local') = 'local' AND {} AND EXISTS (SELECT 1 FROM tracks t \
         WHERE t.album_id = a.id AND {PISTE_EXPOSEE})",
        tune_core::db::facet_filter::hidden_albums_excluded()
    )
}

/// Les identifiants rendus par une requête dont la première colonne est un id.
pub fn ids(state: &AppState, sql: &str, params: &[&dyn ToSqlValue]) -> Vec<i64> {
    match state.backend.query_many(sql, params) {
        Ok(lignes) => lignes
            .iter()
            .filter_map(|l| l.first().and_then(|v| v.as_i64()))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "subsonic_requete_echouee");
            Vec::new()
        }
    }
}

/// La politique parentale du profil, prête à juger un objet.
#[derive(Default)]
pub struct Garde {
    ids: HashSet<i64>,
    noms: HashSet<String>,
}

impl Garde {
    /// La garde du profil ; une politique illisible bloque tout plutôt que de
    /// laisser passer la bibliothèque entière.
    pub fn du_profil(state: &AppState, profile_id: i64) -> Result<Self, String> {
        let politique = match pp::lire(&state.backend, profile_id) {
            Ok(Some(p)) => p.politique,
            Ok(None) => return Ok(Self::default()),
            Err(e) if pp::table_absente(&e) => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        Ok(Garde {
            ids: politique
                .artistes_bloques
                .iter()
                .filter_map(|a| a.id)
                .collect(),
            noms: politique
                .artistes_bloques
                .iter()
                .filter_map(|a| a.nom.as_deref().map(pp::normaliser))
                .filter(|n| !n.is_empty())
                .collect(),
        })
    }

    fn bloque(&self, id: Option<i64>, nom: Option<&str>) -> bool {
        id.is_some_and(|i| self.ids.contains(&i))
            || nom.is_some_and(|n| self.noms.contains(&pp::normaliser(n)))
    }

    pub fn artiste_bloque(&self, id: i64, nom: &str) -> bool {
        self.bloque(Some(id), Some(nom))
    }

    pub fn album_bloque(&self, a: &Album) -> bool {
        self.bloque(a.artist_id, a.artist_name.as_deref())
    }

    pub fn piste_bloquee(&self, t: &Track) -> bool {
        self.bloque(t.artist_id, t.artist_name.as_deref())
            || self.bloque(None, t.album_artist.as_deref())
    }
}

/// `artistID3`.
pub fn artiste(id: i64, nom: &str, tri: Option<&str>, nb_albums: i64) -> Value {
    let mut o = json!({
        "id": encoder(Genre::Artiste, id),
        "name": nom,
        "coverArt": encoder(Genre::Artiste, id),
        "albumCount": nb_albums,
    });
    if let Some(t) = tri.filter(|t| !t.trim().is_empty()) {
        o["sortName"] = json!(t);
    }
    o
}

/// Ce qu'un album tire de ses pistes exposées : nombre, durée, date d'entrée.
#[derive(Default, Clone, Copy)]
pub struct Totaux {
    pub pistes: i64,
    pub duree_ms: i64,
    pub entree: Option<f64>,
}

/// Les totaux des albums donnés, en une requête.
pub fn totaux(state: &AppState, albums: &[i64]) -> HashMap<i64, Totaux> {
    if albums.is_empty() {
        return HashMap::new();
    }
    let liste = albums
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT t.album_id, COUNT(*), SUM(t.duration_ms), MIN(t.file_mtime) FROM tracks t \
         WHERE t.album_id IN ({liste}) AND {PISTE_EXPOSEE} GROUP BY t.album_id"
    );
    let lignes = state.backend.query_many(&sql, &[]).unwrap_or_default();
    lignes
        .iter()
        .filter_map(|l| {
            Some((
                l.first()?.as_i64()?,
                Totaux {
                    pistes: l.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                    duree_ms: l.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                    entree: l.get(3).and_then(|v| v.as_f64()),
                },
            ))
        })
        .collect()
}

fn date_iso(secondes: Option<f64>) -> String {
    secondes
        .and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// `albumID3`.
pub fn album(a: &Album, t: Totaux) -> Value {
    let id = a.id.unwrap_or_default();
    let mut o = Map::new();
    o.insert("id".into(), json!(encoder(Genre::Album, id)));
    o.insert("name".into(), json!(a.title));
    o.insert("title".into(), json!(a.title));
    if let Some(nom) = &a.artist_name {
        o.insert("artist".into(), json!(nom));
        o.insert("displayArtist".into(), json!(nom));
    }
    if let Some(ar) = a.artist_id {
        o.insert("artistId".into(), json!(encoder(Genre::Artiste, ar)));
    }
    o.insert("coverArt".into(), json!(encoder(Genre::Album, id)));
    o.insert("songCount".into(), json!(t.pistes));
    o.insert("duration".into(), json!(t.duree_ms / 1000));
    o.insert("created".into(), json!(date_iso(t.entree)));
    if let Some(y) = a.year.filter(|y| *y > 0) {
        o.insert("year".into(), json!(y));
    }
    if let Some(g) = a.genre.as_deref().filter(|g| !g.is_empty()) {
        o.insert("genre".into(), json!(g));
    }
    if let Some(mb) = a
        .musicbrainz_release_id
        .as_deref()
        .filter(|m| !m.is_empty())
    {
        o.insert("musicBrainzId".into(), json!(mb));
    }
    o.insert("isCompilation".into(), json!(a.is_compilation));
    Value::Object(o)
}

/// `child` d'une piste.
pub fn piste(t: &Track) -> Value {
    let id = t.id.unwrap_or_default();
    let suffixe = t
        .format
        .clone()
        .or_else(|| {
            t.file_path
                .as_deref()
                .and_then(|p| std::path::Path::new(p).extension())
                .map(|e| e.to_string_lossy().to_lowercase())
        })
        .unwrap_or_default()
        .to_lowercase();
    let type_mime = tune_core::audio::formats::AudioFormat::from_extension(&suffixe)
        .map(|f| f.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".into());
    let mut o = Map::new();
    o.insert("id".into(), json!(encoder(Genre::Piste, id)));
    o.insert("isDir".into(), json!(false));
    o.insert("title".into(), json!(t.title));
    if let Some(al) = t.album_id {
        o.insert("parent".into(), json!(encoder(Genre::Album, al)));
        o.insert("albumId".into(), json!(encoder(Genre::Album, al)));
        o.insert("coverArt".into(), json!(encoder(Genre::Album, al)));
    }
    if let Some(titre) = &t.album_title {
        o.insert("album".into(), json!(titre));
    }
    if let Some(nom) = &t.artist_name {
        o.insert("artist".into(), json!(nom));
        o.insert("displayArtist".into(), json!(nom));
    }
    if let Some(ar) = t.artist_id {
        o.insert("artistId".into(), json!(encoder(Genre::Artiste, ar)));
    }
    if t.track_number > 0 {
        o.insert("track".into(), json!(t.track_number));
    }
    if t.disc_number > 0 {
        o.insert("discNumber".into(), json!(t.disc_number));
    }
    if let Some(y) = t.year.filter(|y| *y > 0) {
        o.insert("year".into(), json!(y));
    }
    if let Some(g) = t.genre.as_deref().filter(|g| !g.is_empty()) {
        o.insert("genre".into(), json!(g));
    }
    if let Some(taille) = t.file_size.filter(|s| *s > 0) {
        o.insert("size".into(), json!(taille));
    }
    o.insert("contentType".into(), json!(type_mime));
    o.insert("suffix".into(), json!(suffixe));
    o.insert("duration".into(), json!(t.duration_ms / 1000));
    if let Some(debit) = t.bitrate_kbps.filter(|d| *d > 0) {
        o.insert("bitRate".into(), json!(debit));
    }
    if let Some(sr) = t.sample_rate.filter(|s| *s > 0) {
        o.insert("samplingRate".into(), json!(sr));
    }
    if let Some(bd) = t.bit_depth.filter(|b| *b > 0) {
        o.insert("bitDepth".into(), json!(bd));
    }
    if t.channels > 0 {
        o.insert("channelCount".into(), json!(t.channels));
    }
    if let Some(mb) = t
        .musicbrainz_recording_id
        .as_deref()
        .filter(|m| !m.is_empty())
    {
        o.insert("musicBrainzId".into(), json!(mb));
    }
    o.insert("type".into(), json!("music"));
    o.insert("mediaType".into(), json!("song"));
    Value::Object(o)
}

/// Une piste est-elle exposée ? (le même critère que [`PISTE_EXPOSEE`]).
pub fn piste_exposee(t: &Track) -> bool {
    (t.source.is_empty() || t.source == "local")
        && t.file_path.as_deref().is_some_and(|p| !p.is_empty())
}

/// Les albums donnés, dans l'ordre donné, en `albumID3`, ceux que la garde
/// bloque retirés.
pub fn albums(state: &AppState, garde: &Garde, ordre: &[i64]) -> Vec<Value> {
    let repo = tune_core::db::album_repo::AlbumRepo::with_backend(state.backend.clone());
    let charges: Vec<Album> = ordre
        .iter()
        .filter_map(|id| repo.get(*id).ok().flatten())
        .filter(|a| !garde.album_bloque(a))
        .collect();
    let ids: Vec<i64> = charges.iter().filter_map(|a| a.id).collect();
    let totaux = totaux(state, &ids);
    charges
        .iter()
        .map(|a| {
            album(
                a,
                totaux
                    .get(&a.id.unwrap_or_default())
                    .copied()
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// Les pistes données, dans l'ordre donné, en `child`, celles que la garde
/// bloque retirées.
pub fn pistes(state: &AppState, garde: &Garde, ordre: &[i64]) -> Vec<Value> {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut par_id: HashMap<i64, Track> = repo
        .list_by_ids(ordre)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| Some((t.id?, t)))
        .collect();
    ordre
        .iter()
        .filter_map(|id| par_id.remove(id))
        .filter(|t| piste_exposee(t) && !garde.piste_bloquee(t))
        .map(|t| piste(&t))
        .collect()
}
