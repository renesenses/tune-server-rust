//! Témoins de la sauvegarde locale PostgreSQL (#7288).
//!
//! Un faux `pg_dump` (script shell) tient la place du vrai : il note ses
//! arguments et son environnement, puis écrit un fichier qui commence comme un
//! dump custom (`PGDMP`). Le vrai `pg_dump` contre une vraie base n'est exercé
//! que par `pg_7288_sauvegarde_reelle`, quand `TUNE_TEST_PG_URL` est posée.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;

const URL: &str = "postgres://tune:secret%40mdp@db.example:5432/tune";

#[cfg(unix)]
fn faux_pg_dump(dossier: &Path, corps: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dossier.join("faux_pg_dump.sh");
    fs::write(&script, format!("#!/bin/sh\n{corps}\n")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Le faux `pg_dump` qui réussit : arguments et `PGPASSWORD` dans `journal`.
#[cfg(unix)]
fn faux_pg_dump_qui_reussit(dossier: &Path, journal: &Path) -> PathBuf {
    faux_pg_dump(
        dossier,
        &format!(
            r#"printf '%s\n' "$@" > '{j}'
echo "PGPASSWORD=$PGPASSWORD" >> '{j}'
while [ $# -gt 0 ]; do
  if [ "$1" = "--file" ]; then shift; printf 'PGDMP-faux' > "$1"; fi
  shift
done"#,
            j = journal.display()
        ),
    )
}

fn noms(dossier: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dossier)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn separer_mot_de_passe_retire_le_secret_de_l_url_7288() {
    let (sans, mdp) = separer_mot_de_passe(URL);
    assert_eq!(sans, "postgres://tune@db.example:5432/tune");
    assert_eq!(mdp.as_deref(), Some("secret@mdp"));

    let (sans, mdp) = separer_mot_de_passe("postgresql://tune@localhost/tune");
    assert_eq!(sans, "postgresql://tune@localhost/tune");
    assert_eq!(mdp, None);

    let (sans, mdp) = separer_mot_de_passe("postgres://localhost/tune?sslmode=disable");
    assert_eq!(sans, "postgres://localhost/tune?sslmode=disable");
    assert_eq!(mdp, None);
}

#[cfg(unix)]
#[test]
fn sauvegarde_pg_ecrit_un_dump_liste_sans_mot_de_passe_en_argument_7288() {
    let tmp = tempfile::tempdir().unwrap();
    let backups = tmp.path().join("backups");
    let journal = tmp.path().join("journal.txt");
    let pg_dump = faux_pg_dump_qui_reussit(tmp.path(), &journal);

    let info = create_pg_backup(URL, &backups, &pg_dump)
        .expect("la sauvegarde PostgreSQL doit aboutir quand pg_dump réussit");

    assert!(
        info.filename.starts_with("tune_pg_") && info.filename.ends_with(".dump"),
        "nom inattendu : {}",
        info.filename
    );
    assert_eq!(info.size, "PGDMP-faux".len() as u64);
    assert_eq!(
        noms(&backups),
        vec![info.filename.clone()],
        "le .partial doit avoir été renommé, rien d'autre"
    );

    let liste = list_pg_backups(&backups);
    assert_eq!(liste.len(), 1);
    assert_eq!(liste[0].filename, info.filename);

    let lu = fs::read_to_string(&journal).unwrap();
    assert!(lu.contains("--format=custom"), "{lu}");
    assert!(lu.contains("postgres://tune@db.example:5432/tune"), "{lu}");
    assert!(
        !lu.lines()
            .any(|l| !l.starts_with("PGPASSWORD=") && l.contains("secret")),
        "le mot de passe ne doit jamais être un argument de pg_dump : {lu}"
    );
    assert!(lu.contains("PGPASSWORD=secret@mdp"), "{lu}");
}

#[test]
fn pg_dump_introuvable_rend_une_erreur_qui_dit_quoi_faire_7288() {
    let tmp = tempfile::tempdir().unwrap();
    let backups = tmp.path().join("backups");
    let absent = tmp.path().join("pas_de_pg_dump_ici");

    let err = create_pg_backup(URL, &backups, &absent).unwrap_err();

    assert_eq!(err.code(), "pg_dump_not_found");
    let message = err.to_string();
    assert!(message.contains("pg_dump"), "{message}");
    assert!(message.contains(VARIABLE_PG_DUMP), "{message}");
    assert!(!message.contains("secret"), "{message}");
    assert!(list_pg_backups(&backups).is_empty());
}

#[cfg(unix)]
#[test]
fn echec_de_pg_dump_ne_laisse_aucun_fichier_et_masque_le_mot_de_passe_7288() {
    let tmp = tempfile::tempdir().unwrap();
    let backups = tmp.path().join("backups");
    let pg_dump = faux_pg_dump(
        tmp.path(),
        r#"while [ $# -gt 0 ]; do
  if [ "$1" = "--file" ]; then shift; printf 'moitie' > "$1"; fi
  shift
done
echo "pg_dump: error: password secret@mdp rejected" >&2
exit 1"#,
    );

    let err = create_pg_backup(URL, &backups, &pg_dump).unwrap_err();

    assert_eq!(err.code(), "pg_dump_failed");
    let message = err.to_string();
    assert!(message.contains("rejected"), "{message}");
    assert!(!message.contains("secret@mdp"), "{message}");
    assert!(
        noms(&backups).is_empty(),
        "un dump interrompu ne doit pas rester : {:?}",
        noms(&backups)
    );
}

#[cfg(unix)]
#[test]
fn elagage_garde_cinq_dumps_et_ne_touche_pas_aux_sauvegardes_sqlite_7288() {
    let tmp = tempfile::tempdir().unwrap();
    let backups = tmp.path().join("backups");
    fs::create_dir_all(&backups).unwrap();
    for i in 0..6 {
        fs::write(backups.join(format!("tune_pg_2020010{i}_000000.dump")), "x").unwrap();
    }
    fs::write(backups.join("tune_20200101_000000.db"), "sqlite").unwrap();
    let journal = tmp.path().join("journal.txt");
    let pg_dump = faux_pg_dump_qui_reussit(tmp.path(), &journal);

    let info = create_pg_backup(URL, &backups, &pg_dump).unwrap();

    let liste = list_pg_backups(&backups);
    assert_eq!(liste.len(), super::super::MAX_BACKUPS);
    assert_eq!(liste[0].filename, info.filename, "la plus récente en tête");
    assert!(backups.join("tune_20200101_000000.db").exists());
    assert!(!backups.join("tune_pg_20200100_000000.dump").exists());
}

/// Le vrai `pg_dump` contre une vraie base : le fichier produit est un dump
/// au format custom (en-tête `PGDMP`).
#[test]
fn pg_7288_sauvegarde_reelle() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée — PostgreSQL non exercé");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let backups = tmp.path().join("backups");

    let info = create_pg_backup(&url, &backups, &programme_pg_dump())
        .unwrap_or_else(|e| panic!("sauvegarde réelle en échec : {e}"));

    let octets = fs::read(backups.join(&info.filename)).unwrap();
    assert!(
        octets.starts_with(b"PGDMP"),
        "pas un dump custom de pg_dump"
    );
    assert_eq!(list_pg_backups(&backups).len(), 1);
}
