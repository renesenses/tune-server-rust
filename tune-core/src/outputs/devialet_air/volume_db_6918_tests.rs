//! #6918 — course du curseur en dB réglable entre −80 et −10 dB (arbitrage
//! de Bertrand du 27/10), le plafond de −10 dB restant le dernier mot.
//!
//! Tout se joue face au simulateur d'ampli, sur la boucle locale : aucun son
//! ne sort.

use std::sync::Arc;
use std::time::Duration;

use super::super::protocole::opcode;
use super::super::reglages::{self, BornesDb, PLAFOND_DB, PLANCHER_DB};
use super::super::simulateur::annonce_de_test;
use super::banc_avec_bornes;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::outputs::traits::OutputTarget;

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Témoin de conversion pourcentage ↔ dB sur une course réglée.
#[test]
fn air_6918_conversion_pour_cent_db_sur_une_course_reglee() {
    let b = BornesDb::nouvelles(-60.0, -30.0).unwrap();
    assert_eq!(b.volume_vers_db(0.0), -60.0);
    assert_eq!(b.volume_vers_db(0.5), -45.0);
    assert_eq!(b.volume_vers_db(1.0), -30.0);
    assert_eq!(b.volume_vers_db(0.25), -52.5);
    assert_eq!(b.db_vers_volume(-45.0), 0.5);
    assert_eq!(b.db_vers_volume(-30.0), 1.0);
    // Réglé plus fort ou plus bas à la télécommande : le curseur bute.
    assert_eq!(b.db_vers_volume(-12.0), 1.0);
    assert_eq!(b.db_vers_volume(-79.0), 0.0);
    // Aller-retour sur la grille : chaque pas de 0,5 dB se relit tel quel.
    let mut db = -60.0;
    while db <= -30.0 {
        let v = b.demande_vers_volume(db).unwrap();
        assert_eq!(b.volume_vers_db(v), db, "{db} dB → {v} → retour");
        db += 0.5;
    }
    // La course par défaut est celle d'avant : 50 % = −45 dB.
    assert_eq!(BornesDb::PAR_DEFAUT.min_db(), PLANCHER_DB);
    assert_eq!(BornesDb::PAR_DEFAUT.max_db(), PLAFOND_DB);
    assert_eq!(reglages::volume_vers_db(0.5), -45.0);
}

/// Témoin de bornes : dans `[−80 ; −10]`, sur la grille, assez écartées.
#[test]
fn air_6918_les_bornes_tiennent_dans_moins_80_moins_10() {
    assert!(BornesDb::nouvelles(-80.0, -10.0).is_ok());
    let refus = BornesDb::nouvelles(-80.0, 0.0).unwrap_err();
    assert!(refus.contains("plafond"), "{refus}");
    assert!(
        BornesDb::nouvelles(-80.0, -9.5).is_err(),
        "−9,5 dB > plafond"
    );
    assert!(
        BornesDb::nouvelles(-90.0, -10.0).is_err(),
        "sous le plancher"
    );
    assert!(BornesDb::nouvelles(-40.0, -38.0).is_err(), "écart < 5 dB");
    assert!(BornesDb::nouvelles(f64::NAN, -10.0).is_err());
    assert!(BornesDb::nouvelles(-60.0, f64::INFINITY).is_err());
    // Hors grille : ramenées vers l'intérieur de la course.
    let b = BornesDb::nouvelles(-60.3, -30.2).unwrap();
    assert_eq!((b.min_db(), b.max_db()), (-60.0, -30.5));
}

/// Témoin de refus au-dessus du plafond : une demande en dB au-dessus de la
/// borne haute est REFUSÉE, jamais ramenée ; et aucune course, même lue
/// d'une base abîmée, ne fait sortir une consigne au-dessus de −10 dB.
#[test]
fn air_6918_rien_au_dessus_du_plafond_quelle_que_soit_la_demande() {
    let b = BornesDb::nouvelles(-70.0, -20.0).unwrap();
    for db in [-19.5, -10.0, -5.0, 0.0, 6.0, f64::INFINITY, f64::NAN] {
        assert!(b.demande_vers_volume(db).is_err(), "{db} dB accepté");
    }
    assert!(b.demande_vers_volume(-71.0).is_err(), "sous la borne basse");

    // Base abîmée : une borne haute à 0 dB écrite à la main.
    let db = base();
    let r = SettingsRepo::with_backend(db.clone());
    r.set(reglages::CLE_VOLUME_MIN_DB, "-80").unwrap();
    r.set(reglages::CLE_VOLUME_MAX_DB, "0").unwrap();
    let lues = BornesDb::lues(&db);
    assert_eq!(lues, BornesDb::PAR_DEFAUT, "la base abîmée retombe sur −10");
    for v in [0.0, 0.5, 0.99, 1.0, 1.5, 100.0, f64::INFINITY, f64::NAN] {
        assert!(lues.volume_vers_db(v) <= PLAFOND_DB, "{v}");
    }
    // Une course enregistrée se relit telle quelle.
    BornesDb::nouvelles(-65.0, -25.0)
        .unwrap()
        .enregistrer(&db)
        .unwrap();
    let relues = BornesDb::lues(&db);
    assert_eq!((relues.min_db(), relues.max_db()), (-65.0, -25.0));
}

/// De bout en bout face au simulateur : la consigne envoyée à l'ampli suit
/// la course réglée, et ne dépasse jamais sa borne haute ni le plafond.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_6918_l_ampli_recoit_la_course_reglee() {
    let bornes = BornesDb::nouvelles(-60.0, -30.0).unwrap();
    let b = banc_avec_bornes(annonce_de_test(), true, bornes).await;
    b.sortie.set_volume(1.0).await.unwrap();
    b.sortie.set_volume(0.5).await.unwrap();
    b.sortie.set_volume(7.0).await.unwrap();
    b.sortie.set_volume(0.0).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let s = b.sim.etat();
    let volumes: Vec<f32> = s
        .commandes
        .iter()
        .filter(|c| c.opcode == opcode::VOLUME)
        .map(|c| c.valeur)
        .collect();
    // Chaque commande part deux fois (comme Roon).
    assert_eq!(
        volumes,
        vec![-30.0, -30.0, -45.0, -45.0, -30.0, -30.0, -60.0, -60.0],
        "consignes envoyées"
    );
    assert!(volumes.iter().all(|v| f64::from(*v) <= PLAFOND_DB));
}
