//! #8913 — YouTube sans yt-dlp : un refus nommé, et plus de temps perdu sur
//! les deux voies natives que Google refuse depuis août (#2162).
//!
//! Aucun réseau, aucun son : un faux InnerTube (`/player`) rejoue les refus
//! RÉELS des deux clients du chemin natif (`lecteur_refus_android_music.json`,
//! `lecteur_refus_tv_embarque.json`, captures du 27/10/2026), avec un délai
//! fixe par appel qui tient lieu d'aller-retour vers Google ; un faux yt-dlp
//! (script shell) rend une URL tout de suite.
//!
//! La mesure (`mesure_8913_latence_de_resolution`, ignorée par défaut) :
//! `cargo test -p tune-core --lib mesure_8913 -- --ignored --nocapture --test-threads=1`
//! Elle ne passe que par `extract_audio_url` et `crate::ytdlp::set_binary`,
//! qui existaient avant #8913 : la même épreuve se joue sur l'ancien code.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;

use super::*;

/// Le délai de chaque `/player` du faux InnerTube.
const DELAI_PLAYER_MS: u64 = 300;

fn capture(nom: &str) -> Value {
    let chemin = format!(
        "{}/tests/fixtures/youtube/{nom}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&chemin).expect(&chemin)).expect("json")
}

#[derive(Clone)]
struct Faux {
    delai: Duration,
    appels: Arc<StdMutex<Vec<String>>>,
    /// Google a-t-il rouvert la voie native ? Faux : les refus réels.
    ouvert: Arc<AtomicBool>,
}

/// Une réponse `/player` qui sert un flux Opus (Google a rouvert la voie).
fn lecteur_ouvert(id: &str) -> Value {
    serde_json::json!({
        "playabilityStatus": {"status": "OK"},
        "streamingData": {"adaptiveFormats": [{
            "itag": 251,
            "mimeType": "audio/webm; codecs=\"opus\"",
            "bitrate": 140_000,
            "url": format!("https://cdn.invalide/videoplayback?id={id}&itag=251"),
        }]},
    })
}

async fn faux_player(State(f): State<Faux>, Json(corps): Json<Value>) -> Json<Value> {
    let client = corps["context"]["client"]["clientName"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let id = corps["videoId"].as_str().unwrap_or("").to_string();
    f.appels
        .lock()
        .unwrap()
        .push(format!("player:{client}:{id}"));
    tokio::time::sleep(f.delai).await;
    if f.ouvert.load(Ordering::SeqCst) {
        return Json(lecteur_ouvert(&id));
    }
    Json(match client.as_str() {
        "ANDROID_MUSIC" => capture("lecteur_refus_android_music"),
        _ => capture("lecteur_refus_tv_embarque"),
    })
}

async fn service(delai: Duration) -> (YouTubeService, Arc<StdMutex<Vec<String>>>) {
    let (yt, appels, _) = service_ouvrable(delai).await;
    (yt, appels)
}

async fn service_ouvrable(
    delai: Duration,
) -> (YouTubeService, Arc<StdMutex<Vec<String>>>, Arc<AtomicBool>) {
    let appels = Arc::new(StdMutex::new(Vec::new()));
    let ouvert = Arc::new(AtomicBool::new(false));
    let app = Router::new()
        .route("/player", post(faux_player))
        .with_state(Faux {
            delai,
            appels: appels.clone(),
            ouvert: ouvert.clone(),
        });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ecoute, app).await });
    let base = format!("http://{adresse}");
    (YouTubeService::avec_bases(&base, &base), appels, ouvert)
}

/// Un faux yt-dlp : une URL, ses en-têtes, le format, servable tout de suite.
fn faux_ytdlp(dossier: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let binaire = dossier.join("yt-dlp");
    std::fs::write(
        &binaire,
        "#!/bin/sh\n\
         echo 'http://127.0.0.1:9/flux'\n\
         echo '{\"User-Agent\": \"Mozilla/5.0\"}'\n\
         echo 'tune-format=251,opus,128.93'\n\
         echo 'tune-disponible=NA'\n",
    )
    .unwrap();
    std::fs::set_permissions(&binaire, std::fs::Permissions::from_mode(0o755)).unwrap();
    binaire
}

/// Résout `n` pistes l'une après l'autre ; rend (durées, nombre de `/player`).
async fn resoudre_en_serie(
    yt: &YouTubeService,
    appels: &Arc<StdMutex<Vec<String>>>,
    prefixe: &str,
    n: usize,
) -> (Vec<Duration>, usize, usize) {
    let avant = appels.lock().unwrap().len();
    let mut durees = Vec::new();
    let mut reussies = 0;
    for i in 0..n {
        let t = Instant::now();
        if yt
            .extract_audio_url(&format!("{prefixe}{i:02}"))
            .await
            .is_ok()
        {
            reussies += 1;
        }
        durees.push(t.elapsed());
    }
    (durees, appels.lock().unwrap().len() - avant, reussies)
}

fn dire(nom: &str, durees: &[Duration], appels: usize, reussies: usize) {
    let total: Duration = durees.iter().sum();
    let ms: Vec<u128> = durees.iter().map(|d| d.as_millis()).collect();
    println!(
        "MESURE-8913 {nom}: pistes={} reussies={reussies} appels_player={appels} \
         total_ms={} par_piste_ms={ms:?}",
        durees.len(),
        total.as_millis()
    );
}

#[tokio::test]
#[ignore = "mesure : cargo test -p tune-core --lib mesure_8913 -- --ignored --nocapture --test-threads=1"]
async fn mesure_8913_latence_de_resolution() {
    let delai = Duration::from_millis(DELAI_PLAYER_MS);
    // 1. yt-dlp NON activé (le cas du .18) : dix pistes de suite.
    let (yt, appels) = service(delai).await;
    let (d, a, r) = resoudre_en_serie(&yt, &appels, "sans", 10).await;
    dire("sans_ytdlp", &d, a, r);

    // 2. yt-dlp activé, service neuf (disjoncteur fermé) : dix pistes.
    let dossier = tempfile::tempdir().unwrap();
    crate::ytdlp::set_binary(faux_ytdlp(dossier.path()));
    let (yt, appels) = service(delai).await;
    let (d, a, r) = resoudre_en_serie(&yt, &appels, "avec", 10).await;
    dire("avec_ytdlp", &d, a, r);
}

// ── Épreuves ───────────────────────────────────────────────────────────
//
// Elles passent par `extraire_flux`, yt-dlp désigné en argument : aucune ne
// touche au binaire global, que d'autres essais posent.

fn nb(appels: &Arc<StdMutex<Vec<String>>>) -> usize {
    appels.lock().unwrap().len()
}

/// Sans yt-dlp, le refus porte le code stable que la route de lecture rend
/// en 409 traduit — et non plus « native API and yt-dlp both failed ».
#[tokio::test]
async fn sans_yt_dlp_le_refus_porte_le_code_stable() {
    let (yt, appels) = service(Duration::ZERO).await;
    let erreur = yt
        .extraire_flux("piste01", None)
        .await
        .expect_err("ni voie native ni yt-dlp");
    assert!(
        erreur.starts_with(CODE_LECTURE_YOUTUBE_INACTIVE),
        "le code stable en tête : {erreur}"
    );
    assert!(!erreur.contains("both failed"), "{erreur}");
    // La première piste essaie encore les deux voies natives.
    assert_eq!(
        appels.lock().unwrap().clone(),
        [
            "player:ANDROID_MUSIC:piste01",
            "player:TVHTML5_SIMPLY_EMBEDDED_PLAYER:piste01"
        ]
    );
}

/// 🔴 Le cœur de #8913 (point 2) : sans yt-dlp, après deux pistes refusées
/// par les deux voies natives, la troisième ne les essaie plus — le refus
/// nommé vient tout de suite. Contre-épreuve : avant #8913, le disjoncteur
/// n'était consulté qu'avec yt-dlp, et la troisième piste refaisait ses deux
/// appels (6 au lieu de 4) et attendait deux délais.
#[tokio::test]
async fn sans_yt_dlp_les_voies_natives_refusees_sont_sautees() {
    let delai = Duration::from_millis(200);
    let (yt, appels) = service(delai).await;
    for id in ["piste01", "piste02"] {
        yt.extraire_flux(id, None).await.expect_err("refus");
    }
    assert_eq!(nb(&appels), 4, "deux voies natives par piste, deux pistes");

    let t = Instant::now();
    let erreur = yt.extraire_flux("piste03", None).await.expect_err("refus");
    let duree = t.elapsed();
    assert!(
        erreur.starts_with(CODE_LECTURE_YOUTUBE_INACTIVE),
        "{erreur}"
    );
    assert_eq!(nb(&appels), 4, "aucun appel natif pour la troisième piste");
    assert!(
        duree < delai,
        "refus immédiat, pas un aller-retour : {duree:?}"
    );
}

/// Avec yt-dlp : les deux premières pistes passent par yt-dlp après les
/// refus natifs, la troisième y va directement.
#[tokio::test]
async fn avec_yt_dlp_la_troisieme_piste_va_directement_a_yt_dlp() {
    let dossier = tempfile::tempdir().unwrap();
    let ytdlp = faux_ytdlp(dossier.path());
    let (yt, appels) = service(Duration::ZERO).await;
    for id in ["piste01", "piste02"] {
        let flux = yt
            .extraire_flux(id, Some(ytdlp.clone()))
            .await
            .expect("yt-dlp sert la piste");
        assert_eq!(flux.url, "http://127.0.0.1:9/flux");
    }
    assert_eq!(nb(&appels), 4);
    let flux = yt
        .extraire_flux("piste03", Some(ytdlp))
        .await
        .expect("yt-dlp sert la piste");
    assert_eq!(flux.format.as_ref().map(|f| f.id.as_str()), Some("251"));
    assert_eq!(nb(&appels), 4, "voies natives sautées");
}

/// 🔴 Rien ne casse si Google rouvre une voie : après le délai de repos, une
/// sonde réessaie la voie native, et sa réussite referme le disjoncteur —
/// sans yt-dlp, la piste se joue de nouveau par la voie native.
#[tokio::test]
async fn la_sonde_rouvre_la_voie_native_quand_google_la_rend() {
    let (mut yt, appels, ouvert) = service_ouvrable(Duration::ZERO).await;
    let repos = Duration::from_millis(100);
    yt.native_extraction_circuit = Mutex::new(NativeExtractionCircuit::new(
        NATIVE_FAILURE_THRESHOLD,
        repos,
    ));
    for id in ["piste01", "piste02", "piste03"] {
        yt.extraire_flux(id, None).await.expect_err("refus");
    }
    assert_eq!(nb(&appels), 4, "disjoncteur ouvert après deux pistes");

    // Google rouvre la voie ; avant la fin du repos, rien ne change.
    ouvert.store(true, Ordering::SeqCst);
    yt.extraire_flux("piste04", None)
        .await
        .expect_err("encore au repos");
    assert_eq!(nb(&appels), 4);

    tokio::time::sleep(repos + Duration::from_millis(20)).await;
    let flux = yt
        .extraire_flux("piste05", None)
        .await
        .expect("la sonde passe par la voie native rouverte");
    assert!(flux.url.contains("id=piste05"), "{}", flux.url);
    assert_eq!(nb(&appels), 5, "une sonde : un seul appel, ANDROID_MUSIC");

    // Disjoncteur refermé : la piste suivante prend la voie native d'emblée.
    yt.extraire_flux("piste06", None)
        .await
        .expect("voie native");
    assert_eq!(nb(&appels), 6);
}
