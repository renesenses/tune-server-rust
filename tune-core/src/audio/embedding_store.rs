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
///
/// ⚠️ **Filtré sur le modèle courant.** Deux modèles produisent deux espaces
/// vectoriels sans rapport : un cosinus entre un vecteur `clap-music-2023` et un
/// vecteur `clap-music-2023-pcm2` ne veut rien dire. Sans ce filtre, après un
/// changement de modèle, le classement mélangeait les deux — mesuré sur .18 le
/// 16/08 : 16 782 vecteurs de l'ancien espace contre 8 257 du nouveau, comparés
/// entre eux (#1819, et piste sérieuse pour #1820).
///
/// Conséquence assumée : juste après un changement de modèle, la recherche par
/// ambiance ne porte que sur les pistes déjà ré-analysées. Moins de résultats,
/// mais des résultats qui veulent dire quelque chose ; les autres reviennent au
/// fil de la passe.
pub fn fetch_all(backend: &Arc<dyn DbBackend>, limit: i64) -> Vec<(i64, Vec<f32>)> {
    let rows = match backend.query_many(
        "SELECT track_id, embedding FROM track_audio_embedding \
         WHERE model = ? LIMIT ?",
        &[&MODEL_ID as &dyn ToSqlValue, &limit as &dyn ToSqlValue],
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
/// ⚠️ Compte les embeddings **du modèle courant**, et pas les lignes de la
/// table. Sans ce filtre, la jauge était figée par construction (#1819) :
/// `track_audio_embedding` a `track_id` pour clé primaire, donc ré-analyser une
/// piste **écrase** sa ligne au lieu d'en ajouter une. Après un changement de
/// modèle, le total ne bougeait plus d'un pouce pendant que la passe
/// retravaillait toute la discothèque.
///
/// Mesuré sur la machine de Bertrand (.18) le 16/08 : 16 782 embeddings sous
/// `clap-music-2023` et 8 257 sous `clap-music-2023-pcm2`. La jauge affichait
/// 25 039 / 25 090 — 99,8 % — alors qu'il restait 16 832 pistes à ré-analyser.
pub fn analysed_count(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one(
            "SELECT COUNT(*) FROM track_audio_embedding WHERE model = ?",
            &[&MODEL_ID as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Pistes **traitées** par la passe pour le modèle courant : celles qui portent
/// le témoin `audio_embed_analyzed` estampillé du `MODEL_ID` en cours.
///
/// C'est le numérateur honnête d'une barre de progression, et il diffère de
/// [`analysed_count`] : le témoin est posé même quand l'inférence échoue, pour
/// qu'une piste en échec ne soit pas retentée sans fin. La différence entre les
/// deux, ce sont les échecs — voir [`failed_count`].
///
/// Le filtre sur la valeur est indispensable : un changement de modèle rend
/// toutes les pistes candidates à nouveau, et un compteur qui ignorerait la
/// valeur afficherait 100 % pendant que la passe recommence tout.
pub fn processed_count(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one(
            "SELECT COUNT(*) FROM track_metadata \
             WHERE key = 'audio_embed_analyzed' AND value = ?",
            &[&MODEL_ID as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Pistes que la passe a traitées sans parvenir à en tirer un embedding.
///
/// Ni un blocage ni une file d'attente : ces pistes sont **finies**, elles ont
/// échoué. Les taire fabriquait une jauge qui n'atteignait jamais 100 % et
/// poussait à relancer indéfiniment une passe terminée.
pub fn failed_count(backend: &Arc<dyn DbBackend>) -> i64 {
    (processed_count(backend) - analysed_count(backend)).max(0)
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

/// Héritage d'embeddings entre éditions d'une même piste (#1732, phase 1).
///
/// Le DSD est exclu de l'analyse acoustique (le rééchantillonneur boucle sur
/// certains repiquages SACD) : une piste DSD n'a donc JAMAIS de vecteur et ne
/// remonte dans aucune ambiance. Quand la bibliothèque contient la même piste
/// dans un format analysable (FLAC…), on copie son embedding : même
/// enregistrement, même acoustique.
///
/// Jumelle = titre + artiste normalisés via [`crate::library::track_matcher::normalize`]
/// (casse, accents, suffixes d'édition « (Remastered) »/« (Deluxe) »/feat.,
/// blancs surnuméraires) identiques ET durée à ±1 s. C'est la lettre de
/// l'issue : un repiquage SACD est presque toujours étiqueté avec un suffixe
/// d'édition que la FLAC ne porte pas — un simple lower/trim ne les
/// appariait pas. La durée à ±1 s reste le discriminant qui écarte les
/// autres versions (live/edit) une fois les suffixes tombés.
/// La copie porte `source = 'inherited:<id>'` ; seuls les vecteurs ANALYSÉS
/// du modèle courant servent de source (jamais d'héritage en chaîne). Au
/// changement de modèle, les hérités du vieux modèle sont purgés puis
/// recopiés depuis les sources ré-analysées — sans purge, ils mélangeraient
/// deux espaces vectoriels dans le même classement.
///
/// Idempotent et borné : ne recopie jamais par-dessus un embedding existant,
/// ne charge aucun blob tant qu'aucune cible n'attend.
pub fn inherit_from_local_twins(backend: &Arc<dyn DbBackend>) -> u64 {
    // 1. Purge des hérités d'un autre espace vectoriel (bump de modèle).
    let purge_params: [&dyn ToSqlValue; 1] = [&MODEL_ID];
    let _ = backend.execute(
        "DELETE FROM track_audio_embedding \
         WHERE source LIKE 'inherited:%' AND model != ?",
        &purge_params,
    );

    // 2. Cibles : pistes des formats exclus, sans embedding.
    let targets = match backend.query_many(
        "SELECT t.id, t.title, coalesce(a.name, ''), t.duration_ms \
         FROM tracks t LEFT JOIN artists a ON a.id = t.artist_id \
         WHERE lower(t.format) IN ('dsd', 'dsf', 'dff', 'dsdiff') \
           AND NOT EXISTS (SELECT 1 FROM track_audio_embedding e WHERE e.track_id = t.id)",
        &[],
    ) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    if targets.is_empty() {
        return 0;
    }

    // 3. Sources : vecteurs ANALYSÉS du modèle courant (métadonnées seules,
    //    pas les blobs — on ne les lit qu'au moment de copier).
    let sources = match backend.query_many(
        "SELECT e.track_id, t.title, coalesce(a.name, ''), \
                t.duration_ms, e.analyzed_at \
         FROM track_audio_embedding e \
         JOIN tracks t ON t.id = e.track_id \
         LEFT JOIN artists a ON a.id = t.artist_id \
         WHERE e.model = ? AND e.source IS NULL",
        &purge_params,
    ) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let normalize = crate::library::track_matcher::normalize;
    let mut by_key: std::collections::HashMap<(String, String), Vec<(i64, i64, i64)>> =
        std::collections::HashMap::new();
    for r in &sources {
        let (Some(id), Some(title), Some(artist), Some(dur)) = (
            r.first().and_then(|v| v.as_i64()),
            r.get(1).and_then(|v| v.as_string()),
            r.get(2).and_then(|v| v.as_string()),
            r.get(3).and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        let at = r.get(4).and_then(|v| v.as_i64()).unwrap_or(0);
        by_key
            .entry((normalize(&title), normalize(&artist)))
            .or_default()
            .push((id, dur, at));
    }

    let mut inherited = 0u64;
    for r in &targets {
        let (Some(target_id), Some(title), Some(artist), Some(dur)) = (
            r.first().and_then(|v| v.as_i64()),
            r.get(1).and_then(|v| v.as_string()),
            r.get(2).and_then(|v| v.as_string()),
            r.get(3).and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        let (title, artist) = (normalize(&title), normalize(&artist));
        if title.is_empty() {
            continue;
        }
        // Meilleure jumelle = écart de durée minimal, sous ±1 s.
        let Some(&(src_id, _, analyzed_at)) = by_key.get(&(title, artist)).and_then(|c| {
            c.iter()
                .filter(|(_, d, _)| (d - dur).abs() <= 1000)
                .min_by_key(|(_, d, _)| (d - dur).abs())
        }) else {
            continue;
        };
        let Some(embedding) = fetch_one(backend, src_id) else {
            continue;
        };
        let blob = Some(to_bytes(&embedding));
        let provenance = format!("inherited:{src_id}");
        let params: [&dyn ToSqlValue; 5] =
            [&target_id, &MODEL_ID, &blob, &analyzed_at, &provenance];
        let ok = backend.execute(
            "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at, source) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT (track_id) DO NOTHING",
            &params,
        );
        if ok.is_ok() {
            inherited += 1;
        }
    }
    inherited
}

#[cfg(test)]
mod inherit_tests {
    use super::*;
    use crate::db::models::Track;
    use crate::db::sqlite::SqliteDb;
    use crate::db::track_repo::TrackRepo;

    fn setup() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn mk_track(backend: &Arc<dyn DbBackend>, title: &str, format: &str, dur: i64) -> i64 {
        let repo = TrackRepo::with_backend(backend.clone());
        let mut t = Track::new(title.into());
        t.format = Some(format.into());
        t.duration_ms = dur;
        t.file_path = Some(format!("/m/{title}.{format}"));
        repo.create(&t).unwrap()
    }

    fn store_analysed(backend: &Arc<dyn DbBackend>, track_id: i64) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBED_DIM];
        v[0] = 1.0;
        let blob = Some(to_bytes(&v));
        let params: [&dyn ToSqlValue; 3] = [&track_id, &MODEL_ID, &blob];
        backend
            .execute(
                "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at) \
                 VALUES (?, ?, ?, 42)",
                &params,
            )
            .unwrap();
        v
    }

    #[test]
    fn herite_vers_la_jumelle_dsd_et_reste_idempotent() {
        let backend = setup();
        let flac = mk_track(&backend, "So What", "flac", 200_000);
        let dsd = mk_track(&backend, "So What", "dsf", 200_400); // ±1 s : ok
        let orphan = mk_track(&backend, "Blue in Green", "dsf", 300_000); // pas de jumelle
        let v = store_analysed(&backend, flac);

        assert_eq!(inherit_from_local_twins(&backend), 1);
        // La DSD a reçu LE vecteur de la FLAC, marqué hérité.
        assert_eq!(fetch_one(&backend, dsd).as_deref(), Some(v.as_slice()));
        assert!(fetch_one(&backend, orphan).is_none());
        let src = backend
            .query_one(
                "SELECT source FROM track_audio_embedding WHERE track_id = ?",
                &[&dsd as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_string()));
        assert_eq!(src.as_deref(), Some(format!("inherited:{flac}").as_str()));

        // Idempotent : rien de nouveau au second passage.
        assert_eq!(inherit_from_local_twins(&backend), 0);
    }

    #[test]
    fn le_suffixe_d_edition_et_les_accents_ne_cachent_plus_la_jumelle() {
        // La lettre de #1732 : normalisation via track_matcher::normalize.
        // Un repiquage SACD étiqueté « (Remastered 2003) » avec un titre
        // accentué doit trouver sa jumelle FLAC au titre nu — le lower/trim
        // d'origine ne les appariait pas.
        let backend = setup();
        let flac = mk_track(&backend, "Deja Vu", "flac", 248_000);
        let dsd = mk_track(&backend, "Déjà Vu (Remastered 2003)", "dsf", 248_300);
        let v = store_analysed(&backend, flac);

        assert_eq!(inherit_from_local_twins(&backend), 1);
        assert_eq!(fetch_one(&backend, dsd).as_deref(), Some(v.as_slice()));
        let src = backend
            .query_one(
                "SELECT source FROM track_audio_embedding WHERE track_id = ?",
                &[&dsd as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_string()));
        assert_eq!(src.as_deref(), Some(format!("inherited:{flac}").as_str()));
    }

    #[test]
    fn duree_trop_differente_ne_matche_pas() {
        // Même titre mais 5 s d'écart : autre version (live/edit), on ne
        // copie pas — c'est le même seuil d'esprit que le dédoublonnage.
        let backend = setup();
        let flac = mk_track(&backend, "Imagine", "flac", 187_000);
        let dsd = mk_track(&backend, "Imagine", "dsf", 192_500);
        store_analysed(&backend, flac);
        assert_eq!(inherit_from_local_twins(&backend), 0);
        assert!(fetch_one(&backend, dsd).is_none());
    }

    #[test]
    fn un_bump_de_modele_purge_puis_recopie() {
        let backend = setup();
        let flac = mk_track(&backend, "Nightswimming", "flac", 255_000);
        let dsd = mk_track(&backend, "Nightswimming", "dsf", 255_000);
        store_analysed(&backend, flac);
        assert_eq!(inherit_from_local_twins(&backend), 1);

        // Simule un bump : l'hérité porte un vieux modèle, la source a été
        // ré-analysée dans le nouvel espace.
        backend
            .execute(
                "UPDATE track_audio_embedding SET model = 'clap-old' WHERE track_id = ?",
                &[&dsd as &dyn ToSqlValue],
            )
            .unwrap();
        assert_eq!(inherit_from_local_twins(&backend), 1, "purgé puis recopié");
        let model = backend
            .query_one(
                "SELECT model FROM track_audio_embedding WHERE track_id = ?",
                &[&dsd as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_string()));
        assert_eq!(model.as_deref(), Some(MODEL_ID));
    }
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

#[cfg(test)]
mod progress_counter_tests {
    use super::*;
    use crate::db::models::Track;
    use crate::db::sqlite::SqliteDb;
    use crate::db::track_repo::TrackRepo;

    fn setup() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn mk_track(backend: &Arc<dyn DbBackend>, n: usize) -> i64 {
        let repo = TrackRepo::with_backend(backend.clone());
        let mut t = Track::new(format!("piste {n}"));
        t.format = Some("flac".into());
        t.file_path = Some(format!("/m/{n}.flac"));
        repo.create(&t).unwrap()
    }

    /// Pose le témoin de passage, avec le modèle sous lequel la piste a été
    /// traitée — c'est ce que fait la passe, y compris quand elle échoue.
    fn stamp(backend: &Arc<dyn DbBackend>, track_id: i64, model: &str) {
        let params: [&dyn ToSqlValue; 2] = [&track_id, &model];
        backend
            .execute(
                "INSERT INTO track_metadata (track_id, key, value) \
                 VALUES (?, 'audio_embed_analyzed', ?)",
                &params,
            )
            .unwrap();
    }

    fn store_embedding(backend: &Arc<dyn DbBackend>, track_id: i64, model: &str) {
        let mut v = vec![0.0f32; EMBED_DIM];
        v[0] = 1.0;
        let blob = Some(to_bytes(&v));
        let params: [&dyn ToSqlValue; 3] = [&track_id, &model, &blob];
        backend
            .execute(
                "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at) \
                 VALUES (?, ?, ?, 42)",
                &params,
            )
            .unwrap();
    }

    /// #1819, cas RÉEL de la machine .18 (16/08) : le modèle a été bumpé.
    /// 16 832 pistes portent l'ancien témoin, 8 258 le nouveau. La jauge
    /// affichait 99,8 % et ne bougeait plus, alors qu'il restait un tiers de la
    /// discothèque à ré-analyser.
    ///
    /// Réduit à l'échelle : 5 pistes, 3 sous l'ancien modèle, 2 sous le nouveau.
    #[test]
    fn un_changement_de_modele_ne_doit_pas_afficher_une_jauge_pleine() {
        let backend = setup();
        for n in 0..5 {
            let id = mk_track(&backend, n);
            if n < 3 {
                // Ancien modèle : traitées, mais à refaire.
                stamp(&backend, id, "clap-music-2023");
                store_embedding(&backend, id, "clap-music-2023");
            } else {
                stamp(&backend, id, MODEL_ID);
                store_embedding(&backend, id, MODEL_ID);
            }
        }

        assert_eq!(eligible_count(&backend), 5, "les 5 pistes sont analysables");
        assert_eq!(
            analysed_count(&backend),
            2,
            "seuls les embeddings du modèle COURANT comptent — c'est le cœur du \
             défaut : sans ce filtre, on comptait 5 et la jauge était pleine"
        );
        assert_eq!(
            processed_count(&backend),
            2,
            "les 3 pistes de l'ancien modèle sont redevenues candidates"
        );
        assert_eq!(failed_count(&backend), 0, "aucun échec ici");
    }

    /// Le cas de Reivax66 : 17 292 éligibles, 17 277 traitées, 15 pistes qui
    /// ont échoué. La passe est TERMINÉE — il ne reste aucun candidat — mais la
    /// jauge affichait 17 277/17 292 et l'utilisateur concluait au blocage.
    ///
    /// Réduit : 4 pistes traitées sous le modèle courant, dont 1 sans embedding.
    #[test]
    fn les_echecs_sont_comptes_a_part_et_la_passe_peut_finir() {
        let backend = setup();
        for n in 0..4 {
            let id = mk_track(&backend, n);
            stamp(&backend, id, MODEL_ID);
            // La dernière a échoué : témoin posé, aucun embedding écrit.
            if n < 3 {
                store_embedding(&backend, id, MODEL_ID);
            }
        }

        assert_eq!(eligible_count(&backend), 4);
        assert_eq!(analysed_count(&backend), 3, "3 embeddings écrits");
        assert_eq!(
            processed_count(&backend),
            4,
            "les 4 sont traitées : c'est ce compteur qui atteint le total"
        );
        assert_eq!(
            failed_count(&backend),
            1,
            "la 4e a échoué, elle doit être dite"
        );
        assert_eq!(
            eligible_count(&backend) - processed_count(&backend),
            0,
            "plus rien en attente : la passe est finie, la jauge doit le montrer"
        );
    }

    /// Le classement ne doit jamais comparer deux espaces vectoriels : un
    /// cosinus entre modèles différents ne veut rien dire (#1820).
    #[test]
    fn le_classement_ignore_les_vecteurs_d_un_autre_modele() {
        let backend = setup();
        let ancien = mk_track(&backend, 0);
        let courant = mk_track(&backend, 1);
        store_embedding(&backend, ancien, "clap-music-2023");
        store_embedding(&backend, courant, MODEL_ID);

        let charges = fetch_all(&backend, 100);
        assert_eq!(charges.len(), 1, "un seul espace vectoriel à la fois");
        assert_eq!(charges[0].0, courant, "celui du modèle courant");
    }
}
