//! rc14 (rc13.91) — relevé de Xavier (Windows, WASAPI partagé) :
//! suréchantillonnage activé, la sortie le refuse, et TESS se taisait (écran
//! TESS : étape « active » ; bloc `tess` du chemin : `not_in_service` sans
//! raison).

use super::*;
use std::sync::Arc;
use tune_core::db::sqlite::SqliteDb;
use tune_core::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm};
use tune_core::playback::NowPlaying;
use tune_core::tess::resolution::{CodeEtape, EtapeResolue, MotifEtape};

use super::refus_surechantillonnage_tess::{
    avec_le_refus_du_surechantillonneur, refus_de_la_sortie_locale,
};

const REFUS_WASAPI: &str = "shared_mixer_rate_not_a_multiple";

/// Une chaîne qui demande le suréchantillonnage PCM.
fn chaine_pcm() -> tune_core::tess::ChaineTess {
    serde_json::from_value(serde_json::json!({
        "version": 1,
        "fixed": {"upsampling": {"mode": "pcm", "filter": "precision"}}
    }))
    .unwrap()
}

/// Une chaîne avec ces greffons cochés.
fn chaine_avec(greffons: &[(&str, Value)]) -> tune_core::tess::ChaineTess {
    let plugins: Vec<Value> = greffons
        .iter()
        .map(|(id, s)| serde_json::json!({"id": id, "enabled": true, "settings": s}))
        .collect();
    serde_json::from_value(serde_json::json!({"version": 1, "plugins": plugins})).unwrap()
}

fn zone_locale() -> (Arc<dyn tune_core::db::backend::DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Salon", Some("local"), Some("local:DAC"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            format: Some("flac".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        volume: 1.0,
        // La sortie a ouvert à 44,1 kHz et refusé le greffon.
        transformations_reelles: Some(
            TransformationsReelles::nouvelles(
                AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier16, 2).unwrap(),
                FormatOuvert::new(44_100, 2),
                false,
            )
            .avec_refus_surechantillonnage(Some(REFUS_WASAPI)),
        ),
        ..Default::default()
    };
    (backend, zone, ps)
}

/// Premium, greffon installé, suréchantillonnage activé par la page du
/// greffon (ancienne clé `zone_{id}_upsampler`) — le geste de Xavier.
fn activer_par_la_page_du_greffon(
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
) {
    let s = SettingsRepo::with_backend(backend.clone());
    s.set(tune_core::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    s.set("plugin_upsampler_installed", "true").unwrap();
    s.set("plugin_upsampler_enabled", "true").unwrap();
    tune_core::audio::surechantillonnage::enregistrer_les_reglages(
        &s,
        zone_id,
        &serde_json::json!({"enabled": true, "filter": "precision", "max_ratio": null}),
    )
    .unwrap();
}

fn etape_upsampling(tess: &Value) -> &Value {
    tess["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == "upsampling")
        .expect("étape upsampling")
}

#[test]
fn le_bloc_tess_du_chemin_dit_pourquoi_l_upsampling_ne_tourne_pas() {
    let (backend, zone, ps) = zone_locale();
    activer_par_la_page_du_greffon(&backend, zone.id.unwrap());
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        ..Default::default()
    };
    let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "WASAPI", Some(&fil)).unwrap();
    // Le chemin historique le savait déjà.
    assert_eq!(sp["upsampling"]["reason"], REFUS_WASAPI, "{sp}");

    let tess =
        super::chemin_tess_t16::chemin_tess_de_la_zone(&backend, &zone, &ps, &sp, true).unwrap();
    let e = etape_upsampling(&tess);
    // Réglage unique : activé par la page du greffon, TESS le voit demandé.
    assert_eq!(e["requested"], true, "{e}");
    assert_eq!(e["effective"], false, "{e}");
    assert_eq!(e["reason"], "not_in_service", "{e}");
    assert_eq!(e["detail"], REFUS_WASAPI, "{e}");
    assert_eq!(e["params"]["refusal"], REFUS_WASAPI, "{e}");
    let texte = e["params"]["refusal_text"].as_str().unwrap_or_default();
    assert!(texte.contains("Windows"), "le geste est nommé : {texte}");
    // Les réglages affichés restent là.
    assert_eq!(e["params"]["filter"], "precision", "{e}");
}

#[test]
fn l_ecran_tess_ne_dit_plus_actif_un_upsampling_refuse() {
    // `GET /zones/{id}/tess` publie la résolution : l'étape y était
    // effective. La raison de la sortie l'éteint.
    let mut etapes = [EtapeResolue {
        code: CodeEtape::Upsampling,
        plugin_id: None,
        fixed: true,
        requested: true,
        effective: true,
        reason: None,
        detail: None,
        params: Some(serde_json::json!({"mode": "pcm", "filter": "precision"})),
        latency_frames: None,
    }];
    let (_, zone, ps) = zone_locale();
    let refus = refus_de_la_sortie_locale(zone.output_type.as_deref(), &ps);
    assert_eq!(refus, Some(REFUS_WASAPI));
    avec_le_refus_du_surechantillonneur(etapes.iter_mut(), &chaine_pcm(), refus);
    let e = &etapes[0];
    assert!(!e.effective);
    assert_eq!(e.reason, Some(MotifEtape::NotInService));
    let p = e.params.as_ref().unwrap();
    assert_eq!(p["refusal"], REFUS_WASAPI);
    assert_eq!(p["filter"], "precision");
    assert!(!p["refusal_text"].as_str().unwrap().is_empty());
}

#[test]
fn temoins_rien_a_dire_rien_ne_change() {
    let base = EtapeResolue {
        code: CodeEtape::Upsampling,
        plugin_id: None,
        fixed: true,
        requested: true,
        effective: false,
        reason: Some(MotifEtape::PremiumRequired),
        detail: None,
        params: None,
        latency_frames: None,
    };
    // Une autre raison (licence) garde la sienne.
    let mut etapes = [base.clone()];
    avec_le_refus_du_surechantillonneur(etapes.iter_mut(), &chaine_pcm(), Some(REFUS_WASAPI));
    assert_eq!(etapes[0], base);
    // Aucun refus : rien ne bouge.
    let mut actif = base.clone();
    actif.effective = true;
    actif.reason = None;
    let mut etapes = [actif.clone()];
    avec_le_refus_du_surechantillonneur(etapes.iter_mut(), &chaine_pcm(), None);
    assert_eq!(etapes[0], actif);
    // Zone réseau, ou rien ne joue : pas de refus de la sortie locale.
    let (_, _, mut ps) = zone_locale();
    assert_eq!(refus_de_la_sortie_locale(Some("dlna"), &ps), None);
    ps.now_playing = None;
    assert_eq!(refus_de_la_sortie_locale(Some("local"), &ps), None);
}

#[test]
fn chaque_refus_de_la_sortie_a_une_phrase_dans_chaque_langue() {
    for code in [
        "rates_not_measured",
        REFUS_WASAPI,
        "output_path_not_supported",
    ] {
        let d = super::description_du_refus_surechantillonnage(code);
        assert!(!d.is_empty(), "{code}");
        assert_ne!(
            d,
            super::description_du_refus_surechantillonnage("code_inconnu"),
            "{code} a sa phrase, pas le « non appliqué » générique"
        );
    }
    let fr = super::description_du_refus_surechantillonnage(REFUS_WASAPI);
    assert!(fr.contains("mélangeur") && fr.contains("Windows"), "{fr}");
    let geste = super::description_du_refus_surechantillonnage("output_path_not_supported");
    assert!(
        geste.contains("exclusif") && geste.contains("ASIO"),
        "{geste}"
    );
}

// ── Rémi (fil 3097) : la convolution sautée à cette cadence ─────────

/// Un WAV mono en flottants 32 bits : une impulsion de Dirac.
fn ecrire_ri(chemin: &std::path::Path, cadence: u32, taps: usize) {
    let donnees = taps * 4;
    let mut w = Vec::with_capacity(44 + donnees);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&cadence.to_le_bytes());
    w.extend_from_slice(&(cadence * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 1.0 } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(chemin, w).unwrap();
}

#[test]
fn une_convolution_sautee_a_cette_cadence_le_dit_dans_les_deux_chemins() {
    let (backend, zone, mut ps) = zone_locale();
    ps.transformations_reelles = None;
    let zid = zone.id.unwrap();
    // Filtre à 384 kHz, flux (radio) à 48 kHz, rééchantillonnage du filtre
    // refusé par la zone : le convolueur saute.
    let ri = tune_core::test_scratch::scratch_file("ri-3097-384k", ".wav");
    ecrire_ri(ri.path(), 384_000, 256);
    let s = SettingsRepo::with_backend(backend.clone());
    s.set(&format!("ir_path_{zid}"), &ri.as_str()).unwrap();
    s.set(&format!("ir_resample_{zid}"), "false").unwrap();
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 48_000,
        bit_depth: 16,
        ..Default::default()
    };
    let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "ALSA", Some(&fil)).unwrap();

    // Chemin historique : inactif, avec les deux cadences en kHz.
    let rc = sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == "room_correction")
        .expect("étape correction de pièce");
    assert_eq!(rc["applied"], false, "{rc}");
    let d = rc["description"].as_str().unwrap();
    assert!(
        d.contains("inactive à cette fréquence") && d.contains("filtre 384 kHz, flux 48 kHz"),
        "{d}"
    );

    // Bloc TESS : la ligne du greffon dit la même chose.
    let tess =
        super::chemin_tess_t16::chemin_tess_de_la_zone(&backend, &zone, &ps, &sp, true).unwrap();
    let conv = tess["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["plugin_id"] == "convolution")
        .unwrap_or_else(|| panic!("greffon convolution : {tess}"));
    assert_eq!(conv["requested"], true, "{conv}");
    assert_eq!(conv["effective"], false, "{conv}");
    assert_eq!(conv["params"]["refusal"], "rate_mismatch", "{conv}");
    assert_eq!(conv["params"]["refusal_text"], d, "{conv}");
}

#[test]
fn l_ecran_tess_eteint_la_convolution_sautee() {
    let mut etapes = [EtapeResolue {
        code: CodeEtape::Plugin,
        plugin_id: Some("convolution".into()),
        fixed: false,
        requested: true,
        effective: true,
        reason: None,
        detail: None,
        params: None,
        latency_frames: None,
    }];
    super::refus_surechantillonnage_tess::avec_la_raison_de_la_convolution(
        etapes.iter_mut(),
        &chaine_avec(&[(
            "convolution",
            serde_json::json!({"ir_path": "/ir/salon-384k.wav"}),
        )]),
        Some(("rate_mismatch", "inactive à cette fréquence")),
    );
    assert!(!etapes[0].effective);
    assert_eq!(etapes[0].reason, Some(MotifEtape::NotInService));
    assert_eq!(
        etapes[0].params.as_ref().unwrap()["refusal_text"],
        "inactive à cette fréquence"
    );
    // Témoin : un autre greffon n'est pas touché.
    let mut eq = [EtapeResolue {
        plugin_id: Some("equalizer-graphic".into()),
        ..etapes[0].clone()
    }];
    eq[0].effective = true;
    eq[0].reason = None;
    eq[0].params = None;
    let avant = eq.clone();
    super::refus_surechantillonnage_tess::avec_la_raison_de_la_convolution(
        eq.iter_mut(),
        &chaine_avec(&[]),
        Some(("rate_mismatch", "x")),
    );
    assert_eq!(eq, avant);
}

/// Relecture de #9533 (M1) : `room-correction` est le plus souvent une
/// façade d'égaliseur (bandes). Une convolution sautée à cette cadence ne
/// doit pas l'éteindre : elle s'applique.
#[test]
fn la_correction_par_egaliseur_reste_active_quand_la_convolution_saute() {
    let etape = |id: &str| EtapeResolue {
        code: CodeEtape::Plugin,
        plugin_id: Some(id.into()),
        fixed: false,
        requested: true,
        effective: true,
        reason: None,
        detail: None,
        params: None,
        latency_frames: None,
    };
    let chaine = chaine_avec(&[
        (
            "room-correction",
            serde_json::json!({"bands": [{"freq": 63.0, "gain": -4.0, "q": 2.0}]}),
        ),
        (
            "convolution",
            serde_json::json!({"ir_path": "/ir/salon-384k.wav"}),
        ),
    ]);
    let mut etapes = [etape("room-correction"), etape("convolution")];
    super::refus_surechantillonnage_tess::avec_la_raison_de_la_convolution(
        etapes.iter_mut(),
        &chaine,
        Some(("rate_mismatch", "inactive à cette fréquence")),
    );
    assert!(
        etapes[0].effective,
        "correction par égaliseur : {:?}",
        etapes[0]
    );
    assert_eq!(etapes[0].reason, None);
    assert!(!etapes[1].effective, "la convolution, elle, saute");

    // Une correction de pièce qui désigne un filtre de convolution saute
    // avec lui.
    let chaine = chaine_avec(&[(
        "room-correction",
        serde_json::json!({"ir_path": "/ir/piece-384k.wav"}),
    )]);
    let mut etapes = [etape("room-correction")];
    super::refus_surechantillonnage_tess::avec_la_raison_de_la_convolution(
        etapes.iter_mut(),
        &chaine,
        Some(("rate_mismatch", "inactive à cette fréquence")),
    );
    assert!(!etapes[0].effective);
}

/// Relecture de #9533 (M2) : vers le DSD (#9518), le greffon PCM de la
/// sortie est éteint par projection et déclaré refusé ; l'étape, portée par
/// le modulateur, ne doit pas se dire inactive. Et `disabled` n'est jamais
/// une raison à afficher.
#[test]
fn le_refus_du_greffon_pcm_n_eteint_pas_une_cible_dsd() {
    let actif = EtapeResolue {
        code: CodeEtape::Upsampling,
        plugin_id: None,
        fixed: true,
        requested: true,
        effective: true,
        reason: None,
        detail: None,
        params: None,
        latency_frames: None,
    };
    let dsd: tune_core::tess::ChaineTess = serde_json::from_value(serde_json::json!({
        "version": 1,
        "fixed": {"upsampling": {"mode": "dsd", "rate": "dsd128"}}
    }))
    .unwrap();
    for refus in [Some("disabled"), Some(REFUS_WASAPI)] {
        let mut etapes = [actif.clone()];
        avec_le_refus_du_surechantillonneur(etapes.iter_mut(), &dsd, refus);
        assert_eq!(etapes[0], actif, "cible DSD, refus {refus:?}");
    }
    // En PCM, `disabled` (réglage désactivé à l'ouverture) ne dit rien non plus.
    let mut etapes = [actif.clone()];
    avec_le_refus_du_surechantillonneur(etapes.iter_mut(), &chaine_pcm(), Some("disabled"));
    assert_eq!(etapes[0], actif);
}

/// Relecture de #9533 (m1, m2, m3) : chaque texte nomme le geste de SA
/// plateforme.
#[test]
fn les_textes_suivent_la_plateforme_et_nomment_le_geste() {
    use super::signal_path::cle_du_refus_selon_la_plateforme as cle;
    let fr = |k: &str| super::textes_signal::texte("fr", k);
    let w = fr(cle("rates_not_measured", "windows"));
    assert!(w.contains("Windows") && !w.contains("hw:"), "{w}");
    let l = fr(cle("rates_not_measured", "linux"));
    assert!(l.contains("hw:"), "{l}");
    let m = fr(cle("output_path_not_supported", "macos"));
    assert!(m.contains("exclusif") && !m.contains("ASIO"), "{m}");
    let win = fr(cle("output_path_not_supported", "windows"));
    assert!(win.contains("ASIO"), "{win}");
    let test = fr(cle("machine_test_missing", "windows"));
    assert!(
        test.contains("page du greffon Suréchantillonnage"),
        "{test}"
    );
    let egal = fr(cle("shared_mixer_rate_not_above_source", "windows"));
    assert!(
        egal.contains("supérieure") && !egal.contains("pas un multiple"),
        "{egal}"
    );
    assert!(fr(cle(REFUS_WASAPI, "windows")).contains("multiple"));
}
