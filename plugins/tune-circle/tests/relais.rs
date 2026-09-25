//! Le relais du cercle contre un faux mozaiklabs qui implémente le contrat
//! de #5018. Chaque test porte un des témoins demandés pour T1.

mod commun;

use axum::http::StatusCode;
use serde_json::json;
use tune_core::db::settings_repo::SettingsRepo;

use commun::*;

// 1. Non connecté ---------------------------------------------------------

#[tokio::test]
async fn sans_session_sso_l_etat_est_clair_et_rien_ne_part() {
    let faux = demarrer().await;
    for jeton in [None, Some(""), Some("   ")] {
        let app = app(base(&faux.base, jeton));

        let r = appel(&app, "GET", "/", None).await;
        assert_eq!(r.statut, StatusCode::OK);
        assert_eq!(r.json(), json!({ "connected": false }));

        for (m, chemin, corps) in [
            (
                "POST",
                "/invitations",
                Some(json!({ "email": COURRIEL_INVITE })),
            ),
            ("POST", "/invitations/21/accept", None),
            ("POST", "/invitations/21/decline", None),
            ("DELETE", "/invitations/11", None),
            ("DELETE", "/members/7", None),
        ] {
            let r = appel(&app, m, chemin, corps).await;
            assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {chemin}");
            assert_eq!(
                r.json(),
                json!({ "connected": false, "code": "circle.not_connected" }),
                "{m} {chemin}"
            );
        }
    }
    assert_eq!(
        faux.etat.lock().unwrap().appels,
        0,
        "sans session, aucun appel ne doit partir vers le cloud"
    );
}

// 2. La liste relayée à l'identique ---------------------------------------

#[tokio::test]
async fn la_liste_est_relayee_a_l_identique() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(&app, "GET", "/", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), cercle_initial());
    // À l'octet près : ce que le cloud a sérialisé, pas une resérialisation.
    let attendu = serde_json::to_vec(&faux.etat.lock().unwrap().cercle()).unwrap();
    assert_eq!(r.octets, attendu);
    assert_eq!(r.entetes.get("content-type").unwrap(), "application/json");
}

// 3. Invitation : 201 relayé, 429 relayé ; 409 et 422 du cloud relayés ---

#[tokio::test]
async fn l_invitation_relaie_201_puis_429_avec_son_delai() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().invitations_permises = 1;
    let app = app(base(&faux.base, Some(JETON)));

    // Un champ de trop dans le corps du client ne transite pas.
    let r = appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": COURRIEL_INVITE, "admin": true })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::CREATED);
    assert_eq!(
        r.json(),
        json!({ "id": 12, "name_or_email": COURRIEL_INVITE,
                "created_at": "2026-09-25T08:00:00Z", "expires_at": "2026-10-25T08:00:00Z" })
    );
    assert_eq!(
        faux.etat.lock().unwrap().dernier_corps,
        Some(json!({ "email": COURRIEL_INVITE }))
    );

    let r = appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": "autre@exemple.fr" })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.json(), json!({ "message": "Too Many Attempts." }));
    assert_eq!(r.entetes.get("retry-after").unwrap(), "42");
}

/// Le cloud seul juge l'adresse : ses 409 et 422 arrivent avec leur motif,
/// sans qu'un second juge local ne les remplace.
#[tokio::test]
async fn les_refus_409_et_422_du_cloud_sont_relayes_avec_leur_motif() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));
    for (corps, statut, motif) in [
        (
            json!({ "email": COURRIEL_DU_COMPTE }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "self_invitation",
        ),
        (
            json!({ "email": "pas-une-adresse" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_email",
        ),
        (json!({}), StatusCode::UNPROCESSABLE_ENTITY, "invalid_email"),
        (
            json!({ "email": COURRIEL_MEMBRE }),
            StatusCode::CONFLICT,
            "already_member",
        ),
        (
            json!({ "email": COURRIEL_DEJA_INVITE }),
            StatusCode::CONFLICT,
            "already_invited",
        ),
        (
            json!({ "email": COURRIEL_QUI_NOUS_INVITE }),
            StatusCode::CONFLICT,
            "invitation_received",
        ),
    ] {
        let r = appel(&app, "POST", "/invitations", Some(corps.clone())).await;
        assert_eq!(r.statut, statut, "{corps}");
        assert_eq!(r.json(), json!({ "error": motif }), "{corps}");
        if corps == json!({}) {
            assert_eq!(
                faux.etat.lock().unwrap().dernier_corps,
                Some(json!({ "email": null })),
                "un corps sans `email` part avec `email: null` : le cloud juge"
            );
        }
    }
}

// 4. Accepter, refuser, retirer -------------------------------------------

#[tokio::test]
async fn accepter_refuser_retirer_passent_au_cloud() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    // `accept` rend le membre créé, tel quel.
    let r = appel(&app, "POST", "/invitations/21/accept", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({ "user_id": 102, "name": "Denis", "since": "2026-09-25T09:00:00Z" })
    );

    let r = appel(&app, "POST", "/invitations/22/decline", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));

    let r = appel(&app, "DELETE", "/invitations/11", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));

    let cercle = appel(&app, "GET", "/", None).await.json();
    assert_eq!(cercle["received"], json!([]));
    assert_eq!(cercle["sent"], json!([]));
    let noms: Vec<_> = cercle["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(noms, ["Alice", "Bruno", "Denis"]);
}

// 5. Révocation, sans cache -----------------------------------------------

fn ids_des_membres(v: &serde_json::Value) -> Vec<i64> {
    v["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["user_id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn la_revocation_par_tune_se_voit_a_la_lecture_suivante() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    assert_eq!(
        ids_des_membres(&appel(&app, "GET", "/", None).await.json()),
        [7, 9]
    );
    let r = appel(&app, "DELETE", "/members/9", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));
    assert_eq!(
        ids_des_membres(&appel(&app, "GET", "/", None).await.json()),
        [7]
    );
}

/// Le cas qui compte : la révocation faite par L'AUTRE membre, que Tune n'a
/// pas vue passer. Aucun cache ne doit la masquer — chaque lecture retourne
/// au cloud, qui porte le droit.
#[tokio::test]
async fn la_revocation_vue_par_le_cloud_est_vraie_immediatement() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let avant = appel(&app, "GET", "/", None).await.json();
    assert_eq!(ids_des_membres(&avant), [7, 9]);
    let appels_avant = faux.etat.lock().unwrap().appels;

    // Alice révoque le lien de son côté : seul le cloud le sait.
    faux.etat
        .lock()
        .unwrap()
        .members
        .retain(|m| m["user_id"] != 7);

    let apres = appel(&app, "GET", "/", None).await.json();
    assert_eq!(
        ids_des_membres(&apres),
        [9],
        "le membre révoqué côté cloud ne doit plus apparaître, sans délai"
    );
    assert_eq!(
        faux.etat.lock().unwrap().appels,
        appels_avant + 1,
        "chaque lecture doit repartir vers le cloud"
    );
}

// 6. 404 relayé ------------------------------------------------------------

#[tokio::test]
async fn ce_qui_n_est_pas_a_l_appelant_rend_le_404_du_cloud() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    for (m, chemin) in [
        ("POST", "/invitations/999/accept"),
        ("POST", "/invitations/999/decline"),
        ("DELETE", "/invitations/999"),
        // L'auteur ne peut pas accepter sa propre invitation : 404, pas 403.
        ("POST", "/invitations/11/accept"),
        ("DELETE", "/members/12345"),
        // Un identifiant qui tente de sortir de son segment reste UN segment :
        // le cloud reçoit `invitations/x%2F..%2F..%2Fmembers%2F7`, pas une
        // révocation d'Alice.
        ("DELETE", "/invitations/x%2F..%2F..%2Fmembers%2F7"),
    ] {
        let r = appel(&app, m, chemin, None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{m} {chemin}");
        assert_eq!(r.json(), json!({ "error": "not_found" }), "{m} {chemin}");
    }
    assert_eq!(ids_des_membres(&faux.etat.lock().unwrap().cercle()), [7, 9]);
}

// 7. Cloud en panne ---------------------------------------------------------

#[tokio::test]
async fn un_5xx_du_cloud_devient_un_etat_lisible() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().panne = true;
    let app = app(base(&faux.base, Some(JETON)));

    for (m, chemin) in [("GET", "/"), ("DELETE", "/members/7")] {
        let r = appel(&app, m, chemin, None).await;
        assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE, "{m} {chemin}");
        assert_eq!(
            r.json(),
            json!({ "connected": true, "code": "circle.cloud_unavailable", "upstream_status": 500 })
        );
    }
}

#[tokio::test]
async fn un_cloud_injoignable_devient_un_etat_lisible() {
    let app = app(base(&adresse_morte().await, Some(JETON)));
    let r = appel(&app, "GET", "/", None).await;
    assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        r.json(),
        json!({ "connected": true, "code": "circle.cloud_unavailable", "upstream_status": null })
    );
}

// Le jeton : celui de la session SSO, rafraîchi une fois ---------------------

#[tokio::test]
async fn un_jeton_perime_est_rafraichi_une_fois_dans_la_session_du_serveur() {
    let faux = demarrer().await;
    let backend = base(&faux.base, Some(JETON_PERIME));
    let app = app(backend.clone());

    let r = appel(&app, "GET", "/", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), cercle_initial());
    // La session du SERVEUR a tourné — pas une seconde session du greffon.
    let s = SettingsRepo::with_backend(backend);
    assert_eq!(
        s.get("mozaik_access_token").unwrap().as_deref(),
        Some(JETON_NEUF)
    );
    assert_eq!(
        s.get("mozaik_refresh_token").unwrap().as_deref(),
        Some(RAFRAICHISSEMENT_NEUF)
    );
}

/// Le cloud rend 401, puis le rafraîchissement unique échoue : le greffon
/// rend l'état « non connecté au cloud », JAMAIS 401. Côté client web, tout
/// 401 est la fin de la session TUNE (`fetchJSON` efface le jeton Tune) : une
/// session mozaiklabs expirée ne doit pas déconnecter l'utilisateur de Tune.
#[tokio::test]
async fn un_401_du_cloud_sans_rafraichissement_rend_non_connecte_jamais_401() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().rafraichissement_valide = "autre".into();
    let backend = base(&faux.base, Some(JETON_PERIME));
    let app = app(backend.clone());

    // `GET /` : l'état, comme sans session SSO.
    let r = appel(&app, "GET", "/", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "connected": false }));

    // Toute autre route, T1 comme avenant : 412 `circle.not_connected`.
    for (m, chemin, corps) in [
        (
            "POST",
            "/invitations",
            Some(json!({ "email": COURRIEL_INVITE })),
        ),
        ("POST", "/invitations/21/accept", None),
        ("POST", "/invitations/21/decline", None),
        ("DELETE", "/invitations/11", None),
        ("DELETE", "/members/7", None),
        ("POST", "/circles", Some(json!({ "name": "Voisins" }))),
        ("PATCH", "/circles/1", Some(json!({ "name": "Tribu" }))),
        ("DELETE", "/circles/1", None),
        ("PUT", "/circles/2/members/7", None),
        ("DELETE", "/circles/1/members/7", None),
    ] {
        let r = appel(&app, m, chemin, corps).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {chemin}");
        assert_eq!(
            r.json(),
            json!({ "connected": false, "code": "circle.not_connected" }),
            "{m} {chemin}"
        );
    }
    // Rien n'a été réécrit : la session reste ce que le serveur avait.
    assert_eq!(
        SettingsRepo::with_backend(backend.clone())
            .get("mozaik_access_token")
            .unwrap()
            .as_deref(),
        Some(JETON_PERIME)
    );

    // Sans jeton de rafraîchissement du tout, même état.
    SettingsRepo::with_backend(backend)
        .set("mozaik_refresh_token", "")
        .unwrap();
    let r = appel(&app, "DELETE", "/members/7", None).await;
    assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        r.json(),
        json!({ "connected": false, "code": "circle.not_connected" })
    );
    assert_eq!(ids_des_membres(&faux.etat.lock().unwrap().cercle()), [7, 9]);
}

// Le greffon lui-même -------------------------------------------------------

#[test]
fn le_greffon_s_appelle_circle_reste_opt_in_et_est_au_catalogue() {
    use tune_core::plugin_sdk::TunePlugin;
    let g = tune_circle::CirclePlugin::new(tune_circle::HostServices {
        backend: base("http://127.0.0.1:9", None),
    });
    assert_eq!(g.name(), "circle");
    assert!(!g.default_enabled(), "opt-in, comme cd");
    assert!(g.catalogued(), "au catalogue, comme cd (#5018)");
}
