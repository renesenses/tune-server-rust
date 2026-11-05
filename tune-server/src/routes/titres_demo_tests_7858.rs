//! #7858 — témoins des titres de démonstration.
//!
//! Aucun fichier audio : le faux site sert un manifeste d'exemple et de
//! petits fichiers d'octets synthétiques. Le serveur ne lit jamais leur
//! contenu ; seules leur somme et leur taille comptent ici.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use tune_core::db::models::Track;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use super::{Refus, installer, lire_le_manifeste};
use crate::state::AppState;

// ── Faux site mozaiklabs.fr ────────────────────────────────────────────────

/// Un serveur HTTP local minimal : chemin → (statut, corps). Rend l'adresse
/// de base, `http://127.0.0.1:<port>`.
async fn faux_site(pages: Vec<(String, u16, Vec<u8>)>) -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let pages: Arc<HashMap<String, (u16, Vec<u8>)>> = Arc::new(
        pages
            .into_iter()
            .map(|(chemin, statut, corps)| (chemin, (statut, corps)))
            .collect(),
    );
    tokio::spawn(async move {
        while let Ok((mut flux, _)) = ecoute.accept().await {
            let pages = pages.clone();
            tokio::spawn(async move {
                let mut lu = Vec::new();
                let mut tampon = [0u8; 4096];
                while !lu.windows(4).any(|w| w == b"\r\n\r\n") {
                    match flux.read(&mut tampon).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => lu.extend_from_slice(&tampon[..n]),
                    }
                }
                let texte = String::from_utf8_lossy(&lu);
                let chemin = texte.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (statut, corps) = pages
                    .get(&chemin)
                    .cloned()
                    .unwrap_or((404, b"not found".to_vec()));
                let entete = format!(
                    "HTTP/1.1 {statut} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = flux.write_all(entete.as_bytes()).await;
                let _ = flux.write_all(&corps).await;
                let _ = flux.shutdown().await;
            });
        }
    });
    format!("http://{adresse}")
}

fn somme(octets: &[u8]) -> String {
    format!("{:x}", Sha256::digest(octets))
}

/// Deux « titres » synthétiques : des octets, pas de l'audio.
fn fichiers() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "01 - Demo CD.flac",
            b"octets synthetiques du titre CD".to_vec(),
        ),
        ("02 - Demo DSD.dsf", vec![7u8; 70_000]),
    ]
}

/// Le manifeste d'exemple servi par le faux site. `fausse` : le nom du
/// fichier dont la somme annoncée est fausse.
fn manifeste(base: &str, fausse: Option<&str>) -> Vec<u8> {
    let titres: Vec<Value> = fichiers()
        .iter()
        .enumerate()
        .map(|(i, (nom, octets))| {
            let sha = if Some(*nom) == fausse {
                somme(b"autre contenu")
            } else {
                somme(octets)
            };
            json!({
                "title": format!("Titre {i}"),
                "format": if nom.ends_with(".dsf") { "dsd64" } else { "cd_16_44" },
                "license": "CC BY 4.0",
                "url": format!("{base}/fichiers/{i}"),
                "sha256": sha,
                "size": octets.len(),
                "file_name": nom,
            })
        })
        .collect();
    serde_json::to_vec(&json!({ "version": 1, "tracks": titres })).unwrap()
}

/// Le faux site complet : manifeste plus fichiers. Le port n'est connu
/// qu'après l'écoute : le manifeste est donc servi par un second site qui
/// pointe vers le premier.
async fn site_complet(fausse: Option<&str>) -> String {
    let fichiers_site = faux_site(
        fichiers()
            .into_iter()
            .enumerate()
            .map(|(i, (_, o))| (format!("/fichiers/{i}"), 200, o))
            .collect(),
    )
    .await;
    faux_site(vec![(
        super::CHEMIN_DU_MANIFESTE.to_string(),
        200,
        manifeste(&fichiers_site, fausse),
    )])
    .await
}

fn n(p: &Path) -> String {
    tune_core::scanner::walker::normalize_path(&p.to_string_lossy())
}

fn etat(dossier_demo: &Path, base: Option<&str>) -> AppState {
    let mut state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.dossier_titres_demo = Some(dossier_demo.to_path_buf());
    if let Some(base) = base {
        SettingsRepo::with_backend(state.backend.clone())
            .set("mozaik_base_url", base)
            .unwrap();
    }
    state
}

fn racines(state: &AppState) -> Vec<String> {
    crate::routes::system::get_music_dirs_list(&state.backend)
}

fn poser_racines(state: &AppState, dirs: &[String]) {
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &serde_json::to_string(dirs).unwrap())
        .unwrap();
}

fn piste(state: &AppState, chemin: &Path) -> i64 {
    let mut t = Track::new(format!("piste {}", chemin.display()));
    t.file_path = Some(n(chemin));
    TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .unwrap()
}

fn existe(state: &AppState, id: i64) -> bool {
    TrackRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .is_some()
}

async fn appel(state: &AppState, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let app = crate::routes::router(state.clone());
    let r = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(format!("/api/v1/onboarding{chemin}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

// ── Le manifeste ───────────────────────────────────────────────────────────

/// L'exemple documenté pour Bertrand reste lisible : la doc et le code ne
/// peuvent pas diverger sans que ce test rougisse.
#[test]
fn le_manifeste_documente_est_lisible() {
    let exemple = include_bytes!("../../../docs/titres-de-demonstration.exemple.json");
    let m = lire_le_manifeste(exemple).expect("l'exemple documenté doit être valide");
    assert_eq!(m.version, super::VERSION_DU_MANIFESTE);
    assert_eq!(m.tracks.len(), 4);
    assert!(m.tracks.iter().any(|t| t.file_name.ends_with(".dsf")));
}

#[test]
fn un_manifeste_invalide_est_refuse_avec_sa_raison() {
    let bon = json!({
        "title": "T", "format": "cd_16_44", "license": "CC BY 4.0",
        "url": "https://mozaiklabs.fr/a.flac", "sha256": "a".repeat(64),
        "file_name": "a.flac",
    });
    let avec = |champ: &str, valeur: Value| {
        let mut t = bon.clone();
        t[champ] = valeur;
        serde_json::to_vec(&json!({ "version": 1, "tracks": [t] })).unwrap()
    };
    let cas: Vec<(&str, Vec<u8>)> = vec![
        ("json", b"<html>pas du json</html>".to_vec()),
        (
            "version",
            serde_json::to_vec(&json!({ "version": 2, "tracks": [bon] })).unwrap(),
        ),
        (
            "liste vide",
            serde_json::to_vec(&json!({ "version": 1, "tracks": [] })).unwrap(),
        ),
        ("somme courte", avec("sha256", json!("abc"))),
        ("somme non hexa", avec("sha256", json!("z".repeat(64)))),
        ("chemin", avec("file_name", json!("../../etc/passwd.flac"))),
        ("sous-dossier", avec("file_name", json!("x/a.flac"))),
        ("antislash", avec("file_name", json!("x\\a.flac"))),
        ("caché", avec("file_name", json!(".a.flac"))),
        ("pas audio", avec("file_name", json!("a.exe"))),
        (
            "http distant",
            avec("url", json!("http://exemple.org/a.flac")),
        ),
        ("ftp", avec("url", json!("ftp://mozaiklabs.fr/a.flac"))),
        ("licence vide", avec("license", json!(" "))),
        ("taille nulle", avec("size", json!(0))),
        (
            "doublon",
            serde_json::to_vec(&json!({ "version": 1, "tracks": [bon, bon] })).unwrap(),
        ),
    ];
    for (nom, octets) in cas {
        assert!(
            lire_le_manifeste(&octets).is_err(),
            "cas « {nom} » accepté à tort"
        );
    }
    let ok = serde_json::to_vec(&json!({ "version": 1, "tracks": [bon] })).unwrap();
    assert!(lire_le_manifeste(&ok).is_ok());
}

// ── L'ajout ────────────────────────────────────────────────────────────────

/// Le chemin heureux : les deux titres sont téléchargés, vérifiés, rangés
/// dans le dossier dédié, et ce dossier rejoint les dossiers de musique SANS
/// faire perdre l'autre racine. Relancé, il ne crée aucun doublon.
#[tokio::test]
async fn l_ajout_telecharge_verifie_et_garde_les_autres_racines() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("demo-tracks");
    let autre = tmp.path().join("ma-musique");
    std::fs::create_dir_all(&autre).unwrap();
    let base = site_complet(None).await;
    let state = etat(&demo, Some(&base));
    poser_racines(&state, &[n(&autre)]);
    let url = super::url_du_manifeste(&state);

    let r = installer(&state, &demo, &url).await.expect("ajout");
    assert_eq!((r.installes, r.echecs), (2, 0), "{:?}", r.titres);
    assert!(r.dossier_ajoute);
    for (nom, octets) in fichiers() {
        assert_eq!(std::fs::read(demo.join(nom)).unwrap(), octets, "{nom}");
    }
    assert_eq!(racines(&state), vec![n(&autre), n(&demo)]);

    let r2 = installer(&state, &demo, &url).await.expect("second ajout");
    assert!(!r2.dossier_ajoute);
    assert!(
        r2.titres.iter().all(|t| t["status"] == "already_present"),
        "{:?}",
        r2.titres
    );
    assert_eq!(racines(&state), vec![n(&autre), n(&demo)]);

    let (statut, corps) = appel(&state, "GET", "/demo-tracks").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["installed"], json!(true));
    assert_eq!(corps["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(corps["tracks"][0]["license"], json!("CC BY 4.0"));
}

/// 🔴 LE TÉMOIN DE LA SOMME. Un fichier dont le contenu ne correspond pas à
/// la somme du manifeste n'est jamais gardé, ni sous son nom ni en `.part` ;
/// les titres corrects passent quand même.
#[tokio::test]
async fn un_titre_a_la_somme_fausse_est_refuse_et_efface() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("demo-tracks");
    let base = site_complet(Some("02 - Demo DSD.dsf")).await;
    let state = etat(&demo, Some(&base));
    let url = super::url_du_manifeste(&state);

    let r = installer(&state, &demo, &url).await.expect("ajout partiel");
    assert_eq!((r.installes, r.echecs), (1, 1), "{:?}", r.titres);
    let refuse = r
        .titres
        .iter()
        .find(|t| t["file_name"] == "02 - Demo DSD.dsf")
        .unwrap();
    assert_eq!(refuse["error"], json!("checksum_mismatch"), "{refuse}");
    assert!(
        !demo.join("02 - Demo DSD.dsf").exists(),
        "un fichier à la somme fausse a été gardé"
    );
    assert!(!demo.join("02 - Demo DSD.dsf.part").exists());
    assert!(demo.join("01 - Demo CD.flac").exists());
}

/// Toutes les sommes fausses : refus en bloc, aucun dossier ajouté.
#[tokio::test]
async fn sans_aucun_titre_valide_rien_n_est_ajoute() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("demo-tracks");
    let fichiers_site = faux_site(vec![]).await; // tout en 404
    let base = faux_site(vec![(
        super::CHEMIN_DU_MANIFESTE.to_string(),
        200,
        manifeste(&fichiers_site, None),
    )])
    .await;
    let state = etat(&demo, Some(&base));
    let url = super::url_du_manifeste(&state);

    match installer(&state, &demo, &url).await {
        Err(Refus::AucunTitre(titres)) => {
            assert!(
                titres.iter().all(|t| t["error"] == "http_status_404"),
                "{titres:?}"
            )
        }
        autre => panic!("attendu AucunTitre, obtenu {autre:?}"),
    }
    assert!(racines(&state).is_empty());
    assert!(!demo.exists(), "un dossier vide est resté");
}

// ── Les refus propres, par la route ────────────────────────────────────────

#[tokio::test]
async fn un_manifeste_absent_est_refuse_proprement() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("demo-tracks");
    let base = faux_site(vec![]).await;
    let state = etat(&demo, Some(&base));

    let (statut, corps) = appel(&state, "POST", "/demo-tracks/add").await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY, "{corps}");
    assert_eq!(corps["code"], json!("demo_manifest_unavailable"));
    assert_eq!(
        corps["message_key"],
        json!("api.demoTracks.manifestUnavailable")
    );
    assert!(corps["cause"].as_str().unwrap().contains("404"), "{corps}");
    assert!(racines(&state).is_empty());
    assert!(!demo.exists());
}

#[tokio::test]
async fn un_manifeste_invalide_est_refuse_proprement() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("demo-tracks");
    let base = faux_site(vec![(
        super::CHEMIN_DU_MANIFESTE.to_string(),
        200,
        br#"{"version":1,"tracks":[{"title":"x"}]}"#.to_vec(),
    )])
    .await;
    let state = etat(&demo, Some(&base));

    let (statut, corps) = appel(&state, "POST", "/demo-tracks/add").await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY, "{corps}");
    assert_eq!(corps["code"], json!("demo_manifest_invalid"));
    assert!(racines(&state).is_empty());
    assert!(!demo.exists());
}

#[tokio::test]
async fn sans_dossier_de_donnees_la_fonction_est_indisponible() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    assert!(state.dossier_titres_demo.is_none());
    let (statut, corps) = appel(&state, "POST", "/demo-tracks/add").await;
    assert_eq!(statut, StatusCode::SERVICE_UNAVAILABLE, "{corps}");
    assert_eq!(corps["code"], json!("demo_tracks_unavailable"));
    let (_, corps) = appel(&state, "GET", "/demo-tracks").await;
    assert_eq!(corps["offered"], json!(false));
}

// ── La proposition au premier démarrage ────────────────────────────────────

#[tokio::test]
async fn la_demo_n_est_proposee_qu_a_une_bibliotheque_vide() {
    let tmp = tempfile::tempdir().unwrap();
    let state = etat(&tmp.path().join("demo-tracks"), None);
    let (_, corps) = appel(&state, "GET", "/status").await;
    assert_eq!(corps["demo_tracks_offered"], json!(true), "{corps}");
    let (_, corps) = appel(&state, "GET", "/demo-tracks").await;
    assert_eq!(corps["offered"], json!(true));
    assert_eq!(
        corps["manifest_url"],
        json!("https://mozaiklabs.fr/tune/demo-tracks/manifest.json")
    );

    piste(&state, &tmp.path().join("ma-musique/a.flac"));
    let (_, corps) = appel(&state, "GET", "/status").await;
    assert_eq!(corps["demo_tracks_offered"], json!(false), "{corps}");
    let (_, corps) = appel(&state, "GET", "/demo-tracks").await;
    assert_eq!(corps["offered"], json!(false));
}

// ── Le retrait ─────────────────────────────────────────────────────────────

/// 🔴 LE TÉMOIN DU RETRAIT. Retirer les titres de démo efface leur dossier
/// et leurs pistes, et RIEN d'autre : l'autre racine, ses pistes, et un
/// fichier voisin dans le dossier de données restent.
#[tokio::test]
async fn le_retrait_ne_touche_qu_aux_titres_de_demo() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("demo-tracks");
    let autre = tmp.path().join("ma-musique");
    std::fs::create_dir_all(&demo).unwrap();
    std::fs::create_dir_all(&autre).unwrap();
    std::fs::write(demo.join("01 - Demo CD.flac"), b"x").unwrap();
    std::fs::write(autre.join("a.flac"), b"y").unwrap();
    let voisin = tmp.path().join("tune.db");
    std::fs::write(&voisin, b"base").unwrap();

    let state = etat(&demo, None);
    poser_racines(&state, &[n(&autre), n(&demo)]);
    let p_demo1 = piste(&state, &demo.join("01 - Demo CD.flac"));
    let p_demo2 = piste(&state, &demo.join("02 - Demo DSD.dsf"));
    let p_autre = piste(&state, &autre.join("a.flac"));

    let (statut, corps) = appel(&state, "POST", "/demo-tracks/remove").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["purged"], json!(2), "{corps}");
    assert_eq!(racines(&state), vec![n(&autre)]);
    assert!(!existe(&state, p_demo1) && !existe(&state, p_demo2));
    assert!(existe(&state, p_autre), "une piste hors démo a été retirée");
    assert!(!demo.exists(), "le dossier des titres de démo est resté");
    assert!(
        autre.join("a.flac").exists(),
        "un fichier hors démo a été effacé"
    );
    assert!(
        voisin.exists(),
        "un fichier du dossier de données a été effacé"
    );

    // Relancé sans rien à retirer : pas d'erreur.
    let (statut, _) = appel(&state, "POST", "/demo-tracks/remove").await;
    assert_eq!(statut, StatusCode::OK);
}
