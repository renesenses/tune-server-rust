//! #8701 — un saut dans une piste TIDAL en DASH (fil 2722).
//!
//! 1. Après le TTL du cache d'URL (240 s, simulé ici par un TTL nul), un saut
//!    reprend le fMP4 assemblé de la piste en cours au lieu de retélécharger
//!    tous les segments (`tidal_dash_multi_segment_detected` ne revient pas).
//! 2. Pendant le téléchargement en croissance, un saut dans la partie déjà
//!    écrite joue tout de suite ; au-delà, il n'attend que le segment visé,
//!    pas la fin du téléchargement.
//!
//! Le banc : le vrai FLAC en fMP4 de `tidal_cd_dash_jusqu_au_play_7266.rs`
//! (30 s, 60 segments d'une demi-seconde), servi par un faux TIDAL sur la
//! boucle locale dont le CDN retient les segments au-delà d'une VANNE que le
//! test ouvre à sa guise. Aucun réseau, aucune sortie audio : la session WAV
//! est lue directement, rien n'est joué.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;

use super::tidal_cd_dash_jusqu_au_play_7266::{
    PISTE, PisteDash, SECONDES, SR, decouper, flac_cd, mpd, pcm_cd, pcm_de,
};
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::streaming::tidal::TidalService;

/// Au-delà de tout en-tête WAV : la preuve que du PCM est arrivé.
const DU_PCM: usize = 4096;

struct Cdn {
    base: String,
    /// Inits servis : un par téléchargement de la piste.
    inits: AtomicUsize,
    /// Segments média servis.
    servis: AtomicUsize,
    /// Les segments de numéro <= vanne sont servis, les autres attendent.
    vanne: tokio::sync::watch::Sender<usize>,
}

impl Cdn {
    fn ouvrir_jusqu_a(&self, segment: usize) {
        self.vanne.send_replace(segment);
    }

    async fn attendre_servis(&self, n: usize) {
        let limite = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.servis.load(Ordering::SeqCst) < n {
            assert!(
                tokio::time::Instant::now() < limite,
                "le CDN n'a servi que {} segments sur {n}",
                self.servis.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Le temps que le téléchargeur écrive le dernier segment reçu.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn faux_tidal(piste: Arc<PisteDash>, vanne: usize) -> Arc<Cdn> {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    #[derive(Clone)]
    struct Etat {
        piste: Arc<PisteDash>,
        cdn: Arc<Cdn>,
    }

    async fn manifeste(State(e): State<Etat>) -> axum::response::Response {
        let uri = format!(
            "data:application/dash+xml;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(mpd(&e.cdn.base, "flac", &e.piste))
        );
        axum::Json(
            serde_json::json!({"data": {"id": PISTE, "type": "trackManifests",
            "attributes": {"trackPresentation": "FULL", "uri": uri, "formats": ["FLAC"]}}}),
        )
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
            e.cdn.inits.fetch_add(1, Ordering::SeqCst);
            return e.piste.init.clone().into_response();
        }
        let Some((octets, _)) = e.piste.segments.get(numero.wrapping_sub(1)) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let mut vanne = e.cdn.vanne.subscribe();
        let _ = vanne.wait_for(|&ouverte| numero <= ouverte).await;
        e.cdn.servis.fetch_add(1, Ordering::SeqCst);
        octets.clone().into_response()
    }

    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    let cdn = Arc::new(Cdn {
        base: base.clone(),
        inits: AtomicUsize::new(0),
        servis: AtomicUsize::new(0),
        vanne: tokio::sync::watch::channel(vanne).0,
    });
    let app = axum::Router::new()
        .route("/trackManifests/{id}", get(manifeste))
        .route(
            "/tracks/{id}/playbackinfopostpaywall",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route(
            "/tracks/{id}",
            get(|| async {
                axum::Json(serde_json::json!({
                    "id": 72660001, "title": "Témoin 8701", "duration": SECONDES,
                    "audioQuality": "LOSSLESS",
                    "artist": {"id": 1, "name": "Banc"}, "album": {"id": 2, "title": "DASH"},
                }))
            }),
        )
        .route("/cdn/{fichier}", get(segment))
        .with_state(Etat {
            piste,
            cdn: cdn.clone(),
        });
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    cdn
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

/// Une zone locale qui lit la piste TIDAL du banc. `apres` : la zone prend
/// l'identifiant suivant (les épingles sont globales au processus, et tous
/// les bancs créent sinon la zone 1).
async fn zone_locale(
    orch: &PlaybackOrchestrator,
    svc: TidalService,
    apres: Option<i64>,
) -> PlayRequest {
    orch.services.lock().await.register(Box::new(svc));
    if let Some(id) = apres {
        orch.db
            .execute(
                &format!("INSERT INTO zones (id, name) VALUES ({id}, 'bouchon 8701')"),
                &[],
            )
            .unwrap();
    }
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Casque 8701", Some("local"), Some("local:Casque"))
        .unwrap();
    PlayRequest {
        zone_id,
        output_device_id: Some("local:Casque".into()),
        source: Some("tidal".into()),
        source_id: Some(PISTE.into()),
        title: Some("Témoin 8701".into()),
        artist_name: Some("Banc".into()),
        album_title: Some("DASH".into()),
        duration_ms: Some(SECONDES as i64 * 1000),
        ..Default::default()
    }
}

async fn session(
    orch: &PlaybackOrchestrator,
    sid: &str,
) -> Arc<crate::http::streamer::StreamSession> {
    orch.streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session inscrite")
}

/// Lit la session jusqu'à `au_moins` octets, sa fin, ou `delai` sans rien.
/// Rend les octets lus et si la session est finie.
async fn lire(
    s: &crate::http::streamer::StreamSession,
    au_moins: usize,
    delai: Duration,
) -> (Vec<u8>, bool) {
    let mut octets = Vec::new();
    while octets.len() < au_moins {
        match tokio::time::timeout(delai, s.recv_chunk()).await {
            Ok(Some(c)) => octets.extend_from_slice(&c),
            Ok(None) => return (octets, true),
            Err(_) => return (octets, false),
        }
    }
    (octets, false)
}

async fn lire_jusqu_a_la_fin(s: &crate::http::streamer::StreamSession, debut: Vec<u8>) -> Vec<u8> {
    let (suite, fini) = lire(s, usize::MAX, Duration::from_secs(10)).await;
    assert!(fini, "la session n'avance plus depuis 10 s");
    [debut, suite].concat()
}

fn trames_wav(octets: &[u8]) -> usize {
    let (pcm, _, bd) = pcm_de(octets, "wav");
    pcm.len() / (bd as usize / 8) / 2
}

fn assert_depart_a(octets: &[u8], pcm_total: usize, secondes: usize) {
    let trames = trames_wav(octets);
    let attendues = pcm_total / 4 - secondes * SR as usize;
    assert!(
        trames.abs_diff(attendues) < SR as usize / 2,
        "{trames} trames lues après un saut à {secondes} s, ~{attendues} attendues"
    );
}

/// Lecture depuis le début : la session, lue en partie, puis lâchée comme le
/// fait un saut (l'ancienne session disparaît, son décodeur lâche la poignée).
async fn jouer_puis_lacher(orch: &PlaybackOrchestrator, req: &PlayRequest) {
    let premier = orch.resolve_stream(req).await.expect("lecture");
    let sid = premier.stream_id.clone().expect("session WAV");
    let s = session(orch, &sid).await;
    let (lu, _) = lire(&s, DU_PCM, Duration::from_secs(10)).await;
    assert!(
        lu.len() >= DU_PCM,
        "la lecture depuis le début ne démarre pas"
    );
    drop(s);
    orch.streamer.remove_session(&sid).await;
}

/// (1) La piste joue depuis plus longtemps que le TTL du cache d'URL : un
/// saut reprend le fichier assemblé, sans nouveau téléchargement. Témoin :
/// sans l'épingle de la zone, le même saut retélécharge tout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_saut_apres_le_ttl_du_cache_reprend_le_fichier_8701() {
    let pcm = pcm_cd();
    let piste = Arc::new(decouper(&flac_cd(&pcm).await));
    let cdn = faux_tidal(piste, usize::MAX).await;
    let orch = orchestrateur();
    // TTL nul : toute entrée du cache a « plus de 240 s ».
    let mut req = zone_locale(
        &orch,
        TidalService::pour_un_faux_tidal(cdn.base.clone()).avec_ttl_du_cache_d_url(0),
        Some(8_701_000),
    )
    .await;
    let premier = orch.resolve_stream(&req).await.expect("lecture");
    let s = session(&orch, &premier.stream_id.clone().expect("session")).await;
    let lu = lire_jusqu_a_la_fin(&s, Vec::new()).await;
    assert_eq!(trames_wav(&lu), pcm.len() / 4, "première lecture entière");
    assert_eq!(cdn.inits.load(Ordering::SeqCst), 1);

    req.seek_ms = Some(20_000);
    let saut = orch.resolve_stream(&req).await.expect("saut");
    assert_eq!(
        cdn.inits.load(Ordering::SeqCst),
        1,
        "le saut a retéléchargé la piste au lieu de reprendre son fichier"
    );
    let s = session(&orch, &saut.stream_id.clone().expect("session du saut")).await;
    let lu = lire_jusqu_a_la_fin(&s, Vec::new()).await;
    assert_depart_a(&lu, pcm.len(), 20);

    // Témoin : la zone a changé de piste (épingles libérées), le TTL est
    // passé — le fichier n'est plus servi et la piste est retéléchargée.
    crate::audio::dash_epingles::liberer_la_zone(req.zone_id);
    let apres = orch.resolve_stream(&req).await.expect("saut sans épingle");
    assert_eq!(
        cdn.inits.load(Ordering::SeqCst),
        2,
        "témoin : retéléchargement"
    );
    let s = session(&orch, &apres.stream_id.clone().expect("session")).await;
    let lu = lire_jusqu_a_la_fin(&s, Vec::new()).await;
    assert_depart_a(&lu, pcm.len(), 20);
}

/// (2a) Pendant le téléchargement, un saut dans la partie déjà écrite joue
/// tout de suite : du PCM arrive alors que le CDN retient encore la suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_saut_dans_la_partie_telechargee_joue_sans_attendre_8701() {
    let pcm = pcm_cd();
    let piste = Arc::new(decouper(&flac_cd(&pcm).await));
    // Les 20 premiers segments (10 s) passent, le reste attend.
    let cdn = faux_tidal(piste, 20).await;
    let orch = orchestrateur();
    let mut req = zone_locale(
        &orch,
        TidalService::pour_un_faux_tidal(cdn.base.clone()),
        None,
    )
    .await;
    jouer_puis_lacher(&orch, &req).await;
    cdn.attendre_servis(20).await;

    req.seek_ms = Some(5_000);
    let saut = orch.resolve_stream(&req).await.expect("saut");
    let s = session(&orch, &saut.stream_id.clone().expect("session du saut")).await;
    let (debut, _) = lire(&s, DU_PCM, Duration::from_secs(5)).await;
    assert!(
        debut.len() >= DU_PCM,
        "saut à 5 s, 10 s déjà téléchargées : {} octets reçus en 5 s, le saut attend la fin",
        debut.len()
    );
    assert_eq!(
        cdn.servis.load(Ordering::SeqCst),
        20,
        "la vanne est restée fermée"
    );
    assert_eq!(
        cdn.inits.load(Ordering::SeqCst),
        1,
        "un seul téléchargement"
    );

    cdn.ouvrir_jusqu_a(usize::MAX);
    let lu = lire_jusqu_a_la_fin(&s, debut).await;
    assert_depart_a(&lu, pcm.len(), 5);
}

/// (2b) Un saut au-delà de ce qui est écrit attend le segment visé, et lui
/// seul : rien tant qu'il manque, du PCM dès qu'il arrive, alors que les
/// segments suivants sont encore retenus par le CDN.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_saut_au_dela_du_telecharge_n_attend_que_son_segment_8701() {
    let pcm = pcm_cd();
    let piste = Arc::new(decouper(&flac_cd(&pcm).await));
    let cdn = faux_tidal(piste, 20).await;
    let orch = orchestrateur();
    let mut req = zone_locale(
        &orch,
        TidalService::pour_un_faux_tidal(cdn.base.clone()),
        None,
    )
    .await;
    jouer_puis_lacher(&orch, &req).await;
    cdn.attendre_servis(20).await;

    // 15 s : le segment 31 (demi-secondes), au-delà des 20 écrits.
    req.seek_ms = Some(15_000);
    let saut = orch.resolve_stream(&req).await.expect("saut");
    let s = session(&orch, &saut.stream_id.clone().expect("session du saut")).await;
    let (avant, fini) = lire(&s, DU_PCM, Duration::from_millis(1500)).await;
    assert!(
        avant.len() < DU_PCM && !fini,
        "{} octets avant l'arrivée du segment visé : la lecture ne part pas de 15 s",
        avant.len()
    );

    // Le segment visé (et le suivant) arrivent ; la suite reste retenue.
    cdn.ouvrir_jusqu_a(32);
    let (debut, _) = lire(&s, DU_PCM, Duration::from_secs(5)).await;
    let debut = [avant, debut].concat();
    assert!(
        debut.len() >= DU_PCM,
        "segment visé arrivé, toujours pas de PCM ({} octets) : le saut attend la fin du téléchargement",
        debut.len()
    );
    assert!(
        cdn.servis.load(Ordering::SeqCst) <= 32,
        "la vanne s'est ouverte au-delà du segment 32"
    );
    assert_eq!(
        cdn.inits.load(Ordering::SeqCst),
        1,
        "un seul téléchargement"
    );

    cdn.ouvrir_jusqu_a(usize::MAX);
    let lu = lire_jusqu_a_la_fin(&s, debut).await;
    assert_depart_a(&lu, pcm.len(), 15);
}

/// Le chemin du fMP4 DASH qu'une résolution de piste TIDAL a rendu.
fn fichier_dash(r: &super::ResolvedStream) -> String {
    r.origin_url
        .as_deref()
        .and_then(|u| u.strip_prefix("file://"))
        .expect("la piste TIDAL est servie depuis son fMP4 DASH")
        .to_string()
}

/// (3) L'épingle ne survit pas à la zone qui joue : l'arrêt la libère, un
/// saut la reprend, et une piste d'une autre source (ici un fichier
/// téléversé) libère le fMP4 de la piste TIDAL quittée. Le fichier
/// redevient alors nettoyable (ramasse-miettes, TTL du cache d'URL).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l_arret_et_une_autre_source_liberent_le_fichier_8701() {
    let pcm = pcm_cd();
    let piste = Arc::new(decouper(&flac_cd(&pcm).await));
    let cdn = faux_tidal(piste, usize::MAX).await;
    let orch = orchestrateur();
    let mut req = zone_locale(
        &orch,
        TidalService::pour_un_faux_tidal(cdn.base.clone()),
        Some(8_701_100),
    )
    .await;
    let premier = orch.resolve_stream(&req).await.expect("lecture");
    let chemin = fichier_dash(&premier);
    let s = session(&orch, &premier.stream_id.clone().expect("session")).await;
    let _ = lire_jusqu_a_la_fin(&s, Vec::new()).await;
    assert!(crate::audio::dash_epingles::est_epingle(&chemin));

    orch.stop(req.zone_id, None).await;
    assert!(
        !crate::audio::dash_epingles::est_epingle(&chemin),
        "zone arrêtée : son fMP4 reste épinglé"
    );

    // Relancée à 20 s : le fichier du cache sert, et il est de nouveau retenu.
    req.seek_ms = Some(20_000);
    let saut = orch.resolve_stream(&req).await.expect("saut");
    assert_eq!(fichier_dash(&saut), chemin);
    assert_eq!(cdn.inits.load(Ordering::SeqCst), 1);
    assert!(crate::audio::dash_epingles::est_epingle(&chemin));
    if let Some(sid) = saut.stream_id.as_deref() {
        orch.streamer.remove_session(sid).await;
    }

    // Une piste d'une autre source : un WAV téléversé.
    let wav = crate::test_scratch::scratch_file("tune-8701-autre-source", ".wav");
    let silence = vec![0u8; SR as usize * 4];
    std::fs::write(
        &*wav,
        [
            crate::audio::wav::build_wav_header_with_data_size(2, SR, 16, silence.len() as u32)
                .to_vec(),
            silence,
        ]
        .concat(),
    )
    .unwrap();
    let autre = PlayRequest {
        source: Some("upload".into()),
        source_id: Some(wav.to_string_lossy().into_owned()),
        seek_ms: None,
        title: Some("Autre source".into()),
        ..req.clone()
    };
    orch.resolve_stream(&autre)
        .await
        .expect("fichier téléversé");
    assert!(
        !crate::audio::dash_epingles::est_epingle(&chemin),
        "la piste TIDAL quittée pour une autre source reste épinglée"
    );
    let _ = std::fs::remove_file(&chemin);
}
