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

// ---------------------------------------------------------------------------
// #6774 — décision de Bertrand du 23/10 : sans limite de confort, ni de
// sécurité, ni mémoire du volume fixe, le désarmement ramène la zone à 30 %
// du CURSEUR au plus, jamais relevée.
// ---------------------------------------------------------------------------

/// 🔴 avant / 🟢 après : PURE verrouillé à 100 % sans aucun repère — la zone
/// repart de 30 % (appareil réseau : 30 sur son échelle), pas de 100 %.
#[test]
fn pure_verrouille_sans_repere_repart_de_30_pour_cent() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 100.0).unwrap();
    poser_pure_verrouille(&db, id);
    repo.update_output_device(id, "uuid:b").unwrap();
    assert!(!pure_force(&db, id));
    assert_eq!(
        volume(&repo, id),
        30.0,
        "#6774 : sans limite ni mémoire, PURE désarmé laisse la zone à 100 % du nouvel appareil"
    );
}

/// Volume fixe armé SANS mémoire d'avant l'armement : 30 % aussi.
#[test]
fn volume_fixe_sans_memoire_repart_de_30_pour_cent() {
    let (repo, _db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 100.0).unwrap();
    repo.update_fixed_volume(id, true).unwrap();
    repo.update_output_device(id, "uuid:b").unwrap();
    assert!(!repo.get(id).unwrap().unwrap().fixed_volume);
    assert_eq!(volume(&repo, id), 30.0);
}

/// Jamais relevée : une zone à 12 % reste à 12 %.
#[test]
fn sans_repere_une_zone_plus_basse_n_est_jamais_relevee() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 12.0).unwrap();
    poser_pure_verrouille(&db, id);
    repo.update_output_device(id, "uuid:b").unwrap();
    assert_eq!(volume(&repo, id), 12.0);
}

/// Une limite présente garde la règle du 22/10 : le plus bas des repères,
/// même au-dessus de 30 %.
#[test]
fn avec_une_limite_la_regle_du_22_10_reste() {
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 100.0).unwrap();
    poser_pure_verrouille(&db, id);
    SettingsRepo::with_backend(db.clone())
        .set(&format!("zone_{id}_volume_limit_safety"), "0.6")
        .unwrap();
    repo.update_output_device(id, "uuid:b").unwrap();
    assert_eq!(volume(&repo, id), 60.0);
}

/// Sortie LOCALE (curseur `db60`, #6787) : 30 % du CURSEUR, soit −42 dB de
/// gain — pas 30 % de gain (−10,5 dB). Le curseur relu vaut 30 % au plus.
#[test]
fn sur_une_sortie_locale_30_pour_cent_du_curseur_db60() {
    use crate::audio::volume_scale::EchelleDeVolume::Perceptive;
    let (repo, db) = repo();
    let id = repo
        .create("Bureau", Some("local"), Some("local:DAC A"))
        .unwrap();
    repo.update_volume(id, 100.0).unwrap();
    poser_pure_verrouille(&db, id);
    repo.update_output_device(id, "local:DAC B").unwrap();
    let gain = volume(&repo, id) / 100.0;
    let db_rendu = crate::audio::volume_scale::linear_to_db(gain).unwrap();
    assert!(
        (db_rendu + 42.0).abs() < 1e-6,
        "#6774 : 30 % du curseur db60 = −42 dB, reçu {db_rendu} dB"
    );
    assert!(Perceptive.position(gain) <= 0.30 + 1e-12);
}

/// Réseau → local, ou local → réseau : au moment du changement, l'échelle
/// retenue est la plus basse des deux (`output_type` n'est pas encore
/// réécrit) — jamais plus de 30 % du curseur de la nouvelle sortie.
#[test]
fn le_plafond_retient_l_echelle_la_plus_basse_des_deux_sorties() {
    use crate::audio::reglages_de_volume::plafond_sans_repere;
    let db60 = crate::audio::volume_scale::gain_perceptif(0.30);
    assert_eq!(plafond_sans_repere(Some("dlna"), "uuid:b"), 0.30);
    assert_eq!(plafond_sans_repere(Some("dlna"), "local:DAC"), db60);
    assert_eq!(plafond_sans_repere(Some("local"), "uuid:b"), db60);
    assert_eq!(plafond_sans_repere(Some("local"), "local:DAC"), db60);
    assert_eq!(plafond_sans_repere(None, "browser:1"), db60);

    // Par le dépôt : une zone réseau à 100 % sous PURE qui passe sur une
    // sortie locale repart de −42 dB, pas de 30 % de gain.
    let (repo, db) = repo();
    let id = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
    repo.update_volume(id, 100.0).unwrap();
    poser_pure_verrouille(&db, id);
    repo.update_output_device(id, "local:DAC").unwrap();
    repo.update_output_type(id, "local").unwrap();
    assert!((volume(&repo, id) - db60 * 100.0).abs() < 1e-9);
}

/// #6787 (b) — une zone locale à 5 % de gain (curseur `db60` 57 %, −26 dB)
/// passe sur un appareil réseau : l'appareil repart de 5 sur son échelle, au
/// même dB nominal, et non de 57. `volume` est un gain : rien à convertir.
#[test]
fn une_zone_locale_passee_sur_un_appareil_reseau_ne_monte_pas_6787() {
    use crate::audio::volume_scale::EchelleDeVolume;
    let (repo, _db) = repo();
    let id = repo
        .create("Bureau", Some("local"), Some("local:DAC A"))
        .unwrap();
    repo.update_volume(id, 5.0).unwrap();
    let curseur_avant = EchelleDeVolume::Perceptive.position(0.05);
    assert!(curseur_avant > 0.56, "{curseur_avant}");
    repo.update_output_device(id, "uuid:renderer").unwrap();
    repo.update_output_type(id, "dlna").unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    assert_eq!(
        zone.volume, 5.0,
        "(b) l'appareil réseau doit repartir de 5, pas du curseur 57"
    );
    let echelle = EchelleDeVolume::de_la_sortie(zone.output_type.as_deref());
    assert_eq!(echelle, EchelleDeVolume::Lineaire);
    assert!(echelle.position(zone.volume / 100.0) <= 0.05 + 1e-12);
}
