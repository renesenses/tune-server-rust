//! #8795 point 2 — la sonde des VU-mètres d'une piste TIDAL avancée sans
//! blanc ne télécharge plus rien : elle vise un fMP4 complet déjà au cache
//! d'URL (ou épinglé, #8701), sinon elle ne tourne pas.
//!
//! Le faux TIDAL compte toutes les requêtes qu'il reçoit et répond 500.
//!
//! Contre-épreuve : faire passer TIDAL par `flux_de_la_sonde_de_niveaux`
//! (le chemin d'avant, `get_track_url`) fait tomber
//! `sans_fichier_complet_la_sonde_ne_demande_rien_a_tidal_8795`.
use super::flux_de_la_sonde_apres_avance;
use crate::streaming::tidal::TidalService;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn faux_tidal() -> (String, Arc<AtomicUsize>) {
    use axum::http::StatusCode;
    let requetes = Arc::new(AtomicUsize::new(0));
    let compte = requetes.clone();
    let app = axum::Router::new().fallback(move || {
        let compte = compte.clone();
        async move {
            compte.fetch_add(1, Ordering::SeqCst);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (base, requetes)
}

#[tokio::test]
async fn sans_fichier_complet_la_sonde_ne_demande_rien_a_tidal_8795() {
    let (base, requetes) = faux_tidal().await;
    let svc = TidalService::pour_un_faux_tidal(base);
    let flux = flux_de_la_sonde_apres_avance(&svc, "tidal", "88800717", None).await;
    assert!(flux.is_none(), "rien au cache : pas de sonde");
    assert_eq!(
        requetes.load(Ordering::SeqCst),
        0,
        "la sonde a interrogé TIDAL : la piste qui joue serait retéléchargée (#8795)"
    );
}

#[tokio::test]
async fn un_fichier_complet_au_cache_est_sonde_sans_requete_8795() {
    let (base, requetes) = faux_tidal().await;
    let svc = TidalService::pour_un_faux_tidal(base);
    let fichier = crate::test_scratch::scratch_file("tune-dash-8795-sonde", ".mp4");
    std::fs::write(fichier.path(), b"fMP4 complet").unwrap();
    let url = format!("file://{}", fichier.path().display());
    svc.mettre_au_cache_8795("88800717", "LOSSLESS", &url).await;
    let flux = flux_de_la_sonde_apres_avance(&svc, "tidal", "88800717", Some("LOSSLESS")).await;
    assert_eq!(
        flux.map(|f| f.url),
        Some(url),
        "le fichier au cache est sondé"
    );
    assert_eq!(requetes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn un_fichier_disparu_ne_relance_pas_de_telechargement_8795() {
    let (base, requetes) = faux_tidal().await;
    let svc = TidalService::pour_un_faux_tidal(base);
    svc.mettre_au_cache_8795(
        "88800717",
        "LOSSLESS",
        "file:///nulle-part/tune-dash-8795-disparu.mp4",
    )
    .await;
    let flux = flux_de_la_sonde_apres_avance(&svc, "tidal", "88800717", Some("LOSSLESS")).await;
    assert!(flux.is_none(), "fichier consommé : pas de sonde");
    assert_eq!(requetes.load(Ordering::SeqCst), 0);
}

/// Le préchauffage de la suivante ne supprime plus un fMP4 qu'une zone
/// épingle (#8701) : c'est lui que l'entrée du cache désigne après l'avance.
///
/// Contre-épreuve : revenir au `remove_file` sans condition dans
/// `consommer_le_fmp4_prechauffe` fait tomber ce test.
#[test]
fn le_prechauffage_garde_un_fmp4_epingle_8795() {
    let dossier = crate::test_scratch::scratch_dir("dash-8795-prechauffe");
    let chemin = dossier
        .path()
        .join("tune-dash-8795-prechauffe.mp4")
        .to_string_lossy()
        .to_string();
    std::fs::write(&chemin, b"fMP4").unwrap();
    let zone = 879_502;
    crate::audio::dash_epingles::noter_la_piste_resolue(zone, Some(&format!("file://{chemin}")));
    let supprime = super::consommer_le_fmp4_prechauffe(&chemin);
    let reste = std::path::Path::new(&chemin).exists();
    crate::audio::dash_epingles::liberer_la_zone(zone);
    assert!(
        !supprime && reste,
        "le préchauffage a supprimé un fMP4 épinglé"
    );

    assert!(
        super::consommer_le_fmp4_prechauffe(&chemin),
        "désépinglé : consommé comme avant"
    );
    assert!(!std::path::Path::new(&chemin).exists());
}
