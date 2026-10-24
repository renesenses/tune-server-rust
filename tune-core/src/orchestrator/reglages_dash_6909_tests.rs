//! #6909 — le pré-transcodage DASH (TIDAL/Qobuz Hi-Res) honore les réglages
//! DLNA de la zone, comme le chemin des fichiers locaux.
//!
//! Marantz NR1602 (fil 2410) : le renderer annonce `audio/flac`, la piste
//! partait en FLAC 24 bits, et ni « Servir en WAV/LPCM » ni « Plafonner à
//! 16 bits » n'y changeaient rien : `choisir_l_encodage_dash` ne consultait
//! que le forçage FLAC natif et la sonde du renderer.

use std::sync::Arc;

use super::resolve_stream::{ReglagesDash, format_dash_impose, profondeur_dash, reglages_dash};
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    // Les colonnes de réglage DLNA arrivent par migration.
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

#[test]
fn une_zone_dlna_reglee_en_wav_et_16_bits_est_lue_comme_telle() {
    let db = base();
    let depot = ZoneRepo::with_backend(db.clone());
    let zone = depot
        .create("NR1602", Some("dlna"), Some("uuid:nr1602"))
        .unwrap();
    depot.update_dlna_lpcm(zone, true).unwrap();
    depot.update_dlna_cap_16bit(zone, true).unwrap();
    let r = reglages_dash(&db, zone, 24);
    assert!(r.forcer_wav, "« Servir en WAV/LPCM » doit être lu : {r:?}");
    assert!(r.plafond_16, "« Plafonner à 16 bits » doit être lu : {r:?}");
    // Une source déjà en 16 bits n'a rien à plafonner.
    assert!(!reglages_dash(&db, zone, 16).plafond_16);
}

#[test]
fn hors_sortie_reseau_aucun_reglage_dlna_ne_s_applique() {
    let db = base();
    let depot = ZoneRepo::with_backend(db.clone());
    let zone = depot
        .create("Ici", Some("local"), Some("local:defaut"))
        .unwrap();
    depot.update_dlna_lpcm(zone, true).unwrap();
    depot.update_dlna_cap_16bit(zone, true).unwrap();
    assert_eq!(reglages_dash(&db, zone, 24), ReglagesDash::default());
}

#[test]
fn le_wav_force_par_la_zone_l_emporte_sur_la_sonde_et_sur_le_flac_natif() {
    let wav = ReglagesDash {
        forcer_wav: true,
        plafond_16: false,
    };
    // Un renderer qui annonce `audio/flac` : la sonde n'est même pas posée.
    assert_eq!(format_dash_impose(false, wav, false, false), Some("wav"));
    assert_eq!(format_dash_impose(false, wav, false, true), Some("wav"));
    // Sans réglage, rien n'est imposé : la sonde décide, comme avant.
    assert_eq!(
        format_dash_impose(false, ReglagesDash::default(), false, false),
        None
    );
    assert_eq!(
        format_dash_impose(false, ReglagesDash::default(), false, true),
        Some("flac")
    );
    assert_eq!(
        format_dash_impose(true, ReglagesDash::default(), false, true),
        Some("wav"),
        "navigateur : toujours WAV"
    );
}

#[test]
fn le_plafond_16_bits_de_la_zone_vaut_aussi_pour_le_flac() {
    let plafond = ReglagesDash {
        forcer_wav: false,
        plafond_16: true,
    };
    assert_eq!(profondeur_dash("flac", 24, plafond), 16);
    assert_eq!(profondeur_dash("flac", 24, ReglagesDash::default()), 24);
    assert_eq!(profondeur_dash("wav", 24, ReglagesDash::default()), 16);
}

/// Le site d'appel : les règles ci-dessus ne valent que si le bras DASH les
/// CONSULTE. Garde de texte ; les définitions vivent dans le même fichier,
/// donc on compte les APPELS (définition exclue) plutôt qu'une présence.
///
/// Sabotage qui rend ce témoin ROUGE : rétablir, dans
/// `choisir_l_encodage_dash`, l'ancien `if is_browser_output { "wav" } else
/// if dash_did.is_empty() || force || … sonde …`.
#[test]
fn le_bras_dash_consulte_les_reglages_de_la_zone() {
    let site = include_str!("resolve_stream.rs");
    let appels = |motif: &str| site.matches(motif).count();
    assert!(
        site.contains("let reglages = reglages_dash(&self.db, req.zone_id,"),
        "les réglages de la zone doivent être lus par le bras DASH"
    );
    // 1 définition + cache chaud + `choisir_l_encodage_dash`.
    assert_eq!(
        appels("format_dash_impose("),
        3,
        "le cache chaud ET la lecture doivent décider par `format_dash_impose`"
    );
    assert!(
        site.contains("(dash_is_wav || dash_plafond_16) && actual_bd > 16"),
        "le plafond 16 bits doit s'appliquer au FLAC pré-transcodé"
    );
    assert!(
        appels("!reglages.plafond_16") + appels("!dash_plafond_16") >= 2,
        "les deux remux (copie des trames 24 bits) doivent céder au plafond"
    );
    let file = include_str!("queue.rs");
    assert!(
        file.contains("resolve_stream::profondeur_dash("),
        "le préchauffage de la piste suivante doit appliquer le même plafond"
    );
}
