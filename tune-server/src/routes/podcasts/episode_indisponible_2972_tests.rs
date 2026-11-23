//! Forum, fil 2972 — un épisode que l'éditeur ne sert plus (404).
//!
//! Le flux de Radio France listait encore un épisode dont le fichier avait
//! disparu. Tune « démarrait », le compteur avançait d'une ou deux secondes,
//! puis revenait en arrière, sans son ni message.
//!
//! Le banc : un faux éditeur local qui sert un flux RSS de deux épisodes,
//! l'un présent (`/present.mp3`, de l'audio), l'autre absent (`/absent.mp3`,
//! 404). La zone est une zone DLNA factice (`MockOutput`) : aucun son n'est
//! produit. Le faux éditeur compte les demandes faites à chaque fichier.
//!
//! Témoins :
//!  1. cliquer l'épisode absent répond `episode_unavailable`, n'envoie rien à
//!     la sortie, laisse la zone à l'arrêt, et ne pose qu'UNE question à
//!     l'éditeur (aucune boucle, aucune relance) ;
//!  2. la liste ne sonde rien à l'affichage, puis, après le clic, marque
//!     l'épisode absent `unavailable: true` ;
//!  3. l'épisode présent n'est pas touché : il part vers la sortie, et la
//!     liste le dit disponible.
//!
//! Contre-épreuve : retirer le bloc « Fil 2972 » de `play_episode` fait
//! tomber les témoins 1 et 2 (statut 200, épisode relayé, rien de marqué).

use crate::state::AppState;
use axum::{Router, body::Body, http::Request, http::StatusCode, routing::get};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::PlayState;

const APPAREIL: &str = "uuid:salon-2972";

struct Banc {
    app: Router,
    state: AppState,
    zone: i64,
    base: String,
    /// Adresses propres au banc : le registre des marques est partagé.
    absent: String,
    present: String,
    demandes_absent: Arc<AtomicUsize>,
    demandes_present: Arc<AtomicUsize>,
    amont: tokio::task::JoinHandle<()>,
}
impl Drop for Banc {
    fn drop(&mut self) {
        self.amont.abort();
        tune_core::streaming::episodes_indisponibles::oublier(&self.absent);
        tune_core::streaming::episodes_indisponibles::oublier(&self.present);
        tune_core::taches_de_fond::priorite::oublier_la_zone(self.zone);
    }
}

async fn banc(nom: &str) -> Banc {
    let demandes_absent = Arc::new(AtomicUsize::new(0));
    let demandes_present = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let absent = format!("{base}/{nom}/absent.mp3");
    let present = format!("{base}/{nom}/present.mp3");
    let flux = format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Le concert de l'après-midi</title>
<item><title>Épisode retiré</title><pubDate>Thu, 01 Jan 2015 10:00:00 +0000</pubDate><enclosure url="{absent}" type="audio/mpeg"/></item>
<item><title>Épisode en ligne</title><pubDate>Thu, 01 Jan 2015 11:00:00 +0000</pubDate><enclosure url="{present}" type="audio/mpeg"/></item>
</channel></rss>"#
    );
    let (da, dp) = (demandes_absent.clone(), demandes_present.clone());
    let amont = Router::new()
        .route(
            &format!("/{nom}/flux.xml"),
            get(move || {
                let flux = flux.clone();
                async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/rss+xml")],
                        flux,
                    )
                }
            }),
        )
        .route(
            &format!("/{nom}/absent.mp3"),
            get(move || {
                da.fetch_add(1, Ordering::SeqCst);
                async { (StatusCode::NOT_FOUND, "not found") }
            }),
        )
        .route(
            &format!("/{nom}/present.mp3"),
            get(move || {
                dp.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        [(axum::http::header::CONTENT_TYPE, "audio/mpeg")],
                        vec![0u8; 64],
                    )
                }
            }),
        );
    let amont = tokio::spawn(async move {
        axum::serve(listener, amont).await.unwrap();
    });
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon — témoin 2972", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
    ));
    let app = crate::routes::router(state.clone());
    Banc {
        app,
        state,
        zone,
        base: format!("{base}/{nom}"),
        absent,
        present,
        demandes_absent,
        demandes_present,
        amont,
    }
}

async fn appel(b: &Banc, req: Request<Body>) -> (StatusCode, Value) {
    let rep = b.app.clone().oneshot(req).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&octets).unwrap_or_else(|e| {
        panic!(
            "corps non JSON ({e}) : {}",
            String::from_utf8_lossy(&octets)
        )
    });
    (statut, v)
}

async fn cliquer(b: &Banc, adresse: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(format!("/api/v1/podcasts/play/{}", b.zone))
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"audio_url": adresse, "title": "Épisode", "podcast_name": "Le concert de l'après-midi"})
                .to_string(),
        ))
        .unwrap();
    appel(b, req).await
}

/// Les épisodes de la liste, par leur adresse audio.
async fn liste(b: &Banc) -> Vec<Value> {
    let flux = format!("{}/flux.xml", b.base);
    let req = Request::get(format!(
        "/api/v1/podcasts/episodes?feed_url={}",
        urlencoding::encode(&flux)
    ))
    .body(Body::empty())
    .unwrap();
    let (statut, v) = appel(b, req).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    v["episodes"].as_array().cloned().unwrap_or_default()
}

fn episode<'a>(liste: &'a [Value], adresse: &str) -> &'a Value {
    liste
        .iter()
        .find(|e| e["audio_url"] == adresse)
        .unwrap_or_else(|| panic!("{adresse} absent de la liste : {liste:?}"))
}

async fn titres_envoyes(b: &Banc) -> Vec<String> {
    let registre = b.state.outputs.lock().await;
    let sortie = registre.get(APPAREIL).expect("sortie enregistrée");
    let garde = sortie.lock().await;
    garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice")
        .play_titles()
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_episode_en_404_chez_l_editeur_repond_episode_unavailable_sans_boucle_2972() {
    let b = banc("t1").await;
    let (statut, r) = cliquer(&b, &b.absent).await;
    assert_eq!(
        r["code"], "episode_unavailable",
        "fil 2972 : un fichier en 404 chez l'éditeur se dit clairement : {statut} {r}"
    );
    assert!(
        !statut.is_success(),
        "fil 2972 : un épisode introuvable n'est pas un succès : {statut} {r}"
    );
    assert_eq!(r["output_sent"], false, "{r}");
    assert!(
        titres_envoyes(&b).await.is_empty(),
        "fil 2972 : rien n'est envoyé à l'enceinte pour un fichier absent"
    );
    assert_ne!(
        b.state.playback.get_state(b.zone).await.state,
        PlayState::Playing,
        "fil 2972 : la zone ne fait pas semblant de jouer"
    );
    // Laisser à une éventuelle relance le temps de partir.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        b.demandes_absent.load(Ordering::SeqCst),
        1,
        "fil 2972 : une seule question à l'éditeur, aucune boucle ni relance"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_liste_marque_l_episode_indisponible_apres_le_clic_seulement_2972() {
    let b = banc("t2").await;
    let avant = liste(&b).await;
    assert_eq!(
        episode(&avant, &b.absent)["unavailable"],
        false,
        "fil 2972 : avant tout clic, rien n'est marqué : {avant:?}"
    );
    assert_eq!(
        b.demandes_absent.load(Ordering::SeqCst) + b.demandes_present.load(Ordering::SeqCst),
        0,
        "fil 2972 : aucune sonde des fichiers à l'affichage de la liste"
    );
    let (_, r) = cliquer(&b, &b.absent).await;
    assert_eq!(r["code"], "episode_unavailable", "{r}");
    let apres = liste(&b).await;
    assert_eq!(
        episode(&apres, &b.absent)["unavailable"],
        true,
        "fil 2972 : l'épisode refusé par l'éditeur est marqué dans la liste : {apres:?}"
    );
    assert_eq!(
        episode(&apres, &b.present)["unavailable"],
        false,
        "fil 2972 : son voisin, lui, reste disponible : {apres:?}"
    );
    assert_eq!(
        b.demandes_absent.load(Ordering::SeqCst),
        1,
        "fil 2972 : relire la liste ne sonde toujours rien"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_episode_disponible_n_est_pas_affecte_2972() {
    let b = banc("t3").await;
    let (statut, r) = cliquer(&b, &b.present).await;
    assert_eq!(statut, StatusCode::OK, "{r}");
    assert_eq!(r["output_sent"], true, "{r}");
    assert!(r["code"].is_null(), "{r}");
    assert_eq!(titres_envoyes(&b).await, vec!["Épisode".to_string()]);
    let apres = liste(&b).await;
    assert_eq!(
        episode(&apres, &b.present)["unavailable"],
        false,
        "fil 2972 : un épisode servi n'est jamais marqué : {apres:?}"
    );
}

/// Suite de la relecture (#9453) : la sonde du clic part seulement si le
/// fichier sera réellement demandé. Une zone inconnue, ou dont la sortie
/// n'est pas encore découverte, ne coûte aucune requête à l'éditeur.
///
/// Contre-épreuve : remettre la sonde avant la lecture de la zone (ou
/// rendre `le_fichier_sera_demande` toujours vrai) fait tomber ce témoin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sans_zone_ni_sortie_aucune_requete_a_l_editeur_2972() {
    let b = banc("t4").await;
    // Zone inconnue.
    let req = Request::builder()
        .uri("/api/v1/podcasts/play/987654")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"audio_url": b.absent, "title": "Épisode"}).to_string(),
        ))
        .unwrap();
    let (_, r) = appel(&b, req).await;
    assert_ne!(r["code"], "episode_unavailable", "{r}");
    // Zone dont la sortie n'est pas découverte : la route répond 503.
    let sans_sortie = ZoneRepo::with_backend(b.state.backend.clone())
        .create(
            "Cuisine — témoin 2972",
            Some("dlna"),
            Some("uuid:absent-2972"),
        )
        .unwrap();
    let req = Request::builder()
        .uri(format!("/api/v1/podcasts/play/{sans_sortie}"))
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"audio_url": b.absent, "title": "Épisode"}).to_string(),
        ))
        .unwrap();
    let (statut, r) = appel(&b, req).await;
    assert_eq!(statut, StatusCode::SERVICE_UNAVAILABLE, "{r}");
    assert_eq!(r["code"], "podcast_sortie_introuvable", "{r}");
    tune_core::taches_de_fond::priorite::oublier_la_zone(sans_sortie);
    assert_eq!(
        b.demandes_absent.load(Ordering::SeqCst),
        0,
        "fil 2972 : sans zone ni sortie, rien n'est demandé à l'éditeur"
    );
}
