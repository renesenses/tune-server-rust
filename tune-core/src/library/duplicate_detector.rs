use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::scanner::hasher::{
    compute_audio_hash_str, files_are_byte_identical, is_current_audio_hash,
};

#[derive(Debug, Clone)]
pub struct DuplicateEntry {
    pub id: i64,
    pub title: String,
    pub artist_name: Option<String>,
    pub file_path: String,
}

#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    pub hash: String,
    pub tracks: Vec<DuplicateEntry>,
}

#[derive(Debug, Clone)]
pub struct DuplicateScanResult {
    pub total_scanned: usize,
    pub duplicates_found: usize,
    pub groups: Vec<DuplicateGroup>,
    pub errors: usize,
}

fn exact_duplicate_groups(hash: String, tracks: Vec<DuplicateEntry>) -> Vec<DuplicateGroup> {
    let mut exact_partitions: Vec<Vec<DuplicateEntry>> = Vec::new();

    for track in tracks {
        let mut pending = Some(track);
        for partition in &mut exact_partitions {
            let Some(candidate) = pending.as_ref() else {
                break;
            };
            let representative = &partition[0];
            if files_are_byte_identical(
                Path::new(&candidate.file_path),
                Path::new(&representative.file_path),
            )
            .unwrap_or(false)
            {
                partition.push(pending.take().expect("track still pending"));
            }
        }
        if let Some(track) = pending {
            exact_partitions.push(vec![track]);
        }
    }

    exact_partitions
        .into_iter()
        .filter(|partition| partition.len() > 1)
        .map(|tracks| DuplicateGroup {
            hash: hash.clone(),
            tracks,
        })
        .collect()
}

/// La clef de regroupement d'un candidat au dédoublonnage.
///
/// 🔴 `Option<i64>` et pas rien : **deux pistes d'une même feuille CUE ne sont
/// PAS des doublons.** Elles partagent le même `cue_media_path`, donc la même
/// empreinte de fichier, et une clef réduite au hachage les déclarerait toutes
/// identiques — quinze « doublons » proposés à la suppression sur un disque qui
/// n'en contient aucun. Le début de tranche les sépare, et il fait mieux que
/// cela : sur DEUX copies de la même image, il apparie la piste 3 de l'une avec
/// la piste 3 de l'autre, ce qui est le vrai doublon.
type CleDeRegroupement = (String, Option<i64>);

pub fn scan_duplicates(db: &Arc<dyn DbBackend>, limit: usize) -> DuplicateScanResult {
    // `COALESCE(file_path, cue_media_path)` : une piste de feuille CUE porte
    // `file_path = NULL` par construction. Sur `file_path IS NOT NULL`, une
    // bibliothèque rangée en CUE n'était JAMAIS dédoublonnée — pas un doublon
    // détecté, quel que soit le nombre de copies.
    let chemin = crate::db::track_repo::sql::CHEMIN_OUVRABLE;
    let a_un_fichier = crate::db::track_repo::sql::A_UN_FICHIER;
    let projection = format!(
        "SELECT t.id, {chemin}, t.title, t.audio_hash, t.file_path, \
                COALESCE(t.cue_start_ms, 0) \
         FROM tracks t WHERE t.source = 'local' AND {a_un_fichier}"
    );
    let query = if limit > 0 {
        format!("{projection} LIMIT {limit}")
    } else {
        projection
    };

    let raw_rows = match db.query_many(&query, &[]) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "duplicate_scan_query_error");
            return DuplicateScanResult {
                total_scanned: 0,
                duplicates_found: 0,
                groups: Vec::new(),
                errors: 1,
            };
        }
    };

    // Le 5e champ est `file_path` BRUT : vide ou absent ⇒ la piste est une
    // tranche de feuille CUE, et le chemin du 2e champ est celui de l'image.
    let rows: Vec<(i64, String, String, Option<String>, Option<i64>)> = raw_rows
        .iter()
        .map(|r| {
            let a_un_fichier_a_soi = r
                .get(4)
                .and_then(|v| v.as_string())
                .is_some_and(|p| !p.is_empty());
            let tranche =
                (!a_un_fichier_a_soi).then(|| r.get(5).and_then(|v| v.as_i64()).unwrap_or(0));
            (
                r[0].as_i64().unwrap_or(0),
                r[1].as_string().unwrap_or_default(),
                r[2].as_string().unwrap_or_default(),
                r[3].as_string(),
                tranche,
            )
        })
        .collect();

    let mut hash_map: HashMap<CleDeRegroupement, Vec<DuplicateEntry>> = HashMap::new();
    let mut scanned = 0;
    let mut errors = 0;

    for (id, file_path, title, existing_hash, tranche) in &rows {
        let current_hash = existing_hash
            .as_deref()
            .filter(|hash| is_current_audio_hash(hash))
            .map(str::to_owned);
        let h = if current_hash.is_some() {
            current_hash
        } else {
            let computed = compute_audio_hash_str(file_path);
            // 🔴 On n'inscrit le hachage QUE sur une piste qui possède son
            // fichier. Sur une tranche de CUE, ce serait l'empreinte de
            // l'IMAGE rangée dans la colonne d'identité de la PISTE : les
            // quinze pistes du disque porteraient la même, et tout ce qui lit
            // `tracks.audio_hash` (dédoublonnage du scanner, appariement par
            // album) hériterait de la confusion qu'on vient d'éviter ici.
            if let (Some(h), None) = (&computed, tranche) {
                if let Err(error) = db.execute(
                    "UPDATE tracks SET audio_hash = ? WHERE id = ?",
                    &[&h.as_str(), id],
                ) {
                    warn!(%error, track_id = id, path = file_path, "audio_hash_update_failed");
                }
            }
            computed
        };

        if let Some(h) = h {
            hash_map
                .entry((h, *tranche))
                .or_default()
                .push(DuplicateEntry {
                    id: *id,
                    title: title.clone(),
                    artist_name: None,
                    file_path: file_path.clone(),
                });
            scanned += 1;
        } else {
            errors += 1;
        }
    }

    // The sampled hash only selects inexpensive candidates. It can never by
    // itself make two tracks duplicates: every reported group is partitioned
    // by a complete byte-for-byte comparison.
    let groups: Vec<DuplicateGroup> = hash_map
        .into_iter()
        .flat_map(|((hash, _tranche), tracks)| exact_duplicate_groups(hash, tracks))
        .collect();

    let duplicates_found: usize = groups.iter().map(|g| g.tracks.len() - 1).sum();

    info!(
        scanned,
        groups = groups.len(),
        duplicates = duplicates_found,
        errors,
        "duplicate_scan_complete"
    );

    DuplicateScanResult {
        total_scanned: scanned,
        duplicates_found,
        groups,
        errors,
    }
}

pub fn scan_fingerprint_duplicates(db: &Arc<dyn DbBackend>) -> Vec<DuplicateGroup> {
    let raw_rows = match db.query_many(
        "SELECT t.id, t.title, ar.name, t.file_path, t.acoustid_fingerprint
         FROM tracks t
         LEFT JOIN artists ar ON t.artist_id = ar.id
         WHERE t.acoustid_fingerprint IS NOT NULL AND t.acoustid_fingerprint != ''
         ORDER BY t.acoustid_fingerprint, t.id",
        &[],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "fingerprint_duplicate_query_error");
            return Vec::new();
        }
    };

    let mut fp_map: HashMap<String, Vec<DuplicateEntry>> = HashMap::new();
    for r in &raw_rows {
        let id = r[0].as_i64().unwrap_or(0);
        let title = r[1].as_string().unwrap_or_default();
        let artist = r[2].as_string();
        let path = r[3].as_string().unwrap_or_default();
        let fp = r[4].as_string().unwrap_or_default();
        fp_map.entry(fp).or_default().push(DuplicateEntry {
            id,
            title,
            artist_name: artist,
            file_path: path,
        });
    }

    fp_map
        .into_iter()
        .filter(|(_, g)| g.len() > 1)
        .map(|(fp, tracks)| DuplicateGroup { hash: fp, tracks })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let mut data = vec![0u8; 16 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        std::fs::write(&path, &data).unwrap();

        let h1 = compute_audio_hash_str(path.to_str().unwrap());
        let h2 = compute_audio_hash_str(path.to_str().unwrap());
        assert!(h1.is_some());
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_nonexistent_file() {
        let result = compute_audio_hash_str("/nonexistent/file.flac");
        assert!(result.is_none());
    }

    #[test]
    fn hash_tiny_nonempty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.bin");
        std::fs::write(&path, &[0u8; 100]).unwrap();
        let result = compute_audio_hash_str(path.to_str().unwrap());
        assert!(result.is_some());
    }

    #[test]
    fn different_files_different_hashes() {
        let dir = tempfile::tempdir().unwrap();

        let mut data_a = vec![0u8; 16 * 1024];
        for (i, b) in data_a.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        let path_a = dir.path().join("a.bin");
        std::fs::write(&path_a, &data_a).unwrap();

        let mut data_b = vec![0xFFu8; 16 * 1024];
        for (i, b) in data_b.iter_mut().enumerate() {
            *b = ((i + 1) % 256) as u8;
        }
        let path_b = dir.path().join("b.bin");
        std::fs::write(&path_b, &data_b).unwrap();

        let ha = compute_audio_hash_str(path_a.to_str().unwrap()).unwrap();
        let hb = compute_audio_hash_str(path_b.to_str().unwrap()).unwrap();
        assert_ne!(ha, hb);
    }

    #[test]
    fn hash_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let data = vec![42u8; 16 * 1024];
        std::fs::write(&path, &data).unwrap();

        let h = compute_audio_hash_str(path.to_str().unwrap()).unwrap();
        assert!(is_current_audio_hash(&h));
    }

    // ─── Pistes de feuille CUE ──────────────────────────────────────────────

    fn backend_de_test() -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Un fichier image de 16 Kio, au contenu dicté par `graine`.
    fn ecrire_image(chemin: &std::path::Path, graine: u8) {
        let mut data = vec![0u8; 16 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = ((i + graine as usize) % 256) as u8;
        }
        std::fs::write(chemin, &data).unwrap();
    }

    /// 🔴 `file_path = None` : c'est ce que le scanner écrit pour une tranche
    /// de feuille CUE, et c'est CE cas que le témoin doit décrire. Une fixture
    /// avec `file_path` renseigné décrirait le cas qui marchait déjà.
    fn creer_piste_cue(
        db: &Arc<dyn DbBackend>,
        titre: &str,
        image: &std::path::Path,
        debut_ms: i64,
    ) -> i64 {
        let mut t = crate::db::models::Track::new(titre.to_string());
        t.source = "local".into();
        t.file_path = None;
        t.cue_media_path = Some(image.to_string_lossy().into_owned());
        t.cue_start_ms = Some(debut_ms);
        crate::db::track_repo::TrackRepo::with_backend(db.clone())
            .create(&t)
            .unwrap()
    }

    /// 🔴 LES DEUX COPIES D'UN MÊME DISQUE CUE SONT VUES — ET SEULEMENT ELLES.
    ///
    /// Le détecteur filtrait sur `file_path IS NOT NULL`. Une piste découpée
    /// par une feuille CUE porte `file_path = NULL` par construction : une
    /// bibliothèque rangée en CUE n'a jamais vu un seul doublon détecté.
    ///
    /// Et le piège de la correction naïve est dans le même témoin : les deux
    /// tranches d'UNE MÊME image partagent le fichier, donc le hachage. Les
    /// regrouper sur ce seul hachage proposerait à la suppression des pistes
    /// parfaitement distinctes. On attend donc DEUX groupes de deux — la
    /// piste 1 des deux copies, la piste 2 des deux copies — et surtout pas un
    /// groupe de quatre.
    #[test]
    fn deux_copies_d_une_image_cue_donnent_deux_groupes_pas_un_seul() {
        let base = crate::test_scratch::scratch_dir("tune_dup_cue");
        let copie_a = base.join("copie-a.flac");
        let copie_b = base.join("copie-b.flac");
        ecrire_image(&copie_a, 0);
        ecrire_image(&copie_b, 0); // octet pour octet identiques

        let db = backend_de_test();
        let a1 = creer_piste_cue(&db, "Aria", &copie_a, 0);
        let a2 = creer_piste_cue(&db, "Variatio 1", &copie_a, 180_000);
        let b1 = creer_piste_cue(&db, "Aria", &copie_b, 0);
        let b2 = creer_piste_cue(&db, "Variatio 1", &copie_b, 180_000);

        let bilan = scan_duplicates(&db, 0);
        assert_eq!(
            bilan.total_scanned, 4,
            "les quatre tranches CUE doivent être examinées ; \
             avec le filtre `file_path IS NOT NULL` il y en avait ZÉRO"
        );
        assert_eq!(
            bilan.groups.len(),
            2,
            "un groupe par TRANCHE, pas un seul gros groupe d'image ; \
             groupes rendus : {:?}",
            bilan
                .groups
                .iter()
                .map(|g| g.tracks.iter().map(|t| t.id).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
        for g in &bilan.groups {
            let mut ids: Vec<i64> = g.tracks.iter().map(|t| t.id).collect();
            ids.sort_unstable();
            assert!(
                ids == vec![a1.min(b1), a1.max(b1)] || ids == vec![a2.min(b2), a2.max(b2)],
                "un groupe doit apparier la MÊME tranche des deux copies, \
                 rendu : {ids:?}"
            );
        }
        assert_eq!(bilan.duplicates_found, 2, "un doublon par tranche");
    }

    /// La contre-partie : un disque CUE présent une SEULE fois n'a aucun
    /// doublon. C'est le dégât qu'un regroupement par hachage seul ferait —
    /// quinze pistes distinctes proposées à la suppression.
    #[test]
    fn les_tranches_d_une_seule_image_ne_sont_jamais_des_doublons() {
        let base = crate::test_scratch::scratch_dir("tune_dup_cue_solo");
        let image = base.join("image.flac");
        ecrire_image(&image, 7);

        let db = backend_de_test();
        for (n, debut) in [(1, 0i64), (2, 180_000), (3, 300_000)] {
            creer_piste_cue(&db, &format!("piste {n}"), &image, debut);
        }

        let bilan = scan_duplicates(&db, 0);
        assert_eq!(bilan.total_scanned, 3, "les trois tranches sont examinées");
        assert_eq!(
            bilan.duplicates_found, 0,
            "trois pistes d'une même image ne sont PAS des doublons"
        );
        assert!(bilan.groups.is_empty(), "aucun groupe ne doit être proposé");
    }

    /// Et le hachage de l'IMAGE ne doit pas être inscrit dans la colonne
    /// d'identité de la PISTE : `tracks.audio_hash` est lu ailleurs, et quinze
    /// pistes portant la même valeur y rejoueraient la confusion.
    #[test]
    fn le_hachage_de_l_image_n_est_pas_inscrit_sur_la_tranche() {
        let base = crate::test_scratch::scratch_dir("tune_dup_cue_hash");
        let image = base.join("image.flac");
        ecrire_image(&image, 3);

        let db = backend_de_test();
        let id = creer_piste_cue(&db, "Aria", &image, 0);
        scan_duplicates(&db, 0);

        let hash = db
            .query_one(
                "SELECT audio_hash FROM tracks WHERE id = ?",
                &[&id as &dyn crate::db::backend::ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_string()));
        assert_eq!(
            hash, None,
            "une tranche de CUE ne doit pas hériter du hachage de son image"
        );
    }
}
