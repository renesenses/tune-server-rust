//! Fil 3107 — témoins de la cadence plafonnée et du rééchantillonneur radio.
//!
//! Sécurité auditive (règle de Bertrand) : gain ≤ 1, aucun clic, aucun saut
//! de niveau. Les signaux traversent des CENTAINES de paquets de tailles
//! variables, jamais multiples du rapport, comme les paquets d'une radio.
use super::*;

const A: f32 = 0.5;

/// Les tailles de paquets d'une radio : pseudo-aléatoires, de 97 à 4 999
/// trames, jamais choisies multiples du rapport.
fn tailles_de_paquets(graine: u64) -> impl Iterator<Item = usize> {
    let mut x = graine;
    std::iter::from_fn(move || {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        Some(97 + ((x >> 33) % 4_903) as usize)
    })
}

/// Un sinus stéréo entrelacé (voie droite déphasée d'un quart de tour).
fn sinus(freq: f64, cadence: u32, trames: usize) -> Vec<f32> {
    let w = 2.0 * std::f64::consts::PI * freq / f64::from(cadence);
    let mut v = Vec::with_capacity(trames * 2);
    for n in 0..trames {
        let t = w * n as f64;
        v.push(A * t.sin() as f32);
        v.push(A * t.cos() as f32);
    }
    v
}

/// Passe le signal par paquets dans un rééchantillonneur radio.
fn par_paquets(signal: &[f32], de: u32, vers: u32) -> (Vec<f32>, usize) {
    let mut r = ReechantillonneurRadio::nouveau(de, vers, 2);
    assert_eq!(
        r.noyau(),
        "sinc",
        "{de} → {vers} : le sinc doit se construire"
    );
    let mut sortie = Vec::new();
    let mut paquets = 0;
    let mut pos = 0;
    let mut tailles = tailles_de_paquets(u64::from(de) ^ u64::from(vers));
    while pos < signal.len() {
        let n = (tailles.next().unwrap() * 2).min(signal.len() - pos);
        sortie.extend(r.traiter(&signal[pos..pos + n]));
        pos += n;
        paquets += 1;
    }
    assert_eq!(r.trames_recues() as usize, signal.len() / 2);
    (sortie, paquets)
}

/// La zone stable : sans la montée du filtre en tête ni la fin.
fn zone_stable(v: &[f32]) -> &[f32] {
    let marge = 4_096 * 2;
    &v[marge..v.len() - marge]
}

/// La pire dérivée seconde des deux voies (un clic la fait exploser).
fn pire_derivee_seconde(v: &[f32]) -> f32 {
    (0..2)
        .map(|voie| {
            let s: Vec<f32> = v.iter().skip(voie).step_by(2).copied().collect();
            s.windows(3)
                .map(|t| (t[2] - 2.0 * t[1] + t[0]).abs())
                .fold(0.0f32, f32::max)
        })
        .fold(0.0f32, f32::max)
}

fn rms(v: &[f32]) -> f64 {
    (v.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>() / v.len() as f64).sqrt()
}

const RAPPORTS: [(u32, u32); 3] = [(192_000, 48_000), (96_000, 48_000), (88_200, 44_100)];

#[test]
fn f3107_une_radio_192k_vers_un_sonos_reste_sous_son_plafond() {
    let plafond = crate::device_catalog::quirks_for("Sonos", "inconnu").max_sample_rate;
    assert_eq!(plafond, Some(48_000));
    let servi = cadence_de_sortie_radio(192_000, None, plafond);
    assert!(
        plafond.is_none_or(|p| servi <= p),
        "radio servie à {servi} Hz à un Sonos plafonné à {plafond:?} Hz"
    );
    assert_eq!(servi, 48_000);
}

#[test]
fn la_cadence_descend_dans_sa_famille_sous_le_plafond() {
    let p = Some(48_000);
    assert_eq!(cadence_de_sortie_radio(192_000, None, p), 48_000);
    assert_eq!(cadence_de_sortie_radio(96_000, None, p), 48_000);
    assert_eq!(cadence_de_sortie_radio(88_200, None, p), 44_100);
    assert_eq!(cadence_de_sortie_radio(176_400, None, p), 44_100);
    assert_eq!(cadence_de_sortie_radio(352_800, None, p), 44_100);
    assert_eq!(cadence_de_sortie_radio(384_000, None, Some(96_000)), 96_000);
    assert_eq!(cadence_de_sortie_radio(176_400, None, Some(96_000)), 88_200);
    // Hors famille : le plafond lui-même.
    assert_eq!(cadence_de_sortie_radio(64_000, None, p), 48_000);
    assert_eq!(cadence_de_sortie_radio(48_000, None, Some(44_100)), 44_100);
}

#[test]
fn une_station_a_44_1_ou_48_khz_est_inchangee_et_sans_reechantillonneur() {
    for source in [44_100, 48_000] {
        let sortie = cadence_de_sortie_radio(source, None, Some(48_000));
        assert_eq!(sortie, source);
        assert!(
            reechantillonneur_de_connexion(source, sortie, 2).is_none(),
            "{source} Hz : aucun rééchantillonneur"
        );
    }
    // Sans plafond : la station garde sa cadence, comme avant.
    assert_eq!(cadence_de_sortie_radio(192_000, None, None), 192_000);
    assert_eq!(cadence_de_sortie_radio(192_000, None, Some(0)), 192_000);
    // Le relèvement des HE-AAC à 22,05 kHz est inchangé.
    assert_eq!(cadence_de_sortie_radio(22_050, None, Some(48_000)), 44_100);
}

#[test]
fn la_frequence_fixe_l_emporte_et_passe_par_le_sinc() {
    assert_eq!(
        cadence_de_sortie_radio(44_100, Some(96_000), Some(48_000)),
        96_000
    );
    let r = reechantillonneur_de_connexion(44_100, 96_000, 2);
    assert_eq!(r.as_ref().map(ReechantillonneurRadio::noyau), Some("sinc"));
}

#[test]
fn un_reechantillonneur_neuf_a_chaque_connexion() {
    let mut r = reechantillonneur_de_connexion(192_000, 48_000, 2).unwrap();
    r.traiter(&sinus(1_000.0, 192_000, 1_000));
    assert_eq!(r.trames_recues(), 1_000);
    // La connexion suivante, même format : un neuf, fondu compris.
    let r = reechantillonneur_de_connexion(192_000, 48_000, 2).unwrap();
    assert_eq!(r.trames_recues(), 0, "reconnexion : rééchantillonneur neuf");
}

/// Un sinus de `freq` qui commence à la phase `phase` (radians).
fn sinus_dephase(freq: f64, cadence: u32, trames: usize, phase: f64) -> Vec<f32> {
    let w = 2.0 * std::f64::consts::PI * freq / f64::from(cadence);
    let mut v = Vec::with_capacity(trames * 2);
    for n in 0..trames {
        let t = phase + w * n as f64;
        v.push(A * t.sin() as f32);
        v.push(A * t.cos() as f32);
    }
    v
}

fn crete(v: &[f32]) -> f32 {
    v.iter().fold(0.0f32, |m, x| m.max(x.abs()))
}

/// RECONNEXION DÉPHASÉE (relecture du fil 3107) : la station reprend
/// ailleurs dans l'onde. La nouvelle connexion part d'un rééchantillonneur
/// neuf : crête jamais au-dessus de la source, et son fondu se voit en tête.
/// Contre-épreuve : garder l'ancien filtre fait dépasser la source.
#[test]
fn une_reconnexion_dephasee_ne_depasse_jamais_la_source() {
    let (de, vers) = (192_000, 48_000);
    let avant = sinus(1_000.0, de, de as usize);
    let apres = sinus_dephase(1_000.0, de, de as usize, std::f64::consts::FRAC_PI_2);

    let mut premiere = reechantillonneur_de_connexion(de, vers, 2).unwrap();
    let sortie_avant = premiere.traiter(&avant);
    let mut seconde = reechantillonneur_de_connexion(de, vers, 2).unwrap();
    let sortie_apres = seconde.traiter(&apres);
    let pire = crete(&sortie_avant).max(crete(&sortie_apres));
    println!("reconnexion déphasée, rééchantillonneur neuf : crête {pire:.6}");
    assert!(
        pire <= A,
        "reconnexion : crête {pire} au-dessus de la source {A}"
    );

    // Le fondu : les 2 premières ms de la nouvelle connexion restent basses
    // (gain du cosinus surélevé ≤ 0,1 à 2 ms sur 10).
    let tete = &sortie_apres[..(vers as usize / 500) * 2];
    let crete_tete = crete(tete);
    println!("reconnexion déphasée : crête des 2 premières ms {crete_tete:.4}");
    assert!(crete_tete < 0.15 * A, "pas de fondu en tête : {crete_tete}");

    // Contre-épreuve : le même filtre à travers la reconnexion.
    let colle = premiere.traiter(&apres);
    let pire_colle = crete(&colle);
    println!("reconnexion déphasée, filtre gardé : crête {pire_colle:.6}");
    assert!(pire_colle > A, "contre-épreuve : {pire_colle} ≤ {A}");
}

#[test]
fn le_bit_perfect_strict_refuse_la_station_plafonnee() {
    let sortie = cadence_de_sortie_radio(192_000, None, Some(48_000));
    let refus = crate::audio::bitperfect_strict::decision_bitperfect(192_000, sortie, true)
        .refus()
        .expect("strict : la conversion 192 → 48 kHz est refusée");
    assert!(!refus.sentinelle().is_empty());
    // Non strict : la station est jouée, convertie.
    assert!(
        crate::audio::bitperfect_strict::decision_bitperfect(192_000, sortie, false)
            .refus()
            .is_none()
    );
}

/// CONTINUITÉ : par paquets ou d'un bloc, la sortie est la même à
/// l'échantillon près (aucun effet de frontière de paquet), et la dérivée
/// seconde d'un sinus de 1 kHz ne dépasse jamais sa valeur théorique.
#[test]
fn continuite_sur_des_centaines_de_paquets_sans_clic() {
    for (de, vers) in RAPPORTS {
        let signal = sinus(1_000.0, de, 600_000);
        let (sortie, paquets) = par_paquets(&signal, de, vers);
        assert!(
            paquets >= 200,
            "{de} → {vers} : {paquets} paquets seulement"
        );

        // Rien de perdu : au bloc de rubato et au délai du filtre près.
        let attendu = (signal.len() / 2) as f64 * f64::from(vers) / f64::from(de);
        let recu = (sortie.len() / 2) as f64;
        assert!(
            (attendu - recu).abs() < 1_100.0,
            "{de} → {vers} : {recu} trames pour {attendu} attendues"
        );

        // Même calcul d'un seul bloc : aucune frontière de paquet ne se voit.
        let mut bloc = ReechantillonneurRadio::nouveau(de, vers, 2);
        let d_un_bloc = bloc.traiter(&signal);
        let n = sortie.len().min(d_un_bloc.len());
        let ecart = sortie[..n]
            .iter()
            .zip(&d_un_bloc[..n])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            ecart < 1e-6,
            "{de} → {vers} : écart {ecart} entre paquets et bloc"
        );

        // Dérivée seconde bornée par A·ω² (+5 %) : un clic la ferait exploser.
        let w = 2.0 * std::f64::consts::PI * 1_000.0 / f64::from(vers);
        let borne = (f64::from(A) * w * w * 1.05) as f32;
        let pire = pire_derivee_seconde(zone_stable(&sortie));
        println!("{de} → {vers} : {paquets} paquets, dérivée seconde {pire:.5} ≤ {borne:.5}");
        assert!(
            pire <= borne,
            "{de} → {vers} : discontinuité {pire} > {borne}"
        );

        // Contre-épreuve : l'ancien chemin, paquet par paquet, fait des clics
        // que la même mesure voit.
        let mut ancien = Vec::new();
        let mut pos = 0;
        let mut tailles = tailles_de_paquets(u64::from(de) ^ u64::from(vers));
        while pos < signal.len() {
            let n = (tailles.next().unwrap() * 2).min(signal.len() - pos);
            ancien.extend(crate::audio::simple_resample(
                &signal[pos..pos + n],
                de,
                vers,
                2,
            ));
            pos += n;
        }
        let pire_ancien = pire_derivee_seconde(zone_stable(&ancien));
        println!("{de} → {vers} : ancien chemin linéaire, dérivée seconde {pire_ancien:.5}");
        assert!(
            pire_ancien > borne,
            "contre-épreuve : {pire_ancien} ≤ {borne}"
        );
    }
}

/// NIVEAU : RMS à ±0,1 dB de la source, crête jamais au-dessus de la source.
#[test]
fn niveau_tenu_a_0_1_db_et_crete_jamais_au_dessus() {
    for (de, vers) in RAPPORTS {
        for freq in [100.0, 1_000.0, 10_000.0] {
            let signal = sinus(freq, de, de as usize * 2);
            let (sortie, _) = par_paquets(&signal, de, vers);
            let stable = zone_stable(&sortie);
            let ecart_db = 20.0 * (rms(stable) / rms(zone_stable(&signal))).log10();
            assert!(
                ecart_db.abs() <= 0.1,
                "{de} → {vers}, {freq} Hz : niveau {ecart_db:+.4} dB"
            );
            let crete = sortie.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            println!("{de} → {vers}, {freq} Hz : niveau {ecart_db:+.5} dB, crête {crete:.6}");
            assert!(
                crete <= A,
                "{de} → {vers}, {freq} Hz : crête {crete} au-dessus de la source {A}"
            );
        }
    }
}

/// REPLIEMENT : une raie à 30 kHz n'a pas sa place sous 24 kHz. Le sinc
/// l'atténue d'au moins 60 dB ; l'interpolation linéaire d'avant la repliait
/// à 18 kHz presque intacte (contre-épreuve).
#[test]
fn une_raie_a_30_khz_est_atteinte_d_au_moins_60_db_en_192_vers_48() {
    let signal = sinus(30_000.0, 192_000, 192_000 * 2);
    let (sortie, _) = par_paquets(&signal, 192_000, 48_000);
    let attenuation = 20.0 * (rms(zone_stable(&sortie)) / rms(zone_stable(&signal))).log10();
    println!("raie à 30 kHz, 192 → 48 kHz : {attenuation:.1} dB");
    assert!(attenuation <= -60.0, "raie à 30 kHz : {attenuation:.1} dB");

    let lineaire = crate::audio::simple_resample(&signal, 192_000, 48_000, 2);
    let avant = 20.0 * (rms(zone_stable(&lineaire)) / rms(zone_stable(&signal))).log10();
    println!("raie à 30 kHz, ancien chemin linéaire : {avant:.1} dB");
    assert!(
        avant > -20.0,
        "contre-épreuve : le linéaire repliait ({avant:.1} dB)"
    );
}

/// COÛT : 60 s de 192 kHz stéréo vers 48 kHz, en paquets de radio. À lancer
/// en `--release -- --ignored --nocapture` ; il imprime la part du temps réel.
#[test]
#[ignore = "mesure de coût, à lancer en release"]
fn cout_192k_vers_48k_stereo() {
    let signal = sinus(1_000.0, 192_000, 192_000 * 60);
    let debut = std::time::Instant::now();
    let (sortie, paquets) = par_paquets(&signal, 192_000, 48_000);
    let calcul = debut.elapsed();
    let part = calcul.as_secs_f64() / 60.0;
    println!(
        "cout_192k_vers_48k_stereo : {paquets} paquets, {} trames, {:.1} ms pour 60 s, \
         {:.4} % du temps réel ({:.0}× plus vite)",
        sortie.len() / 2,
        calcul.as_secs_f64() * 1e3,
        part * 100.0,
        1.0 / part
    );
}

// ───────────── le plafond est-il BRANCHÉ dans `resolve_direct` ? ─────────────

/// Un WAV 16 bits stéréo de `secondes` à `cadence`, sinus de 1 kHz.
fn wav_de_station(cadence: u32, secondes: u32) -> Vec<u8> {
    let trames = (cadence * secondes) as usize;
    let donnees = (trames * 4) as u32;
    let mut w = Vec::with_capacity(44 + donnees as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + donnees).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&cadence.to_le_bytes());
    w.extend_from_slice(&(cadence * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&donnees.to_le_bytes());
    for x in sinus(1_000.0, cadence, trames) {
        w.extend_from_slice(&((x * 16_000.0) as i16).to_le_bytes());
    }
    w
}

/// Une station locale qui sert ce WAV à chaque connexion.
fn station_wav(cadence: u32) -> String {
    let corps = std::sync::Arc::new(wav_de_station(cadence, 4));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        // Un fil par connexion : un client qui ne lit plus (le décodeur dont
        // personne ne vide la session) ne doit pas bloquer les suivants.
        for mut s in listener.incoming().flatten() {
            let corps = corps.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(&corps);
                let _ = s.flush();
            });
        }
    });
    format!("http://{addr}/live.wav")
}

fn orchestrateur() -> crate::orchestrator::PlaybackOrchestrator {
    use std::sync::Arc;
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    crate::orchestrator::PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// La cadence que la session de la radio annonce au lecteur (en-tête WAV).
async fn cadence_servie(
    orch: &crate::orchestrator::PlaybackOrchestrator,
    zone_id: i64,
    sortie: &str,
    station: &str,
) -> (u32, u16) {
    let req = crate::orchestrator::PlayRequest {
        zone_id,
        output_device_id: Some(sortie.to_owned()),
        track_id: None,
        source: Some("radio".into()),
        source_id: Some(station.into()),
        title: Some("Station 3107".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
    };
    let resolu = orch.resolve_direct_url(&req).await.expect("résolution");
    let sid = resolu
        .stream_id
        .expect("une radio servie passe par une session");
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session");
    let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(format) = session.detected_output_format() {
            // Fermer la session : le décodeur, bloqué sur un canal que
            // personne ne vide, s'arrête, et le runtime du test peut finir.
            drop(session);
            orch.streamer.remove_session(&sid).await;
            return format;
        }
        assert!(tokio::time::Instant::now() < fin, "format jamais publié");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Le plafond est BRANCHÉ : une station 96 kHz servie à une zone DLNA
/// identifiée Sonos part à 48 kHz. Si `resolve_direct` passait `None` au
/// décodeur, elle partirait à 96 kHz et ce témoin tomberait. Contre-épreuve :
/// la même station vers une zone sans plafond garde ses 96 kHz.
/// Runtime à part, arrêté sans attendre : les décodeurs radio tournent dans
/// `spawn_blocking` et ne s'arrêtent qu'avec leur station ; un runtime de test
/// ordinaire les attendrait sans fin à sa fermeture.
#[test]
fn le_plafond_de_la_zone_atteint_le_decodeur_radio_dlna() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    // Un échec doit ROUGIR, pas pendre : la panique est rattrapée, le
    // runtime arrêté sans attendre, puis la panique relancée.
    let issue = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(plafond_branche())
    }));
    rt.shutdown_background();
    if let Err(panique) = issue {
        std::panic::resume_unwind(panique);
    }
}

async fn plafond_branche() {
    let station = station_wav(96_000);
    let orch = orchestrateur();
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone());
    let sonos = zones
        .create("Sonos", Some("dlna"), Some("dlna:uuid-f3107-sonos"))
        .unwrap();
    let sans_plafond = zones
        .create("Sans plafond", Some("dlna"), Some("dlna:uuid-f3107-libre"))
        .unwrap();
    let reglages = crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone());
    reglages
        .set(&format!("zone_{sonos}_brand"), "Sonos")
        .unwrap();
    reglages.set(&format!("zone_{sonos}_model"), "One").unwrap();
    assert_eq!(
        crate::audio::frequence_fixe::plafond_de_zone(&orch.db, sonos),
        Some(48_000)
    );

    let servi = cadence_servie(&orch, sonos, "dlna:uuid-f3107-sonos", &station).await;
    assert_eq!(
        servi,
        (48_000, 2),
        "zone Sonos : la station doit partir à 48 kHz"
    );
    let libre = cadence_servie(&orch, sans_plafond, "dlna:uuid-f3107-libre", &station).await;
    assert_eq!(libre, (96_000, 2), "contre-épreuve : sans plafond, 96 kHz");
}
