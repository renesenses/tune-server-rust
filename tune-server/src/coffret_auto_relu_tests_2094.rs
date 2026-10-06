//! Fil 2094, suite de #5812 (« non prouvé ») — un coffret AUTOMATIQUE perdait
//! ses numéros et ses sous-titres de disque quand un scan relisait ses
//! fichiers.
//!
//! La passe réunit les disques et écrit `disc_number` et `disc_subtitle` en
//! base, sans rien écrire dans les fichiers. Le scan forcé reconstruit chaque
//! ligne piste depuis ses BALISES : chaque disque revenait au disque 1, sans
//! nom. Aucune tenue ne protégeait un coffret automatique.
//!
//! 🔴 Mesuré avant le correctif : un scan forcé SEUL ne montre pas le défaut,
//! parce que la passe qui le suit reforme le coffret et repose les
//! sous-titres. Le défaut se voit là où aucune passe ne rattrape :
//! - la relecture d'UNE piste (`POST /library/tracks/{id}/rescan`, et la
//!   même voie pour le surveillant de fichiers) : disque 1, sans nom ;
//! - un coffret que l'utilisateur a RENOMMÉ : son titre ne porte plus de
//!   marqueur de disque, la passe ne le reconnaît plus, et le scan forcé le
//!   coupe en deux albums.
//!
//! Le montage est celui d'*Early Works* (Laurent Garnier) : le numéro n'est
//! que dans la balise ALBUM (« Early Works, Disc 2 »), le DISCNUMBER des
//! fichiers vaut 1 pour chaque disque, et le nom du dossier ne dit rien.
//!
//! Ces épreuves exécutent le VRAI scan forcé (`spawn_library_scan`) et la
//! VRAIE route « Défaire », sur une base SQLite de fichier.
use super::coffret_manuel_scan_tests_5319::{
    Bibliotheque, albums, appel, bibliotheque, scan_force,
};
use super::surveillant_retouche_tests_4896::{baliser, flac_8_canaux};
use axum::http::StatusCode;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tune_core::db::backend::DbBackend;

/// `(chemin, album_id, disc_number, disc_subtitle)` de chaque piste, triées
/// par chemin.
fn pistes(db: &Arc<dyn DbBackend>) -> Vec<(String, i64, i64, Option<String>)> {
    let mut v: Vec<(String, i64, i64, Option<String>)> = db
        .query_many(
            "SELECT file_path, album_id, disc_number, disc_subtitle FROM tracks",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[0].as_string().unwrap_or_default(),
                r[1].as_i64().unwrap_or(-1),
                r[2].as_i64().unwrap_or(-1),
                r[3].as_string(),
            )
        })
        .collect();
    v.sort();
    v
}

/// Deux disques frères que la passe AUTOMATIQUE réunit au premier scan. Le
/// numéro de disque n'est QUE dans la balise ALBUM : ni DISCNUMBER, ni
/// chiffre dans le nom des dossiers.
fn coffret_auto(epreuve: &str) -> (Bibliotheque, String) {
    let b = bibliotheque(epreuve);
    let parent = b.racine.join("Laurent Garnier - Early Works");
    let hier = SystemTime::now() - Duration::from_secs(86_400);
    let mut premier = String::new();
    for (n, dossier) in [(1, "Premiere partie"), (2, "Seconde partie")] {
        let dossier = parent.join(dossier);
        std::fs::create_dir_all(&dossier).unwrap();
        if n == 1 {
            premier = dossier.to_string_lossy().into_owned();
        }
        for k in 1..=2 {
            let piste = dossier.join(format!("0{k}.flac"));
            std::fs::write(&piste, flac_8_canaux()).unwrap();
            baliser(
                &piste,
                &[
                    ("TITLE", &format!("Disque {n} piste {k}")),
                    ("ARTIST", "Laurent Garnier"),
                    ("ALBUMARTIST", "Laurent Garnier"),
                    ("ALBUM", &format!("Early Works, Disc {n}")),
                    ("TRACKNUMBER", &k.to_string()),
                    ("DATE", "1999"),
                ],
                hier,
            );
        }
    }
    (b, premier)
}

/// Le disque (1 ou 2) d'un chemin du montage.
fn disque_du_montage(chemin: &str, premier: &str) -> i64 {
    if chemin.starts_with(premier) { 1 } else { 2 }
}

/// LE TÉMOIN — composer (la passe), relire tous les fichiers : numéros et
/// sous-titres de disque restent ceux de la composition.
#[tokio::test]
async fn un_coffret_auto_garde_ses_disques_au_scan_force_2094() {
    let (b, premier) = coffret_auto("auto-relu");
    scan_force(&b.etat).await;
    assert_eq!(
        albums(&b.db)
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>(),
        vec!["Early Works"],
        "montage : la passe a réuni les deux disques"
    );
    let compose = pistes(&b.db);
    for (chemin, _, disque, nom) in &compose {
        let n = disque_du_montage(chemin, &premier);
        assert_eq!(
            (*disque, nom.as_deref()),
            (n, Some(format!("Early Works, Disc {n}").as_str())),
            "montage : {chemin} porte son numéro et son titre d'origine : {compose:?}"
        );
    }

    scan_force(&b.etat).await;

    assert_eq!(
        pistes(&b.db),
        compose,
        "le scan forcé a défait les disques du coffret automatique"
    );
    assert_eq!(
        albums(&b.db),
        vec![(compose[0].1, "Early Works".to_string())],
        "toujours un seul album, le coffret, sous son titre"
    );

    // Un second scan forcé : rien ne dérive.
    scan_force(&b.etat).await;
    assert_eq!(pistes(&b.db), compose, "second scan forcé");
}

/// « Défaire » reste exact après une relecture : chaque disque redevient son
/// album, sans le sous-titre que la composition avait posé — et un nouveau
/// scan forcé ne lui rend ni le coffret, ni ce nom.
#[tokio::test]
async fn defaire_un_coffret_auto_relu_reste_exact_2094() {
    let (b, premier) = coffret_auto("auto-relu-defaire");
    scan_force(&b.etat).await;
    scan_force(&b.etat).await;
    let coffret = pistes(&b.db)[0].1;
    assert!(
        pistes(&b.db)
            .iter()
            .all(|p| p.1 == coffret && p.3.is_some()),
        "montage : le coffret relu est entier et nommé : {:?}",
        pistes(&b.db)
    );

    let (statut, corps) = appel(
        &b.etat,
        "POST",
        &format!("/api/v1/library/coffrets/{coffret}/defaire"),
        Value::Null,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "défaire refusé : {corps}");

    let defait = pistes(&b.db);
    let album_2 = defait
        .iter()
        .find(|(c, ..)| disque_du_montage(c, &premier) == 2)
        .map(|p| p.1)
        .unwrap();
    assert_ne!(album_2, coffret, "le disque 2 est sorti du coffret");
    for (chemin, album, _, nom) in &defait {
        let attendu = if disque_du_montage(chemin, &premier) == 1 {
            coffret
        } else {
            album_2
        };
        assert_eq!(
            (*album, nom.as_deref()),
            (attendu, None),
            "{chemin} : son album, sans nom : {defait:?}"
        );
    }

    scan_force(&b.etat).await;
    let relu = pistes(&b.db);
    for (chemin, album, disque, nom) in &relu {
        let attendu = if disque_du_montage(chemin, &premier) == 1 {
            coffret
        } else {
            album_2
        };
        assert_eq!(
            (*album, *disque, nom.as_deref()),
            (attendu, 1, None),
            "{chemin} : après le scan forcé, le coffret défait reste défait, \
             chaque disque au numéro de sa balise et sans nom : {relu:?}"
        );
    }
}

/// La relecture d'UNE piste (`POST /library/tracks/{id}/rescan`, « Relire les
/// balises ») : aucune passe ne suit, rien ne reforme le coffret.
#[tokio::test]
async fn un_coffret_auto_garde_ses_disques_a_la_relecture_d_une_piste_2094() {
    let (b, _) = coffret_auto("auto-relu-piste");
    scan_force(&b.etat).await;
    let compose = pistes(&b.db);
    assert!(
        compose.iter().any(|p| p.2 == 2),
        "montage : un disque 2 : {compose:?}"
    );
    let ids: Vec<i64> =
        b.db.query_many("SELECT id FROM tracks ORDER BY id", &[])
            .unwrap()
            .iter()
            .filter_map(|r| r[0].as_i64())
            .collect();
    for id in ids {
        let (statut, corps) = appel(
            &b.etat,
            "POST",
            &format!("/api/v1/library/tracks/{id}/rescan"),
            Value::Null,
        )
        .await;
        assert!(statut.is_success(), "relecture de {id} : {statut} {corps}");
    }
    assert_eq!(
        pistes(&b.db),
        compose,
        "la relecture des balises a défait les disques du coffret automatique"
    );
}

/// Un coffret automatique que l'utilisateur a RENOMMÉ (titre tenu à la main)
/// : après le scan forcé, la passe ne le reconnaît plus — son titre ne porte
/// plus de marqueur de disque. Seule la tenue garde ses disques.
#[tokio::test]
async fn un_coffret_auto_renomme_garde_ses_disques_au_scan_force_2094() {
    let (b, _) = coffret_auto("auto-relu-renomme");
    scan_force(&b.etat).await;
    let coffret = pistes(&b.db)[0].1;
    let (statut, corps) = appel(
        &b.etat,
        "PUT",
        &format!("/api/v1/library/albums/{coffret}/edition"),
        serde_json::json!({ "title": "Early Works (le coffret)" }),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "montage : renommage refusé : {corps}"
    );
    let compose = pistes(&b.db);

    scan_force(&b.etat).await;

    assert_eq!(
        pistes(&b.db),
        compose,
        "le scan forcé a défait les disques du coffret automatique renommé"
    );
    assert_eq!(
        albums(&b.db),
        vec![(coffret, "Early Works (le coffret)".to_string())],
        "un seul album, sous le titre de l'utilisateur"
    );
}
