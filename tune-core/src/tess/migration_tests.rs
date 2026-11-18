//! Preuves de T02 : migration sans perte d'une base réaliste générée,
//! aller-retour ancienne route → TESS → ancienne route, écriture croisée dans
//! les deux sens, refus explicite d'une clé corrompue, idempotence, P2.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::audio::eq::{EqBandSpec, EqProfile, HeadroomMode};
use crate::audio::surechantillonnage::greffon::OFFERED_FILTERS;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::tess::{
    ChaineTess, ContexteValidation, Dither, Headroom, MaillonGreffon, TauxDsd, Upsampling,
    catalogue_de_reference, cle_chaine_zone,
};

fn base() -> (Arc<dyn DbBackend>, SettingsRepo) {
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let settings = SettingsRepo::with_backend(backend.clone());
    (backend, settings)
}

fn tout(s: &SettingsRepo) -> BTreeMap<String, String> {
    s.all().unwrap().into_iter().collect()
}

/// Toutes les clés sauf celles de TESS (chaîne, ombre, sauvegarde).
fn hors_tess(s: &SettingsRepo) -> BTreeMap<String, String> {
    tout(s)
        .into_iter()
        .filter(|(k, _)| {
            !k.ends_with("_tess_chain")
                && !k.ends_with("_tess_chain_sync")
                && !k.ends_with("_eq_profile_before_tess")
        })
        .collect()
}

fn ids(c: &ChaineTess) -> Vec<(&str, bool)> {
    c.plugins
        .iter()
        .map(|m| (m.id.as_str(), m.enabled))
        .collect()
}

fn maillon<'a>(c: &'a ChaineTess, id: &str) -> &'a MaillonGreffon {
    c.plugins
        .iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("{id} absent de {:?}", ids(c)))
}

fn bande(freq: f64, gain: f64, q: f64, t: &str, channel: Option<u16>) -> EqBandSpec {
    EqBandSpec {
        freq,
        gain,
        q,
        band_type: t.into(),
        channel,
    }
}

fn profil(enabled: bool, bands: Vec<EqBandSpec>) -> EqProfile {
    EqProfile {
        enabled,
        bands,
        ..EqProfile::default()
    }
}

fn graphique(n: u32, gains: impl Fn(usize) -> f64, canaux: &[Option<u16>]) -> Vec<EqBandSpec> {
    let (grille, q) = grille_graphique(n);
    canaux
        .iter()
        .flat_map(|c| {
            grille
                .iter()
                .enumerate()
                .map(|(i, &f)| bande(f, gains(i), q, "peak", *c))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn parametrique() -> Vec<EqBandSpec> {
    vec![
        bande(80.0, 4.0, 0.7, "low_shelf", None),
        bande(1200.0, -3.5, 1.4, "peak", None),
        bande(3300.0, 2.5, 2.0, "peak", Some(0)),
        bande(9000.0, -2.0, 0.7, "high_shelf", None),
    ]
}

// ── La base réaliste générée ────────────────────────────────────────

/// Une variante de chaque réglage connu ; la zone `i` prend la variante
/// `i % n` de chaque dimension (longueurs différentes : toutes les variantes
/// apparaissent, et beaucoup de combinaisons).
const ZONES: i64 = 72;

fn eq_variante(i: i64, zone: i64) -> Option<(EqProfile, &'static str)> {
    let p = match i % 12 {
        0 => return None,
        1 => (
            EqProfile {
                enabled: true,
                bass_gain_db: 3.0,
                treble_gain_db: -1.5,
                ..EqProfile::default()
            },
            ID_EQ_GRAPHIQUE,
        ),
        2 => (
            EqProfile {
                enabled: false,
                mid_gain_db: 2.0,
                ..EqProfile::default()
            },
            ID_EQ_GRAPHIQUE,
        ),
        3 => (
            profil(true, graphique(10, |k| k as f64 - 4.0, &[None])),
            ID_EQ_GRAPHIQUE,
        ),
        4 => (
            profil(true, graphique(15, |k| (k % 5) as f64, &[None])),
            ID_EQ_GRAPHIQUE,
        ),
        5 => (
            profil(true, graphique(31, |k| -((k % 7) as f64), &[None])),
            ID_EQ_GRAPHIQUE,
        ),
        6 => (
            profil(true, graphique(10, |k| (k % 3) as f64, &[Some(0), Some(1)])),
            ID_EQ_GRAPHIQUE,
        ),
        7 => (profil(true, parametrique()), ID_EQ_PARAMETRIQUE),
        8 => (profil(false, parametrique()), ID_EQ_PARAMETRIQUE),
        9 => (
            crate::room_correction::profil_eq_de_correction(&filtres_de_piece()),
            ID_CORRECTION_PIECE,
        ),
        10 => (
            EqProfile {
                enabled: true,
                headroom_mode: HeadroomMode::Realistic,
                bands: parametrique(),
                ..EqProfile::default()
            },
            ID_EQ_PARAMETRIQUE,
        ),
        _ => (
            profil(false, graphique(10, |_| 0.0, &[None])),
            ID_EQ_GRAPHIQUE,
        ),
    };
    let _ = zone;
    Some(p)
}

fn filtres_de_piece() -> Vec<crate::room_correction::CorrectionFilter> {
    use crate::room_correction::{CorrectionFilter, FilterType};
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

fn crossfeed_variante(i: i64) -> Option<Value> {
    match i % 5 {
        0 => None,
        1 => Some(json!({"enabled": true, "amount": 0.3, "delay_ms": 0.3})),
        2 => Some(json!({"enabled": false, "amount": 0.55, "delay_ms": 0.45})),
        3 => Some(json!({"enabled": true, "amount": 0.4, "delay_ms": 0.3,
                         "head_shadow_enabled": true, "cutoff_hz": 700.0,
                         "slope_db_per_octave": 4.5})),
        _ => Some(json!({"enabled": true, "level": "medium", "amount": 0.2})),
    }
}

fn remap_variante(i: i64) -> Option<Value> {
    match i % 3 {
        0 => None,
        1 => Some(json!({"enabled": true, "inputs": 2, "outputs": 2,
                         "gains_db": [[null, 0.0], [0.0, null]],
                         "normalize": true, "preset": "swap"})),
        _ => Some(json!({"enabled": false, "inputs": 2, "outputs": 2,
                         "gains_db": [[0.0, null], [null, 0.0]],
                         "normalize": true, "preset": null})),
    }
}

fn mono_variante(i: i64) -> Option<&'static str> {
    match i % 3 {
        0 => None,
        1 => Some("true"),
        _ => Some("false"),
    }
}

fn upsampler_variante(i: i64) -> Option<Value> {
    match i % 5 {
        0 => None,
        1 => Some(
            json!({"enabled": false, "filter": OFFERED_FILTERS[2], "max_rate_hz": null, "max_ratio": 4}),
        ),
        2 => Some(
            json!({"enabled": true, "filter": OFFERED_FILTERS[0], "max_rate_hz": null, "max_ratio": null}),
        ),
        3 => Some(
            json!({"enabled": true, "filter": OFFERED_FILTERS[3], "max_rate_hz": 192000, "max_ratio": 8}),
        ),
        _ => Some(
            json!({"enabled": true, "filter": OFFERED_FILTERS[5], "max_rate_hz": 384000, "max_ratio": null}),
        ),
    }
}

fn natifs_variante(i: i64) -> Vec<(&'static str, Value)> {
    match i % 6 {
        0 => vec![],
        1 => vec![(
            "crossfeed-pro",
            json!({"enabled": true, "profile": "hd800", "width": 0.6}),
        )],
        2 => vec![("crosstalk", json!({"enabled": true, "span_deg": 60}))],
        3 => vec![
            (
                "crossfeed-pro",
                json!({"enabled": true, "profile": "studio"}),
            ),
            ("crosstalk", json!({"enabled": true, "span_deg": 30})),
        ],
        4 => vec![("reverb-hall", json!({"enabled": false, "mix": 0.2}))],
        _ => vec![
            ("crossfeed-pro", json!({"enabled": false})),
            ("loudness-plus", json!({"enabled": true, "target": -18})),
        ],
    }
}

/// Pose la base : 72 zones anonymes, toutes les variantes, plus des
/// réglages de zone que la migration ne doit pas toucher.
fn semer(backend: &Arc<dyn DbBackend>, s: &SettingsRepo) -> Vec<i64> {
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    s.set("eq_expert_bands", "10").unwrap();
    let mut zones = Vec::new();
    for i in 0..ZONES {
        let appareil = format!("dev-{i:03}");
        let z = repo
            .create(
                &format!("Zone {i:03}"),
                Some("local"),
                Some(appareil.as_str()),
            )
            .unwrap();
        zones.push(z);
        if let Some((p, id)) = eq_variante(i, z) {
            if id == ID_CORRECTION_PIECE {
                let piece = crate::room_correction::RoomProfile {
                    name: format!("Room {i:03}"),
                    zone_id: z.to_string(),
                    filters: filtres_de_piece(),
                    created_at: "2026-11-18T00:00:00Z".into(),
                    measurement_data: None,
                };
                s.set(
                    &format!("room_profile_{z}"),
                    &serde_json::to_string(&piece).unwrap(),
                )
                .unwrap();
            }
            s.set(
                &format!("zone_{z}_eq_profile"),
                &serde_json::to_string(&p).unwrap(),
            )
            .unwrap();
        }
        if let Some(v) = crossfeed_variante(i) {
            s.set(&format!("zone_{z}_crossfeed"), &v.to_string())
                .unwrap();
        }
        if i % 2 == 1 {
            s.set(
                &format!("ir_path_{z}"),
                &format!("/data/ir/zone-{i:03}.wav"),
            )
            .unwrap();
        }
        if let Some(v) = remap_variante(i) {
            s.set(&format!("zone_{z}_channel_remap"), &v.to_string())
                .unwrap();
        }
        if let Some(v) = mono_variante(i) {
            s.set(&format!("zone_{z}_mono_downmix"), v).unwrap();
        }
        if let Some(v) = upsampler_variante(i) {
            s.set(&format!("zone_{z}_upsampler"), &v.to_string())
                .unwrap();
        }
        for (id, v) in natifs_variante(i) {
            s.set(&format!("zone_{z}_native_plugin_{id}"), &v.to_string())
                .unwrap();
        }
        match i % 3 {
            1 => s
                .set(&format!("zone_{z}_audiophile"), r#"{"enabled":true}"#)
                .unwrap(),
            2 => s
                .set(
                    &format!("zone_{z}_audiophile"),
                    r#"{"enabled":false,"lock_volume":true}"#,
                )
                .unwrap(),
            _ => {}
        }
        if i % 4 == 3 {
            s.set(&format!("zone_{z}_strict_bitperfect"), "true")
                .unwrap();
        }
        // Hors chaîne, jamais touchés.
        s.set(
            &format!("zone_{z}_gain_trim_db"),
            &format!("{}", (i % 7) as f64 - 3.5),
        )
        .unwrap();
        s.set(&format!("zone_{z}_volume_limit_safety"), "85")
            .unwrap();
    }
    zones
}

/// Ce que la chaîne doit contenir pour la zone `i`, déduit des variantes et
/// non du code de migration.
fn attendu(i: i64) -> Vec<(String, bool, Value)> {
    let mut v = Vec::new();
    let sans = |x: Value| {
        let mut m = x.as_object().unwrap().clone();
        m.remove("enabled");
        Value::Object(m)
    };
    if let Some((p, id)) = eq_variante(i, 0) {
        v.push((
            id.to_owned(),
            p.enabled,
            sans(serde_json::to_value(&p).unwrap()),
        ));
    }
    if i % 2 == 1 {
        v.push((
            ID_CONVOLUTION.into(),
            true,
            json!({"ir_path": format!("/data/ir/zone-{i:03}.wav")}),
        ));
    }
    if let Some(x) = crossfeed_variante(i) {
        v.push((ID_CROSSFEED.into(), x["enabled"] == json!(true), sans(x)));
    }
    let mut natifs = natifs_variante(i);
    natifs.sort_by_key(|(id, _)| *id);
    for (id, x) in natifs {
        v.push((id.into(), x["enabled"] == json!(true), sans(x)));
    }
    if mono_variante(i) == Some("true") {
        v.push((ID_MONO.into(), true, Value::Null));
    }
    if let Some(x) = remap_variante(i) {
        v.push((
            ID_REAFFECTATION.into(),
            x["enabled"] == json!(true),
            sans(x),
        ));
    }
    v
}

fn upsampling_attendu(i: i64) -> Upsampling {
    match upsampler_variante(i) {
        Some(x) if x["enabled"] == json!(true) => Upsampling::Pcm {
            filter: x["filter"].as_str().unwrap().into(),
            max_ratio: x["max_ratio"].as_u64().map(|r| r as u32),
            max_rate_hz: x["max_rate_hz"].as_u64().map(|r| r as u32),
        },
        _ => Upsampling::Off {},
    }
}

#[test]
fn une_base_realiste_est_migree_sans_perte() {
    let (backend, s) = base();
    let zones = semer(&backend, &s);
    let avant = hors_tess(&s);

    let bilans = migrer_toutes_les_zones(&backend, true).unwrap();
    assert_eq!(bilans.len(), zones.len());

    // 1. La migration n'a touché AUCUNE ancienne clé.
    assert_eq!(
        hors_tess(&s),
        avant,
        "la migration a modifié une ancienne clé"
    );

    let catalogue = catalogue_de_reference();
    for (i, &z) in zones.iter().enumerate() {
        let i = i as i64;
        let bilan = bilans
            .iter()
            .find(|(id, _)| *id == z)
            .unwrap()
            .1
            .as_ref()
            .unwrap_or_else(|e| panic!("zone {i} refusée : {e}"));
        let c = &bilan.chaine;
        assert!(bilan.ecrite);

        // 2. Chaque réglage est dans la chaîne, coché comme avant, réglages
        //    identiques, dans l'ordre de la lecture d'aujourd'hui.
        let obtenu: Vec<(String, bool, Value)> = c
            .plugins
            .iter()
            .map(|m| (m.id.clone(), m.enabled, m.settings.clone()))
            .collect();
        assert_eq!(obtenu, attendu(i), "zone {i}");
        assert_eq!(c.fixed.upsampling, upsampling_attendu(i), "zone {i}");
        assert_eq!(c.fixed.headroom, Headroom::Auto {});
        assert_eq!(c.fixed.quantization.dither, Dither::Tpdf);
        assert!(!c.bypass_all);

        // 3. La chaîne enregistrée est celle rendue.
        let brut = s.get(&cle_chaine_zone(z)).unwrap().unwrap();
        assert_eq!(&serde_json::from_str::<ChaineTess>(&brut).unwrap(), c);

        // 4. Pure : hors chaîne.
        let l = lire_zone(&s, z, true).unwrap();
        assert_eq!(l.pure, i % 3 == 1 || i % 4 == 3, "zone {i}");
        assert_eq!(&l.chaine, c);
        assert!(!l.depuis_les_anciennes_cles);

        // 5. Aucune chaîne migrée n'est refusée (le groupe crossfeed reste
        //    entièrement coché, politique `priority`) ; les paquets tiers
        //    absents du catalogue de référence mis à part.
        let mut sans_tiers = c.clone();
        sans_tiers
            .plugins
            .retain(|m| catalogue.get(&m.id).is_some());
        sans_tiers
            .valider(&catalogue, ContexteValidation { premium: true })
            .unwrap_or_else(|e| panic!("zone {i} : {e:?}"));
    }

    // 6. Sans perte : réécrire chaque chaîne migrée ne change AUCUNE
    //    ancienne clé (tout ce qu'elles disaient est dans la chaîne).
    for &z in &zones {
        let c = lire_zone(&s, z, true).unwrap().chaine;
        let b = ecrire_chaine(&s, z, &c).unwrap();
        assert!(
            b.cles_modifiees.is_empty(),
            "zone {z} : {:?}",
            b.cles_modifiees
        );
        assert!(!b.chaine_ecrite);
    }
    assert_eq!(hors_tess(&s), avant);
}

#[test]
fn la_base_couvre_toutes_les_combinaisons_connues() {
    // Le groupe crossfeed entier coché ensemble (§ 8) existe dans la base.
    let i = (0..ZONES)
        .find(|i| crossfeed_variante(*i).is_some_and(|x| x["enabled"] == json!(true)) && i % 6 == 3)
        .expect("une zone crossfeed + Crossfeed Pro + Crosstalk");
    let a = attendu(i);
    for id in ["crossfeed", "crossfeed-pro", "crosstalk"] {
        assert!(a.iter().any(|(x, on, _)| x == id && *on), "{id}");
    }
    // Chaque variante de chaque dimension apparaît.
    for (n, len) in [(12, 12), (5, 5), (3, 3), (6, 6), (2, 2), (4, 4)] {
        let vues: std::collections::BTreeSet<i64> = (0..ZONES).map(|i| i % n).collect();
        assert_eq!(vues.len(), len);
    }
}

#[test]
fn deux_migrations_valent_une() {
    let (backend, s) = base();
    semer(&backend, &s);
    migrer_toutes_les_zones(&backend, true).unwrap();
    let apres_une = tout(&s);
    let bilans = migrer_toutes_les_zones(&backend, true).unwrap();
    assert!(bilans.iter().all(|(_, b)| !b.as_ref().unwrap().ecrite));
    assert_eq!(tout(&s), apres_une);
    // Sans Premium aussi : la conversion P2 elle-même est idempotente.
    migrer_toutes_les_zones(&backend, false).unwrap();
    let apres_p2 = tout(&s);
    let bilans = migrer_toutes_les_zones(&backend, false).unwrap();
    assert!(bilans.iter().all(|(_, b)| !b.as_ref().unwrap().ecrite));
    assert_eq!(tout(&s), apres_p2);
}

// ── Aller-retour avec les anciennes routes ──────────────────────────

/// Ce que les anciennes routes écrivent, par leurs propres fonctions.
fn ecrire_comme_les_anciennes_routes(s: &SettingsRepo, z: i64) {
    crate::audio::surechantillonnage::enregistrer_les_reglages(
        s,
        z,
        &json!({"enabled": true, "filter": OFFERED_FILTERS[1], "max_ratio": 4}),
    )
    .unwrap();
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&profil(true, parametrique())).unwrap(),
    )
    .unwrap();
    s.set(
        &format!("zone_{z}_crossfeed"),
        r#"{"enabled":true,"amount":0.35,"delay_ms":0.3}"#,
    )
    .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    let mut remap = crate::audio::reaffectation_canaux::identite(2);
    remap.enabled = true;
    s.set(
        &crate::audio::reaffectation_canaux::cle_de_zone(z),
        &serde_json::to_string(&remap).unwrap(),
    )
    .unwrap();
    s.set(&format!("ir_path_{z}"), "/data/ir/salon.wav")
        .unwrap();
    s.set(
        &format!("zone_{z}_native_plugin_crosstalk"),
        r#"{"enabled":true,"span_deg":45}"#,
    )
    .unwrap();
}

#[test]
fn aller_retour_ancienne_route_tess_ancienne_route_identique() {
    let (_b, s) = base();
    let z = 4;
    ecrire_comme_les_anciennes_routes(&s, z);
    let avant = tout(&s);
    let up_avant = crate::audio::surechantillonnage::reglages_de_la_zone(&s, z);
    let remap_avant = crate::audio::reaffectation_canaux::lire(
        &s,
        &crate::audio::reaffectation_canaux::cle_de_zone(z),
    );

    // TESS lit, puis réécrit la chaîne telle quelle.
    let c = lire_zone(&s, z, true).unwrap().chaine;
    let b = ecrire_chaine(&s, z, &c).unwrap();
    assert!(b.cles_modifiees.is_empty(), "{:?}", b.cles_modifiees);
    assert!(b.chaine_ecrite);

    // Les anciennes clés sont identiques à l'octet près, et les anciennes
    // lectures rendent la même chose.
    let apres: BTreeMap<String, String> = tout(&s)
        .into_iter()
        .filter(|(k, _)| !k.ends_with("_tess_chain") && !k.ends_with("_tess_chain_sync"))
        .collect();
    assert_eq!(apres, avant);
    assert_eq!(
        crate::audio::surechantillonnage::reglages_de_la_zone(&s, z),
        up_avant
    );
    assert_eq!(
        crate::audio::reaffectation_canaux::lire(
            &s,
            &crate::audio::reaffectation_canaux::cle_de_zone(z)
        ),
        remap_avant
    );
    // Et TESS relit la même chaîne.
    assert_eq!(lire_zone(&s, z, true).unwrap().chaine, c);
}

#[test]
fn ecrire_via_tess_met_a_jour_les_anciennes_cles() {
    let (_b, s) = base();
    let z = 5;
    ecrire_comme_les_anciennes_routes(&s, z);
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    let eq_avant = s.get(&format!("zone_{z}_eq_profile")).unwrap().unwrap();

    // Ce que seul TESS sait dire.
    c.fixed.headroom = Headroom::Manual { db: -6.0 };
    c.fixed.quantization.dither = Dither::Off;
    c.plugins.insert(
        1,
        MaillonGreffon {
            id: "declick-live".into(),
            enabled: true,
            settings: Value::Null,
        },
    );
    // Ce que les anciennes clés disent aussi.
    c.plugins
        .iter_mut()
        .find(|m| m.id == ID_CROSSFEED)
        .unwrap()
        .enabled = false;
    c.plugins
        .retain(|m| m.id != ID_MONO && m.id != ID_CONVOLUTION);
    let eq = c
        .plugins
        .iter_mut()
        .find(|m| m.id == ID_EQ_PARAMETRIQUE)
        .unwrap();
    eq.id = ID_EQ_GRAPHIQUE.into();
    eq.settings = {
        let p = profil(true, graphique(10, |k| k as f64 / 2.0, &[None]));
        let mut v = serde_json::to_value(&p).unwrap();
        v.as_object_mut().unwrap().remove("enabled");
        v
    };
    c.fixed.upsampling = Upsampling::Pcm {
        filter: OFFERED_FILTERS[0].into(),
        max_ratio: Some(2),
        max_rate_hz: Some(96_000),
    };

    let b = ecrire_chaine(&s, z, &c).unwrap();
    let mut cles = b.cles_modifiees.clone();
    cles.sort();
    let mut voulues = vec![
        format!("zone_{z}_eq_profile"),
        format!("ir_path_{z}"),
        format!("zone_{z}_crossfeed"),
        format!("zone_{z}_mono_downmix"),
        format!("zone_{z}_upsampler"),
    ];
    voulues.sort();
    assert_eq!(cles, voulues);

    // Crossfeed décoché, réglages gardés ; mono effacé ; IR effacée.
    let cf: Value =
        serde_json::from_str(&s.get(&format!("zone_{z}_crossfeed")).unwrap().unwrap()).unwrap();
    assert_eq!(
        cf,
        json!({"enabled": false, "amount": 0.35, "delay_ms": 0.3})
    );
    assert!(s.get(&format!("zone_{z}_mono_downmix")).unwrap().is_none());
    assert!(s.get(&format!("ir_path_{z}")).unwrap().is_none());
    // L'égaliseur graphique est dans l'ancienne clé ; l'ancien profil est
    // sauvegardé.
    let p: EqProfile =
        serde_json::from_str(&s.get(&format!("zone_{z}_eq_profile")).unwrap().unwrap()).unwrap();
    assert!(p.enabled && est_courbe_graphique(&p.bands));
    assert_eq!(
        s.get(&cle_sauvegarde_eq(z)).unwrap().as_deref(),
        Some(eq_avant.as_str())
    );
    let up = crate::audio::surechantillonnage::reglages_de_la_zone(&s, z);
    assert!(up.enabled);
    assert_eq!(up.max_ratio, Some(2));
    assert_eq!(up.max_rate_hz, Some(96_000));

    // TESS relit exactement ce qu'il a écrit, ce que seul TESS sait dire
    // compris.
    assert_eq!(lire_zone(&s, z, true).unwrap().chaine, c);
}

#[test]
fn une_ancienne_route_apres_tess_est_lue_par_tess() {
    let (_b, s) = base();
    let z = 6;
    ecrire_comme_les_anciennes_routes(&s, z);
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    c.fixed.headroom = Headroom::Manual { db: -4.5 };
    c.fixed.upsampling = Upsampling::Dsd {
        rate: TauxDsd::Dsd128,
        filter: OFFERED_FILTERS[0].into(),
    };
    c.plugins.insert(
        0,
        MaillonGreffon {
            id: "declick-live".into(),
            enabled: false,
            settings: Value::Null,
        },
    );
    ecrire_chaine(&s, z, &c).unwrap();
    // L'ancien suréchantillonneur ne fait pas de DSD : il est éteint pour
    // les anciennes routes, la chaîne garde le DSD.
    assert!(!crate::audio::surechantillonnage::reglages_de_la_zone(&s, z).enabled);
    assert_eq!(lire_zone(&s, z, true).unwrap().chaine, c);

    // Une ancienne route change le crossfeed, efface le repli mono et un
    // réglage tiers.
    s.set(
        &format!("zone_{z}_crossfeed"),
        r#"{"enabled":true,"amount":0.6,"delay_ms":0.3}"#,
    )
    .unwrap();
    s.delete(&format!("zone_{z}_mono_downmix")).unwrap();
    s.delete(&format!("zone_{z}_native_plugin_crosstalk"))
        .unwrap();

    let relue = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(maillon(&relue, ID_CROSSFEED).settings["amount"], json!(0.6));
    assert!(
        relue
            .plugins
            .iter()
            .all(|m| m.id != ID_MONO && m.id != "crosstalk")
    );
    // Ce que seul TESS sait dire n'a pas bougé.
    assert_eq!(relue.fixed.headroom, Headroom::Manual { db: -4.5 });
    assert!(matches!(relue.fixed.upsampling, Upsampling::Dsd { .. }));
    assert_eq!(relue.plugins[0].id, "declick-live");

    // Une ancienne route rallume le suréchantillonneur : il l'emporte.
    crate::audio::surechantillonnage::enregistrer_les_reglages(
        &s,
        z,
        &json!({"enabled": true, "filter": OFFERED_FILTERS[2]}),
    )
    .unwrap();
    assert!(matches!(
        lire_zone(&s, z, true).unwrap().chaine.fixed.upsampling,
        Upsampling::Pcm { .. }
    ));
}

#[test]
fn headphone_eq_survit_a_l_aller_retour() {
    let (_b, s) = base();
    let z = 7;
    let mut c = ChaineTess::default();
    let mut v = serde_json::to_value(profil(true, parametrique())).unwrap();
    v.as_object_mut().unwrap().remove("enabled");
    c.plugins.push(MaillonGreffon {
        id: ID_EQ_CASQUE.into(),
        enabled: true,
        settings: v,
    });
    ecrire_chaine(&s, z, &c).unwrap();
    assert_eq!(lire_zone(&s, z, true).unwrap().chaine, c);

    // Une ancienne route retouche une bande : c'est toujours le casque.
    let mut p = profil(true, parametrique());
    p.bands[1].gain = -5.0;
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&p).unwrap(),
    )
    .unwrap();
    assert_eq!(
        lire_zone(&s, z, true).unwrap().chaine.plugins[0].id,
        ID_EQ_CASQUE
    );

    // Elle pose une courbe graphique : c'est le graphique.
    let g = profil(true, graphique(10, |_| 1.0, &[None]));
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&g).unwrap(),
    )
    .unwrap();
    assert_eq!(
        lire_zone(&s, z, true).unwrap().chaine.plugins[0].id,
        ID_EQ_GRAPHIQUE
    );
}

// ── Contre-épreuve : clé corrompue ──────────────────────────────────

#[test]
fn une_cle_corrompue_donne_un_refus_explicite_jamais_une_chaine_vide() {
    let z = 9;
    let cas: Vec<(String, &str)> = vec![
        (format!("zone_{z}_eq_profile"), "{oops"),
        (format!("zone_{z}_eq_profile"), "\"loudness\""),
        (
            format!("zone_{z}_eq_profile"),
            r#"{"enabled":true,"bands":"x"}"#,
        ),
        (format!("zone_{z}_crossfeed"), "[1,2]"),
        (format!("zone_{z}_crossfeed"), r#"{"enabled":"yes"}"#),
        (
            format!("zone_{z}_crossfeed"),
            r#"{"enabled":true,"amount":"loud"}"#,
        ),
        (format!("ir_path_{z}"), "  "),
        (format!("zone_{z}_mono_downmix"), "yes"),
        (
            format!("zone_{z}_upsampler"),
            r#"{"enabled":true,"filter":"nope"}"#,
        ),
        (
            format!("zone_{z}_upsampler"),
            r#"{"enabled":true,"max_ratio":3}"#,
        ),
        (
            format!("zone_{z}_channel_remap"),
            r#"{"enabled":true,"inputs":"two"}"#,
        ),
        (format!("zone_{z}_native_plugin_crosstalk"), "not json"),
        (format!("zone_{z}_audiophile"), "on"),
        (format!("zone_{z}_strict_bitperfect"), "1"),
        (format!("zone_{z}_tess_chain"), "{"),
        (format!("zone_{z}_tess_chain"), r#"{"plugins":[]}"#),
        (format!("zone_{z}_tess_chain_sync"), "[1]"),
    ];
    for (cle, valeur) in cas {
        let (_b, s) = base();
        // Une zone par ailleurs normale : le refus ne vient que de la clé.
        s.set(
            &format!("zone_{z}_crossfeed"),
            r#"{"enabled":true,"amount":0.3}"#,
        )
        .unwrap();
        s.set(&cle, valeur).unwrap();
        let avant = tout(&s);

        match lire_zone(&s, z, true) {
            Err(ErreurMigration::CleCorrompue { cle: c, raison }) => {
                assert_eq!(c, cle, "{valeur}");
                assert!(!raison.is_empty());
                let texte = ErreurMigration::CleCorrompue {
                    cle: c.clone(),
                    raison,
                }
                .to_string();
                assert!(texte.contains(&cle), "{texte}");
            }
            autre => panic!("{cle} = {valeur:?} : attendu un refus, obtenu {autre:?}"),
        }
        assert!(migrer_zone(&s, z, true).is_err(), "{cle}");
        // Rien n'a été écrit : ni chaîne vide, ni ombre.
        assert_eq!(tout(&s), avant, "{cle}");
    }
}

#[test]
fn temoin_les_memes_cles_lisibles_passent() {
    let (_b, s) = base();
    let z = 9;
    s.set(&format!("zone_{z}_mono_downmix"), "false").unwrap();
    s.set(&format!("zone_{z}_strict_bitperfect"), "false")
        .unwrap();
    s.set(&format!("zone_{z}_audiophile"), "{}").unwrap();
    s.set(&format!("zone_{z}_upsampler"), "null").unwrap();
    let l = lire_zone(&s, z, true).unwrap();
    assert!(l.chaine.plugins.is_empty());
    assert!(!l.pure);
    assert_eq!(l.chaine.fixed.upsampling, Upsampling::Off {});
}

#[test]
fn une_chaine_plus_recente_n_est_jamais_reecrite() {
    let (_b, s) = base();
    let z = 3;
    let brut =
        r#"{"version":2,"plugins":[{"id":"crossfeed","enabled":true,"settings":null}],"future":1}"#;
    s.set(&cle_chaine_zone(z), brut).unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    let l = lire_zone(&s, z, true).unwrap();
    assert_eq!(l.chaine.version, 2);
    assert!(!migrer_zone(&s, z, true).unwrap().ecrite);
    assert_eq!(s.get(&cle_chaine_zone(z)).unwrap().as_deref(), Some(brut));
    assert_eq!(
        ecrire_chaine(&s, z, &l.chaine),
        Err(ErreurMigration::VersionPlusRecente { trouvee: 2 })
    );
}

// ── P2 : paramétrique d'un compte gratuit ───────────────────────────

#[test]
fn p2_un_compte_gratuit_voit_son_parametrique_converti_au_plus_pres() {
    let (_b, s) = base();
    let z = 11;
    let original = profil(true, parametrique());
    let brut = serde_json::to_string(&original).unwrap();
    s.set(&format!("zone_{z}_eq_profile"), &brut).unwrap();

    // Premium : rien ne change.
    let premium = lire_zone(&s, z, true).unwrap();
    assert!(!premium.conversion_p2);
    assert_eq!(premium.chaine.plugins[0].id, ID_EQ_PARAMETRIQUE);

    // Gratuit : graphique, sur la grille réglée (10), deux courbes puisqu'une
    // bande ne vise que le canal gauche.
    let b = migrer_zone(&s, z, false).unwrap();
    assert!(b.conversion_p2);
    let m = &b.chaine.plugins[0];
    assert_eq!(m.id, ID_EQ_GRAPHIQUE);
    assert!(m.enabled);
    let mut v = m.settings.clone();
    v.as_object_mut()
        .unwrap()
        .insert("enabled".into(), json!(true));
    let converti: EqProfile = serde_json::from_value(v).unwrap();
    assert!(est_courbe_graphique(&converti.bands));
    assert_eq!(converti.bands.len(), 20);
    assert!(converti.bands.iter().all(|b| b.gain.abs() <= 12.0));

    // Au plus près : aux fréquences de la grille, la courbe graphique suit
    // la paramétrique à moins de 1 dB.
    let (grille, _) = grille_graphique(10);
    let cible = courbe_db(&original, grille);
    let obtenu = courbe_db(&converti, grille);
    for ch in 0..2 {
        for i in 0..grille.len() {
            let e = (cible[ch][i] - obtenu[ch][i]).abs();
            assert!(e < 1.0, "canal {ch}, {} Hz : écart {e:.2} dB", grille[i]);
        }
    }

    // La migration n'a pas touché l'ancienne clé : la lecture d'aujourd'hui
    // ne change pas avant T03/T19.
    assert_eq!(
        s.get(&format!("zone_{z}_eq_profile")).unwrap().as_deref(),
        Some(brut.as_str())
    );
    // Écrite via TESS, la conversion passe dans l'ancienne clé ; l'original
    // est sauvegardé.
    ecrire_chaine(&s, z, &b.chaine).unwrap();
    assert_eq!(
        s.get(&cle_sauvegarde_eq(z)).unwrap().as_deref(),
        Some(brut.as_str())
    );
    let ecrit: EqProfile =
        serde_json::from_str(&s.get(&format!("zone_{z}_eq_profile")).unwrap().unwrap()).unwrap();
    assert!(est_courbe_graphique(&ecrit.bands));
}

#[test]
fn p2_suit_la_grille_reglee_et_une_courbe_liee() {
    let (_b, s) = base();
    let z = 12;
    s.set("eq_expert_bands", "31").unwrap();
    let mut p = profil(true, parametrique());
    for b in &mut p.bands {
        b.channel = None;
    }
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&p).unwrap(),
    )
    .unwrap();
    let l = lire_zone(&s, z, false).unwrap();
    assert!(l.conversion_p2);
    let bands: Vec<EqBandSpec> =
        serde_json::from_value(l.chaine.plugins[0].settings["bands"].clone()).unwrap();
    assert_eq!(bands.len(), 31);
    assert!(bands.iter().all(|b| b.channel.is_none() && b.q == 4.32));
}

#[test]
fn p2_un_parametrique_decoche_reste_tel_quel() {
    let (_b, s) = base();
    let z = 13;
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&profil(false, parametrique())).unwrap(),
    )
    .unwrap();
    let l = lire_zone(&s, z, false).unwrap();
    assert!(!l.conversion_p2);
    assert_eq!(ids(&l.chaine), vec![(ID_EQ_PARAMETRIQUE, false)]);
    // Un greffon Premium DÉCOCHÉ ne demande aucun droit : pas de 402.
    l.chaine
        .valider(
            &catalogue_de_reference(),
            ContexteValidation { premium: false },
        )
        .unwrap();
}

#[test]
fn p2_un_graphique_ou_des_curseurs_ne_sont_jamais_convertis() {
    for p in [
        EqProfile {
            enabled: true,
            bass_gain_db: 4.0,
            ..EqProfile::default()
        },
        profil(true, graphique(15, |k| k as f64 / 3.0, &[None])),
    ] {
        let (_b, s) = base();
        let brut = serde_json::to_string(&p).unwrap();
        s.set("zone_2_eq_profile", &brut).unwrap();
        let l = lire_zone(&s, 2, false).unwrap();
        assert!(!l.conversion_p2);
        let mut v = l.chaine.plugins[0].settings.clone();
        v.as_object_mut()
            .unwrap()
            .insert("enabled".into(), json!(true));
        assert_eq!(v, serde_json::from_str::<Value>(&brut).unwrap());
    }
}

// ── Export de configuration ─────────────────────────────────────────

#[test]
fn config_export_inclut_la_chaine() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let a = repo.create("Salon", Some("local"), Some("dev-a")).unwrap();
    let b = repo.create("Bureau", Some("local"), Some("dev-b")).unwrap();
    s.set(
        &format!("zone_{a}_crossfeed"),
        r#"{"enabled":true,"amount":0.3}"#,
    )
    .unwrap();
    s.set(&format!("zone_{b}_mono_downmix"), "true").unwrap();
    // La zone b est migrée et porte un réglage que seul TESS sait dire.
    let mut c = lire_zone(&s, b, true).unwrap().chaine;
    c.fixed.headroom = Headroom::Manual { db: -2.0 };
    ecrire_chaine(&s, b, &c).unwrap();

    let export = crate::config_export::exporter(&backend, false).unwrap();
    let zone = |id: i64| {
        export["zones"]
            .as_array()
            .unwrap()
            .iter()
            .find(|z| z["id"] == json!(id))
            .unwrap()
            .clone()
    };
    // Zone jamais migrée : la chaîne est calculée pour l'export.
    let chaine_a: ChaineTess =
        serde_json::from_value(zone(a)["settings"]["zone_{id}_tess_chain"].clone()).unwrap();
    assert_eq!(ids(&chaine_a), vec![(ID_CROSSFEED, true)]);
    assert!(zone(a)["settings"]["zone_{id}_tess_chain_sync"].is_object());
    // Zone migrée : la chaîne enregistrée part telle quelle.
    let chaine_b: ChaineTess =
        serde_json::from_value(zone(b)["settings"]["zone_{id}_tess_chain"].clone()).unwrap();
    assert_eq!(chaine_b, c);
    // L'export n'écrit rien.
    assert!(s.get(&cle_chaine_zone(a)).unwrap().is_none());
}
