//! #8489 — capture d'un micro qui n'offre QUE du 24 bits « tassé » sur trois
//! octets (`S24_3LE`, 2 canaux, 48 kHz) : miniDSP UMIK-1 et UMIK-2.
//!
//! CPAL ne connaissait aucun format sur trois octets : un tel périphérique
//! n'avait AUCUNE configuration, et `default_input_config` rendait « The
//! requested stream type is not supported by the device ». Le correctif est
//! dans `vendor/cpal/src/host/alsa/` (voir `vendor/cpal/TUNE-PATCH.md`).
//!
//! Deux étages :
//!
//! - les fonctions pures du correctif (`packed24.rs`, compilé ici TEL QUEL par
//!   `#[path]`, sans cpal ni ALSA) : choix du format ouvert et conversion
//!   S24_3LE/S24_3BE -> entier 24 bits dans un `i32` ;
//! - de bout en bout, sous `local-audio` : un PCM ALSA `null` privé que la
//!   cale `fixtures/alsa_8489/umik_s24_3le.c` (LD_PRELOAD, processus enfant
//!   seulement) réduit à S24_3LE/2 canaux/48 kHz, lu par la VRAIE API publique
//!   de CPAL : la liste des configurations, la configuration choisie,
//!   l'ouverture et le rappel de données.
//!
//! Aucun micro réel : la preuve matérielle reste à faire chez Xavier Joly.

#[path = "../../vendor/cpal/src/host/alsa/packed24.rs"]
mod packed24;

use packed24::{Packed24, unpack_s24_3};

#[test]
fn le_format_tasse_n_est_ouvert_qu_en_capture_et_faute_de_mieux() {
    // L'UMIK-1 : S24_3LE seulement, en capture.
    assert_eq!(
        Packed24::for_capture(true, false, true, false),
        Some(Packed24::Le)
    );
    // Gros-boutiste seulement : ouvert aussi (comme les autres formats).
    assert_eq!(
        Packed24::for_capture(true, false, false, true),
        Some(Packed24::Be)
    );
    // Les deux : l'ordre natif d'abord.
    let natif = if cfg!(target_endian = "little") {
        Packed24::Le
    } else {
        Packed24::Be
    };
    assert_eq!(Packed24::for_capture(true, false, true, true), Some(natif));
    // Le S24 sur quatre octets existe : rien ne change.
    assert_eq!(Packed24::for_capture(true, true, true, true), None);
    // Jamais en lecture : la sortie ne sait pas tasser.
    assert_eq!(Packed24::for_capture(false, false, true, true), None);
    // Rien de tassé : rien à ouvrir.
    assert_eq!(Packed24::for_capture(true, false, false, false), None);
}

#[test]
fn s24_3le_et_s24_3be_donnent_l_entier_24_bits_signe() {
    let cas: [(i32, [u8; 3]); 7] = [
        (0, [0x00, 0x00, 0x00]),
        (1, [0x01, 0x00, 0x00]),
        (-1, [0xFF, 0xFF, 0xFF]),
        (0x12_3456, [0x56, 0x34, 0x12]),
        (8_388_607, [0xFF, 0xFF, 0x7F]),
        (-8_388_608, [0x00, 0x00, 0x80]),
        (-4_194_304, [0x00, 0x00, 0xC0]),
    ];
    for (attendu, le) in cas {
        assert_eq!(Packed24::Le.sample(le), attendu, "LE {le:02X?}");
        let be = [le[2], le[1], le[0]];
        assert_eq!(Packed24::Be.sample(be), attendu, "BE {be:02X?}");
    }
}

#[test]
fn une_periode_stereo_s24_3le_devient_des_i32_puis_des_f32_justes() {
    // Deux trames stéréo : (+0,5 ; -0,5) puis (pleine échelle - 1 LSB ; -1).
    let tasse: [u8; 12] = [
        0x00, 0x00, 0x40, // +4 194 304
        0x00, 0x00, 0xC0, // -4 194 304
        0xFF, 0xFF, 0x7F, // +8 388 607
        0x00, 0x00, 0x80, // -8 388 608
    ];
    let mut sortie = [0xAAu8; 16];
    assert_eq!(unpack_s24_3(Packed24::Le, &tasse, &mut sortie), 4);
    let entiers: Vec<i32> = sortie
        .chunks_exact(4)
        .map(|c| i32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(entiers, [4_194_304, -4_194_304, 8_388_607, -8_388_608]);
    // La mise à l'échelle que fait Tune pour `I24` (tune-entree-audio).
    let flottants: Vec<f32> = entiers.iter().map(|v| *v as f32 / 8_388_608.0).collect();
    assert_eq!(flottants[0], 0.5);
    assert_eq!(flottants[1], -0.5);
    assert_eq!(flottants[3], -1.0);
    assert!((flottants[2] - 1.0).abs() < 1.0e-6);
}

#[test]
fn un_tampon_trop_court_n_est_ni_deborde_ni_lu_en_partie() {
    // 2 échantillons et un octet de trop en entrée, place pour un seul en sortie.
    let tasse = [1u8, 0, 0, 2, 0, 0, 9];
    let mut sortie = [0u8; 7];
    assert_eq!(unpack_s24_3(Packed24::Le, &tasse, &mut sortie), 1);
    assert_eq!(
        i32::from_ne_bytes([sortie[0], sortie[1], sortie[2], sortie[3]]),
        1
    );
    assert_eq!(&sortie[4..], &[0, 0, 0]);
}

/// De bout en bout, par l'API publique de CPAL, contre un PCM ALSA privé.
///
/// Le greffon `null` d'alsa-lib accepte tout format, et n'a pas de champ
/// `format` (alsa-lib 1.2.11 : « Unknown field format ») : la cale C le
/// restreint en interceptant `snd_pcm_hw_params_any`, comme le banc #4295.
#[cfg(all(target_os = "linux", feature = "local-audio"))]
mod bout_en_bout {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const ENFANT: &str = "TUNE_8489_CHILD";

    #[test]
    fn un_micro_s24_3le_seul_est_ouvert_en_i24_a_48_khz() {
        if std::env::var(ENFANT).is_err() {
            let dir = tempfile::tempdir().unwrap();
            let cale = dir.path().join("umik_s24_3le.so");
            let source = dir.path().join("umik_s24_3le.c");
            std::fs::write(&source, include_str!("fixtures/alsa_8489/umik_s24_3le.c")).unwrap();
            let sortie = Command::new("cc")
                .args(["-shared", "-fPIC", "-std=c11", "-Werror", "-O1"])
                .arg(&source)
                .args(["-ldl", "-o"])
                .arg(&cale)
                .output()
                .unwrap();
            assert!(
                sortie.status.success(),
                "cale : {}",
                String::from_utf8_lossy(&sortie.stderr)
            );
            let config = dir.path().join("alsa.conf");
            std::fs::write(&config, "pcm.!default { type null }\n").unwrap();
            let mut enfant = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bout_en_bout::un_micro_s24_3le_seul_est_ouvert_en_i24_a_48_khz",
                    "--nocapture",
                ])
                .env(ENFANT, "1")
                .env("ALSA_CONFIG_PATH", config)
                .env("LD_PRELOAD", cale)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let echeance = Instant::now() + Duration::from_secs(8);
            loop {
                if let Some(s) = enfant.try_wait().unwrap() {
                    assert!(s.success(), "capture S24_3LE : {s}");
                    return;
                }
                if Instant::now() >= echeance {
                    enfant.kill().unwrap();
                    enfant.wait().unwrap();
                    panic!("capture S24_3LE : plus de 8 s");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let host = cpal::host_from_id(cpal::HostId::Alsa).unwrap();
        let d = host.default_input_device().expect("entrée ALSA par défaut");
        let configs: Vec<_> = d.supported_input_configs().unwrap().collect();
        // Le PCM privé n'offre que S24_3LE/2 canaux/48 kHz : sans ça le banc
        // ne prouverait rien.
        assert!(
            configs.iter().all(|c| c.channels() == 2
                && c.min_sample_rate() == 48_000
                && c.max_sample_rate() == 48_000),
            "le PCM de test n'est pas contraint : {configs:?}"
        );
        assert_eq!(
            configs
                .iter()
                .map(|c| c.sample_format())
                .collect::<Vec<_>>(),
            vec![cpal::SampleFormat::I24],
            "S24_3LE seul doit s'exposer en I24, rien d'autre : {configs:?}"
        );
        // Ce que Tune demande (`tune-entree-audio`, `format_natif_de`).
        let c = d
            .default_input_config()
            .expect("#8489 : « stream type not supported » sur un micro S24_3LE");
        assert_eq!(c.sample_format(), cpal::SampleFormat::I24);
        assert_eq!(c.channels(), 2);
        assert_eq!(
            c.sample_rate(),
            48_000,
            "jamais 44,1 kHz : refusé par l'UMIK"
        );

        // Lecture : c'est le chemin tassé qui sert le rappel.
        let rappels = Arc::new(AtomicUsize::new(0));
        let r = rappels.clone();
        let flux = d
            .build_input_stream_raw(
                &c.config(),
                c.sample_format(),
                move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
                    let s = data.as_slice::<cpal::I24>().expect("données I24");
                    assert!(!s.is_empty() && s.len() % 2 == 0);
                    assert!(
                        s.iter()
                            .all(|v| (-8_388_608..=8_388_607).contains(&v.inner()))
                    );
                    r.fetch_add(1, Ordering::SeqCst);
                },
                |e| panic!("erreur de capture : {e}"),
                Some(Duration::from_millis(200)),
            )
            .expect("ouverture de la capture S24_3LE");
        flux.play().unwrap();
        let fin = Instant::now() + Duration::from_secs(3);
        while rappels.load(Ordering::SeqCst) < 3 {
            assert!(Instant::now() < fin, "aucune donnée captée");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(flux);

        // Et la sortie ne l'expose pas : elle ne saurait pas tasser.
        let sorties: Vec<_> = d
            .supported_output_configs()
            .map(|it| it.collect())
            .unwrap_or_default();
        assert!(
            sorties
                .iter()
                .all(|c: &cpal::SupportedStreamConfigRange| c.sample_format()
                    != cpal::SampleFormat::I24),
            "I24 exposé en lecture : {sorties:?}"
        );
    }
}
