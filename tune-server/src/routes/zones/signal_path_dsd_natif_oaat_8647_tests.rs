//! #8647 (fil 2794, rc11, macOS) — DSD256 vers une zone OAAT : le chemin du
//! signal affichait « DSD256 → WAV 352 kHz/24 bits » alors que la sortie
//! OAAT lisait le `.dsf` elle-même et que le DAC recevait du DSD256 natif
//! (`oaat: native DSD streaming ... dsd_rate=Some(256)` dans le journal).
//!
//! Contre-épreuve : sans le correctif, `oaat_transcodes` reste vrai pour tout
//! DSD et le premier test tombe (étape « → WAV », pas d'étape DSD natif,
//! transport non bit-perfect). Le témoin (sortie OAAT repliée en PCM) reste
//! vert avant comme après.

use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn zone_oaat(device_id: &str) -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Zone 1", Some("oaat"), Some(device_id))
        .unwrap();
    repo.update_dsd_mode(id, "native").unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    (backend, zone)
}

fn dsd256_en_lecture() -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Lights of Barcelona".into(),
            format: Some("dsf".into()),
            sample_rate: Some(11_289_600),
            bit_depth: Some(1),
            stream_id: Some("sid-dsd256".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

/// Le WAV que l'orchestrateur a préparé (`oaat_needs_wav`), tel que la
/// session le décrit — que la sortie le lise ou non.
fn fil_wav_352() -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 352_800,
        bit_depth: 24,
        ..Default::default()
    }
}

fn etape<'a>(v: &'a Value, cle: &str, valeur: &str) -> Option<&'a Value> {
    v.get("steps")?
        .as_array()?
        .iter()
        .find(|s| s.get(cle).and_then(Value::as_str) == Some(valeur))
}

fn chemin(device_id: &str, natif: bool, fil: Option<&StreamInfo>) -> (Value, Arc<AtomicBool>) {
    let (backend, zone) = zone_oaat(device_id);
    let drapeau = Arc::new(AtomicBool::new(natif));
    tune_core::outputs::dsd_natif_oaat_8647::inscrire(device_id, &drapeau);
    let sp = build_signal_path_lang(
        "fr",
        &dsd256_en_lecture(),
        &zone,
        &backend,
        Some("Zone 1"),
        "none",
        fil,
    )
    .unwrap();
    (sp, drapeau)
}

/// ⭐ Le cas du fil 2794 : la sortie part en DSD natif. Aucune conversion
/// DSD → PCM, une étape « DSD256 natif (OAAT) », un transport bit-perfect.
#[test]
fn le_dsd_natif_d_oaat_se_dit_natif_et_sans_conversion_8647() {
    for fil in [None, Some(fil_wav_352())] {
        let (sp, _drapeau) = chemin("oaat:fil-2794-natif", true, fil.as_ref());
        let natif = etape(&sp, "code", "dsd_natif")
            .unwrap_or_else(|| panic!("étape DSD natif absente : {sp}"));
        assert_eq!(natif["description"], "DSD256 natif (OAAT)", "{sp}");
        assert_eq!(natif["bit_perfect"], true, "{sp}");
        let conversions: Vec<&Value> = sp["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| {
                s["name"] == "Transcoder"
                    && s["description"].as_str().is_some_and(|d| d.contains("WAV"))
            })
            .collect();
        assert!(
            conversions.is_empty(),
            "aucune conversion DSD -> WAV n'a lieu : {sp}"
        );
        let transport = etape(&sp, "name", "Transport").expect("étape Transport");
        assert_eq!(transport["bit_perfect"], true, "{sp}");
        assert_eq!(sp["bit_perfect"], true, "le DSD part intact : {sp}");
        assert_eq!(
            etape(&sp, "name", "Source").unwrap()["description"],
            "DSD256 11.3 MHz"
        );
    }
}

/// Témoin : la voie directe n'a pas été prise (récepteur sans DSD, plafond
/// dépassé, #8486) — le WAV préparé part, la conversion reste affichée et
/// rien n'est dit natif.
#[test]
fn le_repli_pcm_d_oaat_garde_sa_conversion_8647() {
    let fil = fil_wav_352();
    let (sp, _drapeau) = chemin("oaat:fil-2794-repli", false, Some(&fil));
    assert!(etape(&sp, "code", "dsd_natif").is_none(), "{sp}");
    let transcoder = etape(&sp, "name", "Transcoder")
        .unwrap_or_else(|| panic!("la conversion DSD -> WAV doit être dite : {sp}"));
    assert!(
        transcoder["description"].as_str().unwrap().contains("WAV"),
        "{sp}"
    );
    assert_eq!(transcoder["bit_perfect"], false, "{sp}");
    assert_eq!(sp["bit_perfect"], false, "{sp}");
}

/// Le constat suit la sortie en direct : la même zone, une fois la sortie
/// arrêtée (`stop()` efface le drapeau), retombe sur la conversion.
#[test]
fn le_constat_suit_le_drapeau_de_la_sortie_8647() {
    let device = "oaat:fil-2794-bascule";
    let (sp, drapeau) = chemin(device, true, None);
    assert!(etape(&sp, "code", "dsd_natif").is_some(), "{sp}");
    drapeau.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(!dsd_natif_sur_la_sortie_oaat(true, "oaat", Some(device)));
    // Une source PCM ou une autre sortie ne sont jamais concernées.
    drapeau.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(!dsd_natif_sur_la_sortie_oaat(false, "oaat", Some(device)));
    assert!(!dsd_natif_sur_la_sortie_oaat(true, "dlna", Some(device)));
}

#[test]
fn le_repli_oaat_nomme_la_cause_dans_le_chemin_du_signal_8486() {
    use tune_core::outputs::repli_dsd_oaat_8486::{self, Observation, Raison, Repli};
    let id = "oaat:signal-repli-8486";
    let (db, zone) = zone_oaat(id);
    let observation = Arc::new(std::sync::Mutex::new(Observation {
        annonce_recue: true,
        dsd_max_rate: Some(64),
        repli: Some(Repli {
            raison: Raison::Plafond,
            dsd_rate: Some(256),
            dsd_max_rate: Some(64),
        }),
    }));
    repli_dsd_oaat_8486::inscrire(id, &observation);
    let rendre = || {
        build_signal_path_lang(
            "fr",
            &dsd256_en_lecture(),
            &zone,
            &db,
            Some("OAAT"),
            "none",
            Some(&fil_wav_352()),
        )
        .unwrap()
    };
    let chemin = rendre();
    let conversion = etape(&chemin, "code", "oaat_dsd_pcm_fallback")
        .expect("le repli OAAT doit expliquer sa cause sur la zone");
    assert_eq!(
        conversion["detail"],
        "DSD256 converti en PCM : le récepteur accepte au plus DSD64."
    );
    for (raison, fragment) in [
        (Raison::SansDsd, "n’annonce pas"),
        (Raison::Conteneur, "DFF"),
        (Raison::Cadence, "non reconnue"),
    ] {
        observation.lock().unwrap().repli.as_mut().unwrap().raison = raison;
        let chemin = rendre();
        assert!(
            etape(&chemin, "code", "oaat_dsd_pcm_fallback").unwrap()["detail"]
                .as_str()
                .unwrap()
                .contains(fragment)
        );
    }
    repli_dsd_oaat_8486::effacer_repli(&observation);
    assert!(
        etape(&rendre(), "code", "oaat_dsd_pcm_fallback").is_none(),
        "pas de motif périmé après arrêt ou changement de piste"
    );
}
