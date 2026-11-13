//! #7266 — sur une sortie LOCALE ou OAAT, un FLAC CD TIDAL en DASH ne fait
//! plus attendre sa première note jusqu'à la fin du téléchargement.
//!
//! Fil 2719 (Windows, ASIO) : 86 segments, 18,6 s de téléchargement sur 20 s
//! avant le premier PCM. `get_track_url` assemblait le fMP4 entier avant de
//! rendre son `file://`. Désormais, pour une sortie qui lit une session WAV
//! sans longueur annoncée, il le rend dès l'init et le premier segment, et le
//! décodeur suit l'écriture (`audio/dash_growth.rs`).
//!
//! Le banc : le faux TIDAL et le vrai FLAC en fMP4 de
//! `tidal_cd_dash_jusqu_au_play_7266.rs`, avec une latence de CDN réglable
//! et un segment qui peut échouer. Aucune sortie audio : la session WAV est
//! lue directement, rien n'est joué.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;

use super::tidal_cd_dash_jusqu_au_play_7266::{
    PISTE, PisteDash, SECONDES, SR, decouper, flac_cd, mpd, pcm_cd, pcm_de,
};
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::streaming::StreamingService;
use crate::streaming::tidal::TidalService;

/// 60 segments, six en vol, 150 ms chacun : ~1,5 s de téléchargement.
const LATENCE_MS: u64 = 150;

#[derive(Default)]
struct Journal {
    inits: AtomicUsize,
    dernier_segment: Mutex<Option<Instant>>,
}

/// Faux TIDAL : `trackManifests` rend le MPD, la v1 répond 404, le CDN sert
/// l'init puis chaque segment après [`LATENCE_MS`]. Le segment `en_echec`
/// répond toujours 503.
async fn faux_tidal(
    piste: Arc<PisteDash>,
    codec: &'static str,
    en_echec: Option<usize>,
) -> (String, Arc<Journal>) {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    #[derive(Clone)]
    struct Etat {
        base: String,
        codec: &'static str,
        piste: Arc<PisteDash>,
        journal: Arc<Journal>,
        en_echec: Option<usize>,
    }

    async fn manifeste(State(e): State<Etat>) -> axum::response::Response {
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
            e.journal.inits.fetch_add(1, Ordering::SeqCst);
            return e.piste.init.clone().into_response();
        }
        let Some((octets, _)) = e.piste.segments.get(numero.wrapping_sub(1)) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        tokio::time::sleep(Duration::from_millis(LATENCE_MS)).await;
        if Some(numero) == e.en_echec {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let maintenant = Instant::now();
        let mut d = e.journal.dernier_segment.lock().unwrap();
        *d = Some(d.map_or(maintenant, |t| t.max(maintenant)));
        octets.clone().into_response()
    }

    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    let journal = Arc::new(Journal::default());
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
            en_echec,
        });
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (base, journal)
}

async fn piste_cd() -> (Vec<u8>, Arc<PisteDash>) {
    let pcm = pcm_cd();
    let flac = flac_cd(&pcm).await;
    let piste = Arc::new(decouper(&flac));
    (pcm, piste)
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        Some("127.0.0.1".into()),
    )
}

/// Ce que la sortie locale aurait lu : les octets de la session WAV, et
/// l'instant où le premier PCM (au-delà de l'en-tête) est arrivé.
struct Lu {
    octets: Vec<u8>,
    premier_pcm: Option<Instant>,
    fin: Instant,
}

async fn lire_la_session(orch: &PlaybackOrchestrator, sid: &str) -> Lu {
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session inscrite");
    let mut octets = Vec::new();
    let mut premier_pcm = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), session.recv_chunk()).await {
            Ok(Some(c)) => {
                octets.extend_from_slice(&c);
                // Bien au-delà de tout en-tête WAV (44 à 80 octets).
                if premier_pcm.is_none() && octets.len() > 4096 {
                    premier_pcm = Some(Instant::now());
                }
            }
            Ok(None) => break,
            Err(_) => panic!(
                "la session WAV n'avance plus depuis 10 s ({} octets reçus) : \
                 le décodeur attend un segment qui ne viendra pas",
                octets.len()
            ),
        }
    }
    Lu {
        octets,
        premier_pcm,
        fin: Instant::now(),
    }
}

async fn resoudre_en_local(
    orch: &PlaybackOrchestrator,
    base: String,
    seek_ms: Option<u64>,
) -> (super::ResolvedStream, Instant, Duration) {
    resoudre_sur(orch, base, seek_ms, "local", "local:Casque").await
}

async fn resoudre_sur(
    orch: &PlaybackOrchestrator,
    base: String,
    seek_ms: Option<u64>,
    type_de_sortie: &str,
    appareil: &str,
) -> (super::ResolvedStream, Instant, Duration) {
    orch.services
        .lock()
        .await
        .register(Box::new(TidalService::pour_un_faux_tidal(base)));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Casque 7266", Some(type_de_sortie), Some(appareil))
        .unwrap();
    let req = PlayRequest {
        zone_id,
        output_device_id: Some(appareil.into()),
        source: Some("tidal".into()),
        source_id: Some(PISTE.into()),
        title: Some("Témoin 7266".into()),
        artist_name: Some("Banc".into()),
        album_title: Some("DASH".into()),
        duration_ms: Some(SECONDES as i64 * 1000),
        seek_ms,
        ..Default::default()
    };
    let debut = Instant::now();
    let r = orch.resolve_stream(&req).await.expect("résolution");
    (r, debut, debut.elapsed())
}

fn trames_wav(octets: &[u8]) -> (usize, u32) {
    let (pcm, sr, bd) = pcm_de(octets, "wav");
    (pcm.len() / (bd as usize / 8) / 2, sr)
}

/// LE TÉMOIN : sortie locale, le premier PCM arrive AVANT que le CDN ait
/// servi le dernier segment, et la piste arrive entière (aucune trame
/// perdue à la frontière d'écriture).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sortie_locale_la_premiere_note_precede_la_fin_du_telechargement_7266() {
    let (pcm, piste) = piste_cd().await;
    let (base, journal) = faux_tidal(piste, "flac", None).await;
    let orch = orchestrateur();
    let (r, debut, resolution) = resoudre_en_local(&orch, base, None).await;
    let sid = r.stream_id.clone().expect("session WAV");
    let lu = lire_la_session(&orch, &sid).await;

    let premier_pcm = lu.premier_pcm.expect("du PCM");
    let dernier_segment = journal
        .dernier_segment
        .lock()
        .unwrap()
        .expect("segments servis");
    println!(
        "#7266 local : résolution {:.2} s, premier PCM {:.2} s, dernier segment {:.2} s, fin {:.2} s",
        resolution.as_secs_f64(),
        premier_pcm.duration_since(debut).as_secs_f64(),
        dernier_segment.duration_since(debut).as_secs_f64(),
        lu.fin.duration_since(debut).as_secs_f64(),
    );
    assert!(
        premier_pcm < dernier_segment,
        "premier PCM {:?} APRÈS le dernier segment : la sortie locale attend \
         encore la fin du téléchargement",
        premier_pcm.duration_since(dernier_segment)
    );
    let (trames, sr) = trames_wav(&lu.octets);
    assert_eq!(sr, SR);
    assert_eq!(
        trames,
        pcm.len() / 4,
        "la piste n'est pas arrivée entière à la sortie locale"
    );
}

/// Un saut (reprise à 20 s) ne passe pas par le décodeur en croissance, qui
/// ne sait pas aller au-delà du téléchargé : fichier complet, comme avant,
/// et la lecture part bien de 20 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sortie_locale_un_saut_prend_le_fichier_complet_7266() {
    let (pcm, piste) = piste_cd().await;
    let (base, journal) = faux_tidal(piste, "flac", None).await;
    let orch = orchestrateur();
    let (r, debut, resolution) = resoudre_en_local(&orch, base, Some(20_000)).await;
    let dernier_segment = journal
        .dernier_segment
        .lock()
        .unwrap()
        .expect("segments servis");
    assert!(
        dernier_segment <= debut + resolution,
        "un saut doit attendre le fichier complet avant de rendre la session"
    );
    let lu = lire_la_session(&orch, &r.stream_id.clone().expect("session")).await;
    let (trames, _) = trames_wav(&lu.octets);
    let attendues = pcm.len() / 4 - 20 * SR as usize;
    assert!(
        trames.abs_diff(attendues) < SR as usize / 2,
        "{trames} trames lues après un saut à 20 s, ~{attendues} attendues"
    );
}

/// Un segment qui échoue (deux fois) au milieu de la piste : la session se
/// termine sans pendre, ET la zone reçoit une erreur de lecture — pas une fin
/// de piste muette qui ferait passer à la suivante comme si de rien n'était.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sortie_locale_un_segment_en_echec_termine_la_session_en_erreur_7266() {
    let (pcm, piste) = piste_cd().await;
    let (base, _) = faux_tidal(piste, "flac", Some(30)).await;
    let mut orch = orchestrateur();
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let mut evenements = bus.subscribe();
    orch.event_bus = Some(bus);
    let (r, _, _) = resoudre_en_local(&orch, base, None).await;
    let lu = lire_la_session(&orch, &r.stream_id.clone().expect("session")).await;
    let (trames, _) = if lu.octets.len() > 4096 {
        trames_wav(&lu.octets)
    } else {
        (0, SR)
    };
    assert!(
        trames < pcm.len() / 4,
        "segment 30 refusé : {trames} trames servies, autant que la piste entière"
    );
    let mut erreur = None;
    let limite = Instant::now() + Duration::from_secs(5);
    while erreur.is_none() && Instant::now() < limite {
        match tokio::time::timeout(Duration::from_millis(200), evenements.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "zone.playback_error" => erreur = Some(ev.data),
            Ok(Ok(_)) | Err(_) => {}
            Ok(Err(_)) => break,
        }
    }
    let erreur =
        erreur.expect("segment 30 perdu : aucune zone.playback_error, la piste a fini en silence");
    assert!(
        erreur["cause"]
            .as_str()
            .unwrap_or("")
            .starts_with(crate::audio::decode::TELECHARGEMENT_DASH_INTERROMPU),
        "{erreur}"
    );
}

/// Le témoin, sur une zone OAAT : même session WAV, même gain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sortie_oaat_la_premiere_note_precede_la_fin_du_telechargement_7266() {
    let (pcm, piste) = piste_cd().await;
    let (base, journal) = faux_tidal(piste, "flac", None).await;
    let orch = orchestrateur();
    let (r, _, _) = resoudre_sur(&orch, base, None, "oaat", "oaat:Salon").await;
    let lu = lire_la_session(&orch, &r.stream_id.clone().expect("session WAV")).await;
    let premier_pcm = lu.premier_pcm.expect("du PCM");
    let dernier_segment = journal
        .dernier_segment
        .lock()
        .unwrap()
        .expect("segments servis");
    assert!(
        premier_pcm < dernier_segment,
        "OAAT : premier PCM après le dernier segment"
    );
    let (trames, _) = trames_wav(&lu.octets);
    assert_eq!(trames, pcm.len() / 4, "OAAT : piste incomplète");
}

/// Qui décode pendant le téléchargement : les sorties locales et OAAT, au
/// départ de la piste. Les renderers réseau gardent le fichier complet.
#[test]
fn seules_les_sorties_locales_et_oaat_decodent_pendant_le_telechargement_7266() {
    let req = |id: &str, seek_ms: Option<u64>| PlayRequest {
        output_device_id: Some(id.into()),
        seek_ms,
        ..Default::default()
    };
    for id in ["local:Casque", "oaat:Salon", "oaat-group:Maison"] {
        assert!(
            super::resolve_stream::decoder_pendant_le_telechargement(&req(id, None)),
            "{id}"
        );
        assert!(
            !super::resolve_stream::decoder_pendant_le_telechargement(&req(id, Some(20_000))),
            "{id} avec un saut"
        );
    }
    for id in [
        "dlna:uuid-renderer",
        "uuid:renderer",
        "chromecast-1234",
        "airplay-34:02:0E:00:00:00",
        "airplay2:salon",
    ] {
        assert!(
            !super::resolve_stream::decoder_pendant_le_telechargement(&req(id, None)),
            "{id}"
        );
    }
    assert!(!super::resolve_stream::decoder_pendant_le_telechargement(
        &PlayRequest::default()
    ));
}

/// Au niveau du service : un `file://` encore en croissance n'est jamais
/// resservi par le cache (un second lecteur le lirait tronqué) ; une fois
/// complet, il sert comme avant (préchargement, reprise), sans nouveau
/// téléchargement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_cache_ne_sert_que_les_fichiers_complets_7266() {
    let (_, piste) = piste_cd().await;
    let total: usize =
        piste.init.len() + piste.segments.iter().map(|(s, _)| s.len()).sum::<usize>();
    let (base, journal) = faux_tidal(piste, "flac", None).await;
    let svc = TidalService::pour_un_faux_tidal(base);
    let progressif = || {
        crate::audio::dash_growth::pendant_le_telechargement(
            true,
            svc.get_track_url(PISTE, Some("LOSSLESS")),
        )
    };

    let a = progressif().await.unwrap();
    let chemin_a = a.url.strip_prefix("file://").unwrap().to_string();
    let poignee_a = crate::audio::dash_growth::take_for(&chemin_a)
        .expect("progressif : poignée de croissance inscrite");
    // Pendant la croissance de A : B ne reçoit pas le fichier de A.
    let b = progressif().await.unwrap();
    assert_ne!(b.url, a.url, "un fichier en croissance a été resservi");
    assert_eq!(journal.inits.load(Ordering::SeqCst), 2);
    let chemin_b = b.url.strip_prefix("file://").unwrap().to_string();
    let poignee_b = crate::audio::dash_growth::take_for(&chemin_b).expect("poignée de B");
    let (fin_a, fin_b) = tokio::task::spawn_blocking(move || {
        (poignee_a.attendre_la_fin(), poignee_b.attendre_la_fin())
    })
    .await
    .unwrap();
    assert_eq!(fin_a.unwrap() as usize, total);
    assert_eq!(fin_b.unwrap() as usize, total);

    // B complet : le préchargement (hors sortie locale) le reprend du cache.
    let complet = svc.get_track_url(PISTE, Some("LOSSLESS")).await.unwrap();
    assert_eq!(complet.url, b.url, "le fichier complet du cache sert");
    assert_eq!(journal.inits.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::metadata(&chemin_b).unwrap().len() as usize, total);
    let _ = std::fs::remove_file(&chemin_a);
    let _ = std::fs::remove_file(&chemin_b);
}

/// Un saut dans la piste pendant son téléchargement en croissance : l'ancien
/// décodeur lâche la poignée, la nouvelle résolution (hors croissance, elle
/// veut le fichier entier) ATTEND ce téléchargement au lieu d'en refaire un ;
/// le partiel n'est ni supprimé ni abandonné.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_saut_attend_le_telechargement_en_cours_7266() {
    let (_, piste) = piste_cd().await;
    let total: usize =
        piste.init.len() + piste.segments.iter().map(|(s, _)| s.len()).sum::<usize>();
    let (base, journal) = faux_tidal(piste, "flac", None).await;
    let svc = TidalService::pour_un_faux_tidal(base);
    let a = crate::audio::dash_growth::pendant_le_telechargement(
        true,
        svc.get_track_url(PISTE, Some("LOSSLESS")),
    )
    .await
    .unwrap();
    let chemin = a.url.strip_prefix("file://").unwrap().to_string();
    // L'ancien décodeur s'arrête (saut) : il lâche la poignée.
    drop(crate::audio::dash_growth::take_for(&chemin).expect("poignée"));
    let saut = svc.get_track_url(PISTE, Some("LOSSLESS")).await.unwrap();
    assert_eq!(
        saut.url, a.url,
        "le saut reprend le téléchargement en cours"
    );
    assert_eq!(
        journal.inits.load(Ordering::SeqCst),
        1,
        "un second téléchargement a été lancé"
    );
    assert_eq!(std::fs::metadata(&chemin).unwrap().len() as usize, total);
    let _ = std::fs::remove_file(&chemin);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_aac_dash_garde_le_fichier_complet_meme_en_local_7266() {
    let aac = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/aac320_frag.mp4"
    ))
    .unwrap();
    let piste = Arc::new(PisteDash {
        init: aac.clone(),
        segments: vec![(Vec::new(), SR)],
    });
    let (base, _) = faux_tidal(piste, "mp4a.40.2", None).await;
    let svc = TidalService::pour_un_faux_tidal(base);
    let u = crate::audio::dash_growth::pendant_le_telechargement(
        true,
        svc.get_track_url(PISTE, Some("HIGH")),
    )
    .await
    .unwrap();
    let chemin = u.url.strip_prefix("file://").unwrap();
    assert!(
        crate::audio::dash_growth::take_for(chemin).is_none(),
        "un AAC n'emprunte pas le chemin progressif"
    );
    assert_eq!(std::fs::read(chemin).unwrap(), aac);
}

/// #7266, par le vrai chemin de l'orchestrateur : une lecture locale démarre en
/// croissance, puis un SAUT à 20 s arrive avant la fin du téléchargement
/// (l'ancienne session est retirée, son décodeur lâche la poignée). La
/// résolution du saut attend le téléchargement en cours, qui progresse : un
/// seul téléchargement, et la session du saut part bien de 20 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_saut_en_cours_de_telechargement_reprend_le_meme_telechargement_7266() {
    let (pcm, piste) = piste_cd().await;
    let (base, journal) = faux_tidal(piste, "flac", None).await;
    let orch = orchestrateur();
    orch.services
        .lock()
        .await
        .register(Box::new(TidalService::pour_un_faux_tidal(base)));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Casque 7266", Some("local"), Some("local:Casque"))
        .unwrap();
    let mut req = PlayRequest {
        zone_id,
        output_device_id: Some("local:Casque".into()),
        source: Some("tidal".into()),
        source_id: Some(PISTE.into()),
        title: Some("Témoin 7266".into()),
        artist_name: Some("Banc".into()),
        album_title: Some("DASH".into()),
        duration_ms: Some(SECONDES as i64 * 1000),
        ..Default::default()
    };
    let premier = orch.resolve_stream(&req).await.expect("lecture");
    let sid = premier.stream_id.clone().expect("session WAV");
    // Quelques morceaux joués, puis le saut : l'ancienne session disparaît.
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session");
    for _ in 0..3 {
        let _ = tokio::time::timeout(Duration::from_secs(5), session.recv_chunk()).await;
    }
    drop(session);
    let encore_en_cours = journal.dernier_segment.lock().unwrap().is_none();
    println!("#7266 saut : téléchargement encore en cours au saut = {encore_en_cours}");
    orch.streamer.remove_session(&sid).await;

    req.seek_ms = Some(20_000);
    let saut = orch.resolve_stream(&req).await.expect("saut");
    assert_eq!(
        journal.inits.load(Ordering::SeqCst),
        1,
        "le saut a relancé un téléchargement au lieu d'attendre celui en cours"
    );
    let lu = lire_la_session(&orch, &saut.stream_id.clone().expect("session du saut")).await;
    let (trames, _) = trames_wav(&lu.octets);
    let attendues = pcm.len() / 4 - 20 * SR as usize;
    assert!(
        trames.abs_diff(attendues) < SR as usize / 2,
        "{trames} trames lues après un saut à 20 s, ~{attendues} attendues"
    );
}
