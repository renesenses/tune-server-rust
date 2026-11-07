//! #7614 — témoins du côté hôte du suréchantillonneur.
use super::*;

fn settings() -> SettingsRepo {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    SettingsRepo::with_backend(std::sync::Arc::new(db))
}

fn verdict_tout_admis() -> VerdictMachine {
    let mut results = Vec::new();
    for f in greffon::OFFERED_FILTERS {
        for (de, vers) in [
            (44_100, 352_800),
            (48_000, 384_000),
            (44_100, 705_600),
            (48_000, 768_000),
        ] {
            results.push(MesureMachine {
                filter: f.to_string(),
                from_hz: de,
                to_hz: vers,
                realtime_factor: 10.0,
                admitted: true,
            });
        }
    }
    VerdictMachine {
        simd_path: "test".into(),
        margin: 3.0,
        results,
        measured_at: 0,
    }
}

fn installer(s: &SettingsRepo) {
    s.set("plugin_upsampler_installed", "true").unwrap();
    s.set("plugin_upsampler_enabled", "true").unwrap();
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
}

#[test]
fn desactive_par_defaut() {
    let s = settings();
    installer(&s);
    enregistrer_le_verdict(&s, &verdict_tout_admis()).unwrap();
    assert_eq!(reglages_de_la_zone(&s, 1), UpsamplerSettings::default());
    assert!(!UpsamplerSettings::default().enabled);
    assert_eq!(choix_de_la_zone(&s, true, false, 1), Err(Ecarte::Desactive));
}

#[test]
fn chaque_garde_ecarte_le_greffon() {
    let s = settings();
    enregistrer_les_reglages(&s, 1, &serde_json::json!({"enabled": true})).unwrap();
    // Greffon non installé.
    assert_eq!(
        choix_de_la_zone(&s, true, false, 1),
        Err(Ecarte::GreffonNonInstalle)
    );
    installer(&s);
    // Compte gratuit : jamais, même installé et activé.
    assert_eq!(
        choix_de_la_zone(&s, false, false, 1),
        Err(Ecarte::PremiumRequis)
    );
    // Sans test de la machine, rien n'est appliqué.
    assert_eq!(
        choix_de_la_zone(&s, true, false, 1),
        Err(Ecarte::TestDeLaMachineAbsent)
    );
    enregistrer_le_verdict(&s, &verdict_tout_admis()).unwrap();
    // PURE le contourne.
    assert_eq!(choix_de_la_zone(&s, true, true, 1), Err(Ecarte::Pure));
    let choix = choix_de_la_zone(&s, true, false, 1).expect("toutes les gardes levées");
    assert_eq!(choix.reglages.filter, "polyvalent");
    // Une autre zone garde son défaut.
    assert_eq!(choix_de_la_zone(&s, true, false, 2), Err(Ecarte::Desactive));
}

#[test]
fn reglage_refuse_n_est_pas_enregistre() {
    let s = settings();
    for refuse in [
        serde_json::json!({"enabled": true, "filter": "precision-longue"}),
        serde_json::json!({"enabled": true, "max_ratio": 3}),
        serde_json::json!({"enabled": true, "inconnu": 1}),
    ] {
        assert!(
            enregistrer_les_reglages(&s, 1, &refuse).is_err(),
            "{refuse}"
        );
    }
    assert!(s.get(&cle_de_zone(1)).unwrap().is_none());
}

#[test]
fn le_test_de_la_machine_borne_les_cadences() {
    let s = settings();
    installer(&s);
    enregistrer_les_reglages(&s, 1, &serde_json::json!({"enabled": true})).unwrap();
    let mut v = verdict_tout_admis();
    // La machine ne tient pas le polyvalent vers 705,6 kHz.
    for r in &mut v.results {
        if r.filter == "polyvalent" && r.to_hz == 705_600 {
            r.admitted = false;
        }
    }
    enregistrer_le_verdict(&s, &v).unwrap();
    let choix = choix_de_la_zone(&s, true, false, 1).unwrap();
    assert_eq!(
        choix.negocier(44_100, &[44_100, 176_400, 352_800, 705_600]),
        Some(352_800)
    );
    assert_eq!(choix.negocier(44_100, &[705_600]), None);
    assert_eq!(choix.negocier(48_000, &[384_000, 768_000]), Some(768_000));
    // Aucun multiple entier accepté : rien.
    assert_eq!(choix.negocier(44_100, &[48_000, 96_000]), None);
}

fn sinus(frames: usize, amplitude: f32) -> Vec<f32> {
    (0..frames)
        .flat_map(|n| {
            let t = n as f32 / 44_100.0;
            let x = amplitude * (std::f32::consts::TAU * 997.0 * t).sin();
            [x, -x]
        })
        .collect()
}

/// Longueur conservée : n trames d'entrée donnent n·L trames, plus la queue
/// du noyau rendue par `vider`, ni plus ni moins.
#[test]
fn longueur_du_signal_conservee() {
    let reglages = UpsamplerSettings {
        enabled: true,
        ..Default::default()
    };
    let mut etage = EtageSurechantillonneur::nouveau(&reglages, 44_100, 352_800, 2).unwrap();
    let entree = sinus(10_000, 0.5);
    let corps = etage.traiter(&entree);
    assert_eq!(corps.len(), entree.len() * 8);
    let queue = etage.vider();
    let latence = Instance::new(
        RateRequest::new(
            AudioFormat::new(44_100, ChannelLayout::Stereo, SampleEncoding::F32).unwrap(),
            352_800,
            64,
        )
        .unwrap(),
        "polyvalent",
    )
    .unwrap()
    .latency_frames();
    assert_eq!(queue.len(), latence as usize * 2);
}

/// Bertrand, 07/11 : le niveau ne bouge jamais pendant une piste. La sortie
/// ne dépend pas du découpage en blocs : le gain est fixé à l'ouverture.
#[test]
fn le_niveau_ne_depend_pas_du_decoupage() {
    let reglages = UpsamplerSettings {
        enabled: true,
        filter: "precision".into(),
        ..Default::default()
    };
    let entree = sinus(20_000, 0.9);
    let mut d_un_bloc = EtageSurechantillonneur::nouveau(&reglages, 44_100, 176_400, 2).unwrap();
    let reference = d_un_bloc.traiter(&entree);
    let mut en_morceaux = EtageSurechantillonneur::nouveau(&reglages, 44_100, 176_400, 2).unwrap();
    let mut rendu = Vec::new();
    let mut i = 0;
    for (k, taille) in [1usize, 7, 300, 4096, 5000, 13].iter().cycle().enumerate() {
        if i >= entree.len() {
            break;
        }
        let fin = (i + taille * 2).min(entree.len());
        rendu.extend(en_morceaux.traiter(&entree[i..fin]));
        i = fin;
        assert!(k < 10_000);
    }
    assert_eq!(rendu, reference);
}

/// Aucune hausse de niveau : la crête de sortie ne dépasse jamais celle de
/// l'entrée, et un plein niveau ne sature pas.
#[test]
fn aucune_crete_au_dessus_de_l_entree() {
    for filtre in greffon::OFFERED_FILTERS {
        let reglages = UpsamplerSettings {
            enabled: true,
            filter: filtre.into(),
            ..Default::default()
        };
        let mut etage = EtageSurechantillonneur::nouveau(&reglages, 44_100, 352_800, 2).unwrap();
        // fs/4 à 45° à 0 dBFS : +3,01 dB entre les échantillons.
        let entree: Vec<f32> = (0..8192)
            .map(|n| [1.0f32, 1.0, -1.0, -1.0][n % 4])
            .flat_map(|x| [x, x])
            .collect();
        let mut sortie = etage.traiter(&entree);
        sortie.extend(etage.vider());
        let crete = sortie.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(crete <= 1.0, "{filtre} : crête {crete}");
        assert!(etage.reserve_db() <= 0.0, "{filtre}");
    }
}
