//! Témoins du go du 13/11 : forme d'onde et spectrogramme d'une piste Qobuz
//! ou TIDAL, par de FAUX services sur 127.0.0.1 (aucun réseau réel, aucun
//! son) : calcul, audio effacé, cache au second appel, plafond de taille,
//! délai, service non connecté, aucun compte rendu de lecture.

use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use base64::Engine as _;
use serde_json::{Value, json};
use tune_core::streaming::traits::StreamingService;

use super::*;
use crate::routes::library::forme_onde;
use crate::routes::library::spectrogramme;

const SR: u32 = 44_100;

/// Un WAV PCM 16 bits stéréo : un sinus de 1 kHz à −6 dBFS.
fn wav(secondes: f64) -> Vec<u8> {
    let n = (secondes * f64::from(SR)) as usize;
    let donnees = (n * 4) as u32;
    let mut o = Vec::with_capacity(44 + donnees as usize);
    o.extend_from_slice(b"RIFF");
    o.extend_from_slice(&(36 + donnees).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&2u16.to_le_bytes());
    o.extend_from_slice(&SR.to_le_bytes());
    o.extend_from_slice(&(SR * 4).to_le_bytes());
    o.extend_from_slice(&4u16.to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&donnees.to_le_bytes());
    for i in 0..n {
        let s = (std::f64::consts::TAU * 1000.0 * i as f64 / f64::from(SR)).sin() * 0.5;
        let v = ((s * 32767.0) as i16).to_le_bytes();
        o.extend_from_slice(&v);
        o.extend_from_slice(&v);
    }
    o
}

/// Les chemins demandés au faux service, dans l'ordre.
type Journal = Arc<Mutex<Vec<String>>>;

async fn noter(State(j): State<Journal>, req: Request, next: Next) -> axum::response::Response {
    j.lock().unwrap().push(req.uri().path().to_string());
    next.run(req).await
}

fn appels(j: &Journal, motif: &str) -> usize {
    j.lock()
        .unwrap()
        .iter()
        .filter(|p| p.contains(motif))
        .count()
}

/// Aucun compte rendu de lecture (Qobuz `reportStreamingStart/End`, journal
/// de lecture TIDAL) : ce n'est pas une écoute.
fn aucun_compte_rendu(j: &Journal) {
    let journal = j.lock().unwrap();
    for p in journal.iter() {
        let p = p.to_lowercase();
        assert!(
            !p.contains("report") && !p.contains("playlog") && !p.contains("event"),
            "appel de compte rendu de lecture : {p} (journal : {journal:?})"
        );
    }
}

async fn servir_faux(app: axum::Router, journal: &Journal) -> String {
    let app = app.layer(axum::middleware::from_fn_with_state(journal.clone(), noter));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    base
}

/// Un faux Qobuz : `getFileUrl` rend l'adresse d'un WAV servi par le même
/// serveur ; `attente` retarde l'audio (délai dépassé).
async fn faux_qobuz(audio: Vec<u8>, attente: Option<std::time::Duration>) -> (String, Journal) {
    let journal: Journal = Arc::default();
    let base_audio = Arc::new(Mutex::new(String::new()));
    let pour_url = base_audio.clone();
    let audio = Arc::new(audio);
    let app = axum::Router::new()
        .route(
            "/track/getFileUrl",
            axum::routing::get(move || {
                let base = pour_url.lock().unwrap().clone();
                async move {
                    axum::Json(json!({
                        "track_id": 1, "duration": 3,
                        "url": format!("{base}/audio/piste.flac"),
                        "mime_type": "audio/flac", "sampling_rate": 44.1, "bit_depth": 16
                    }))
                }
            }),
        )
        .route(
            "/audio/piste.flac",
            axum::routing::get(move || {
                let audio = audio.clone();
                async move {
                    if let Some(a) = attente {
                        tokio::time::sleep(a).await;
                    }
                    (*audio).clone().into_response()
                }
            }),
        );
    let base = servir_faux(app, &journal).await;
    *base_audio.lock().unwrap() = base.clone();
    (base, journal)
}

fn qobuz_connecte(base: &str) -> tune_core::streaming::qobuz::QobuzService {
    let mut svc = tune_core::streaming::qobuz::QobuzService::avec_bases_de_test(
        "app-factice".into(),
        "secret-factice".into(),
        tune_core::streaming::qobuz::BasesQobuz {
            api: base.to_string(),
            proxy: base.to_string(),
            config_distante: base.to_string(),
        },
    );
    assert!(svc.restore_tokens(&json!({"user_auth_token": "jeton-factice"})));
    svc
}

/// Un faux TIDAL : pas de manifeste v2 (404), un manifeste v1 DASH à
/// plusieurs segments dont la réunion est un WAV — le chemin du fMP4 que
/// TIDAL assemble lui-même.
async fn faux_tidal(audio: Vec<u8>) -> (String, Journal) {
    let journal: Journal = Arc::default();
    let morceaux: Vec<Vec<u8>> = {
        let (init, reste) = audio.split_at(44);
        let mut v = vec![init.to_vec()];
        v.extend(reste.chunks(reste.len().div_ceil(4)).map(<[u8]>::to_vec));
        v
    };
    let segments = morceaux.len() - 1;
    let morceaux = Arc::new(morceaux);
    let base_partagee = Arc::new(Mutex::new(String::new()));
    let pour_manifeste = base_partagee.clone();
    let app = axum::Router::new()
        .route(
            "/tracks/{id}/playbackinfopostpaywall",
            axum::routing::get(move || {
                let base = pour_manifeste.lock().unwrap().clone();
                async move {
                    let mpd = format!(
                        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD type="static" mediaPresentationDuration="PT3S"><Period id="0">
<AdaptationSet contentType="audio" mimeType="audio/mp4">
<Representation codecs="flac" bandwidth="1411200" audioSamplingRate="44100">
<SegmentTemplate timescale="44100" initialization="{base}/seg/0.mp4" media="{base}/seg/$Number$.mp4" startNumber="1">
<SegmentTimeline><S t="0" d="44100" r="{}"/></SegmentTimeline>
</SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#,
                        segments - 1
                    );
                    axum::Json(json!({
                        "trackId": 7,
                        "audioQuality": "HI_RES_LOSSLESS",
                        "manifestMimeType": "application/dash+xml",
                        "manifest": base64::engine::general_purpose::STANDARD.encode(mpd),
                        "bitDepth": 16,
                        "sampleRate": 44100
                    }))
                }
            }),
        )
        .route(
            "/seg/{n}",
            axum::routing::get(move |axum::extract::Path(n): axum::extract::Path<String>| {
                let morceaux = morceaux.clone();
                async move {
                    let i: usize = n.trim_end_matches(".mp4").parse().unwrap();
                    morceaux[i].clone().into_response()
                }
            }),
        )
        .fallback(|| async { StatusCode::NOT_FOUND });
    let base = servir_faux(app, &journal).await;
    *base_partagee.lock().unwrap() = base.clone();
    (base, journal)
}

fn tidal_connecte(base: &str) -> tune_core::streaming::tidal::TidalService {
    let mut svc =
        tune_core::streaming::tidal::TidalService::avec_bases_de_test(base.into(), base.into());
    assert!(svc.restore_tokens(&json!({
        "access_token": "jeton-tidal-factice",
        "refresh_token": "renouvellement-factice",
        "user_id": 7,
        "country_code": "BE"
    })));
    svc
}

struct Banc {
    state: AppState,
    dossier: tempfile::TempDir,
}

impl Banc {
    async fn new(service: Box<dyn StreamingService>, id: i64, source: &str) -> Self {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state.services.lock().await.register(service);
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, duration_ms, source, source_id) VALUES (?, 'service', 3000, ?, 'piste-7')",
                &[&id, &source.to_string()],
            )
            .unwrap();
        Self {
            state,
            dossier: tempfile::tempdir().unwrap(),
        }
    }

    fn cache(&self) -> PathBuf {
        self.dossier.path().join("cache")
    }

    /// Ce qui reste dans le dossier des téléchargements : rien, toujours.
    fn audio_restant(&self) -> Vec<String> {
        std::fs::read_dir(dossier_temporaire(&self.cache()))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn onde(
        &self,
        id: i64,
        colonnes: Option<usize>,
        limites: Limites,
    ) -> (StatusCode, Option<String>, Value) {
        let p = forme_onde::Parametres {
            colonnes,
            ..Default::default()
        };
        let r = forme_onde::servir_avec(&self.state, id, p, &self.cache(), 64 << 20, limites).await;
        lire(r).await
    }
}

async fn lire(r: axum::response::Response) -> (StatusCode, Option<String>, Value) {
    let statut = r.status();
    let cache = r
        .headers()
        .get("x-tune-cache")
        .map(|v| v.to_str().unwrap().to_string());
    let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&corps)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&corps).into_owned()));
    (statut, cache, v)
}

fn limites() -> Limites {
    Limites {
        plafond_octets: PLAFOND_DEFAUT_OCTETS,
        delai: std::time::Duration::from_secs(30),
    }
}

fn fichiers_dash() -> HashSet<PathBuf> {
    fichiers_dash_presents()
}

#[tokio::test]
async fn qobuz_forme_onde_calculee_audio_efface_puis_cache() {
    let (base, journal) = faux_qobuz(wav(3.0), None).await;
    let banc = Banc::new(Box::new(qobuz_connecte(&base)), 9101, "qobuz").await;

    let (statut, cache, v) = banc.onde(9101, None, limites()).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(cache.as_deref(), Some("miss"));
    assert_eq!(v["canaux"], 2);
    assert!(
        (v["duree_s"].as_f64().unwrap() - 3.0).abs() < 0.05,
        "{}",
        v["duree_s"]
    );
    let crete = v["global"]["crete_dbfs"].as_f64().unwrap();
    assert!((crete + 6.0).abs() < 0.2, "crête {crete}");
    assert!(
        banc.audio_restant().is_empty(),
        "audio gardé : {:?}",
        banc.audio_restant()
    );

    let (statut, cache, _) = banc.onde(9101, None, limites()).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        cache.as_deref(),
        Some("hit"),
        "le second appel lit le cache"
    );
    assert_eq!(appels(&journal, "getFileUrl"), 1, "une seule résolution");
    assert_eq!(appels(&journal, "/audio/"), 1, "un seul téléchargement");
    aucun_compte_rendu(&journal);

    // La fiche dit la forme à jour, sous la clé du service.
    let track = tune_core::db::track_repo::TrackRepo::with_backend(banc.state.backend.clone())
        .get(9101)
        .unwrap()
        .unwrap();
    let s = forme_onde::section(&track, &banc.cache()).unwrap();
    assert_eq!(s["etat"], "a_jour");
    assert_eq!(s["service"], "qobuz");
}

#[tokio::test]
async fn tidal_dash_forme_onde_calculee_fichier_du_service_efface() {
    let (base, journal) = faux_tidal(wav(3.0)).await;
    let banc = Banc::new(Box::new(tidal_connecte(&base)), 9102, "tidal").await;
    let avant = fichiers_dash();

    let (statut, cache, v) = banc.onde(9102, None, limites()).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(cache.as_deref(), Some("miss"));
    assert_eq!(v["canaux"], 2);
    assert!(
        (v["duree_s"].as_f64().unwrap() - 3.0).abs() < 0.05,
        "{}",
        v["duree_s"]
    );
    assert!(
        banc.audio_restant().is_empty(),
        "audio gardé : {:?}",
        banc.audio_restant()
    );
    // Le fMP4 que TIDAL a assemblé (la taille exacte de notre piste) ne
    // reste pas ; d'autres témoins en parallèle peuvent en poser d'autres.
    let taille = wav(3.0).len() as u64;
    for f in fichiers_dash().difference(&avant) {
        assert_ne!(
            std::fs::metadata(f).map(|m| m.len()).unwrap_or(0),
            taille,
            "le fMP4 assemblé par TIDAL est resté : {}",
            f.display()
        );
    }

    let (_, cache, _) = banc.onde(9102, None, limites()).await;
    assert_eq!(cache.as_deref(), Some("hit"));
    assert_eq!(
        appels(&journal, "playbackinfopostpaywall"),
        1,
        "une seule résolution"
    );
    assert_eq!(appels(&journal, "/seg/0.mp4"), 1, "un seul téléchargement");
    aucun_compte_rendu(&journal);
}

#[tokio::test]
async fn qobuz_spectrogramme_calcule_audio_efface_puis_cache() {
    let (base, journal) = faux_qobuz(wav(3.0), None).await;
    let banc = Banc::new(Box::new(qobuz_connecte(&base)), 9103, "qobuz").await;
    for attendu in ["miss", "hit"] {
        let r = spectrogramme::servir(
            &banc.state,
            9103,
            Default::default(),
            &banc.cache(),
            256 << 20,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "image/png");
        assert_eq!(r.headers()["x-tune-cache"], attendu);
        assert!(banc.audio_restant().is_empty());
    }
    assert_eq!(appels(&journal, "/audio/"), 1);
    aucun_compte_rendu(&journal);
    let s = spectrogramme::section_service(
        9103,
        &PisteDeService {
            service: "qobuz".into(),
            id_service: "piste-7".into(),
            qualite: "hires".into(),
        },
        &banc.cache(),
    );
    assert_eq!(s["etat"], "a_jour");
}

#[tokio::test]
async fn tidal_spectrogramme_calcule() {
    let (base, journal) = faux_tidal(wav(2.0)).await;
    let banc = Banc::new(Box::new(tidal_connecte(&base)), 9104, "tidal").await;
    let r = spectrogramme::servir(
        &banc.state,
        9104,
        Default::default(),
        &banc.cache(),
        256 << 20,
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(banc.audio_restant().is_empty());
    aucun_compte_rendu(&journal);
}

#[tokio::test]
async fn qobuz_plafond_de_taille() {
    let (base, _) = faux_qobuz(wav(3.0), None).await;
    let banc = Banc::new(Box::new(qobuz_connecte(&base)), 9105, "qobuz").await;
    let petit = Limites {
        plafond_octets: 100_000,
        ..limites()
    };
    let (statut, _, v) = banc.onde(9105, None, petit).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["raison"], "trop_volumineux", "{v}");
    assert!(banc.audio_restant().is_empty());
}

#[tokio::test]
async fn tidal_plafond_de_taille_efface_le_fmp4() {
    let (base, _) = faux_tidal(wav(2.5)).await;
    let banc = Banc::new(Box::new(tidal_connecte(&base)), 9106, "tidal").await;
    let avant = fichiers_dash();
    let petit = Limites {
        plafond_octets: 100_000,
        ..limites()
    };
    let (statut, _, v) = banc.onde(9106, None, petit).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["raison"], "trop_volumineux", "{v}");
    assert!(banc.audio_restant().is_empty());
    // Les fMP4 nouveaux viennent d'autres témoins en parallèle, jamais de
    // celui-ci : aucun ne fait la taille de notre piste.
    let taille = wav(2.5).len() as u64;
    for f in fichiers_dash().difference(&avant) {
        assert_ne!(
            std::fs::metadata(f).map(|m| m.len()).unwrap_or(0),
            taille,
            "fMP4 trop gros gardé : {}",
            f.display()
        );
    }
}

#[tokio::test]
async fn delai_depasse() {
    let (base, _) = faux_qobuz(wav(1.0), Some(std::time::Duration::from_secs(10))).await;
    let banc = Banc::new(Box::new(qobuz_connecte(&base)), 9107, "qobuz").await;
    let court = Limites {
        delai: std::time::Duration::from_millis(500),
        ..limites()
    };
    let (statut, _, v) = banc.onde(9107, None, court).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["raison"], "delai_depasse", "{v}");
    assert!(banc.audio_restant().is_empty());
}

#[tokio::test]
async fn service_non_connecte_422_sans_resolution() {
    let (base, journal) = faux_qobuz(wav(1.0), None).await;
    let svc = tune_core::streaming::qobuz::QobuzService::avec_bases_de_test(
        "app-factice".into(),
        "secret-factice".into(),
        tune_core::streaming::qobuz::BasesQobuz {
            api: base.clone(),
            proxy: base.clone(),
            config_distante: base,
        },
    );
    let banc = Banc::new(Box::new(svc), 9108, "qobuz").await;
    let (statut, _, v) = banc.onde(9108, None, limites()).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["raison"], "service_non_connecte", "{v}");
    assert_eq!(appels(&journal, "getFileUrl"), 0);

    // Même refus pour le spectrogramme.
    let r = spectrogramme::servir(
        &banc.state,
        9108,
        Default::default(),
        &banc.cache(),
        256 << 20,
    )
    .await;
    let (statut, _, v) = lire(r).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["raison"], "service_non_connecte", "{v}");
}

#[tokio::test]
async fn tidal_non_connecte_422() {
    let (base, journal) = faux_tidal(wav(1.0)).await;
    let svc = tune_core::streaming::tidal::TidalService::avec_bases_de_test(base.clone(), base);
    let banc = Banc::new(Box::new(svc), 9109, "tidal").await;
    let (statut, _, v) = banc.onde(9109, None, limites()).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["raison"], "service_non_connecte", "{v}");
    assert_eq!(appels(&journal, "playbackinfo"), 0);
}

#[tokio::test]
async fn zoom_refuse_pour_une_piste_de_service() {
    let (base, journal) = faux_qobuz(wav(1.0), None).await;
    let banc = Banc::new(Box::new(qobuz_connecte(&base)), 9110, "qobuz").await;
    let (statut, _, v) = banc.onde(9110, Some(500), limites()).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["raison"], "zoom_non_disponible", "{v}");
    assert_eq!(appels(&journal, "getFileUrl"), 0, "rien n'est téléchargé");
}

#[tokio::test]
async fn service_non_pris_en_charge_et_piste_inconnue() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, duration_ms, source, source_id) VALUES (9111, 's', 3000, 'spotify', 'sp-1')",
            &[],
        )
        .unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let r = forme_onde::servir(&state, 9111, Default::default(), dossier.path(), 1 << 20).await;
    let (statut, _, v) = lire(r).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["raison"], "service_non_pris_en_charge");
    assert_eq!(
        v["error"], "track has no local file",
        "un écran d'avant le reconnaît"
    );
    let r = forme_onde::servir(&state, 9999, Default::default(), dossier.path(), 1 << 20).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[test]
fn classement_des_refus() {
    assert_eq!(
        classer("Qobuz session expired — please reconnect in Settings", None),
        Refus::ServiceNonConnecte
    );
    assert_eq!(
        classer("tidal playback 401: not authenticated", None),
        Refus::ServiceNonConnecte
    );
    assert_eq!(
        classer(
            "qobuz /track/getFileUrl: 400 This track is not available in your region",
            None
        ),
        Refus::Region
    );
    assert_eq!(
        classer(
            "tidal: no usable stream found for track 7 (all qualities failed)",
            None
        ),
        Refus::NonDiffusable
    );
    assert_eq!(classer("boom", Some(false)), Refus::NonDiffusable);
    assert_eq!(classer("boom", None), Refus::TelechargementImpossible);
}

#[test]
fn extension_par_le_contenu() {
    assert_eq!(extension_selon(b"fLaC\0\0\0\"", Some("audio/mp4")), "flac");
    assert_eq!(extension_selon(&wav(0.01)[..16], Some("audio/flac")), "wav");
    assert_eq!(extension_selon(b"\0\0\0\x18ftypiso6", None), "mp4");
    assert_eq!(extension_selon(b"ID3\x04", None), "mp3");
    assert_eq!(extension_selon(b"????", Some("audio/flac")), "flac");
}

#[tokio::test]
async fn cle_par_service_identifiant_et_qualite() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, duration_ms, source, source_id) VALUES (1, 'q', 1, 'qobuz', '123'), (2, 't', 1, 'tidal', '123'), (3, 's', 1, 'spotify', '123'), (4, 'v', 1, 'qobuz', ' ')",
            &[],
        )
        .unwrap();
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let piste = |id: i64| PisteDeService::de(&repo.get(id).unwrap().unwrap());
    let q = piste(1).unwrap();
    assert_eq!(q.qualite, "hires");
    let ti = piste(2).unwrap();
    assert_eq!(ti.qualite, "HI_RES_LOSSLESS");
    assert_ne!(q.cle(), ti.cle());
    assert!(ti.cle().starts_with("tidal-"));
    assert!(piste(3).is_none());
    assert!(piste(4).is_none());
}

#[test]
fn l_audio_temporaire_s_efface() {
    let d = tempfile::tempdir().unwrap();
    let chemin = d.path().join("x.wav");
    std::fs::write(&chemin, b"RIFF").unwrap();
    drop(AudioTemporaire {
        chemin: chemin.clone(),
    });
    assert!(!chemin.exists());
}
