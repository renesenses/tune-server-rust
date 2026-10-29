//! #6917 — témoins de la règle « même racine » et du nettoyage qui la lit.
//!
//! Le banc de bout en bout (vrai scan, vraies routes, SQLite et PostgreSQL)
//! est `tune-server/tests/dedoublonnage_par_racine_6917.rs`.
use super::*;
use crate::db::models::Track;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

fn racines() -> Vec<String> {
    vec![
        "/mnt/disque".into(),
        "/mnt/nas".into(),
        "/mnt/nas/hires".into(),
    ]
}

#[test]
fn le_reglage_est_eteint_par_defaut_6917() {
    const { assert!(!DEFAUT) };
    assert!(!depuis_valeur(None));
    assert!(!depuis_valeur(Some("false")));
    assert!(!depuis_valeur(Some("n'importe quoi")));
    assert!(depuis_valeur(Some("true")));
    assert!(depuis_valeur(Some("\"true\"")));
    assert!(depuis_valeur(Some(" On ")));
}

#[test]
fn la_meme_racine_se_lit_par_la_racine_la_plus_longue_6917() {
    let r = racines();
    assert!(meme_racine(
        "/mnt/disque/a.flac",
        "/mnt/disque/x/b.flac",
        &r
    ));
    assert!(!meme_racine("/mnt/disque/a.flac", "/mnt/nas/a.flac", &r));
    // Racines imbriquées : `hires` est un support à part entière.
    assert!(!meme_racine("/mnt/nas/a.flac", "/mnt/nas/hires/a.flac", &r));
    // Un préfixe de nom n'est pas un dossier parent.
    assert!(!meme_racine(
        "/mnt/disque/a.flac",
        "/mnt/disque2/a.flac",
        &r
    ));
    // Hors de toute racine : la copie de personne.
    assert!(!meme_racine("/ailleurs/a.flac", "/ailleurs/b.flac", &r));
}

#[test]
fn sans_borne_tout_candidat_reste_repliable_6917() {
    let c = vec![
        "/mnt/nas/a.flac".to_string(),
        "/mnt/disque/b.flac".to_string(),
    ];
    assert_eq!(
        candidats_repliables("/mnt/disque/a.flac", c.clone(), None),
        c
    );
    let r = racines();
    assert_eq!(
        candidats_repliables("/mnt/disque/a.flac", c, Some(&r)),
        vec!["/mnt/disque/b.flac".to_string()]
    );
}

/// Trois copies octet pour octet : deux sous `disque` (même support), une
/// sous `nas`. Rend les lignes retirées par le nettoyage, les lignes qui
/// restent et leurs chemins relatifs.
fn nettoyer_trois_copies(option: Option<&str>) -> (i64, i64, Vec<String>) {
    let dir = tempfile::TempDir::new().unwrap();
    let disque = dir.path().join("disque");
    let nas = dir.path().join("nas");
    let octets = vec![0x5au8; 65_536 * 3];
    let chemins = [
        disque.join("Album/01.flac"),
        disque.join("Copie/01.flac"),
        nas.join("Album/01.flac"),
    ];
    for c in &chemins {
        std::fs::create_dir_all(c.parent().unwrap()).unwrap();
        std::fs::write(c, &octets).unwrap();
    }
    let hash = crate::scanner::hasher::compute_audio_hash(&chemins[0]).unwrap();

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    // `settings` naît des migrations, pas de `CORE_SCHEMA`.
    db.execute_batch("CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    let dirs = serde_json::to_string(&[
        disque.to_string_lossy().into_owned(),
        nas.to_string_lossy().into_owned(),
    ])
    .unwrap();
    db.execute(
        "INSERT INTO settings (key, value) VALUES ('music_dirs', ?1)",
        &[&dirs],
    )
    .unwrap();
    if let Some(v) = option {
        db.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)",
            &[&CLE, &v],
        )
        .unwrap();
    }
    let repo = TrackRepo::new(db);
    for (i, c) in chemins.iter().enumerate() {
        let mut t = Track::new(format!("Copie {i}"));
        t.file_path = Some(c.to_string_lossy().into_owned());
        t.audio_hash = Some(hash.clone());
        repo.create(&t).unwrap();
    }
    let retirees = repo.deduplicate().unwrap();
    let restantes = repo.count().unwrap();
    // Aucun FICHIER n'est jamais effacé, quelle que soit l'option.
    for c in &chemins {
        assert!(
            c.exists(),
            "le nettoyage a effacé un fichier : {}",
            c.display()
        );
    }
    let mut chemins_restants: Vec<String> = repo
        .list_all_local()
        .unwrap()
        .into_iter()
        .filter_map(|t| t.file_path)
        .map(|p| {
            p.strip_prefix(&*dir.path().to_string_lossy())
                .unwrap_or(&p)
                .to_string()
        })
        .collect();
    chemins_restants.sort();
    (retirees, restantes, chemins_restants)
}

/// ⭐ Option allumée : la copie du NAS reste une entrée, la copie redondante
/// du MÊME disque est toujours retirée.
#[test]
fn option_allumee_le_nettoyage_garde_la_copie_de_l_autre_racine_6917() {
    let (retirees, restantes, chemins) = nettoyer_trois_copies(Some("true"));
    assert_eq!(
        (retirees, restantes),
        (1, 2),
        "option allumée : seule la copie du même disque doit partir, la copie \
         du NAS reste une entrée — restent {chemins:?}"
    );
    assert!(
        chemins.iter().any(|c| c.starts_with("/nas/")),
        "la copie du NAS a été retirée de la bibliothèque : {chemins:?}"
    );
    assert!(
        chemins.iter().any(|c| c.starts_with("/disque/")),
        "{chemins:?}"
    );
}

/// Option éteinte (absente ou `false`) : le comportement d'avant, une seule
/// entrée pour les trois copies.
#[test]
fn option_eteinte_le_nettoyage_ne_change_pas_6917() {
    for option in [None, Some("false")] {
        let (retirees, restantes, chemins) = nettoyer_trois_copies(option);
        assert_eq!(
            (retirees, restantes),
            (2, 1),
            "option {option:?} : le nettoyage d'avant ne garde qu'une entrée — restent {chemins:?}"
        );
    }
}
