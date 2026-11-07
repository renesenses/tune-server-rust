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
            eq_de_depart_pose: false,
        };
        let rt = tokio::runtime::Handle::current();
        let canaux = CanauxRadio {
            tx: &tx,
            data_ready: &data_ready,
            session: &session,
            eq_profile: &None,
            en_vol: &None,
            levels_tx: &None,
            rt: &rt,
            strict_bitperfect,
            sortie_locale: false,
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
async fn radio_4346_flac_probe_keeps_original_resolution_5336() {
    let wire = observe_station(
        include_bytes!("../../tests/fixtures/flac/ref_24_96000_stereo.flac"),
        "flac",
    )
    .await;
    assert_eq!(
        wire.radio_source,
        Some(RadioSourceInfo {
            channels: Some(2),
            format: Some("flac"),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            ..Default::default()
        }),
        "source must retain 24-bit FLAC even though the radio output is 16-bit WAV"
    );
    assert_eq!(wire.format, "wav");
    assert_eq!(wire.sample_rate, 96_000);
    assert_eq!(wire.bit_depth, 16);
}

/// #5716 — de bout en bout : la sonde RÉELLE d'une station MP3 servie en
/// local remplit le codec de la station qui n'en avait pas, par l'observateur
/// que `decoder_la_radio_en_wav` et `servir_la_radio_au_reseau` posent sur
/// leur session. Une station au codec déjà connu le garde.
#[tokio::test]
async fn radio_5716_la_sonde_retient_le_codec_de_la_station() {
    use crate::db::backend::DbBackend;
    use crate::db::radio_repo::{RadioRepo, RadioStation};
    let bytes: &'static [u8] = include_bytes!("../../tests/fixtures/test.mp3");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/{file}",
        axum::routing::get(move || async move {
            ([("content-type", "application/octet-stream")], bytes)
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let sqlite = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    crate::db::migrations::run_migrations(&sqlite).unwrap();
    let db: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(sqlite);
    let repo = RadioRepo::with_backend(db.clone());
    let station = |url: &str, codec: Option<&str>| RadioStation {
        id: None,
        name: url.into(),
        url: url.into(),
        homepage: None,
        logo_url: None,
        country: None,
        language: None,
        genre: None,
        codec: codec.map(Into::into),
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    };
    let sans_codec_url = format!("{base}/sans-codec.mp3");
    let connue_url = format!("{base}/connue.mp3");
    let sans_codec = repo.create(&station(&sans_codec_url, None)).unwrap();
    let connue = repo.create(&station(&connue_url, Some("AAC"))).unwrap();

    for url in [sans_codec_url.clone(), connue_url.clone()] {
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
        retenir_le_codec_de_la_station(&db, &session, Some(&url));
        let preparee = tokio::task::spawn_blocking(move || {
            let sonde = sonder_la_station(&url).unwrap();
            let mut etat = EtatRadio {
                first_chunk_sent: false,
                pcm_buf: Vec::new(),
                chunk_size: 32768,
                reconnects: 0,
                dropped_at: None,
                expected_format: None,
                radio_eq: None,
                eq_de_depart_pose: false,
            };
            let rt = tokio::runtime::Handle::current();
            let canaux = CanauxRadio {
                tx: &tx,
                data_ready: &data_ready,
                session: &session,
                eq_profile: &None,
                en_vol: &None,
                levels_tx: &None,
                rt: &rt,
                strict_bitperfect: false,
                sortie_locale: false,
            };
            preparer_la_sortie(&mut etat, &url, &canaux, &sonde).is_ok()
        })
        .await
        .unwrap();
        assert!(preparee);
        streamer.remove_session(&id).await;
    }
    server.abort();

    let codec = |id| repo.get(id).unwrap().unwrap().codec;
    assert_eq!(
        codec(sans_codec).as_deref(),
        Some("MP3"),
        "#5716 : la station sans codec doit retenir le codec que la sonde a reconnu"
    );
    assert_eq!(
        codec(connue).as_deref(),
        Some("AAC"),
        "un codec déjà connu n'est pas remplacé par la sonde"
    );
}

/// #6821 — la sonde relève le débit que la station ANNONCE (`icy-br`), et
/// rien quand l'en-tête manque. Station factice locale, jamais une vraie.
async fn debit_sonde(entete: Option<&'static str>) -> Option<u32> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/station.wav", listener.local_addr().unwrap());
    let corps = wav_a(44_100);
    let app = axum::Router::new().route(
        "/{file}",
        axum::routing::get(move || async move {
            let mut h = axum::http::HeaderMap::new();
            h.insert("content-type", "application/octet-stream".parse().unwrap());
            if let Some(v) = entete {
                h.insert("icy-br", v.parse().unwrap());
            }
            (h, corps)
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let debit = tokio::task::spawn_blocking(move || {
        sonder_la_station(&url).unwrap().source_info.bitrate_kbps
    })
    .await
    .unwrap();
    server.abort();
    debit
}

#[tokio::test]
async fn radio_6821_la_sonde_releve_le_debit_annonce() {
    assert_eq!(debit_sonde(Some("128")).await, Some(128));
    assert_eq!(debit_sonde(Some("192,192")).await, Some(192));
    assert_eq!(
        debit_sonde(None).await,
        None,
        "sans en-tête, aucun débit inventé"
    );
    assert_eq!(debit_sonde(Some("haut")).await, None);
}

#[test]
fn radio_6821_lecture_de_l_entete_icy_br() {
    assert_eq!(debit_icy("128"), Some(128));
    assert_eq!(debit_icy(" 64 "), Some(64));
    assert_eq!(debit_icy("320,320"), Some(320));
    for illisible in ["", "0", "-128", "128k", "abc", "99999"] {
        assert_eq!(debit_icy(illisible), None, "« {illisible} »");
    }
}

// ---------------------------------------------------------------------------
// #7200 — Café del Mar (fil forum 2476, rc8.1 macOS)
// ---------------------------------------------------------------------------
//
// Le vrai flux, `https://streams.radio.co/se1a320b47/listen`, vérifié le
// 27/10/2026 : HTTP 200, `audio/mpeg`, `icy-br: 192`, `icy-name: Café del Mar
// Radio`. Ces en-têtes sont REJOUÉS ici par une station locale ; la vraie
// n'est jamais contactée. S'y ajoutent les deux formes soupçonnées d'arrêter
// la lecture : une redirection avant le flux (l'adresse publique de radio.co
// est un point d'entrée) et l'en-tête `icy-br` répété. Le corps est un MP3
// 192 kbit/s, 44,1 kHz, comme celui de la station.

/// Le chemin public du flux, sans extension, comme sur radio.co.
const CAFE_DEL_MAR_CHEMIN: &str = "/se1a320b47/listen";

/// Quatre secondes de MP3 192 kbit/s : la fixture (MPEG-1 couche III,
/// en-tête `FFFBB000` : 192 kbit/s, 44,1 kHz), puis ses trames répétées sans
/// leur étiquette ID3. Construit une seule fois pour tout le témoin.
fn mp3_192_en_boucle() -> &'static [u8] {
    static CORPS: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    CORPS.get_or_init(|| {
        let fixture: &[u8] = include_bytes!("../../tests/fixtures/test.mp3");
        let premiere_trame = fixture
            .windows(2)
            .position(|w| w == [0xFF, 0xFB])
            .expect("la fixture MP3 a une trame");
        assert_eq!(
            &fixture[premiere_trame..premiere_trame + 3],
            &[0xFF, 0xFB, 0xB0],
            "la fixture doit rester un MP3 192 kbit/s à 44,1 kHz, comme le flux"
        );
        let mut corps = fixture.to_vec();
        for _ in 0..3 {
            corps.extend_from_slice(&fixture[premiere_trame..]);
        }
        corps
    })
}

/// La station rejouée : `CAFE_DEL_MAR_CHEMIN` répond par une redirection
/// quand `redirige`, sinon directement par le flux, avec les en-têtes réels.
async fn station_cafe_del_mar(redirige: bool) -> (String, tokio::task::JoinHandle<()>) {
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    let corps = mp3_192_en_boucle();
    let flux = move || async move {
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("audio/mpeg"));
        // `icy-br` répété : deux lignes, comme le renvoient certains relais.
        h.append("icy-br", HeaderValue::from_static("192"));
        h.append("icy-br", HeaderValue::from_static("192"));
        // Le nom porte un « é » : un octet hors ASCII dans un en-tête.
        h.insert(
            "icy-name",
            HeaderValue::from_bytes("Café del Mar Radio".as_bytes()).unwrap(),
        );
        h.insert("icy-pub", HeaderValue::from_static("1"));
        (h, corps)
    };
    let mut app = axum::Router::new().route("/relais/se1a320b47/listen", axum::routing::get(flux));
    app = if redirige {
        app.route(
            CAFE_DEL_MAR_CHEMIN,
            axum::routing::get(|| async {
                (
                    StatusCode::FOUND,
                    [("location", "/relais/se1a320b47/listen")],
                )
            }),
        )
    } else {
        app.route(CAFE_DEL_MAR_CHEMIN, axum::routing::get(flux))
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{CAFE_DEL_MAR_CHEMIN}",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, server)
}

/// Décode la station rejouée vers une sortie factice — un canal que le test
/// vide, aucun périphérique audio — jusqu'à `secondes` de PCM 16 bits
/// stéréo à 44,1 kHz, puis raccroche. Rend les octets reçus, l'issue du
/// décodeur et la source publiée sur la session.
async fn decoder_cafe_del_mar(
    redirige: bool,
    secondes: usize,
) -> (usize, Result<(), String>, Option<RadioSourceInfo>) {
    let (url, server) = station_cafe_del_mar(redirige).await;
    let streamer = AudioStreamer::new(8080);
    let (id, _tx_de_la_session, data_ready, session) = streamer
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
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(url, tx, data_ready, session, None, None, false, None)
    });
    let voulu = secondes * 44_100 * 2 * 2;
    let mut recu = 0usize;
    let lecture = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while recu < voulu {
            match rx.recv().await {
                Some(bloc) => recu += bloc.len(),
                None => break,
            }
        }
    })
    .await;
    // La sortie factice raccroche : le décodeur doit s'arrêter de lui-même.
    drop(rx);
    let issue = tokio::time::timeout(std::time::Duration::from_secs(10), decodeur)
        .await
        .expect("le décodeur s'arrête quand la sortie raccroche")
        .unwrap();
    assert!(
        lecture.is_ok(),
        "décodage trop lent : {recu} octets en 30 s"
    );
    let source = streamer
        .stream_output_wire(&id)
        .await
        .and_then(|w| w.radio_source);
    server.abort();
    streamer.remove_session(&id).await;
    (recu, issue, source)
}

/// #7200 — les en-têtes réels de Café del Mar, avec ou sans redirection
/// devant le flux, donnent trois secondes de PCM décodé, le format MP3 et le
/// débit annoncé de 192 kbit/s. Témoin : si l'un de ces en-têtes ou la
/// redirection arrêtait la lecture côté serveur, `recu` resterait à zéro et
/// l'issue du décodeur nommerait l'étape fautive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radio_7200_cafe_del_mar_se_decode_avec_ses_en_tetes_reels() {
    for redirige in [false, true] {
        let (recu, issue, source) = decoder_cafe_del_mar(redirige, 3).await;
        assert_eq!(
            issue,
            Ok(()),
            "Café del Mar (redirection : {redirige}) : le décodeur a échoué"
        );
        assert!(
            recu >= 3 * 44_100 * 4,
            "Café del Mar (redirection : {redirige}) : {recu} octets de PCM, \
             moins de trois secondes — la station resterait muette"
        );
        let source = source.expect("la source de la station est publiée");
        assert_eq!(source.format, Some("mp3"), "redirection : {redirige}");
        assert_eq!(source.sample_rate, Some(44_100), "redirection : {redirige}");
        assert_eq!(
            source.bitrate_kbps,
            Some(192),
            "Café del Mar (redirection : {redirige}) : `icy-br: 192` répété doit \
             donner 192 kbit/s"
        );
    }
}
