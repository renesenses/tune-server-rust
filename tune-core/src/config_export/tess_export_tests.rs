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

// ── T17 : préréglages de chaîne ─────────────────────────────────────

fn liste_soiree() -> crate::tess::PreReglages {
    let mut l = crate::tess::PreReglages::default();
    l.apply_mode = crate::tess::ModeApplication::AtNextPlayback;
    l.presets.push(crate::tess::PreReglage {
        id: "evening-listening".into(),
        name: "Evening listening".into(),
        builtin: false,
        chain: ChaineTess {
            plugins: vec![crate::tess::MaillonGreffon {
                id: "crossfeed".into(),
                enabled: true,
                settings: json!({"amount": 0.4, "delay_ms": 0.3}),
            }],
            ..ChaineTess::default()
        },
    });
    l
}

#[test]
fn les_prereglages_et_le_prereglage_actif_suivent_l_export_t17() {
    let (src, id) = depart();
    let s = SettingsRepo::with_backend(src.clone());
    crate::tess::prereglages::ecrire(&s, &liste_soiree()).unwrap();
    s.set(
        &format!("zone_{id}_tess_active_preset"),
        "evening-listening",
    )
    .unwrap();
    let fichier = exporter(&src, false).unwrap();
    assert!(fichier["settings"]["tess_presets"].is_object(), "{fichier}");

    let dst = base();
    // Une zone déjà là décale les numéros : le préréglage actif doit suivre
    // la zone, pas son ancien numéro.
    ZoneRepo::with_backend(dst.clone())
        .create("Autre", Some("local"), Some("local:autre"))
        .unwrap();
    let (_plan, bilan) = importer(&dst, &fichier);
    let d = SettingsRepo::with_backend(dst.clone());
    assert_eq!(crate::tess::prereglages::lire(&d).unwrap(), liste_soiree());
    let arrivee = bilan
        .zones_creees
        .first()
        .copied()
        .expect("zone créée à l'import");
    assert_ne!(arrivee, id);
    assert_eq!(
        d.get(&format!("zone_{arrivee}_tess_active_preset"))
            .unwrap()
            .as_deref(),
        Some("evening-listening")
    );
    assert!(
        !bilan.avertissements.join(" | ").contains("TESS presets"),
        "{:?}",
        bilan.avertissements
    );
}

#[test]
fn une_liste_de_prereglages_incoherente_n_ecrase_pas_celle_de_la_machine_t17() {
    let dst = base();
    let d = SettingsRepo::with_backend(dst.clone());
    crate::tess::prereglages::ecrire(&d, &liste_soiree()).unwrap();
    // Une liste qui ne se relit pas du tout (`presets` n'est pas une liste).
    let illisible = json!({"apply_mode": "immediately", "presets": "pas une liste"});
    let fichier = json!({
        "format": "tune-config", "format_version": 2,
        "settings": { "tess_presets": illisible },
        "zones": [],
    });
    let (_plan, bilan) = importer(&dst, &fichier);
    assert_eq!(crate::tess::prereglages::lire(&d).unwrap(), liste_soiree());
    let dit = bilan.avertissements.join(" | ");
    assert!(dit.contains("TESS presets are not readable"), "{dit}");

    // Contre-épreuve : la même liste sans le doublon est importée.
    let mut seule = liste_soiree();
    seule.presets.remove(0);
    let fichier = json!({
        "format": "tune-config", "format_version": 2,
        "settings": { "tess_presets": serde_json::to_value(&seule).unwrap() },
        "zones": [],
    });
    let (_plan, bilan) = importer(&dst, &fichier);
    assert_eq!(crate::tess::prereglages::lire(&d).unwrap(), seule);
    assert!(!bilan.avertissements.join(" | ").contains("TESS presets"));
}

#[test]
fn un_export_se_reimporte_avec_toutes_ses_entrees_meme_anciennes_suite_t17() {
    let (src, id) = depart();
    let s = SettingsRepo::with_backend(src.clone());
    // En base : une entrée en Cf (U+200B) et une au nom trop long, que les
    // règles d'entrée d'aujourd'hui refuseraient, plus un doublon.
    let mut l = liste_soiree();
    let mut cf = l.presets[1].clone();
    cf.id = "cf".into();
    cf.name = "Soi\u{200B}rée".into();
    let mut long = l.presets[1].clone();
    long.id = "long".into();
    long.name = "x".repeat(crate::tess::prereglages::NOM_MAX + 1);
    l.presets.push(cf);
    l.presets.push(long);
    l.presets.push(l.presets[1].clone());
    s.set("tess_presets", &serde_json::to_string(&l).unwrap())
        .unwrap();
    s.set(&format!("zone_{id}_tess_active_preset"), "cf")
        .unwrap();
    let fichier = exporter(&src, false).unwrap();

    let dst = base();
    let (_plan, bilan) = importer(&dst, &fichier);
    let d = SettingsRepo::with_backend(dst.clone());
    // Toutes les entrées sont là, dans l'ordre, sans avertissement.
    assert_eq!(crate::tess::prereglages::lire(&d).unwrap(), l);
    let dit = bilan.avertissements.join(" | ");
    assert!(!dit.contains("TESS"), "{dit}");
    // Le marqueur suit la zone (son préréglage existe).
    let arrivee = bilan.zones_creees[0];
    assert_eq!(
        d.get(&format!("zone_{arrivee}_tess_active_preset"))
            .unwrap()
            .as_deref(),
        Some("cf")
    );
}

#[test]
fn un_marqueur_vers_un_prereglage_absent_est_ecarte_a_l_import_suite_t17() {
    let (src, id) = depart();
    let s = SettingsRepo::with_backend(src.clone());
    crate::tess::prereglages::ecrire(&s, &liste_soiree()).unwrap();
    s.set(&format!("zone_{id}_tess_active_preset"), "disparu")
        .unwrap();
    let fichier = exporter(&src, false).unwrap();
    let dst = base();
    let (_plan, bilan) = importer(&dst, &fichier);
    let d = SettingsRepo::with_backend(dst.clone());
    let arrivee = bilan.zones_creees[0];
    assert_eq!(
        d.get(&format!("zone_{arrivee}_tess_active_preset"))
            .unwrap(),
        None
    );
    let dit = bilan.avertissements.join(" | ");
    assert!(
        dit.contains("active TESS preset \"disparu\" does not exist"),
        "{dit}"
    );
    // Contre-épreuve : vers un préréglage présent, il est gardé.
    let (src, id) = depart();
    let s = SettingsRepo::with_backend(src.clone());
    crate::tess::prereglages::ecrire(&s, &liste_soiree()).unwrap();
    s.set(
        &format!("zone_{id}_tess_active_preset"),
        "evening-listening",
    )
    .unwrap();
    let dst = base();
    let (_plan, bilan) = importer(&dst, &exporter(&src, false).unwrap());
    let d = SettingsRepo::with_backend(dst.clone());
    let arrivee = bilan.zones_creees[0];
    assert_eq!(
        d.get(&format!("zone_{arrivee}_tess_active_preset"))
            .unwrap()
            .as_deref(),
        Some("evening-listening")
    );
}

#[test]
fn un_import_de_plus_de_mille_prereglages_est_refuse_suite_t17() {
    let dst = base();
    let d = SettingsRepo::with_backend(dst.clone());
    crate::tess::prereglages::ecrire(&d, &liste_soiree()).unwrap();
    let fichier_de = |n: usize| {
        let mut l = crate::tess::PreReglages::default();
        for i in 1..n {
            let mut p = liste_soiree().presets[1].clone();
            p.id = format!("p{i}");
            l.presets.push(p);
        }
        json!({
            "format": "tune-config", "format_version": 2,
            "settings": { "tess_presets": serde_json::to_value(&l).unwrap() },
            "zones": [],
        })
    };
    let max = crate::tess::prereglages::PREREGLAGES_IMPORT_MAX;
    let (_plan, bilan) = importer(&dst, &fichier_de(max + 1));
    assert_eq!(crate::tess::prereglages::lire(&d).unwrap(), liste_soiree());
    let dit = bilan.avertissements.join(" | ");
    assert!(dit.contains("more than 1000"), "{dit}");
    // Contre-épreuve : 1000 tout juste, importés (au-delà des 200 de la route).
    let (_plan, bilan) = importer(&dst, &fichier_de(max));
    assert_eq!(
        crate::tess::prereglages::lire(&d).unwrap().presets.len(),
        max
    );
    assert!(!bilan.avertissements.join(" | ").contains("TESS"));
}

#[test]
fn un_pure_modifie_a_l_import_est_remplace_par_le_pure_standard_suite_t17() {
    let dst = base();
    let d = SettingsRepo::with_backend(dst.clone());
    let mut l = liste_soiree();
    l.presets[0].chain = l.presets[1].chain.clone();
    l.presets[0].name = "Pure (mien)".into();
    let fichier = json!({
        "format": "tune-config", "format_version": 2,
        "settings": { "tess_presets": serde_json::to_value(&l).unwrap() },
        "zones": [],
    });
    let (_plan, bilan) = importer(&dst, &fichier);
    let lue = crate::tess::prereglages::lire(&d).unwrap();
    assert_eq!(lue.presets[0], crate::tess::PreReglage::pure());
    assert_eq!(lue.presets[1], l.presets[1], "le reste est importé");
    let dit = bilan.avertissements.join(" | ");
    assert!(dit.contains("standard Pure preset is kept"), "{dit}");
    // Contre-épreuve : un Pure intact passe sans avertissement.
    let (_plan, bilan) = importer(
        &dst,
        &json!({
            "format": "tune-config", "format_version": 2,
            "settings": { "tess_presets": serde_json::to_value(liste_soiree()).unwrap() },
            "zones": [],
        }),
    );
    assert!(!bilan.avertissements.join(" | ").contains("TESS"));
    assert_eq!(crate::tess::prereglages::lire(&d).unwrap(), liste_soiree());
}
