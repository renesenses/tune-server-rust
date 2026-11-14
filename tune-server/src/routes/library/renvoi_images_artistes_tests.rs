//! rc12 — le renvoi des images d'artistes perdues : armé au démarrage,
//! suspendu avec les images d'artistes, une seule fois, jamais sans
//! consentement.

use super::*;

#[test]
fn le_renvoi_se_suspend_avec_les_images_d_artistes() {
    assert_eq!(
        crate::background_tasks::tache_suspendable(TACHE),
        Some(Tache::ImagesArtistes),
        "le renvoi doit se garer quand l'utilisateur suspend les images d'artistes, \
         et paraître sous cette carte dans « Ce qui tourne »"
    );
}

#[test]
fn le_renvoi_est_arme_au_demarrage() {
    let source = include_str!("../../background.rs");
    assert_eq!(
        source
            .matches("renvoi_images_artistes::armer(state)")
            .count(),
        1,
        "le démarrage n'arme plus le renvoi des images d'artistes"
    );
}

/// Un faux site qui n'a aucune image : il rend tous les MBID demandés comme
/// manquants et compte les envois.
async fn faux_site() -> (String, std::sync::Arc<std::sync::atomic::AtomicU32>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    let envois = std::sync::Arc::new(AtomicU32::new(0));
    let e = envois.clone();
    let app = axum::Router::new()
        .route(
            "/api/v1/community/artist-images/missing",
            axum::routing::post(|axum::Json(c): axum::Json<serde_json::Value>| async move {
                axum::Json(serde_json::json!({ "missing": c["mbids"] }))
            }),
        )
        .route(
            "/api/v1/community/artist-images",
            axum::routing::post(move || {
                let e = e.clone();
                async move {
                    e.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::CREATED
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://127.0.0.1:{port}"), envois)
}

fn etat_avec_un_artiste(cache: &std::path::Path) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let mut a = tune_core::db::models::Artist::new("Queen".into());
    a.musicbrainz_id = Some("0383dadf-2a4e-4d10-a46a-e9e041da8eb3".into());
    let id = repo.create(&a).unwrap();
    let octets: Vec<u8> = [0xFF, 0xD8, 0xFF, 0xE0]
        .into_iter()
        .chain((0..2048u32).map(|i| i as u8))
        .collect();
    let adresse = tune_core::library::artwork::cache_fetched_image(&octets, cache, "jpg").unwrap();
    repo.update_image(id, &adresse, "auto").unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set("instance_id", "c3c3c3c3-0000-4000-8000-000000000001")
        .unwrap();
    state
}

fn reglages(base: &str, cache: &std::path::Path) -> renvoi::Reglages {
    renvoi::Reglages {
        base_url: base.to_string(),
        instance_id: "c3c3c3c3-0000-4000-8000-000000000001".into(),
        version: version().to_string(),
        cache_dir: cache.to_path_buf(),
        cadence: Duration::ZERO,
        attente_transitoire: Duration::from_millis(10),
        essais_max: 2,
    }
}

#[tokio::test]
async fn sans_consentement_le_renvoi_ne_tourne_pas() {
    let cache = tune_core::test_scratch::scratch_dir("renvoi-serveur-consentement");
    let state = etat_avec_un_artiste(&cache);
    let (base, envois) = faux_site().await;

    assert!(
        executer(&state, Some(reglages(&base, &cache)))
            .await
            .is_none()
    );
    assert_eq!(envois.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn le_renvoi_tourne_une_seule_fois_et_quitte_le_registre() {
    let cache = tune_core::test_scratch::scratch_dir("renvoi-serveur-une-fois");
    let state = etat_avec_un_artiste(&cache);
    SettingsRepo::with_backend(state.backend.clone())
        .set(tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY, "true")
        .unwrap();
    let (base, envois) = faux_site().await;

    let bilan = executer(&state, Some(reglages(&base, &cache)))
        .await
        .expect("la passe tourne");
    assert!(bilan.terminee);
    assert_eq!(envois.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        state.background_tasks.get(TACHE).is_none(),
        "la passe finie quitte le registre des tâches de fond"
    );

    assert!(
        executer(&state, Some(reglages(&base, &cache)))
            .await
            .is_none(),
        "le marqueur empêche une seconde passe"
    );
    assert_eq!(envois.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn un_renvoi_fait_par_une_version_precedente_ne_se_refait_pas() {
    let cache = tune_core::test_scratch::scratch_dir("renvoi-serveur-version-precedente");
    let state = etat_avec_un_artiste(&cache);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY, "true")
        .unwrap();
    // Le marqueur posé par une version antérieure du serveur.
    settings.set(renvoi::CLE_MARQUEUR, "1.0.0-rc11").unwrap();
    let (base, envois) = faux_site().await;

    assert!(
        executer(&state, Some(reglages(&base, &cache)))
            .await
            .is_none(),
        "le renvoi est unique : une nouvelle version ne le relance pas"
    );
    assert_eq!(envois.load(std::sync::atomic::Ordering::SeqCst), 0);
}
