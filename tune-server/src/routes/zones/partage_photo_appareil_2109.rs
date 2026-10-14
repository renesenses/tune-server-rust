//! tune-web-client#2109 — proposer la photo d'appareil d'une zone au
//! catalogue communautaire, et seulement sur ce geste.
//!
//! Un faux site (127.0.0.1) tient lieu de mozaiklabs : on y lit ce qui part
//! VRAIMENT — la route, la marque canonique, le modèle, l'identifiant
//! d'instance et les octets de la photo.

use super::{cle_photo_proposee, etat_partage_photo, zone_image_share};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tune_core::db::settings_repo::SettingsRepo;

/// Une « photo » PNG : la signature suffit à `sniff_image_ext`.
fn photo_png(graine: u8) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&[graine; 64]);
    v
}

/// Faux site : lit chaque requête jusqu'à sa fin (en-têtes + Content-Length),
/// la garde, et répond `statut`.
async fn faux_site(statut: u16) -> (String, Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>) {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", ecoute.local_addr().unwrap().port());
    let recues = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let r = recues.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut flux, _)) = ecoute.accept().await else {
                return;
            };
            let r = r.clone();
            tokio::spawn(async move {
                let mut tout = Vec::new();
                let mut tampon = vec![0u8; 16384];
                loop {
                    let n = flux.read(&mut tampon).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    tout.extend_from_slice(&tampon[..n]);
                    let texte = String::from_utf8_lossy(&tout).to_string();
                    if let Some(fin) = texte.find("\r\n\r\n") {
                        let longueur = texte[..fin]
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if tout.len() >= fin + 4 + longueur {
                            break;
                        }
                    }
                }
                r.lock().await.push(tout);
                let corps = r#"{"message":"pending review","status":"pending"}"#;
                let reponse = format!(
                    "HTTP/1.1 {statut} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = flux.write_all(reponse.as_bytes()).await;
                let _ = flux.flush().await;
            });
        }
    });
    (base, recues)
}

/// Une zone 7 avec sa photo posée localement, et le site pointé sur `base`.
fn preparer(state: &AppState, base: &str, marque: Option<&str>, modele: Option<&str>) -> String {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let octets = photo_png(7);
    let cache = crate::routes::library::artwork_cache_dir();
    let hash = tune_core::library::artwork::content_hash(&octets);
    tune_core::library::artwork::save_to_cache(&octets, &cache, &hash, "png")
        .expect("la photo doit entrer au cache");
    settings.set("zone_7_image", &hash).unwrap();
    if let Some(m) = marque {
        settings.set("zone_7_brand", m).unwrap();
    }
    if let Some(m) = modele {
        settings.set("zone_7_model", m).unwrap();
    }
    settings.set("mozaik_base_url", base).unwrap();
    hash
}

async fn partager(state: &AppState) -> (StatusCode, serde_json::Value) {
    let rep = zone_image_share(State(state.clone()), Path(7), HeaderMap::new())
        .await
        .into_response();
    let statut = rep.status();
    let corps = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&corps).unwrap_or_default())
}

#[tokio::test]
async fn la_photo_part_au_catalogue_avec_la_marque_canonique() {
    let (base, recues) = faux_site(201).await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // Graphie tapée en minuscules : c'est le nom du CATALOGUE qui part.
    let hash = preparer(&state, &base, Some("eversolo"), Some("DMP-A8"));

    let (statut, corps) = partager(&state).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["image_share_status"], "pending");

    let recues = recues.lock().await;
    assert_eq!(recues.len(), 1, "UNE requête doit atteindre le site");
    let brut = String::from_utf8_lossy(&recues[0]).to_string();
    assert!(
        brut.starts_with("POST /api/v1/community/device-images "),
        "mauvaise route : {}",
        brut.lines().next().unwrap_or("")
    );
    let champ =
        |nom: &str, valeur: &str| brut.contains(&format!("name=\"{nom}\"\r\n\r\n{valeur}\r\n"));
    assert!(
        champ("brand", "Eversolo"),
        "marque canonique absente : {brut}"
    );
    assert!(champ("model", "DMP-A8"));
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let instance = settings
        .get("instance_id")
        .unwrap()
        .expect("instance_id retenu");
    assert!(champ("instance_id", &instance));
    assert!(brut.contains("name=\"image\"; filename=\"appareil.png\""));
    assert!(brut.contains("Content-Type: image/png"));
    let octets = photo_png(7);
    assert!(
        recues[0]
            .windows(octets.len())
            .any(|w| w == octets.as_slice()),
        "les octets de la photo ne sont pas partis"
    );

    // L'état « en attente » suit la photo envoyée…
    assert_eq!(settings.get(&cle_photo_proposee(7)).unwrap(), Some(hash));
    assert_eq!(etat_partage_photo(&settings, 7), Some("pending"));
    // … et une NOUVELLE photo redevient locale.
    settings.set("zone_7_image", "autre-condensat").unwrap();
    assert_eq!(etat_partage_photo(&settings, 7), None);
}

#[tokio::test]
async fn sans_marque_ni_modele_rien_ne_part() {
    let (base, recues) = faux_site(201).await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    preparer(&state, &base, None, Some("DMP-A8"));

    let (statut, _) = partager(&state).await;
    assert_eq!(statut, StatusCode::CONFLICT);
    assert!(
        recues.lock().await.is_empty(),
        "rien ne doit atteindre le site"
    );
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(etat_partage_photo(&settings, 7), None);
}

#[tokio::test]
async fn une_marque_hors_catalogue_garde_la_photo_locale() {
    let (base, recues) = faux_site(201).await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // L'OEM détecté d'un Eversolo : décision du 20/09, pas de clef.
    preparer(&state, &base, Some("Shenzhen Zidoo"), Some("DMP-A8"));

    let (statut, corps) = partager(&state).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
    assert!(recues.lock().await.is_empty());
}

#[tokio::test]
async fn un_refus_du_site_ne_marque_pas_la_photo_proposee() {
    let (base, _recues) = faux_site(422).await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    preparer(&state, &base, Some("Eversolo"), Some("DMP-A8"));

    let (statut, _) = partager(&state).await;
    assert!(!statut.is_success());
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(etat_partage_photo(&settings, 7), None);
}
