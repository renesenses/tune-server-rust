//! #6401 — le repli Qobuz en 16/44,1, contre un Qobuz simulé (API
//! `getFileUrl` ET CDN) servi en local. Aucun son : rien n'est décodé ni
//! joué, seuls des octets factices transitent.
//!
//! Contre-épreuves (faites le 15/10/2026, voir la PR) :
//! - rendre `inchange(flux)` juste après une sonde en échec fait rougir
//!   `hr_en_erreur_http_…` et `hr_coupe_au_demarrage_…` : le flux retenu
//!   reste celui du format 27 ;
//! - laisser `repli_possible` accepter le format 6 et un flux déjà en CD fait
//!   rougir `qualite_deja_cd_…` et `repli_possible_suit_…`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::*;
use crate::streaming::qobuz::QobuzService;

/// Ce que le faux CDN fait d'un fichier.
#[derive(Clone, Copy)]
enum Cdn {
    /// 200 et des octets.
    Sert,
    /// 500.
    Erreur500,
    /// 200 et son en-tête, puis la connexion tombe avant le premier octet
    /// du corps. Servi par un CDN brut sur TCP : hyper n'enverrait pas
    /// l'en-tête d'un corps qui échoue d'emblée.
    Coupe,
}

struct Etat {
    base: String,
    /// L'adresse du CDN brut qui coupe, quand un format est en `Coupe`.
    coupe: Option<String>,
    /// format → (sampling_rate en kHz, bit_depth) rendus par `getFileUrl`.
    formats: HashMap<&'static str, (f64, u64)>,
    cdn: HashMap<&'static str, Cdn>,
    /// Appels à `getFileUrl`, par format demandé.
    demandes: Mutex<HashMap<String, usize>>,
    /// GET du CDN, par format.
    telechargements: Arc<Mutex<HashMap<String, usize>>>,
}

impl Etat {
    fn demandes(&self, format: &str) -> usize {
        *self.demandes.lock().unwrap().get(format).unwrap_or(&0)
    }
    fn telechargements(&self, format: &str) -> usize {
        *self
            .telechargements
            .lock()
            .unwrap()
            .get(format)
            .unwrap_or(&0)
    }
}

async fn get_file_url(
    State(etat): State<Arc<Etat>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let format = q.get("format_id").cloned().unwrap_or_default();
    *etat
        .demandes
        .lock()
        .unwrap()
        .entry(format.clone())
        .or_default() += 1;
    match etat.formats.get(format.as_str()) {
        Some(&(cadence, profondeur)) => Json(json!({
            "track_id": 6401,
            "format_id": format,
            "url": match (etat.cdn.get(format.as_str()), &etat.coupe) {
                (Some(Cdn::Coupe), Some(coupe)) => format!("{coupe}/cdn/{format}.flac"),
                _ => format!("{}/cdn/{format}.flac", etat.base),
            },
            "mime_type": "audio/flac",
            "sampling_rate": cadence,
            "bit_depth": profondeur,
        }))
        .into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            r#"{"status":"error","code":400,"message":"Invalid format"}"#,
        )
            .into_response(),
    }
}

async fn cdn(State(etat): State<Arc<Etat>>, Path(fichier): Path<String>) -> Response {
    let format = fichier.trim_end_matches(".flac").to_string();
    *etat
        .telechargements
        .lock()
        .unwrap()
        .entry(format.clone())
        .or_default() += 1;
    match etat
        .cdn
        .get(format.as_str())
        .copied()
        .unwrap_or(Cdn::Erreur500)
    {
        Cdn::Sert => (
            [("Content-Type", "audio/flac")],
            Bytes::from_static(b"fLaC\0\0\0\x22 octets factices, aucun son"),
        )
            .into_response(),
        Cdn::Erreur500 => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Cdn::Coupe => unreachable!("servi par le CDN brut"),
    }
}

/// Un CDN qui répond `200` avec `Content-Length`, puis ferme la connexion
/// sans envoyer un octet du corps. Rend sa base `http://hôte:port`.
async fn cdn_qui_coupe(telechargements: Arc<Mutex<HashMap<String, usize>>>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse"));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = ecoute.accept().await {
            let mut requete = Vec::new();
            let mut octet = [0u8; 1];
            while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                if socket.read_exact(&mut octet).await.is_err() {
                    break;
                }
                requete.push(octet[0]);
            }
            let ligne = String::from_utf8_lossy(&requete);
            let format = ligne
                .split("/cdn/")
                .nth(1)
                .and_then(|reste| reste.split(".flac").next())
                .unwrap_or_default()
                .to_string();
            *telechargements.lock().unwrap().entry(format).or_default() += 1;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: 41008207\r\nConnection: close\r\n\r\n",
                )
                .await;
            let _ = socket.shutdown().await;
        }
    });
    base
}

/// Monte le faux Qobuz. `getFileUrl` connaît 27 (24/192) et 6 (16/44,1).
async fn faux_qobuz(cdn_27: Cdn, cdn_6: Cdn) -> (Arc<Etat>, QobuzService) {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse"));
    let telechargements = Arc::new(Mutex::new(HashMap::new()));
    let coupe = if matches!(cdn_27, Cdn::Coupe) || matches!(cdn_6, Cdn::Coupe) {
        Some(cdn_qui_coupe(telechargements.clone()).await)
    } else {
        None
    };
    let etat = Arc::new(Etat {
        base: base.clone(),
        coupe,
        formats: HashMap::from([("27", (192.0, 24)), ("6", (44.1, 16))]),
        cdn: HashMap::from([("27", cdn_27), ("6", cdn_6)]),
        demandes: Mutex::new(HashMap::new()),
        telechargements,
    });
    let app = Router::new()
        .route("/track/getFileUrl", get(get_file_url))
        .route("/cdn/{fichier}", get(cdn))
        .with_state(etat.clone());
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (etat, QobuzService::connecte_a_base_forcee(base))
}

/// Résout comme l'orchestrateur : `get_track_url` à la qualité de la zone,
/// puis la sonde et l'éventuel repli.
async fn resoudre(svc: &QobuzService, jeton: Option<&str>) -> Result<FluxRetenu, String> {
    let flux = svc
        .get_track_url("6401", jeton)
        .await
        .map_err(|e| e.to_string())?;
    Ok(retenir_le_flux(svc, crate::http::client::shared(), "6401", jeton, flux).await)
}

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl Journal {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capturer() -> (Journal, tracing::subscriber::DefaultGuard) {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let garde = tracing::subscriber::set_default(abonne);
    (journal, garde)
}

fn ligne<'a>(journal: &'a str, message: &str) -> Option<&'a str> {
    journal
        .lines()
        .find(|l| l.contains(message) && !l.contains(&format!("{message}_")))
}

#[tokio::test]
async fn hr_en_erreur_http_le_format_6_est_servi_et_journalise() {
    let (journal, _garde) = capturer();
    let (etat, svc) = faux_qobuz(Cdn::Erreur500, Cdn::Sert).await;

    let retenu = resoudre(&svc, None).await.expect("le repli doit réussir");

    assert!(retenu.replie, "le flux HR en 500 doit déclencher le repli");
    assert!(
        retenu.flux.url.ends_with("/cdn/6.flac"),
        "le flux retenu est le format 6 : {}",
        retenu.flux.url
    );
    assert_eq!(retenu.flux.quality.sample_rate, 44_100);
    assert_eq!(retenu.flux.quality.bit_depth, 16);
    assert_eq!(
        retenu.jeton.as_deref(),
        Some("cd"),
        "les re-résolutions de cette piste restent au format 6"
    );
    assert_eq!(etat.demandes("27"), 1);
    assert_eq!(etat.demandes("6"), 1, "un seul repli");

    let texte = journal.texte();
    let l = ligne(&texte, "qobuz_repli_qualite")
        .unwrap_or_else(|| panic!("ligne qobuz_repli_qualite absente :\n{texte}"));
    assert!(l.contains("track_id=\"6401\""), "track_id : {l}");
    assert!(l.contains("format_demande=\"27\""), "format demandé : {l}");
    assert!(l.contains("HTTP 500"), "l'erreur : {l}");
    assert!(l.contains("format_obtenu=\"6\""), "format obtenu : {l}");
}

#[tokio::test]
async fn hr_coupe_au_demarrage_le_format_6_est_servi() {
    let (journal, _garde) = capturer();
    let (etat, svc) = faux_qobuz(Cdn::Coupe, Cdn::Sert).await;

    let retenu = resoudre(&svc, Some("hires"))
        .await
        .expect("le repli doit réussir");

    assert!(retenu.replie, "un flux HR coupé avant le premier octet");
    assert!(retenu.flux.url.ends_with("/cdn/6.flac"));
    assert_eq!(etat.demandes("6"), 1);
    let texte = journal.texte();
    let l = ligne(&texte, "qobuz_repli_qualite")
        .unwrap_or_else(|| panic!("ligne qobuz_repli_qualite absente :\n{texte}"));
    assert!(
        l.contains("au démarrage"),
        "l'erreur nomme la coupure : {l}"
    );
}

#[tokio::test]
async fn format_6_en_echec_aussi_erreur_ordinaire_sans_boucle() {
    let (journal, _garde) = capturer();
    let (etat, svc) = faux_qobuz(Cdn::Erreur500, Cdn::Erreur500).await;

    let retenu = resoudre(&svc, None).await.expect("getFileUrl répond");

    // Pas de second repli : le flux d'origine revient, et la lecture échoue
    // comme avant le repli — l'erreur ordinaire, celle du CDN.
    assert!(!retenu.replie);
    assert!(retenu.flux.url.ends_with("/cdn/27.flac"));
    assert_eq!(retenu.jeton, None, "le réglage de la zone n'est pas touché");
    assert_eq!(etat.demandes("27"), 1);
    assert_eq!(
        etat.demandes("6"),
        1,
        "le format 6 n'est demandé qu'une fois"
    );
    assert_eq!(etat.telechargements("27"), 1, "le HR n'est pas resondé");
    assert_eq!(
        etat.telechargements("6"),
        1,
        "le format 6 n'est sondé qu'une fois"
    );
    let texte = journal.texte();
    assert!(
        ligne(&texte, "qobuz_repli_qualite").is_none(),
        "aucun repli annoncé comme réussi :\n{texte}"
    );
    let echec = texte
        .lines()
        .find(|l| l.contains("qobuz_repli_qualite_echec"))
        .unwrap_or_else(|| panic!("l'échec du repli se dit :\n{texte}"));
    assert!(echec.contains("WARN"), "{echec}");
    assert!(echec.contains("format_demande=\"27\""), "{echec}");
    assert!(echec.contains("erreur_repli=HTTP 500"), "{echec}");
    // L'erreur ordinaire : le flux rendu échoue au premier GET, comme avant.
    let ordinaire = sonder_le_debut(crate::http::client::shared(), &retenu.flux.url)
        .await
        .expect_err("le flux d'origine est toujours en échec");
    assert_eq!(ordinaire, "HTTP 500");
}

#[tokio::test]
async fn qualite_deja_cd_pas_de_repli_ni_de_sonde() {
    let (etat, svc) = faux_qobuz(Cdn::Erreur500, Cdn::Erreur500).await;

    // Zone réglée en CD : le format 6 est demandé d'emblée.
    let retenu = resoudre(&svc, Some("cd")).await.expect("aucune sonde");
    assert!(!retenu.replie);
    assert!(retenu.flux.url.ends_with("/cdn/6.flac"));
    assert_eq!(retenu.jeton.as_deref(), Some("cd"));
    assert_eq!(etat.demandes("6"), 1);
    assert_eq!(etat.telechargements("6"), 0, "rien n'est sondé");
    assert_eq!(etat.telechargements("27"), 0);
}

#[tokio::test]
async fn hr_sain_aucun_repli() {
    let (etat, svc) = faux_qobuz(Cdn::Sert, Cdn::Sert).await;

    let retenu = resoudre(&svc, None).await.expect("HR sain");

    assert!(!retenu.replie);
    assert!(retenu.flux.url.ends_with("/cdn/27.flac"));
    assert_eq!(retenu.flux.quality.sample_rate, 192_000);
    assert_eq!(retenu.jeton, None, "le réglage de la zone n'est pas touché");
    assert_eq!(etat.demandes("6"), 0);
}

#[test]
fn repli_possible_suit_la_qualite_demandee_et_obtenue() {
    let hr = StreamQuality {
        codec: "FLAC".into(),
        sample_rate: 192_000,
        bit_depth: 24,
        bitrate: None,
        channels: 2,
    };
    let cd = StreamQuality {
        sample_rate: 44_100,
        bit_depth: 16,
        ..hr.clone()
    };
    let inconnue = StreamQuality {
        sample_rate: 0,
        bit_depth: 0,
        ..hr.clone()
    };
    assert_eq!(repli_possible(None, &hr), Some("27"));
    assert_eq!(repli_possible(Some("hires"), &hr), Some("27"));
    assert_eq!(repli_possible(Some("cd"), &hr), None);
    assert_eq!(repli_possible(Some("mp3"), &hr), None);
    assert_eq!(
        repli_possible(None, &cd),
        None,
        "l'échelle est déjà descendue au CD"
    );
    assert_eq!(
        repli_possible(None, &inconnue),
        Some("27"),
        "une cadence inconnue ne prouve pas le CD"
    );
}
