//! #7983 — la passe « Enrichir les métadonnées » reprend où elle en était.
//!
//! Fil 2673, Tune OS, ≈ 540 000 pistes : le reste à faire remontait
//! (≈ 500 000 → 517 000) sans ajout de fichier, et après un redémarrage la
//! passe repartait des premiers `track_id`, les pistes en 503
//! (`mb_details_failed`) remises dans la file. La requête de sélection est
//! EXÉCUTÉE contre une vraie base SQLite migrée.
use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::enrich_tentatives::{self, Issue};
use tune_core::db::sqlite::SqliteDb;

const T0: i64 = 1_800_000_000;

/// Cinq pistes à qui il manque le genre, l'année, le label : toutes
/// candidates tant que rien n'est noté.
fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let mut sql = String::from(
        "INSERT INTO artists (id, name) VALUES (1, 'Keith Jarrett'); \
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Live', 1);",
    );
    for id in [5, 3, 1, 4, 2] {
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, source, file_mtime) \
             VALUES ({id}, 'Piste {id}', 1, 1, '/m/{id}.flac', 'local', 1700000000.25);"
        ));
    }
    backend.execute_batch(&sql).unwrap();
    backend
}

fn candidats(backend: &Arc<dyn DbBackend>, maintenant: i64) -> Vec<i64> {
    backend
        .query_many(&sql_candidats_enrichissement(backend, maintenant), &[])
        .expect("la sélection s'exécute")
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

/// 🔴 LE TÉMOIN DU REDÉMARRAGE. Une passe a traité les pistes 1 à 3 (une
/// faite, une introuvable, une en 503) puis le serveur s'arrête. La sélection
/// suivante ne repart pas des premiers identifiants : seules restent les
/// pistes 4 et 5, et le compte ne remonte pas au-dessus du reste d'avant.
#[test]
fn apres_un_redemarrage_la_passe_saute_les_pistes_deja_tentees() {
    let backend = base();
    assert_eq!(candidats(&backend, T0), vec![1, 2, 3, 4, 5], "ordre par id");
    enrich_tentatives::noter(&backend, 1, Issue::Fait, T0).unwrap();
    enrich_tentatives::noter(&backend, 2, Issue::Introuvable, T0).unwrap();
    enrich_tentatives::noter(&backend, 3, Issue::EchecTemporaire, T0).unwrap();
    let apres = candidats(&backend, T0 + 60);
    assert_eq!(
        apres,
        vec![4, 5],
        "les pistes déjà tentées (faite, introuvable, 503 en attente) ne reviennent pas"
    );
    assert!(apres.len() <= 5 - 3, "le reste à faire ne remonte pas");
}

/// Un échec temporaire revient, mais seulement après son délai — et le
/// délai suivant est plus long : pas de nouvelle tentative en boucle.
#[test]
fn un_echec_temporaire_revient_apres_son_delai_seulement() {
    let backend = base();
    let reessai = enrich_tentatives::noter(&backend, 3, Issue::EchecTemporaire, T0).unwrap();
    assert!(
        !candidats(&backend, reessai - 1).contains(&3),
        "avant le délai"
    );
    assert!(candidats(&backend, reessai).contains(&3), "délai écoulé");
    let second = enrich_tentatives::noter(&backend, 3, Issue::EchecTemporaire, reessai).unwrap();
    assert!(second - reessai > reessai - T0, "le délai grandit");
    assert_eq!(
        enrich_tentatives::compter_a_retenter(&backend, reessai).unwrap(),
        1
    );
}

/// Une piste « faite » ou « introuvable » ne revient qu'avec un fichier
/// modifié (relu par un scan) : la marque vaut pour le fichier vu.
#[test]
fn un_fichier_modifie_redevient_candidat() {
    let backend = base();
    enrich_tentatives::noter(&backend, 1, Issue::Fait, T0).unwrap();
    enrich_tentatives::noter(&backend, 2, Issue::Introuvable, T0).unwrap();
    let loin = T0 + 365 * 24 * 3600;
    assert_eq!(candidats(&backend, loin), vec![3, 4, 5]);
    backend
        .execute(
            "UPDATE tracks SET file_mtime = 1700009999.5 WHERE id = 2",
            &[],
        )
        .unwrap();
    assert_eq!(candidats(&backend, loin), vec![2, 3, 4, 5]);
}

/// 🔴 LE TÉMOIN DU 503. Écrite avec seulement son MBID parce que la requête
/// de détails a échoué, la piste n'est pas « faite » : elle était comptée
/// enrichie, puis remise dans la file à chaque passe.
#[test]
fn des_details_en_echec_ne_valent_pas_une_piste_faite() {
    assert_eq!(
        issue_de_la_tentative(Etape::Ecrite {
            appel_en_echec: true
        }),
        Some(Issue::EchecTemporaire),
        "mb_details_failed (503) doit être un échec temporaire, pas un succès"
    );
    assert_eq!(
        issue_de_la_tentative(Etape::Ecrite {
            appel_en_echec: false
        }),
        Some(Issue::Fait)
    );
    assert_eq!(
        issue_de_la_tentative(Etape::RechercheEnEchec),
        Some(Issue::EchecTemporaire)
    );
    assert_eq!(
        issue_de_la_tentative(Etape::RechercheIntrouvable),
        Some(Issue::Introuvable)
    );
    assert_eq!(issue_de_la_tentative(Etape::EcritureEnEchec), None);
}

/// Une recherche qui rend 503 ou 429 n'est pas un « introuvable » définitif.
#[test]
fn un_service_indisponible_n_est_pas_introuvable() {
    assert!(reponse_indisponible(StatusCode::SERVICE_UNAVAILABLE));
    assert!(reponse_indisponible(StatusCode::TOO_MANY_REQUESTS));
    assert!(reponse_indisponible(StatusCode::BAD_GATEWAY));
    assert!(!reponse_indisponible(StatusCode::NOT_FOUND));
    assert!(!reponse_indisponible(StatusCode::BAD_REQUEST));
}

/// L'état exposé distingue les restantes et les échecs à retenter, sans
/// changer `enriched` ni `total` (contrat du client web, #2870).
#[test]
fn le_releve_distingue_restantes_et_a_retenter() {
    let r = releve_enrichissement("running", "t", 7, 2, 100, 40, 3);
    assert_eq!(r["enriched"], 7);
    assert_eq!(r["total"], 100);
    assert_eq!(r["examined"], 40);
    assert_eq!(r["remaining"], 60);
    assert_eq!(r["retry_pending"], 3);
    assert_eq!(
        releve_enrichissement("done", "t", 0, 0, 5, 9, 0)["remaining"],
        0
    );
}
