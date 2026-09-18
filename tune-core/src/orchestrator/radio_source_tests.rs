use super::*;
use crate::http::streamer::{AudioStreamer, RadioSourceInfo, StreamInfo};

async fn observe_station(bytes: &'static [u8], extension: &str) -> StreamInfo {
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
    tokio::task::spawn_blocking(move || {
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
        };
        assert!(preparer_la_sortie(&mut etat, &url, &canaux, &sonde).is_ok());
    })
    .await
    .unwrap();
    let wire = streamer.stream_output_wire(&id).await.unwrap();
    server.abort();
    streamer.remove_session(&id).await;
    wire
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
