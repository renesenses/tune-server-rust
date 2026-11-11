//! #6178 (kiran, fil 2233) — sur une zone HQPlayer sans traitement, le chemin
//! du signal montrait une étape « Decoder » (« FLAC ») entre la source et
//! HQPlayer : on y lisait que Tune décode avant HQPlayer. C'est HQPlayer qui
//! décode ; Tune sert les octets tels quels.

use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn chemin_de(output_type: &str, reglages: &[(&str, &str)]) -> Value {
    chemin_avec_fil(output_type, reglages, "flac", 50.0)
}

/// `format_du_fil` : ce que Tune sert réellement (`flac` = octets de la
/// source, `wav` = Tune a décodé). `volume` : le curseur de la zone.
fn chemin_avec_fil(
    output_type: &str,
    reglages: &[(&str, &str)],
    format_du_fil: &str,
    volume: f64,
) -> Value {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    for (cle, valeur) in reglages {
        SettingsRepo::with_backend(backend.clone())
            .set(cle, valeur)
            .unwrap();
    }
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Salon", Some(output_type), Some("dev-1"))
        .unwrap();
    repo.update_volume(id, volume).unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.format = Some("flac".into());
    t.sample_rate = Some(96_000);
    t.bit_depth = Some(24);
    let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_track_gain", "-6.5 dB")
        .unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            track_id: Some(tid),
            format: Some("flac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };
    let fil = StreamInfo {
        format: format_du_fil.into(),
        sample_rate: 96_000,
        bit_depth: 24,
        ..Default::default()
    };
    build_signal_path(&ps, &zone, &backend, Some("HQPlayer"), "none", Some(&fil)).unwrap()
}

fn noms(sp: &Value) -> Vec<String> {
    sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn hqplayer_sans_traitement_n_a_pas_d_etape_de_decodage_par_tune_6178() {
    let sp = chemin_de("hqplayer", &[]);
    assert!(
        !noms(&sp).contains(&"Decoder".to_string()),
        "#6178 : HQPlayer décode lui-même, le panneau prête un décodage à Tune : {:?}",
        noms(&sp)
    );
    assert_eq!(noms(&sp)[0], "Source");
}

/// Les sorties que Tune alimente en PCM gardent leur décodeur : c'est Tune
/// qui décode pour elles.
///
/// #8462 — DLNA ne fait plus partie de cette liste. Jusqu'à la rc11, ce test
/// gardait VOLONTAIREMENT l'étape « Decoder » sur une zone DLNA servie en
/// passthrough. Décision de Bertrand du 11/11 (fil 2762, kiran, renderer
/// « Diretta UPnP ») : en passthrough vers DLNA ou UPnP, le FLAC part tel
/// quel et c'est le RENDERER qui le décode ; l'étape prêtait ce décodage à
/// Tune. Elle ne revient que quand Tune décode lui-même (tests ci-dessous).
#[test]
fn les_sorties_que_tune_alimente_gardent_leur_decodeur() {
    for sortie in ["local", "oaat"] {
        let sp = chemin_de(sortie, &[]);
        assert_eq!(
            noms(&sp).get(1).map(String::as_str),
            Some("Decoder"),
            "{sortie} : {:?}",
            noms(&sp)
        );
    }
}

/// #8462 — DLNA ou UPnP (OpenHome) en passthrough : le renderer décode, le
/// chemin ne montre pas d'étape « Decoder » de Tune.
#[test]
fn dlna_en_passthrough_le_renderer_decode_pas_d_etape_decoder_8462() {
    for sortie in ["dlna", "openhome"] {
        let sp = chemin_de(sortie, &[]);
        assert!(
            !noms(&sp).contains(&"Decoder".to_string()),
            "#8462 {sortie} : FLAC servi tel quel, le panneau prête le décodage à Tune : {:?}",
            noms(&sp)
        );
        assert_eq!(noms(&sp)[0], "Source");
    }
}

/// #8462 — témoin : dès que Tune décode pour le renderer (fil WAV : WAV forcé,
/// plafond LPCM, repli), l'étape revient.
#[test]
fn dlna_servi_en_wav_garde_son_decodeur_8462() {
    let sp = chemin_avec_fil("dlna", &[], "wav", 50.0);
    assert_eq!(
        noms(&sp).get(1).map(String::as_str),
        Some("Decoder"),
        "{:?}",
        noms(&sp)
    );
}

/// #8462 — témoin : ReplayGain cuit dans le flux DLNA, Tune décode.
#[test]
fn dlna_avec_replaygain_garde_son_decodeur_8462() {
    let sp = chemin_de("dlna", &[(tune_core::audio::replaygain::MODE_KEY, "track")]);
    assert!(
        noms(&sp).contains(&"ReplayGain".to_string()),
        "prémisse : le ReplayGain s'applique : {:?}",
        noms(&sp)
    );
    assert_eq!(noms(&sp).get(1).map(String::as_str), Some("Decoder"));
}

fn etape<'a>(sp: &'a Value, nom: &str) -> Option<&'a Value> {
    sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == nom)
}

/// #8462 — sur une sortie réseau, le volume est une commande au renderer, pas
/// un traitement du flux : l'étape le dit, et le verdict ne bouge pas.
#[test]
fn dlna_le_volume_est_celui_du_renderer_et_ne_change_pas_le_verdict_8462() {
    let sp = chemin_de("dlna", &[]);
    let volume = etape(&sp, "Volume").expect("curseur à 50 % : étape Volume");
    assert_eq!(volume["description"], "Volume du renderer 50%", "{sp}");
    assert_eq!(volume["bit_perfect"], true);
    let plein = chemin_avec_fil("dlna", &[], "flac", 100.0);
    assert!(etape(&plein, "Volume").is_none());
    assert_eq!(
        sp["bit_perfect"], plein["bit_perfect"],
        "le volume du renderer ne touche pas le verdict"
    );
    assert_eq!(sp["bit_perfect"], true, "FLAC servi tel quel : {sp}");
}

/// #8462 — témoin : sur la sortie locale, l'étape garde son libellé.
#[test]
fn local_sans_sonde_garde_son_libelle_de_volume_8462() {
    let sp = chemin_de("local", &[]);
    let volume = etape(&sp, "Volume").expect("étape Volume");
    assert!(
        !volume["description"].as_str().unwrap().contains("renderer"),
        "{sp}"
    );
}

/// Dès que Tune touche le signal pour HQPlayer (ReplayGain), il décode : l'étape
/// revient.
#[test]
fn hqplayer_avec_replaygain_retrouve_son_decodeur() {
    let sp = chemin_de(
        "hqplayer",
        &[(tune_core::audio::replaygain::MODE_KEY, "track")],
    );
    assert!(
        noms(&sp).contains(&"ReplayGain".to_string()),
        "prémisse : le ReplayGain s'applique : {:?}",
        noms(&sp)
    );
    assert_eq!(noms(&sp).get(1).map(String::as_str), Some("Decoder"));
}
