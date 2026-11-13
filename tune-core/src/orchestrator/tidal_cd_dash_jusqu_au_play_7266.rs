//! #7266 — de bout en bout : un FLAC CD TIDAL (16 bits / 44,1 kHz) en DASH,
//! de l'ordre de lecture jusqu'au `Play` reçu par un renderer DLNA.
//!
//! Sur une zone DLNA en rc8.1, ce FLAC démarrait après 11,6 s et 30,7 s ;
//! en rc10, jusqu'à 60 s sur les pistes de 60 Mo. Depuis la rc8 (#6334), la
//! v2 `trackManifests` rend le FLAC CD en MPEG-DASH à SegmentTemplate : Tune
//! demandait les segments un par un au CDN, puis décodait la piste entière et
//! la ré-encodait en FLAC, avant d'envoyer la moindre URL au renderer.
//!
//! ## Le banc
//!
//! Tout est sur la boucle locale, rien ne sort de la machine, aucun son :
//! - **faux TIDAL** : `trackManifests` rend un MPD SegmentTemplate (codec
//!   `flac`, 44 100 Hz) ; la v1 répond 404 ;
//! - **faux CDN** : l'init puis les segments d'un VRAI FLAC en fMP4,
//!   fabriqué ici depuis un FLAC encodé par Tune (trames recopiées dans des
//!   `moof`/`mdat`, `dfLa` porteur du STREAMINFO). Chaque segment met
//!   [`LATENCE_MS`] à partir, une latence de CDN ordinaire vue d'une box ;
//! - **faux renderer DLNA** : il répond aux actions SOAP de la vraie
//!   `DlnaOutput`, horodate `SetAVTransportURI` et `Play`, puis va chercher
//!   l'URL reçue comme un renderer et horodate sa première réponse ;
//! - **serveur de flux** : un petit serveur sur le port du `AudioStreamer`
//!   sert le fichier de la session (le vrai est dans `tune-stream-http`, que
//!   `tune-core` ne voit pas).
//!
//! Chaque étape du dossier est horodatée depuis l'appel à `play()`, et le
//! tableau s'affiche avec `--nocapture`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;

use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::streaming::tidal::TidalService;

const PISTE: &str = "72660001";
const SR: u32 = 44_100;
/// Une piste courte, découpée en autant de segments qu'une piste réelle de
/// 4 min (TIDAL : ~4 s par segment). C'est le nombre d'allers-retours, pas
/// la longueur, qui retenait le départ.
const SECONDES: u32 = 30;
const SEGMENTS: usize = 60;
const LATENCE_MS: u64 = 100;
/// La borne du témoin, de l'appel à `play()` au `Play` reçu. Avant #7266 :
/// 60 × 100 ms de segments un par un, puis le décodage et le ré-encodage
/// de la piste entière. Après : six segments en vol (~1 s), puis un remux.
const BORNE_PLAY: Duration = Duration::from_secs(3);
/// La borne de l'étape « transcodage ou remux », du dernier segment reçu au
/// `SetAVTransportURI`. Le remux des 30 s recopie des trames (quelques
/// centièmes de seconde) ; le décodage puis le ré-encodage de la piste
/// entière en prennent plus d'une seconde ici, et ~12 s sur 4 min.
const BORNE_REMUX: Duration = Duration::from_millis(750);

// ── Le FLAC CD en fMP4 ────────────────────────────────────────────────────

/// Le PCM 16 bits stéréo de la piste : deux sinus et un bruit bas, assez
/// riches pour que le FLAC ait de vraies trames à recopier.
fn pcm_cd() -> Vec<u8> {
    let mut x: u32 = 7266;
    (0..SR * SECONDES)
        .flat_map(|n| {
            let t = n as f32 / SR as f32;
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let bruit = ((x >> 16) as i16 as f32) / 64.0;
            let g = (8000.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() + bruit) as i16;
            let d = (8000.0 * (2.0 * std::f32::consts::PI * 660.0 * t).sin() - bruit) as i16;
            [g.to_le_bytes(), d.to_le_bytes()].concat()
        })
        .collect()
}

async fn flac_cd(pcm: &[u8]) -> Vec<u8> {
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    enc.write(pcm).await.unwrap();
    enc.finish().await.unwrap()
}

/// Le STREAMINFO (34 octets) et les trames du FLAC, chacune avec son nombre
/// d'échantillons, lues par le démultiplexeur FLAC de symphonia.
fn trames_flac(flac: &[u8]) -> ([u8; 34], Vec<(Vec<u8>, u32)>) {
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    assert_eq!(&flac[..4], b"fLaC");
    assert_eq!(flac[4] & 0x7f, 0, "STREAMINFO en premier bloc");
    let mut info = [0u8; 34];
    info.copy_from_slice(&flac[8..42]);
    let mss = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(flac.to_vec())),
        Default::default(),
    );
    let mut hint = Hint::new();
    hint.with_extension("flac");
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    let piste = format.default_track(TrackType::Audio).unwrap().id;
    let mut trames = Vec::new();
    while let Ok(Some(p)) = format.next_packet() {
        if p.track_id == piste {
            trames.push((p.data.to_vec(), p.dur.get() as u32));
        }
    }
    (info, trames)
}

fn boite(nom: &[u8; 4], corps: &[u8]) -> Vec<u8> {
    let mut b = ((corps.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(nom);
    b.extend_from_slice(corps);
    b
}

fn boite_pleine(nom: &[u8; 4], version: u8, drapeaux: u32, corps: &[u8]) -> Vec<u8> {
    let mut c = vec![version];
    c.extend_from_slice(&drapeaux.to_be_bytes()[1..]);
    c.extend_from_slice(corps);
    boite(nom, &c)
}

const MATRICE: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrice() -> Vec<u8> {
    MATRICE.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// Le segment d'initialisation : `ftyp` + `moov` d'une piste audio `fLaC`
/// dont le `dfLa` porte le STREAMINFO, `mvex` pour les fragments.
fn segment_init(info: &[u8; 34]) -> Vec<u8> {
    let ftyp = boite(b"ftyp", b"iso6\0\0\0\0iso6mp41");
    let mut mvhd = vec![0u8; 8]; // création, modification
    mvhd.extend_from_slice(&SR.to_be_bytes());
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // durée (fragments)
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
    mvhd.extend_from_slice(&[0u8; 10]);
    mvhd.extend_from_slice(&matrice());
    mvhd.extend_from_slice(&[0u8; 24]);
    mvhd.extend_from_slice(&2u32.to_be_bytes());
    let mvhd = boite_pleine(b"mvhd", 0, 0, &mvhd);

    let mut tkhd = vec![0u8; 8];
    tkhd.extend_from_slice(&1u32.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 4]);
    tkhd.extend_from_slice(&0u32.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 8]);
    tkhd.extend_from_slice(&[0u8; 4]); // couche, groupe
    tkhd.extend_from_slice(&0x0100u16.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 2]);
    tkhd.extend_from_slice(&matrice());
    tkhd.extend_from_slice(&[0u8; 8]);
    let tkhd = boite_pleine(b"tkhd", 0, 7, &tkhd);

    let mut mdhd = vec![0u8; 8];
    mdhd.extend_from_slice(&SR.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&0x55c4u16.to_be_bytes());
    mdhd.extend_from_slice(&[0u8; 2]);
    let mdhd = boite_pleine(b"mdhd", 0, 0, &mdhd);
    let mut hdlr = vec![0u8; 4];
    hdlr.extend_from_slice(b"soun");
    hdlr.extend_from_slice(&[0u8; 13]);
    let hdlr = boite_pleine(b"hdlr", 0, 0, &hdlr);

    // dfLa : un seul bloc de métadonnées, STREAMINFO, marqué dernier.
    let mut dfla = vec![0x80, 0, 0, 34];
    dfla.extend_from_slice(info);
    let dfla = boite_pleine(b"dfLa", 0, 0, &dfla);
    let mut flac = vec![0u8; 6];
    flac.extend_from_slice(&1u16.to_be_bytes());
    flac.extend_from_slice(&[0u8; 8]);
    flac.extend_from_slice(&2u16.to_be_bytes());
    flac.extend_from_slice(&16u16.to_be_bytes());
    flac.extend_from_slice(&[0u8; 4]);
    flac.extend_from_slice(&(SR << 16).to_be_bytes());
    flac.extend_from_slice(&dfla);
    let entree = boite(b"fLaC", &flac);
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&entree);
    let stbl = [
        boite_pleine(b"stsd", 0, 0, &stsd),
        boite_pleine(b"stts", 0, 0, &0u32.to_be_bytes()),
        boite_pleine(b"stsc", 0, 0, &0u32.to_be_bytes()),
        boite_pleine(b"stsz", 0, 0, &[0u8; 8]),
        boite_pleine(b"stco", 0, 0, &0u32.to_be_bytes()),
    ]
    .concat();
    let mut dref = 1u32.to_be_bytes().to_vec();
    dref.extend_from_slice(&boite_pleine(b"url ", 0, 1, &[]));
    let dinf = boite(b"dinf", &boite_pleine(b"dref", 0, 0, &dref));
    let minf = boite(
        b"minf",
        &[
            boite_pleine(b"smhd", 0, 0, &[0u8; 4]),
            dinf,
            boite(b"stbl", &stbl),
        ]
        .concat(),
    );
    let mdia = boite(b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = boite(b"trak", &[tkhd, mdia].concat());
    let mut trex = 1u32.to_be_bytes().to_vec();
    trex.extend_from_slice(&1u32.to_be_bytes());
    trex.extend_from_slice(&[0u8; 12]);
    let mvex = boite(b"mvex", &boite_pleine(b"trex", 0, 0, &trex));
    [ftyp, boite(b"moov", &[mvhd, trak, mvex].concat())].concat()
}

/// Un segment média : `moof` (`trun` avec durée et taille de chaque trame)
/// puis `mdat` des trames FLAC, recopiées telles quelles.
fn segment_media(numero: u32, debut: u64, trames: &[(Vec<u8>, u32)]) -> Vec<u8> {
    let mfhd = boite_pleine(b"mfhd", 0, 0, &numero.to_be_bytes());
    // default-base-is-moof : les décalages partent du début du moof.
    let tfhd = boite_pleine(b"tfhd", 0, 0x02_0000, &1u32.to_be_bytes());
    let tfdt = boite_pleine(b"tfdt", 1, 0, &debut.to_be_bytes());
    let trun_taille = 12 + 8 + trames.len() * 8;
    let moof_taille = 8 + mfhd.len() + 8 + tfhd.len() + tfdt.len() + trun_taille;
    let mut trun = (trames.len() as u32).to_be_bytes().to_vec();
    trun.extend_from_slice(&((moof_taille + 8) as i32).to_be_bytes());
    for (octets, duree) in trames {
        trun.extend_from_slice(&duree.to_be_bytes());
        trun.extend_from_slice(&(octets.len() as u32).to_be_bytes());
    }
    let trun = boite_pleine(b"trun", 0, 0x0301, &trun);
    let traf = boite(b"traf", &[tfhd, tfdt, trun].concat());
    let moof = boite(b"moof", &[mfhd, traf].concat());
    assert_eq!(moof.len(), moof_taille);
    let mdat: Vec<u8> = trames.iter().flat_map(|(o, _)| o.clone()).collect();
    [moof, boite(b"mdat", &mdat)].concat()
}

/// La piste DASH : l'init, puis [`SEGMENTS`] segments au plus.
struct PisteDash {
    init: Vec<u8>,
    segments: Vec<(Vec<u8>, u32)>,
}

fn decouper(flac: &[u8]) -> PisteDash {
    let (info, trames) = trames_flac(flac);
    assert!(trames.len() >= SEGMENTS, "{} trames", trames.len());
    // Exactement SEGMENTS segments, les trames réparties au plus juste.
    let n = trames.len();
    let mut debut = 0u64;
    let segments = (0..SEGMENTS)
        .map(|i| {
            let lot = &trames[i * n / SEGMENTS..(i + 1) * n / SEGMENTS];
            let duree: u32 = lot.iter().map(|(_, d)| d).sum();
            let s = segment_media(i as u32 + 1, debut, lot);
            debut += duree as u64;
            (s, duree)
        })
        .collect();
    PisteDash {
        init: segment_init(&info),
        segments,
    }
}

// ── Faux TIDAL et faux CDN ────────────────────────────────────────────────

#[derive(Default)]
struct JournalTidal {
    manifeste: Option<Instant>,
    init: Option<Instant>,
    dernier_segment: Option<Instant>,
    /// Segments média servis jusqu'ici.
    servis: usize,
}

fn mpd(base: &str, codec: &str, piste: &PisteDash) -> String {
    let timeline: String = piste
        .segments
        .iter()
        .map(|(_, d)| format!(r#"<S d="{d}"/>"#))
        .collect();
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT{secondes}S">"#,
            r#"<Period id="0"><AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" segmentAlignment="true">"#,
            r#"<Representation id="FLAC,44100,16" codecs="{codec}" bandwidth="1411200" audioSamplingRate="44100">"#,
            r#"<SegmentTemplate timescale="44100" initialization="{base}/cdn/0.mp4?token=essai" media="{base}/cdn/$Number$.mp4?token=essai" startNumber="1">"#,
            r#"<SegmentTimeline>{timeline}</SegmentTimeline></SegmentTemplate>"#,
            r#"</Representation></AdaptationSet></Period></MPD>"#
        ),
        secondes = SECONDES,
        codec = codec,
        base = base,
        timeline = timeline,
    )
}

/// Le faux TIDAL et son CDN. `codec` est celui que le MPD annonce, et que
/// `formats` reprend (`FLAC` ou `AACLC`).
async fn faux_tidal(
    piste: Arc<PisteDash>,
    codec: &'static str,
) -> (String, Arc<Mutex<JournalTidal>>) {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    #[derive(Clone)]
    struct Etat {
        base: String,
        codec: &'static str,
        piste: Arc<PisteDash>,
        journal: Arc<Mutex<JournalTidal>>,
    }

    async fn manifeste(State(e): State<Etat>) -> axum::response::Response {
        e.journal
            .lock()
            .unwrap()
            .manifeste
            .get_or_insert(Instant::now());
        let format = if e.codec == "flac" { "FLAC" } else { "AACLC" };
        let uri = format!(
            "data:application/dash+xml;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(mpd(&e.base, e.codec, &e.piste))
        );
        axum::Json(
            serde_json::json!({"data": {"id": PISTE, "type": "trackManifests",
            "attributes": {"trackPresentation": "FULL", "uri": uri, "formats": [format]}}}),
        )
        .into_response()
    }

    async fn fiche() -> axum::response::Response {
        axum::Json(serde_json::json!({
            "id": 72660001, "title": "Témoin 7266", "duration": SECONDES,
            "audioQuality": "LOSSLESS",
            "artist": {"id": 1, "name": "Banc"}, "album": {"id": 2, "title": "DASH"},
        }))
        .into_response()
    }

    async fn segment(
        State(e): State<Etat>,
        Path(fichier): Path<String>,
    ) -> axum::response::Response {
        let numero: usize = fichier
            .trim_end_matches(".mp4")
            .parse()
            .unwrap_or(usize::MAX);
        if numero == 0 {
            e.journal.lock().unwrap().init.get_or_insert(Instant::now());
            return e.piste.init.clone().into_response();
        }
        let Some((octets, _)) = e.piste.segments.get(numero.wrapping_sub(1)) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        tokio::time::sleep(Duration::from_millis(LATENCE_MS)).await;
        let mut j = e.journal.lock().unwrap();
        j.servis += 1;
        let maintenant = Instant::now();
        j.dernier_segment = Some(j.dernier_segment.map_or(maintenant, |t| t.max(maintenant)));
        octets.clone().into_response()
    }

    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    let journal = Arc::new(Mutex::new(JournalTidal::default()));
    let app = axum::Router::new()
        .route("/trackManifests/{id}", get(manifeste))
        .route(
            "/tracks/{id}/playbackinfopostpaywall",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route("/tracks/{id}", get(fiche))
        .route("/cdn/{fichier}", get(segment))
        .with_state(Etat {
            base: base.clone(),
            codec,
            piste,
            journal: journal.clone(),
        });
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (base, journal)
}

// ── Faux renderer DLNA ────────────────────────────────────────────────────

#[derive(Default)]
struct JournalRenderer {
    set_uri: Option<Instant>,
    play: Option<Instant>,
    premier_get: Option<Instant>,
    uri: String,
    /// Les octets que le renderer a reçus en suivant l'URL.
    recus: Option<Vec<u8>>,
    type_mime: String,
}

fn entre<'a>(corps: &'a str, balise: &str) -> &'a str {
    corps
        .split_once(&format!("<{balise}>"))
        .and_then(|(_, reste)| reste.split_once(&format!("</{balise}>")))
        .map(|(v, _)| v)
        .unwrap_or("")
}

/// Le renderer : AVTransport, RenderingControl et ConnectionManager sur
/// `/control`. Il annonce `audio/flac`. Au `Play`, il va chercher l'URL
/// posée, comme un vrai renderer.
async fn faux_renderer() -> (String, Arc<Mutex<JournalRenderer>>) {
    use axum::routing::post;
    let journal = Arc::new(Mutex::new(JournalRenderer::default()));
    let j = journal.clone();
    let app = axum::Router::new().route(
        "/control",
        post(move |entetes: axum::http::HeaderMap, corps: String| {
            let journal = j.clone();
            async move {
                let action = entetes
                    .get("SOAPAction")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .trim_matches('"')
                    .rsplit('#')
                    .next()
                    .unwrap_or("")
                    .to_owned();
                let mut jr = journal.lock().unwrap();
                let interieur = match action.as_str() {
                    "SetAVTransportURI" => {
                        jr.set_uri.get_or_insert(Instant::now());
                        jr.uri = entre(&corps, "CurrentURI").replace("&amp;", "&");
                        String::new()
                    }
                    "Play" => {
                        jr.play.get_or_insert(Instant::now());
                        let (uri, journal) = (jr.uri.clone(), journal.clone());
                        tokio::spawn(async move {
                            let reponse = crate::http::client::shared()
                                .get(&uri)
                                .send()
                                .await
                                .expect("GET du renderer");
                            let mime = reponse
                                .headers()
                                .get("content-type")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_owned();
                            {
                                let mut jr = journal.lock().unwrap();
                                jr.premier_get.get_or_insert(Instant::now());
                                jr.type_mime = mime;
                            }
                            let octets = reponse.bytes().await.expect("corps servi");
                            journal.lock().unwrap().recus = Some(octets.to_vec());
                        });
                        String::new()
                    }
                    "GetTransportInfo" => format!(
                        "<CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
                        if jr.play.is_some() { "PLAYING" } else { "STOPPED" }
                    ),
                    "GetMediaInfo" => format!("<NrTracks>1</NrTracks><CurrentURI>{}</CurrentURI>", jr.uri),
                    "GetProtocolInfo" => "<Source></Source><Sink>http-get:*:audio/flac:*,http-get:*:audio/L16:*,http-get:*:audio/wav:*</Sink>".to_string(),
                    "GetVolume" => "<CurrentVolume>0</CurrentVolume>".to_string(),
                    "GetPositionInfo" => "<Track>1</Track><RelTime>0:00:00</RelTime>".to_string(),
                    _ => String::new(),
                };
                format!(
                    r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">{interieur}</u:{action}Response></s:Body></s:Envelope>"#
                )
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hote = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (hote, journal)
}

/// Le serveur de flux du banc, sur le port annoncé par l'`AudioStreamer` :
/// `GET /stream/{session}.{ext}` sert le fichier de la session.
async fn serveur_de_flux(
    ecoute: tokio::net::TcpListener,
    streamer: Arc<crate::http::streamer::AudioStreamer>,
) {
    use axum::extract::{Path, State};
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;

    async fn servir(
        State(streamer): State<Arc<crate::http::streamer::AudioStreamer>>,
        Path(nom): Path<String>,
    ) -> axum::response::Response {
        let id = nom.rsplit_once('.').map_or(nom.as_str(), |(id, _)| id);
        let sessions = streamer.sessions_state();
        let session = sessions.lock().await.get(id).cloned();
        let Some(session) = session else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let chemin = session.file_path.lock().await.clone();
        let Some(octets) = chemin.and_then(|c| std::fs::read(c).ok()) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        (
            [(header::CONTENT_TYPE, session.info.mime_type.clone())],
            octets,
        )
            .into_response()
    }

    let app = axum::Router::new()
        .route("/stream/{nom}", get(servir))
        .with_state(streamer);
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
}

// ── Le banc ───────────────────────────────────────────────────────────────

struct Mesure {
    debut: Instant,
    tidal: Arc<Mutex<JournalTidal>>,
    renderer: Arc<Mutex<JournalRenderer>>,
    /// Durée de `play()` jusqu'à son retour.
    play_rendu: Duration,
    resultat: Result<super::PlayResult, String>,
}

async fn jouer(piste: Arc<PisteDash>, codec: &'static str) -> Mesure {
    let ecoute_flux = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute_flux.local_addr().unwrap().port();
    let streamer = Arc::new(crate::http::streamer::AudioStreamer::new(port));
    serveur_de_flux(ecoute_flux, streamer.clone()).await;

    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        streamer,
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        Some("127.0.0.1".into()),
    );

    let (base, tidal) = faux_tidal(piste, codec).await;
    orch.services
        .lock()
        .await
        .register(Box::new(TidalService::pour_un_faux_tidal(base)));

    let (hote, renderer) = faux_renderer().await;
    let appareil = format!("dlna:uuid-renderer-7266-{}", uuid::Uuid::new_v4());
    let controle = format!("{hote}/control");
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::dlna::DlnaOutput::new(
            "Salon 7266".into(),
            appareil.clone(),
            hote.clone(),
            controle.clone(),
            controle.clone(),
            Some(controle),
        )));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon 7266", Some("dlna"), Some(&appareil))
        .unwrap();

    let req = PlayRequest {
        zone_id,
        output_device_id: Some(appareil),
        source: Some("tidal".into()),
        source_id: Some(PISTE.into()),
        title: Some("Témoin 7266".into()),
        artist_name: Some("Banc".into()),
        album_title: Some("DASH".into()),
        duration_ms: Some(SECONDES as i64 * 1000),
        ..Default::default()
    };
    let debut = Instant::now();
    let resultat = orch.play(req).await;
    let play_rendu = debut.elapsed();
    // Le GET du renderer part au Play : laisser le flux arriver en entier.
    for _ in 0..200 {
        if renderer.lock().unwrap().recus.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Mesure {
        debut,
        tidal,
        renderer,
        play_rendu,
        resultat,
    }
}

fn depuis(debut: Instant, t: Option<Instant>) -> String {
    t.map_or("—".into(), |t| {
        format!("{:.2} s", t.duration_since(debut).as_secs_f64())
    })
}

/// Le tableau du dossier, une ligne par étape, en temps écoulé depuis
/// l'appel à `play()`.
fn tableau(m: &Mesure) -> String {
    let t = m.tidal.lock().unwrap();
    let r = m.renderer.lock().unwrap();
    let ecart = |a: Option<Instant>, b: Option<Instant>| match (a, b) {
        (Some(a), Some(b)) if b >= a => format!("{:.2} s", (b - a).as_secs_f64()),
        _ => "—".into(),
    };
    format!(
        "| Étape | Instant (depuis play()) | Durée de l'étape |\n\
         |---|---|---|\n\
         | Résolution de l'URL et du manifeste (trackManifests reçu) | {} | {} |\n\
         | Premier octet (segment d'init demandé) | {} | {} |\n\
         | Prétampon (dernier segment servi) | {} | {} |\n\
         | Transcodage ou remux, jusqu'au SetAVTransportURI | {} | {} |\n\
         | SetAVTransportURI puis Play | {} / {} | {} |\n\
         | Premier GET du renderer (réponse reçue) | {} | {} |\n",
        depuis(m.debut, t.manifeste),
        ecart(Some(m.debut), t.manifeste),
        depuis(m.debut, t.init),
        ecart(t.manifeste, t.init),
        depuis(m.debut, t.dernier_segment),
        ecart(t.init, t.dernier_segment),
        depuis(m.debut, r.set_uri),
        ecart(t.dernier_segment, r.set_uri),
        depuis(m.debut, r.set_uri),
        depuis(m.debut, r.play),
        ecart(r.set_uri, r.play),
        depuis(m.debut, r.premier_get),
        ecart(r.play, r.premier_get),
    )
}

fn pcm_de(octets: &[u8], ext: &str) -> (Vec<u8>, u32, u16) {
    let chemin = crate::test_scratch::scratch_file("tune-7266-banc", &format!(".{ext}"));
    std::fs::write(chemin.path(), octets).unwrap();
    let d = crate::audio::decode::decode_to_pcm(&chemin.as_str(), None, None, 0.0, 0.0).unwrap();
    (d.pcm_bytes(), d.sample_rate, d.bit_depth)
}

/// LE TÉMOIN DU DOSSIER : un FLAC CD TIDAL en DASH (60 segments, 100 ms
/// chacun au CDN) arrive au renderer DLNA sous [`BORNE_PLAY`], du FLAC
/// bit-identique à la source. Avant #7266 : segments un par un (6 s au
/// moins), puis décodage et ré-encodage de la piste entière.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_flac_cd_tidal_dash_recoit_son_play_sous_la_borne_7266() {
    let pcm = pcm_cd();
    let flac = flac_cd(&pcm).await;
    let piste = Arc::new(decouper(&flac));
    let segments = piste.segments.len();

    let m = jouer(piste, "flac").await;
    let tableau = tableau(&m);
    println!(
        "#7266 — FLAC CD 16/44,1, {segments} segments, {LATENCE_MS} ms par segment \
         au CDN, play() rendu en {:.2} s :\n{tableau}",
        m.play_rendu.as_secs_f64()
    );
    let resultat = m.resultat.as_ref().expect("la lecture part");
    assert!(
        resultat.output_sent,
        "rien d'envoyé au renderer : {resultat:?}"
    );

    let r = m.renderer.lock().unwrap();
    let play = r
        .play
        .expect("le renderer n'a pas reçu de Play")
        .duration_since(m.debut);
    let un_par_un = Duration::from_millis(LATENCE_MS * segments as u64);
    assert!(
        play < BORNE_PLAY,
        "Play reçu par le renderer {play:?} après l'ordre de lecture (borne \
         {BORNE_PLAY:?} ; {segments} segments un par un : {un_par_un:?} au moins) : \
         le Play attend encore le dernier segment requête après requête, ou le \
         décodage et le ré-encodage de la piste entière\n{tableau}"
    );
    let dernier_segment = m.tidal.lock().unwrap().dernier_segment;
    let remux = r
        .set_uri
        .zip(dernier_segment)
        .map(|(u, s)| u.saturating_duration_since(s))
        .expect("SetAVTransportURI et segments horodatés");
    assert!(
        remux < BORNE_REMUX,
        "{remux:?} entre le dernier segment et le SetAVTransportURI (borne \
         {BORNE_REMUX:?}) : le FLAC est encore décodé puis ré-encodé en entier \
         au lieu d'être remuxé\n{tableau}"
    );
    assert!(r.premier_get.is_some(), "le renderer n'a pas pu lire l'URL");

    // Sans dégrader la qualité : du FLAC, 16/44,1, le PCM de la source à
    // l'échantillon près.
    assert_eq!(r.type_mime, "audio/flac", "{}", r.uri);
    let recus = r.recus.clone().expect("flux reçu en entier");
    assert_eq!(&recus[..4], b"fLaC");
    let (servi, sr, bd) = pcm_de(&recus, "flac");
    assert_eq!((sr, bd), (SR, 16));
    assert!(
        servi == pcm,
        "le FLAC servi ne rend pas le PCM de la source ({} octets contre {})",
        servi.len(),
        pcm.len()
    );
}

/// Relecture de la PR #7924 (B1) : un MPD qui annonce du FLAC mais dont les
/// trames n'en sont pas (ici un AAC fragmenté) fait échouer le remux ; la
/// piste retombe sur le décodage et joue, au lieu d'échouer en « DASH
/// transcode failed ».
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_fmp4_sans_trames_flac_retombe_sur_le_decodage_7266() {
    let aac = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/aac320_frag.mp4"
    ))
    .unwrap();
    // L'AAC fragmenté tel quel en un seul segment après une init vide : le
    // fichier reconstitué par Tune est l'AAC d'origine, à l'octet près.
    let piste = Arc::new(PisteDash {
        init: aac,
        segments: vec![(Vec::new(), SR)],
    });
    let m = jouer(piste, "flac").await;
    let resultat = m.resultat.as_ref().expect("la lecture part");
    assert!(
        resultat.output_sent && resultat.error.is_none(),
        "un remux en échec doit retomber sur le décodage : {resultat:?}"
    );
    let r = m.renderer.lock().unwrap();
    assert!(r.play.is_some(), "aucun Play reçu");
    let recus = r.recus.clone().expect("flux reçu");
    let (servi, _, _) = pcm_de(&recus, "flac");
    assert!(!servi.is_empty(), "flux vide");
}

/// Un AAC DASH annoncé AAC (qualité « Haute ») garde le décodage : servi en
/// FLAC 16 bits comme avant #7266.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_aac_dash_annonce_aac_est_decode_et_joue_7266() {
    let aac = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/aac320_frag.mp4"
    ))
    .unwrap();
    let piste = Arc::new(PisteDash {
        init: aac,
        segments: vec![(Vec::new(), SR)],
    });
    let m = jouer(piste, "mp4a.40.2").await;
    let resultat = m.resultat.as_ref().expect("la lecture part");
    assert!(
        resultat.output_sent && resultat.error.is_none(),
        "{resultat:?}"
    );
    let r = m.renderer.lock().unwrap();
    let recus = r.recus.clone().expect("flux reçu");
    assert_eq!(&recus[..4], b"fLaC", "décodé puis encodé en FLAC");
}

// ── Sortie locale (Windows ASIO, macOS) ───────────────────────────────────

/// Les octets PCM d'un flux WAV, en-tête retiré.
fn pcm_du_wav(octets: &[u8]) -> &[u8] {
    if !octets.starts_with(b"RIFF") {
        return octets;
    }
    let mut i = 12;
    while i + 8 <= octets.len() {
        let taille = u32::from_le_bytes(octets[i + 4..i + 8].try_into().unwrap()) as usize;
        if &octets[i..i + 4] == b"data" {
            return &octets[i + 8..];
        }
        i += 8 + taille;
    }
    &[]
}

/// LE TÉMOIN DE LA SORTIE LOCALE (fil forum 2719, Windows ASIO ; macOS) : la
/// première note d'un FLAC CD TIDAL en DASH n'attend plus le dernier segment.
/// Avant : `get_track_url` assemblait les 60 segments sur disque avant que
/// la session WAV reçoive un seul échantillon (18,6 s en rc10 sur 86
/// segments). Après : le décodage suit la frontière d'écriture, et la piste
/// entière arrive quand même, échantillon pour échantillon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sur_une_sortie_locale_la_premiere_note_n_attend_pas_le_dernier_segment_7266() {
    let pcm = pcm_cd();
    let flac = flac_cd(&pcm).await;
    let piste = Arc::new(decouper(&flac));

    let ecoute_flux = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute_flux.local_addr().unwrap().port();
    drop(ecoute_flux);
    let streamer = Arc::new(crate::http::streamer::AudioStreamer::new(port));
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        streamer.clone(),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        Some("127.0.0.1".into()),
    );
    let (base, tidal) = faux_tidal(piste, "flac").await;
    orch.services
        .lock()
        .await
        .register(Box::new(TidalService::pour_un_faux_tidal(base)));
    let appareil = "local:Banc 7266".to_string();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Bureau 7266", Some("local"), Some(&appareil))
        .unwrap();
    let req = PlayRequest {
        zone_id,
        output_device_id: Some(appareil),
        source: Some("tidal".into()),
        source_id: Some(PISTE.into()),
        title: Some("Témoin 7266".into()),
        artist_name: Some("Banc".into()),
        album_title: Some("DASH".into()),
        duration_ms: Some(SECONDES as i64 * 1000),
        ..Default::default()
    };
    assert!(super::resolve_stream::decoder_pendant_le_telechargement(
        &req
    ));

    let debut = Instant::now();
    let resolu = orch
        .resolve_streaming_url("tidal", &req)
        .await
        .expect("la piste se résout");
    let resolution = debut.elapsed();
    let servis_a_la_resolution = tidal.lock().unwrap().servis;
    let sid = resolu.stream_id.expect("une session WAV");
    let session = streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session ouverte");

    let mut recu = Vec::new();
    let mut premiere_note: Option<(Duration, usize)> = None;
    while let Ok(Some(morceau)) =
        tokio::time::timeout(Duration::from_secs(10), session.recv_chunk()).await
    {
        recu.extend_from_slice(&morceau);
        if premiere_note.is_none() && !pcm_du_wav(&recu).is_empty() {
            premiere_note = Some((debut.elapsed(), tidal.lock().unwrap().servis));
        }
    }
    let (instant, servis) = premiere_note.expect("aucun échantillon reçu");
    println!(
        "#7266 sortie locale — {SEGMENTS} segments, {LATENCE_MS} ms chacun : résolution \
         {:.2} s ({servis_a_la_resolution} segments servis), première note {:.2} s \
         ({servis} segments servis)",
        resolution.as_secs_f64(),
        instant.as_secs_f64()
    );
    assert!(
        servis < SEGMENTS / 2,
        "première note après {servis}/{SEGMENTS} segments servis ({instant:?}) : la \
         sortie locale attend encore la piste entière"
    );

    // La piste entière, sans perte : le WAV 32 bits de la sortie locale porte
    // les échantillons 16 bits de la source.
    let servi = pcm_du_wav(&recu);
    let source: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let rendu: Vec<i16> = servi
        .chunks_exact(4)
        .map(|c| (i32::from_le_bytes([c[0], c[1], c[2], c[3]]) >> 16) as i16)
        .collect();
    assert_eq!(rendu.len(), source.len(), "piste tronquée ou allongée");
    assert!(
        rendu == source,
        "les échantillons servis diffèrent de la source"
    );
}
