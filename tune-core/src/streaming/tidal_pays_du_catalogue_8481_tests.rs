//! #8481 — le pays du catalogue TIDAL choisi dans les Réglages (comme « PL »
//! dans LMS). Un faux TIDAL sur la boucle locale note le `countryCode` de
//! chaque requête : aucun appel à TIDAL, aucun identifiant.

use super::*;
use crate::streaming::traits::{PaysDuCatalogue, StreamingService};

async fn faux_tidal() -> (TidalService, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::extract::{Query, State};
    use axum::routing::get;

    type Journal = Arc<std::sync::Mutex<Vec<String>>>;

    async fn moi(
        State(journal): State<Journal>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        journal
            .lock()
            .unwrap()
            .push(q.get("countryCode").cloned().unwrap_or_default());
        axum::Json(serde_json::json!({"username": "temoin", "countryCode": "FR"}))
    }

    let journal: Journal = Arc::default();
    let app = axum::Router::new()
        .route("/users/me", get(moi))
        .with_state(journal.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (
        TidalService::pour_un_faux_tidal(format!("http://{adresse}")),
        journal,
    )
}

#[tokio::test]
async fn le_pays_choisi_part_dans_les_requetes_et_survit_au_compte_8481() {
    let (mut svc, journal) = faux_tidal().await;

    // Sans choix : le pays du compte.
    svc.refresh_user_info().await;
    assert_eq!(svc.pays_effectif(), "FR");

    svc.choisir_le_pays_du_catalogue(Some("pl"))
        .expect("PL valide");
    svc.api_get("/users/me")
        .await
        .expect("réponse du faux TIDAL");
    // Le compte reparle (FR) : le choix de l'utilisateur tient.
    svc.refresh_user_info().await;
    assert_eq!(svc.pays_effectif(), "PL");
    assert_eq!(
        svc.pays_du_catalogue(),
        Some(PaysDuCatalogue {
            du_compte: Some("FR".into()),
            choisi: Some("PL".into()),
            effectif: "PL".into(),
        })
    );

    // Rendu au compte.
    svc.choisir_le_pays_du_catalogue(None)
        .expect("retour au compte");
    svc.api_get("/users/me")
        .await
        .expect("réponse du faux TIDAL");

    assert_eq!(
        journal.lock().unwrap().as_slice(),
        ["US", "PL", "PL", "FR"],
        "countryCode de chaque requête, dans l'ordre"
    );
}

#[test]
fn un_pays_mal_forme_est_refuse_sans_rien_changer_8481() {
    let mut svc = TidalService::new();
    svc.choisir_le_pays_du_catalogue(Some("PL")).unwrap();
    for faux in ["POL", "pl-PL", "1A", "P"] {
        assert!(
            svc.choisir_le_pays_du_catalogue(Some(faux)).is_err(),
            "{faux} doit être refusé"
        );
    }
    assert_eq!(svc.pays_effectif(), "PL", "un refus ne touche pas au choix");
    // Une chaîne vide vaut « celui du compte ».
    svc.choisir_le_pays_du_catalogue(Some("  ")).unwrap();
    assert_eq!(svc.pays_du_catalogue().unwrap().choisi, None);
}

#[test]
fn sans_connexion_le_repli_n_est_pas_presente_comme_le_pays_du_compte_8481() {
    let svc = TidalService::new();
    let pays = svc.pays_du_catalogue().unwrap();
    assert_eq!(pays.du_compte, None);
    assert_eq!(pays.effectif, "US");
}

#[test]
fn changer_de_pays_vide_le_cache_a_la_une_8481() {
    let mut svc = TidalService::new();
    *svc.featured_cache.lock().unwrap() = Some(FeaturedCache {
        sections: vec![],
        fetched_at: Instant::now(),
    });
    svc.choisir_le_pays_du_catalogue(Some("PL")).unwrap();
    assert!(svc.featured_cache.lock().unwrap().is_none());
}

#[test]
fn qobuz_ne_laisse_pas_choisir_le_pays_8481() {
    let mut q = crate::streaming::qobuz::QobuzService::new("app".into(), "secret".into());
    assert!(q.pays_du_catalogue().is_none());
    assert!(q.choisir_le_pays_du_catalogue(Some("PL")).is_err());
}
