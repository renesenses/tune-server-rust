//! #5439 — un FLAC servi tel quel par un serveur multimédia part au DAC à la
//! cadence de la SOURCE quand le DAC la sait, comme sur le chemin PCM.
//!
//! Le témoin joue le chemin compressé de `play_url` de bout en bout, moins la
//! carte son : un serveur multimédia factice sert le FLAC en HTTP, le lecteur
//! de production (`LecteurHttpAnnulable`) le lit, `decode_compressed_stream`
//! le décode, la décision de cadence choisit la configuration avec les faits
//! du DAC de Yacine (défaut cpal à 44,1 kHz, `alsa:hw:` qui annonce la cadence
//! de la source), `conformer_la_piste_decodee` met la piste au format ouvert,
//! et un `CaptureOutput` reçoit ce qui serait parti au DAC.
//!
//! Le serveur annonce ou non le format (en-têtes DLNA, type MIME) : aucune de
//! ces annonces n'entre dans la décision — la cadence est celle que le
//! décodage lit.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::Duration;

use super::cadence_du_flux_compresse::{
    choisir_la_config_du_flux_compresse, conformer_la_piste_decodee,
};
use super::periode::config_de_flux;
use super::*;
use crate::outputs::traits::{CaptureOutput, FormatOuvert, PuitsDEchantillons};

const FLAC_96K: &[u8] = include_bytes!("../../../tests/fixtures/flac/ref_24_96000_stereo.flac");
const FLAC_44K: &[u8] = include_bytes!("../../../tests/fixtures/flac/ref_16_44100_stereo.flac");

/// Le PCM de Yacine, tel que cpal le nomme.
const DAC_HW: &str = "alsa:hw:CARD=2,DEV=0";

/// Ce que le serveur multimédia dit du fichier qu'il sert.
#[derive(Clone, Copy, Debug)]
enum Annonce {
    /// `audio/flac` et `contentFeatures.dlna.org`, comme un serveur DLNA.
    Dlna,
    /// Rien : `application/octet-stream`, aucun en-tête DLNA.
    Muette,
}

/// Un serveur multimédia factice : une requête, un FLAC, puis il se ferme.
struct ServeurMultimediaFactice {
    url: String,
    fil: Option<JoinHandle<()>>,
}

impl ServeurMultimediaFactice {
    fn servir(corps: &'static [u8], annonce: Annonce) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/files/Freebox/Musiques/Io%20Capitano.flac",
            listener.local_addr().unwrap()
        );
        let fil = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut requete = Vec::new();
            let mut bloc = [0u8; 1024];
            while !requete.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut bloc) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => requete.extend_from_slice(&bloc[..n]),
                }
            }
            let en_tetes = match annonce {
                Annonce::Dlna => {
                    "Content-Type: audio/flac\r\n\
                     transferMode.dlna.org: Streaming\r\n\
                     contentFeatures.dlna.org: DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000\r\n"
                }
                Annonce::Muette => "Content-Type: application/octet-stream\r\n",
            };
            let entete = format!(
                "HTTP/1.1 200 OK\r\n{en_tetes}Content-Length: {}\r\nConnection: close\r\n\r\n",
                corps.len()
            );
            let _ = socket.write_all(entete.as_bytes());
            let _ = socket.write_all(corps);
        });
        Self {
            url,
            fil: Some(fil),
        }
    }
}

impl Drop for ServeurMultimediaFactice {
    fn drop(&mut self) {
        if let Some(fil) = self.fil.take() {
            let _ = fil.join();
        }
    }
}

/// Le DAC tel que cpal le décrit à la branche compressée.
struct Peripherique {
    /// `default_output_config()` ; sur ALSA, 44 100 Hz dès que la plage le
    /// contient (`cpal-0.17.3/src/host/alsa/mod.rs:634-637`).
    defaut_sr: Option<u32>,
    /// La cadence de la source est-elle dans son énumération ?
    annonce_la_source: bool,
    backend: &'static str,
    endpoint_id: &'static str,
}

const DENAFRIPS: Peripherique = Peripherique {
    defaut_sr: Some(44_100),
    annonce_la_source: true,
    backend: "Alsa",
    endpoint_id: DAC_HW,
};

struct Lecture {
    decision: LocalRateOpening,
    source_sr: u32,
    trames_decodees: u64,
    puits: CaptureOutput,
    /// L'empreinte de la piste décodée elle-même, capturée à sa cadence : ce
    /// que le puits doit avoir reçu quand rien n'est converti.
    empreinte_decodee: u64,
}

/// Le chemin compressé de `play_url`, du serveur au puits.
fn jouer(serveur: &ServeurMultimediaFactice, dac: &Peripherique) -> Lecture {
    let arret = Arc::new(AtomicBool::new(false));
    let mut lecteur = LecteurHttpAnnulable::ouvrir(&serveur.url, arret).expect("ouverture HTTP");
    assert!(lecteur.status().is_success());
    // `play_url` lit l'en-tête, n'y trouve pas de RIFF, puis lit tout le reste.
    let mut octets = Vec::new();
    lecteur.read_to_end(&mut octets).expect("lecture du flux");
    assert!(
        parse_wav_header(&octets).is_none(),
        "un FLAC n'est pas un WAV : c'est bien la branche compressée"
    );

    let (dec_ch, dec_sr, echantillons) = decode_compressed_stream(&octets, &AtomicBool::new(false))
        .expect("le FLAC se décode")
        .expect("aucun arrêt n'a été demandé");

    let defaut = dac.defaut_sr.map(|sr| config_de_flux(dec_ch, sr));
    let enumeree = if dac.defaut_sr == Some(dec_sr) || !dac.annonce_la_source {
        None
    } else {
        Some(config_de_flux(dec_ch, dec_sr))
    };
    let preuve = sample_rate_evidence_for_device(dac.backend, dac.endpoint_id, true);
    let (cfg, decision) =
        choisir_la_config_du_flux_compresse(dec_sr, defaut, enumeree, preuve, || {
            config_de_flux(dec_ch, dec_sr)
        });

    let mut temoin = CaptureOutput::ouvert(FormatOuvert::new(dec_sr, dec_ch));
    temoin.ecrire(&echantillons);
    let trames_decodees = temoin.trames();

    let sortie = FormatOuvert::new(cfg.sample_rate, cfg.channels);
    let conforme = conformer_la_piste_decodee(echantillons, dec_sr, dec_ch, sortie, None, None);
    let mut puits = CaptureOutput::ouvert(sortie);
    assert!(puits.ecrire(&conforme));

    Lecture {
        decision,
        source_sr: dec_sr,
        trames_decodees,
        puits,
        empreinte_decodee: temoin.empreinte(),
    }
}

#[test]
fn chemin_compresse_5439_flac_haute_cadence_sur_hw_part_a_la_cadence_source() {
    for annonce in [Annonce::Dlna, Annonce::Muette] {
        let serveur = ServeurMultimediaFactice::servir(FLAC_96K, annonce);
        let lecture = jouer(&serveur, &DENAFRIPS);

        assert_eq!(lecture.source_sr, 96_000, "la cadence vient du décodage");
        assert_eq!(
            lecture.puits.format().cadence,
            96_000,
            "serveur {annonce:?} : un FLAC 96 kHz sur `{DAC_HW}`, qui annonce \
             96 kHz, part au DAC à la cadence par défaut de cpal ({:?}) au lieu \
             de la cadence de la source — la branche compressée rééchantillonne \
             ce que le chemin PCM ouvre tel quel (#5439)",
            DENAFRIPS.defaut_sr
        );
        assert_eq!(lecture.decision, LocalRateOpening::AtSourceRateMeasured);
        assert_eq!(
            lecture.puits.trames(),
            lecture.trames_decodees,
            "aucune trame ajoutée ni perdue : rien n'a été converti"
        );
        assert_eq!(
            lecture.puits.empreinte(),
            lecture.empreinte_decodee,
            "le puits reçoit la piste décodée, mot pour mot"
        );
    }
}

/// Un PCM ALSA qui n'est pas le matériel (`dmix:`, `plughw:`) dit oui à
/// tout : sa liste ne prouve rien, la conversion vers la cadence par défaut
/// reste — et elle est désormais nommée avec sa raison.
#[test]
fn chemin_compresse_5439_un_greffon_alsa_convertit_toujours() {
    let dmix = Peripherique {
        endpoint_id: "alsa:dmix:CARD=2,DEV=0",
        ..DENAFRIPS
    };
    let serveur = ServeurMultimediaFactice::servir(FLAC_96K, Annonce::Dlna);
    let lecture = jouer(&serveur, &dmix);

    assert_eq!(
        lecture.decision,
        LocalRateOpening::ResampleToDeviceRate {
            device_sample_rate: 44_100,
            reason: LocalRateFallback::CapabilitiesUnverified,
        }
    );
    assert_eq!(lecture.puits.format().cadence, 44_100);
    let attendu = (lecture.trames_decodees as f64 * 44_100.0 / 96_000.0).round() as u64;
    assert_eq!(
        lecture.puits.trames(),
        attendu,
        "rééchantillonnage de la piste entière, sans délai de groupe (#2246)"
    );
}

/// Le DAC n'annonce pas la cadence de la source : conversion vers son défaut.
#[test]
fn chemin_compresse_5439_cadence_non_annoncee_par_le_dac_convertit() {
    let sans_96k = Peripherique {
        annonce_la_source: false,
        ..DENAFRIPS
    };
    let serveur = ServeurMultimediaFactice::servir(FLAC_96K, Annonce::Muette);
    let lecture = jouer(&serveur, &sans_96k);
    assert_eq!(
        lecture.decision,
        LocalRateOpening::ResampleToDeviceRate {
            device_sample_rate: 44_100,
            reason: LocalRateFallback::RateNotSupported,
        }
    );
    assert_eq!(lecture.puits.format().cadence, 44_100);
}

/// Source à 44,1 kHz sur un défaut à 44,1 kHz : rien ne change.
#[test]
fn chemin_compresse_5439_source_a_la_cadence_par_defaut_inchangee() {
    let serveur = ServeurMultimediaFactice::servir(FLAC_44K, Annonce::Dlna);
    let lecture = jouer(&serveur, &DENAFRIPS);
    assert_eq!(
        lecture.decision,
        LocalRateOpening::DeviceAlreadyAtSourceRate
    );
    assert_eq!(lecture.puits.format().cadence, 44_100);
    assert_eq!(lecture.puits.empreinte(), lecture.empreinte_decodee);
}

/// Sonde du défaut en échec (`None`) : le dernier recours d'avant #5439.
#[test]
fn chemin_compresse_5439_sans_cadence_par_defaut_dernier_recours() {
    let muet = Peripherique {
        defaut_sr: None,
        ..DENAFRIPS
    };
    let serveur = ServeurMultimediaFactice::servir(FLAC_96K, Annonce::Muette);
    let lecture = jouer(&serveur, &muet);
    // Énumération mesurée et positive : la décision ouvre à la source.
    assert_eq!(lecture.decision, LocalRateOpening::AtSourceRateMeasured);
    assert_eq!(lecture.puits.format().cadence, 96_000);

    let (cfg, decision) = choisir_la_config_du_flux_compresse(
        96_000,
        None,
        None,
        SampleRateEvidence::Measured,
        || config_de_flux(2, 88_200),
    );
    assert_eq!(decision, LocalRateOpening::LastResortSourceRate);
    assert_eq!(cfg.sample_rate, 88_200, "le dernier recours de l'appelant");
}

// ---------------------------------------------------------------------------
// Garde de site : `play_url` appelle-t-il ces fonctions ?
// ---------------------------------------------------------------------------

/// Les épreuves ci-dessus jouent les fonctions ; aucune ne voit que la branche
/// compressée de `play_url` les APPELLE. `local.rs` est lu sans ses modules
/// d'épreuves ; les définitions vivent dans `cadence_du_flux_compresse.rs`,
/// qui n'est pas lu : chaque occurrence comptée est un appel. Les aiguilles
/// sont assemblées à l'exécution pour ne pas se trouver dans ce fichier.
#[test]
fn chemin_compresse_5439_play_url_appelle_la_decision_et_la_mise_au_format() {
    const TOUT: &str = include_str!("../local.rs");
    let fin = TOUT
        .find("mod relache_peripherique_i3575")
        .expect("module d'épreuves renommé : la découpe ne protège plus rien");
    let production: String = TOUT[..fin].chars().filter(|c| !c.is_whitespace()).collect();

    for (aiguille, cause) in [
        (
            [
                "choisir_la_config",
                "_du_flux_compresse(dec_sr,default_cfg,enumeree,preuve,",
            ]
            .concat(),
            "la branche compressée ne passe plus par la décision de cadence du \
             chemin PCM : elle reprend le défaut de cpal (44,1 kHz sur ALSA) et \
             rééchantillonne un FLAC que le DAC sait ouvrir tel quel (#5439)",
        ),
        (
            ["conformer_la_piste", "_decodee(samples,dec_sr,dec_ch,"].concat(),
            "la branche compressée ne met plus la piste au format ouvert par \
             la fonction que joue le témoin",
        ),
    ] {
        assert_eq!(production.matches(&aiguille).count(), 1, "{cause}");
    }
    // Le retour en arrière exact : le défaut cpal pris tel quel.
    let retour = ["ifdefault_sr==Some(dec_sr){", "default_cfg.unwrap()"].concat();
    assert!(
        !production.contains(&retour),
        "la branche compressée reprend `default_output_config()` sans décision (#5439)"
    );
}
