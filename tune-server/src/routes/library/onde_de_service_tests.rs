//! Témoins du go du 17/11 (rc13) : l'enveloppe de la barre pour une piste
//! Qobuz ou TIDAL en lecture. Le calcul est SIMULÉ (aucun service, aucun
//! réseau, aucun son) : on compte les analyses lancées.

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::to_bytes;
use tokio::sync::oneshot;

use super::*;

fn piste(id: &str) -> PisteDeService {
    PisteDeService::du_service("qobuz", id).expect("qobuz est analysable")
}

fn ondes() -> (tempfile::TempDir, Arc<Ondes>) {
    let dir = tempfile::tempdir().unwrap();
    let ondes = Arc::new(Ondes::new(dir.path().join("ondes-service")));
    (dir, ondes)
}

/// Laisse tourner les tâches de fond.
async fn laisser_finir() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

async fn corps(r: Response) -> (StatusCode, serde_json::Value) {
    let statut = r.status();
    let octets = to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (statut, serde_json::from_slice(&octets).unwrap())
}

#[test]
fn piste_de_service_depuis_le_service_et_son_identifiant() {
    assert!(PisteDeService::du_service("qobuz", "48098083").is_some());
    assert!(PisteDeService::du_service(" tidal ", " 123 ").is_some());
    for (s, id) in [("spotify", "x"), ("radio", "1"), ("podcast", "1"), ("local", "1"), ("qobuz", "  ")] {
        assert!(PisteDeService::du_service(s, id).is_none(), "{s}/{id}");
    }
    assert_eq!(piste("1").cle(), piste("1").cle());
    assert_ne!(piste("1").cle(), piste("2").cle());
}

#[tokio::test]
async fn une_seule_analyse_par_piste_pending_puis_prete_puis_cache_disque() {
    let (_dir, ondes) = ondes();
    let analyses = Arc::new(AtomicUsize::new(0));
    let (liberer, attendre) = oneshot::channel::<()>();
    let mut attendre = Some(attendre);

    let p = piste("48098083");
    // Première demande : « pas encore », l'analyse part en fond.
    let n = Arc::clone(&analyses);
    let rx = attendre.take().unwrap();
    let etat = ondes.consulter(&p, move || async move {
        n.fetch_add(1, Ordering::SeqCst);
        let _ = rx.await;
        Ok(vec![0.1, 0.9, 0.4])
    });
    assert_eq!(etat, Etat::EnCours);

    // Demandes suivantes pendant le calcul : « pas encore », RIEN de relancé.
    for _ in 0..5 {
        let n = Arc::clone(&analyses);
        let etat = ondes.consulter(&p, move || async move {
            n.fetch_add(1, Ordering::SeqCst);
            Ok(vec![1.0, 1.0])
        });
        assert_eq!(etat, Etat::EnCours);
    }
    laisser_finir().await;
    assert_eq!(analyses.load(Ordering::SeqCst), 1);

    liberer.send(()).unwrap();
    laisser_finir().await;
    let etat = ondes.consulter(&p, || async { panic!("déjà en cache : rien à recalculer") });
    assert_eq!(etat, Etat::Prete(vec![0.1, 0.9, 0.4]));

    // Le cache est sur le DISQUE : un autre suivi (redémarrage) le relit.
    let relu = Arc::new(Ondes::new(ondes.dossier.clone()));
    let etat = relu.consulter(&p, || async { panic!("cache disque ignoré") });
    assert_eq!(etat, Etat::Prete(vec![0.1, 0.9, 0.4]));
    assert_eq!(analyses.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refus_definitif_retenu_echec_passager_retenu_puis_503() {
    let (_dir, ondes) = ondes();
    let p = piste("1");
    ondes.consulter(&p, || async { Err(Refus::NonDiffusable) });
    laisser_finir().await;
    let etat = ondes.consulter(&p, || async { panic!("refus définitif : jamais relancé") });
    assert_eq!(etat, Etat::Refusee(Refus::NonDiffusable));
    let (statut, v) = corps(reponse(etat)).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(v["waveform"], serde_json::Value::Null);
    assert_eq!(v["raison"], "non_diffusable");

    let q = piste("2");
    ondes.consulter(&q, || async { Err(Refus::DelaiDepasse) });
    laisser_finir().await;
    let etat = ondes.consulter(&q, || async { panic!("échec passager : retenu deux minutes") });
    assert_eq!(etat, Etat::Refusee(Refus::DelaiDepasse));
    let (statut, _) = corps(reponse(etat)).await;
    assert_eq!(statut, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn plafond_des_calculs_en_route() {
    let (_dir, ondes) = ondes();
    let analyses = Arc::new(AtomicUsize::new(0));
    let mut gardes = Vec::new();
    for i in 0..(CALCULS_EN_ROUTE_MAX + 3) {
        let (tx, rx) = oneshot::channel::<()>();
        gardes.push(tx);
        let n = Arc::clone(&analyses);
        let etat = ondes.consulter(&piste(&format!("p{i}")), move || async move {
            n.fetch_add(1, Ordering::SeqCst);
            let _ = rx.await;
            Ok(vec![0.5, 0.5])
        });
        assert_eq!(etat, Etat::EnCours);
    }
    laisser_finir().await;
    assert_eq!(analyses.load(Ordering::SeqCst), CALCULS_EN_ROUTE_MAX);
    drop(gardes);
}

#[tokio::test]
async fn reponses_http() {
    let (statut, v) = corps(reponse(Etat::EnCours)).await;
    assert_eq!(statut, StatusCode::ACCEPTED);
    assert_eq!(v["pending"], true);
    assert_eq!(v["waveform"], serde_json::Value::Null);

    let (statut, v) = corps(reponse(Etat::Prete(vec![0.25, 1.0]))).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(v["waveform"], serde_json::json!([0.25, 1.0]));

    let (statut, v) = corps(reponse(Etat::Refusee(Refus::NonPrisEnCharge))).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(v["raison"], "service_non_pris_en_charge");
}
