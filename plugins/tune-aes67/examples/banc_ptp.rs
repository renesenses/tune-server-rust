//! Banc E2 de longue durée (#7336) : le suiveur PTP face au maître simulé,
//! sans son, sans droits, sur 127.0.0.1.
//!
//! ```text
//! cargo run --release -p tune-aes67 --example banc_ptp -- <secondes> [ppm]
//! ```
//!
//! Toutes les 10 s : écart suiveur − maître (min, max, moyenne), fréquence
//! apprise, état. À la fin : un résumé JSON sur la sortie standard, et un
//! code de sortie non nul si l'horloge a décroché ou dépassé 250 µs après la
//! période de calage.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use tune_aes67::horloge::SourceHorloge;
use tune_aes67::ptp::banc::{ConfigMaitre, MaitreSimule};
use tune_aes67::ptp::transport::ConfigReseauPtp;
use tune_aes67::ptp::{ConfigPtp, SuiveurPtp};

fn main() {
    let mut args = std::env::args().skip(1);
    let secondes: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);
    let ppm: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(40.0);
    let mut cm = ConfigMaitre::par_defaut();
    cm.derive_ppm = ppm;
    let maitre = MaitreSimule::demarrer(cm).expect("maître");
    let (e, g) = maitre.adresses();
    let mut cfg = ConfigPtp::aes67(None);
    cfg.reseau = ConfigReseauPtp::unicast(Ipv4Addr::LOCALHOST, e, g);
    cfg.intervalle_annonce_log2 = -2;
    cfg.intervalle_delay_req_log2 = -3;
    cfg.delai_sans_sync = Duration::from_secs(1);
    let s = SuiveurPtp::demarrer(cfg);
    maitre.servir(s.adresses_locales().expect("suiveur"));

    let debut = Instant::now();
    while !s.calee() {
        assert!(debut.elapsed() < Duration::from_secs(30), "pas de calage");
        std::thread::sleep(Duration::from_millis(20));
    }
    let calage = debut.elapsed();
    eprintln!("calé en {calage:?}");
    std::thread::sleep(Duration::from_secs(5));
    let generation = s.generation();

    // Une lecture interrompue par l'ordonnanceur (plus de 20 µs entre les
    // deux lectures du suiveur) est refaite et comptée : c'est le banc qui
    // aurait mesuré de travers, pas l'horloge.
    let relectures = std::cell::Cell::new(0u64);
    let ecart = || loop {
        let a = s.maintenant_ns();
        let m = maitre.maintenant_ns();
        let b = s.maintenant_ns();
        if b.saturating_sub(a) < 20_000 {
            return (i128::from(a / 2 + b / 2) - i128::from(m)) as i64;
        }
        relectures.set(relectures.get() + 1);
    };
    let fin = Instant::now() + Duration::from_secs(secondes);
    let (mut min, mut max, mut somme, mut n) = (i64::MAX, i64::MIN, 0i128, 0i64);
    let mut decroche = 0u64;
    let mut raisons: std::collections::BTreeMap<String, u64> = Default::default();
    let mut fenetre: Vec<i64> = Vec::new();
    let mut bilan = Instant::now();
    while Instant::now() < fin {
        let x = ecart();
        min = min.min(x);
        max = max.max(x);
        somme += i128::from(x);
        n += 1;
        fenetre.push(x);
        if !s.calee() {
            decroche += 1;
            let e = s.etat();
            let raison = e.raison_du_repli.unwrap_or_default();
            eprintln!(
                "t+{:>5}s DÉCROCHÉ : {raison} ; dernière Sync il y a {:?} ms ; décalage estimé {:?} ns ; sauts {}",
                debut.elapsed().as_secs(),
                e.derniere_sync_il_y_a_ms,
                e.decalage_ns,
                e.sauts
            );
            *raisons.entry(raison).or_insert(0u64) += 1;
        }
        if bilan.elapsed() >= Duration::from_secs(10) {
            let e = s.etat();
            eprintln!(
                "t+{:>5}s écart [{:>7}, {:>7}] ns moyenne {:>6} ns ; {:+.3} ppm ; calée {} ; décalage estimé {:?} ns ; délai {:?} ns",
                debut.elapsed().as_secs(),
                fenetre.iter().min().unwrap(),
                fenetre.iter().max().unwrap(),
                fenetre.iter().sum::<i64>() / fenetre.len() as i64,
                e.frequence_ppm,
                e.calee,
                e.decalage_ns,
                e.delai_moyen_ns,
            );
            fenetre.clear();
            bilan = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let e = s.etat();
    let ok =
        decroche == 0 && min.abs() < 250_000 && max.abs() < 250_000 && s.generation() == generation;
    println!(
        "{}",
        serde_json::json!({
            "duree_s": secondes,
            "derive_maitre_ppm": ppm,
            "calage_ms": calage.as_millis() as u64,
            "mesures": n,
            "ecart_min_ns": min,
            "ecart_max_ns": max,
            "ecart_moyen_ns": (somme / i128::from(n.max(1))) as i64,
            "mesures_non_calees": decroche,
            "lectures_refaites": relectures.get(),
            "raisons_des_decrochages": raisons,
            "changements_de_generation": s.generation() - generation,
            "frequence_apprise_ppm": e.frequence_ppm,
            "sync_envoyees": maitre.compteurs.sync.load(std::sync::atomic::Ordering::Relaxed),
            "delay_req_recus": maitre.compteurs.delay_req.load(std::sync::atomic::Ordering::Relaxed),
            "verdict": if ok { "OK" } else { "ECHEC" },
        })
    );
    s.arreter();
    maitre.arreter();
    if !ok {
        std::process::exit(1);
    }
}
