//! Storage + similarity for CLAP audio embeddings — the READ side of the
//! acoustic Smart Radio. Pure: no onnxruntime, always compiled. Only the WRITE
//! side (computing embeddings, `embedding.rs`) needs `ort` and is feature-gated,
//! so any build can rank by acoustic similarity over vectors an embedding-
//! enabled instance produced.

use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};

/// Dimensionality of the CLAP audio embedding.
pub const EMBED_DIM: usize = 512;

/// Model identifier stored alongside each embedding, so a future model can
/// re-embed without invalidating rows produced by this one. The write sweep
/// keys its "already analysed" sentinel on this value, so bumping the ID makes
/// an embedding-enabled instance re-sweep the whole library into the new space
/// (old `clap-audio-2023` vectors are silently superseded track-by-track).
///
/// `clap-music-2023` = LAION `music_audioset` towers (HTSAT-base), the
/// music-specialised checkpoint. It replaces the generalist `clap-audio-2023`
/// (`630k-audioset`, HTSAT-tiny) and shares its joint space with the CLAP text
/// tower, enabling natural-language acoustic search (Phase 3).
// Le suffixe `-pcm2` n'est pas un nouveau modèle : c'est la VERSION DU
// PIPELINE d'entrée. Les vecteurs produits avant #1508+#1498 l'ont été sur du
// stéréo entrelacé à la cadence source présenté comme du mono 48 kHz — ils ne
// vivent pas dans le même espace que les vecteurs corrects. Changer cette
// chaîne invalide la sentinelle `audio_embed_analyzed` et fait ré-analyser la
// bibliothèque une fois, proprement.
pub const MODEL_ID: &str = "clap-music-2023-pcm2";

/// Pack a normalised embedding into the `BLOB`/`BYTEA` column (little-endian f32).
pub fn to_bytes(embedding: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(embedding.len() * 4);
    for x in embedding {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Reverse of [`to_bytes`].
pub fn from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity of two already-normalised embeddings (a plain dot product).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The stored embedding for one track, if analysed.
pub fn fetch_one(backend: &Arc<dyn DbBackend>, track_id: i64) -> Option<Vec<f32>> {
    let rows = backend
        .query_many(
            "SELECT embedding FROM track_audio_embedding WHERE track_id = ? LIMIT 1",
            &[&track_id as &dyn ToSqlValue],
        )
        .ok()?;
    let blob = rows.first()?.first()?.as_blob()?;
    let v = from_bytes(blob);
    (v.len() == EMBED_DIM).then_some(v)
}

/// All (track_id, embedding) pairs. Bounded by `limit`; a typical audiophile
/// library is 20-200k tracks and each vector is 2 KB, so an in-memory
/// brute-force cosine over the set is a few ms — no vector index needed yet.
pub fn fetch_all(backend: &Arc<dyn DbBackend>, limit: i64) -> Vec<(i64, Vec<f32>)> {
    let rows = match backend.query_many(
        "SELECT track_id, embedding FROM track_audio_embedding LIMIT ?",
        &[&limit as &dyn ToSqlValue],
    ) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.iter()
        .filter_map(|r| {
            let id = r.first()?.as_i64()?;
            let v = from_bytes(r.get(1)?.as_blob()?);
            (v.len() == EMBED_DIM).then_some((id, v))
        })
        .collect()
}

/// Nombre de pistes déjà analysées acoustiquement.
///
/// Sert à distinguer « la bibliothèque n'est pas encore analysée » de « aucun
/// résultat pour cette recherche » : sans cette distinction, une recherche par
/// ambiance renvoyait une liste vide dans les deux cas et l'utilisateur ne
/// pouvait pas savoir s'il devait reformuler ou attendre (retour Fabien).
pub fn analysed_count(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one("SELECT COUNT(*) FROM track_audio_embedding", &[])
        .ok()
        .flatten()
        .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Combien de pistes la passe acoustique peut-elle analyser en tout — le
/// dénominateur honnête d'une barre de progression.
///
/// Ce n'est PAS le nombre de pistes de la bibliothèque : la passe ignore les
/// pistes sans fichier local et saute le DSD (le rééchantillonneur DSD→PCM peut
/// tourner en boucle sur certains repiquages SACD). Compter toutes les pistes
/// donnerait une jauge qui n'atteint jamais 100 % sur une discothèque qui
/// contient du DSD, et personne ne saurait pourquoi.
///
/// Les conditions ci-dessous doivent rester le miroir exact de la requête de
/// candidats de `analyze_embedding_batch`, moins le `NOT EXISTS`.
pub fn eligible_count(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one(
            "SELECT COUNT(*) FROM tracks t \
             WHERE t.file_path IS NOT NULL AND t.file_path != '' \
               AND (t.format IS NULL OR \
                    lower(t.format) NOT IN ('dsd', 'dsf', 'dff', 'dsdiff'))",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Rank the library by cosine similarity to an arbitrary (already-normalised)
/// query vector, most similar first. `exclude` drops one track id (the seed,
/// when querying by track). Returns `(track_id, cosine)`. Works for any query in
/// the CLAP joint space — a seed track's audio embedding OR a text-tower query
/// embedding (natural-language acoustic search), since both share that space.
pub fn rank_by_vector(
    backend: &Arc<dyn DbBackend>,
    query: &[f32],
    limit: usize,
    exclude: Option<i64>,
) -> Vec<(i64, f32)> {
    let mut scored: Vec<(i64, f32)> = fetch_all(backend, 500_000)
        .into_iter()
        .filter(|(id, _)| Some(*id) != exclude)
        .map(|(id, v)| (id, cosine(query, &v)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    scored
}

/// Rank the library by acoustic similarity to a seed track's embedding, most
/// similar first, excluding the seed itself. Returns `(track_id, cosine)`; empty
/// when the seed has no embedding (caller falls back to the metadata path).
pub fn acoustic_neighbors(
    backend: &Arc<dyn DbBackend>,
    seed_track_id: i64,
    limit: usize,
) -> Vec<(i64, f32)> {
    let seed = match fetch_one(backend, seed_track_id) {
        Some(v) => v,
        None => return Vec::new(),
    };
    rank_by_vector(backend, &seed, limit, Some(seed_track_id))
}
