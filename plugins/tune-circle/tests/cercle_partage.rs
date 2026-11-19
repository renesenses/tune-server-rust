//! Le cercle partagé (site-mozaiklabs#568, rc13) : les quatre interrupteurs
//! `PUT|DELETE /circles/{id}/sharing/members` (propriétaire) et `PUT|DELETE
//! /member-of/{id}/show-me` (membre), relayés contre le faux mozaiklabs.

mod commun;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;

use commun::*;

const ROUTES: [(&str, &str); 4] = [
    ("PUT", "/circles/{id}/sharing/members"),
    ("DELETE", "/circles/{id}/sharing/members"),
    ("PUT", "/member-of/{id}/show-me"),
    ("DELETE", "/member-of/{id}/show-me"),
];

fn chemin(gabarit: &str, id: &str) -> String {
    gabarit.replace("{id}", id)
}

/// Deux cercles d'Alice où je suis rangé : le 5 partagé, le 6 non.
fn range_chez_alice(faux: &Serveur) {
    faux.etat.lock().unwrap().member_of = vec![
        json!({ "circle_id": 5, "owner_id": 7, "owner_name": "Alice",
                "circle_name": "Jazz", "members_see_each_other": true, "show_me": false }),
        json!({ "circle_id": 6, "owner_id": 7, "owner_name": "Alice",
                "circle_name": "Famille", "members_see_each_other": false, "show_me": false }),
    ];
}

fn gestes(faux: &Serveur) -> Vec<(String, Vec<u8>)> {
    faux.etat.lock().unwrap().gestes_entre_membres.clone()
}

#[tokio::test]
async fn le_proprietaire_partage_puis_ne_partage_plus_son_cercle() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    // Un corps envoyé par le client ne part pas : le contrat n'en a pas.
    let r = appel(
        &app,
        "PUT",
        "/circles/1/sharing/members",
        Some(json!({ "server_id": "srv-pirate" })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({ "id": 1, "name": "Famille", "member_ids": [7, 9],
                "members_see_each_other": true })
    );
    assert_eq!(faux.etat.lock().unwrap().cercles_entre_membres, vec![1]);

    let r = appel(&app, "DELETE", "/circles/1/sharing/members", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()["members_see_each_other"], json!(false));
    assert!(faux.etat.lock().unwrap().cercles_entre_membres.is_empty());

    assert_eq!(
        gestes(&faux),
        vec![
            ("PUT /circles/1/sharing/members".to_string(), Vec::new()),
            ("DELETE /circles/1/sharing/members".to_string(), Vec::new()),
        ]
    );
}

#[tokio::test]
async fn le_cercle_d_un_autre_rend_le_404_du_cloud() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));
    for m in ["PUT", "DELETE"] {
        let r = appel(
            &app,
            m,
            &format!("/circles/{CERCLE_D_UN_AUTRE}/sharing/members"),
            None,
        )
        .await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{m}");
        assert_eq!(r.json(), json!({ "error": "not_found" }), "{m}");
    }
    assert_eq!(gestes(&faux).len(), 2, "le cloud a jugé, pas le greffon");
}

#[tokio::test]
async fn le_membre_se_montre_puis_se_retire() {
    let faux = demarrer().await;
    range_chez_alice(&faux);
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(&app, "PUT", "/member-of/5/show-me", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({ "circle_id": 5, "owner_id": 7, "owner_name": "Alice",
                "circle_name": "Jazz", "members_see_each_other": true, "show_me": true })
    );
    let r = appel(&app, "DELETE", "/member-of/5/show-me", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()["show_me"], json!(false));
    assert_eq!(
        gestes(&faux)
            .into_iter()
            .map(|(g, _)| g)
            .collect::<Vec<_>>(),
        vec!["PUT /member-of/5/show-me", "DELETE /member-of/5/show-me"]
    );
}

#[tokio::test]
async fn le_409_circle_not_shared_et_le_404_hors_membre_sont_relayes() {
    let faux = demarrer().await;
    range_chez_alice(&faux);
    let app = app(base(&faux.base, Some(JETON)));
    for m in ["PUT", "DELETE"] {
        let r = appel(&app, m, "/member-of/6/show-me", None).await;
        assert_eq!(r.statut, StatusCode::CONFLICT, "{m}");
        assert_eq!(r.json(), json!({ "error": "circle_not_shared" }), "{m}");

        let r = appel(&app, m, "/member-of/99/show-me", None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{m}");
        assert_eq!(r.json(), json!({ "error": "not_found" }), "{m}");
    }
}

#[tokio::test]
async fn un_identifiant_non_numerique_est_refuse_sans_appel() {
    let faux = demarrer().await;
    range_chez_alice(&faux);
    let app = app(base(&faux.base, Some(JETON)));
    let trop_long = "1".repeat(21);
    for (m, gabarit) in ROUTES {
        for id in [
            "abc",
            "5a",
            "-5",
            "5.0",
            "%2E%2E",
            "%2E",
            "%20",
            "5%2F..",
            trop_long.as_str(),
        ] {
            let c = chemin(gabarit, id);
            let r = appel(&app, m, &c, None).await;
            assert_eq!(r.statut, StatusCode::NOT_FOUND, "{m} {c}");
            assert_eq!(r.json(), json!({ "code": "circle.not_found" }), "{m} {c}");
        }
    }
    assert_eq!(faux.etat.lock().unwrap().appels, 0, "rien n'est parti");
}

#[tokio::test]
async fn un_403_du_cloud_est_relaye_avec_son_corps() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().refus_entre_membres = Some((403, "forbidden"));
    let app = app(base(&faux.base, Some(JETON)));
    for (m, gabarit) in ROUTES {
        let c = chemin(gabarit, "5");
        let r = appel(&app, m, &c, None).await;
        assert_eq!(r.statut, StatusCode::FORBIDDEN, "{m} {c}");
        assert_eq!(r.json(), json!({ "error": "forbidden" }), "{m} {c}");
    }
}

#[tokio::test]
async fn sans_session_ou_session_refusee_412_jamais_401() {
    // Sans session SSO : rien ne part.
    let faux = demarrer().await;
    let app_sans = app(base(&faux.base, None));
    for (m, gabarit) in ROUTES {
        let r = appel(&app_sans, m, &chemin(gabarit, "5"), None).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {gabarit}");
        assert_eq!(
            r.json(),
            json!({ "connected": false, "code": "circle.not_connected" })
        );
    }
    assert_eq!(faux.etat.lock().unwrap().appels, 0);

    // 401 du cloud, puis rafraîchissement refusé : 412, jamais 401.
    faux.etat.lock().unwrap().rafraichissement_valide = "autre".into();
    let app_perimee = app(base(&faux.base, Some(JETON_PERIME)));
    for (m, gabarit) in ROUTES {
        let r = appel(&app_perimee, m, &chemin(gabarit, "5"), None).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {gabarit}");
        assert_eq!(r.json()["code"], json!("circle.not_connected"));
    }
}

#[tokio::test]
async fn un_jeton_perime_est_rafraichi_puis_le_geste_passe() {
    let faux = demarrer().await;
    range_chez_alice(&faux);
    let backend = base(&faux.base, Some(JETON_PERIME));
    let app = app(backend.clone());
    let r = appel(&app, "PUT", "/member-of/5/show-me", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()["show_me"], json!(true));
    assert_eq!(
        SettingsRepo::with_backend(backend)
            .get("mozaik_access_token")
            .unwrap()
            .as_deref(),
        Some(JETON_NEUF)
    );
}

#[tokio::test]
async fn cloud_en_panne_ou_injoignable_503_lisible() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().panne = true;
    let app_panne = app(base(&faux.base, Some(JETON)));
    let app_morte = app(base(&adresse_morte().await, Some(JETON)));
    for (m, gabarit) in ROUTES {
        let c = chemin(gabarit, "5");
        let r = appel(&app_panne, m, &c, None).await;
        assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE, "{m} {c}");
        assert_eq!(
            r.json(),
            json!({ "connected": true, "code": "circle.cloud_unavailable",
                    "upstream_status": 500 })
        );
        let r = appel(&app_morte, m, &c, None).await;
        assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE, "{m} {c}");
        assert_eq!(r.json()["upstream_status"], Value::Null);
    }
}
