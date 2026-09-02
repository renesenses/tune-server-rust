//! `POST /api/v1/eq/import/autoeq` est MONTÉE, et elle répond — #1405.
//!
//! Le défaut que ce fichier garde n'est pas dans l'analyseur : celui-là est
//! éprouvé sur trois profils publiés dans
//! `tune-core/tests/autoeq_profils_reels.rs`. Le défaut est plus bête et plus
//! fréquent — treize occurrences recensées dans ce dépôt : une route écrite,
//! testée par ses fonctions, et **jamais branchée** au routeur. Elle rend alors
//! `api_not_found` à tous ses clients pendant que ses tests restent verts.
//!
//! Ces tests passent donc par `tune_server::routes::router(state)` — le routeur
//! réel, avec son préfixe `/api/v1` et son repli 404 — et par le chemin exact
//! qu'un client tapera. Retirer le `.route("/import/autoeq", …)` de
//! `eq_pro::router()`, ou déplacer le `.nest("/eq", …)`, fait rougir ce
//! fichier.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Le HD 650 d'oratory1990, tel que publié par AutoEq.
///
/// La fixture vit dans `tune-core` — c'est le crate qui l'analyse — et elle est
/// lue ici depuis sa source unique plutôt que recopiée : deux exemplaires
/// finiraient par diverger, et le test du serveur prouverait alors autre chose
/// que le test du cœur. Les deux crates sont toujours voisins dans l'espace de
/// travail.
const HD_650: &str = include_str!("../../tune-core/tests/fixtures/autoeq/sennheiser_hd_650.txt");

const CHEMIN: &str = "/api/v1/eq/import/autoeq";

/// Un serveur en mémoire, **Premium** — la route est derrière la même porte que
/// `POST /eq/presets`.
///
/// Sans ce droit la route répond 402 : c'est déjà la preuve qu'elle est montée,
/// mais pas qu'elle fonctionne. On veut le 201.
async fn app_premium() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    tune_server::routes::router(state)
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

// --- Le défaut gardé : la route existe-t-elle au bout du chemin ? ---

/// Un vrai profil AutoEq, posté sur le chemin monté, rend 201 et un préréglage.
///
/// Un 404 ici = la route n'est pas branchée. C'est LE test de ce fichier.
#[tokio::test]
async fn la_route_dimport_autoeq_est_montee_et_rend_201() {
    let app = app_premium().await;
    let (status, corps) = post(&app, CHEMIN, json!({ "text": HD_650, "name": "HD 650" })).await;

    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "route non montée sur {CHEMIN} : {corps}"
    );
    assert_eq!(status, StatusCode::CREATED, "corps : {corps}");

    assert_eq!(corps["band_count"], 10);
    assert_eq!(corps["preset"]["name"], "HD 650");
    assert_eq!(corps["preset"]["source"], "autoeq");
    assert_eq!(corps["preset"]["bands"].as_array().unwrap().len(), 10);
    // La première bande du fichier, valeur par valeur.
    let premiere = &corps["preset"]["bands"][0];
    assert_eq!(premiere["type"], "low_shelf");
    assert_eq!(premiere["freq"], 105.0);
    assert_eq!(premiere["gain"], 6.4);
}

/// Le préréglage importé est un préréglage comme les autres : il se relit.
///
/// Un import qui écrirait ailleurs que dans `eq_presets` produirait un 201
/// vide de conséquence — le préréglage n'apparaîtrait dans aucune liste et
/// aucun client ne pourrait l'activer.
#[tokio::test]
async fn le_prereglage_importe_apparait_dans_la_liste_des_prereglages() {
    let app = app_premium().await;
    let (_, cree) = post(&app, CHEMIN, json!({ "text": HD_650, "name": "HD 650" })).await;
    let id = cree["preset"]["id"].as_str().unwrap().to_string();

    let (status, liste) = get(&app, "/api/v1/eq/presets").await;
    assert_eq!(status, StatusCode::OK);
    let presets = liste["presets"].as_array().unwrap();
    assert!(
        presets.iter().any(|p| p["id"].as_str() == Some(&id)),
        "le préréglage importé doit être listé : {liste}"
    );

    // Et il se relit un par un, par le chemin que le client web utilise.
    let (status, relu) = get(&app, &format!("/api/v1/eq/presets/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(relu["bands"].as_array().unwrap().len(), 10);
}

// --- Le Preamp, dit et non appliqué deux fois ---

/// La réponse chiffre la marge, dit qu'elle n'ajoute pas le `Preamp`, et
/// affirme que ce choix couvre bien ce que le fichier demandait.
#[tokio::test]
async fn la_reponse_dit_le_preamp_lu_la_marge_reservee_et_quelle_le_couvre() {
    let app = app_premium().await;
    let (status, corps) = post(&app, CHEMIN, json!({ "text": HD_650 })).await;
    assert_eq!(status, StatusCode::CREATED);

    assert_eq!(corps["preamp_db"], -6.1);
    assert_eq!(corps["preamp_applied"], false);
    assert_eq!(corps["reserved_headroom_db"], -13.8);
    assert_eq!(corps["preamp_covered_by_headroom"], true);
    // Rien à signaler : la marge de Tune est la plus protectrice des deux.
    assert!(corps.get("warning").is_none(), "corps : {corps}");
}

/// Un `Preamp` que la somme des gains ne justifie pas ne passe PAS en silence.
///
/// Le cas ne se produit pas sur un export AutoEq — le maximum d'une réponse
/// combinée ne dépasse jamais la somme de ses gains positifs — mais un fichier
/// écrit à la main peut le fabriquer. La marge de Tune est alors moins
/// protectrice que celle du fichier, et la réponse le DIT.
#[tokio::test]
async fn un_preamp_que_la_marge_ne_couvre_pas_produit_un_avertissement() {
    let app = app_premium().await;
    let bricole = "Preamp: -18.0 dB\nFilter 1: ON PK Fc 1000 Hz Gain 1.0 dB Q 1\n";
    let (status, corps) = post(&app, CHEMIN, json!({ "text": bricole })).await;
    assert_eq!(status, StatusCode::CREATED, "corps : {corps}");
    assert_eq!(corps["preamp_covered_by_headroom"], false);
    assert!(
        corps["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("-18"),
        "l'avertissement doit chiffrer le Preamp : {corps}"
    );
}

// --- Les filtres OFF : écartés, mais comptés ---

/// Sept bandes pour dix lignes : la réponse explique les trois manquantes.
#[tokio::test]
async fn les_filtres_off_sont_ecartes_mais_comptes_dans_le_compte_rendu() {
    let app = app_premium().await;
    // Le vrai HD 650, dont on désactive trois filtres comme le ferait
    // Equalizer APO.
    let avec_off = HD_650
        .replace("Filter 8: ON", "Filter 8: OFF")
        .replace("Filter 9: ON", "Filter 9: OFF")
        .replace("Filter 10: ON", "Filter 10: OFF");
    let (status, corps) = post(&app, CHEMIN, json!({ "text": avec_off })).await;
    assert_eq!(status, StatusCode::CREATED, "corps : {corps}");
    assert_eq!(corps["band_count"], 7);
    assert_eq!(corps["ignored_filter_count"], 3);
}

// --- Ce qui doit être refusé, en nommant la ligne ---

/// Un fichier malformé rend 400 et NOMME la ligne fautive.
///
/// La ligne 3 du HD 650 porte `Fc 8800 Hz` ; on la rend illisible. Un message
/// qui ne dit pas où chercher laisserait l'utilisateur relire trente lignes.
#[tokio::test]
async fn un_fichier_malforme_rend_400_en_nommant_la_ligne_fautive() {
    let app = app_premium().await;
    let mutile = HD_650.replace("Fc 8800 Hz", "Fc abc Hz");
    let (status, corps) = post(&app, CHEMIN, json!({ "text": mutile })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "corps : {corps}");
    let message = corps.to_string();
    assert!(message.contains("ligne 3"), "message : {message}");

    // Et RIEN n'a été enregistré : pas de préréglage à moitié construit.
    let (_, liste) = get(&app, "/api/v1/eq/presets").await;
    assert!(
        liste["presets"].as_array().unwrap().is_empty(),
        "un import refusé ne doit rien laisser : {liste}"
    );
}

/// Un type de filtre que l'égaliseur ne sait pas construire est refusé, pas
/// approché en silence par un `peak`.
#[tokio::test]
async fn un_type_de_filtre_inconnu_est_refuse_et_nomme() {
    let app = app_premium().await;
    let mutile = HD_650.replace("ON PK Fc 8800", "ON BP Fc 8800");
    let (status, corps) = post(&app, CHEMIN, json!({ "text": mutile })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "corps : {corps}");
    let message = corps.to_string();
    assert!(message.contains("ligne 3"), "message : {message}");
    assert!(message.contains("BP"), "message : {message}");
}

/// Au-delà de la borne annoncée par `GET /eq/status`, l'import refuse — il ne
/// tronque pas.
#[tokio::test]
async fn un_profil_trop_long_est_refuse_et_non_tronque() {
    let app = app_premium().await;
    // 32 bandes : une de plus que le `max_bands` annoncé.
    let (status, statut) = get(&app, "/api/v1/eq/status").await;
    assert_eq!(status, StatusCode::OK);
    let max = statut["max_bands"].as_u64().unwrap() as usize;

    let mut texte = String::from("Preamp: -3.0 dB\n");
    for n in 1..=(max + 1) {
        texte.push_str(&format!(
            "Filter {n}: ON PK Fc {} Hz Gain -0.5 dB Q 1\n",
            100 + n * 10
        ));
    }
    let (status, corps) = post(&app, CHEMIN, json!({ "text": texte })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "corps : {corps}");
    let message = corps.to_string();
    assert!(message.contains(&max.to_string()), "message : {message}");

    // Aucun préréglage tronqué n'a été enregistré.
    let (_, liste) = get(&app, "/api/v1/eq/presets").await;
    assert!(
        liste["presets"].as_array().unwrap().is_empty(),
        "rien ne doit être enregistré : {liste}"
    );
}

/// Sans Premium, la route répond 402 — et surtout PAS 404.
///
/// Ce test vaut aussi comme second témoin du montage : il atteint le chemin
/// sans licence, et prouve que c'est bien la porte payante qui répond, pas le
/// repli `api_not_found`.
#[tokio::test]
async fn sans_premium_la_route_repond_402_et_non_404() {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state);
    let (status, corps) = post(&app, CHEMIN, json!({ "text": HD_650 })).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "corps : {corps}");
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "corps : {corps}");
    assert_eq!(corps["error"], "premium_required");
}
