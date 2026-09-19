use super::*;
use crate::http::streamer::{AudioStreamer, RadioSourceInfo, StreamInfo};

async fn observe_station(bytes: &'static [u8], extension: &str) -> StreamInfo {
    let (wire, preparation) = preparer_contre_la_station(bytes, extension, false).await;
    assert!(preparation.is_ok());
    wire
}

/// La station factice, sondée puis passée à `preparer_la_sortie` avec le
/// réglage « bit-perfect strict » voulu (#3973). Rend le fil publié et le
/// verdict de la préparation (`Err(Some(motif))` pour un refus rendu).
async fn preparer_contre_la_station(
    bytes: &'static [u8],
    extension: &str,
    strict_bitperfect: bool,
) -> (StreamInfo, Result<(), Option<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/station.{extension}",
        listener.local_addr().unwrap()
    );
    let app = axum::Router::new().route(
        "/{file}",
        axum::routing::get(move || async move {
            ([("content-type", "application/octet-stream")], bytes)
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let streamer = AudioStreamer::new(8080);
    let (id, tx, data_ready, session) = streamer
        .create_radio_session(
            StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            8,
        )
        .await;
    assert_eq!(
        streamer.stream_output_wire(&id).await.unwrap().radio_source,
        Some(RadioSourceInfo::default()),
        "unprobed decoded radio must be explicitly unknown"
    );
    let preparation = tokio::task::spawn_blocking(move || {
        // Exercise the actual network probe, codec detection, decoder creation,
        // and publication before PCM delivery. Never contact a real station.
        let sonde = sonder_la_station(&url).unwrap();
        let mut etat = EtatRadio {
            first_chunk_sent: false,
            pcm_buf: Vec::new(),
            chunk_size: 32768,
            reconnects: 0,
            dropped_at: None,
            expected_format: None,
            radio_eq: None,
        };
        let rt = tokio::runtime::Handle::current();
        let canaux = CanauxRadio {
            tx: &tx,
            data_ready: &data_ready,
            session: &session,
            eq_profile: &None,
            levels_tx: &None,
            rt: &rt,
            strict_bitperfect,
        };
        match preparer_la_sortie(&mut etat, &url, &canaux, &sonde) {
            Ok(_) => Ok(()),
            Err(SuiteRadio::Rendre(Err(motif))) => Err(Some(motif)),
            Err(_) => Err(None),
        }
    })
    .await
    .unwrap();
    let wire = streamer.stream_output_wire(&id).await.unwrap();
    server.abort();
    streamer.remove_session(&id).await;
    (wire, preparation)
}

/// Un WAV PCM 16 bits stéréo d'une seconde à `cadence` Hz — une station
/// HE-AAC décodée à son cœur AAC-LC ressemble à ça (22 050 Hz).
fn wav_a(cadence: u32) -> &'static [u8] {
    let trames = cadence as usize;
    let donnees = trames * 4;
    let mut v = Vec::with_capacity(44 + donnees);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&cadence.to_le_bytes());
    v.extend_from_slice(&(cadence * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..trames {
        let e = ((i as f32 * 0.05).sin() * 8000.0) as i16;
        v.extend_from_slice(&e.to_le_bytes());
        v.extend_from_slice(&e.to_le_bytes());
    }
    Box::leak(v.into_boxed_slice())
}

/// #3973 — site « décodage radio » : une station à 22 050 Hz que le décodeur
/// relèverait à 44 100 Hz (`renderer_safe_wav_rate`). Zone en bit-perfect
/// strict ⇒ la préparation REFUSE, avec la sentinelle qui nomme les deux
/// fréquences ; sans strict ⇒ elle prépare, comme avant.
#[tokio::test]
async fn radio_3973_strict_refuse_de_relever_la_cadence() {
    let (_, strict) = preparer_contre_la_station(wav_a(22_050), "wav", true).await;
    assert_eq!(
        strict,
        Err(Some("bitperfect_strict_refused:22050:44100".to_string())),
        "bit-perfect strict : la radio à 22,05 kHz ne doit pas être relevée à 44,1 kHz en silence"
    );
    let (wire, defaut) = preparer_contre_la_station(wav_a(22_050), "wav", false).await;
    assert_eq!(defaut, Ok(()), "sans strict, la conversion est jouée");
    assert_eq!(wire.sample_rate, 44_100);
}

#[tokio::test]
async fn radio_4346_mp3_probe_keeps_source_distinct_from_wav_output() {
    let wire = observe_station(include_bytes!("../../tests/fixtures/test.mp3"), "mp3").await;
    assert_eq!(
        wire.radio_source.unwrap().format,
        Some("mp3"),
        "MP3 station codec must survive decoding to WAV"
    );
    assert_eq!(
        wire.radio_source.unwrap().bit_depth,
        None,
        "16-bit WAV output must not become an invented MP3 source bit depth"
    );
    assert!(wire.radio_source.unwrap().sample_rate.unwrap() > 0);
    assert_eq!(wire.format, "wav");
    assert_eq!(wire.mime_type, "audio/wav");
    assert_eq!(wire.bit_depth, 16);
}

#[tokio::test]
async fn radio_4346_flac_probe_keeps_original_resolution() {
    let wire = observe_station(
        include_bytes!("../../tests/fixtures/flac/ref_24_96000_stereo.flac"),
        "flac",
    )
    .await;
    assert_eq!(
        wire.radio_source,
        Some(RadioSourceInfo {
            format: Some("flac"),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
        }),
        "source must retain 24-bit FLAC even though the radio output is 16-bit WAV"
    );
    assert_eq!(wire.format, "wav");
    assert_eq!(wire.sample_rate, 96_000);
    assert_eq!(wire.bit_depth, 16);
}
