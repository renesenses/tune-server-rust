//! TESS T02, décision B du 19/11 : l'export de configuration garde la chaîne
//! TESS de chaque zone, et l'import la RÉÉCRIT par `ecrire_chaine` (origine
//! et marques recalculées, zone remappée, ombre jamais importée). Le profil
//! de pièce d'une zone est restauré comme MESURÉ, sous le numéro d'arrivée.
use super::*;
use crate::db::{migrations::run_migrations, sqlite::SqliteDb};
use crate::room_correction::{CorrectionFilter, FilterType, RoomProfile};
use crate::tess::migration::{ID_CORRECTION_PIECE, correction_estimee, ecrire_chaine, lire_zone};
use crate::tess::{ChaineTess, Dither, Headroom};

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    Arc::new(db)
}

fn importer(db: &Arc<dyn DbBackend>, corps: &Value) -> (Plan, Bilan) {
    let fichier = lire(corps.as_object().unwrap().clone()).unwrap();
    let plan = planifier(db, &fichier).unwrap();
    let bilan = appliquer(db, &plan).unwrap();
    (plan, bilan)
}

fn filtres() -> Vec<CorrectionFilter> {
    vec![
        CorrectionFilter {
            frequency_hz: 52.0,
            gain_db: -6.5,
            q_factor: 4.0,
            filter_type: FilterType::Peaking,
        },
        CorrectionFilter {
            frequency_hz: 118.0,
            gain_db: -3.0,
            q_factor: 2.5,
            filter_type: FilterType::Peaking,
        },
    ]
}

fn eq_de_la_piece() -> String {
    serde_json::to_string(&crate::room_correction::profil_eq_de_correction(&filtres())).unwrap()
}

fn chaine_stockee(s: &SettingsRepo, id: i64) -> ChaineTess {
    serde_json::from_str(
        &s.get(&format!("zone_{id}_tess_chain"))
            .unwrap()
            .expect("chaîne TESS écrite"),
    )
    .unwrap()
}

/// Le serveur de départ : une zone locale avec une correction de pièce
/// MESURÉE et des réglages que seul TESS sait dire (réserve manuelle,
/// dither à mise en forme du bruit).
fn depart() -> (Arc<dyn DbBackend>, i64) {
    let db = base();
    let id = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("local"), Some("local:hw:1"))
        .unwrap();
    crate::room_correction::save_profile(
        &db,
        &RoomProfile {
            name: "Calibration salon".into(),
            zone_id: id.to_string(),
            filters: filtres(),
            created_at: "2026-11-19T00:00:00Z".into(),
            measurement_data: None,
            origin: Some("measured".into()),
        },
    )
    .unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    s.set(&format!("zone_{id}_eq_profile"), &eq_de_la_piece())
        .unwrap();
    let mut c = lire_zone(&s, id, true).unwrap().chaine;
    assert!(
        c.plugins
            .iter()
            .any(|m| m.id == ID_CORRECTION_PIECE && m.enabled),
        "{c:?}"
    );
    c.fixed.headroom = Headroom::Manual { db: -4.5 };
    c.fixed.quantization.dither = Dither::NoiseShaped;
    ecrire_chaine(&s, id, &c).unwrap();
    (db, id)
}

/// Le serveur d'arrivée : deux zones existent déjà, la zone importée aura
/// donc un autre numéro qu'au départ.
fn arrivee() -> Arc<dyn DbBackend> {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    repo.create("Cuisine", Some("local"), Some("local:hw:7"))
        .unwrap();
    repo.create("Bureau", Some("local"), Some("local:hw:8"))
        .unwrap();
    db
}

fn zone_du_fichier<'a>(fichier: &'a mut Value) -> &'a mut Map<String, Value> {
    fichier["zones"][0]["settings"].as_object_mut().unwrap()
}

#[test]
fn aller_retour_la_chaine_tess_suit_la_zone_et_le_premium_reste_au_serveur() {
    let (src, id_src) = depart();
    let mut fichier = exporter(&src, false).unwrap();
    {
        let r = zone_du_fichier(&mut fichier);
        assert!(r.contains_key("zone_{id}_tess_chain"), "{r:?}");
        assert!(
            !r.contains_key("zone_{id}_tess_chain_sync"),
            "l'ombre ne doit plus partir"
        );
        // Le fichier MENT : la correction se dit « estimation » (Free),
        // dans la chaîne comme dans le profil, et porte une ombre piégée.
        let chaine = r.get_mut("zone_{id}_tess_chain").unwrap();
        for m in chaine["plugins"].as_array_mut().unwrap() {
            if m["id"] == ID_CORRECTION_PIECE {
                m["settings"]["origin"] = json!("estimation");
            }
        }
        let piece = r.get_mut("room_profile_{id}").expect("profil exporté");
        piece["origin"] = json!("estimation");
        piece["name"] = json!("Estimation 3");
        r.insert(
            "zone_{id}_tess_chain_sync".into(),
            json!({"eq_profile": "poison"}),
        );
    }

    let dst = arrivee();
    let (_plan, bilan) = importer(&dst, &fichier);
    let s = SettingsRepo::with_backend(dst.clone());
    let id = ZoneRepo::with_backend(dst.clone())
        .get_by_device_id("local:hw:1")
        .unwrap()
        .and_then(|z| z.id)
        .expect("zone importée");
    assert_ne!(id, id_src, "le test doit remapper la zone");
    assert_eq!(bilan.zones_creees, vec![id]);

    // Les réglages propres à TESS ont survécu, sous le numéro d'arrivée.
    let c = chaine_stockee(&s, id);
    assert_eq!(c.fixed.headroom, Headroom::Manual { db: -4.5 }, "{c:?}");
    assert_eq!(c.fixed.quantization.dither, Dither::NoiseShaped, "{c:?}");
    // Rien n'est écrit sous le numéro de départ (une autre zone, ici).
    assert_eq!(s.get(&format!("zone_{id_src}_tess_chain")).unwrap(), None);
    assert_eq!(s.get(&format!("room_profile_{id_src}")).unwrap(), None);

    // L'ombre est celle du serveur d'arrivée, jamais celle du fichier.
    let ombre = s
        .get(&format!("zone_{id}_tess_chain_sync"))
        .unwrap()
        .expect("ombre reconstruite");
    assert!(!ombre.contains("poison"), "ombre importée : {ombre}");

    // Origine recalculée : la correction mesurée reste Premium.
    let m = c
        .plugins
        .iter()
        .find(|m| m.id == ID_CORRECTION_PIECE)
        .expect("correction de pièce");
    assert_eq!(m.settings["origin"], json!("measured"), "{m:?}");
    assert!(!correction_estimee(m));
    let gratuit = lire_zone(&s, id, false).unwrap();
    assert!(
        gratuit.conversion_piece,
        "le fichier a levé le Premium de la correction de pièce"
    );
    let premium = lire_zone(&s, id, true).unwrap();
    assert!(!premium.conversion_piece);

    // Le profil de pièce est restauré MESURÉ, zone remappée, index cohérent.
    let piece: Value =
        serde_json::from_str(&s.get(&format!("room_profile_{id}")).unwrap().unwrap()).unwrap();
    assert_eq!(piece["origin"], json!("measured"), "{piece}");
    assert_eq!(piece["zone_id"], json!(id.to_string()), "{piece}");
    let index: Vec<String> =
        serde_json::from_str(&s.get("room_profile_index").unwrap().unwrap()).unwrap();
    assert_eq!(index, vec![id.to_string()]);

    // Plus d'avertissement « server-managed » dans un aller-retour.
    let dit = bilan.avertissements.join(" | ");
    assert!(!dit.contains("server-managed"), "{dit}");
    assert!(dit.contains("restored as measured"), "{dit}");

    // Témoin : réimporter le même fichier ne change plus rien.
    let (plan, _) = importer(&dst, &fichier);
    assert_eq!(plan.zones.len(), 1);
    assert_eq!(
        plan.zones[0].statut,
        Statut::Inchange,
        "{:?}",
        plan.zones[0].changements
    );
}

#[test]
fn le_profil_de_piece_d_une_zone_est_importe_comme_mesure() {
    let dst = arrivee();
    let estimation = json!({
        "name": "Estimation 9", "zone_id": "9", "origin": "estimation",
        "filters": serde_json::to_value(filtres()).unwrap(),
        "created_at": "2026-11-19T00:00:00Z", "measurement_data": null,
    });
    let fichier = json!({
        "format": "tune-config", "format_version": 2,
        "settings": { "room_profile_index": "[\"9\"]" },
        "zones": [{
            "id": 9, "name": "Salon", "output_type": "local",
            "output_device_id": "local:hw:1",
            "settings": {
                "room_profile_{id}": estimation,
                "zone_{id}_eq_profile": eq_de_la_piece(),
            },
        }],
    });
    let (_plan, bilan) = importer(&dst, &fichier);
    let s = SettingsRepo::with_backend(dst.clone());
    let id = ZoneRepo::with_backend(dst.clone())
        .get_by_device_id("local:hw:1")
        .unwrap()
        .and_then(|z| z.id)
        .unwrap();
    assert_ne!(id, 9);
    let piece: Value = serde_json::from_str(
        &s.get(&format!("room_profile_{id}"))
            .unwrap()
            .expect("profil de pièce restauré"),
    )
    .unwrap();
    assert_eq!(piece["origin"], json!("measured"), "{piece}");
    assert_eq!(piece["zone_id"], json!(id.to_string()));
    assert_eq!(s.get("room_profile_9").unwrap(), None);
    let index: Vec<String> =
        serde_json::from_str(&s.get("room_profile_index").unwrap().unwrap()).unwrap();
    assert_eq!(index, vec![id.to_string()], "index du fichier importé brut");
    let dit = bilan.avertissements.join(" | ");
    assert!(dit.contains("restored as measured"), "{dit}");
    assert!(!dit.contains("server-managed"), "{dit}");
    // Sans chaîne dans le fichier, la chaîne vient des anciennes clés, et la
    // correction reste Premium.
    assert!(lire_zone(&s, id, false).unwrap().conversion_piece);
}
