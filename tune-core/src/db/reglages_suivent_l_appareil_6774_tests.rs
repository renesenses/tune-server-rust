//! #6774 — témoins du changement d'appareil d'une zone, par le dépôt.
//!
//! Tous les chemins (PATCH, hot-swap, profil de zones, découverte, bascule
//! Tune OS, rattachement ASIO, rebond par nom) écrivent `output_device_id`
//! par [`ZoneRepo::changer_d_appareil`] ou [`ZoneRepo::update_output_device`].
//! Les témoins par route vivent côté serveur
//! (`routes/zones/reglages_suivent_l_appareil_6774_tests.rs`).

use super::*;
use crate::audio::reglages_de_volume::Rattachement;
use crate::db::settings_repo::SettingsRepo;
use crate::outputs::identite_de_sortie::SortieEnumeree;

fn repo() -> (ZoneRepo, Arc<dyn DbBackend>) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    (ZoneRepo::with_backend(backend.clone()), backend)
}

fn lire(backend: &Arc<dyn DbBackend>, cle: &str) -> Option<String> {
    SettingsRepo::with_backend(backend.clone())
        .get(cle)
        .unwrap()
}

fn poser(backend: &Arc<dyn DbBackend>, zone: i64) {
    let s = SettingsRepo::with_backend(backend.clone());
    s.set(&format!("zone_{zone}_gain_trim_db"), "8.5").unwrap();
    s.set(&format!("zone_{zone}_pose_gain_trim_db"), "1")
        .unwrap();
    s.set(&format!("zone_{zone}_volume_limit_safety"), "0.5")
        .unwrap();
    s.set(&format!("zone_{zone}_volume_limit_comfort"), "0.4")
        .unwrap();
}

fn poser_la_mac(backend: &Arc<dyn DbBackend>, zone: i64, mac: &str) {
    backend
        .execute(
            "UPDATE zones SET mac = ? WHERE id = ?",
            &[
                &mac.to_string() as &dyn ToSqlValue,
                &zone as &dyn ToSqlValue,
            ],
        )
        .unwrap();
}

/// 🔴 avant / 🟢 après : la zone passe sur un AUTRE appareil — le trim
/// repart de 0 dB, les limites restent.
#[test]
fn un_autre_appareil_remet_le_trim_a_zero_et_garde_les_limites() {
    let (repo, db) = repo();
    let id = repo
        .create("Salon", Some("dlna"), Some("uuid:marantz"))
        .unwrap();
    poser(&db, id);

    repo.update_output_device(id, "uuid:meitner").unwrap();

    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")),
        None,
        "le trim de l'ancien appareil a suivi la ligne de zone sur un AUTRE appareil (#6774)"
    );
    assert_eq!(lire(&db, &format!("zone_{id}_pose_gain_trim_db")), None);
    assert_eq!(
        lire(&db, &format!("zone_{id}_volume_limit_safety")).as_deref(),
        Some("0.5"),
        "les limites restent : elles ne font que borner"
    );
    assert_eq!(
        lire(&db, &format!("zone_{id}_volume_limit_comfort")).as_deref(),
        Some("0.4")
    );
}

/// Même règle par `update_device_id` (rattachement SSDP par adresse).
#[test]
fn le_rattachement_par_adresse_sans_mac_remet_le_trim_a_zero() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    poser(&db, id);
    repo.update_device_id(id, "uuid:b").unwrap();
    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")),
        None,
        "une même adresse ne prouve pas le même appareil (#6774)"
    );
}

/// Témoin vert : le même identifiant ne change rien.
#[test]
fn le_meme_identifiant_garde_tout() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    poser(&db, id);
    repo.update_output_device(id, "uuid:a").unwrap();
    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")).as_deref(),
        Some("8.5")
    );
}

/// Témoin vert : le même appareil retrouvé par sa MAC garde tout.
#[test]
fn le_meme_appareil_par_mac_garde_tout() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    poser_la_mac(&db, id, "80:0A:80:5C:26:89");
    poser(&db, id);
    repo.changer_d_appareil(
        id,
        "uuid:b",
        Rattachement::SansPreuve {
            origine: "test",
            mac: Some("800a805c2689"),
        },
    )
    .unwrap();
    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")).as_deref(),
        Some("8.5"),
        "la même MAC est le même appareil : le trim reste"
    );
}

/// 🔴 avant / 🟢 après : une AUTRE MAC est un autre appareil.
#[test]
fn une_autre_mac_remet_le_trim_a_zero() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    poser_la_mac(&db, id, "80:0A:80:5C:26:89");
    poser(&db, id);
    repo.changer_d_appareil(
        id,
        "uuid:b",
        Rattachement::SansPreuve {
            origine: "test",
            mac: Some("80:0A:80:5C:26:90"),
        },
    )
    .unwrap();
    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")),
        None,
        "une autre MAC est un AUTRE appareil : le trim doit repartir de 0 dB (#6774)"
    );
}

/// Témoin vert : la ré-association par endpoint (même périphérique local,
/// renommé par le pilote) garde le trim.
#[test]
fn la_reassociation_par_endpoint_garde_le_trim() {
    const GUID: &str = "wasapi:{0.0.0.00000000}.{e0ea21cf-56cc-445f-8454-880d431d7cb0}";
    let (repo, db) = repo();
    let id = repo
        .create("Salon", Some("local"), Some("local:audio-gd USB audio"))
        .unwrap();
    poser(&db, id);
    let sortie = |nom: &str| SortieEnumeree {
        nom: nom.to_string(),
        endpoint_id: GUID.to_string(),
    };
    repo.appliquer_identite_de_sortie(&[sortie("audio-gd USB audio")])
        .unwrap();
    let rapport = repo
        .appliquer_identite_de_sortie(&[sortie("audio-gd USB audio (44,1 kHz)")])
        .unwrap();
    assert_eq!(rapport.reassociees.len(), 1);
    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")).as_deref(),
        Some("8.5"),
        "même endpoint = même périphérique : le trim reste"
    );
}

// ---------------------------------------------------------------------------
// #6774 suite — décisions de Bertrand du 22/10 : PURE verrouillé et volume
// fixe désarmés, trim négatif gardé, trim positif à 0 dB, jamais une hausse.
// ---------------------------------------------------------------------------

fn poser_le_trim(backend: &Arc<dyn DbBackend>, zone: i64, trim: &str) {
    let s = SettingsRepo::with_backend(backend.clone());
    s.set(&format!("zone_{zone}_gain_trim_db"), trim).unwrap();
    s.set(&format!("zone_{zone}_pose_gain_trim_db"), "1")
        .unwrap();
}

fn poser_pure_verrouille(backend: &Arc<dyn DbBackend>, zone: i64) {
    SettingsRepo::with_backend(backend.clone())
        .set(
            &format!("zone_{zone}_audiophile"),
            r#"{"enabled":true,"lock_volume":true}"#,
        )
        .unwrap();
}

fn pure_force(backend: &Arc<dyn DbBackend>, zone: i64) -> bool {
    crate::audio::audiophile::zone_enabled(backend, zone)
        && crate::audio::audiophile::volume_lock_enabled(backend, zone)
}

fn volume(repo: &ZoneRepo, zone: i64) -> f64 {
    repo.get(zone).unwrap().unwrap().volume
}

/// 🔴 avant / 🟢 après : un trim NÉGATIF ne fait que baisser, il reste.
#[test]
fn un_trim_negatif_reste_sur_un_autre_appareil() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    poser_le_trim(&db, id, "-6");
    repo.update_output_device(id, "uuid:b").unwrap();
    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")).as_deref(),
        Some("-6"),
        "#6774 : un trim NÉGATIF (−6 dB) ne fait que baisser, il doit rester sur un autre appareil"
    );
    assert_eq!(
        lire(&db, &format!("zone_{id}_pose_gain_trim_db")).as_deref(),
        Some("1")
    );
}

/// Un trim POSITIF (+4 dB) repart de 0 dB.
#[test]
fn un_trim_positif_repart_de_zero_sur_un_autre_appareil() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    poser_le_trim(&db, id, "4");
    repo.update_output_device(id, "uuid:b").unwrap();
    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")),
        None,
        "#6774 : un trim POSITIF (+4 dB) de l'ancien appareil ne doit pas suivre la zone"
    );
}

/// 🔴 avant / 🟢 après : PURE verrouillé est désarmé, le volume repart de
/// la limite de confort (40 %), pas du 100 % de l'ancien appareil.
#[test]
fn pure_verrouille_est_desarme_sur_un_autre_appareil() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 100.0).unwrap();
    poser_pure_verrouille(&db, id);
    SettingsRepo::with_backend(db.clone())
        .set(&format!("zone_{id}_volume_limit_comfort"), "0.4")
        .unwrap();

    repo.update_output_device(id, "uuid:b").unwrap();

    assert!(
        !pure_force(&db, id),
        "#6774 : PURE verrouillé (Forcer à 100 %) de l'ancien appareil tient encore le nouveau à 100 %"
    );
    assert!(!crate::audio::audiophile::zone_enabled(&db, id));
    assert_eq!(
        volume(&repo, id),
        40.0,
        "le volume repart de la limite de confort"
    );
}

/// Le verrou hérité du réglage GLOBAL compte aussi : PURE est désarmé.
#[test]
fn pure_verrouille_par_le_reglage_global_est_desarme() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    s.set(crate::audio::audiophile::SETTING_LOCK_VOLUME, "true")
        .unwrap();
    s.set(&format!("zone_{id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    repo.update_output_device(id, "uuid:b").unwrap();
    assert!(
        !pure_force(&db, id),
        "#6774 : PURE verrouillé par le réglage global tient encore le nouvel appareil à 100 %"
    );
}

/// Témoin : PURE SANS verrou ne force rien, il reste.
#[test]
fn pure_sans_verrou_reste() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 30.0).unwrap();
    SettingsRepo::with_backend(db.clone())
        .set(
            &format!("zone_{id}_audiophile"),
            r#"{"enabled":true,"lock_volume":false}"#,
        )
        .unwrap();
    repo.update_output_device(id, "uuid:b").unwrap();
    assert!(crate::audio::audiophile::zone_enabled(&db, id));
    assert_eq!(volume(&repo, id), 30.0);
}

/// 🔴 avant / 🟢 après : le volume fixe est désarmé, le volume repart de la
/// mémoire d'avant l'armement (25 %), et cette mémoire est effacée.
#[test]
fn le_volume_fixe_est_desarme_sur_un_autre_appareil() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 100.0).unwrap();
    repo.update_fixed_volume(id, true).unwrap();
    crate::audio::fixed_volume::remember(&db, id, 25.0).unwrap();

    repo.update_output_device(id, "uuid:b").unwrap();

    assert!(
        !repo.get(id).unwrap().unwrap().fixed_volume,
        "#6774 : le volume fixe de l'ancien appareil tient encore le nouveau à 100 %"
    );
    assert_eq!(
        volume(&repo, id),
        25.0,
        "jamais tenu à 100 % du seul fait de l'ancien"
    );
    assert_eq!(crate::audio::fixed_volume::peek(&db, id), None);
}

/// Jamais une hausse : un volume déjà sous la limite ne remonte pas.
#[test]
fn le_desarmement_ne_fait_jamais_monter_le_volume() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 20.0).unwrap();
    poser_pure_verrouille(&db, id);
    SettingsRepo::with_backend(db.clone())
        .set(&format!("zone_{id}_volume_limit_comfort"), "0.4")
        .unwrap();
    repo.update_output_device(id, "uuid:b").unwrap();
    assert_eq!(volume(&repo, id), 20.0);
}

/// Témoin vert : le MÊME appareil (identifiant, puis MAC) garde tout —
/// PURE verrouillé, volume fixe, trim positif, volume.
#[test]
fn le_meme_appareil_garde_pure_volume_fixe_et_trim() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    poser_la_mac(&db, id, "80:0A:80:5C:26:89");
    repo.update_volume(id, 100.0).unwrap();
    repo.update_fixed_volume(id, true).unwrap();
    poser_pure_verrouille(&db, id);
    poser_le_trim(&db, id, "4");

    repo.update_output_device(id, "uuid:a").unwrap();
    repo.changer_d_appareil(
        id,
        "uuid:a-bis",
        Rattachement::SansPreuve {
            origine: "test",
            mac: Some("800a805c2689"),
        },
    )
    .unwrap();

    assert!(pure_force(&db, id), "même appareil : PURE verrouillé reste");
    assert!(repo.get(id).unwrap().unwrap().fixed_volume);
    assert_eq!(volume(&repo, id), 100.0);
    assert_eq!(
        lire(&db, &format!("zone_{id}_gain_trim_db")).as_deref(),
        Some("4")
    );
}
