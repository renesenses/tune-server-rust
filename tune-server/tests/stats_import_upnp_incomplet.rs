//! `GET /library/stats` annonçait les pistes UPnP importées SANS RÉSERVE.
//!
//! Mesuré par une session paire sur le .18 le 24/09/2026 : 49 395 pistes
//! importées sur 50 772 vues, plafond de 1 000 conteneurs atteint, 34
//! paginations interrompues. Le dernier bilan de la source synchronisée (table
//! `upnp_library_sources`, champ `report`) le savait — `complet: false` —, mais
//! `library/stats` n'en disait rien : le compte `tracks_by_source.upnp` se
//! lisait comme la bibliothèque distante entière.
//!
//! Le témoin pose en base le bilan tel que `synchronisation_upnp::run_one`
//! l'écrit, puis appelle la route réelle. Il exige le champ ADDITIF
//! `upnp_import` et vérifie que les champs existants sont toujours là.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` de `Cargo.toml`, sans quoi il ne serait jamais compilé.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::DbBackend;
use tune_server::state::AppState;

/// Le bilan d'une passe d'indexation, aux champs que `indexer` écrit.
fn bilan_du_18() -> Value {
    let erreurs: Vec<String> = (0..34)
        .map(|i| format!("NAS : pagination interrompue (0/{})", 10 + i))
        .collect();
    json!({
        "indexe": true,
        "complet": false,
        "parcours": {
            "conteneurs_visites": 1000,
            "items_vus": 50772,
            "plafond_atteint": "conteneurs",
            "plafond": {
                "nature": "conteneurs",
                "valeur": 1000,
                "reglage": "upnp_index_max_conteneurs",
                "message": "plafond de CONTENEURS atteint (1000) : le parcours a visité autant de dossiers qu'il s'y autorise",
            },
        },
        "pistes": { "distinctes": 49395, "ajoutees": 0, "mises_a_jour": 49395,
                    "ecartees_sans_url_de_lecture": 0, "sans_res_size": 0 },
        "erreurs": erreurs,
    })
}

fn bilan_complet() -> Value {
    json!({
        "indexe": true,
        "complet": true,
        "parcours": { "conteneurs_visites": 12, "items_vus": 300, "plafond_atteint": null },
        "pistes": { "distinctes": 300, "ecartees_sans_url_de_lecture": 0 },
        "erreurs": [],
    })
}

fn poser_source(etat: &AppState, cle: &str, nom: &str, statut: &str, bilan: Value) {
    let source = json!({
        "key": cle, "udn": format!("uuid:{cle}"), "container": "0", "name": nom,
        "enabled": true, "status": statut, "last_attempt": 1_790_000_000i64,
        "last_success": null, "report": bilan, "generation": "g1", "pending": [],
    });
    let corps = source.to_string();
    let udn = format!("uuid:{cle}");
    let conteneur = "0".to_string();
    let cle = cle.to_string();
    etat.backend
        .execute(
            "INSERT INTO upnp_library_sources (source_key, udn, container, state_json) VALUES (?, ?, ?, ?)",
            &[&cle, &udn, &conteneur, &corps],
        )
        .expect("source posée");
}

async fn stats(etat: AppState) -> Value {
    let app = tune_server::routes::router(etat);
    let reponse = app
        .oneshot(
            Request::get("/api/v1/library/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("routeur en échec");
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps lisible");
    serde_json::from_slice(&octets).expect("JSON")
}

#[tokio::test]
async fn un_import_upnp_tronque_est_dit_par_library_stats() {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    poser_source(&etat, "nas", "NAS du .18", "partial", bilan_du_18());
    let corps = stats(etat).await;

    // Les champs existants restent (champ additif).
    for champ in ["artists", "albums", "tracks", "tracks_by_source"] {
        assert!(
            corps.get(champ).is_some(),
            "champ existant {champ} perdu.\n{corps}"
        );
    }
    let import = &corps["upnp_import"];
    assert_eq!(
        import["complet"], false,
        "le dernier bilan de la source dit `complet: false` (plafond de 1 000 \
         conteneurs, 34 paginations interrompues) et `library/stats` ne le dit \
         pas : le compte UPnP se lit comme la bibliothèque entière.\n{corps}"
    );
    assert_eq!(import["sources"], 1, "{corps}");
    assert_eq!(import["sources_incompletes"], 1, "{corps}");
    assert_eq!(
        import["plafonds_atteints"],
        json!(["conteneurs"]),
        "{corps}"
    );
    assert_eq!(import["paginations_interrompues"], 34, "{corps}");
    assert_eq!(import["erreurs"], 34, "{corps}");
    assert_eq!(import["pistes_distinctes"], 49395, "{corps}");
    assert_eq!(import["items_vus"], 50772, "{corps}");
    let raison = import["raisons"][0].as_str().unwrap_or("");
    assert!(
        raison.starts_with("NAS du .18 : ")
            && raison.contains("CONTENEURS")
            && raison.contains("34 pagination(s) interrompue(s)"),
        "la raison doit nommer la source, le plafond et les paginations.\n{corps}"
    );
    assert_eq!(import["par_source"][0]["complet"], false, "{corps}");
    assert_eq!(import["par_source"][0]["status"], "partial", "{corps}");
}

/// Contrôle : une source complète n'est pas déclarée tronquée, et une
/// bibliothèque sans source UPnP n'a pas d'import à juger (`complet: null`).
#[tokio::test]
async fn un_import_complet_ou_absent_n_est_pas_declare_tronque() {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    poser_source(&etat, "ok", "NAS sain", "ready", bilan_complet());
    let corps = stats(etat).await;
    let import = &corps["upnp_import"];
    assert_eq!(import["complet"], true, "{corps}");
    assert_eq!(import["sources_incompletes"], 0, "{corps}");
    assert_eq!(import["raisons"], json!([]), "{corps}");

    let vide = stats(AppState::new(":memory:", 0, Default::default()).unwrap()).await;
    assert_eq!(vide["upnp_import"]["sources"], 0, "{vide}");
    assert!(vide["upnp_import"]["complet"].is_null(), "{vide}");
}

/// Une source jamais importée (souscrite, passe pas encore terminée) n'est
/// pas un import complet.
#[tokio::test]
async fn une_source_jamais_importee_n_est_pas_complete() {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    poser_source(&etat, "neuf", "NAS neuf", "pending", json!({}));
    let corps = stats(etat).await;
    let import = &corps["upnp_import"];
    assert_eq!(import["complet"], false, "{corps}");
    assert!(
        import["raisons"][0]
            .as_str()
            .unwrap_or("")
            .contains("aucun import terminé"),
        "{corps}"
    );
}
