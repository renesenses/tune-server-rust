//! Avenant « PLUSIEURS cercles par utilisateur » (#5018, 25/09) : le relais
//! des routes `/circles…`, de `circle_id` sur `POST /invitations` et de la clé
//! `circles` de `GET /`, contre le faux mozaiklabs qui implémente l'avenant.

mod commun;

use axum::http::StatusCode;
use serde_json::json;

use commun::*;

#[tokio::test]
async fn sans_session_les_routes_des_cercles_rendent_412_et_rien_ne_part() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, None));
    for (m, chemin, corps) in [
        ("POST", "/circles", Some(json!({ "name": "Voisins" }))),
        ("PATCH", "/circles/1", Some(json!({ "name": "Tribu" }))),
        ("DELETE", "/circles/1", None),
        ("PUT", "/circles/2/members/7", None),
        ("DELETE", "/circles/1/members/7", None),
        (
            "POST",
            "/invitations",
            Some(json!({ "email": COURRIEL_INVITE, "circle_id": 1 })),
        ),
    ] {
        let r = appel(&app, m, chemin, corps).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {chemin}");
        assert_eq!(
            r.json(),
            json!({ "connected": false, "code": "circle.not_connected" }),
            "{m} {chemin}"
        );
    }
    assert_eq!(faux.etat.lock().unwrap().appels, 0);
}

#[tokio::test]
async fn la_cle_circles_de_la_liste_est_relayee_telle_quelle() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));
    let r = appel(&app, "GET", "/", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        r.json()["circles"],
        json!([
            { "id": 1, "name": "Famille", "member_ids": [7, 9] },
            { "id": 2, "name": "Jazz", "member_ids": [9] }
        ])
    );
    let attendu = serde_json::to_vec(&faux.etat.lock().unwrap().cercle()).unwrap();
    assert_eq!(r.octets, attendu, "à l'octet près");
}

#[tokio::test]
async fn creer_un_cercle_relaie_201_409_et_422() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(&app, "POST", "/circles", Some(json!({ "name": "Voisins" }))).await;
    assert_eq!(r.statut, StatusCode::CREATED);
    assert_eq!(
        r.json(),
        json!({ "id": 3, "name": "Voisins", "member_ids": [] })
    );

    // Unique par propriétaire, casse ignorée.
    let r = appel(&app, "POST", "/circles", Some(json!({ "name": "fAmIlLe" }))).await;
    assert_eq!(r.statut, StatusCode::CONFLICT);
    assert_eq!(r.json(), json!({ "error": "circle_name_taken" }));

    // Le nom est jugé par le cloud seul : vide, trop long, absent.
    for corps in [
        json!({ "name": "" }),
        json!({ "name": "x".repeat(61) }),
        json!({}),
    ] {
        let r = appel(&app, "POST", "/circles", Some(corps.clone())).await;
        assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
        assert_eq!(
            r.json(),
            corps_de_validation("name", MESSAGE_NOM),
            "{corps}"
        );
    }
    assert_eq!(faux.etat.lock().unwrap().circles.len(), 3);
}

#[tokio::test]
async fn au_dela_de_cinquante_cercles_le_422_du_cloud_est_relaye() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));
    for k in 3..=CERCLES_MAX {
        let r = appel(
            &app,
            "POST",
            "/circles",
            Some(json!({ "name": format!("Cercle {k}") })),
        )
        .await;
        assert_eq!(r.statut, StatusCode::CREATED, "cercle {k}");
    }
    let r = appel(&app, "POST", "/circles", Some(json!({ "name": "De trop" }))).await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json(), json!({ "error": "too_many_circles" }));
    assert_eq!(faux.etat.lock().unwrap().circles.len(), CERCLES_MAX);
}

#[tokio::test]
async fn renommer_relaie_le_cercle_le_409_et_le_404() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(
        &app,
        "PATCH",
        "/circles/2",
        Some(json!({ "name": "Jazz & blues" })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({ "id": 2, "name": "Jazz & blues", "member_ids": [9] })
    );

    let r = appel(
        &app,
        "PATCH",
        "/circles/2",
        Some(json!({ "name": "FAMILLE" })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::CONFLICT);
    assert_eq!(r.json(), json!({ "error": "circle_name_taken" }));

    let chemin = format!("/circles/{CERCLE_D_UN_AUTRE}");
    let r = appel(&app, "PATCH", &chemin, Some(json!({ "name": "À moi" }))).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "error": "not_found" }));
}

#[tokio::test]
async fn ranger_un_contact_est_idempotent_et_un_non_contact_rend_404() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    for _ in 0..2 {
        let r = appel(&app, "PUT", "/circles/2/members/7", None).await;
        assert_eq!(r.statut, StatusCode::OK);
        assert_eq!(
            r.json(),
            json!({ "id": 2, "name": "Jazz", "member_ids": [9, 7] })
        );
    }
    assert_eq!(
        faux.etat.lock().unwrap().membres_du_cercle(2),
        Some(vec![9, 7])
    );

    let autre = format!("/circles/{CERCLE_D_UN_AUTRE}/members/7");
    for (m, chemin) in [
        // Un non-contact : 404, comme tout ce qui n'est pas à l'appelant.
        ("PUT", "/circles/2/members/12345"),
        ("PUT", autre.as_str()),
        ("DELETE", autre.as_str()),
        ("DELETE", "/circles/999"),
        // Un identifiant qui tente de sortir de son segment reste UN segment.
        ("PUT", "/circles/2%2F..%2F1/members/7"),
    ] {
        let r = appel(&app, m, chemin, None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{m} {chemin}");
        assert_eq!(r.json(), json!({ "error": "not_found" }), "{m} {chemin}");
    }
    assert_eq!(
        faux.etat.lock().unwrap().membres_du_cercle(1),
        Some(vec![7, 9])
    );
}

#[tokio::test]
async fn retirer_d_un_cercle_ne_touche_que_ce_cercle() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(&app, "DELETE", "/circles/1/members/9", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));

    // Alice est un contact, mais elle n'est pas rangée dans Jazz : 404 relayé.
    let r = appel(&app, "DELETE", "/circles/2/members/7", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "error": "not_found" }));

    let f = faux.etat.lock().unwrap();
    assert_eq!(f.membres_du_cercle(1), Some(vec![7]));
    assert_eq!(f.membres_du_cercle(2), Some(vec![9]), "Jazz garde Bruno");
    assert_eq!(f.members.len(), 2, "Bruno reste un contact");
}

#[tokio::test]
async fn supprimer_un_cercle_ne_revoque_personne() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(&app, "DELETE", "/circles/1", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));

    let cercle = appel(&app, "GET", "/", None).await.json();
    assert_eq!(
        cercle["circles"],
        json!([{ "id": 2, "name": "Jazz", "member_ids": [9] }])
    );
    assert_eq!(cercle["members"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn revoquer_un_contact_le_retire_de_tous_les_cercles() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(&app, "DELETE", "/members/9", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    let cercle = appel(&app, "GET", "/", None).await.json();
    assert_eq!(
        cercle["circles"],
        json!([
            { "id": 1, "name": "Famille", "member_ids": [7] },
            { "id": 2, "name": "Jazz", "member_ids": [] }
        ])
    );
}

#[tokio::test]
async fn circle_id_part_tel_quel_et_seulement_s_il_est_donne() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": COURRIEL_INVITE, "circle_id": 2, "admin": true })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::CREATED);
    assert_eq!(
        faux.etat.lock().unwrap().dernier_corps,
        Some(json!({ "email": COURRIEL_INVITE, "circle_id": 2 })),
        "circle_id transmis tel quel, le champ de trop non"
    );
    let invitation = r.json()["id"].as_i64().unwrap();

    // À l'acceptation (côté invité), le cloud le range dans ce cercle.
    let uid = faux
        .etat
        .lock()
        .unwrap()
        .acceptee_par_l_invite(invitation)
        .unwrap();
    let cercle = appel(&app, "GET", "/", None).await.json();
    assert_eq!(cercle["circles"][1]["member_ids"], json!([9, uid]));

    // Sans `circle_id`, la clé ne part pas du tout.
    appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": "sans.cercle@exemple.fr" })),
    )
    .await;
    assert_eq!(
        faux.etat.lock().unwrap().dernier_corps,
        Some(json!({ "email": "sans.cercle@exemple.fr" }))
    );

    // Un identifiant non entier part aussi tel quel : le cloud juge (422 de
    // validation), et son refus revient avec son corps.
    let r = appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": "chaine@exemple.fr", "circle_id": "1" })),
    )
    .await;
    assert_eq!(
        faux.etat.lock().unwrap().dernier_corps,
        Some(json!({ "email": "chaine@exemple.fr", "circle_id": "1" }))
    );
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json(),
        corps_de_validation("circle_id", MESSAGE_CIRCLE_ID)
    );

    // Le cercle d'un autre : 404 relayé, sans invitation.
    let envoyees = faux.etat.lock().unwrap().sent.len();
    let r = appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": "autre.cercle@exemple.fr", "circle_id": CERCLE_D_UN_AUTRE })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "error": "not_found" }));
    assert_eq!(faux.etat.lock().unwrap().sent.len(), envoyees);
}

/// site-mozaiklabs#224 : sans aucun cercle, le cloud garde la forme exacte de
/// T1, SANS la clé `circles`. Le relais n'en ajoute pas : il transmet tel quel.
#[tokio::test]
async fn sans_cercle_la_liste_reste_la_forme_de_t1_telle_quelle() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().circles.clear();
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(&app, "GET", "/", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    let v = r.json();
    assert!(v.get("circles").is_none(), "{v}");
    let mut t1 = cercle_initial();
    t1.as_object_mut().unwrap().remove("circles");
    assert_eq!(v, t1);
    let attendu = serde_json::to_vec(&faux.etat.lock().unwrap().cercle()).unwrap();
    assert_eq!(r.octets, attendu, "à l'octet près");

    // Le premier cercle créé fait apparaître la clé.
    appel(&app, "POST", "/circles", Some(json!({ "name": "Voisins" }))).await;
    let v = appel(&app, "GET", "/", None).await.json();
    assert_eq!(
        v["circles"],
        json!([{ "id": 3, "name": "Voisins", "member_ids": [] }])
    );
}

#[tokio::test]
async fn un_cercle_supprime_avant_l_acceptation_donne_le_lien_sans_rangement() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON)));

    let r = appel(
        &app,
        "POST",
        "/invitations",
        Some(json!({ "email": COURRIEL_INVITE, "circle_id": 1 })),
    )
    .await;
    let invitation = r.json()["id"].as_i64().unwrap();
    appel(&app, "DELETE", "/circles/1", None).await;

    let uid = faux
        .etat
        .lock()
        .unwrap()
        .acceptee_par_l_invite(invitation)
        .unwrap();
    let cercle = appel(&app, "GET", "/", None).await.json();
    assert!(
        cercle["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["user_id"] == uid)
    );
    assert_eq!(
        cercle["circles"],
        json!([{ "id": 2, "name": "Jazz", "member_ids": [9] }])
    );
}

#[tokio::test]
async fn un_5xx_du_cloud_sur_les_cercles_devient_503() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().panne = true;
    let app = app(base(&faux.base, Some(JETON)));
    for (m, chemin, corps) in [
        ("POST", "/circles", Some(json!({ "name": "Voisins" }))),
        ("PATCH", "/circles/1", Some(json!({ "name": "Tribu" }))),
        ("DELETE", "/circles/1", None),
        ("PUT", "/circles/2/members/7", None),
        ("DELETE", "/circles/1/members/7", None),
    ] {
        let r = appel(&app, m, chemin, corps).await;
        assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE, "{m} {chemin}");
        assert_eq!(
            r.json(),
            json!({ "connected": true, "code": "circle.cloud_unavailable", "upstream_status": 500 })
        );
    }
}

#[tokio::test]
async fn un_jeton_perime_est_rafraichi_une_fois_sur_les_cercles() {
    let faux = demarrer().await;
    let app = app(base(&faux.base, Some(JETON_PERIME)));
    let r = appel(&app, "PUT", "/circles/2/members/7", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()["member_ids"], json!([9, 7]));
}
