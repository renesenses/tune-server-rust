//! PTS réellement reçus sur le fil, sans appareil ni restitution sonore.
use super::OaatOutput;
use super::rejeu_lecture_directe_6497_tests::{PaquetCapture, faux_endpoint_capturant, wav};
use crate::outputs::{OutputTarget, PlayMedia};
use oaat_core::wire::{AudioPacketHeader, PacketFlags};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

fn wav_trames(trames: u32) -> Vec<u8> {
    let mut b = wav(0);
    let taille = trames * 4;
    b[4..8].copy_from_slice(&(36 + taille).to_le_bytes());
    b[40..44].copy_from_slice(&taille.to_le_bytes());
    b.resize(44 + taille as usize, 0);
    b
}

fn dsf(path: &std::path::Path, octets_par_canal: usize) {
    const BLOC: usize = 4096;
    let blocs = octets_par_canal.div_ceil(BLOC);
    let taille = blocs * BLOC * 2;
    let mut b = b"DSD ".to_vec();
    b.extend(28_u64.to_le_bytes());
    b.extend((28 + 52 + 12 + taille as u64).to_le_bytes());
    b.extend(0_u64.to_le_bytes());
    b.extend(b"fmt ");
    b.extend(52_u64.to_le_bytes());
    for v in [1_u32, 0, 2, 2, 2_822_400, 1] {
        b.extend(v.to_le_bytes());
    }
    b.extend((octets_par_canal as u64 * 8).to_le_bytes());
    b.extend((BLOC as u32).to_le_bytes());
    b.extend(0_u32.to_le_bytes());
    b.extend(b"data");
    b.extend((12 + taille as u64).to_le_bytes());
    for _ in 0..blocs {
        b.extend(std::iter::repeat_n(0x69, BLOC));
        b.extend(std::iter::repeat_n(0x96, BLOC));
    }
    std::fs::write(path, b).unwrap();
}

async fn recevoir_frontiere(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<PaquetCapture>,
) -> (AudioPacketHeader, AudioPacketHeader, usize) {
    let mut premier = None;
    let mut octets = 0;
    loop {
        let (header, payload) = rx.recv().await.expect("le faux endpoint reste ouvert");
        if payload.is_empty() {
            continue;
        }
        if header.flags.contains(PacketFlags::FIRST_PACKET) {
            if let Some(premier) = premier {
                return (premier, header, octets);
            }
            premier = Some(header);
        }
        octets += payload.len();
    }
}

async fn direct(dsd_natif: bool) {
    let dir = crate::test_scratch::scratch_dir("oaat-pts-direct-7332");
    let (a, b, unites, cadence, octets_attendus, mime) = if dsd_natif {
        let a = dir.join("a.dsf");
        let b = dir.join("b.dsf");
        // La fin utile se trouve au milieu du dernier bloc DSF.
        let par_canal = 352_802_usize;
        dsf(&a, par_canal);
        dsf(&b, 12_003);
        (
            a,
            b,
            par_canal as u64 * 8,
            2_822_400_u64,
            par_canal * 2,
            "audio/x-dsf",
        )
    } else {
        let a = dir.join("a.wav");
        let b = dir.join("b.wav");
        let trames = 44_103_u32;
        std::fs::write(&a, wav_trames(trames)).unwrap();
        std::fs::write(&b, wav_trames(23_113)).unwrap();
        (
            a,
            b,
            u64::from(trames),
            44_100_u64,
            trames as usize * 4,
            "audio/wav",
        )
    };
    let (port, mut rx) = faux_endpoint_capturant(dsd_natif).await;
    let output = OaatOutput::new(
        "Banc PTS 7332".into(),
        "127.0.0.1".into(),
        port,
        "pts-7332".into(),
    );
    output
        .play_media(&PlayMedia {
            url: "http://127.0.0.1:1/flux-fictif-a",
            mime_type: mime,
            title: Some("A"),
            file_path: a.to_str(),
            duration_ms: Some(unites * 1000 / cadence),
            ..Default::default()
        })
        .await
        .unwrap();
    // La commande est mise en file pendant la négociation, sans attendre
    // une durée arbitraire ni dépendre du temps pris par l'ordonnanceur.
    output
        .set_next_media(&PlayMedia {
            url: "http://127.0.0.1:1/flux-fictif-b",
            mime_type: mime,
            title: Some("B"),
            file_path: b.to_str(),
            duration_ms: Some(500),
            ..Default::default()
        })
        .await
        .unwrap();
    let frontiere =
        tokio::time::timeout(Duration::from_secs(15), recevoir_frontiere(&mut rx)).await;
    output.stop().await.unwrap();
    let (premier, suivant, octets) =
        frontiere.expect("deux premières trames sur la même connexion préparée");
    assert_eq!(
        octets, octets_attendus,
        "toutes les données utiles de A doivent précéder B"
    );
    assert_eq!(premier.stream_id, suivant.stream_id);
    assert_eq!(premier.sample_offset, 0);
    assert_eq!(
        suivant.sample_offset, 0,
        "l'offset local de B repart à zéro"
    );
    let duree = u64::try_from(u128::from(unites) * 1_000_000_000 / u128::from(cadence)).unwrap();
    assert_eq!(
        suivant.pts_ns,
        premier.pts_ns + duree,
        "#7332 : le premier PTS de B doit suivre exactement les données de A, sans réancrage sur l'instant de traitement"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_pts_pcm_direct_suivent_les_trames_reelles_aux_frontieres_7332() {
    direct(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_pts_dsd_natif_suivent_les_bits_utiles_aux_frontieres_7332() {
    direct(true).await;
}

struct HttpPrepare {
    port: u16,
    fin: Option<Arc<Notify>>,
    fin_envoyee: Arc<Notify>,
    tache: tokio::task::JoinHandle<()>,
}

impl Drop for HttpPrepare {
    fn drop(&mut self) {
        self.tache.abort();
    }
}

impl HttpPrepare {
    async fn nouveau(a: Vec<u8>, b: Vec<u8>, preparation: Arc<Notify>, retenir_fin: bool) -> Self {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        let fin = retenir_fin.then(|| Arc::new(Notify::new()));
        let fin_tache = fin.clone();
        let fin_envoyee = Arc::new(Notify::new());
        let envoyee = fin_envoyee.clone();
        let a = Arc::new(a);
        let b = Arc::new(b);
        let tache = tokio::spawn(async move {
            let mut connexions = tokio::task::JoinSet::new();
            while let Ok((mut socket, _)) = tcp.accept().await {
                let (a, b, preparation, fin, envoyee) = (
                    a.clone(),
                    b.clone(),
                    preparation.clone(),
                    fin_tache.clone(),
                    envoyee.clone(),
                );
                connexions.spawn(async move {
                    let mut requete = Vec::new();
                    let mut buf = [0; 1024];
                    while !requete.windows(4).any(|x| x == b"\r\n\r\n") {
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        if n == 0 || requete.len() > 16_384 { return; }
                        requete.extend_from_slice(&buf[..n]);
                    }
                    let est_a = String::from_utf8_lossy(&requete).split_whitespace().nth(1) == Some("/a.wav");
                    let body = if est_a { a } else { b };
                    let header = format!("HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    if socket.write_all(header.as_bytes()).await.is_err() { return; }
                    if est_a {
                        let debut = body.len().min(4096);
                        if socket.write_all(&body[..debut]).await.is_err() { return; }
                        // La fin de A ne peut arriver avant NextTrackReady.
                        // Le contrôleur est alors dans settle_prefetch : il
                        // termine cette préparation avant de lire la fin.
                        preparation.notified().await;
                        if let Some(fin) = fin { fin.notified().await; }
                        let _ = socket.write_all(&body[debut..]).await;
                        let _ = socket.shutdown().await;
                        envoyee.notify_one();
                    } else {
                        let _ = socket.write_all(&body).await;
                        let _ = socket.shutdown().await;
                    }
                });
            }
        });
        Self {
            port,
            fin,
            fin_envoyee,
            tache,
        }
    }

    fn url(&self, nom: &str) -> String {
        format!("http://127.0.0.1:{}/{nom}.wav", self.port)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_flux_http_prepare_garde_la_meme_ligne_de_pts_7332() {
    let preparation = Arc::new(Notify::new());
    let (port, mut rx) =
        super::rejeu_lecture_directe_6497_tests::faux_endpoint_capturant_avec_preparation(
            false,
            vec![preparation.clone()],
        )
        .await;
    let trames = 44_103_u32;
    let http =
        HttpPrepare::nouveau(wav_trames(trames), wav_trames(23_113), preparation, false).await;
    let output = OaatOutput::new(
        "Banc HTTP PTS".into(),
        "127.0.0.1".into(),
        port,
        "pts-http-7332".into(),
    );
    let a = http.url("a");
    let b = http.url("b");
    output
        .play_media(&PlayMedia {
            url: &a,
            mime_type: "audio/wav",
            title: Some("A"),
            duration_ms: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap();
    output
        .set_next_media(&PlayMedia {
            url: &b,
            mime_type: "audio/wav",
            title: Some("B"),
            duration_ms: Some(500),
            ..Default::default()
        })
        .await
        .unwrap();
    let frontiere =
        tokio::time::timeout(Duration::from_secs(15), recevoir_frontiere(&mut rx)).await;
    output.stop().await.unwrap();
    let (premier, suivant, octets) =
        frontiere.expect("la piste HTTP suivante est effectivement préparée");
    assert_eq!(octets, trames as usize * 4);
    assert_eq!(premier.stream_id, suivant.stream_id);
    assert_eq!(suivant.sample_offset, 0);
    let duree = u64::from(trames) * 1_000_000_000 / 44_100;
    assert_eq!(
        suivant.pts_ns,
        premier.pts_ns + duree,
        "#7332 : le flux HTTP ajoute un trou/recouvrement au réancrage de B"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn annuler_une_preparation_http_ne_lance_pas_la_piste_suivante_7332() {
    let preparation = Arc::new(Notify::new());
    let observee = Arc::new(Notify::new());
    let (port, mut rx) =
        super::rejeu_lecture_directe_6497_tests::faux_endpoint_capturant_avec_preparation(
            false,
            vec![preparation.clone(), observee.clone()],
        )
        .await;
    let http = HttpPrepare::nouveau(
        wav_trames(2_205),
        wav_trames(23_113),
        preparation.clone(),
        true,
    )
    .await;
    let output = OaatOutput::new(
        "Banc HTTP annulé".into(),
        "127.0.0.1".into(),
        port,
        "pts-annule-7332".into(),
    );
    let a = http.url("a");
    let b = http.url("b");
    output
        .play_media(&PlayMedia {
            url: &a,
            mime_type: "audio/wav",
            title: Some("A"),
            ..Default::default()
        })
        .await
        .unwrap();
    output
        .set_next_media(&PlayMedia {
            url: &b,
            mime_type: "audio/wav",
            title: Some("B"),
            ..Default::default()
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(12), observee.notified())
        .await
        .expect("NextTrackReady observé");
    assert_eq!(
        output.get_status().await.unwrap().track_title.as_deref(),
        Some("A")
    );
    output.stop().await.unwrap();
    preparation.notify_one();
    http.fin.as_ref().unwrap().notify_one();
    tokio::time::timeout(Duration::from_secs(3), http.fin_envoyee.notified())
        .await
        .expect("fin HTTP libérée après Stop");
    let observation = tokio::time::timeout(Duration::from_millis(300), async {
        let mut debuts = 0;
        while let Some((h, p)) = rx.recv().await {
            if !p.is_empty() && h.flags.contains(PacketFlags::FIRST_PACKET) {
                debuts += 1;
            }
            assert!(
                debuts <= 1,
                "#7332 : B a commencé malgré l'annulation avant la fin de A"
            );
        }
    })
    .await;
    assert!(
        observation.is_err(),
        "l'endpoint doit rester ouvert pendant l'observation négative"
    );
    assert_eq!(
        output.get_status().await.unwrap().state,
        crate::outputs::TransportState::Stopped
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_format_direct_incompatible_garde_le_repli_de_fin_de_piste_7332() {
    let dir = crate::test_scratch::scratch_dir("oaat-pts-incompatible-7332");
    let a = dir.join("a.wav");
    let b = dir.join("b.dsf");
    std::fs::write(&a, wav_trames(44_103)).unwrap();
    // Une autre fréquence PCM est convertible par l'adaptateur. Le DSD
    // ne peut en revanche pas satisfaire ce contrat PCM déjà négocié.
    dsf(&b, 12_003);
    let (port, mut rx) = faux_endpoint_capturant(false).await;
    let output = OaatOutput::new(
        "Banc format incompatible".into(),
        "127.0.0.1".into(),
        port,
        "pts-format-7332".into(),
    );
    output
        .play_media(&PlayMedia {
            url: "http://127.0.0.1:1/a.wav",
            mime_type: "audio/wav",
            file_path: a.to_str(),
            title: Some("A"),
            duration_ms: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap();
    output
        .set_next_media(&PlayMedia {
            url: "http://127.0.0.1:1/b.dsf",
            mime_type: "audio/x-dsf",
            file_path: b.to_str(),
            title: Some("B"),
            duration_ms: Some(500),
            ..Default::default()
        })
        .await
        .unwrap();
    let fin = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let s = output.get_status().await.unwrap();
            if s.state == crate::outputs::TransportState::Stopped {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let repli_libere = !output.supports_internal_gapless();
    output.stop().await.unwrap();
    let fin =
        fin.expect("la piste incompatible doit libérer le repli normal, pas bloquer le sondeur");
    assert_eq!(
        fin.track_title.as_deref(),
        Some("A"),
        "B n'est pas adoptée silencieusement au mauvais format"
    );
    assert!(repli_libere);
    let (debuts, octets) = tokio::time::timeout(Duration::from_secs(3), async {
        let mut debuts = 0;
        let mut octets = 0;
        loop {
            let (header, payload) = rx
                .recv()
                .await
                .expect("capture ouverte jusqu'au dernier paquet");
            if !payload.is_empty() {
                debuts += usize::from(header.flags.contains(PacketFlags::FIRST_PACKET));
                octets += payload.len();
            }
            if header.flags.contains(PacketFlags::LAST_PACKET) {
                break (debuts, octets);
            }
        }
    })
    .await
    .expect("le dernier paquet confirme toutes les données précédentes");
    assert_eq!(debuts, 1);
    assert_eq!(octets, 44_103 * 4);
}
