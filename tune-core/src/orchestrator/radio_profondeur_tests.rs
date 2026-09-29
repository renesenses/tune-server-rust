//! #5217 : vraie résolution de radio locale/OAAT, station HTTP synthétique,
//! sonde et décodeur de production. Aucun périphérique audio nécessaire.
use super::*;
use crate::http::streamer::{StreamInfo, StreamSession};
use std::time::Duration;

const FLAC24: &[u8] = include_bytes!("../../tests/fixtures/flac/ref_24_96000_stereo.flac");

struct Lecture {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    id: String,
    session: Arc<StreamSession>,
    serveur: tokio::task::JoinHandle<()>,
    terminer: Arc<std::sync::atomic::AtomicBool>,
    evenements: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    niveaux: tokio::sync::broadcast::Receiver<crate::audio::tap::PcmTapFrame>,
}

impl Lecture {
    async fn arreter(self) {
        self.orch.playback.stop(self.zone_id).await;
        self.terminer
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.orch.streamer.remove_session(&self.id).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self
                .session
                .producer_done
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("le décodeur #5217 doit finir après retrait de la session");
        self.serveur.abort();
    }

    async fn morceau(&self) -> (Vec<u8>, StreamInfo) {
        let pcm = tokio::time::timeout(Duration::from_secs(10), self.session.recv_chunk())
            .await
            .expect("la station #5217 doit produire du PCM")
            .expect("le canal PCM doit rester ouvert");
        let info = self
            .orch
            .streamer
            .stream_output_wire(&self.id)
            .await
            .unwrap();
        (pcm, info)
    }
}

async fn lancer(sortie: &str, strict: bool) -> Lecture {
    lancer_avec(sortie, strict, FLAC24).await
}

async fn lancer_avec(sortie: &str, strict: bool, source: &'static [u8]) -> Lecture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/station.flac", listener.local_addr().unwrap());
    let terminer = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fin = terminer.clone();
    let app = axum::Router::new().route(
        "/station.flac",
        axum::routing::get(move || {
            let fin = fin.clone();
            async move {
                if fin.load(std::sync::atomic::Ordering::SeqCst) {
                    ([("content-type", "text/html")], &b"station terminee"[..])
                } else {
                    ([("content-type", "audio/flac")], source)
                }
            }
        }),
    );
    let serveur = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut orch = test_orchestrator();
    let bus = Arc::new(EventBus::new());
    let evenements = bus.subscribe();
    orch.event_bus = Some(bus);
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Radio 5217", Some("local"), None)
        .unwrap();
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(
            &crate::audio::bitperfect_strict::cle_de_zone(zone_id),
            if strict { "true" } else { "false" },
        )
        .unwrap();
    let req = PlayRequest {
        zone_id,
        source: Some("radio".into()),
        source_id: Some(url),
        output_device_id: Some(sortie.into()),
        ..Default::default()
    };
    let niveaux = orch.playback.zone_tap(zone_id).subscribe();
    orch.playback
        .play(
            zone_id,
            crate::playback::NowPlaying {
                source: "radio".into(),
                ..Default::default()
            },
        )
        .await;
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    let id = resolved
        .stream_id
        .expect("la radio locale/OAAT doit avoir une session PCM");
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&id)
        .unwrap()
        .clone();
    Lecture {
        orch,
        zone_id,
        id,
        session,
        serveur,
        terminer,
        evenements,
        niveaux,
    }
}

fn pcm_de_reference() -> Vec<u8> {
    use md5::{Digest, Md5};
    let audio = crate::audio::decode::decode_to_pcm(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/flac/ref_24_96000_stereo.flac"
        ),
        None,
        None,
        0.0,
        0.0,
    )
    .unwrap();
    assert_eq!(audio.bit_depth, 24);
    // Oracle publié dans flac_empreintes_reference.rs, calculé par libFLAC.
    let mut empreinte = Md5::new();
    for sample in &audio.samples_i32 {
        empreinte.update(sample.to_le_bytes());
    }
    assert_eq!(
        format!("{:x}", empreinte.finalize()),
        "5647a1733e4ec46e7a1dd00e10feaf3c"
    );
    audio
        .samples_i32
        .iter()
        .flat_map(|s| s.to_le_bytes()[..3].to_vec())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radio_locale_publie_24_bits_5217() {
    let lecture = lancer("local:temoin-5217", true).await;
    let (_, info) = lecture.morceau().await;
    lecture.arreter().await;
    assert_eq!(
        info.bit_depth, 24,
        "#5217 : la radio locale 24 bits est annoncée en 16 bits"
    );
    assert_eq!(info.sample_rate, 96_000);
    assert_eq!(info.channels, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radio_locale_conserve_les_octets_et_les_bits_faibles_5217() {
    let lecture = lancer("local:temoin-5217", false).await;
    let (pcm, _) = lecture.morceau().await;
    lecture.arreter().await;
    let attendu = &pcm_de_reference()[..pcm.len()];
    assert!(
        attendu.chunks_exact(3).any(|s| s[0] != 0),
        "la référence doit porter des bits sous le seizième"
    );
    assert!(
        pcm == attendu,
        "#5217 : le décodeur radio a perdu les bits faibles du FLAC 24 bits"
    );
    assert_eq!(
        pcm.len() % 6,
        0,
        "les paquets stéréo 24 bits doivent finir sur une trame"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radio_oaat_conserve_le_format_compatible_16_bits_5217() {
    let lecture = lancer("oaat:temoin-5217", false).await;
    let (pcm, info) = lecture.morceau().await;
    lecture.arreter().await;
    assert_eq!(
        info.bit_depth, 16,
        "le correctif local ne relève pas la profondeur réseau"
    );
    let attendu: Vec<u8> = pcm_de_reference()
        .chunks_exact(3)
        .take(pcm.len() / 2)
        .flat_map(|s| {
            let entier =
                i32::from_le_bytes([s[0], s[1], s[2], if s[2] & 0x80 != 0 { 255 } else { 0 }]);
            ((entier / 256) as i16).to_le_bytes()
        })
        .collect();
    assert!(pcm == attendu, "le PCM réseau reste identique");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radio_stricte_refuse_la_reduction_de_profondeur_5217() {
    let mut lecture = lancer("oaat:temoin-5217", true).await;
    let erreur = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let ev = lecture.evenements.recv().await.unwrap();
            if ev.event_type == "zone.playback_error" {
                return ev.data;
            }
        }
    })
    .await;
    let audio = tokio::time::timeout(Duration::from_millis(20), lecture.session.recv_chunk()).await;
    lecture.arreter().await;
    let erreur =
        erreur.expect("#5217 : Bit-perfect strict laisse passer la réduction 24 vers 16 bits");
    assert_eq!(erreur["fatal"], true);
    assert!(erreur["error"].as_str().unwrap().contains("24 bits"));
    assert!(erreur["error"].as_str().unwrap().contains("16 bits"));
    assert!(
        !matches!(audio, Ok(Some(_))),
        "le refus strict doit précéder le premier octet PCM"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radio_locale_vumetres_observent_le_pcm_24_bits_5217() {
    let mut lecture = lancer("local:temoin-5217", false).await;
    let _ = lecture.morceau().await;
    let fenetre = tokio::time::timeout(Duration::from_secs(2), lecture.niveaux.recv()).await;
    lecture.arreter().await;
    let fenetre = fenetre.unwrap().unwrap();
    assert_eq!(
        fenetre.format.bit_depth, 24,
        "#5217 : les vumètres interprètent le PCM 24 bits comme du 16 bits"
    );
    assert_eq!(fenetre.format.channels, 2);
    assert_eq!(fenetre.format.sample_rate, 96_000);
    assert_eq!(fenetre.pcm.len() % 6, 0);
    assert_eq!(&*fenetre.pcm, &pcm_de_reference()[..fenetre.pcm.len()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radio_locale_16_bits_reste_16_bits_mono_et_stereo_5217() {
    for (source, canaux) in [
        (
            &include_bytes!("../../tests/fixtures/flac/ref_16_44100_stereo.flac")[..],
            2,
        ),
        (
            &include_bytes!("../../tests/fixtures/flac/ref_16_44100_mono.flac")[..],
            1,
        ),
    ] {
        let lecture = lancer_avec("local:temoin-5217", true, source).await;
        let (pcm, info) = lecture.morceau().await;
        lecture.arreter().await;
        assert_eq!(info.bit_depth, 16);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, canaux);
        assert_eq!(pcm.len() % usize::from(canaux * 2), 0);
    }
}
