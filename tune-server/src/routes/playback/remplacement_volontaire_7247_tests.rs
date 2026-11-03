//! #7247 : le contrat de réponse partagé des routes de lecture distingue un
//! remplacement volontaire d'un vrai échec. Base en mémoire, aucun appareil.
use super::build_zone_json_with_result;
use crate::state::AppState;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::{PLAY_SUPERSEDED, PlayResult};

async fn charge_utile(cause: &str, output_sent: bool) -> serde_json::Value {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create(
            "HQPlayer factice",
            Some("hqplayer"),
            Some("hqplayer-factice"),
        )
        .unwrap();
    build_zone_json_with_result(
        &state,
        zone,
        &PlayResult {
            stream_url: None,
            output_sent,
            source: "tidal".into(),
            error: Some(cause.into()),
        },
    )
    .await
}

#[tokio::test]
async fn un_remplacement_volontaire_n_est_pas_une_erreur_client_7247() {
    let valeur = charge_utile(PLAY_SUPERSEDED, false).await;
    assert!(
        valeur.get("error").is_none(),
        "#7247 : remplacer volontairement une préparation ne doit pas afficher une erreur au client : {valeur}"
    );
    assert_eq!(valeur["output_sent"], false);
}

#[tokio::test]
async fn un_vrai_refus_reste_visible_dans_la_reponse_7247() {
    let valeur = charge_utile("HQPlayer a refusé Play", false).await;
    assert_eq!(
        valeur["error"], "HQPlayer a refusé Play",
        "ne pas masquer les vrais refus en taisant les remplacements"
    );
    assert_eq!(valeur["output_sent"], false);
}

#[tokio::test]
async fn un_avertissement_de_sortie_acceptee_reste_visible_7247() {
    let valeur = charge_utile("une sortie du groupe est indisponible", true).await;
    assert_eq!(valeur["error"], "une sortie du groupe est indisponible");
    assert_eq!(valeur["output_sent"], true);
}
