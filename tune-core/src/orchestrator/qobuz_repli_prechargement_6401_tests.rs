//! #6401 — le préchargement sans blanc (`prefetch.rs`) et la sonde de
//! niveaux après une avance sans blanc (`queue.rs`) passent par le même
//! repli Qobuz en 16/44,1 que la lecture explicite (`resolve_stream`).
//!
//! Contre un Qobuz simulé en local (celui des essais de
//! `streaming::qobuz_repli_qualite`). Aucun son : rien n'est décodé ni joué,
//! seuls des octets factices transitent.
//!
//! Contre-épreuve (19/10/2026, voir la PR) : rendre le flux de
//! `get_track_url` sans passer par `retenir_le_flux_du_service`, dans
//! `resoudre_le_flux_a_precharger` et dans `flux_de_la_sonde_de_niveaux`,
//! fait rougir les trois essais « HR refusé » : le flux retenu reste le
//! format 27.

use crate::prefetch::resoudre_le_flux_a_precharger;
use crate::streaming::StreamingService;
use crate::streaming::qobuz_repli_qualite::retenir_le_flux_du_service;
use crate::streaming::qobuz_repli_qualite::tests::{Cdn, faux_qobuz};

use super::queue::flux_de_la_sonde_de_niveaux;

#[tokio::test]
async fn prechargement_hr_en_erreur_http_precharge_le_format_6() {
    let (etat, mut svc) = faux_qobuz(Cdn::Erreur500, Cdn::Sert).await;

    let flux = resoudre_le_flux_a_precharger(&mut svc, "qobuz", "6401")
        .await
        .expect("le préchargement doit retenir le format 6");

    assert!(
        flux.url.ends_with("/cdn/6.flac"),
        "le préchargement d'un titre refusé en HR doit viser le format 6, pas : {}",
        flux.url
    );
    assert_eq!(flux.quality.sample_rate, 44_100);
    assert_eq!(flux.quality.bit_depth, 16);
    assert_eq!(
        etat.demandes("6"),
        1,
        "le format 6 n'est demandé qu'une fois"
    );
}

#[tokio::test]
async fn prechargement_hr_coupe_au_demarrage_precharge_le_format_6() {
    let (_etat, mut svc) = faux_qobuz(Cdn::Coupe, Cdn::Sert).await;

    let flux = resoudre_le_flux_a_precharger(&mut svc, "qobuz", "6401")
        .await
        .expect("le préchargement doit retenir le format 6");

    assert!(
        flux.url.ends_with("/cdn/6.flac"),
        "un flux HR coupé avant le premier octet doit être préchargé au format 6, pas : {}",
        flux.url
    );
}

/// #6658 — le fil 2350 : le préchargement est la PREMIÈRE des huit
/// tentatives, et le CDN y coupe le flux HR après son premier octet. Le
/// préchargement doit se replier au format 6, comme la lecture.
#[tokio::test]
async fn prechargement_hr_coupe_apres_un_octet_precharge_le_format_6_6658() {
    let (etat, mut svc) = faux_qobuz(Cdn::CoupeApres(1), Cdn::Sert).await;

    let flux = resoudre_le_flux_a_precharger(&mut svc, "qobuz", "6401")
        .await
        .expect("le préchargement doit retenir le format 6");

    assert!(
        flux.url.ends_with("/cdn/6.flac"),
        "un flux HR coupé après son premier octet doit être préchargé au format 6, pas : {}",
        flux.url
    );
    assert_eq!(flux.quality.sample_rate, 44_100);
    assert_eq!(etat.demandes("6"), 1, "un seul repli");
}

/// #6658 — un flux HR lent mais sain reste préchargé en haute résolution.
#[tokio::test]
async fn prechargement_hr_lent_reste_en_haute_resolution_6658() {
    let (etat, mut svc) = faux_qobuz(Cdn::Lent, Cdn::Sert).await;

    let flux = resoudre_le_flux_a_precharger(&mut svc, "qobuz", "6401")
        .await
        .expect("le flux HR lent doit être préchargé");

    assert!(flux.url.ends_with("/cdn/27.flac"), "{}", flux.url);
    assert_eq!(etat.demandes("6"), 0, "aucun repli sur un flux lent");
}

#[tokio::test]
async fn prechargement_hr_sain_reste_en_haute_resolution() {
    let (etat, mut svc) = faux_qobuz(Cdn::Sert, Cdn::Sert).await;

    let flux = resoudre_le_flux_a_precharger(&mut svc, "qobuz", "6401")
        .await
        .expect("le flux HR sain doit être préchargé");

    assert!(flux.url.ends_with("/cdn/27.flac"), "{}", flux.url);
    assert_eq!(flux.quality.sample_rate, 192_000);
    assert_eq!(etat.demandes("6"), 0, "aucun repli sur un flux HR sain");
}

#[tokio::test]
async fn sonde_de_niveaux_apres_avance_vise_le_format_6() {
    let (etat, svc) = faux_qobuz(Cdn::Erreur500, Cdn::Sert).await;

    let flux = flux_de_la_sonde_de_niveaux(&svc, "qobuz", "6401", None)
        .await
        .expect("la sonde de niveaux doit avoir un flux");

    assert!(
        flux.url.ends_with("/cdn/6.flac"),
        "la sonde de niveaux d'un titre refusé en HR doit viser le format 6, pas : {}",
        flux.url
    );
    assert_eq!(etat.demandes("6"), 1);
}

/// Le repli est propre à Qobuz : un autre service garde son flux, sans même
/// une sonde du CDN.
#[tokio::test]
async fn autre_service_flux_inchange_sans_sonde() {
    let (etat, svc) = faux_qobuz(Cdn::Erreur500, Cdn::Sert).await;
    let flux = svc
        .get_track_url("6401", None)
        .await
        .expect("getFileUrl au format 27");

    let retenu = retenir_le_flux_du_service(
        "tidal",
        &svc,
        crate::http::client::shared(),
        "6401",
        None,
        flux,
    )
    .await;

    assert!(!retenu.replie);
    assert!(retenu.flux.url.ends_with("/cdn/27.flac"));
    assert_eq!(etat.telechargements("27"), 0, "aucune sonde hors Qobuz");
    assert_eq!(etat.demandes("6"), 0);
}
