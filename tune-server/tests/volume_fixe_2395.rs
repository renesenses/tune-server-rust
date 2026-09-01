//! #2395 — Volume fixe (bit-perfect) : un saut, annoncé et réversible.
//!
//! Marco Polo (fil forum 1546) écoute sur un **Denon RC12**, qui est à la fois
//! un renderer DLNA et un amplificateur. Cocher « Volume fixe » a porté son
//! installation à 100 % ; il demande si ses haut-parleurs ont souffert.
//!
//! Ce que ces essais tiennent, du côté de la ROUTE :
//!
//! 1. l'armement commande le plein volume **une** fois, et seulement sur une
//!    vraie transition ;
//! 2. quitter le mode **rend le volume d'origine** — la pièce qui manquait ;
//! 3. hors transition, la route ne commande **rien** ;
//! 4. **aucun type de sortie n'est dispensé** de l'accord explicite — `local`
//!    et `browser` l'étaient, ils ne le sont plus.
//!
//! Les mesures portent sur ce que l'APPAREIL a reçu (`MockOutput::volume_calls`),
//! jamais sur un code HTTP seul : un `200` ne dit pas si une commande est
//! partie, et une consigne à la valeur déjà en place ne laisse aucune trace
//! dans l'état.
//!
//! Le pendant côté lecture — « trois lectures successives ne commandent rien »
//! — vit dans `tune-core/src/orchestrator.rs`, là où la réassertion vivait.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn envoyer(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(chemin)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (status, json)
}

/// Zone réglée à 30 %, du type demandé, avec sa sortie factice enregistrée.
///
/// 30 % et non le défaut : un niveau d'écoute ordinaire, franchement distinct
/// du plein volume. Une restauration qui rendrait « à peu près » se verrait.
///
/// Ce réglage initial est lui-même une commande : chaque essai part donc d'un
/// journal à `[0.3]`, et non vide. C'est voulu — il prouve au passage que le
/// banc voit bien partir les commandes qu'il doit voir.
async fn zone_a_30(
    app: &axum::Router,
    state: &tune_server::state::AppState,
    device_id: &str,
    output_type: &str,
) -> i64 {
    let (status, body) = envoyer(
        app,
        "POST",
        "/api/v1/zones",
        json!({"name": "Denon RC12", "output_type": output_type, "output_device_id": device_id}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "creation de zone : {body}");
    let zone_id = body["id"].as_i64().unwrap();

    state.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(device_id, "Denon RC12").with_type(output_type),
    ));

    let (status, body) = envoyer(
        app,
        "PATCH",
        &format!("/api/v1/zones/{zone_id}"),
        json!({"volume": 30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reglage a 30 % : {body}");

    zone_id
}

/// Même chose, mais la zone est posée directement en base.
///
/// `POST /api/v1/zones` refuse en 404 une zone `local` dont le périphérique
/// n'existe pas sur la machine (« Local audio device not found ») — une carte
/// son factice ne se déclare pas. Or ce qui est éprouvé ici n'est pas la
/// création : c'est la garde du PATCH. On crée donc la zone par le dépôt, comme
/// le fait la route elle-même, et on garde la route pour ce qui compte.
async fn zone_a_30_posee_en_base(
    app: &axum::Router,
    state: &tune_server::state::AppState,
    device_id: &str,
    output_type: &str,
) -> i64 {
    let zone_id = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Poste de travail", Some(output_type), Some(device_id))
        .expect("creation de zone en base");

    state.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(device_id, "Poste de travail")
            .with_type(output_type),
    ));

    let (status, body) = envoyer(
        app,
        "PATCH",
        &format!("/api/v1/zones/{zone_id}"),
        json!({"volume": 30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reglage a 30 % : {body}");

    zone_id
}

/// Les commandes de volume que l'APPAREIL a reçues, dans l'ordre.
async fn volumes_recus(state: &tune_server::state::AppState, device_id: &str) -> Vec<f64> {
    let outputs = state.outputs.lock().await;
    let sortie = outputs.get(device_id).expect("sortie enregistree");
    let guard = sortie.lock().await;
    guard
        .as_any()
        .downcast_ref::<tune_core::outputs::mock::MockOutput>()
        .expect("la sortie factice")
        .volume_calls()
        .await
}

/// Le tour complet : armer, puis quitter — et retrouver son volume.
///
/// C'est la contre-épreuve de la restauration. Avant ce correctif, aucun
/// `previous_volume` n'existait nulle part dans le dépôt : décocher la case
/// laissait la zone à 100 %, et l'utilisateur devait retrouver son niveau à
/// l'oreille, en partant du plein volume sur un appareil qui porte son ampli.
#[tokio::test]
async fn quitter_le_mode_rend_le_volume_d_origine() {
    let (app, state) = app();
    let device_id = "dlna-denon-aller-retour";
    let zone_id = zone_a_30(&app, &state, device_id, "dlna").await;

    let apres_reglage = volumes_recus(&state, device_id).await;
    assert_eq!(
        apres_reglage,
        vec![0.3],
        "le reglage a 30 % est bien parti vers l'appareil"
    );

    // Armement, avec l'accord explicite — exigé sur toute sortie (#2395).
    let (status, body) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{zone_id}"),
        json!({"fixed_volume": true, "confirm_full_volume": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "armement : {body}");
    assert_eq!(
        volumes_recus(&state, device_id).await,
        vec![0.3, 1.0],
        "l'armement commande le plein volume, UNE fois"
    );

    // Sortie du mode.
    let (status, body) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{zone_id}"),
        json!({"fixed_volume": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "desarmement : {body}");
    assert_eq!(
        volumes_recus(&state, device_id).await,
        vec![0.3, 1.0, 0.3],
        "quitter le mode doit REAPPLIQUER le volume d'origine a l'appareil"
    );

    // Et la base dit la même chose que l'appareil : le curseur de l'interface
    // ne doit pas rester bloqué à 100 % après la restauration.
    let (status, zone) = envoyer(
        &app,
        "GET",
        &format!("/api/v1/zones/{zone_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(zone["fixed_volume"], json!(false));
    // La route expose le volume en linéaire 0..1 (`zone.volume / 100.0`), là où
    // la colonne le garde en pour-cent : 30 % en base se lit donc `0.3` ici,
    // la même valeur que celle reçue par l'appareil.
    assert_eq!(
        zone["volume"].as_f64().unwrap(),
        0.3,
        "le volume persiste doit suivre l'appareil"
    );
}

/// #2395 — `local` et `browser` ne sont plus dispensés de l'accord.
///
/// Ces deux types passaient sans confirmation. La garde protège le niveau qui
/// sort des haut-parleurs, pas l'identité de ce qu'on commande : qui écoutait à
/// 30 % a compensé au gain de son ampli, et le saut à pleine échelle lui rend
/// une quinzaine de décibels — que l'atténuation vive dans un renderer, dans la
/// chaîne locale, ou dans le client web d'une zone `browser`, souvent un casque
/// sur un portable.
///
/// Mesuré des deux côtés, et sur ce que l'appareil a **reçu** : refus sans
/// accord et rien qui parte, puis armement avec accord et le plein volume qui
/// part une fois.
///
/// Rouge avant ce changement : les deux types s'armaient en `200` sans accord.
#[tokio::test]
async fn local_et_browser_exigent_l_accord_comme_les_autres() {
    for output_type in ["local", "browser"] {
        let (app, state) = app();
        let device_id = &format!("{output_type}:poste-de-travail");
        let zone_id = zone_a_30_posee_en_base(&app, &state, device_id, output_type).await;

        // Sans accord : refus, et l'appareil ne reçoit rien.
        let (status, body) = envoyer(
            &app,
            "PATCH",
            &format!("/api/v1/zones/{zone_id}"),
            json!({"fixed_volume": true}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{output_type} : l'armement sans accord doit etre refuse — {body}"
        );
        assert_eq!(
            body["error"], "full_volume_confirmation_required",
            "{output_type} : le motif du refus doit etre nommable par le client"
        );
        assert_eq!(
            volumes_recus(&state, device_id).await,
            vec![0.3],
            "{output_type} : un refus ne laisse AUCUNE commande partir"
        );

        // Le refus précède toute écriture : la zone n'est pas armée en base.
        let (_, zone) = envoyer(
            &app,
            "GET",
            &format!("/api/v1/zones/{zone_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(
            zone["fixed_volume"],
            json!(false),
            "{output_type} : un refus ne doit rien avoir ecrit"
        );

        // Avec accord : le saut a lieu, une fois.
        let (status, body) = envoyer(
            &app,
            "PATCH",
            &format!("/api/v1/zones/{zone_id}"),
            json!({"fixed_volume": true, "confirm_full_volume": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{output_type} : armement — {body}");
        assert_eq!(
            volumes_recus(&state, device_id).await,
            vec![0.3, 1.0],
            "{output_type} : l'accord donne, le plein volume part UNE fois"
        );
    }
}

/// Sans accord, rien ne part — et rien n'est écrit.
///
/// Le refus 409 était déjà là (#2477). Ce qui est mesuré ici et ne l'était
/// pas : que l'appareil n'a **rien reçu**. Un 409 seul ne le dit pas.
#[tokio::test]
async fn sans_accord_l_appareil_ne_recoit_rien() {
    let (app, state) = app();
    let device_id = "dlna-denon-refus";
    let zone_id = zone_a_30(&app, &state, device_id, "dlna").await;

    let (status, _) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{zone_id}"),
        json!({"fixed_volume": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    assert_eq!(
        volumes_recus(&state, device_id).await,
        vec![0.3],
        "un refus ne doit laisser AUCUNE commande de volume partir"
    );
}

/// Hors transition, la route ne commande jamais le volume.
///
/// Cet essai n'est PAS un témoin : il était rouge avant le correctif, parce
/// qu'à l'époque l'armement ne commandait rien du tout à l'appareil. Il tient
/// l'autre bord de la garde de transition — que le saut a bien lieu une fois,
/// et une seule, quoi que le client republie ensuite.
///
/// Le cas est réel : un client qui renvoie l'état complet de la zone à chaque
/// enregistrement d'écran repostera `fixed_volume: true` indéfiniment. Si ces
/// PATCH commandaient, on aurait seulement déplacé la réassertion de la
/// lecture vers le réglage.
#[tokio::test]
async fn temoin_un_patch_qui_ne_change_rien_ne_commande_rien() {
    let (app, state) = app();
    let device_id = "dlna-denon-temoin";
    let zone_id = zone_a_30(&app, &state, device_id, "dlna").await;

    envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{zone_id}"),
        json!({"fixed_volume": true, "confirm_full_volume": true}),
    )
    .await;
    let apres_armement = volumes_recus(&state, device_id).await;
    assert_eq!(apres_armement, vec![0.3, 1.0]);

    // Trois PATCH qui réaffirment l'état courant, plus un qui ne parle pas de
    // volume du tout.
    for corps in [
        json!({"fixed_volume": true, "confirm_full_volume": true}),
        json!({"fixed_volume": true}),
        json!({"name": "Salon"}),
    ] {
        let (status, body) = envoyer(
            &app,
            "PATCH",
            &format!("/api/v1/zones/{zone_id}"),
            corps.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{corps} : {body}");
    }

    assert_eq!(
        volumes_recus(&state, device_id).await,
        apres_armement,
        "reaffirmer le mode ne doit RIEN commander : le saut a deja eu lieu"
    );
}

/// Décocher une zone qui n'a jamais été armée par ce chemin ne devine rien.
///
/// Une zone armée par une version antérieure à ce correctif n'a pas de
/// mémoire. Le serveur la laisse alors où elle est plutôt que de commander un
/// niveau inventé — commander une valeur devinée serait le défaut qu'on
/// corrige, à l'envers.
#[tokio::test]
async fn sans_memoire_le_desarmement_ne_commande_aucun_volume() {
    let (app, state) = app();
    let device_id = "dlna-denon-sans-memoire";
    let zone_id = zone_a_30(&app, &state, device_id, "dlna").await;

    // Armement « à l'ancienne » : directement en base, sans passer par la
    // route, donc sans mémoire écrite.
    tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .update_fixed_volume(zone_id, true)
        .unwrap();

    let (status, body) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{zone_id}"),
        json!({"fixed_volume": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "desarmement : {body}");

    assert_eq!(
        volumes_recus(&state, device_id).await,
        vec![0.3],
        "sans volume memorise, le desarmement ne commande rien"
    );
}
