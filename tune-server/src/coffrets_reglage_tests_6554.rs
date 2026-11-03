//! #6554 (fil 2294, Marco Polo et Gros Bidon) — le réglage de bibliothèque
//! « Coffrets automatiques ». Go de Bertrand du 19/10/2026.
//!
//! Ces épreuves exécutent le VRAI scan forcé (`spawn_library_scan`, le bouton
//! « Analyse complète »), les VRAIES routes (`GET /library/coffrets`,
//! `PATCH` et `GET /system/config`), sur une base SQLite de fichier.
//!
//! Elles répondent aussi à la question posée par Bertrand : que devient une
//! bibliothèque DÉJÀ scannée quand on coupe le réglage ? Faut-il rescanner ?
use super::coffret_manuel_scan_tests_5319::{
    Bibliotheque, albums, appel, bibliotheque, scan_force,
};
use super::surveillant_retouche_tests_4896::{baliser, flac_8_canaux};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};
use tune_core::db::coffrets_auto::CLE_REGLAGE_AUTO;

/// Pose des pistes FLAC balisées dans `dossier`.
fn pistes(dossier: &std::path::Path, album: &str, disque: Option<u32>, n: u32) {
    std::fs::create_dir_all(dossier).unwrap();
    let hier = SystemTime::now() - Duration::from_secs(86_400);
    for k in 1..=n {
        let piste = dossier.join(format!("0{k}.flac"));
        std::fs::write(&piste, flac_8_canaux()).unwrap();
        let titre = format!("{album} {disque:?} piste {k}");
        let numero = k.to_string();
        let d = disque.map(|d| d.to_string());
        let mut balises: Vec<(&str, &str)> = vec![
            ("TITLE", titre.as_str()),
            ("ARTIST", "Laurent Garnier"),
            ("ALBUMARTIST", "Laurent Garnier"),
            ("ALBUM", album),
            ("TRACKNUMBER", numero.as_str()),
            ("DATE", "1999"),
        ];
        if let Some(d) = d.as_deref() {
            balises.push(("DISCNUMBER", d));
        }
        baliser(&piste, &balises, hier);
    }
}

/// Un coffret ÉCLATÉ — le cas que la passe réunit : deux dossiers frères,
/// le disque n'est QUE dans la balise ALBUM (« Early Works, Disc 2 »).
fn coffret_eclate(b: &Bibliotheque) {
    let parent = b.racine.join("Laurent Garnier - Early Works");
    for n in [1u32, 2] {
        pistes(
            &parent.join(format!("Partie {n}")),
            &format!("Early Works, Disc {n}"),
            None,
            2,
        );
    }
}

/// Le cas de Didier : un album à plusieurs disques rangé `Album/CD 1`,
/// `Album/CD 2`, la MÊME balise ALBUM partout. Le scan en fait un album à
/// deux disques (C4) — sans la passe.
fn album_en_cd1_cd2(b: &Bibliotheque) {
    let parent = b.racine.join("Laurent Garnier - The Cloud Making Machine");
    for n in [1u32, 2] {
        pistes(
            &parent.join(format!("CD {n}")),
            "The Cloud Making Machine",
            Some(n),
            2,
        );
    }
}

async fn regler(b: &Bibliotheque, actif: bool) {
    let (statut, corps) = appel(
        &b.etat,
        "PATCH",
        "/api/v1/system/config",
        Value::Object(serde_json::Map::from_iter([(
            CLE_REGLAGE_AUTO.to_string(),
            json!(actif),
        )])),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "PATCH /system/config refusé : {corps}"
    );
}

/// Les titres des albums listés sous Coffrets.
async fn onglet_coffrets(b: &Bibliotheque) -> Vec<String> {
    let (statut, corps) = appel(&b.etat, "GET", "/api/v1/library/coffrets", Value::Null).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn titres(b: &Bibliotheque) -> Vec<String> {
    let mut t: Vec<String> = albums(&b.db).into_iter().map(|(_, t)| t).collect();
    t.sort();
    t
}

/// La route publie le réglage même absent de la base : ACTIF.
#[tokio::test]
async fn la_config_publie_le_reglage_actif_par_defaut_6554() {
    let b = bibliotheque("6554-config");
    let (statut, corps) = appel(&b.etat, "GET", "/api/v1/system/config", Value::Null).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        corps[CLE_REGLAGE_AUTO],
        json!(true),
        "{}",
        corps[CLE_REGLAGE_AUTO]
    );
    regler(&b, false).await;
    let (_, corps) = appel(&b.etat, "GET", "/api/v1/system/config", Value::Null).await;
    assert_eq!(corps[CLE_REGLAGE_AUTO], json!(false));
}

/// Réglage coupé AVANT le premier scan : le coffret éclaté reste deux
/// albums, et l'album en `CD 1` / `CD 2` reste UN album à deux disques mais
/// n'est plus rangé sous Coffrets.
#[tokio::test]
async fn reglage_coupe_le_scan_ne_reunit_rien_et_coffrets_reste_vide_6554() {
    let b = bibliotheque("6554-coupe");
    coffret_eclate(&b);
    album_en_cd1_cd2(&b);
    regler(&b, false).await;

    scan_force(&b.etat).await;

    assert_eq!(
        titres(&b),
        vec![
            "Early Works, Disc 1".to_string(),
            "Early Works, Disc 2".to_string(),
            "The Cloud Making Machine".to_string(),
        ],
        "réglage coupé : aucune réunion automatique"
    );
    assert_eq!(
        onglet_coffrets(&b).await,
        Vec::<String>::new(),
        "réglage coupé : un album en CD 1 / CD 2 n'est pas un coffret"
    );
}

/// Le témoin du défaut d'aujourd'hui, réglage ACTIF (le défaut) : le
/// comportement ne change pas.
#[tokio::test]
async fn reglage_actif_par_defaut_rien_ne_change_6554() {
    let b = bibliotheque("6554-actif");
    coffret_eclate(&b);
    album_en_cd1_cd2(&b);

    scan_force(&b.etat).await;

    assert_eq!(
        titres(&b),
        vec![
            "Early Works".to_string(),
            "The Cloud Making Machine".to_string()
        ],
        "défaut : la passe réunit le coffret éclaté"
    );
    let mut onglet = onglet_coffrets(&b).await;
    onglet.sort();
    assert_eq!(
        onglet,
        vec![
            "Early Works".to_string(),
            "The Cloud Making Machine".to_string()
        ],
        "défaut : les deux sont sous Coffrets, comme avant"
    );
}

/// LA QUESTION DE BERTRAND — bibliothèque déjà scannée, réglage coupé
/// ensuite.
///
/// - L'onglet Coffrets change TOUT DE SUITE, sans rescan : l'album en
///   `CD 1` / `CD 2` n'y est plus.
/// - Le coffret déjà réuni RESTE réuni, même après un scan forcé (ses
///   pistes sont tenues, #2094), et reste sous Coffrets, où « Défaire »
///   le rend à ses disques. Défait, il ne revient pas.
#[tokio::test]
async fn bibliotheque_deja_scannee_puis_reglage_coupe_6554() {
    let b = bibliotheque("6554-apres");
    coffret_eclate(&b);
    album_en_cd1_cd2(&b);
    scan_force(&b.etat).await;
    assert_eq!(
        titres(&b),
        vec![
            "Early Works".to_string(),
            "The Cloud Making Machine".to_string()
        ],
        "montage : la passe a réuni le coffret"
    );

    regler(&b, false).await;
    assert_eq!(
        onglet_coffrets(&b).await,
        vec!["Early Works".to_string()],
        "sans rescan : seul le coffret MARQUÉ reste sous Coffrets"
    );

    scan_force(&b.etat).await;
    assert_eq!(
        titres(&b),
        vec![
            "Early Works".to_string(),
            "The Cloud Making Machine".to_string()
        ],
        "un scan forcé ne défait pas le coffret déjà réuni"
    );
    assert_eq!(onglet_coffrets(&b).await, vec!["Early Works".to_string()]);

    let coffret = albums(&b.db)
        .into_iter()
        .find(|(_, t)| t == "Early Works")
        .unwrap()
        .0;
    let (statut, corps) = appel(
        &b.etat,
        "POST",
        &format!("/api/v1/library/coffrets/{coffret}/defaire"),
        Value::Null,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "défaire refusé : {corps}");
    scan_force(&b.etat).await;
    assert_eq!(
        titres(&b),
        vec![
            "Early Works, Disc 1".to_string(),
            "Early Works, Disc 2".to_string(),
            "The Cloud Making Machine".to_string(),
        ],
        "défait, le coffret ne revient pas"
    );
    assert_eq!(onglet_coffrets(&b).await, Vec::<String>::new());
}
