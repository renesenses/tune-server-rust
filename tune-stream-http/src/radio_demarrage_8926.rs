//! #8926 — le banc du démarrage d'une radio vers un renderer DLNA.
//!
//! Le terrain (.18, rc12, 17/11 11:25:27, FIP AAC → Eversolo DMP-A8, client
//! `Lavf/58.45.100`) :
//!
//! - 27.696 session créée ; 28.200 connecté à icecast (504 ms) ;
//! - 28.278 réserve de 3 s remplie (#1628) ; 28.310 `Play` parti ;
//! - 29.009 le renderer ouvre `bytes=0-`… en tire 1 058 924 octets en
//!   1 490 ms (5,5 s d'audio) puis ferme : c'est une SONDE ;
//! - 30.529 il revient avec `bytes=0-` : la vraie lecture, qui ne recevait que
//!   le direct.
//!
//! Ce banc rejoue la chaîne de production SANS AUCUN SON : un faux icecast
//! sur la boucle locale (latence de connexion et rafale réglables, puis débit
//! réel), le vrai décodeur radio, la vraie barrière de 3 s, le vrai corps HTTP
//! (`router`), et un faux renderer qui lit comme `Lavf` (sonde, puis lecture,
//! ICY demandé). Il note, depuis la demande de lecture, l'instant où la
//! connexion qui JOUE a reçu N secondes d'audio.
//!
//! Les chiffres de la PR sont ceux de `--nocapture`.
use super::router;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tune_core::http::streamer::{AudioStreamer, ICY_METAINT, StreamInfo, StreamSession};

const LAVF: &str = "Lavf/58.45.100";
const MP3: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tune-core/tests/fixtures/test.mp3"
));
/// Début des trames MPEG de la fixture (après l'ID3v2 de 45 octets).
const DEBUT_DES_TRAMES: usize = 45;
/// 192 kbit/s : le débit réel de la fixture, celui de la station factice.
const OCTETS_MP3_PAR_SECONDE: usize = 24_000;
/// PCM servi : 44,1 kHz, 16 bits, stéréo.
const OCTETS_PCM_PAR_SECONDE: usize = 44_100 * 4;
/// Ce que la sonde du DMP-A8 a emporté le 17/11, et en combien de temps.
const SONDE_OCTETS: usize = 1_058_924;
const SONDE_DUREE: Duration = Duration::from_millis(1_490);
/// Du `Play` à la première requête du renderer, le 17/11 (28.310 → 29.009).
const DELAI_DU_RENDERER: Duration = Duration::from_millis(700);

/// Une station icecast sur un port éphémère de la boucle locale : elle répond
/// après `latence`, envoie d'un trait `rafale` secondes de MP3 (le
/// « burst-on-connect » d'icecast), puis le reste au débit réel, sans fin.
fn icecast_factice(latence: Duration, rafale: Duration) -> String {
    let ecoute = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let adresse = ecoute.local_addr().expect("adresse");
    std::thread::spawn(move || {
        for flux in ecoute.incoming().flatten() {
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut flux = flux;
                let mut requete = [0u8; 2048];
                let _ = flux.read(&mut requete);
                std::thread::sleep(latence);
                if write!(
                    flux,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nicy-br: 192\r\n\r\n"
                )
                .is_err()
                {
                    return;
                }
                let trames = &MP3[DEBUT_DES_TRAMES..];
                let mut position = 0usize;
                let mut envoyer = |n: usize, flux: &mut std::net::TcpStream| -> bool {
                    let mut reste = n;
                    while reste > 0 {
                        let bout = (trames.len() - position).min(reste);
                        if flux.write_all(&trames[position..position + bout]).is_err() {
                            return false;
                        }
                        position = (position + bout) % trames.len();
                        reste -= bout;
                    }
                    true
                };
                let rafale = (rafale.as_secs_f64() * OCTETS_MP3_PAR_SECONDE as f64) as usize;
                if !envoyer(rafale, &mut flux) {
                    return;
                }
                let depart = Instant::now();
                let mut envoye = 0usize;
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                    let du =
                        (depart.elapsed().as_secs_f64() * OCTETS_MP3_PAR_SECONDE as f64) as usize;
                    if du > envoye {
                        if !envoyer(du - envoye, &mut flux) {
                            return;
                        }
                        envoye = du;
                    }
                }
            });
        }
    });
    format!("http://{adresse}/fip.mp3")
}

/// Ce que le faux renderer a reçu sur la connexion qui joue.
struct Lecture {
    /// Le PCM, en-tête WAV et blocs ICY retirés.
    pcm: Vec<u8>,
    /// Instant (depuis la demande de lecture) où chaque seconde d'audio est
    /// arrivée : `[0]` = 1 s reçue, `[1]` = 2 s, …
    secondes: Vec<Duration>,
    /// Blocs ICY rencontrés, et leur charge lisible.
    blocs_icy: Vec<String>,
}

/// Le faux renderer : un GET façon `Lavf` (`bytes=0-`, `Icy-MetaData: 1`).
/// Lit jusqu'à `au_plus` octets ou pendant `pendant`, puis ferme.
async fn get_lavf(
    url: &str,
    t0: Instant,
    au_plus: usize,
    pendant: Duration,
    secondes_voulues: usize,
) -> Lecture {
    let mut reponse = tune_core::http::client::builder()
        .build()
        .expect("client")
        .get(url)
        .header("User-Agent", LAVF)
        .header("Range", "bytes=0-")
        .header("Icy-MetaData", "1")
        .send()
        .await
        .expect("réponse du flux");
    assert_eq!(
        reponse
            .headers()
            .get("icy-metaint")
            .and_then(|v| v.to_str().ok()),
        Some(ICY_METAINT.to_string().as_str()),
        "le canal ICY doit être ouvert à la connexion"
    );
    let debut = Instant::now();
    let mut brut: Vec<u8> = Vec::new();
    let mut lecture = Lecture {
        pcm: Vec::new(),
        secondes: Vec::new(),
        blocs_icy: Vec::new(),
    };
    // Découpe ICY du côté du renderer : `ICY_METAINT` octets de corps (en-tête
    // WAV compris), un octet de longueur, la charge, et ainsi de suite.
    let mut curseur = 0usize;
    let mut dans_la_fenetre = 0usize;
    let mut corps_audio: Vec<u8> = Vec::new();
    while brut.len() < au_plus && debut.elapsed() < pendant {
        let reste = pendant.saturating_sub(debut.elapsed());
        let morceau = match tokio::time::timeout(reste, reponse.chunk()).await {
            Err(_) => break,
            Ok(Ok(Some(m))) => m,
            Ok(Ok(None)) => panic!("le flux radio s'est terminé"),
            Ok(Err(e)) => panic!("flux interrompu : {e}"),
        };
        brut.extend_from_slice(&morceau);
        loop {
            if dans_la_fenetre < ICY_METAINT {
                let n = (ICY_METAINT - dans_la_fenetre).min(brut.len() - curseur);
                if n == 0 {
                    break;
                }
                corps_audio.extend_from_slice(&brut[curseur..curseur + n]);
                curseur += n;
                dans_la_fenetre += n;
            } else {
                let Some(&l) = brut.get(curseur) else { break };
                let taille = usize::from(l) * 16;
                if brut.len() < curseur + 1 + taille {
                    break;
                }
                let charge = &brut[curseur + 1..curseur + 1 + taille];
                lecture.blocs_icy.push(
                    String::from_utf8_lossy(charge)
                        .trim_end_matches('\0')
                        .to_string(),
                );
                curseur += 1 + taille;
                dans_la_fenetre = 0;
            }
        }
        let pcm = corps_audio.len().saturating_sub(44);
        while lecture.secondes.len() < secondes_voulues
            && pcm >= (lecture.secondes.len() + 1) * OCTETS_PCM_PAR_SECONDE
        {
            lecture.secondes.push(t0.elapsed());
        }
        if lecture.secondes.len() >= secondes_voulues && au_plus == usize::MAX {
            break;
        }
    }
    assert!(corps_audio.len() >= 44, "l'en-tête WAV doit arriver");
    assert_eq!(
        &corps_audio[..4],
        b"RIFF",
        "le corps commence par l'en-tête WAV"
    );
    lecture.pcm = corps_audio[44..].to_vec();
    lecture
}

struct Mesure {
    connexion_station: Duration,
    play: Duration,
    lecture: Lecture,
    sonde: Option<Lecture>,
}

/// La chaîne de `servir_la_radio_au_reseau` + `initial_prebuffer` + `Play`.
async fn demarrer(latence: Duration, rafale: Duration, sonde: bool, secondes: usize) -> Mesure {
    let station = icecast_factice(latence, rafale);
    let streamer = Arc::new(AudioStreamer::new(0));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    let sessions = streamer.sessions_state();
    tokio::spawn(async move {
        axum::serve(ecoute, router(sessions)).await.ok();
    });

    let t0 = Instant::now();
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..Default::default()
    };
    let (sid, tx, data_ready, session) = streamer.create_radio_session(info, 256).await;
    tune_core::http::streamer::publish_radio_now(
        &sid,
        Some("Kareen Guiock-Thuram".into()),
        "Mister Bojangles".into(),
        None,
    );
    let pret = data_ready.clone();
    let pour_le_decodeur = session.clone();
    // Un fil à part et non `spawn_blocking` : la station ne finit jamais, et
    // l'arrêt du runtime du test attendrait sans fin une tâche bloquante. Le
    // fil s'arrête de lui-même quand le runtime disparaît.
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let _dans_le_runtime = runtime.enter();
        tune_core::orchestrator::banc_radio_8926::decoder_la_station(
            station,
            tx,
            data_ready,
            pour_le_decodeur,
        )
    });
    // `radio_local_decode_started` : le format sondé est publié.
    let _ = tokio::time::timeout(Duration::from_secs(10), pret.notified()).await;
    let connexion_station = t0.elapsed();
    assert!(
        streamer
            .wait_radio_prefill_ready(&sid, 3, Duration::from_secs(8))
            .await,
        "la réserve de 3 s (#1628) doit se remplir"
    );
    let play = t0.elapsed();
    tokio::time::sleep(DELAI_DU_RENDERER).await;
    let url = format!("http://127.0.0.1:{port}/stream/{sid}.wav");
    let sonde = if sonde {
        Some(get_lavf(&url, t0, SONDE_OCTETS, SONDE_DUREE, 0).await)
    } else {
        None
    };
    let lecture = get_lavf(&url, t0, usize::MAX, Duration::from_secs(20), secondes).await;
    let _garde = session;
    Mesure {
        connexion_station,
        play,
        lecture,
        sonde,
    }
}

fn ms(d: Duration) -> u128 {
    d.as_millis()
}

fn rapport(nom: &str, m: &Mesure) -> String {
    let secondes: Vec<String> = m
        .lecture
        .secondes
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} s reçues à {} ms", i + 1, ms(*t)))
        .collect();
    format!(
        "[banc 8926] {nom} : station connectée {} ms, Play {} ms, sonde {} octets ; {}",
        ms(m.connexion_station),
        ms(m.play),
        m.sonde.as_ref().map_or(0, |s| s.pcm.len()),
        secondes.join(", ")
    )
}

/// LE défaut du 17/11 : latence de 500 ms, rafale de 3 s (FIP), sonde `Lavf`
/// puis lecture. La lecture doit recevoir l'avance d'un trait : 4 s d'audio
/// en moins de 500 ms après sa connexion, au lieu de 4 s au rythme réel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_lecture_qui_suit_la_sonde_recoit_l_avance_d_un_trait() {
    let m = demarrer(Duration::from_millis(500), Duration::from_secs(3), true, 6).await;
    eprintln!(
        "{}",
        rapport("FIP 17/11 (latence 500 ms, rafale 3 s, sonde Lavf)", &m)
    );
    let sonde = m.sonde.as_ref().expect("sonde");
    let debut_lecture = m.play + DELAI_DU_RENDERER + SONDE_DUREE;
    let quatre = m.lecture.secondes[3];
    assert!(
        quatre < debut_lecture + Duration::from_millis(1_200),
        "4 s d'audio reçues {} ms après la connexion de lecture : l'avance emportée par la \
         sonde ({} octets) n'a pas été rejouée",
        ms(quatre.saturating_sub(debut_lecture)),
        sonde.pcm.len()
    );
    // Rejouée À L'OCTET depuis le début : la lecture commence par exactement
    // ce que la sonde avait reçu.
    let n = sonde.pcm.len().min(m.lecture.pcm.len());
    assert!(
        n > OCTETS_PCM_PAR_SECONDE,
        "la sonde a reçu plus d'une seconde"
    );
    assert!(
        sonde.pcm[..n] == m.lecture.pcm[..n],
        "la lecture doit reprendre au premier octet de PCM de la sonde"
    );
    // ICY intact : chaque bloc se relit, et porte le titre publié.
    assert!(!m.lecture.blocs_icy.is_empty());
    for bloc in &m.lecture.blocs_icy {
        assert!(
            bloc.starts_with("StreamTitle='Kareen Guiock-Thuram - Mister Bojangles';"),
            "bloc ICY illisible : {bloc:?}"
        );
    }
}

/// Ni trou ni doublon à la jonction entre l'avance rejouée et le direct : le
/// PCM de la lecture qui suit une sonde est, octet pour octet, celui d'un
/// renderer qui ne sonde pas (même station, même décodeur, même début). Et
/// pas de dérive ensuite : l'avance du renderer ne fond pas.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ni_trou_ni_doublon_ni_derive_apres_la_rafale() {
    let (avec_sonde, sans_sonde) = tokio::join!(
        demarrer(Duration::from_millis(500), Duration::from_secs(3), true, 9),
        demarrer(Duration::from_millis(500), Duration::from_secs(3), false, 9),
    );
    eprintln!("{}", rapport("contre-épreuve sans sonde", &sans_sonde));
    eprintln!("{}", rapport("avec sonde", &avec_sonde));
    let n = avec_sonde
        .lecture
        .pcm
        .len()
        .min(sans_sonde.lecture.pcm.len());
    assert!(
        n >= 8 * OCTETS_PCM_PAR_SECONDE,
        "au moins 8 s comparées ({n} octets)"
    );
    if let Some(i) = (0..n).find(|&i| avec_sonde.lecture.pcm[i] != sans_sonde.lecture.pcm[i]) {
        panic!(
            "le PCM diverge à l'octet {i} ({:.2} s) : trou ou doublon à la jonction",
            i as f64 / OCTETS_PCM_PAR_SECONDE as f64
        );
    }
    // Pas de dérive : après la rafale, la neuvième seconde arrive au rythme
    // du direct — ni en avance (ce qui voudrait dire un doublon), ni avec du
    // retard sur le renderer qui n'a pas sondé.
    let s = &avec_sonde.lecture.secondes;
    let r = &sans_sonde.lecture.secondes;
    assert!(
        s[8] <= r[8] + Duration::from_millis(400),
        "la 9e seconde arrive {} ms après celle du renderer sans sonde : l'avance a fondu",
        ms(s[8].saturating_sub(r[8]))
    );
}

/// Contre-épreuve : la PREMIÈRE connexion ne reçoit rien de plus qu'avant —
/// la tête est vide, c'est le canal qu'elle lit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_premiere_connexion_ne_change_pas() {
    let m = demarrer(Duration::from_millis(500), Duration::from_secs(3), false, 4).await;
    eprintln!("{}", rapport("renderer sans sonde", &m));
    let connexion = m.play + DELAI_DU_RENDERER;
    assert!(
        m.lecture.secondes[2] < connexion + Duration::from_millis(500),
        "la réserve de 3 s doit arriver d'un trait à la première connexion"
    );
}

/// #8926 — sortie de radio, de bout en bout sur la boucle locale : un
/// renderer qui garde sa connexion et ne lit plus (canal immobile 1 s). Ce
/// banc ne reproduit PAS le blocage du .18 : sur la boucle locale, le réveil
/// du congé suffit à faire relancer le corps par hyper, qui part en ~20 ms.
/// Il garde que la sortie ne régresse pas sur ce chemin. Le blocage du .18
/// (corps jamais relancé, `consommateurs_restants=1` après 2 s) est modelé
/// dans `sortie_de_radio_dlna_6130_tests`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sortie_de_radio_un_renderer_qui_ne_lit_plus_ne_retient_pas_la_sortie() {
    let (attente, depasse, ne_lit_plus) = sortie_de_radio(false).await;
    eprintln!(
        "[banc 8926] sortie de radio, renderer arrêté (boucle locale) : attente {} ms, \
         delai_depasse={depasse}, renderer_ne_lit_plus={ne_lit_plus}",
        ms(attente)
    );
    assert!(!depasse, "l'attente ne doit pas épuiser le délai de 2 s");
    assert!(
        attente < Duration::from_millis(700),
        "attente de {} ms",
        ms(attente)
    );
}

/// Contre-épreuve : un renderer qui lit encore voit le congé tout de suite
/// (les 20 ms du journal du .18) — inchangé, et ce n'est pas le nouveau cas.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sortie_de_radio_un_renderer_qui_lit_encore_part_tout_de_suite() {
    let (attente, depasse, ne_lit_plus) = sortie_de_radio(true).await;
    eprintln!(
        "[banc 8926] sortie de radio, renderer qui lit : attente {} ms, \
         delai_depasse={depasse}, renderer_ne_lit_plus={ne_lit_plus}",
        ms(attente)
    );
    assert!(!depasse && !ne_lit_plus);
    assert!(
        attente < Duration::from_millis(300),
        "attente de {} ms",
        ms(attente)
    );
}

/// Une radio servie à un renderer qui lit (`lit`) ou qui s'est arrêté de lire
/// en gardant sa connexion ouverte ; puis le congé et l'attente de
/// `dlna_sortie_de_radio`.
async fn sortie_de_radio(lit: bool) -> (Duration, bool, bool) {
    let streamer = Arc::new(AudioStreamer::new(0));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    let sessions = streamer.sessions_state();
    tokio::spawn(async move {
        axum::serve(ecoute, router(sessions)).await.ok();
    });
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 48_000,
        bit_depth: 16,
        channels: 2,
        ..Default::default()
    };
    let (sid, tx, _pret, session) = streamer.create_radio_session(info, 64).await;
    session.publish_detected_pcm_format(48_000, 2, 16);
    // Le décodeur : du PCM aussi vite que le canal l'accepte.
    let envoyes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let compte = envoyes.clone();
    tokio::spawn(async move {
        let bloc = vec![0x11u8; 32_768];
        while tx.send(bloc.clone()).await.is_ok() {
            compte.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let mut reponse = tune_core::http::client::builder()
        .build()
        .unwrap()
        .get(format!("http://127.0.0.1:{port}/stream/{sid}.wav"))
        .header("User-Agent", LAVF)
        .header("Range", "bytes=0-")
        .send()
        .await
        .unwrap();
    let _ = reponse.chunk().await;
    // La connexion du renderer arrêté reste ouverte jusqu'à la fin du test :
    // la réponse est tenue ici, puis rendue après le verdict.
    let mut reponse_tenue = None;
    let lecteur = if lit {
        Some(tokio::spawn(async move {
            while let Ok(Some(_)) = reponse.chunk().await {}
        }))
    } else {
        // Le renderer a reçu `Stop` : il garde la connexion et ne lit plus.
        // On attend que plus rien ne sorte du canal pendant 1 s : tampons
        // TCP pleins, hyper ne relance plus le corps.
        let debut = Instant::now();
        let mut vu = envoyes.load(std::sync::atomic::Ordering::SeqCst);
        let mut immobile_depuis = Instant::now();
        while immobile_depuis.elapsed() < Duration::from_secs(1) {
            assert!(
                debut.elapsed() < Duration::from_secs(30),
                "le corps devrait finir par ne plus rien tirer du canal"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            let n = envoyes.load(std::sync::atomic::Ordering::SeqCst);
            if n != vu {
                vu = n;
                immobile_depuis = Instant::now();
            }
        }
        assert!(
            session.consommateurs_radio_tous_en_ecriture(),
            "le corps immobile doit être noté arrêté sur l'écriture"
        );
        reponse_tenue = Some(reponse);
        None
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    session.congedier_les_consommateurs();
    let sessions: Vec<Arc<StreamSession>> = vec![session.clone()];
    let resultat = tune_core::orchestrator::banc_radio_8926::attendre_le_depart(
        &sessions,
        tune_core::orchestrator::banc_radio_8926::DELAI_FERMETURE_RADIO,
    )
    .await;
    if let Some(l) = lecteur {
        l.abort();
    }
    drop(reponse_tenue);
    resultat
}
