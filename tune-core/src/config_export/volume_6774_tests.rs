//! Le chemin réel de l'import HTTP de configuration, distinct de config_backup.
use super::*;
use crate::db::{migrations::run_migrations, sqlite::SqliteDb};

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    Arc::new(db)
}
fn importer(db: &Arc<dyn DbBackend>, corps: Value) -> Bilan {
    let fichier = lire(corps.as_object().unwrap().clone()).unwrap();
    let plan = planifier(db, &fichier).unwrap();
    appliquer(db, &plan).unwrap()
}
fn fichier(zones: Value) -> Value {
    json!({"format":"tune-config","format_version":2,"settings":{},"zones":zones})
}

#[test]
fn le_format_ancien_ne_pose_pas_de_volume_sans_identite_6774() {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let id = repo
        .create("Marantz", Some("dlna"), Some("uuid:marantz-6774"))
        .unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    s.set(&format!("zone_{id}_volume_limit_safety"), "0.3")
        .unwrap();
    let bilan = importer(
        &db,
        json!({
            format!("zone_{id}_gain_trim_db"): "8.5",
            format!("zone_{id}_volume_limit_safety"): "1.0",
            format!("zone_{id}_audiophile"): "{\"enabled\":true,\"lock_volume\":true}",
            "theme":"sombre"
        }),
    );
    assert_eq!(
        s.get(&format!("zone_{id}_gain_trim_db")).unwrap(),
        None,
        "#6774 import HTTP ancien : trim +8,5 dB posé par numéro sans identité"
    );
    assert_eq!(
        s.get(&format!("zone_{id}_volume_limit_safety"))
            .unwrap()
            .as_deref(),
        Some("0.3")
    );
    assert_eq!(s.get(&format!("zone_{id}_audiophile")).unwrap(), None);
    assert_eq!(s.get("theme").unwrap().as_deref(), Some("sombre"));
    assert!(
        !bilan.avertissements.is_empty(),
        "l'écart doit être annoncé"
    );
}
#[test]
fn sans_appareil_le_format_v2_ne_restaure_aucun_reglage_de_volume_6774() {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let id = repo
        .create("Salon sans identité", Some("dlna"), None)
        .unwrap();
    repo.update_volume(id, 20.0).unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    s.set(&format!("zone_{id}_gain_trim_db"), "8.5").unwrap();
    let bilan = importer(
        &db,
        fichier(
            json!([{"id":9999,"name":"Salon sans identité","output_type":"dlna","volume":100,
        "settings":{"zone_{id}_gain_trim_db":"12","zone_{id}_volume_limit_safety":"1","zone_{id}_audiophile":{"enabled":true,"lock_volume":true},"zone_{id}_crossfeed":{"enabled":true}}}]),
        ),
    );
    assert_eq!(
        repo.get(id).unwrap().unwrap().volume,
        20.0,
        "#6774 import HTTP sans identité : volume utilisateur remonté à 100"
    );
    for gabarit in crate::audio::reglages_de_volume::GABARITS_DE_VOLUME {
        assert_eq!(
            s.get(&gabarit.replace("{id}", &id.to_string())).unwrap(),
            None,
            "#6774 import HTTP sans identité : réglage de volume restauré ou laissé en place : {gabarit}"
        );
    }
    assert!(s.get(&format!("zone_{id}_crossfeed")).unwrap().is_some());
    assert!(!bilan.avertissements.is_empty());
}
#[test]
fn la_mac_unique_retrouve_la_zone_et_garde_son_identifiant_actuel_6774() {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let id = repo
        .create("Nom actuel", Some("dlna"), Some("uuid:nouveau-6774"))
        .unwrap();
    repo.set_identity(id, "192.0.2.50", Some("AA:BB:CC:11:22:33"))
        .unwrap();
    let bilan = importer(
        &db,
        fichier(
            json!([{"id":9999,"name":"Ancien nom","output_type":"dlna","output_device_id":"uuid:ancien-6774","mac":"aabbcc112233",
        "settings":{"zone_{id}_gain_trim_db":"-3"}}]),
        ),
    );
    assert!(
        bilan.zones_creees.is_empty(),
        "#6774 import HTTP : même MAC devenue une autre zone"
    );
    assert_eq!(
        repo.get(id).unwrap().unwrap().output_device_id.as_deref(),
        Some("uuid:nouveau-6774")
    );
    assert_eq!(
        SettingsRepo::with_backend(db)
            .get(&format!("zone_{id}_gain_trim_db"))
            .unwrap()
            .as_deref(),
        Some("-3")
    );
}
#[test]
fn deux_appareils_de_meme_nom_ne_partagent_pas_leurs_reglages_6774() {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let id = repo
        .create("Salon", Some("dlna"), Some("uuid:local-6774"))
        .unwrap();
    importer(
        &db,
        fichier(
            json!([{"id":id,"name":"Salon","output_type":"dlna","output_device_id":"uuid:autre-6774","settings":{"zone_{id}_gain_trim_db":"4.5"}}]),
        ),
    );
    assert_eq!(
        repo.get(id).unwrap().unwrap().output_device_id.as_deref(),
        Some("uuid:local-6774")
    );
    assert_eq!(
        SettingsRepo::with_backend(db.clone())
            .get(&format!("zone_{id}_gain_trim_db"))
            .unwrap(),
        None
    );
    let autre = repo.get_by_device_id("uuid:autre-6774").unwrap().unwrap();
    assert_ne!(autre.id, Some(id));
    assert_eq!(
        SettingsRepo::with_backend(db)
            .get(&format!("zone_{}_gain_trim_db", autre.id.unwrap()))
            .unwrap()
            .as_deref(),
        Some("4.5")
    );
}

#[test]
fn lexport_actuel_emporte_la_mac_pour_retrouver_le_meme_appareil_6774() {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let id = repo
        .create("Salon 6774", Some("dlna"), Some("uuid:export-6774"))
        .unwrap();
    repo.set_identity(id, "192.0.2.50", Some("AA:BB:CC:11:22:33"))
        .unwrap();
    let export = exporter(&db, false).unwrap();
    let zone = export["zones"]
        .as_array()
        .unwrap()
        .iter()
        .find(|z| z["id"] == id)
        .unwrap();
    assert_eq!(
        zone["mac"], "AA:BB:CC:11:22:33",
        "#6774 export HTTP : identité MAC absente du fichier à restaurer : {zone}"
    );
}

fn sauvegarde_a_20(
    appareil: Option<&str>,
) -> (
    Arc<dyn DbBackend>,
    i64,
    crate::config_backup::ConfigSnapshot,
) {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let id = repo
        .create("Salon Premium 6774", Some("dlna"), appareil)
        .unwrap();
    repo.update_volume(id, 20.0).unwrap();
    let archive = crate::config_backup::export_config(&db).unwrap();
    (db, id, archive)
}
#[test]
fn sauvegarde_premium_sans_identite_ne_restaure_pas_le_volume_6774() {
    for volume in [100.0, 0.0] {
        let (db, id, mut archive) = sauvegarde_a_20(None);
        archive.zones[0]["volume"] = json!(volume);
        crate::config_backup::import_config(&db, archive).unwrap();
        assert_eq!(
            ZoneRepo::with_backend(db).get(id).unwrap().unwrap().volume,
            20.0,
            "#6774 sauvegarde Premium sans identité : un volume a traversé le nom seul"
        );
    }
}
#[test]
fn sauvegarde_premium_sans_volume_valide_ne_pose_pas_50_par_defaut_6774() {
    for valeur in [
        None,
        Some(Value::Null),
        Some(json!("fort")),
        Some(json!(101)),
        Some(json!(-1)),
    ] {
        let (db, id, mut archive) = sauvegarde_a_20(Some("uuid:premium-6774"));
        match valeur {
            Some(v) => {
                archive.zones[0]["volume"] = v;
            }
            None => {
                archive.zones[0].as_object_mut().unwrap().remove("volume");
            }
        }
        crate::config_backup::import_config(&db, archive).unwrap();
        assert_eq!(
            ZoneRepo::with_backend(db).get(id).unwrap().unwrap().volume,
            20.0,
            "#6774 sauvegarde Premium : volume absent/invalide devenu niveau inventé"
        );
    }
    let (db, id, mut archive) = sauvegarde_a_20(Some("uuid:premium-6774"));
    archive.zones[0]["volume"] = json!(30.0);
    crate::config_backup::import_config(&db, archive).unwrap();
    assert_eq!(
        ZoneRepo::with_backend(db).get(id).unwrap().unwrap().volume,
        30.0,
        "une préférence valide du même appareil reste restaurable"
    );
}
