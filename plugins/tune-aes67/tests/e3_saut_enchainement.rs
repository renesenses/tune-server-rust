//! Étape E3 (#7336) en boucle locale : saut, enchaînement sans blanc (même
//! cadence et changement de cadence), pause et reprise, nouvelle lecture.
//!
//! Aucun son, aucun réseau extérieur : RTP et SAP en unicast vers
//! 127.0.0.1, sur des ports libres non privilégiés. Les pistes « compteur »
//! portent dans chaque trame son numéro : le PCM reçu dit à l'échantillon
//! près quelle trame du fichier il transporte.
//!
//! Ce que chaque essai exige, en plus du sien :
//! - la séquence RTP avance de un à chaque paquet, du premier au dernier,
//!   à travers toutes les commandes ;
//! - l'horodatage ne recule ni ne chevauche jamais : chaque paquet commence
//!   au plus tôt à la trame qui suit le précédent, exactement à celle-là
//!   s'il n'est pas marqué.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tune_aes67::banc::{self, Capture, RecepteurDeTest};
use tune_aes67::config::FluxAes67;
use tune_aes67::droit::DroitDuBanc;
use tune_aes67::horloge::HorlogeSysteme;
use tune_aes67::l24;
use tune_aes67::rtp::EnteteRtp;
use tune_aes67::sortie::{ReglagesEmission, SortieAes67};
use tune_core::outputs::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

struct Banc {
    rtp: RecepteurDeTest,
    sap: RecepteurDeTest,
    sortie: SortieAes67,
    _dossier: tempfile::TempDir,
    dossier: std::path::PathBuf,
}

impl Banc {
    fn new() -> Self {
        let locale = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        let rtp = RecepteurDeTest::ecouter(locale).unwrap();
        let sap = RecepteurDeTest::ecouter(locale).unwrap();
        let flux: FluxAes67 = serde_json::from_value(serde_json::json!({
            "id": "banc-e3",
            "nom": "Banc E3",
            "adresse": "127.0.0.1",
            "port": rtp.adresse().port(),
            "interface": "127.0.0.1",
        }))
        .unwrap();
        let sortie = SortieAes67::new(
            flux,
            Arc::new(HorlogeSysteme::new()),
            Arc::new(DroitDuBanc),
            ReglagesEmission {
                destination_sap: sap.adresse(),
                intervalle_sap: Duration::from_secs(3600),
                temps_reel: false,
                avance: Duration::ZERO,
                decalage_media: 0,
            },
        )
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        Self {
            dossier: d.path().to_path_buf(),
            _dossier: d,
            rtp,
            sap,
            sortie,
        }
    }

    fn compteur(&self, nom: &str, cadence: u32, trames: u32, depart: i32) -> String {
        let c = self.dossier.join(nom);
        banc::ecrire_wav_compteur(&c, cadence, trames, depart).unwrap();
        c.to_str().unwrap().to_string()
    }

    fn sinus(&self, nom: &str, cadence: u32, frequence: f64, duree: Duration) -> String {
        let c = self.dossier.join(nom);
        banc::ecrire_wav_sinus(&c, cadence, &[frequence, frequence], duree).unwrap();
        c.to_str().unwrap().to_string()
    }

    fn terminer(self) -> Capture {
        self.sortie.fermer();
        std::thread::sleep(Duration::from_millis(50));
        let _ = self.sap.terminer();
        self.rtp.terminer()
    }
}

fn media<'a>(chemin: &'a str, uri: &'a str) -> PlayMedia<'a> {
    PlayMedia {
        url: uri,
        mime_type: "audio/wav",
        file_path: Some(chemin),
        ..Default::default()
    }
}

async fn statut(s: &SortieAes67) -> OutputStatus {
    s.get_status().await.unwrap()
}

async fn attendre_la_fin(s: &SortieAes67, limite: Duration) -> Vec<OutputStatus> {
    let debut = Instant::now();
    let mut vus = Vec::new();
    loop {
        let st = statut(s).await;
        let fini = st.state == TransportState::Stopped && st.ended_naturally;
        if st.state == TransportState::Stopped
            && !st.ended_naturally
            && let Some(e) = s.take_output_failure()
        {
            panic!("lecture en échec : {e}");
        }
        vus.push(st);
        if fini {
            return vus;
        }
        assert!(
            debut.elapsed() < limite,
            "pas de fin naturelle : {:?}",
            vus.last()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Les paquets reçus : en-tête et trames (gauche) décodées.
fn paquets(c: &Capture) -> Vec<(EnteteRtp, Vec<i32>)> {
    c.datagrammes
        .iter()
        .map(|d| {
            let (e, charge) = EnteteRtp::lire(&d.donnees).unwrap();
            let pcm = l24::lire_l24(charge);
            (e, pcm.iter().step_by(2).copied().collect())
        })
        .collect()
}

/// Séquence +1 partout ; horodatage jamais en recul ni en chevauchement,
/// exactement +48 hors paquets marqués. Rend le plus petit pas vu.
fn exiger_la_continuite(p: &[(EnteteRtp, Vec<i32>)]) -> i32 {
    let mut pas_min = i32::MAX;
    for (i, paire) in p.windows(2).enumerate() {
        let (a, b) = (&paire[0].0, &paire[1].0);
        assert_eq!(
            b.sequence,
            a.sequence.wrapping_add(1),
            "séquence au paquet {} : {} après {}",
            i + 1,
            b.sequence,
            a.sequence
        );
        let pas = b.horodatage.wrapping_sub(a.horodatage) as i32;
        pas_min = pas_min.min(pas);
        assert!(
            pas >= 48,
            "paquet {} : horodatage +{pas} trames (recul ou chevauchement)",
            i + 1
        );
        if !b.marqueur {
            assert_eq!(pas, 48, "paquet {} non marqué : +{pas}", i + 1);
        }
    }
    pas_min
}

/// Les ruptures du compteur gauche (trame suivante ≠ trame + 1), hors
/// silence final : `(rang de l'échantillon, avant, après)`.
fn ruptures_du_compteur(trames: &[i32]) -> Vec<(usize, i32, i32)> {
    trames
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[1] != w[0] + 1 && w[1] != 0)
        .map(|(i, w)| (i + 1, w[0], w[1]))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deux_lectures_de_suite_ne_font_reculer_ni_la_sequence_ni_l_horodatage() {
    let b = Banc::new();
    let a = b.compteur("a.wav", 48_000, 9_600, 0);
    b.sortie.play_media(&media(&a, "file:///a")).await.unwrap();
    attendre_la_fin(&b.sortie, Duration::from_secs(10)).await;
    b.sortie.play_media(&media(&a, "file:///a")).await.unwrap();
    attendre_la_fin(&b.sortie, Duration::from_secs(10)).await;
    let c = b.terminer();
    let p = paquets(&c);
    assert_eq!(p.len(), 400);
    let pas_min = exiger_la_continuite(&p);
    println!("deux lectures : {} paquets, pas min {pas_min}", p.len());
    assert!(p[200].0.marqueur, "la seconde lecture part marquée");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_saut_repart_a_la_trame_demandee_sans_recul_ni_trou() {
    let b = Banc::new();
    // 3 s à 48 kHz : trames 0 à 143 999.
    let a = b.compteur("a.wav", 48_000, 144_000, 0);
    assert!(b.sortie.capabilities().can_seek);
    b.sortie.play_media(&media(&a, "file:///a")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let avant = statut(&b.sortie).await;
    let t0 = Instant::now();
    b.sortie.seek(2_000).await.unwrap();
    let apres = statut(&b.sortie).await;
    let duree_du_saut = t0.elapsed();
    assert_eq!(apres.state, TransportState::Playing);
    assert!(
        (2_000..2_100).contains(&apres.position_ms),
        "position annoncée après le saut : {}",
        apres.position_ms
    );
    let fin = attendre_la_fin(&b.sortie, Duration::from_secs(10)).await;
    let derniere = fin.last().unwrap();
    assert_eq!(derniere.current_uri.as_deref(), Some("file:///a"));
    let c = b.terminer();
    let p = paquets(&c);
    let pas_min = exiger_la_continuite(&p);
    let trames: Vec<i32> = p.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    let ruptures = ruptures_du_compteur(&trames);
    println!(
        "saut : position avant {} ms, après {} ms ; saut posé en {:?} ; {} paquets ; \
         pas min {pas_min} ; ruptures {ruptures:?} ; fin à {} ms",
        avant.position_ms,
        apres.position_ms,
        duree_du_saut,
        p.len(),
        derniere.position_ms
    );
    // Une seule rupture : la trame 96 000 (2 s) suit la dernière trame émise
    // avant le saut ; puis tout jusqu'à 143 999, sans rien perdre.
    assert_eq!(ruptures.len(), 1, "{ruptures:?}");
    let (rang, _, apres_saut) = ruptures[0];
    assert_eq!(apres_saut, 96_000, "le saut tombe à la trame près");
    assert_eq!(trames[0], 0);
    assert_eq!(rang % 48, 0, "le saut ouvre un paquet neuf");
    let paquet_du_saut = rang / 48;
    assert!(p[paquet_du_saut].0.marqueur, "le saut repart marqué");
    let fin_utile = trames.iter().rposition(|&v| v != 0).unwrap();
    assert_eq!(trames[fin_utile], 143_999);
    assert_eq!(
        fin_utile - rang + 1,
        48_000,
        "1 s après le saut, intégralement"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l_enchainement_colle_la_piste_suivante_a_l_echantillon_pres() {
    let b = Banc::new();
    // A : 24 504 trames (pas un multiple de 48 : la frontière tombe au
    // milieu d'un paquet). B : 14 400 trames, numérotées à partir d'un million.
    let a = b.compteur("a.wav", 48_000, 24_504, 0);
    let bb = b.compteur("b.wav", 48_000, 14_400, 1_000_000);
    assert!(b.sortie.capabilities().can_gapless);
    assert!(b.sortie.supports_internal_gapless());
    b.sortie.play_media(&media(&a, "file:///a")).await.unwrap();
    b.sortie
        .set_next_media(&media(&bb, "file:///b"))
        .await
        .unwrap();
    let vus = attendre_la_fin(&b.sortie, Duration::from_secs(10)).await;
    let c = b.terminer();
    let p = paquets(&c);
    let pas_min = exiger_la_continuite(&p);
    let marques = p.iter().filter(|(e, _)| e.marqueur).count();
    let trames: Vec<i32> = p.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    let ruptures = ruptures_du_compteur(&trames);
    // L'état : A puis B, la position de B repart de zéro.
    let uris: Vec<&str> = vus
        .iter()
        .filter_map(|s| s.current_uri.as_deref())
        .collect();
    let bascule = uris
        .iter()
        .position(|u| *u == "file:///b")
        .expect("B annoncée");
    let pos_b = vus[bascule].position_ms;
    println!(
        "enchaînement : {} paquets, {marques} marqué(s), pas min {pas_min}, ruptures \
         {ruptures:?}, B annoncée à {pos_b} ms",
        p.len()
    );
    assert!(uris[..bascule].iter().all(|u| *u == "file:///a"));
    assert!(uris[bascule..].iter().all(|u| *u == "file:///b"));
    assert!(pos_b < 100, "position de B à la bascule : {pos_b} ms");
    assert_eq!(
        vus.last().unwrap().current_uri.as_deref(),
        Some("file:///b")
    );
    // Le PCM : 0..24 503 puis 1 000 000..1 014 399, sans un échantillon de
    // silence entre les deux, puis le silence du dernier paquet.
    assert_eq!(ruptures, vec![(24_504, 24_503, 1_000_000)]);
    assert_eq!(trames[0], 0);
    assert_eq!(p.len(), (24_504 + 14_400usize).div_ceil(48));
    let fin_utile = trames.iter().rposition(|&v| v != 0).unwrap();
    assert_eq!(trames[fin_utile], 1_014_399);
    assert_eq!(fin_utile + 1, 24_504 + 14_400);
    // La frontière est au milieu du paquet 510 (24 504 = 510 × 48 + 24) :
    // ce paquet n'est pas marqué, il porte la fin de A et le début de B.
    assert!(!p[510].0.marqueur);
    assert_eq!(p[510].1[23], 24_503);
    assert_eq!(p[510].1[24], 1_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l_enchainement_44_1_puis_96_khz_reste_un_flux_48_khz_continu() {
    let b = Banc::new();
    let a = b.sinus("a-44100.wav", 44_100, 1000.0, Duration::from_millis(700));
    let bb = b.sinus("b-96000.wav", 96_000, 1000.0, Duration::from_millis(500));
    b.sortie.play_media(&media(&a, "file:///a")).await.unwrap();
    b.sortie
        .set_next_media(&media(&bb, "file:///b"))
        .await
        .unwrap();
    let vus = attendre_la_fin(&b.sortie, Duration::from_secs(20)).await;
    // Avant `terminer`, qui efface le dossier des pistes.
    let (attendu, trames_par_piste) =
        banc::pcm_attendu_enchaine(&[&a, &bb], 48_000, 2, 48).unwrap();
    let c = b.terminer();
    let analyse = c.analyser(48, Duration::from_millis(1));
    let p = paquets(&c);
    let pas_min = exiger_la_continuite(&p);
    // Autour de la frontière, aucun silence : la plus longue suite
    // d'échantillons quasi nuls (|x| < 2⁻¹⁰ de la pleine échelle) d'un
    // sinus 1 kHz à −6 dBFS est de 1 ou 2.
    let gauche: Vec<i32> = p.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    let frontiere = trames_par_piste[0] as usize;
    let fenetre = &gauche[frontiere - 480..frontiere + 480];
    let mut plus_long_silence = 0;
    let mut courant = 0;
    for &v in fenetre {
        if v.unsigned_abs() < 8_192 {
            courant += 1;
            plus_long_silence = plus_long_silence.max(courant);
        } else {
            courant = 0;
        }
    }
    let uris: Vec<&str> = vus
        .iter()
        .filter_map(|s| s.current_uri.as_deref())
        .collect();
    println!(
        "changement de cadence : trames par piste à 48 kHz {trames_par_piste:?} (attendu ~33 600 \
         et ~24 000) ; {} paquets ; marqueurs {} ; pas min {pas_min} ; reculs {} ; \
         plus long silence à la frontière {plus_long_silence} échantillon(s) ; sha reçu = attendu : {}",
        analyse.paquets,
        analyse.marqueurs,
        analyse.reculs_d_horodatage,
        analyse.sha256_pcm == banc::hex(&<sha2::Sha256 as sha2::Digest>::digest(&attendu))
    );
    assert_eq!(analyse.pertes, 0);
    assert_eq!(analyse.reculs_d_horodatage, 0);
    assert_eq!(analyse.octets_pcm, attendu.len());
    assert_eq!(
        analyse.sha256_pcm,
        banc::hex(&<sha2::Sha256 as sha2::Digest>::digest(&attendu)),
        "le flux = A rééchantillonnée puis B rééchantillonnée, bout à bout"
    );
    assert!(
        plus_long_silence <= 2,
        "{plus_long_silence} échantillons de silence"
    );
    assert!(uris.contains(&"file:///a") && uris.last() == Some(&"file:///b"));
    // 1 kHz des deux côtés, à 48 kHz.
    let canal = |debut: usize| -> Vec<f64> {
        gauche[debut..debut + 9_600]
            .iter()
            .map(|&v| f64::from(v) / 8_388_608.0)
            .collect()
    };
    for (nom, debut) in [("A", 4_800), ("B", frontiere + 4_800)] {
        let s = canal(debut);
        let p1000 = banc::puissance_a(&s, 48_000, 1000.0);
        let faux = banc::puissance_a(&s, 48_000, 1000.0 * 48_000.0 / 44_100.0)
            .max(banc::puissance_a(&s, 48_000, 500.0));
        assert!(p1000 > 100.0 * faux, "{nom} : {p1000} contre {faux}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_saut_en_pause_et_reprise_sans_perte_ni_recul() {
    let b = Banc::new();
    let a = b.compteur("a.wav", 48_000, 96_000, 0);
    b.sortie.play_media(&media(&a, "file:///a")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    b.sortie.pause().await.unwrap();
    let en_pause = statut(&b.sortie).await;
    assert_eq!(en_pause.state, TransportState::Paused);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let toujours = statut(&b.sortie).await;
    assert!(
        toujours.position_ms <= en_pause.position_ms + 2,
        "la position avance en pause : {} puis {}",
        en_pause.position_ms,
        toujours.position_ms
    );
    b.sortie.resume().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Saut pendant une seconde pause : on reste en pause, à la position.
    b.sortie.pause().await.unwrap();
    b.sortie.seek(1_500).await.unwrap();
    let saute = statut(&b.sortie).await;
    assert_eq!(saute.state, TransportState::Paused);
    assert_eq!(saute.position_ms, 1_500);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(statut(&b.sortie).await.position_ms, 1_500);
    b.sortie.resume().await.unwrap();
    attendre_la_fin(&b.sortie, Duration::from_secs(10)).await;
    let c = b.terminer();
    let p = paquets(&c);
    let pas_min = exiger_la_continuite(&p);
    let trames: Vec<i32> = p.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    let ruptures = ruptures_du_compteur(&trames);
    // Les reprises : premier paquet après un trou d'arrivée de plus de 100 ms.
    let reprises: Vec<(usize, u32)> = c
        .datagrammes
        .windows(2)
        .enumerate()
        .filter(|(_, d)| d[1].arrivee.duration_since(d[0].arrivee) > Duration::from_millis(100))
        .map(|(i, _)| (i + 1, p[i + 1].0.horodatage.wrapping_sub(p[i].0.horodatage)))
        .collect();
    println!(
        "pause : {} paquets ; pas min {pas_min} ; reprises (paquet, pas d'horodatage) \
         {reprises:?} ; ruptures du compteur {ruptures:?}",
        p.len()
    );
    // Première pause : rien perdu (aucune rupture) ; la seule rupture est
    // le saut, vers la trame 72 000.
    assert_eq!(ruptures.len(), 1, "{ruptures:?}");
    assert_eq!(ruptures[0].2, 72_000);
    assert_eq!(reprises.len(), 2, "{reprises:?}");
    for &(i, pas) in &reprises {
        assert!(p[i].0.marqueur, "reprise au paquet {i} non marquée");
        assert!(
            pas >= 48 * 100,
            "l'horloge média a avancé pendant la pause : +{pas}"
        );
    }
    let fin_utile = trames.iter().rposition(|&v| v != 0).unwrap();
    assert_eq!(trames[fin_utile], 95_999);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_suivante_preparee_en_cours_de_lecture_est_encore_enchainee() {
    // Comme le fait l'hôte : la suivante arrive PENDANT la lecture, ici
    // 600 ms avant la fin de A. Avant la correction, l'émetteur tirait tout
    // le PCM disponible dans une file sans fond : A était décodée en
    // quelques millisecondes, le producteur avait fini, la suivante n'était
    // jamais prise (vu au banc release : la 96 kHz n'a pas joué).
    let b = Banc::new();
    let a = b.compteur("a.wav", 48_000, 57_600, 0);
    let bb = b.compteur("b.wav", 48_000, 9_600, 1_000_000);
    b.sortie.play_media(&media(&a, "file:///a")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    b.sortie
        .set_next_media(&media(&bb, "file:///b"))
        .await
        .unwrap();
    let vus = attendre_la_fin(&b.sortie, Duration::from_secs(10)).await;
    let c = b.terminer();
    let p = paquets(&c);
    exiger_la_continuite(&p);
    let trames: Vec<i32> = p.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    let ruptures = ruptures_du_compteur(&trames);
    println!(
        "suivante tardive : {} paquets ; ruptures {ruptures:?} ; dernière URI {:?}",
        p.len(),
        vus.last().unwrap().current_uri
    );
    assert_eq!(
        ruptures,
        vec![(57_600, 57_599, 1_000_000)],
        "B enchaînée juste après A"
    );
    assert_eq!(
        vus.last().unwrap().current_uri.as_deref(),
        Some("file:///b")
    );
}
