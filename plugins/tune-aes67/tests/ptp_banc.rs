//! Banc E2 (#7336) : le suiveur PTP face à un maître PTPv2 simulé.
//!
//! Aucun son, aucun droit, aucun réseau extérieur : maître et suiveur
//! parlent en unicast sur 127.0.0.1, ports libres. Le maître simulé
//! (`tune_aes67::ptp::banc`) est écrit à la main d'après IEEE 1588-2008,
//! indépendamment de `statime` ; son horloge est décalée de ~3 h et dérive
//! de +40 ppm sur celle de la machine. On compare directement le temps du
//! suiveur au temps vrai du maître.
//!
//! Seuils, sur la boucle locale avec horodatage logiciel :
//! - calage en moins de 20 s ;
//! - après 5 s de stabilisation, écart suiveur − maître sous 250 µs sur
//!   chaque mesure de la fenêtre ;
//! - fréquence apprise à ±10 ppm des +40 ppm du maître.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use tune_aes67::cadenceur::{self, Commandes, Message, ParametresCadence, Statistiques};
use tune_aes67::config::FluxAes67;
use tune_aes67::droit::DroitDuBanc;
use tune_aes67::horloge::{HorlogeSysteme, ReferenceHorloge, SourceHorloge, horodatage_rtp_decale};
use tune_aes67::ptp::banc::{ConfigMaitre, MaitreSimule};
use tune_aes67::ptp::transport::ConfigReseauPtp;
use tune_aes67::ptp::{ConfigPtp, SuiveurPtp};
use tune_aes67::rtp::EnteteRtp;
use tune_aes67::sap;
use tune_aes67::sdp;
use tune_aes67::sortie::{ReglagesEmission, SortieAes67};

const SEUIL_ECART_NS: i64 = 250_000;
const LIMITE_CALAGE: Duration = Duration::from_secs(20);

fn config_du_banc(evenement: SocketAddrV4, general: SocketAddrV4) -> ConfigPtp {
    let mut cfg = ConfigPtp::aes67(None);
    cfg.reseau = ConfigReseauPtp::unicast(Ipv4Addr::LOCALHOST, evenement, general);
    // Cadences du maître simulé : Announce à 4 Hz, d'où la fenêtre.
    cfg.intervalle_annonce_log2 = -2;
    cfg.delai_annonce = 3;
    cfg.intervalle_delay_req_log2 = -3;
    cfg.delai_sans_sync = Duration::from_secs(1);
    cfg
}

fn suiveur_pour(maitre: &MaitreSimule) -> Arc<SuiveurPtp> {
    let (e, g) = maitre.adresses();
    let s = SuiveurPtp::demarrer(config_du_banc(e, g));
    maitre.servir(s.adresses_locales().expect("suiveur lié"));
    s
}

/// suiveur − maître, au même instant (moyenne de deux lectures du suiveur
/// encadrant celle du maître). Une lecture interrompue par l'ordonnanceur
/// (plus de 20 µs entre les deux lectures du suiveur) est refaite : c'est le
/// banc qui aurait mesuré de travers, pas l'horloge.
fn ecart_ns(maitre: &MaitreSimule, s: &SuiveurPtp) -> i64 {
    loop {
        let a = s.maintenant_ns();
        let m = maitre.maintenant_ns();
        let b = s.maintenant_ns();
        if b.saturating_sub(a) < 20_000 {
            return (i128::from(a / 2 + b / 2) - i128::from(m)) as i64;
        }
    }
}

fn attendre(limite: Duration, condition: impl Fn() -> bool) -> Option<Duration> {
    let debut = Instant::now();
    while debut.elapsed() < limite {
        if condition() {
            return Some(debut.elapsed());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Mesure l'écart toutes les 50 ms pendant `duree` ; rend (min, max, moyenne).
fn mesurer(maitre: &MaitreSimule, s: &SuiveurPtp, duree: Duration) -> (i64, i64, i64) {
    let debut = Instant::now();
    let mut v = Vec::new();
    while debut.elapsed() < duree {
        v.push(ecart_ns(maitre, s));
        std::thread::sleep(Duration::from_millis(50));
    }
    let min = *v.iter().min().unwrap();
    let max = *v.iter().max().unwrap();
    let moyenne = v.iter().sum::<i64>() / v.len() as i64;
    (min, max, moyenne)
}

#[test]
fn le_suiveur_se_cale_sur_un_maitre_1588_2008_et_suit_sa_derive() {
    let maitre = MaitreSimule::demarrer(ConfigMaitre::par_defaut()).unwrap();
    let s = suiveur_pour(&maitre);
    assert_eq!(
        s.reference(),
        ReferenceHorloge::Locale,
        "pas calé au départ"
    );
    let avant = ecart_ns(&maitre, &s);
    assert!(
        avant.abs() > 3 * 3600 * 1_000_000_000,
        "écart initial {avant}"
    );

    let duree = attendre(LIMITE_CALAGE, || s.calee())
        .unwrap_or_else(|| panic!("pas de calage en {LIMITE_CALAGE:?} : {:?}", s.etat()));
    eprintln!("calage en {duree:?} ; état {:?}", s.etat());
    assert_eq!(
        s.reference(),
        ReferenceHorloge::Ptp {
            identite_maitre: maitre.identite(),
            domaine: 0
        }
    );
    assert_eq!(
        s.reference().attribut_sdp(),
        "ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0"
    );

    std::thread::sleep(Duration::from_secs(5));
    let (min, max, moyenne) = mesurer(&maitre, &s, Duration::from_secs(5));
    let e = s.etat();
    eprintln!(
        "écart suiveur − maître sur 5 s : min {min} ns, max {max} ns, moyenne {moyenne} ns ; \
         fréquence {:.2} ppm ; état {e:?}",
        e.frequence_ppm
    );
    assert!(
        min.abs() < SEUIL_ECART_NS && max.abs() < SEUIL_ECART_NS,
        "écart hors seuil : [{min}, {max}] ns"
    );
    assert!(
        (e.frequence_ppm - 40.0).abs() < 10.0,
        "fréquence apprise {} ppm",
        e.frequence_ppm
    );
    assert!(e.calee && !e.repli_horloge_locale && e.raison_du_repli.is_none());
    assert_eq!(e.etat_port, "esclave");
    assert_eq!(e.maitre.as_deref(), Some("00-1D-C1-FF-FE-12-34-56"));
    assert!(e.sauts >= 1, "le premier calage est un saut de ~3 h");
    assert!(
        maitre
            .compteurs
            .delay_req
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
    s.arreter();
}

#[test]
fn contre_epreuve_un_maitre_d_un_autre_domaine_n_est_jamais_suivi() {
    let mut cfg = ConfigMaitre::par_defaut();
    cfg.domaine = 1;
    let maitre = MaitreSimule::demarrer(cfg).unwrap();
    let s = suiveur_pour(&maitre);
    std::thread::sleep(Duration::from_secs(6));
    let e = s.etat();
    assert!(
        maitre
            .compteurs
            .sync
            .load(std::sync::atomic::Ordering::Relaxed)
            > 20
    );
    assert!(!e.calee && e.repli_horloge_locale, "{e:?}");
    assert_eq!(e.raison_du_repli.as_deref(), Some("aucun maître vu"));
    assert_eq!(s.reference(), ReferenceHorloge::Locale);
    assert_eq!(e.sauts, 0);
    assert_eq!(
        maitre
            .compteurs
            .delay_req
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    // Et l'horloge est restée l'horloge locale : toujours ~3 h d'écart.
    assert!(ecart_ns(&maitre, &s).abs() > 3 * 3600 * 1_000_000_000);
}

#[test]
fn perte_du_maitre_repli_sans_saut_puis_calage_sur_un_autre_maitre() {
    let maitre_a = MaitreSimule::demarrer(ConfigMaitre::par_defaut()).unwrap();
    let (adr_e, adr_g) = maitre_a.adresses();
    let s = suiveur_pour(&maitre_a);
    attendre(LIMITE_CALAGE, || s.calee()).expect("calage sur A");
    let generation_calee = s.generation();

    // Le maître A disparaît.
    drop(maitre_a);
    let debut = Instant::now();
    let mut precedent = s.maintenant_ns();
    let mut recul = false;
    while s.calee() && debut.elapsed() < Duration::from_secs(10) {
        let t = s.maintenant_ns();
        recul |= t < precedent;
        precedent = t;
        std::thread::sleep(Duration::from_millis(5));
    }
    let e = s.etat();
    eprintln!("repli après {:?} : {e:?}", debut.elapsed());
    assert!(!e.calee && e.repli_horloge_locale, "{e:?}");
    assert!(e.raison_du_repli.is_some());
    assert_eq!(s.reference(), ReferenceHorloge::Locale);
    assert!(s.generation() > generation_calee);
    assert!(!recul, "le temps a reculé pendant le repli");
    // Le repli ne saute pas : quelques secondes plus tard, le temps
    // continue (le temps reste monotone pendant 1 s de repli).
    let a = s.maintenant_ns();
    std::thread::sleep(Duration::from_secs(1));
    let b = s.maintenant_ns();
    assert!(b > a && b - a < 1_100_000_000, "{}", b - a);

    // Un autre maître (autre identité, 5 s plus loin) prend la place.
    let mut cfg_b = ConfigMaitre::par_defaut();
    cfg_b.identite = [0x00, 0x0a, 0xe4, 0xff, 0xfe, 0xab, 0xcd, 0xef];
    cfg_b.decalage_ns += 5_000_000_000;
    cfg_b.derive_ppm = -25.0;
    let maitre_b = MaitreSimule::demarrer_sur(cfg_b, adr_e, adr_g).unwrap();
    maitre_b.servir(s.adresses_locales().unwrap());
    attendre(LIMITE_CALAGE, || {
        s.calee() && s.etat().maitre.as_deref() == Some("00-0A-E4-FF-FE-AB-CD-EF")
    })
    .unwrap_or_else(|| panic!("pas de calage sur B : {:?}", s.etat()));
    assert_eq!(
        s.reference().attribut_sdp(),
        "ptp=IEEE1588-2008:00-0A-E4-FF-FE-AB-CD-EF:0"
    );
    std::thread::sleep(Duration::from_secs(5));
    let (min, max, moyenne) = mesurer(&maitre_b, &s, Duration::from_secs(3));
    eprintln!(
        "écart sur B : [{min}, {max}] ns, moyenne {moyenne} ns ; {:?}",
        s.etat()
    );
    assert!(min.abs() < SEUIL_ECART_NS && max.abs() < SEUIL_ECART_NS);
}

/// Lance le cadenceur sur `horloge`, reçoit `n` paquets et rend, pour
/// chacun, (horodatage RTP, temps du maître à l'arrivée).
fn emettre_et_recevoir(
    horloge: Arc<dyn SourceHorloge>,
    maitre: &MaitreSimule,
    n: usize,
    decalage_media: u32,
) -> Vec<(EnteteRtp, u64)> {
    let ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
    ecoute
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.connect(ecoute.local_addr().unwrap()).unwrap();
    let (tx, rx) = sync_channel(n + 1);
    for _ in 0..n {
        tx.send(Message::Charge(vec![0; 288])).unwrap();
    }
    tx.send(Message::Fin).unwrap();
    let commandes = Arc::new(Commandes::default());
    let fil = cadenceur::lancer(
        socket,
        Some(rx),
        horloge,
        ParametresCadence {
            cadence: 48_000,
            trames_par_paquet: 48,
            type_charge: 98,
            ssrc: 7,
            avance: Duration::ZERO,
            decalage_media,
            pretampon_paquets: 1,
            temps_reel: false,
        },
        commandes.clone(),
        Arc::new(Statistiques::default()),
    )
    .unwrap();
    let mut t = [0u8; 2048];
    let mut recus = Vec::with_capacity(n);
    for _ in 0..n {
        let k = ecoute.recv(&mut t).unwrap();
        let arrivee = maitre.maintenant_ns();
        recus.push((EnteteRtp::lire(&t[..k]).unwrap().0, arrivee));
    }
    // Depuis l'étape E3, le fil survit à la fin d'un flux : l'arrêter.
    commandes
        .arret
        .store(true, std::sync::atomic::Ordering::Release);
    fil.join().unwrap();
    recus
}

#[test]
fn l_emetteur_cale_horodate_en_temps_ptp_et_la_contre_epreuve_non() {
    let maitre = MaitreSimule::demarrer(ConfigMaitre::par_defaut()).unwrap();
    let s = suiveur_pour(&maitre);
    attendre(LIMITE_CALAGE, || s.calee()).expect("calage");
    std::thread::sleep(Duration::from_secs(4));

    // 3 s de flux. L'horodatage RTP d'un paquet est le temps du MAÎTRE à son
    // émission × 48 kHz (+ décalage annoncé). On ne voit que l'ARRIVÉE (le
    // temps du maître lu par le fil de réception), qui suit l'émission de
    // quelques µs, davantage quand la machine est chargée. D'où :
    // - jamais d'horodatage EN AVANCE sur le maître de plus d'une demi-ms ;
    // - un retard médian sous une durée de paquet (48 trames) ;
    // - aucune dérive : les médianes du début et de la fin concordent.
    let decalage = 0x1234_5678;
    let g0 = s.generation();
    let recus = emettre_et_recevoir(s.clone(), &maitre, 3000, decalage);
    assert_eq!(s.generation(), g0, "ni saut ni décrochage pendant l'essai");
    let retards: Vec<i32> = recus
        .iter()
        .map(|(e, a)| horodatage_rtp_decale(*a, 48_000, decalage).wrapping_sub(e.horodatage) as i32)
        .collect();
    let mediane = |v: &[i32]| {
        let mut v = v.to_vec();
        v.sort_unstable();
        v[v.len() / 2]
    };
    let avance_max = -*retards.iter().min().unwrap();
    let (med, med_debut, med_fin) = (
        mediane(&retards),
        mediane(&retards[..500]),
        mediane(&retards[retards.len() - 500..]),
    );
    eprintln!(
        "retards (trames) : médiane {med}, début {med_debut}, fin {med_fin}, \
         max {}, avance max {avance_max}",
        retards.iter().max().unwrap()
    );
    assert!(
        avance_max <= 24,
        "horodatage en avance de {avance_max} trames"
    );
    assert!((0..48).contains(&med), "retard médian {med} trames");
    assert!(
        (med_fin - med_debut).abs() <= 12,
        "dérive {med_debut} → {med_fin}"
    );
    for p in recus.windows(2) {
        assert_eq!(p[1].0.horodatage, p[0].0.horodatage.wrapping_add(48));
    }

    // Contre-épreuve : le même émetteur sur l'horloge système est à ~3 h du
    // maître, soit des centaines de millions de trames.
    let recus = emettre_et_recevoir(Arc::new(HorlogeSysteme::new()), &maitre, 50, decalage);
    let (e, arrivee) = &recus[0];
    let ecart = horodatage_rtp_decale(*arrivee, 48_000, decalage).wrapping_sub(e.horodatage) as i32;
    assert!(ecart.abs() > 48_000 * 60, "écart {ecart} trames");
    s.arreter();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_sdp_passe_de_local_a_ptp_au_calage_et_sap_le_reannonce() {
    let maitre = MaitreSimule::demarrer(ConfigMaitre::par_defaut()).unwrap();
    let s = suiveur_pour(&maitre);
    let sap_ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
    sap_ecoute
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let std::net::SocketAddr::V4(destination_sap) = sap_ecoute.local_addr().unwrap() else {
        unreachable!()
    };
    let flux: FluxAes67 = serde_json::from_value(serde_json::json!({
        "id": "banc", "nom": "Banc", "adresse": "127.0.0.1", "port": 5004,
        "interface": "127.0.0.1",
    }))
    .unwrap();
    let sortie = SortieAes67::new(
        flux,
        s.clone(),
        Arc::new(DroitDuBanc),
        ReglagesEmission {
            destination_sap,
            intervalle_sap: Duration::from_secs(3600),
            temps_reel: false,
            avance: Duration::ZERO,
            decalage_media: 0,
        },
    )
    .unwrap();
    let avant = sdp::lire(&sortie.sdp()).unwrap();
    assert_eq!(avant.ts_refclk, "local");
    assert_eq!(avant.mediaclk, "direct=0");
    assert!(sortie.sdp().contains(" 1 IN IP4 "), "version 1");

    let ecoute = std::thread::spawn(move || {
        let mut lus = Vec::new();
        let mut t = [0u8; 2048];
        let debut = Instant::now();
        while debut.elapsed() < LIMITE_CALAGE + Duration::from_secs(2) {
            if let Ok(n) = sap_ecoute.recv(&mut t) {
                let lu = sap::lire(&t[..n]).unwrap();
                let fin = lu.type_sap == sap::TypeSap::Annonce && lu.sdp.contains("ptp=");
                lus.push(lu);
                if fin {
                    break;
                }
            }
        }
        lus
    });
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || attendre(LIMITE_CALAGE, || s2.calee()))
        .await
        .unwrap()
        .expect("calage");
    let lus = tokio::task::spawn_blocking(move || ecoute.join().unwrap())
        .await
        .unwrap();
    let types: Vec<_> = lus.iter().map(|l| l.type_sap).collect();
    eprintln!("SAP reçus : {types:?}");
    let dernier = lus.last().expect("des annonces SAP");
    assert_eq!(dernier.type_sap, sap::TypeSap::Annonce);
    let apres = sdp::lire(&dernier.sdp).unwrap();
    assert_eq!(
        apres.ts_refclk,
        "ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0"
    );
    assert_eq!(apres.mediaclk, "direct=0");
    assert!(
        dernier.sdp.contains(" 2 IN IP4 "),
        "version 2 : {}",
        dernier.sdp
    );
    // L'ancien SDP (local) a été retiré avant la nouvelle annonce.
    let retrait = &lus[lus.len() - 2];
    assert_eq!(retrait.type_sap, sap::TypeSap::Retrait);
    assert!(retrait.sdp.contains("ts-refclk:local"));
    let diag = tune_core::outputs::traits::OutputTarget::diagnostics_json(&sortie).unwrap();
    assert_eq!(diag["horloge_calee"], true);
    assert_eq!(
        diag["reference_horloge"],
        "ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0"
    );
    sortie.fermer();
    s.arreter();
}
