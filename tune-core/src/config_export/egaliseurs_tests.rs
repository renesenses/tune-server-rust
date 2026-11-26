//! rc14 — le profil d'égaliseur, les préréglages et la chaîne TESS à l'import
//! de configuration et à la restauration d'une sauvegarde suivent la règle de
//! leurs routes ([`super::egaliseurs`]).
use super::*;
use crate::audio::eq::{EqBandSpec, EqProfile};
use crate::db::{migrations::run_migrations, sqlite::SqliteDb};
use crate::tess::ChaineTess;
use crate::tess::migration::{
    ID_EQ_CASQUE, ID_EQ_GRAPHIQUE, ID_EQ_PARAMETRIQUE, ecrire_chaine, est_courbe_graphique,
    lire_zone, memes_bandes, profil_graphique_au_plus_pres,
};

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    Arc::new(db)
}

fn bande(freq: f64, gain: f64, q: f64, t: &str) -> EqBandSpec {
    serde_json::from_value(json!({"freq": freq, "gain": gain, "q": q, "type": t})).unwrap()
}

/// Un égaliseur paramétrique (Premium).
fn parametrique() -> EqProfile {
    let mut p = EqProfile {
        enabled: true,
        ..EqProfile::default()
    };
    p.bands = vec![
        bande(1234.0, 4.0, 2.2, "peak"),
        bande(57.0, -6.0, 5.0, "peak"),
    ];
    p
}

/// Une courbe de casque, telle qu'un import AutoEQ la produit.
fn casque() -> EqProfile {
    let mut p = EqProfile {
        enabled: true,
        ..EqProfile::default()
    };
    p.bands = vec![
        bande(105.0, 6.5, 0.7, "low_shelf"),
        bande(3100.0, -3.2, 2.0, "peak"),
        bande(10000.0, -2.0, 0.7, "high_shelf"),
    ];
    p
}

fn prereglage_casque(bandes: &[EqBandSpec], nom: &str) -> Value {
    json!({
        "id": format!("p-{nom}"), "name": nom, "eq_type": "parametric",
        "bands": bandes, "created_at": 1, "source": "autoeq",
    })
}

fn texte(p: &EqProfile) -> String {
    serde_json::to_string(p).unwrap()
}

/// Une zone locale « Salon » qui porte `profil`, sous la façade `facade`.
fn serveur(profil: &EqProfile, facade: &str, prereglages: &[Value]) -> (Arc<dyn DbBackend>, i64) {
    let db = base();
    let id = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("local"), Some("local:hw:1"))
        .unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    s.set(
        "eq_presets",
        &Value::Array(prereglages.to_vec()).to_string(),
    )
    .unwrap();
    s.set(&format!("zone_{id}_eq_profile"), &texte(profil))
        .unwrap();
    let mut c = lire_zone(&s, id, true).unwrap().chaine;
    for m in &mut c.plugins {
        if m.id == ID_EQ_PARAMETRIQUE || m.id == ID_EQ_GRAPHIQUE || m.id == ID_EQ_CASQUE {
            m.id = facade.to_string();
        }
    }
    ecrire_chaine(&s, id, &c).unwrap();
    (db, id)
}

/// Le serveur d'arrivée : une zone existe déjà, la zone importée aura un
/// autre numéro.
fn arrivee() -> Arc<dyn DbBackend> {
    let db = base();
    ZoneRepo::with_backend(db.clone())
        .create("Cuisine", Some("local"), Some("local:hw:7"))
        .unwrap();
    db
}

fn importer(db: &Arc<dyn DbBackend>, corps: &Value, premium: bool) -> Plan {
    let fichier = lire(corps.as_object().unwrap().clone()).unwrap();
    let plan = planifier(db, &fichier, premium).unwrap();
    appliquer(db, &plan).unwrap();
    plan
}

fn profil_de(db: &Arc<dyn DbBackend>) -> EqProfile {
    let id = ZoneRepo::with_backend(db.clone())
        .get_by_device_id("local:hw:1")
        .unwrap()
        .and_then(|z| z.id)
        .unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    serde_json::from_str(&s.get(&format!("zone_{id}_eq_profile")).unwrap().unwrap()).unwrap()
}

fn chaine_de(db: &Arc<dyn DbBackend>) -> ChaineTess {
    let id = ZoneRepo::with_backend(db.clone())
        .get_by_device_id("local:hw:1")
        .unwrap()
        .and_then(|z| z.id)
        .unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    serde_json::from_str(&s.get(&format!("zone_{id}_tess_chain")).unwrap().unwrap()).unwrap()
}

fn coche(c: &ChaineTess, id: &str) -> bool {
    c.plugins.iter().any(|m| m.id == id && m.enabled)
}

fn dit_premium(avertissements: &[String]) -> bool {
    avertissements
        .iter()
        .any(|a| a.contains("requires Premium"))
}

// ── Import de configuration (`config_export`) ──────────────────────

#[test]
fn import_gratuit_d_un_parametrique_le_rend_graphique_et_le_dit() {
    let (src, _) = serveur(&parametrique(), ID_EQ_PARAMETRIQUE, &[]);
    let fichier = exporter(&src, false).unwrap();
    let dst = arrivee();
    let plan = importer(&dst, &fichier, false);
    let p = profil_de(&dst);
    assert!(est_courbe_graphique(&p.bands), "{:?}", p.bands);
    let c = chaine_de(&dst);
    assert!(!coche(&c, ID_EQ_PARAMETRIQUE), "{c:?}");
    assert!(coche(&c, ID_EQ_GRAPHIQUE), "{c:?}");
    assert!(
        dit_premium(&plan.avertissements),
        "{:?}",
        plan.avertissements
    );
}

#[test]
fn import_premium_sur_premium_est_identique() {
    let (src, _) = serveur(&parametrique(), ID_EQ_PARAMETRIQUE, &[]);
    let fichier = exporter(&src, false).unwrap();
    let dst = arrivee();
    let plan = importer(&dst, &fichier, true);
    assert!(memes_bandes(&profil_de(&dst).bands, &parametrique().bands));
    assert!(coche(&chaine_de(&dst), ID_EQ_PARAMETRIQUE));
    assert!(
        !dit_premium(&plan.avertissements),
        "{:?}",
        plan.avertissements
    );
}

#[test]
fn import_gratuit_sur_gratuit_est_identique() {
    // Une courbe casque (préréglage AutoEQ) et un graphique.
    let casque = casque();
    let presets = [prereglage_casque(&casque.bands, "HD 650")];
    let (src, _) = serveur(&casque, ID_EQ_CASQUE, &presets);
    let fichier = exporter(&src, false).unwrap();
    let dst = arrivee();
    let plan = importer(&dst, &fichier, false);
    assert!(memes_bandes(&profil_de(&dst).bands, &casque.bands));
    assert!(coche(&chaine_de(&dst), ID_EQ_CASQUE));
    let s = SettingsRepo::with_backend(dst.clone());
    let apres: Value = serde_json::from_str(&s.get("eq_presets").unwrap().unwrap()).unwrap();
    assert_eq!(apres, Value::Array(presets.to_vec()));
    assert!(
        !dit_premium(&plan.avertissements),
        "{:?}",
        plan.avertissements
    );

    let graphique = profil_graphique_au_plus_pres(&parametrique(), 10);
    let (src, _) = serveur(&graphique, ID_EQ_GRAPHIQUE, &[]);
    let fichier = exporter(&src, false).unwrap();
    let dst = arrivee();
    let plan = importer(&dst, &fichier, false);
    assert!(memes_bandes(&profil_de(&dst).bands, &graphique.bands));
    assert!(coche(&chaine_de(&dst), ID_EQ_GRAPHIQUE));
    assert!(
        !dit_premium(&plan.avertissements),
        "{:?}",
        plan.avertissements
    );
}

/// Un préréglage « AutoEQ » que l'import AutoEQ ne produit pas (40 bandes)
/// perd sa marque sans le Premium ; avec, il revient tel quel.
#[test]
fn un_prereglage_casque_hors_bornes_perd_sa_marque_sans_premium() {
    let bandes: Vec<EqBandSpec> = (0..40)
        .map(|i| bande(30.0 + 400.0 * f64::from(i), 3.0, 1.0, "peak"))
        .collect();
    let presets = [prereglage_casque(&bandes, "Faux")];
    for premium in [false, true] {
        let (src, _) = serveur(&parametrique(), ID_EQ_PARAMETRIQUE, &presets);
        let fichier = exporter(&src, false).unwrap();
        let dst = arrivee();
        let plan = importer(&dst, &fichier, premium);
        let s = SettingsRepo::with_backend(dst.clone());
        let apres: Value = serde_json::from_str(&s.get("eq_presets").unwrap().unwrap()).unwrap();
        assert_eq!(apres[0]["source"].is_null(), !premium, "{apres}");
        assert_eq!(
            plan.avertissements
                .iter()
                .any(|a| a.contains("equalizer presets")),
            !premium,
            "{:?}",
            plan.avertissements
        );
    }
}

/// Réimporter son propre fichier ne change rien à ce que la zone porte déjà.
#[test]
fn import_gratuit_d_une_courbe_deja_en_service_la_garde() {
    let (src, _) = serveur(&parametrique(), ID_EQ_PARAMETRIQUE, &[]);
    let fichier = exporter(&src, false).unwrap();
    let plan = importer(&src, &fichier, false);
    assert!(memes_bandes(&profil_de(&src).bands, &parametrique().bands));
    assert!(coche(&chaine_de(&src), ID_EQ_PARAMETRIQUE));
    assert!(
        !dit_premium(&plan.avertissements),
        "{:?}",
        plan.avertissements
    );
}

// ── Restauration d'une sauvegarde (`config_backup`) ────────────────

fn restaurer(
    src: &Arc<dyn DbBackend>,
    dst: &Arc<dyn DbBackend>,
    premium: bool,
) -> crate::config_backup::ImportReport {
    let snapshot = crate::config_backup::export_config(src).unwrap();
    crate::config_backup::import_config_with_secret(dst, snapshot, None, premium).unwrap()
}

#[test]
fn sauvegarde_premium_restauree_sans_premium_perd_ses_bandes_premium() {
    let (src, _) = serveur(&parametrique(), ID_EQ_PARAMETRIQUE, &[]);
    let dst = arrivee();
    let r = restaurer(&src, &dst, false);
    let p = profil_de(&dst);
    assert!(est_courbe_graphique(&p.bands), "{:?}", p.bands);
    assert!(dit_premium(&r.warnings), "{:?}", r.warnings);
}

#[test]
fn sauvegarde_premium_restauree_avec_premium_est_identique() {
    let (src, _) = serveur(&parametrique(), ID_EQ_PARAMETRIQUE, &[]);
    let dst = arrivee();
    let r = restaurer(&src, &dst, true);
    assert!(memes_bandes(&profil_de(&dst).bands, &parametrique().bands));
    assert!(!dit_premium(&r.warnings), "{:?}", r.warnings);
}

#[test]
fn sauvegarde_gratuite_restauree_sans_premium_est_identique() {
    let casque = casque();
    let presets = [prereglage_casque(&casque.bands, "HD 650")];
    let (src, _) = serveur(&casque, ID_EQ_CASQUE, &presets);
    let dst = arrivee();
    let r = restaurer(&src, &dst, false);
    assert!(memes_bandes(&profil_de(&dst).bands, &casque.bands));
    let s = SettingsRepo::with_backend(dst.clone());
    let apres: Value = serde_json::from_str(&s.get("eq_presets").unwrap().unwrap()).unwrap();
    assert_eq!(apres, Value::Array(presets.to_vec()));
    assert!(!dit_premium(&r.warnings), "{:?}", r.warnings);
}

/// La section des anciens préréglages n'écrit que des clés de préréglage.
#[test]
fn la_section_des_prereglages_n_ecrit_que_des_prereglages() {
    let (src, _) = serveur(&casque(), ID_EQ_CASQUE, &[]);
    let dst = arrivee();
    let mut snapshot = crate::config_backup::export_config(&src).unwrap();
    snapshot.eq_presets.push(json!({
        "key": "zone_1_eq_profile", "value": texte(&parametrique()),
    }));
    let r = crate::config_backup::import_config_with_secret(&dst, snapshot, None, false).unwrap();
    let s = SettingsRepo::with_backend(dst.clone());
    let ecrit = s.get("zone_1_eq_profile").unwrap();
    assert!(
        ecrit.is_none_or(|t| !t.contains("1234")),
        "la section des préréglages a écrit un profil de zone"
    );
    assert!(
        r.warnings.iter().any(|a| a.contains("not a preset key")),
        "{:?}",
        r.warnings
    );
}
