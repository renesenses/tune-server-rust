//! Preuves de T02 : migration sans perte d'une base réaliste générée,
//! aller-retour ancienne route → TESS → ancienne route, écriture croisée dans
//! les deux sens, refus explicite d'une clé illisible et lecture tolérante
//! des booléens, idempotence, chaîne enregistrée neutre vis-à-vis du droit
//! (P2 à la lecture seulement), course avec une ancienne route, verrou par
//! zone, choix provisoires D1 et D2.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::audio::eq::{EqBandSpec, EqProfile, HeadroomMode};
use crate::audio::surechantillonnage::greffon::OFFERED_FILTERS;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::tess::{
    ChaineTess, ContexteExecution, ContexteValidation, ControleVolume, Dither, Headroom,
    MaillonGreffon, MotifEtape, SourceFlux, TauxDsd, TransportZone, Upsampling, VolumeZone,
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

/// Un profil de casque de forme AutoEQ (« ParametricEQ.txt ») : coupe-bas,
/// plateaux, cloches étroites.
fn autoeq() -> Vec<EqBandSpec> {
    vec![
        bande(20.0, 0.0, 0.7, "high_pass", None),
        bande(105.0, 6.5, 0.7, "low_shelf", None),
        bande(220.0, -2.1, 0.9, "peak", None),
        bande(1450.0, 1.8, 1.6, "peak", None),
        bande(3100.0, -3.4, 2.8, "peak", None),
        bande(6200.0, 4.2, 3.5, "peak", None),
        bande(10000.0, -1.5, 0.7, "high_shelf", None),
    ]
}

fn sans_enabled_v(x: Value) -> Value {
    let mut m = x.as_object().unwrap().clone();
    m.remove("enabled");
    Value::Object(m)
}

// ── La base réaliste générée ────────────────────────────────────────

/// 72 zones anonymes ; la zone `i` prend la variante `i % n` de chaque
/// dimension (longueurs différentes : toutes les variantes apparaissent, et
/// beaucoup de combinaisons). Une zone sur quatre sort sur le réseau.
const ZONES: i64 = 72;

fn reseau(i: i64) -> bool {
    i % 4 == 2
}

fn eq_variante(i: i64) -> Option<(EqProfile, &'static str)> {
    Some(match i % 12 {
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
                bands: autoeq(),
                ..EqProfile::default()
            },
            ID_EQ_PARAMETRIQUE,
        ),
        _ => (
            profil(false, graphique(10, |_| 0.0, &[None])),
            ID_EQ_GRAPHIQUE,
        ),
    })
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

/// Les réglages tiers, DANS l'ordre où la chaîne doit les ranger (écrit à
/// la main : Crossfeed Pro, puis Crosstalk, puis les autres paquets).
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
        // « amp-sim » se trie avant « crossfeed-pro » mais doit passer
        // après : le rang, pas l'alphabet.
        _ => vec![
            ("crossfeed-pro", json!({"enabled": false})),
            ("amp-sim", json!({"enabled": true, "target": -18})),
        ],
    }
}

/// Pose la base : 72 zones anonymes, toutes les variantes, plus des
/// réglages de zone que la migration ne doit pas toucher.
fn semer(backend: &Arc<dyn DbBackend>, s: &SettingsRepo) -> Vec<i64> {
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    s.set("eq_expert_bands", "10").unwrap();
    // Le crossfeed intégré et les deux paquets du groupe sont en service :
    // la priorité seule départage (décision B).
    installer(s, &["crossfeed", "crossfeed-pro", "crosstalk"]);
    let mut zones = Vec::new();
    for i in 0..ZONES {
        let appareil = if reseau(i) {
            format!("dlna:uuid-{i:03}")
        } else {
            format!("local:dev-{i:03}")
        };
        let type_ = if reseau(i) { "dlna" } else { "local" };
        let z = repo
            .create(
                &format!("Zone {i:03}"),
                Some(type_),
                Some(appareil.as_str()),
            )
            .unwrap();
        zones.push(z);
        if let Some((p, id)) = eq_variante(i) {
            if id == ID_CORRECTION_PIECE {
                let piece = crate::room_correction::RoomProfile {
                    // D1 : une zone de pièce sur deux est une estimation
                    // sans micro (C5), au nom par défaut de C5.
                    name: if estimation_c5(i) {
                        format!("Estimation sim-{i:03}")
                    } else {
                        format!("Calibration mes-{i:03}")
                    },
                    zone_id: z.to_string(),
                    filters: filtres_de_piece(),
                    created_at: "2026-11-18T00:00:00Z".into(),
                    measurement_data: None,
                    origin: None,
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
        if i % 5 == 4 {
            s.set(&format!("zone_{z}_level_compensation"), "false")
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
    // Des caches de la table `settings`, que la lecture ne doit pas parcourir.
    for k in 0..50 {
        s.set(&format!("search_cache_{k}"), "{}").unwrap();
    }
    zones
}

/// Ce que la chaîne ENREGISTRÉE doit contenir pour la zone `i`, dans
/// l'ordre, déduit des variantes et non du code de migration.
/// D1 : la correction de pièce de la zone `i` est-elle calculée sans micro ?
fn estimation_c5(i: i64) -> bool {
    i % 24 == 21
}

fn attendu(i: i64) -> Vec<(String, bool, Value)> {
    let mut v = Vec::new();
    if let Some((p, id)) = eq_variante(i) {
        let mut s = sans_enabled_v(serde_json::to_value(&p).unwrap());
        if id == ID_CORRECTION_PIECE {
            let origine = if estimation_c5(i) {
                "estimation"
            } else {
                "measured"
            };
            s.as_object_mut()
                .unwrap()
                .insert("origin".into(), json!(origine));
        }
        v.push((id.to_owned(), p.enabled, s));
    }
    if i % 2 == 1 {
        v.push((
            ID_CONVOLUTION.into(),
            true,
            json!({"ir_path": format!("/data/ir/zone-{i:03}.wav")}),
        ));
    }
    if let Some(x) = crossfeed_variante(i) {
        v.push((
            ID_CROSSFEED.into(),
            x["enabled"] == json!(true),
            sans_enabled_v(x),
        ));
    }
    for (id, x) in natifs_variante(i) {
        v.push((id.into(), x["enabled"] == json!(true), sans_enabled_v(x)));
    }
    if mono_variante(i) == Some("true") {
        if reseau(i) {
            v.push((ID_MONO.into(), false, json!({"inert_on_network": true})));
        } else {
            v.push((ID_MONO.into(), true, Value::Null));
        }
    }
    if let Some(x) = remap_variante(i) {
        v.push((
            ID_REAFFECTATION.into(),
            x["enabled"] == json!(true),
            sans_enabled_v(x),
        ));
    }
    // Décision B, écrite à la main : Crosstalk (3) > crossfeed-* (2) >
    // crossfeed (1) ; les autres membres cochés sont décochés et marqués.
    let prio = |id: &str| match id {
        "crosstalk" => Some(3),
        "crossfeed-pro" => Some(2),
        "crossfeed" => Some(1),
        _ => None,
    };
    let membres: Vec<usize> = (0..v.len())
        .filter(|&k| v[k].1 && prio(&v[k].0).is_some())
        .collect();
    if membres.len() > 1 {
        let gagnant = *membres
            .iter()
            .max_by_key(|&&k| (prio(&v[k].0), std::cmp::Reverse(k)))
            .unwrap();
        let id_gagnant = v[gagnant].0.clone();
        for k in membres {
            if k != gagnant {
                v[k].1 = false;
                v[k].2
                    .as_object_mut()
                    .unwrap()
                    .insert("replaced_in_group".into(), json!(id_gagnant));
            }
        }
    }
    v
}

/// Installe et active ces greffons (intégrés ou paquets).
fn installer(s: &SettingsRepo, ids: &[&str]) {
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    for id in ids {
        s.set(&format!("plugin_{id}_installed"), "true").unwrap();
        s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    }
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

    let bilans = migrer_toutes_les_zones(&backend).unwrap();
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

        // 4. Hors chaîne : Pure, compensation de niveau, sortie.
        let l = lire_zone(&s, z, true).unwrap();
        assert_eq!(l.pure, i % 3 == 1 || i % 4 == 3, "zone {i}");
        assert_eq!(l.compensation_de_niveau, i % 5 != 4, "zone {i}");
        assert_eq!(l.sortie_locale, !reseau(i), "zone {i}");
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
fn la_grande_base_lue_sans_premium() {
    let (backend, s) = base();
    let zones = semer(&backend, &s);
    migrer_toutes_les_zones(&backend).unwrap();
    let enregistre = tout(&s);
    for (i, &z) in zones.iter().enumerate() {
        let i = i as i64;
        let premium = lire_zone(&s, z, true).unwrap();
        let gratuit = lire_zone(&s, z, false).unwrap();
        // P2 sur un paramétrique coché (variantes 7 et 10), D1 sur une
        // correction de pièce MESURÉE (variante 9, hors estimations C5).
        let piece_mesuree = i % 12 == 9 && !estimation_c5(i);
        let converti = matches!(i % 12, 7 | 10) || piece_mesuree;
        assert_eq!(gratuit.conversion_p2, converti, "zone {i}");
        assert_eq!(gratuit.conversion_piece, piece_mesuree, "zone {i}");
        if converti {
            assert_eq!(gratuit.chaine.plugins[0].id, ID_EQ_GRAPHIQUE);
            assert_eq!(gratuit.chaine.plugins[1..], premium.chaine.plugins[1..]);
        } else {
            assert_eq!(gratuit.chaine, premium.chaine, "zone {i}");
        }
        // D1 : l'estimation sans micro reste active et non convertie.
        if estimation_c5(i) {
            let m = maillon(&gratuit.chaine, ID_CORRECTION_PIECE);
            assert!(m.enabled && correction_estimee(m));
        }
    }
    // Lire n'écrit rien.
    assert_eq!(tout(&s), enregistre);
}

#[test]
fn la_base_couvre_toutes_les_combinaisons_connues() {
    // Mesuré sur les chaînes MIGRÉES, pas sur les générateurs.
    let (backend, s) = base();
    semer(&backend, &s);
    let bilans = migrer_toutes_les_zones(&backend).unwrap();
    let chaines: Vec<ChaineTess> = bilans.into_iter().map(|(_, b)| b.unwrap().chaine).collect();
    let vus: BTreeSet<(String, bool)> = chaines
        .iter()
        .flat_map(|c| c.plugins.iter().map(|m| (m.id.clone(), m.enabled)))
        .collect();
    for (id, on) in [
        (ID_EQ_GRAPHIQUE, true),
        (ID_EQ_GRAPHIQUE, false),
        (ID_EQ_PARAMETRIQUE, true),
        (ID_EQ_PARAMETRIQUE, false),
        (ID_CORRECTION_PIECE, true),
        (ID_CONVOLUTION, true),
        (ID_CROSSFEED, true),
        (ID_CROSSFEED, false),
        ("crossfeed-pro", true),
        ("crossfeed-pro", false),
        ("crosstalk", true),
        ("reverb-hall", false),
        ("amp-sim", true),
        (ID_MONO, true),
        (ID_MONO, false),
        (ID_REAFFECTATION, true),
        (ID_REAFFECTATION, false),
    ] {
        assert!(vus.contains(&(id.to_owned(), on)), "{id} {on}");
    }
    // Le groupe crossfeed entier, coché ensemble dans les anciennes clés :
    // un seul membre coché dans la chaîne, les autres marqués (décision B).
    assert!(chaines.iter().any(|c| {
        let membre = |id: &str| c.plugins.iter().find(|m| m.id == id);
        membre("crosstalk").is_some_and(|m| m.enabled)
            && ["crossfeed", "crossfeed-pro"].iter().all(|id| {
                membre(id).is_some_and(|m| {
                    !m.enabled && m.settings["replaced_in_group"] == json!("crosstalk")
                })
            })
    }));
    // Jamais deux membres cochés.
    for c in &chaines {
        let coches = c
            .plugins
            .iter()
            .filter(|m| m.enabled && priorite_dans_le_groupe(&m.id).is_some())
            .count();
        assert!(coches <= 1, "{:?}", ids(c));
    }
    let surechantillonnages: BTreeSet<String> = chaines
        .iter()
        .map(|c| format!("{:?}", c.fixed.upsampling))
        .collect();
    assert!(surechantillonnages.len() >= 4, "{surechantillonnages:?}");
}

#[test]
fn deux_migrations_valent_une() {
    let (backend, s) = base();
    semer(&backend, &s);
    migrer_toutes_les_zones(&backend).unwrap();
    let apres_une = tout(&s);
    let bilans = migrer_toutes_les_zones(&backend).unwrap();
    assert!(bilans.iter().all(|(_, b)| !b.as_ref().unwrap().ecrite));
    assert_eq!(tout(&s), apres_une);
}

// ── B1 : la chaîne enregistrée ne dépend pas du droit ───────────────

#[test]
fn b1_la_migration_ne_grave_jamais_p2() {
    let (_b, s) = base();
    let z = 11;
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&profil(true, parametrique())).unwrap(),
    )
    .unwrap();

    // Compte gratuit au moment de la migration (lecture P2), puis Premium.
    assert!(lire_zone(&s, z, false).unwrap().conversion_p2);
    let b = migrer_zone(&s, z).unwrap();
    assert_eq!(b.chaine.plugins[0].id, ID_EQ_PARAMETRIQUE);
    let premium = lire_zone(&s, z, true).unwrap();
    assert_eq!(premium.chaine.plugins[0].id, ID_EQ_PARAMETRIQUE);
    assert!(!premium.conversion_p2);
    // Et l'inverse : Premium au moment de la migration, gratuit ensuite.
    let gratuit = lire_zone(&s, z, false).unwrap();
    assert!(gratuit.conversion_p2);
    assert_eq!(gratuit.chaine.plugins[0].id, ID_EQ_GRAPHIQUE);
    // La chaîne enregistrée n'a pas bougé, quelle que soit la lecture.
    let brut = s.get(&cle_chaine_zone(z)).unwrap().unwrap();
    assert_eq!(serde_json::from_str::<ChaineTess>(&brut).unwrap(), b.chaine);
    assert!(!migrer_zone(&s, z).unwrap().ecrite);
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
        r#"{"enabled":false,"span_deg":45}"#,
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
    eq.settings = sans_enabled_v(
        serde_json::to_value(profil(true, graphique(10, |k| k as f64 / 2.0, &[None]))).unwrap(),
    );
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

    // M1 : une réécriture de même sens (clés réordonnées, espaces) n'est
    // PAS une écriture d'ancienne route : le DSD reste.
    let brut = s.get(&format!("zone_{z}_upsampler")).unwrap().unwrap();
    let v: Value = serde_json::from_str(&brut).unwrap();
    s.set(
        &format!("zone_{z}_upsampler"),
        &serde_json::to_string_pretty(&v).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        lire_zone(&s, z, true).unwrap().chaine.fixed.upsampling,
        Upsampling::Dsd { .. }
    ));

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
    c.plugins.push(MaillonGreffon {
        id: ID_EQ_CASQUE.into(),
        enabled: true,
        settings: sans_enabled_v(serde_json::to_value(profil(true, autoeq())).unwrap()),
    });
    ecrire_chaine(&s, z, &c).unwrap();
    assert_eq!(lire_zone(&s, z, true).unwrap().chaine, c);
    // Un profil de casque n'est pas un paramétrique : jamais converti.
    assert!(!lire_zone(&s, z, false).unwrap().conversion_p2);

    // Une ancienne route retouche une bande : c'est toujours le casque.
    let mut p = profil(true, autoeq());
    p.bands[2].gain = -5.0;
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

// ── B2 : course avec une ancienne route, verrou par zone ────────────

#[test]
fn b2_une_ancienne_route_qui_ecrit_pendant_tess_n_est_jamais_avalee() {
    let (_b, s) = base();
    let z = 21;
    ecrire_comme_les_anciennes_routes(&s, z);
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    c.plugins
        .iter_mut()
        .find(|m| m.id == ID_CROSSFEED)
        .unwrap()
        .enabled = false;
    // Pendant l'écriture TESS, après les anciennes clés et avant l'ombre, un
    // mobile règle le crossfeed.
    ecrire_chaine_avec(&s, z, &c, || {
        s.set(
            &format!("zone_{z}_crossfeed"),
            r#"{"enabled":true,"amount":0.45,"delay_ms":0.3}"#,
        )
        .unwrap();
    })
    .unwrap();
    let relue = lire_zone(&s, z, true).unwrap().chaine;
    let cf = maillon(&relue, ID_CROSSFEED);
    assert!(cf.enabled, "l'écriture du mobile a été avalée");
    assert_eq!(cf.settings["amount"], json!(0.45));
}

#[test]
fn b2_temoin_sans_ecriture_concurrente_tess_l_emporte() {
    let (_b, s) = base();
    let z = 22;
    ecrire_comme_les_anciennes_routes(&s, z);
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    c.plugins
        .iter_mut()
        .find(|m| m.id == ID_CROSSFEED)
        .unwrap()
        .enabled = false;
    ecrire_chaine_avec(&s, z, &c, || {}).unwrap();
    assert!(!maillon(&lire_zone(&s, z, true).unwrap().chaine, ID_CROSSFEED).enabled);
}

#[test]
fn b2_ecritures_tess_et_migrations_concurrentes_restent_coherentes() {
    // Une base fichier : plusieurs fils, une seule zone.
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("t02.db");
    let db = crate::db::sqlite::SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let z = 31;
    let s = SettingsRepo::with_backend(backend.clone());
    ecrire_comme_les_anciennes_routes(&s, z);
    let base_chaine = lire_zone(&s, z, true).unwrap().chaine;

    let fils: Vec<_> = (0..4)
        .map(|f| {
            let backend = backend.clone();
            let base_chaine = base_chaine.clone();
            std::thread::spawn(move || {
                let s = SettingsRepo::with_backend(backend);
                for k in 0..25 {
                    if f == 0 {
                        migrer_zone(&s, z).unwrap();
                    } else {
                        let mut c = base_chaine.clone();
                        c.fixed.headroom = Headroom::Manual {
                            db: -((f * 100 + k) as f64 % 24.0),
                        };
                        c.plugins
                            .iter_mut()
                            .find(|m| m.id == ID_CROSSFEED)
                            .unwrap()
                            .enabled = k % 2 == 0;
                        ecrire_chaine(&s, z, &c).unwrap();
                    }
                }
            })
        })
        .collect();
    for f in fils {
        f.join().unwrap();
    }
    // À la fin, la chaîne enregistrée et les anciennes clés s'accordent :
    // la relire et la réécrire ne change rien.
    let c = lire_zone(&s, z, true).unwrap().chaine;
    let enregistree: ChaineTess =
        serde_json::from_str(&s.get(&cle_chaine_zone(z)).unwrap().unwrap()).unwrap();
    assert_eq!(c, enregistree);
    let b = ecrire_chaine(&s, z, &c).unwrap();
    assert!(b.cles_modifiees.is_empty() && !b.chaine_ecrite, "{b:?}");
}

// ── I2 : refus explicite seulement pour l'illisible ─────────────────

#[test]
fn une_cle_illisible_donne_un_refus_explicite_jamais_une_chaine_vide() {
    let z = 9;
    let cas: Vec<(String, &str)> = vec![
        (format!("zone_{z}_eq_profile"), "{oops"),
        (format!("zone_{z}_eq_profile"), "\"loudness\""),
        (
            format!("zone_{z}_eq_profile"),
            r#"{"enabled":true,"bands":"x"}"#,
        ),
        (format!("zone_{z}_crossfeed"), "[1,2]"),
        (format!("zone_{z}_crossfeed"), "{oops"),
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
        (format!("zone_{z}_tess_chain"), "{"),
        (format!("zone_{z}_tess_chain"), r#"{"plugins":[]}"#),
    ];
    for (cle, valeur) in cas {
        let (_b, s) = base();
        // Une zone par ailleurs normale : le refus ne vient que de la clé.
        s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
        s.set(&cle, valeur).unwrap();
        let avant = tout(&s);

        match lire_zone(&s, z, true) {
            Err(ErreurMigration::CleCorrompue { cle: c, raison }) => {
                assert_eq!(c, cle, "{valeur}");
                let texte = ErreurMigration::CleCorrompue {
                    cle: c.clone(),
                    raison,
                }
                .to_string();
                assert!(texte.contains(&cle), "{texte}");
            }
            autre => panic!("{cle} = {valeur:?} : attendu un refus, obtenu {autre:?}"),
        }
        assert!(migrer_zone(&s, z).is_err(), "{cle}");
        // Rien n'a été écrit : ni chaîne vide, ni ombre.
        assert_eq!(tout(&s), avant, "{cle}");
    }
}

#[test]
fn les_booleens_non_canoniques_se_lisent_comme_aujourd_hui() {
    let (_b, s) = base();
    let z = 9;
    s.set(&format!("zone_{z}_audiophile"), "on").unwrap();
    s.set(&format!("zone_{z}_strict_bitperfect"), "1").unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "yes").unwrap();
    s.set(&format!("zone_{z}_level_compensation"), "nope")
        .unwrap();
    s.set(
        &format!("zone_{z}_crossfeed"),
        r#"{"enabled":"yes","amount":0.3}"#,
    )
    .unwrap();
    s.set(
        &format!("zone_{z}_native_plugin_crosstalk"),
        r#"{"enabled":1}"#,
    )
    .unwrap();
    s.set(&format!("ir_path_{z}"), "  ").unwrap();
    let avant = tout(&s);

    let l = lire_zone(&s, z, true).unwrap();
    // Exactement ce que lisent `audiophile::zone_enabled`,
    // `bitperfect_strict::zone_enabled`, `zone_mono_downmix_configure_with`,
    // `zone_compensation_de_niveau` et le chargeur du crossfeed.
    assert!(!l.pure);
    assert!(l.compensation_de_niveau);
    assert_eq!(
        ids(&l.chaine),
        vec![(ID_CROSSFEED, false), ("crosstalk", false)]
    );

    // Migrer puis réécrire ne touche à aucune de ces clés.
    migrer_zone(&s, z).unwrap();
    let b = ecrire_chaine(&s, z, &l.chaine).unwrap();
    assert!(b.cles_modifiees.is_empty(), "{:?}", b.cles_modifiees);
    assert_eq!(hors_tess(&s), avant);
}

#[test]
fn une_ombre_illisible_vaut_une_ombre_absente() {
    let (_b, s) = base();
    let z = 10;
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    c.fixed.headroom = Headroom::Manual { db: -3.0 };
    ecrire_chaine(&s, z, &c).unwrap();
    s.set(&cle_ombre_zone(z), "[1]").unwrap();
    let l = lire_zone(&s, z, true).unwrap();
    // Les anciennes clés l'emportent ; ce que seule la chaîne dit reste.
    assert_eq!(ids(&l.chaine), vec![(ID_MONO, true)]);
    assert_eq!(l.chaine.fixed.headroom, Headroom::Manual { db: -3.0 });
    // La migration répare l'ombre.
    assert!(migrer_zone(&s, z).unwrap().ecrite);
    assert!(
        serde_json::from_str::<Value>(&s.get(&cle_ombre_zone(z)).unwrap().unwrap())
            .unwrap()
            .is_object()
    );
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
    assert!(!migrer_zone(&s, z).unwrap().ecrite);
    assert_eq!(s.get(&cle_chaine_zone(z)).unwrap().as_deref(), Some(brut));
    assert_eq!(
        ecrire_chaine(&s, z, &l.chaine),
        Err(ErreurMigration::VersionPlusRecente { trouvee: 2 })
    );
}

// ── I1 : hors chaîne ────────────────────────────────────────────────

#[test]
fn compensation_de_niveau_et_mode_dsd_accompagnent_la_chaine() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Salon", Some("local"), Some("local:hw:0"))
        .unwrap();
    repo.update_dsd_mode(z, "dop").unwrap();
    s.set(&format!("zone_{z}_level_compensation"), "false")
        .unwrap();
    let l = lire_zone(&s, z, true).unwrap();
    assert!(!l.compensation_de_niveau);
    assert_eq!(l.dsd_mode, "dop");
    assert_eq!(lecture_dsd(&l.dsd_mode), Some(crate::tess::LectureDsd::Dop));
    assert_eq!(
        lecture_dsd("pcm"),
        Some(crate::tess::LectureDsd::ConvertedToPcm)
    );
    assert_eq!(lecture_dsd("auto"), None);
    // Rien de cela n'entre dans la chaîne enregistrée.
    migrer_zone(&s, z).unwrap();
    let brut = s.get(&cle_chaine_zone(z)).unwrap().unwrap();
    assert!(!brut.contains("compensation") && !brut.contains("dsd_mode"));
}

#[test]
fn la_reserve_realiste_de_l_egaliseur_reste_dans_ses_reglages() {
    let (_b, s) = base();
    let z = 14;
    let p = EqProfile {
        enabled: true,
        headroom_mode: HeadroomMode::Realistic,
        bands: parametrique(),
        ..EqProfile::default()
    };
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&p).unwrap(),
    )
    .unwrap();
    let c = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(c.plugins[0].settings["headroom_mode"], json!("realistic"));
    // Le headroom de la chaîne reste automatique : pas de seconde réserve
    // inscrite par la migration (T07 : min, jamais la somme).
    assert_eq!(c.fixed.headroom, Headroom::Auto {});
    // Et la conversion P2 la garde.
    let g = lire_zone(&s, z, false).unwrap().chaine;
    assert_eq!(g.plugins[0].settings["headroom_mode"], json!("realistic"));
}

// ── D1 et D2 (choix provisoires) ────────────────────────────────────

fn contexte_gratuit(dispo: &dyn Fn(&str) -> bool) -> ContexteExecution<'_> {
    ContexteExecution {
        transport: TransportZone::Local,
        source: Some(SourceFlux::Pcm {
            sample_rate_hz: 48_000,
            bits: 24,
            channels: 2,
        }),
        premium: false,
        greffon_disponible: dispo,
        pure: false,
        volume: VolumeZone {
            control: ControleVolume::Software,
            db: 0.0,
        },
    }
}

/// Pose un profil de pièce (JSON brut, pour pouvoir y ajouter un champ) et
/// l'égaliseur qu'il produit, comme `POST /room-correction/.../apply`.
fn poser_la_piece(s: &SettingsRepo, z: i64, nom: &str, champ: Option<(&str, Value)>) -> String {
    let piece = crate::room_correction::RoomProfile {
        name: nom.into(),
        zone_id: z.to_string(),
        filters: filtres_de_piece(),
        created_at: "2026-11-18T00:00:00Z".into(),
        measurement_data: None,
        origin: None,
    };
    let mut v = serde_json::to_value(&piece).unwrap();
    if let Some((k, x)) = champ {
        v.as_object_mut().unwrap().insert(k.into(), x);
    }
    s.set(&format!("room_profile_{z}"), &v.to_string()).unwrap();
    let eq = crate::room_correction::profil_eq_de_correction(&filtres_de_piece());
    let brut = serde_json::to_string(&eq).unwrap();
    s.set(&format!("zone_{z}_eq_profile"), &brut).unwrap();
    brut
}

fn etape_piece(c: &ChaineTess, premium: bool) -> crate::tess::EtapeResolue {
    let tout_dispo = |_: &str| true;
    let mut ctx = contexte_gratuit(&tout_dispo);
    ctx.premium = premium;
    c.resoudre(&catalogue_de_reference(), &ctx)
        .etapes
        .into_iter()
        .find(|e| e.plugin_id.as_deref() == Some(ID_CORRECTION_PIECE))
        .expect("étape room-correction")
}

#[test]
fn d1_une_correction_mesuree_est_premium_et_convertie_a_la_lecture() {
    let (_b, s) = base();
    let z = 15;
    let brut = poser_la_piece(&s, z, "Calibration abc", None);

    // Enregistrée telle quelle, origine « measured », sans droit acquis.
    let b = migrer_zone(&s, z).unwrap();
    let m = maillon(&b.chaine, ID_CORRECTION_PIECE);
    assert_eq!(m.settings["origin"], json!("measured"));
    assert!(!correction_estimee(m));
    let e = etape_piece(&b.chaine, false);
    assert_eq!(e.reason, Some(MotifEtape::PremiumRequired), "{e:?}");
    assert!(etape_piece(&b.chaine, true).effective);

    // Compte gratuit : convertie en graphique au plus près, à la lecture.
    let gratuit = lire_zone(&s, z, false).unwrap();
    assert!(gratuit.conversion_p2 && gratuit.conversion_piece);
    assert_eq!(gratuit.chaine.plugins[0].id, ID_EQ_GRAPHIQUE);
    assert!(gratuit.chaine.plugins[0].settings.get("origin").is_none());
    let converti = converti_de(&gratuit);
    assert!(est_courbe_graphique(&converti.bands));
    let original = crate::room_correction::profil_eq_de_correction(&filtres_de_piece());
    let (aux_points, _) = ecarts(&original, &converti, 10);
    assert!(aux_points < 1.5, "{aux_points:.2} dB");
    // Premium : rien n'est converti.
    assert!(!lire_zone(&s, z, true).unwrap().conversion_p2);

    // Jamais détruite : ni la chaîne enregistrée ni `eq_profile` ne bougent,
    // et l'origine n'entre jamais dans `eq_profile`.
    let enregistree: ChaineTess =
        serde_json::from_str(&s.get(&cle_chaine_zone(z)).unwrap().unwrap()).unwrap();
    assert_eq!(enregistree, b.chaine);
    let e = ecrire_chaine(&s, z, &b.chaine).unwrap();
    assert!(e.cles_modifiees.is_empty());
    assert_eq!(
        s.get(&format!("zone_{z}_eq_profile")).unwrap().as_deref(),
        Some(brut.as_str())
    );
}

#[test]
fn d1_une_correction_calculee_sans_micro_est_free_et_jamais_convertie() {
    let (_b, s) = base();
    let z = 18;
    poser_la_piece(&s, z, "Estimation sim-7", None);
    let gratuit = lire_zone(&s, z, false).unwrap();
    assert!(!gratuit.conversion_p2 && !gratuit.conversion_piece);
    let m = maillon(&gratuit.chaine, ID_CORRECTION_PIECE);
    assert!(m.enabled && correction_estimee(m));
    assert_eq!(m.settings["origin"], json!("estimation"));
    let e = etape_piece(&gratuit.chaine, false);
    assert!(e.effective, "{e:?}");

    // Contre-épreuve : le même maillon sans l'origine « estimation » demande
    // le Premium.
    let mut sans = gratuit.chaine.clone();
    sans.plugins[0]
        .settings
        .as_object_mut()
        .unwrap()
        .insert("origin".into(), json!("measured"));
    assert_eq!(
        etape_piece(&sans, false).reason,
        Some(MotifEtape::PremiumRequired)
    );
}

#[test]
fn d1_l_origine_enregistree_a_la_creation_l_emporte_sur_le_nom() {
    // Décision du 18/11 soir : tout profil créé enregistre son origine
    // (`RoomProfile::origin`) ; le nom ne sert plus qu'aux profils anciens.
    let profil = |nom: &str, origine: Option<&str>| crate::room_correction::RoomProfile {
        name: nom.into(),
        zone_id: "1".into(),
        filters: filtres_de_piece(),
        created_at: "2026-11-18T00:00:00Z".into(),
        measurement_data: None,
        origin: origine.map(str::to_owned),
    };
    let v = |p| serde_json::to_value(p).unwrap();
    assert_eq!(
        origine_de_la_piece(&v(profil("Estimation 3", Some("measured")))),
        ORIGINE_MESURE
    );
    assert_eq!(
        origine_de_la_piece(&v(profil("Mon salon", Some("estimation")))),
        ORIGINE_ESTIMATION
    );
    // Profil ancien, sans origine : la règle du nom (et le champ absent du
    // JSON, pour rester lisible par une version précédente).
    let ancien = v(profil("Estimation 3", None));
    assert!(ancien.get("origin").is_none());
    assert_eq!(origine_de_la_piece(&ancien), ORIGINE_ESTIMATION);
    assert_eq!(
        origine_de_la_piece(&v(profil("Salon", None))),
        ORIGINE_MESURE
    );
    // Et de bout en bout : une estimation renommée reste Free sans Premium.
    let (_b, s) = base();
    let z = 22;
    let p = profil("Mon salon", Some("estimation"));
    s.set(
        &format!("room_profile_{z}"),
        &serde_json::to_string(&p).unwrap(),
    )
    .unwrap();
    let eq = crate::room_correction::profil_eq_de_correction(&filtres_de_piece());
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&eq).unwrap(),
    )
    .unwrap();
    let l = lire_zone(&s, z, false).unwrap();
    assert!(!l.conversion_piece);
    assert!(correction_estimee(&l.chaine.plugins[0]));
}

#[test]
fn d1_l_origine_explicite_l_emporte_sur_le_nom() {
    // `"estimation": bool` n'est plus cru (le serveur ne l'écrit jamais) :
    // une fiche « Mon salon » qui se dit estimation reste MESURÉE, donc
    // convertie sans Premium...
    let (_b, s) = base();
    poser_la_piece(&s, 19, "Mon salon", Some(("estimation", json!(true))));
    assert!(lire_zone(&s, 19, false).unwrap().conversion_piece);
    // ... et « Estimation 3 » qui se dit mesurée suit la règle du nom.
    let (_b, s) = base();
    poser_la_piece(&s, 20, "Estimation 3", Some(("estimation", json!(false))));
    assert!(correction_estimee(
        &lire_zone(&s, 20, false).unwrap().chaine.plugins[0]
    ));
    // `origin` mesurée l'emporte sur le nom d'estimation.
    let (_b, s) = base();
    poser_la_piece(&s, 23, "Estimation 4", Some(("origin", json!("measured"))));
    assert!(lire_zone(&s, 23, false).unwrap().conversion_piece);
    // `origin` explicite.
    let (_b, s) = base();
    poser_la_piece(&s, 21, "Salon", Some(("origin", json!("estimation"))));
    assert!(!lire_zone(&s, 21, false).unwrap().conversion_piece);
    // Rien d'explicite, nom quelconque : dans le doute, mesurée.
    assert_eq!(
        origine_de_la_piece(&json!({"name": "Salon"})),
        ORIGINE_MESURE
    );
    assert_eq!(
        origine_de_la_piece(&json!({"name": "estimation 3"})),
        ORIGINE_MESURE
    );
}

#[test]
fn d2_le_repli_mono_inerte_sur_le_reseau_reste_inerte() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Salon DLNA", Some("dlna"), Some("dlna:uuid-1"))
        .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    let c = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(ids(&c), vec![(ID_MONO, false)]);
    assert_eq!(c.plugins[0].settings, json!({"inert_on_network": true}));
    // Réécrire ne touche pas l'interrupteur des anciennes routes.
    let b = ecrire_chaine(&s, z, &c).unwrap();
    assert!(b.cles_modifiees.is_empty());
    assert_eq!(
        s.get(&format!("zone_{z}_mono_downmix")).unwrap().as_deref(),
        Some("true")
    );
    // Cocher via TESS : l'ancienne clé est déjà à « true ».
    let mut coche = c.clone();
    coche.plugins[0].enabled = true;
    coche.plugins[0].settings = Value::Null;
    assert!(
        ecrire_chaine(&s, z, &coche)
            .unwrap()
            .cles_modifiees
            .is_empty()
    );
    // Le retirer via TESS efface l'interrupteur.
    let vide = ChaineTess::default();
    assert_eq!(
        ecrire_chaine(&s, z, &vide).unwrap().cles_modifiees,
        vec![format!("zone_{z}_mono_downmix")]
    );
    // Témoin : sur une sortie locale, il est coché.
    let local = repo
        .create("Casque", Some("local"), Some("local:hw:1"))
        .unwrap();
    s.set(&format!("zone_{local}_mono_downmix"), "true")
        .unwrap();
    assert_eq!(
        ids(&lire_zone(&s, local, true).unwrap().chaine),
        vec![(ID_MONO, true)]
    );
}

// ── P2 : paramétrique d'un compte gratuit ───────────────────────────

/// L'écart maximal entre la cible et le graphique, aux points de la grille
/// où la cible tient dans ±12 dB, et au milieu (géométrique) de deux points.
fn ecarts(original: &EqProfile, converti: &EqProfile, n: u32) -> (f64, f64) {
    let (grille, _) = grille_graphique(n);
    let milieux: Vec<f64> = grille.windows(2).map(|w| (w[0] * w[1]).sqrt()).collect();
    let mesure = |f: &[f64]| {
        let cible = courbe_db(original, f);
        let obtenu = courbe_db(converti, f);
        let mut e = 0.0_f64;
        for ch in 0..2 {
            for i in 0..f.len() {
                if cible[ch][i].abs() <= GAIN_GRAPHIQUE_MAX_DB {
                    e = e.max((cible[ch][i] - obtenu[ch][i]).abs());
                }
            }
        }
        e
    };
    (mesure(grille), mesure(&milieux))
}

fn converti_de(l: &LectureZone) -> EqProfile {
    let mut v = l.chaine.plugins[0].settings.clone();
    v.as_object_mut()
        .unwrap()
        .insert("enabled".into(), json!(true));
    serde_json::from_value(v).unwrap()
}

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
    let l = lire_zone(&s, z, false).unwrap();
    assert!(l.conversion_p2);
    let m = &l.chaine.plugins[0];
    assert_eq!(m.id, ID_EQ_GRAPHIQUE);
    assert!(m.enabled);
    let converti = converti_de(&l);
    assert!(est_courbe_graphique(&converti.bands));
    assert_eq!(converti.bands.len(), 20);

    // Au plus près : moins de 1 dB aux points de la grille ; entre deux
    // points, l'écart est mesuré et borné.
    let (aux_points, entre) = ecarts(&original, &converti, 10);
    assert!(aux_points < 1.0, "{aux_points:.2} dB");
    assert!(entre < 3.0, "{entre:.2} dB entre les points");
    eprintln!("P2 paramétrique : {aux_points:.2} dB aux points, {entre:.2} dB entre");

    // Écrite via TESS, la conversion passe dans l'ancienne clé ; l'original
    // est sauvegardé.
    ecrire_chaine(&s, z, &l.chaine).unwrap();
    assert_eq!(
        s.get(&cle_sauvegarde_eq(z)).unwrap().as_deref(),
        Some(brut.as_str())
    );
    let ecrit: EqProfile =
        serde_json::from_str(&s.get(&format!("zone_{z}_eq_profile")).unwrap().unwrap()).unwrap();
    assert!(est_courbe_graphique(&ecrit.bands));
}

#[test]
fn p2_un_profil_de_forme_autoeq_est_converti_au_plus_pres() {
    let (_b, s) = base();
    let z = 16;
    let original = profil(true, autoeq());
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&original).unwrap(),
    )
    .unwrap();
    let l = lire_zone(&s, z, false).unwrap();
    assert!(l.conversion_p2);
    let converti = converti_de(&l);
    assert_eq!(converti.bands.len(), 10, "courbe liée : une seule passe");
    let (aux_points, entre) = ecarts(&original, &converti, 10);
    eprintln!("P2 AutoEQ : {aux_points:.2} dB aux points, {entre:.2} dB entre");
    assert!(aux_points < 1.5, "{aux_points:.2} dB");
    assert!(entre < 4.0, "{entre:.2} dB entre les points");
}

#[test]
fn p2_les_gains_sont_bornes_a_12_db() {
    let (_b, s) = base();
    let z = 17;
    let original = profil(
        true,
        vec![
            bande(1000.0, 18.0, 1.0, "peak", None),
            bande(63.0, -20.0, 1.0, "peak", None),
        ],
    );
    s.set(
        &format!("zone_{z}_eq_profile"),
        &serde_json::to_string(&original).unwrap(),
    )
    .unwrap();
    let converti = converti_de(&lire_zone(&s, z, false).unwrap());
    assert!(
        converti
            .bands
            .iter()
            .all(|b| b.gain.abs() <= GAIN_GRAPHIQUE_MAX_DB)
    );
    // La cible dépasse ±12 dB : la courbe graphique est bornée là, au lieu
    // de la suivre ou de diverger.
    let rep = courbe_db(&converti, &[63.0, 1000.0]);
    assert!(
        (rep[0][1] - 12.0).abs() < 1.0,
        "1 kHz : {:.2} dB",
        rep[0][1]
    );
    assert!(
        (rep[0][0] + 12.0).abs() < 1.0,
        "63 Hz : {:.2} dB",
        rep[0][0]
    );
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

// ── I4 : lecture par préfixe ────────────────────────────────────────

#[test]
fn la_lecture_par_prefixe_ne_deborde_pas_sur_une_autre_zone() {
    let (_b, s) = base();
    s.set("zone_1_native_plugin_a", r#"{"enabled":true}"#)
        .unwrap();
    s.set("zone_12_native_plugin_b", r#"{"enabled":true}"#)
        .unwrap();
    s.set("zone_1Xnative_plugin_c", r#"{"enabled":true}"#)
        .unwrap();
    s.set("zone_1_native_pluginXd", r#"{"enabled":true}"#)
        .unwrap();
    let cles: Vec<String> = s
        .par_prefixe("zone_1_native_plugin_")
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(cles, vec!["zone_1_native_plugin_a".to_owned()]);
    assert_eq!(
        ids(&lire_zone(&s, 1, true).unwrap().chaine),
        vec![("a", true)]
    );
}

// ── Export de configuration ─────────────────────────────────────────

#[test]
fn config_export_inclut_la_chaine() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let a = repo.create("Salon", Some("local"), Some("dev-a")).unwrap();
    let b = repo
        .create("Bureau", Some("local"), Some("local:dev-b"))
        .unwrap();
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
    // L'ombre ne part plus (décision B du 19/11 : elle est propre à la base
    // qui l'a écrite, l'import la reconstruit).
    assert!(
        zone(a)["settings"]
            .get("zone_{id}_tess_chain_sync")
            .is_none()
    );
    assert!(
        zone(b)["settings"]
            .get("zone_{id}_tess_chain_sync")
            .is_none()
    );
    // Zone migrée : la chaîne que le serveur lit part, réglages TESS compris.
    let chaine_b: ChaineTess =
        serde_json::from_value(zone(b)["settings"]["zone_{id}_tess_chain"].clone()).unwrap();
    assert_eq!(chaine_b, c);
    // L'export n'écrit rien.
    assert!(s.get(&cle_chaine_zone(a)).unwrap().is_none());
}

// ── I3 : relecture d'une COPIE de base réelle ───────────────────────

/// Rejoue la lecture TESS sur une COPIE de base réelle. Ignoré par défaut ;
/// voir la procédure dans la PR (#9025). `TUNE_T02_COPIE` = chemin de la
/// copie (déjà isolée, avec `HOME` et `TUNE_DB_PATH` dédiés). Le test la
/// recopie encore dans un dossier temporaire et n'ouvre que cette seconde
/// copie : l'original n'est jamais ouvert en écriture. Rien n'est migré :
/// lecture seule, puis vérification que la table `settings` n'a pas bougé.
#[test]
#[ignore = "base réelle : lancer à la main, voir la procédure de la PR"]
fn i3_relecture_d_une_copie_de_base_reelle() {
    let Ok(source) = std::env::var("TUNE_T02_COPIE") else {
        eprintln!("TUNE_T02_COPIE absent : rien à relire");
        return;
    };
    let dossier = tempfile::tempdir().unwrap();
    let copie = dossier.path().join("copie.db");
    std::fs::copy(&source, &copie).unwrap();
    let db = crate::db::sqlite::SqliteDb::open(copie.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let s = SettingsRepo::with_backend(backend.clone());
    let avant = tout(&s);
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone())
        .list()
        .unwrap();
    let (mut lues, mut refus, mut p2, mut piece, mut mono_inerte, mut casques) =
        (0, Vec::new(), 0, 0, 0, 0);
    for z in zones.iter().filter_map(|z| z.id) {
        match lire_zone(&s, z, false) {
            Ok(l) => {
                lues += 1;
                p2 += usize::from(l.conversion_p2);
                piece += l
                    .chaine
                    .plugins
                    .iter()
                    .filter(|m| m.id == ID_CORRECTION_PIECE)
                    .count();
                casques += l
                    .chaine
                    .plugins
                    .iter()
                    .filter(|m| m.id == ID_EQ_CASQUE)
                    .count();
                mono_inerte += l
                    .chaine
                    .plugins
                    .iter()
                    .filter(|m| m.id == ID_MONO && !m.enabled)
                    .count();
                let premium = lire_zone(&s, z, true).unwrap();
                eprintln!("zone {z} : {:?}", ids(&premium.chaine));
            }
            Err(e) => refus.push(format!("zone {z} : {e}")),
        }
    }
    eprintln!(
        "T02 relecture : {} zones, {lues} lues, {} refus, {p2} conversions P2, \
         {piece} room-correction, {casques} casque, {mono_inerte} mono inerte (réseau)",
        zones.len(),
        refus.len()
    );
    for r in &refus {
        eprintln!("  refus — {r}");
    }
    assert_eq!(tout(&s), avant, "la relecture a écrit dans la copie");
}

// ── Décision B : un seul membre du groupe crossfeed ─────────────────

fn groupe_tout_coche(s: &SettingsRepo, z: i64) {
    s.set(
        &format!("zone_{z}_crossfeed"),
        r#"{"enabled":true,"amount":0.3}"#,
    )
    .unwrap();
    s.set(
        &format!("zone_{z}_native_plugin_crossfeed-pro"),
        r#"{"enabled":true,"profile":"hd800"}"#,
    )
    .unwrap();
    s.set(
        &format!("zone_{z}_native_plugin_crosstalk"),
        r#"{"enabled":true,"span_deg":30}"#,
    )
    .unwrap();
}

fn contexte_premium(dispo: &dyn Fn(&str) -> bool) -> ContexteExecution<'_> {
    let mut c = contexte_gratuit(dispo);
    c.premium = true;
    c
}

#[test]
fn groupe_b_la_migration_garde_le_membre_qui_traite_et_decoche_les_autres() {
    let (_b, s) = base();
    let z = 40;
    installer(&s, &["crossfeed", "crossfeed-pro", "crosstalk"]);
    groupe_tout_coche(&s, z);
    let avant = hors_tess(&s);

    let b = migrer_zone(&s, z).unwrap();
    let c = &b.chaine;
    assert_eq!(
        ids(c),
        vec![
            ("crossfeed", false),
            ("crossfeed-pro", false),
            ("crosstalk", true)
        ]
    );
    assert_eq!(
        maillon(c, "crossfeed").settings["replaced_in_group"],
        json!("crosstalk")
    );
    assert_eq!(maillon(c, "crossfeed").settings["amount"], json!(0.3));
    assert_eq!(
        maillon(c, "crossfeed-pro").settings["profile"],
        json!("hd800")
    );
    // Aucune ancienne clé touchée, ni par la migration ni par une réécriture.
    assert_eq!(hors_tess(&s), avant);
    assert!(ecrire_chaine(&s, z, c).unwrap().cles_modifiees.is_empty());
    assert_eq!(hors_tess(&s), avant);
    // La chaîne migrée passe la validation (politique « refuse »).
    c.valider(
        &catalogue_de_reference(),
        ContexteValidation { premium: true },
    )
    .unwrap();
    // Le chemin du signal note l'écart, et c'est Crosstalk qui traite, comme
    // l'ancien moteur.
    let dispo = |_: &str| true;
    let r = c.resoudre(&catalogue_de_reference(), &contexte_premium(&dispo));
    let etape = |id: &str| {
        r.etapes
            .iter()
            .find(|e| e.plugin_id.as_deref() == Some(id))
            .unwrap()
            .clone()
    };
    assert!(etape("crosstalk").effective);
    for id in ["crossfeed", "crossfeed-pro"] {
        let e = etape(id);
        assert!(!e.effective);
        assert_eq!(e.reason, Some(MotifEtape::ReplacedInGroup), "{id}");
        assert_eq!(e.params, Some(json!({"replaced_by": "crosstalk"})));
    }
    // Idempotent.
    assert!(!migrer_zone(&s, z).unwrap().ecrite);
}

#[test]
fn groupe_b_contre_epreuve_deux_membres_coches_sont_refuses_au_put_mais_jamais_muets() {
    let mut c = ChaineTess::default();
    for id in ["crossfeed", "crossfeed-pro", "crosstalk"] {
        c.plugins.push(MaillonGreffon {
            id: id.into(),
            enabled: true,
            settings: json!({}),
        });
    }
    // Un PUT qui coche deux membres : 409 `mutually_exclusive`.
    let e = c
        .valider(
            &catalogue_de_reference(),
            ContexteValidation { premium: true },
        )
        .unwrap_err();
    assert_eq!(e.statut_http(), 409);
    assert_eq!(e.code(), "mutually_exclusive");
    // Une chaîne ancienne qui arrive quand même ainsi : la résolution reste
    // tolérante, le plus prioritaire DISPONIBLE traite, jamais de silence.
    let sans_crosstalk = |id: &str| id != "crosstalk";
    let r = c.resoudre(
        &catalogue_de_reference(),
        &contexte_premium(&sans_crosstalk),
    );
    let effectifs: Vec<&str> = r
        .etapes
        .iter()
        .filter(|e| e.effective && e.plugin_id.is_some())
        .filter_map(|e| e.plugin_id.as_deref())
        .collect();
    assert_eq!(effectifs, vec!["crossfeed-pro"]);
}

#[test]
fn groupe_b_le_gagnant_est_un_membre_en_service() {
    let (_b, s) = base();
    let z = 41;
    // Crosstalk coché mais pas installé : aujourd'hui, Crossfeed Pro traite.
    installer(&s, &["crossfeed", "crossfeed-pro"]);
    groupe_tout_coche(&s, z);
    let c = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(
        ids(&c),
        vec![
            ("crossfeed", false),
            ("crossfeed-pro", true),
            ("crosstalk", false)
        ]
    );
    assert_eq!(
        maillon(&c, "crosstalk").settings["replaced_in_group"],
        json!("crossfeed-pro")
    );
    // Témoin : aucun membre en service, la priorité seule départage.
    let (_b, s) = base();
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    groupe_tout_coche(&s, z);
    let c = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(
        ids(&c),
        vec![
            ("crossfeed", false),
            ("crossfeed-pro", false),
            ("crosstalk", true)
        ]
    );
}

#[test]
fn groupe_b_un_membre_ecarte_revient_quand_son_gagnant_disparait() {
    let (_b, s) = base();
    let z = 42;
    installer(&s, &["crossfeed", "crossfeed-pro", "crosstalk"]);
    groupe_tout_coche(&s, z);
    migrer_zone(&s, z).unwrap();
    // Une ancienne route efface le réglage de Crosstalk.
    s.delete(&format!("zone_{z}_native_plugin_crosstalk"))
        .unwrap();
    let c = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(ids(&c), vec![("crossfeed", false), ("crossfeed-pro", true)]);
    assert!(
        maillon(&c, "crossfeed-pro")
            .settings
            .get("replaced_in_group")
            .is_none()
    );
    assert_eq!(
        maillon(&c, "crossfeed").settings["replaced_in_group"],
        json!("crossfeed-pro")
    );
}

// ── Suivi de #9088 : le membre écarté reprend sa place quand son gagnant
// ne traite pas (Premium absent, paquet non chargé) ─────────────────────

/// Crossfeed (simple) et Crosstalk cochés, tous deux installés : la
/// migration garde Crosstalk et décoche le crossfeed, marqué.
fn crossfeed_et_crosstalk_migres(s: &SettingsRepo, z: i64) -> ChaineTess {
    installer(s, &["crossfeed", "crosstalk"]);
    s.set(
        &format!("zone_{z}_crossfeed"),
        r#"{"enabled":true,"amount":0.3}"#,
    )
    .unwrap();
    s.set(
        &format!("zone_{z}_native_plugin_crosstalk"),
        r#"{"enabled":true,"span_deg":30}"#,
    )
    .unwrap();
    let c = migrer_zone(s, z).unwrap().chaine;
    assert_eq!(ids(&c), vec![("crossfeed", false), ("crosstalk", true)]);
    assert_eq!(
        maillon(&c, "crossfeed").settings["replaced_in_group"],
        json!("crosstalk")
    );
    c
}

fn etape_de(
    r: &crate::tess::resolution::Resolution,
    id: &str,
) -> crate::tess::resolution::EtapeResolue {
    r.etapes
        .iter()
        .find(|e| e.plugin_id.as_deref() == Some(id))
        .unwrap()
        .clone()
}

#[test]
fn groupe_b_compte_gratuit_le_crossfeed_remplace_crosstalk_sans_premium() {
    let (_b, s) = base();
    let c = crossfeed_et_crosstalk_migres(&s, 43);
    let dispo = |_: &str| true;
    // Compte gratuit : Crosstalk (Premium) ne traite pas ; l'ancien moteur
    // jouait le crossfeed simple, la chaîne migrée aussi.
    let r = c.resoudre(&catalogue_de_reference(), &contexte_gratuit(&dispo));
    let ct = etape_de(&r, "crosstalk");
    assert!(!ct.effective);
    assert_eq!(ct.reason, Some(MotifEtape::PremiumRequired));
    let cf = etape_de(&r, "crossfeed");
    assert!(cf.effective, "{cf:?}");
    assert_eq!(cf.reason, None);
    assert_eq!(cf.detail.as_deref(), Some("Stands in for crosstalk"));
    assert_ne!(r.badge, crate::tess::resolution::Badge::BitPerfect);
    // Rien de Premium sans payer : Crosstalk reste éteint.
    assert_eq!(
        r.etapes
            .iter()
            .filter(|e| e.effective && e.plugin_id.is_some())
            .filter_map(|e| e.plugin_id.as_deref())
            .collect::<Vec<_>>(),
        vec!["crossfeed"]
    );
    // La chaîne enregistrée n'en dépend pas : toujours Crosstalk coché.
    assert_eq!(
        ids(&lire_zone(&s, 43, false).unwrap().chaine),
        vec![("crossfeed", false), ("crosstalk", true)]
    );
}

#[test]
fn groupe_b_compte_premium_crosstalk_traite_et_le_crossfeed_reste_ecarte() {
    let (_b, s) = base();
    let c = crossfeed_et_crosstalk_migres(&s, 44);
    let dispo = |_: &str| true;
    let r = c.resoudre(&catalogue_de_reference(), &contexte_premium(&dispo));
    assert!(etape_de(&r, "crosstalk").effective);
    let cf = etape_de(&r, "crossfeed");
    assert!(!cf.effective);
    assert_eq!(cf.reason, Some(MotifEtape::ReplacedInGroup));
    assert_eq!(cf.params, Some(json!({"replaced_by": "crosstalk"})));
}

#[test]
fn groupe_b_crosstalk_installe_mais_non_charge_le_crossfeed_traite() {
    let (_b, s) = base();
    let c = crossfeed_et_crosstalk_migres(&s, 45);
    // Premium, mais la bibliothèque Crosstalk n'est pas chargée.
    let non_charge = |id: &str| id != "crosstalk";
    let r = c.resoudre(&catalogue_de_reference(), &contexte_premium(&non_charge));
    assert_eq!(
        etape_de(&r, "crosstalk").reason,
        Some(MotifEtape::PluginUnavailable)
    );
    assert!(etape_de(&r, "crossfeed").effective);

    // Même règle avec la VRAIE disponibilité (`natifs_tiers::actif`) d'un
    // paquet installé que ce processus n'a jamais chargé : Crossfeed Pro.
    let (_b, s) = base();
    let z = 46;
    installer(&s, &["crossfeed", "crossfeed-pro"]);
    s.set(&format!("zone_{z}_crossfeed"), r#"{"enabled":true}"#)
        .unwrap();
    s.set(
        &format!("zone_{z}_native_plugin_crossfeed-pro"),
        r#"{"enabled":true,"profile":"hd800"}"#,
    )
    .unwrap();
    assert!(tune_plugin_native::provider("crossfeed-pro").is_none());
    let c = migrer_zone(&s, z).unwrap().chaine;
    assert_eq!(ids(&c), vec![("crossfeed", false), ("crossfeed-pro", true)]);
    let catalogue = catalogue_de_reference();
    let reel = |id: &str| match catalogue.get(id).map(|d| d.origin) {
        Some(crate::tess::greffon::Origine::Package) => crate::audio::natifs_tiers::actif(&s, id),
        Some(_) => true,
        None => false,
    };
    let r = c.resoudre(&catalogue, &contexte_premium(&reel));
    assert_eq!(
        etape_de(&r, "crossfeed-pro").reason,
        Some(MotifEtape::PluginUnavailable)
    );
    assert!(etape_de(&r, "crossfeed").effective);
}

#[test]
fn groupe_b_contre_epreuves_le_membre_ecarte_ne_revient_pas_a_tort() {
    let (_b, s) = base();
    let c = crossfeed_et_crosstalk_migres(&s, 47);
    let dispo = |_: &str| true;
    // Pure : tout est contourné, le crossfeed ne revient pas.
    let mut x = contexte_gratuit(&dispo);
    x.pure = true;
    let r = c.resoudre(&catalogue_de_reference(), &x);
    assert!(
        r.etapes
            .iter()
            .all(|e| e.plugin_id.is_none() || !e.effective)
    );
    assert_eq!(
        etape_de(&r, "crossfeed").reason,
        Some(MotifEtape::ReplacedInGroup)
    );
    // Flux 5.1 : Crosstalk éteint pour un motif qui frapperait aussi le
    // crossfeed (stéréo seulement) : le crossfeed reste écarté.
    let mut x = contexte_premium(&dispo);
    x.source = Some(SourceFlux::Pcm {
        sample_rate_hz: 48_000,
        bits: 24,
        channels: 6,
    });
    let r = c.resoudre(&catalogue_de_reference(), &x);
    assert_eq!(
        etape_de(&r, "crosstalk").reason,
        Some(MotifEtape::StereoOnly)
    );
    assert_eq!(
        etape_de(&r, "crossfeed").reason,
        Some(MotifEtape::ReplacedInGroup)
    );
    // Décoché par l'utilisateur (aucune marque) : il reste décoché, même
    // quand Crosstalk ne traite pas.
    let mut sans_marque = c.clone();
    sans_marque.plugins[0].settings = json!({"amount": 0.3});
    let r = sans_marque.resoudre(&catalogue_de_reference(), &contexte_gratuit(&dispo));
    let cf = etape_de(&r, "crossfeed");
    assert!(!cf.effective);
    assert!(!cf.requested);
    assert_eq!(cf.reason, None);
}

// ── D1 : l'origine appartient au serveur ────────────────────────────

#[test]
fn d1_un_put_ne_peut_pas_se_dire_sans_micro() {
    // Une correction MESURÉE ; le client prétend « estimation ».
    let (_b, s) = base();
    let z = 50;
    poser_la_piece(&s, z, "Calibration abc", None);
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    c.plugins[0]
        .settings
        .as_object_mut()
        .unwrap()
        .insert("origin".into(), json!("estimation"));
    ecrire_chaine(&s, z, &c).unwrap();
    let enregistree: ChaineTess =
        serde_json::from_str(&s.get(&cle_chaine_zone(z)).unwrap().unwrap()).unwrap();
    assert_eq!(enregistree.plugins[0].settings["origin"], json!("measured"));
    let l = lire_zone(&s, z, false).unwrap();
    assert!(l.conversion_piece, "le droit a été levé par le client");

    // Une estimation existe, mais le client y met d'autres bandes en se
    // disant « estimation » : mesurée.
    let (_b, s) = base();
    poser_la_piece(&s, z, "Estimation 4", None);
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    let reglages = c.plugins[0].settings.as_object_mut().unwrap();
    reglages.insert(
        "bands".into(),
        json!([{"freq": 80.0, "gain": -9.0, "q": 2.0, "type": "peak"}]),
    );
    reglages.insert("origin".into(), json!("estimation"));
    ecrire_chaine(&s, z, &c).unwrap();
    assert!(!correction_estimee(
        &lire_zone(&s, z, false).unwrap().chaine.plugins[0]
    ));

    // Témoin : l'estimation telle quelle reste une estimation, même si le
    // client a retiré l'origine.
    let (_b, s) = base();
    poser_la_piece(&s, z, "Estimation 5", None);
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    c.plugins[0]
        .settings
        .as_object_mut()
        .unwrap()
        .remove("origin");
    ecrire_chaine(&s, z, &c).unwrap();
    assert!(correction_estimee(
        &lire_zone(&s, z, false).unwrap().chaine.plugins[0]
    ));
}

// ── D2 : changement de sortie après la migration ────────────────────

fn changer_de_sortie(backend: &Arc<dyn DbBackend>, z: i64, appareil: &str) {
    let appareil = appareil.to_owned();
    backend
        .execute(
            "UPDATE zones SET output_device_id = ?1 WHERE id = ?2",
            &[&appareil, &z],
        )
        .unwrap();
}

#[test]
fn d2_reseau_vers_local_le_mono_reprend() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-9"))
        .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    migrer_zone(&s, z).unwrap();
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, false)]
    );
    changer_de_sortie(&backend, z, "local:hw:2");
    // L'interrupteur, resté à « true », agit en local : coché.
    let c = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(ids(&c), vec![(ID_MONO, true)]);
    assert_eq!(c.plugins[0].settings, Value::Null);
    assert!(ecrire_chaine(&s, z, &c).unwrap().cles_modifiees.is_empty());
}

#[test]
fn d2_local_vers_reseau_le_mono_est_decoche_d_office_puis_agit_si_recoche() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Casque", Some("local"), Some("local:hw:3"))
        .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    migrer_zone(&s, z).unwrap();
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, true)]
    );
    changer_de_sortie(&backend, z, "dlna:uuid-10");
    // Décision du 18/11 soir : même règle qu'à la migration, le mono est
    // décoché d'office au passage sur le réseau (l'ancien moteur l'y
    // ignorait), marqué, et l'ancienne clé n'est pas touchée.
    let c = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(ids(&c), vec![(ID_MONO, false)]);
    assert_eq!(c.plugins[0].settings, json!({"inert_on_network": true}));
    let avant = hors_tess(&s);
    assert!(ecrire_chaine(&s, z, &c).unwrap().cles_modifiees.is_empty());
    assert_eq!(hors_tess(&s), avant);
    // Toujours décoché après cette écriture (l'ombre dit « réseau »).
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, false)]
    );
    // Recoché à la main sur le réseau : il agit, et le reste.
    let mut c = lire_zone(&s, z, true).unwrap().chaine;
    c.plugins[0].enabled = true;
    ecrire_chaine(&s, z, &c).unwrap();
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, true)]
    );
    // Revenue en local : coché (l'interrupteur, resté à « true », y agit).
    changer_de_sortie(&backend, z, "local:hw:3");
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, true)]
    );
}

#[test]
fn d2_temoin_une_zone_locale_qui_reste_locale_garde_son_mono() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Casque", Some("local"), Some("local:hw:6"))
        .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    migrer_zone(&s, z).unwrap();
    // Une autre sortie locale : rien ne change.
    changer_de_sortie(&backend, z, "local:hw:7");
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, true)]
    );
}

#[test]
fn d2_coche_apres_la_migration_sur_le_reseau_il_agit() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-11"))
        .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    migrer_zone(&s, z).unwrap();
    // Une ancienne route le décoche puis le recoche : il agit.
    s.delete(&format!("zone_{z}_mono_downmix")).unwrap();
    migrer_zone(&s, z).unwrap();
    assert!(lire_zone(&s, z, true).unwrap().chaine.plugins.is_empty());
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, true)]
    );
    // Et coché via TESS sur une zone réseau fraîchement migrée, aussi.
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Cuisine", Some("dlna"), Some("dlna:uuid-12"))
        .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    let mut c = migrer_zone(&s, z).unwrap().chaine;
    c.plugins[0].enabled = true;
    c.plugins[0].settings = Value::Null;
    ecrire_chaine(&s, z, &c).unwrap();
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, true)]
    );
}

// ── Marques de la lecture : elles appartiennent au serveur ──────────

fn coche_dans(s: &SettingsRepo, cle: &str) -> bool {
    s.get(cle)
        .unwrap()
        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .and_then(|v| v.get("enabled").and_then(Value::as_bool))
        .unwrap_or(false)
}

#[test]
fn groupe_b_un_put_qui_decoche_le_gagnant_decoche_aussi_les_ecartes() {
    let (_b, s) = base();
    let z = 43;
    installer(&s, &["crossfeed", "crossfeed-pro", "crosstalk"]);
    groupe_tout_coche(&s, z);
    // Le client lit la chaîne migrée, décoche Crosstalk et renvoie les
    // réglages des membres écartés tels qu'il les a lus (marque comprise).
    let mut c = migrer_zone(&s, z).unwrap().chaine;
    c.plugins
        .iter_mut()
        .find(|m| m.id == "crosstalk")
        .unwrap()
        .enabled = false;
    assert!(
        maillon(&c, "crossfeed-pro")
            .settings
            .get("replaced_in_group")
            .is_some()
    );
    ecrire_chaine(&s, z, &c).unwrap();
    // Plus aucun membre coché, ni dans les anciennes clés (l'ancien moteur ne
    // joue plus rien) ni à la lecture (rien n'est recoché).
    for cle in [
        format!("zone_{z}_crossfeed"),
        format!("zone_{z}_native_plugin_crossfeed-pro"),
        format!("zone_{z}_native_plugin_crosstalk"),
    ] {
        assert!(!coche_dans(&s, &cle), "{cle} reste cochée");
    }
    let lue = lire_zone(&s, z, true).unwrap().chaine;
    assert!(lue.plugins.iter().all(|m| !m.enabled), "{:?}", ids(&lue));
    // Réglages gardés, marque retirée.
    assert_eq!(
        maillon(&lue, "crossfeed-pro").settings["profile"],
        json!("hd800")
    );
    assert!(
        maillon(&lue, "crossfeed")
            .settings
            .get("replaced_in_group")
            .is_none()
    );
}

#[test]
fn groupe_b_une_marque_forgee_par_le_client_n_est_pas_gardee() {
    let (_b, s) = base();
    let z = 44;
    installer(&s, &["crossfeed", "crossfeed-pro"]);
    s.set(
        &format!("zone_{z}_native_plugin_crossfeed-pro"),
        r#"{"enabled":true,"profile":"hd800"}"#,
    )
    .unwrap();
    migrer_zone(&s, z).unwrap();
    // Le client coche le crossfeed intégré et décoche Crossfeed Pro en le
    // disant « remplacé » : l'ancienne clé de Crossfeed Pro, plus
    // prioritaire, ne doit pas rester cochée (l'ancien moteur le jouerait).
    let mut c = ChaineTess::default();
    c.plugins.push(MaillonGreffon {
        id: "crossfeed".into(),
        enabled: true,
        settings: json!({"amount": 0.3}),
    });
    c.plugins.push(MaillonGreffon {
        id: "crossfeed-pro".into(),
        enabled: false,
        settings: json!({"profile": "hd800", "replaced_in_group": "crossfeed"}),
    });
    ecrire_chaine(&s, z, &c).unwrap();
    assert!(coche_dans(&s, &format!("zone_{z}_crossfeed")));
    assert!(!coche_dans(
        &s,
        &format!("zone_{z}_native_plugin_crossfeed-pro")
    ));
    let lue = lire_zone(&s, z, true).unwrap().chaine;
    assert_eq!(
        ids(&lue),
        vec![("crossfeed", true), ("crossfeed-pro", false)]
    );
}

#[test]
fn d2_un_mono_decoche_a_la_main_sur_le_reseau_reste_decoche_en_local() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-13"))
        .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    let mut c = migrer_zone(&s, z).unwrap().chaine;
    // Le client coche puis décoche le mono en renvoyant les réglages lus
    // (marque comprise).
    c.plugins[0].enabled = true;
    ecrire_chaine(&s, z, &c).unwrap();
    c.plugins[0].enabled = false;
    ecrire_chaine(&s, z, &c).unwrap();
    assert_eq!(s.get(&format!("zone_{z}_mono_downmix")).unwrap(), None);
    changer_de_sortie(&backend, z, "local:hw:4");
    let lue = lire_zone(&s, z, true).unwrap().chaine;
    assert!(lue.plugins.iter().all(|m| !m.enabled), "{:?}", ids(&lue));
}

#[test]
fn d2_temoin_un_put_inchange_garde_la_marque_du_mono() {
    let (backend, s) = base();
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let z = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-14"))
        .unwrap();
    s.set(&format!("zone_{z}_mono_downmix"), "true").unwrap();
    let c = migrer_zone(&s, z).unwrap().chaine;
    assert!(ecrire_chaine(&s, z, &c).unwrap().cles_modifiees.is_empty());
    changer_de_sortie(&backend, z, "local:hw:5");
    assert_eq!(
        ids(&lire_zone(&s, z, true).unwrap().chaine),
        vec![(ID_MONO, true)]
    );
}

// ── D1 : une chaîne écrite à la main ne lève pas le droit Premium ───

#[test]
fn d1_une_chaine_stockee_qui_se_dit_estimation_est_relue_par_le_serveur() {
    // Une correction MESURÉE, migrée ; puis la chaîne enregistrée est
    // réécrite directement (sans `ecrire_chaine`) avec une origine
    // « estimation ».
    let (_b, s) = base();
    let z = 60;
    poser_la_piece(&s, z, "Calibration abc", None);
    let mut c = migrer_zone(&s, z).unwrap().chaine;
    assert_eq!(c.plugins[0].settings["origin"], json!("measured"));
    c.plugins[0]
        .settings
        .as_object_mut()
        .unwrap()
        .insert("origin".into(), json!("estimation"));
    s.set(&cle_chaine_zone(z), &serde_json::to_string(&c).unwrap())
        .unwrap();
    let l = lire_zone(&s, z, false).unwrap();
    assert!(
        l.conversion_piece,
        "le droit Premium a été levé par le stockage"
    );
    // Témoin : une vraie estimation enregistrée par le serveur reste Free.
    let (_b, s) = base();
    let z = 62;
    poser_la_piece(&s, z, "Estimation 8", None);
    migrer_zone(&s, z).unwrap();
    let l = lire_zone(&s, z, false).unwrap();
    assert!(!l.conversion_piece);
    assert!(correction_estimee(&l.chaine.plugins[0]));
}

#[test]
fn d1_les_cles_tenues_par_le_serveur() {
    for cle in [
        "room_profile_7",
        "room_profile_index",
        "zone_7_tess_chain",
        "zone_12_tess_chain_sync",
    ] {
        assert!(cle_tenue_par_le_serveur(cle), "{cle}");
    }
    for cle in [
        "zone_7_eq_profile",
        "zone_7_tess_chainette",
        "zone_x_tess_chain",
        "room_correction_enabled",
        "theme",
    ] {
        assert!(!cle_tenue_par_le_serveur(cle), "{cle}");
    }
}

/// Relecture du 19/11 : une chaîne d'une version FUTURE (écrite par un
/// serveur plus récent, puis retour en arrière) ne garde pas l'origine
/// qu'elle stocke : `lire_zone` la recalcule d'après `room_profile_{id}` et
/// convertit pour un compte gratuit, comme pour une chaîne courante.
#[test]
fn d1_une_chaine_de_version_future_est_relue_avec_l_origine_du_serveur() {
    let (_b, s) = base();
    let z = 64;
    poser_la_piece(&s, z, "Calibration abc", None);
    let mut c = migrer_zone(&s, z).unwrap().chaine;
    c.version = crate::tess::chaine::VERSION_CHAINE + 1;
    c.plugins[0]
        .settings
        .as_object_mut()
        .unwrap()
        .insert("origin".into(), json!("estimation"));
    s.set(&cle_chaine_zone(z), &serde_json::to_string(&c).unwrap())
        .unwrap();

    let premium = lire_zone(&s, z, true).unwrap();
    assert_eq!(
        premium.chaine.version,
        crate::tess::chaine::VERSION_CHAINE + 1
    );
    assert_eq!(
        maillon(&premium.chaine, ID_CORRECTION_PIECE).settings["origin"],
        json!("measured"),
        "origine lue dans le stockage brut d'une chaîne future"
    );
    let gratuit = lire_zone(&s, z, false).unwrap();
    assert!(
        gratuit.conversion_piece,
        "le droit Premium a été levé par une chaîne de version future"
    );
    assert!(
        !gratuit
            .chaine
            .plugins
            .iter()
            .any(|m| m.id == ID_CORRECTION_PIECE && m.enabled),
        "{:?}",
        ids(&gratuit.chaine)
    );
    // La chaîne stockée n'est pas réécrite pour autant.
    assert!(!migrer_zone(&s, z).unwrap().ecrite);

    // Témoin : une vraie estimation reste Free sur ce chemin aussi.
    let (_b, s) = base();
    let z = 65;
    poser_la_piece(&s, z, "Estimation 8", None);
    let mut c = migrer_zone(&s, z).unwrap().chaine;
    c.version = crate::tess::chaine::VERSION_CHAINE + 1;
    s.set(&cle_chaine_zone(z), &serde_json::to_string(&c).unwrap())
        .unwrap();
    let l = lire_zone(&s, z, false).unwrap();
    assert!(!l.conversion_piece);
    assert!(correction_estimee(&l.chaine.plugins[0]));
}
