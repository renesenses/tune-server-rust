//! Fil 1915 (Reivax66, Windows, sortie locale WASAPI) — **une erreur de
//! lecture loin de la fin n'est pas une fin de piste.**
//!
//! Le journal du testeur : Mingus, « R and R », 11:52. À 7:51, le corps HTTP
//! de la piste enchaînée rend `error decoding response body`
//! (`local_audio_gapless_read_error`), la boucle producteur le prend pour une
//! fin de flux, la sortie annonce une fin naturelle
//! (`local_audio_track_ended_naturally_post_drain`), et le sondeur l'accepte
//! (`wall_elapsed` ≥ 50 % de la durée) : `track_end_gap … peak_pos=470714
//! track_dur=711666`, `queue_ended`. Un tiers de la piste perdu, sans un mot.
//!
//! Ce que ces témoins gardent :
//! - une erreur à 66 % de la piste rend `Interrompue` et POSE un constat
//!   préfixé « piste tronquée » (le sondeur passe à la suivante), pour les deux
//!   rôles de la boucle ;
//! - une erreur de fin de corps à 99,9 % reste une fin de flux — la
//!   non-régression de #1254 (PR #1076 : MP3 dont la fin de corps lève une
//!   erreur alors que tout l'audio est là) ;
//! - durée inconnue (radio, bras exclusifs) : comportement historique.
//!
//! La cause AMONT de la coupure (durée en base fausse, producteur arrêté trop
//! tôt) n'est pas établie ; ces témoins ne la supposent pas.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;

use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::{BoucleProducteur, CompteursDePiste, FinDeBoucle, RoleDeLaBoucle};
use crate::outputs::traits::{CaptureOutput, FormatOuvert};
use crate::poller::decisions::position_loin_de_la_fin;

/// La durée en base de la piste du journal.
const DUREE_R_AND_R_MS: u64 = 711_666;

/// Un corps HTTP qui livre `octets` de PCM puis rend l'erreur de reqwest.
struct CorpsQuiCasse {
    reste: usize,
}

impl Read for CorpsQuiCasse {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if self.reste == 0 {
            return Err(io::Error::other("error decoding response body"));
        }
        let n = destination.len().min(self.reste);
        // Une rampe non nulle : rien ne doit ressembler à un décodage mort.
        for (i, octet) in destination[..n].iter_mut().enumerate() {
            *octet = (i % 251) as u8 + 1;
        }
        self.reste -= n;
        Ok(n)
    }
}

/// Une seconde de PCM 16 bits stéréo à 44,1 kHz.
const UNE_SECONDE_16_BITS_STEREO: usize = 44_100 * 2 * 2;

struct Issue {
    fin: FinDeBoucle,
    constat: Option<String>,
    mots: u64,
}

/// Joue une seconde d'audio à partir de `depart_ms`, puis casse le corps.
///
/// `depart_ms` passe par `seek_offset`, qui entre dans la position publiée
/// exactement comme dans `play_url` : cela évite de pousser 470 s de PCM
/// pour atteindre la position du journal.
fn jouer_puis_casser(role: RoleDeLaBoucle, depart_ms: u64, duree_ms: u64) -> Issue {
    let arret = AtomicBool::new(false);
    let disparu = AtomicBool::new(false);
    let position = AtomicU64::new(0);
    let duree = AtomicU64::new(duree_ms);
    let constat = std::sync::Mutex::new(None);
    let (_tx, rx) = mpsc::channel();
    let producteur = BoucleProducteur {
        role,
        backend: "capture",
        device_name: "Smart DX1",
        cle_de_flux: Some("d7cdd778-d548-4487-8f51-9c44471f097a"),
        stop_rx: &rx,
        force_silent: &arret,
        device_gone: &disparu,
        position_ms: &position,
        open_failure: &constat,
        debut_du_flux: std::time::Instant::now(),
        duree_de_la_piste_ms: &duree,
    };
    let dsp = DspAuRepos::neuf();
    let mut conversion = etage(&dsp, Vec::new(), 44_100, 2, 16, 44_100, 2);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(44_100, 2));
    let mut compteurs = CompteursDePiste {
        total_bytes_read: 0,
        total_frames_fed: 0,
        seek_offset: depart_ms,
        skip_bytes: 0,
        skipped_bytes: 0,
        premiere_donnee_journalisee: true,
    };
    let fin = producteur.tourner(
        &mut CorpsQuiCasse {
            reste: UNE_SECONDE_16_BITS_STEREO,
        },
        &mut [0; 4096],
        &mut conversion,
        &mut puits,
        &mut |_, _, _| false,
        &mut compteurs,
        &mut |_| true,
    );
    Issue {
        fin,
        constat: constat.lock().unwrap().take(),
        mots: puits.mots(),
    }
}

const LES_DEUX_ROLES: [RoleDeLaBoucle; 2] = [
    RoleDeLaBoucle::PisteInitiale,
    RoleDeLaBoucle::PisteEnchainee,
];

fn nom(role: RoleDeLaBoucle) -> &'static str {
    match role {
        RoleDeLaBoucle::PisteInitiale => "piste initiale",
        RoleDeLaBoucle::PisteEnchainee => "piste enchaînée (gapless)",
    }
}

#[test]
fn f1915_une_coupure_a_66_pour_cent_n_est_pas_une_fin_naturelle() {
    for role in LES_DEUX_ROLES {
        // 469,7 s + 1 s jouée = 470,7 s : le `peak_pos=470714` du journal.
        let issue = jouer_puis_casser(role, 469_700, DUREE_R_AND_R_MS);
        assert!(
            issue.mots > 0,
            "{} : la seconde d'audio livrée doit avoir été poussée",
            nom(role)
        );
        assert!(
            matches!(issue.fin, FinDeBoucle::Interrompue),
            "{} : une erreur de corps à 7:50 d'une piste de 11:51 a été prise pour une \
             fin de flux — la file enchaînerait en silence sur une piste tronquée (fil 1915)",
            nom(role)
        );
        let constat = issue.constat.unwrap_or_else(|| {
            panic!(
                "{} : coupure sans constat — rien n'atteindrait l'écran",
                nom(role)
            )
        });
        assert!(
            crate::poller::decisions::constat_de_piste_tronquee(&constat).is_some(),
            "{} : sans son préfixe, le sondeur prendrait la coupure pour une panne de \
             sortie et arrêterait la zone au lieu de passer à la suivante : {constat}",
            nom(role)
        );
        assert!(
            constat.contains("Smart DX1") && constat.contains("7:50") && constat.contains("11:51"),
            "{} : le constat doit nommer la sortie, la position et la durée : {constat}",
            nom(role)
        );
    }
}

#[test]
fn f1915_une_erreur_de_fin_de_corps_a_99_9_pour_cent_reste_une_fin_1254() {
    for role in LES_DEUX_ROLES {
        // 709,9 s + 1 s = 710,9 s sur 711,666 s : 99,9 %.
        let issue = jouer_puis_casser(role, 709_900, DUREE_R_AND_R_MS);
        assert!(
            matches!(issue.fin, FinDeBoucle::FinDeFlux),
            "{} : l'erreur de fin de corps au bout de la piste (MP3, #1254) doit rester \
             une fin de flux, sinon l'album s'arrête après la piste",
            nom(role)
        );
        assert_eq!(issue.constat, None, "{} : aucun constat au bout", nom(role));
    }
}

#[test]
fn f1915_un_debordement_de_la_duree_annoncee_reste_une_fin_1254() {
    // Le cas exact de #1254 : le décodage MP3 produit PLUS que la durée
    // annoncée, puis la fin de corps lève une erreur.
    let issue = jouer_puis_casser(RoleDeLaBoucle::PisteEnchainee, 715_000, DUREE_R_AND_R_MS);
    assert!(matches!(issue.fin, FinDeBoucle::FinDeFlux));
    assert_eq!(issue.constat, None);
}

#[test]
fn f1915_duree_inconnue_garde_la_fin_historique() {
    for role in LES_DEUX_ROLES {
        let issue = jouer_puis_casser(role, 469_700, 0);
        assert!(
            matches!(issue.fin, FinDeBoucle::FinDeFlux),
            "{} : sans durée, on ne sait pas où est la fin — comportement d'avant",
            nom(role)
        );
        assert_eq!(issue.constat, None);
    }
}

#[test]
fn f1915_le_seuil_de_coupure() {
    // Le journal du fil 1915 : 66 %, 241 s manquantes.
    assert!(position_loin_de_la_fin(470_714, 711_666));
    // Au bout, ou au-delà (remplissage MP3, #1254).
    assert!(!position_loin_de_la_fin(711_000, 711_666));
    assert!(!position_loin_de_la_fin(720_000, 711_666));
    // Durée inconnue : jamais loin.
    assert!(!position_loin_de_la_fin(1_000, 0));
    // Durée en base fausse de quelques secondes : 97,8 %, 4 s manquantes.
    assert!(!position_loin_de_la_fin(176_000, 180_000));
    // Longue piste, 96,8 % : 20 s manquantes mais au-dessus de 95 %.
    assert!(!position_loin_de_la_fin(600_000, 620_000));
    // Les DEUX conditions : 94,4 % ET 10 s manquantes ⇒ coupure.
    assert!(position_loin_de_la_fin(170_000, 180_000));
    // Piste courte : 50 % mais seulement 4 s manquantes ⇒ pas une coupure.
    assert!(!position_loin_de_la_fin(4_000, 8_000));
}
