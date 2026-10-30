//! Témoins de la sortie Devialet AIR (#6542).
//!
//! Trois étages, du plus sûr au plus large :
//!
//! 1. **Le protocole, sur NOS vecteurs** (`vecteurs_tune.json`, produits par
//!    `scripts/devialet-air-vecteurs.py`, implémentation indépendante qui
//!    reproduit octet pour octet les paquets de Roon capturés par Rémi).
//! 2. **L'émetteur contre le simulateur d'ampli** : cadence, pré-remplissage,
//!    retours, retransmissions, délais.
//! 3. **La sortie Tune entière** : option, commandes, plafond de volume,
//!    allumage, source, lecture d'un vrai fichier, pause, rééchantillonnage.
//!
//! Aucun de ces témoins ne remplace un essai sur un vrai Expert.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::ecoute::Ecoute;
use super::emetteur::{
    Cadence, EtatFlux, FinDeFlux, Garde, LotDeTrames, ParametresFlux, diffuser, trames_arrondies,
};
use super::fournisseur::FournisseurDevialetAir;
use super::protocole::*;
use super::reglages;
use super::simulateur::{SESSION_DE_TEST, Simulateur, annonce_de_test};
use super::sortie::{ContexteAir, DevialetAirOutput, Ports, identifiant_de_sortie};
use crate::outputs::traits::{
    OutputProvider, OutputTarget, PlayMedia, ProviderContext, TransportState,
};

const VECTEURS: &str = include_str!("vecteurs_tune.json");

fn vecteurs() -> Value {
    serde_json::from_str(VECTEURS).expect("vecteurs_tune.json lisible")
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn session(v: &Value) -> [u8; 4] {
    hex(v.as_str().unwrap()).try_into().unwrap()
}

fn trames(v: &Value) -> Vec<[i32; 2]> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| [t[0].as_i64().unwrap() as i32, t[1].as_i64().unwrap() as i32])
        .collect()
}

// ─── 1. Protocole ───────────────────────────────────────────────────────────

#[test]
fn air_crc_rend_la_valeur_de_controle_du_catalogue() {
    let v = vecteurs();
    let entree = v["crc_catalogue"]["entree"].as_str().unwrap();
    assert_eq!(
        u64::from(crc16(entree.as_bytes())),
        v["crc_catalogue"]["crc"].as_u64().unwrap(),
        "CRC-16/CCITT-FALSE : poly 0x1021, init 0xFFFF, sans réflexion"
    );
}

#[test]
fn air_les_paquets_audio_sont_reproduits_octet_pour_octet() {
    let v = vecteurs();
    let cas = v["dm_audio"].as_array().unwrap();
    assert_eq!(cas.len(), 16);
    for c in cas {
        let entete = EnteteAudio {
            session: session(&c["session"]),
            flux: c["flux"].as_u64().unwrap() as u32,
            frequence: c["frequence"].as_u64().unwrap() as u32,
            lecture: c["lecture"].as_bool().unwrap(),
            position: c["position"].as_u64().unwrap(),
        };
        let attendu = hex(c["paquet_hex"].as_str().unwrap());
        let obtenu = paquet_audio(&entete, &trames(&c["trames"]));
        assert!(
            obtenu == attendu,
            "« {} » : paquet différent du vecteur (taille {} au lieu de {})",
            c["nom"],
            obtenu.len(),
            attendu.len()
        );
    }
}

#[test]
fn air_le_decodage_rend_l_entree_sans_perte_jusqu_a_96_khz_et_a_moins_d_un_pas_au_dela() {
    let v = vecteurs();
    for c in v["dm_audio"].as_array().unwrap() {
        let entree = trames(&c["trames"]);
        let PaquetLu::Audio {
            entete,
            trames: sortie,
            decalages,
        } = analyser_paquet(&hex(c["paquet_hex"].as_str().unwrap())).unwrap()
        else {
            panic!("{} : pas un paquet audio", c["nom"]);
        };
        assert_eq!(entete.position, c["position"].as_u64().unwrap());
        assert_eq!(entete.lecture, c["lecture"].as_bool().unwrap());
        if c["avec_perte"].as_bool().unwrap() {
            assert!(
                decalages.iter().any(|&s| s > 0),
                "{} : s > 0 attendu",
                c["nom"]
            );
            for (canal, &s) in decalages.iter().enumerate() {
                for (e, d) in entree.iter().zip(&sortie) {
                    let ecart = (i64::from(e[canal]) - i64::from(d[canal])).abs();
                    assert!(ecart < 1 << s, "{} : écart {ecart} ≥ 2^{s}", c["nom"]);
                }
            }
        } else {
            assert_eq!(decalages, [0, 0], "{} : sans perte, s = 0", c["nom"]);
            assert_eq!(sortie, entree, "{} : décodage inexact", c["nom"]);
        }
    }
}

#[test]
fn air_paquet_d_arret_conforme() {
    let v = vecteurs();
    for c in v["dm_arret"].as_array().unwrap() {
        let s = session(&c["session"]);
        let flux = c["flux"].as_u64().unwrap() as u32;
        let o = paquet_arret(s, flux);
        assert_eq!(o, hex(c["paquet_hex"].as_str().unwrap()));
        assert_eq!(o.len(), 152);
        assert_eq!(
            analyser_paquet(&o),
            Ok(PaquetLu::Arret { session: s, flux })
        );
    }
}

#[test]
fn air_commandes_conformes_et_relues() {
    let v = vecteurs();
    for c in v["dr_commande"].as_array().unwrap() {
        let pkt = c["compteur_paquet"].as_u64().unwrap() as u16;
        let cmd = c["compteur_commande"].as_u64().unwrap() as u16;
        let op = c["opcode"].as_u64().unwrap() as u16;
        let val = c["valeur"].as_f64().unwrap() as f32;
        let o = commande(pkt, cmd, op, val);
        assert_eq!(
            o,
            hex(c["paquet_hex"].as_str().unwrap()),
            "opcode {op:#06x}"
        );
        let lue = analyser_commande(&o).unwrap();
        assert_eq!(
            (lue.opcode, lue.valeur, lue.compteur_paquet),
            (op, val, pkt)
        );
    }
}

#[test]
fn air_annonce_analysee_champ_par_champ_et_reconstruite_a_l_identique() {
    let v = vecteurs();
    for c in v["da_annonce"].as_array().unwrap() {
        let o = hex(c["paquet_hex"].as_str().unwrap());
        let a = analyser_annonce(&o).unwrap();
        let e = &c["attendu"];
        assert_eq!(a.mac_texte(), e["mac"].as_str().unwrap());
        assert_eq!(a.nom, e["nom"].as_str().unwrap());
        assert_eq!(a.allume, e["allume"].as_bool().unwrap());
        assert_eq!(a.tonalite_active, e["tonalite_active"].as_bool().unwrap());
        assert_eq!(a.aucun_flux_air, e["aucun_flux_air"].as_bool().unwrap());
        assert_eq!(
            u64::from(a.source_choisie),
            e["source_choisie"].as_u64().unwrap()
        );
        assert_eq!(a.sourdine, e["sourdine"].as_bool().unwrap());
        assert_eq!(a.volume_db(), e["volume_db"].as_f64().unwrap());
        assert_eq!(
            i64::from(a.aigus_demi_db),
            e["aigus_demi_db"].as_i64().unwrap()
        );
        assert_eq!(
            i64::from(a.graves_demi_db),
            e["graves_demi_db"].as_i64().unwrap()
        );
        assert_eq!(
            i64::from(a.balance_pour_cent),
            e["balance_pour_cent"].as_i64().unwrap()
        );
        assert_eq!(u64::from(a.pre_out), e["pre_out"].as_u64().unwrap());
        assert_eq!(
            a.frequence_entree.map(u64::from),
            e["frequence_entree"].as_u64()
        );
        assert_eq!(u64::from(a.horloge_ms), e["horloge_ms"].as_u64().unwrap());
        assert_eq!(a.session, session(&e["session"]));
        assert_eq!(
            u64::from(a.flux_en_cours),
            e["flux_en_cours"].as_u64().unwrap()
        );
        let actives: Vec<u64> = a
            .sources
            .iter()
            .filter(|s| s.active())
            .map(|s| u64::from(s.indice))
            .collect();
        let attendues: Vec<u64> = e["sources_actives"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_u64().unwrap())
            .collect();
        assert_eq!(actives, attendues);
        assert_eq!(a.sources[9].nom, e["source_9"].as_str().unwrap());
        assert_eq!(annonce(&a), o, "reconstruction de l'annonce");
    }
}

#[test]
fn air_retours_analyses_et_plages_manquantes_converties_en_positions() {
    let v = vecteurs();
    for c in v["df_retour"].as_array().unwrap() {
        let o = hex(c["paquet_hex"].as_str().unwrap());
        let r = analyser_retour(&o).unwrap();
        assert_eq!(r.position_jouee, c["position_jouee"].as_u64().unwrap());
        assert_eq!(
            r.derniere_trame_recue,
            c["derniere_trame_recue"].as_u64().unwrap()
        );
        assert_eq!(
            r.dernier_paquet_recu,
            c["dernier_paquet_recu"].as_u64().unwrap()
        );
        let attendues: Vec<u64> = c["positions_reclamees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_u64().unwrap())
            .collect();
        assert_eq!(r.positions_reclamees(), attendues);
        assert_eq!(retour(&r), o, "reconstruction du retour");
    }
}

#[test]
fn air_un_octet_altere_est_refuse_par_le_crc() {
    let v = vecteurs();
    let mut da = hex(v["da_annonce"][0]["paquet_hex"].as_str().unwrap());
    da[310] ^= 1;
    assert_eq!(analyser_annonce(&da), Err(Refus::CrcFaux));
    let mut dm = hex(v["dm_audio"][7]["paquet_hex"].as_str().unwrap());
    dm[40] ^= 0x10;
    assert_eq!(analyser_paquet(&dm), Err(Refus::CrcFaux));
    let mut dr = commande(1, 0, opcode::VOLUME, -30.0);
    dr[9] ^= 1;
    assert_eq!(analyser_commande(&dr), Err(Refus::CrcFaux));
    let mut df = hex(v["df_retour"][1]["paquet_hex"].as_str().unwrap());
    df[30] ^= 1;
    assert_eq!(analyser_retour(&df), Err(Refus::CrcFaux));
}

/// Générateur déterministe pour les allers-retours.
fn lcg(graine: &mut u64) -> i32 {
    *graine = graine
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    // 32 bits signés, ramenés à ±2^23 : la plage d'un échantillon 24 bits.
    ((*graine >> 32) as u32 as i32) >> 8
}

#[test]
fn air_aller_retour_aleatoire_identite_jusqu_a_96_khz() {
    let mut g = 6542u64;
    for &f in &[44_100, 48_000, 88_200, 96_000] {
        for amplitude in [1, 8, 15, 23] {
            let t: Vec<[i32; 2]> = (0..TRAMES_PAR_PAQUET)
                .map(|_| {
                    [
                        lcg(&mut g) >> (23 - amplitude),
                        lcg(&mut g) >> (23 - amplitude),
                    ]
                })
                .collect();
            let e = EnteteAudio {
                session: SESSION_DE_TEST,
                flux: 7,
                frequence: f,
                lecture: true,
                position: 4_000,
            };
            let PaquetLu::Audio { trames: d, .. } = analyser_paquet(&paquet_audio(&e, &t)).unwrap()
            else {
                panic!()
            };
            assert_eq!(d, t, "{f} Hz, amplitude 2^{amplitude}");
        }
    }
}

#[test]
fn air_au_dela_de_96_khz_la_largeur_ne_depasse_jamais_10_bits() {
    let mut g = 1u64;
    for &f in &[176_400, 192_000] {
        let t: Vec<[i32; 2]> = (0..TRAMES_PAR_PAQUET)
            .map(|_| [lcg(&mut g), lcg(&mut g)])
            .collect();
        let e = EnteteAudio {
            session: SESSION_DE_TEST,
            flux: 7,
            frequence: f,
            lecture: true,
            position: 0,
        };
        let o = paquet_audio(&e, &t);
        // Bloc gauche : octet 22 + 5 = b.
        assert!(o[27] <= LARGEUR_MAX_HAUTE_FREQUENCE, "b = {}", o[27]);
        let PaquetLu::Audio {
            trames: d,
            decalages,
            ..
        } = analyser_paquet(&o).unwrap()
        else {
            panic!()
        };
        for c in 0..2 {
            for (x, y) in t.iter().zip(&d) {
                assert!((i64::from(x[c]) - i64::from(y[c])).abs() < 1 << decalages[c]);
            }
        }
    }
}

// ─── Réglages et politiques ─────────────────────────────────────────────────

fn base_de_test() -> Arc<dyn crate::db::backend::DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

#[test]
fn air_option_desactivee_par_defaut_et_source_9_par_defaut() {
    let db = base_de_test();
    assert!(!reglages::activee(&db), "désactivée par défaut");
    assert_eq!(reglages::indice_source(&db), 9);
    let r = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    r.set(reglages::CLE_ACTIVEE, "true").unwrap();
    r.set(reglages::CLE_INDICE_SOURCE, "13").unwrap();
    assert!(reglages::activee(&db));
    assert_eq!(reglages::indice_source(&db), 13);
    r.set(reglages::CLE_INDICE_SOURCE, "15").unwrap();
    assert_eq!(reglages::indice_source(&db), 9, "hors bornes : défaut");
    r.set(reglages::CLE_INDICE_SOURCE, "abc").unwrap();
    assert_eq!(reglages::indice_source(&db), 9, "illisible : défaut");
}

#[test]
fn air_au_dela_de_96_khz_la_cadence_envoyee_est_96_khz() {
    for (source, envoyee) in [
        (44_100, 44_100),
        (48_000, 48_000),
        (88_200, 88_200),
        (96_000, 96_000),
        (176_400, 96_000),
        (192_000, 96_000),
        (352_800, 96_000),
        (384_000, 96_000),
        (22_050, 44_100),
        (32_000, 48_000),
    ] {
        assert_eq!(reglages::cadence_envoyee(source), envoyee, "{source} Hz");
    }
}

#[test]
fn air_volume_plafonne_a_moins_10_db_sur_la_grille_de_0_5_db() {
    assert_eq!(reglages::volume_vers_db(1.0), -10.0);
    assert_eq!(
        reglages::volume_vers_db(7.0),
        -10.0,
        "jamais au-dessus du plafond"
    );
    assert_eq!(reglages::volume_vers_db(0.0), -80.0);
    assert_eq!(reglages::volume_vers_db(f64::NAN), -80.0);
    assert_eq!(reglages::volume_vers_db(0.5), -45.0);
    for i in 0..=1000 {
        let db = reglages::volume_vers_db(f64::from(i) / 1000.0);
        assert!(db <= reglages::PLAFOND_DB);
        assert_eq!((db * 2.0).fract(), 0.0, "{db} hors grille");
    }
    assert_eq!(reglages::db_vers_volume(-45.0), 0.5);
    assert_eq!(
        reglages::db_vers_volume(0.0),
        1.0,
        "réglé plus fort à la télécommande"
    );
}

#[test]
fn air_durees_arrondies_au_paquet_comme_roon() {
    assert_eq!(trames_arrondies(44_100, Duration::from_millis(500)), 22_000);
    assert_eq!(trames_arrondies(88_200, Duration::from_millis(500)), 44_000);
    assert_eq!(trames_arrondies(48_000, Duration::from_millis(500)), 24_000);
    assert_eq!(trames_arrondies(44_100, Duration::from_secs(2)), 88_200);
    assert_eq!(trames_arrondies(96_000, Duration::from_secs(2)), 192_000);
}

// ─── 2. Émetteur contre le simulateur ───────────────────────────────────────

/// Les durées du cycle de vie, raccourcies d'un facteur 10 environ.
const CADENCE_DE_TEST: Cadence = Cadence {
    silence_de_tete: Duration::from_millis(50),
    pre_remplissage: Duration::from_millis(200),
    avance_cible: Duration::from_millis(250),
    periode: Duration::from_millis(10),
    silence_de_queue: Duration::from_millis(50),
    delai_premier_retour: Duration::from_millis(500),
    delai_retour_perdu: Duration::from_secs(1),
    intervalle_de_renvoi: Duration::from_millis(20),
    vidage_max: Duration::from_secs(2),
    attente_allumage: Duration::from_millis(800),
    attente_source: Duration::from_millis(500),
};

fn sinus(f: u32, n: usize) -> Vec<[i32; 2]> {
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(f);
            [
                (4_000_000.0 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()) as i32,
                (2_000_000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i32,
            ]
        })
        .collect()
}

fn source_de(trames: Vec<[i32; 2]>) -> mpsc::Receiver<LotDeTrames> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        for lot in trames.chunks(1_000) {
            if tx.send(Ok(lot.to_vec())).await.is_err() {
                return;
            }
        }
    });
    rx
}

fn sans_garde() -> Garde {
    Arc::new(|_: &EtatFlux| Ok(()))
}

async fn lancer_un_flux(
    sim: &Simulateur,
    session: [u8; 4],
    trames: Vec<[i32; 2]>,
    frequence: u32,
) -> (FinDeFlux, EtatFlux) {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let etat = Arc::new(Mutex::new(EtatFlux::default()));
    let p = ParametresFlux {
        session,
        flux: 0x0042,
        frequence,
        cadence: CADENCE_DE_TEST,
    };
    let fin = tokio::time::timeout(
        Duration::from_secs(10),
        diffuser(
            socket,
            sim.adresse,
            p,
            source_de(trames),
            etat.clone(),
            sans_garde(),
        ),
    )
    .await
    .expect("le flux doit se terminer");
    let e = etat.lock().unwrap().clone();
    (fin, e)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_flux_complet_recu_a_l_echantillon_pres_avec_pre_remplissage() {
    let sim = Simulateur::demarrer(annonce_de_test(), None).await;
    let signal = sinus(44_100, 22_050);
    let (fin, etat) = lancer_un_flux(&sim, SESSION_DE_TEST, signal.clone(), 44_100).await;
    assert_eq!(fin, FinDeFlux::Naturelle);
    let s = sim.etat();
    assert!(s.refus.is_empty(), "paquets refusés : {:?}", s.refus);
    let pcm = s.pcm(0x0042).expect("aucun trou dans le flux");
    let tete = trames_arrondies(44_100, CADENCE_DE_TEST.silence_de_tete) as usize;
    assert_eq!(etat.silence_de_tete, tete as u64);
    assert!(pcm[..tete].iter().all(|t| *t == [0, 0]), "silence de tête");
    assert_eq!(&pcm[tete..tete + signal.len()], &signal[..], "signal exact");
    assert!(
        pcm[tete + signal.len()..].iter().all(|t| *t == [0, 0]),
        "silence de queue"
    );
    assert_eq!(pcm.len() % TRAMES_PAR_PAQUET, 0);
    let seuil = trames_arrondies(44_100, CADENCE_DE_TEST.pre_remplissage);
    for (&pos, (entete, _, _)) in &s.paquets[&0x0042] {
        assert_eq!(
            entete.lecture,
            pos >= seuil,
            "bit lecture à la position {pos}"
        );
        assert_eq!(entete.session, SESSION_DE_TEST);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_les_paquets_perdus_sont_renvoyes_a_l_identique() {
    let sim = Simulateur::demarrer(annonce_de_test(), None).await;
    {
        let mut s = sim.etat();
        for k in [20u64, 21, 22, 40, 61, 75] {
            s.perdre_une_fois.insert(k * 200);
        }
    }
    let signal = sinus(48_000, 24_000);
    let (fin, etat) = lancer_un_flux(&sim, SESSION_DE_TEST, signal.clone(), 48_000).await;
    assert_eq!(fin, FinDeFlux::Naturelle);
    let s = sim.etat();
    assert_eq!(s.perdus.len(), 6, "les six pertes ont eu lieu");
    assert!(
        etat.paquets_renvoyes >= 6,
        "renvois : {}",
        etat.paquets_renvoyes
    );
    let pcm = s.pcm(0x0042).expect("les trous ont été comblés");
    let tete = etat.silence_de_tete as usize;
    assert_eq!(&pcm[tete..tete + signal.len()], &signal[..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_sans_retour_l_emetteur_s_arrete_et_le_dit() {
    let sim = Simulateur::demarrer(annonce_de_test(), None).await;
    sim.etat().muet = true;
    let (fin, _) = lancer_un_flux(&sim, SESSION_DE_TEST, sinus(44_100, 44_100), 44_100).await;
    match fin {
        FinDeFlux::Interrompu { motif, .. } => {
            assert!(motif.contains("aucun retour"), "{motif}");
            assert!(
                motif.contains("aucun datagramme reçu sur le port local"),
                "rien n'est arrivé : le motif doit le dire ({motif})"
            );
        }
        autre => panic!("attendu une interruption, reçu {autre:?}"),
    }
}

/// #6911 — un ampli qui répond, mais avec des retours que l'émetteur écarte
/// (ici : un autre identifiant de flux). Le motif doit dire que des
/// datagrammes sont ARRIVÉS et pourquoi ils ont été écartés : c'est ce qui
/// sépare « rien n'arrive » (réseau, pare-feu) de « ça arrive, mais pas sous
/// la forme attendue » (format, session, flux).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_sans_retour_retenu_le_motif_dit_ce_qui_a_ete_ecarte() {
    let faux_ampli = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let adresse = faux_ampli.local_addr().unwrap();
    let repondeur = tokio::spawn(async move {
        let mut b = vec![0u8; 4096];
        loop {
            let Ok((_, de)) = faux_ampli.recv_from(&mut b).await else {
                continue;
            };
            let df = retour(&Retour {
                session: SESSION_DE_TEST,
                flux: 0x9999,
                derniere_trame_recue: 0,
                dernier_paquet_recu: 0,
                position_jouee: 0,
                manquants: Vec::new(),
            });
            let _ = faux_ampli.send_to(&df, de).await;
        }
    });
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let p = ParametresFlux {
        session: SESSION_DE_TEST,
        flux: 0x0042,
        frequence: 44_100,
        cadence: CADENCE_DE_TEST,
    };
    let fin = tokio::time::timeout(
        Duration::from_secs(10),
        diffuser(
            socket,
            adresse,
            p,
            source_de(sinus(44_100, 44_100)),
            Arc::new(Mutex::new(EtatFlux::default())),
            sans_garde(),
        ),
    )
    .await
    .expect("le flux doit se terminer");
    repondeur.abort();
    match fin {
        FinDeFlux::Interrompu { motif, .. } => {
            assert!(motif.contains("aucun retour"), "{motif}");
            assert!(
                motif.contains("reçu(s) et écarté(s)")
                    && motif.contains("autre session ou d'un autre flux"),
                "le motif doit dire que des retours sont arrivés et pourquoi ils ont été écartés ({motif})"
            );
        }
        autre => panic!("attendu une interruption, reçu {autre:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_une_session_etrangere_n_est_pas_acceptee_par_l_ampli() {
    let sim = Simulateur::demarrer(annonce_de_test(), None).await;
    let (fin, _) = lancer_un_flux(&sim, [1, 2, 3, 4], sinus(44_100, 44_100), 44_100).await;
    assert!(matches!(fin, FinDeFlux::Interrompu { .. }), "{fin:?}");
    assert!(sim.etat().paquets.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_l_avance_sur_la_lecture_est_tenue() {
    let sim = Simulateur::demarrer(annonce_de_test(), None).await;
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let etat = Arc::new(Mutex::new(EtatFlux::default()));
    let p = ParametresFlux {
        session: SESSION_DE_TEST,
        flux: 0x0042,
        frequence: 44_100,
        cadence: CADENCE_DE_TEST,
    };
    let tache = tokio::spawn(diffuser(
        socket,
        sim.adresse,
        p,
        source_de(sinus(44_100, 88_200)),
        etat.clone(),
        sans_garde(),
    ));
    let mut mesures = Vec::new();
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let e = etat.lock().unwrap().clone();
        if e.position_jouee > 0 && !e.tout_envoye {
            mesures.push(e.position_envoyee as i64 - e.position_jouee as i64);
        }
    }
    tache.abort();
    assert!(mesures.len() >= 10, "trop peu de mesures : {mesures:?}");
    let cible = (0.25 * 44_100.0) as i64;
    for m in &mesures[3..] {
        assert!(
            (m - cible).abs() < 4_410,
            "avance {m} trames, cible {cible} (±100 ms) : {mesures:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_un_echec_de_decodage_interrompt_le_flux() {
    let sim = Simulateur::demarrer(annonce_de_test(), None).await;
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let (tx, rx) = mpsc::channel(4);
    tx.send(Err("fichier illisible".to_string())).await.unwrap();
    let p = ParametresFlux {
        session: SESSION_DE_TEST,
        flux: 1,
        frequence: 44_100,
        cadence: CADENCE_DE_TEST,
    };
    let fin = diffuser(
        socket,
        sim.adresse,
        p,
        rx,
        Arc::new(Mutex::new(EtatFlux::default())),
        sans_garde(),
    )
    .await;
    assert_eq!(fin, FinDeFlux::interrompu("décodage : fichier illisible"));
}

// ─── 3. La sortie Tune ──────────────────────────────────────────────────────

struct Banc {
    sim: Simulateur,
    sortie: DevialetAirOutput,
    _activee: Arc<AtomicBool>,
    _relais: tokio::task::JoinHandle<()>,
}

impl Drop for Banc {
    fn drop(&mut self) {
        self._relais.abort();
    }
}

/// Une sortie face à un simulateur. L'écoute est alimentée par un relais
/// qui recopie l'annonce du simulateur toutes les 20 ms, comme le ferait
/// le broadcast.
async fn banc(annonce: super::protocole::Annonce, activee: bool) -> Banc {
    banc_avec_bornes(annonce, activee, reglages::BornesDb::PAR_DEFAUT).await
}

/// #6918 — le même banc, avec une course du curseur réglée.
async fn banc_avec_bornes(
    annonce: super::protocole::Annonce,
    activee: bool,
    bornes: reglages::BornesDb,
) -> Banc {
    let sim = Simulateur::demarrer(annonce, None).await;
    let ecoute = Ecoute::nouvelle();
    let drapeau = Arc::new(AtomicBool::new(activee));
    let indice = Arc::new(AtomicU8::new(9));
    let contexte = ContexteAir {
        activee: {
            let d = drapeau.clone();
            Arc::new(move || d.load(Ordering::SeqCst))
        },
        indice_source: Arc::new(move || indice.load(Ordering::SeqCst)),
        bornes_db: Arc::new(move || bornes),
        ports: Ports {
            annonces: 0,
            ampli: sim.adresse.port(),
            audio_local: 0,
            adresse_locale: Ipv4Addr::LOCALHOST,
        },
        cadence: CADENCE_DE_TEST,
    };
    let relais = {
        let etat = sim.etat.clone();
        let ecoute = ecoute.clone();
        tokio::spawn(async move {
            loop {
                let octets = etat
                    .lock()
                    .unwrap()
                    .annonce
                    .as_ref()
                    .map(super::protocole::annonce);
                if let Some(o) = octets {
                    ecoute.noter(IpAddr::V4(Ipv4Addr::LOCALHOST), &o);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let a = annonce_de_test();
    let sortie = DevialetAirOutput::new("EXPERT-TEST".into(), a.mac_texte(), ecoute, contexte);
    Banc {
        sim,
        sortie,
        _activee: drapeau,
        _relais: relais,
    }
}

/// Un fichier WAV stéréo 16 bits de `n` trames, dans un dossier jetable.
fn wav_16_bits(
    dossier: &crate::test_scratch::ScratchDir,
    frequence: u32,
    n: usize,
) -> (String, Vec<[i32; 2]>) {
    let mut donnees = Vec::with_capacity(n * 4);
    let mut attendu = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / f64::from(frequence);
        let g = (8_000.0 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()) as i16;
        let d = (-4_000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16;
        donnees.extend_from_slice(&g.to_le_bytes());
        donnees.extend_from_slice(&d.to_le_bytes());
        attendu.push([i32::from(g) << 8, i32::from(d) << 8]);
    }
    let mut fichier =
        crate::audio::wav::build_wav_header_with_data_size(2, frequence, 16, donnees.len() as u32)
            .to_vec();
    fichier.extend_from_slice(&donnees);
    let chemin = dossier.path().join(format!("essai-{frequence}.wav"));
    std::fs::write(&chemin, &fichier).unwrap();
    (chemin.to_string_lossy().into_owned(), attendu)
}

fn media(chemin: &str) -> PlayMedia<'_> {
    PlayMedia {
        url: chemin,
        file_path: Some(chemin),
        mime_type: "audio/wav",
        duration_ms: Some(500),
        ..Default::default()
    }
}

async fn attendre_l_arret(sortie: &DevialetAirOutput) -> crate::outputs::traits::OutputStatus {
    for _ in 0..500 {
        let st = sortie.get_status().await.unwrap();
        if st.state == TransportState::Stopped {
            return st;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("la lecture ne s'est pas terminée");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_option_desactivee_rien_ne_part_vers_l_ampli() {
    let b = banc(annonce_de_test(), false).await;
    let refus = b
        .sortie
        .play_media(&media("/nulle/part.wav"))
        .await
        .unwrap_err();
    assert!(
        refus.starts_with(reglages::SENTINELLE_NON_ACTIVEE),
        "{refus}"
    );
    assert!(b.sortie.set_volume(0.5).await.is_err());
    assert!(b.sortie.set_mute(true).await.is_err());
    assert!(!b.sortie.is_available().await);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = b.sim.etat();
    assert!(
        s.commandes.is_empty() && s.paquets.is_empty(),
        "rien ne doit partir"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_volume_et_sourdine_partent_deux_fois_et_jamais_au_dessus_du_plafond() {
    let b = banc(annonce_de_test(), true).await;
    assert!(b.sortie.is_available().await);
    b.sortie.set_volume(1.0).await.unwrap();
    b.sortie.set_volume(0.5).await.unwrap();
    b.sortie.set_mute(true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    {
        let s = b.sim.etat();
        let c = &s.commandes;
        assert_eq!(c.len(), 6, "{c:?}");
        assert_eq!((c[0].opcode, c[0].valeur), (opcode::VOLUME, -10.0));
        assert_eq!(
            c[0].compteur_commande, c[1].compteur_commande,
            "même commande"
        );
        assert_eq!(
            c[0].compteur_paquet + 1,
            c[1].compteur_paquet,
            "deux paquets"
        );
        assert_eq!((c[2].opcode, c[2].valeur), (opcode::VOLUME, -45.0));
        assert_eq!(c[4].opcode, opcode::SOURDINE);
        assert!(
            c.iter()
                .filter(|x| x.opcode == opcode::VOLUME)
                .all(|x| f64::from(x.valeur) <= reglages::PLAFOND_DB)
        );
    }
    tokio::time::sleep(Duration::from_millis(60)).await;
    let st = b.sortie.get_status().await.unwrap();
    assert!(st.muted, "la sourdine se relit dans l'annonce");
    assert_eq!(st.volume, 0.5, "le volume se relit dans l'annonce");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_lecture_d_un_fichier_jusqu_a_la_fin_naturelle() {
    let b = banc(annonce_de_test(), true).await;
    let dossier = crate::test_scratch::scratch_dir("devialet-air");
    let (chemin, attendu) = wav_16_bits(&dossier, 44_100, 22_050);
    b.sortie.play_media(&media(&chemin)).await.unwrap();
    assert_eq!(
        b.sortie.get_status().await.unwrap().state,
        TransportState::Playing
    );
    let t = b.sortie.transformations_reelles().expect("déclarées");
    assert!(!t.reechantillonnage(), "44,1 kHz part tel quel");
    let st = attendre_l_arret(&b.sortie).await;
    assert!(st.ended_naturally, "fin naturelle");
    assert_eq!(b.sortie.take_output_failure(), None);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = b.sim.etat();
    assert!(s.refus.is_empty(), "{:?}", s.refus);
    let (&flux, _) = s.paquets.iter().next().expect("un flux");
    let pcm = s.pcm(flux).expect("flux complet");
    let tete = trames_arrondies(44_100, CADENCE_DE_TEST.silence_de_tete) as usize;
    assert_eq!(
        &pcm[tete..tete + attendu.len()],
        &attendu[..],
        "16 bits décalés de 8"
    );
    assert_eq!(s.arrets.len(), 1, "un arrêt en fin de piste");
    assert_ne!(s.arrets[0], flux, "l'arrêt porte un NOUVEL identifiant");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_un_fichier_a_192_khz_part_reechantillonne_a_96_khz() {
    let b = banc(annonce_de_test(), true).await;
    let dossier = crate::test_scratch::scratch_dir("devialet-air-192");
    let (chemin, _) = wav_16_bits(&dossier, 192_000, 96_000);
    b.sortie.play_media(&media(&chemin)).await.unwrap();
    let t = b.sortie.transformations_reelles().expect("déclarées");
    assert!(t.reechantillonnage());
    assert_eq!(t.entree().cadence(), 192_000);
    assert_eq!(t.ouvert().cadence, 96_000);
    tokio::time::sleep(Duration::from_millis(400)).await;
    b.sortie.stop().await.unwrap();
    let s = b.sim.etat();
    assert!(!s.paquets.is_empty());
    for paquets in s.paquets.values() {
        for (e, _, d) in paquets.values() {
            assert_eq!(e.frequence, 96_000);
            assert_eq!(*d, [0, 0], "sans perte");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_ampli_en_veille_sur_une_autre_source_allume_puis_choisit_la_source_air() {
    let mut a = annonce_de_test();
    a.allume = false;
    a.source_choisie = 6;
    let b = banc(a, true).await;
    let dossier = crate::test_scratch::scratch_dir("devialet-air-veille");
    let (chemin, _) = wav_16_bits(&dossier, 44_100, 4_410);
    b.sortie.play_media(&media(&chemin)).await.unwrap();
    let st = attendre_l_arret(&b.sortie).await;
    assert!(st.ended_naturally);
    let s = b.sim.etat();
    let ops: Vec<u16> = s.commandes.iter().map(|c| c.opcode).collect();
    assert_eq!(
        ops,
        vec![
            opcode::ALLUMER,
            opcode::ALLUMER,
            opcode::SOURCE,
            opcode::SOURCE
        ],
        "allumer, puis source AIR, chaque commande deux fois"
    );
    assert_eq!(s.commandes[2].valeur, 9.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_un_ampli_servi_par_un_autre_hote_n_est_pas_pris() {
    let mut a = annonce_de_test();
    a.flux_en_cours = 0x27;
    a.aucun_flux_air = false;
    let b = banc(a, true).await;
    let refus = b
        .sortie
        .play_media(&media("/nulle/part.wav"))
        .await
        .unwrap_err();
    assert!(refus.contains("autre hôte"), "{refus}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = b.sim.etat();
    assert!(s.paquets.is_empty() && s.arrets.is_empty() && s.commandes.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_pause_envoie_un_arret_et_la_reprise_ouvre_un_flux_neuf() {
    let b = banc(annonce_de_test(), true).await;
    let dossier = crate::test_scratch::scratch_dir("devialet-air-pause");
    let (chemin, _) = wav_16_bits(&dossier, 44_100, 88_200);
    b.sortie.play_media(&media(&chemin)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    b.sortie.pause().await.unwrap();
    let st = b.sortie.get_status().await.unwrap();
    assert_eq!(st.state, TransportState::Paused);
    assert!(st.position_ms > 0, "position mémorisée");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let premier = {
        let s = b.sim.etat();
        assert_eq!(s.arrets.len(), 1, "un arrêt à la pause");
        *s.paquets.keys().next().unwrap()
    };
    b.sortie.resume().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    b.sortie.stop().await.unwrap();
    // L'arrêt voyage en UDP : lui laisser le temps d'arriver au simulateur.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let s = b.sim.etat();
    assert_eq!(s.paquets.len(), 2, "un flux neuf à la reprise");
    let second = *s.paquets.keys().nth(1).unwrap();
    assert_ne!(second, premier);
    assert_eq!(s.arrets.len(), 2);
}

/// #6923 — un déplacement dont le flux neuf ne part pas laisse la sortie
/// arrêtée. Elle doit le DIRE (`take_output_failure`), sinon le sondeur
/// prend cet arrêt pour une fin de piste et passe au morceau suivant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_un_deplacement_qui_ne_relance_pas_le_flux_le_dit() {
    let b = banc(annonce_de_test(), true).await;
    let dossier = crate::test_scratch::scratch_dir("devialet-air-seek-6923");
    let (chemin, _) = wav_16_bits(&dossier, 44_100, 88_200);
    b.sortie.play_media(&media(&chemin)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Le fichier disparaît : la réouverture du décodeur échoue au déplacement.
    std::fs::remove_file(&chemin).unwrap();
    let refus = b.sortie.seek(1_000).await;
    assert!(refus.is_err(), "le déplacement doit échouer : {refus:?}");
    let st = b.sortie.get_status().await.unwrap();
    assert_eq!(st.state, TransportState::Stopped);
    assert!(!st.ended_naturally, "un échec n'est pas une fin de piste");
    let motif = b
        .sortie
        .take_output_failure()
        .expect("l'échec du déplacement doit remonter au sondeur");
    assert!(motif.contains("déplacement impossible"), "{motif}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_le_fournisseur_n_ecoute_rien_tant_que_l_option_est_desactivee() {
    // Un port libre pour les annonces de test.
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let drapeau = Arc::new(AtomicBool::new(false));
    let contexte = ContexteAir {
        activee: {
            let d = drapeau.clone();
            Arc::new(move || d.load(Ordering::SeqCst))
        },
        indice_source: Arc::new(|| 9),
        bornes_db: Arc::new(|| reglages::BornesDb::PAR_DEFAUT),
        ports: Ports {
            annonces: port,
            ..Ports::AIR
        },
        cadence: CADENCE_DE_TEST,
    };
    let f = FournisseurDevialetAir::avec_attente(contexte, Duration::from_millis(300));
    let sim = Simulateur::demarrer(
        annonce_de_test(),
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)),
    )
    .await;
    let ctx = ProviderContext::default();
    assert!(f.discover(&ctx).await.is_empty());
    assert!(!f.ecoute().en_ecoute(), "option désactivée : aucune écoute");
    drapeau.store(true, Ordering::SeqCst);
    let sorties = f.discover(&ctx).await;
    assert_eq!(sorties.len(), 1);
    assert_eq!(
        sorties[0].device_id(),
        identifiant_de_sortie("02:00:5e:00:00:01")
    );
    assert_eq!(sorties[0].name(), "EXPERT-TEST");
    assert_eq!(sorties[0].output_type(), "devialet_air");
    assert!(!sorties[0].capabilities().can_gapless);
    drapeau.store(false, Ordering::SeqCst);
    assert!(f.discover(&ctx).await.is_empty());
    assert!(!f.ecoute().en_ecoute(), "option désactivée : écoute fermée");
    drop(sim);
}

#[path = "volume_db_6918_tests.rs"]
mod volume_db_6918;
