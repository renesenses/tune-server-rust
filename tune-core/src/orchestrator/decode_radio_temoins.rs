//! Témoins de `decode_radio_stream_to_pcm` : un flux radio factice, décodé
//! JUSQU'AU PCM (REF-2 phase 2, #2219).
//!
//! Avant ces témoins, la seule épreuve qui traversait le décodeur de
//! production s'arrêtait à la porte HLS, avant le moindre octet de réseau.
//! Rien ne prouvait que la boucle `'reconnect` — connexion, sonde
//! Symphonia, décodage, découpe en morceaux de 32 768 octets, réveil du
//! consommateur, reconnexion en fin de flux — rende ce qu'elle rend. Ce
//! fichier sert la fixture `tests/fixtures/test.mp3` (44,1 kHz stéréo,
//! 192 kbit/s) depuis un serveur HTTP local et regarde ce qui sort.
//!
//! Chaque témoin est BORNÉ : un décodeur qui ne rend plus la main fait
//! tomber le test sur un message nommé, jamais sur un blocage de la CI.
use super::{RADIO_NOT_AUDIO, decode_radio_stream_to_pcm};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MP3: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test.mp3"
));

/// Une « station » sur un port éphémère : à la n-ième connexion elle sert la
/// n-ième réponse (type MIME, corps), puis répète la dernière indéfiniment.
/// Rend l'URL et le compteur de connexions acceptées. Pas de
/// `Content-Length` : comme une vraie radio, le corps finit quand le serveur
/// ferme.
fn station_factice(reponses: Vec<(&'static str, Vec<u8>)>) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connexions = Arc::new(AtomicUsize::new(0));
    let compteur = connexions.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut s in listener.incoming().flatten() {
            let n = compteur.fetch_add(1, Ordering::SeqCst);
            let (content_type, corps) = &reponses[n.min(reponses.len() - 1)];
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
            );
            let _ = s.write_all(corps);
            let _ = s.flush();
        }
    });
    (format!("http://{addr}/live"), connexions)
}

/// La session de production, pour `publish_detected_output_format`. Le canal
/// PCM et le réveil sont créés par chaque témoin : `create_radio_session`
/// garde un émetteur de maintien à l'intérieur de la session, et c'est la
/// chute du RÉCEPTEUR, tenu par le test, qui doit dire au décodeur que le
/// consommateur est parti.
async fn session_radio() -> Arc<crate::http::streamer::StreamSession> {
    use crate::http::streamer::{AudioStreamer, StreamInfo};
    let streamer = Arc::new(AudioStreamer::new(0));
    let info = StreamInfo {
        format: "wav".to_string(),
        mime_type: "audio/wav".to_string(),
        ..StreamInfo::default()
    };
    let (_id, _tx, _data_ready, session) = streamer.create_radio_session(info, 4).await;
    session
}

/// Le chemin nominal : la station sert du MP3, le décodeur publie le format
/// réellement sondé, réveille le consommateur au premier morceau, sert des
/// morceaux de 32 768 octets de PCM 16 bits qui ne sont pas du silence, et
/// tape les VU-mètres au format de sortie. Quand le consommateur part, il
/// rend `Ok(())` sans attendre la fin de la station.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_decodeur_radio_rend_le_pcm_du_mp3_servi() {
    let (url, _connexions) = station_factice(vec![("audio/mpeg", MP3.to_vec())]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let (levels_tx, mut levels_rx) = tokio::sync::mpsc::unbounded_channel();
    let session_pour_le_decodeur = session.clone();
    let data_ready_pour_le_decodeur = data_ready.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(
            url,
            tx,
            data_ready_pour_le_decodeur,
            session_pour_le_decodeur,
            None,
            Some(levels_tx),
            false,
            None,
        )
    });

    tokio::time::timeout(Duration::from_secs(10), data_ready.notified())
        .await
        .expect("le premier morceau doit réveiller le consommateur en moins de 10 s");
    assert_eq!(
        session.detected_output_format(),
        Some((44_100, 2)),
        "le format publié doit être celui sondé dans le MP3 (44,1 kHz stéréo)"
    );

    let mut morceaux = 0usize;
    let mut octets = 0usize;
    let mut non_nuls = 0usize;
    while morceaux < 3 {
        let morceau = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("un morceau de PCM doit arriver en moins de 10 s")
            .expect("le décodeur ne doit pas fermer le canal avant trois morceaux");
        assert_eq!(morceau.len(), 32_768, "chaque morceau fait 32 768 octets");
        octets += morceau.len();
        non_nuls += morceau.iter().filter(|b| **b != 0).count();
        morceaux += 1;
    }
    assert!(octets >= 3 * 32_768);
    assert!(
        non_nuls > 1_000,
        "le PCM décodé ne doit pas être du silence : {non_nuls} octets non nuls sur {octets}"
    );

    let fenetre = tokio::time::timeout(Duration::from_secs(5), levels_rx.recv())
        .await
        .expect("les VU-mètres doivent recevoir une fenêtre")
        .expect("le bus des niveaux est ouvert");
    assert_eq!(fenetre.sample_rate, 44_100);
    assert_eq!(fenetre.channels, 2);
    assert_eq!(fenetre.bit_depth, 16);

    // Le consommateur s'en va : le décodeur doit rendre la main, `Ok(())`.
    drop(rx);
    let verdict = tokio::time::timeout(Duration::from_secs(15), decodeur)
        .await
        .expect("le décodeur doit rendre la main en moins de 15 s après le départ du consommateur")
        .expect("la tâche de décodage ne doit pas paniquer");
    assert_eq!(verdict, Ok(()));
}

/// La fin du flux amont n'est PAS la fin de la lecture : la boucle se
/// reconnecte en place et continue de nourrir la MÊME session (Xavier, les
/// coupures de Radio France). Preuve : la station voit une seconde
/// connexion après avoir servi son MP3 en entier, et le PCM continue
/// d'arriver après elle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_fin_du_flux_amont_reconnecte_la_meme_session() {
    let (url, connexions) = station_factice(vec![("audio/mpeg", MP3.to_vec())]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let session_pour_le_decodeur = session.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(
            url,
            tx,
            data_ready,
            session_pour_le_decodeur,
            None,
            None,
            false,
            None,
        )
    });

    // On draine tout ce qui vient, en comptant, jusqu'à ce que la station ait
    // accepté une seconde connexion ET qu'au moins un morceau soit arrivé
    // après elle.
    let borne = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut morceaux_avant = 0usize;
    let mut morceaux_apres = 0usize;
    loop {
        let reste = borne.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !reste.is_zero(),
            "aucune seconde connexion suivie de PCM en 20 s : la fin du flux amont \
             n'a pas provoqué de reconnexion (connexions={}, morceaux avant={morceaux_avant}, \
             après={morceaux_apres})",
            connexions.load(Ordering::SeqCst)
        );
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Some(_morceau)) => {
                if connexions.load(Ordering::SeqCst) >= 2 {
                    morceaux_apres += 1;
                } else {
                    morceaux_avant += 1;
                }
                if morceaux_apres >= 1 {
                    break;
                }
            }
            Ok(None) => panic!(
                "le décodeur a fermé le canal au lieu de se reconnecter (connexions={}, \
                 morceaux avant={morceaux_avant})",
                connexions.load(Ordering::SeqCst)
            ),
            Err(_) => continue,
        }
    }
    assert!(
        morceaux_avant >= 1,
        "la première connexion doit déjà avoir livré du PCM"
    );

    drop(rx);
    let verdict = tokio::time::timeout(Duration::from_secs(15), decodeur)
        .await
        .expect("le décodeur doit rendre la main en moins de 15 s après le départ du consommateur")
        .expect("la tâche de décodage ne doit pas paniquer");
    assert_eq!(verdict, Ok(()));
}

/// Une station qui, à la reconnexion, répond une page web : l'erreur est
/// DITE tout de suite (`radio_not_audio`), pas avalée par trente tentatives
/// (#1960, #3756). Le consommateur reste branché : c'est bien le décodeur
/// qui décide de rendre, et il rend `Err`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_page_web_a_la_reconnexion_est_dite_sans_attendre() {
    let (url, connexions) = station_factice(vec![
        ("audio/mpeg", MP3.to_vec()),
        (
            "text/html; charset=utf-8",
            b"<html>La station a ferme</html>".to_vec(),
        ),
    ]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    // Un consommateur qui lit tout, pour que le décodeur ne bloque jamais
    // sur un canal plein.
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let session_pour_le_decodeur = session.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(
            url,
            tx,
            data_ready,
            session_pour_le_decodeur,
            None,
            None,
            false,
            None,
        )
    });

    let verdict = tokio::time::timeout(Duration::from_secs(20), decodeur)
        .await
        .expect("le décodeur doit rendre en moins de 20 s : une page web ne se réessaie pas")
        .expect("la tâche de décodage ne doit pas paniquer");
    let erreur = verdict.expect_err("une page web à la reconnexion doit rendre Err");
    assert!(
        erreur.starts_with(RADIO_NOT_AUDIO),
        "l'erreur doit nommer radio_not_audio : {erreur}"
    );
    assert_eq!(
        connexions.load(Ordering::SeqCst),
        2,
        "exactement deux connexions : le MP3, puis la page web refusée"
    );
    drain
        .await
        .expect("le drain se termine quand le canal se ferme");
}

/// Décode la station jusqu'à `n` morceaux, en déposant `profil` dans le poste
/// de relève après le `apres`-ième (jamais si `None`). Rend les morceaux et le
/// nombre de relèves faites par le décodeur.
async fn decoder_avec_releve(
    n: usize,
    releve: Option<(usize, crate::audio::eq::EqProfile)>,
) -> (Vec<Vec<u8>>, u64) {
    let (url, _connexions) = station_factice(vec![("audio/mpeg", MP3.to_vec())]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let poste = super::eq_en_vol::EqEnVol::pour_la_radio();
    let poste_du_decodeur = poste.clone();
    let data_ready_pour_le_decodeur = data_ready.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(
            url,
            tx,
            data_ready_pour_le_decodeur,
            session,
            None,
            None,
            false,
            Some(poste_du_decodeur),
        )
    });
    tokio::time::timeout(Duration::from_secs(10), data_ready.notified())
        .await
        .expect("premier morceau en moins de 10 s");
    let mut morceaux = Vec::new();
    while morceaux.len() < n {
        if let Some((apres, ref profil)) = releve
            && morceaux.len() == apres
        {
            poste.poser(Some(profil.clone()));
        }
        let m = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("un morceau en moins de 10 s")
            .expect("le décodeur ne doit pas fermer le canal : le flux continue");
        morceaux.push(m);
    }
    drop(rx);
    let _ = tokio::time::timeout(Duration::from_secs(15), decodeur).await;
    (morceaux, poste.releves())
}

/// #4407 — un égaliseur déposé dans le poste d'un flux radio EN COURS est
/// relevé par le décodeur, dans la MÊME session : les premiers morceaux sont
/// ceux d'un flux sans égaliseur, les derniers sont égalisés, et le canal ne
/// s'est jamais fermé.
///
/// Le décodage d'un même MP3 est déterministe : un second décodeur sans
/// relève sert d'étalon, morceau pour morceau.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_egaliseur_depose_en_vol_est_releve_par_le_decodeur_radio_4407() {
    let profil = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 1000.0,
            gain: 12.0,
            q: 0.71,
            band_type: "peak".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (etalon, aucune) = decoder_avec_releve(16, None).await;
    let (servi, releves) = decoder_avec_releve(16, Some((2, profil))).await;
    assert_eq!(aucune, 0);
    assert_eq!(
        releves, 1,
        "le décodeur doit relever le profil déposé, une fois"
    );
    assert_eq!(
        servi[..2],
        etalon[..2],
        "avant le dépôt, le flux est celui d'un flux sans égaliseur"
    );
    assert_ne!(
        servi[15], etalon[15],
        "après le dépôt, le flux en cours porte l'égaliseur"
    );
}

/// Décode la station jusqu'à `octets_voulus` octets de PCM, puis s'en va.
/// Rend le format publié, les octets non nuls, et le nombre de connexions
/// acceptées par la station AU MOMENT où le dernier octet voulu est arrivé.
async fn decoder_la_station_ogg(
    content_type: &'static str,
    corps: Vec<u8>,
    octets_voulus: usize,
) -> (Option<(u32, u16)>, usize, usize) {
    let (url, connexions) = station_factice(vec![(content_type, corps)]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let session_pour_le_decodeur = session.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(
            url,
            tx,
            data_ready,
            session_pour_le_decodeur,
            None,
            None,
            false,
            None,
        )
    });
    let mut octets = 0usize;
    let mut non_nuls = 0usize;
    while octets < octets_voulus {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Some(m)) => {
                octets += m.len();
                non_nuls += m.iter().filter(|b| **b != 0).count();
            }
            Ok(None) => {
                let verdict = decodeur.await.expect("pas de panique");
                panic!("le décodeur a fermé le canal après {octets} octets : {verdict:?}");
            }
            Err(_) => panic!("aucun PCM en 10 s après {octets} octets"),
        }
    }
    let vues = connexions.load(Ordering::SeqCst);
    let format = session.detected_output_format();
    drop(rx);
    let verdict = tokio::time::timeout(Duration::from_secs(15), decodeur)
        .await
        .expect("le décodeur rend la main après le départ du consommateur")
        .expect("pas de panique");
    assert_eq!(verdict, Ok(()));
    (format, non_nuls, vues)
}

/// Ogg Vorbis servi sous `application/ogg`, sur une adresse sans extension
/// (un montage Icecast `/live`) : symphonia le décode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_decodeur_radio_rend_le_pcm_d_un_ogg_vorbis() {
    let corps = crate::ogg_factice::ogg_vorbis(1, &[("TITLE", "Vorbis")]);
    let (format, non_nuls, _) = decoder_la_station_ogg("application/ogg", corps, 3 * 32_768).await;
    let (cadence, canaux) = format.expect("le format sondé est publié");
    assert!(
        cadence >= 44_100 && canaux >= 1,
        "format publié : {format:?}"
    );
    assert!(non_nuls > 1_000, "le PCM Vorbis n'est pas du silence");
}

/// Ogg Opus servi sous `audio/ogg; codecs=opus` : symphonia n'a pas de
/// décodeur Opus, la station ne jouait pas (« radio decoder init failed:
/// unsupported audio codec »). libopus la décode, à 48 kHz stéréo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_decodeur_radio_rend_le_pcm_d_un_ogg_opus() {
    let corps = crate::ogg_factice::ogg_opus(2, 3.0, &[("TITLE", "Opus")]);
    let (format, non_nuls, _) =
        decoder_la_station_ogg("audio/ogg; codecs=opus", corps, 3 * 32_768).await;
    assert_eq!(format, Some((48_000, 2)), "Opus se publie en 48 kHz stéréo");
    assert!(non_nuls > 1_000, "le PCM Opus n'est pas du silence");
}

/// Ogg chaîné, à la manière d'Icecast : un flux logique par titre. Passer au
/// titre suivant ne doit PAS coûter une reconnexion. Opus : un titre d'une
/// seconde, puis un de quatre ; à 1,5 s de PCM reçu, la station n'a vu
/// qu'UNE connexion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_ogg_opus_chaine_passe_au_titre_suivant_sans_se_reconnecter() {
    let mut corps = crate::ogg_factice::ogg_opus(10, 1.0, &[("TITLE", "A - Un")]);
    corps.extend(crate::ogg_factice::ogg_opus(
        11,
        4.0,
        &[("TITLE", "B - Deux")],
    ));
    // 1,5 s à 48 kHz, stéréo, 16 bits.
    let (_, non_nuls, connexions) =
        decoder_la_station_ogg("application/ogg", corps, 48_000 * 4 * 3 / 2).await;
    assert_eq!(
        connexions, 1,
        "le changement de flux logique a provoqué une reconnexion"
    );
    assert!(non_nuls > 1_000);
}

/// Le même passage en Vorbis : trois titres de ~2 s ; à 3 s de PCM reçu,
/// toujours une seule connexion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_ogg_vorbis_chaine_passe_au_titre_suivant_sans_se_reconnecter() {
    let mut corps = Vec::new();
    for (serial, titre) in [(20, "Un"), (21, "Deux"), (22, "Trois")] {
        corps.extend(crate::ogg_factice::ogg_vorbis(serial, &[("TITLE", titre)]));
    }
    let (format, _, _) = decoder_la_station_ogg("application/ogg", corps.clone(), 32_768).await;
    let (cadence, canaux) = format.expect("format publié");
    let trois_secondes = cadence as usize * usize::from(canaux) * 2 * 3;
    let (_, non_nuls, connexions) =
        decoder_la_station_ogg("application/ogg", corps, trois_secondes).await;
    assert_eq!(
        connexions, 1,
        "le changement de flux logique a provoqué une reconnexion"
    );
    assert!(non_nuls > 1_000);
}

/// L'énergie relative de `signal` (mono) à `hz`, par Goertzel.
fn puissance_a(signal: &[f32], cadence: u32, hz: f32) -> f64 {
    let k = 2.0 * (std::f64::consts::TAU * f64::from(hz) / f64::from(cadence)).cos();
    let (mut s1, mut s2) = (0f64, 0f64);
    for x in signal {
        let s0 = f64::from(*x) + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - k * s1 * s2) / (signal.len() as f64).powi(2)
}

/// L'énergie au-dessus de 12 kHz rapportée à celle sous 12 kHz, sur une
/// grille de 250 Hz.
fn part_au_dessus_de_12_khz(signal: &[f32], cadence: u32) -> f64 {
    let (mut haut, mut bas) = (0f64, 0f64);
    let mut hz = 250.0f32;
    while hz < (cadence / 2) as f32 - 250.0 {
        let p = puissance_a(signal, cadence, hz);
        if hz > 12_000.0 {
            haut += p
        } else {
            bas += p
        }
        hz += 250.0;
    }
    haut / bas.max(f64::MIN_POSITIVE)
}

/// #6357 — l'échantillon HE-AAC FABRIQUÉ (SBR implicite, cœur à 24 kHz) est
/// bien du HE-AAC : `rusty_aac` le sort à 48 kHz, avec de l'énergie
/// au-dessus de 12 kHz, que le cœur ne peut pas porter.
#[test]
fn l_echantillon_he_aac_fabrique_se_decode_a_48_khz_avec_ses_aigus() {
    let adts = crate::radio_hls::fabrique_tests::trames_he_aac(1.0);
    let mut decodeur = rusty_aac::AacDecoder::new();
    let (mut pcm, mut cadence, mut canaux) = (Vec::new(), 0, 0);
    let mut i = 0;
    while i + 7 <= adts.len() {
        let longueur = rusty_aac::parse_adts(&adts[i..])
            .expect("en-tête ADTS")
            .frame_length;
        let trame = decodeur
            .decode(&adts[i..i + longueur], None)
            .expect("trame HE-AAC");
        cadence = trame.sample_rate;
        canaux = trame.channels;
        pcm.extend(trame.samples.iter().step_by(usize::from(trame.channels)));
        i += longueur;
    }
    assert_eq!(
        cadence, 48_000,
        "le SBR double la fréquence du cœur (24 kHz)"
    );
    assert_eq!(
        canaux, 2,
        "un cœur mono HE-AAC sort en stéréo (PS implicite)"
    );
    let part = part_au_dessus_de_12_khz(&pcm[4096..], cadence);
    assert!(part > 1e-3, "le SBR doit reconstruire des aigus : {part:e}");
}

/// #6357 — LE TÉMOIN DU BRANCHEMENT. Un faux direct HLS sur 127.0.0.1
/// (liste maîtresse `mp4a.40.5`, liste média, segments MPEG-TS de HE-AAC à
/// SBR implicite, comme la BBC hors Royaume-Uni) passe par le décodeur de
/// PRODUCTION, `decode_radio_stream_to_pcm`, jusqu'au PCM :
///
/// * le format publié est 48 kHz stéréo — la sortie du SBR — et non les
///   24 kHz que l'en-tête ADTS annonce (le piège de l'étude §4.4 : un WAV à
///   24 kHz sur du PCM à 48 kHz jouerait à demi-vitesse) ;
/// * le PCM servi porte des aigus au-dessus de 12 kHz ;
/// * la source publiée dit HLS, HE-AAC, et son débit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_station_hls_he_aac_est_decodee_jusqu_au_pcm_a_48_khz() {
    use crate::http::streamer::{AudioStreamer, StreamInfo};
    use crate::radio_hls::fabrique_tests as fab;

    // Trois segments de 2 s, puis un direct immobile.
    let adts = fab::trames_he_aac(6.0);
    let mut trames = Vec::new();
    let mut i = 0;
    while i + 7 <= adts.len() {
        let n = rusty_aac::parse_adts(&adts[i..]).unwrap().frame_length;
        trames.push(adts[i..i + n].to_vec());
        i += n;
    }
    let par_segment = trames.len() / 3;
    let segments: Vec<Vec<u8>> = trames
        .chunks(par_segment)
        .take(3)
        .enumerate()
        .map(|(n, t)| fab::segment_ts(&t.concat(), (n * 7 % 16) as u8, true))
        .collect();
    let routeur: Arc<fab::Routeur> = Arc::new(move |chemin: &str, _rang: usize| {
        match chemin {
        "/bbc/master.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            b"#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=101760,CODECS=\"mp4a.40.5\"\naudio=96000.m3u8\n".to_vec(),
        ),
        "/bbc/audio=96000.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            fab::liste_media(
                2,
                500,
                &(0..3).map(|n| (format!("s-{}.ts", 500 + n), String::new(), false)).collect::<Vec<_>>(),
                false,
            ),
        ),
        _ => match chemin
            .strip_prefix("/bbc/s-")
            .and_then(|r| r.strip_suffix(".ts"))
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) if (500..503).contains(&n) => fab::ok(fab::TYPE_TS, segments[n - 500].clone()),
            _ => fab::introuvable(),
        },
    }
    });
    let serveur = fab::ServeurHls::lancer(routeur);
    let url = format!("{}/bbc/master.m3u8", serveur.base);

    let streamer = Arc::new(AudioStreamer::new(0));
    let info = StreamInfo {
        format: "wav".to_string(),
        mime_type: "audio/wav".to_string(),
        ..StreamInfo::default()
    };
    let (id, _tx_session, _ready, session) = streamer.create_radio_session(info, 4).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let (session_d, ready_d) = (session.clone(), data_ready.clone());
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(url, tx, ready_d, session_d, None, None, false, None)
    });

    tokio::time::timeout(Duration::from_secs(15), data_ready.notified())
        .await
        .expect("le premier morceau doit réveiller le consommateur en moins de 15 s");
    assert_eq!(
        session.detected_output_format(),
        Some((48_000, 2)),
        "le format publié doit être celui du HE-AAC décodé (48 kHz stéréo), \
         pas celui du cœur AAC-LC que dit l'en-tête ADTS (24 kHz mono)"
    );
    let source = streamer
        .stream_output_wire(&id)
        .await
        .and_then(|w| w.radio_source)
        .expect("la source radio est publiée");
    assert_eq!(source.transport, Some("hls"));
    assert_eq!(source.profil, Some("HE-AAC"));
    assert_eq!(source.format, Some("aac"));
    assert!(source.debit_kbps.is_some_and(|k| k > 0), "{source:?}");

    // Deux secondes de PCM 16 bits stéréo : la voie gauche, en flottants.
    let mut octets = Vec::new();
    while octets.len() < 2 * 48_000 * 4 {
        let morceau = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("un morceau de PCM doit arriver en moins de 10 s")
            .expect("le décodeur ne doit pas fermer le canal");
        octets.extend(morceau);
    }
    let gauche: Vec<f32> = octets
        .chunks_exact(4)
        .map(|t| f32::from(i16::from_le_bytes([t[0], t[1]])) / 32768.0)
        .collect();
    let part = part_au_dessus_de_12_khz(&gauche[8192..], 48_000);
    assert!(
        part > 1e-3,
        "le PCM servi doit porter les aigus reconstruits par le SBR : {part:e}"
    );

    drop(rx);
    let verdict = tokio::time::timeout(Duration::from_secs(15), decodeur)
        .await
        .expect("le décodeur doit rendre la main après le départ du consommateur")
        .expect("la tâche de décodage ne doit pas paniquer");
    assert_eq!(verdict, Ok(()));
}
