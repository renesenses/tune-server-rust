//! #8185 (Rhorn, fil 2712) — « groupe créé » en vert, puis une liste vide.
//!
//! Le refus de création d'un groupe OAAT partait en `200 OK` avec un corps
//! `{"error": …}`. L'interface web ne lit pas ce champ : elle annonçait le
//! succès, rechargeait la liste et n'y trouvait rien. Ces témoins passent par
//! le vrai routeur, sans réseau extérieur ni son.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn routeur() -> axum::Router {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    super::router().with_state(state)
}

async fn envoyer(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(uri);
    let body = match corps {
        Some(c) => {
            req = req.header("Content-Type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

#[tokio::test]
async fn un_groupe_refuse_ne_repond_plus_200_8185() {
    let app = routeur();
    // Port 1 de la boucle locale : rien n'y écoute (voir `port_ferme`).
    let (statut, corps) = envoyer(
        &app,
        "POST",
        "/oaat-groups",
        Some(json!({
            "name": "Salon + Terrasse",
            "endpoints": [{"host": "127.0.0.1", "port": 1}],
        })),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::UNPROCESSABLE_ENTITY,
        "un groupe refusé doit répondre par un statut d'erreur, sinon \
         l'interface affiche « groupe créé » : {corps}"
    );
    assert_eq!(corps["code"], "oaat_endpoints_unreachable");
    assert_eq!(corps["unreachable_endpoints"], json!(["127.0.0.1:1"]));

    let (_, liste) = envoyer(&app, "GET", "/oaat-groups", None).await;
    assert_eq!(liste["oaat_groups"], json!([]), "rien n'a été enregistré");
}

#[tokio::test]
async fn un_groupe_sans_membre_est_refuse_par_un_400_8185() {
    let app = routeur();
    let (statut, corps) = envoyer(
        &app,
        "POST",
        "/oaat-groups",
        Some(json!({"name": "Vide", "endpoints": []})),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert!(corps["error"].is_string());
}

#[tokio::test]
async fn un_groupe_accepte_repond_200_et_apparait_dans_la_liste_8185() {
    // Contre-épreuve : un membre qui accepte la connexion crée le groupe, qui
    // est listé — le statut d'erreur ne touche que les refus.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            if l.accept().await.is_err() {
                break;
            }
        }
    });
    let app = routeur();
    let (statut, corps) = envoyer(
        &app,
        "POST",
        "/oaat-groups",
        Some(json!({
            "name": "Salon + Terrasse",
            "endpoints": [{"host": "127.0.0.1", "port": port}],
        })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let id = corps["id"]
        .as_str()
        .expect("identifiant du groupe")
        .to_string();

    let (_, liste) = envoyer(&app, "GET", "/oaat-groups", None).await;
    let groupes = liste["oaat_groups"].as_array().expect("liste");
    assert_eq!(groupes.len(), 1, "{liste}");
    assert_eq!(groupes[0]["id"], id.as_str());
    assert_eq!(groupes[0]["name"], "Salon + Terrasse");
}
