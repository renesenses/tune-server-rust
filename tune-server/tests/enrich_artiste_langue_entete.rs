//! `GET /metadata/artists/{id}/enrich` lit bien `Accept-Language` (#1849).
//!
//! ## Le trou
//!
//! #2874 a réparé les DEUX routes « bio » de `library/` — elles ignoraient
//! l'en-tête et repliaient sur `"fr"`. Une troisième route de la même famille
//! est restée nue : la poignée d'enrichissement d'artiste, celle du bouton
//! « enrichir la biographie » de la vue Bibliothèque
//! (`tune-web-client/src/components/LibraryView.svelte`, `api.enrichArtist`).
//!
//! Elle n'extrayait AUCUN en-tête. Elle demandait `("lang", "fr")` à Last.fm,
//! estampillait `bio_lang: "fr"` en dur, et interrogeait `en.wikipedia` AVANT
//! `fr.wikipedia`. Les deux sens du défaut en découlaient : interface
//! française et Last.fm muette → notice **anglaise** retenue ; interface
//! anglaise → Last.fm répondait en **français**.
//!
//! ## Ce qui est pincé ici
//!
//! La poignée résout la langue UNE fois, en tête, par
//! `langue_et_encyclopedies(&headers)`, et s'en sert pour tout : la requête
//! Last.fm, l'estampille `bio_lang`, l'ordre des Wikipédia et le message
//! d'absence de clé. Le choix des sources est pincé au plus près, en test
//! unitaire (`routes::metadata::langue_des_bios`).
//!
//! Ce fichier pince l'autre maillon, celui que seul un vrai routeur peut
//! montrer : **l'en-tête de la requête arrive jusqu'à cette résolution**. Sur
//! l'ancien code — aucun `HeaderMap` extrait — il ne pouvait pas y arriver.
//!
//! ## Hermétique : aucun appel réseau
//!
//! Le réglage `lastfm_api_key` est posé VIDE, ce qui fait répondre la route
//! avant la moindre requête sortante — et rend le résultat indépendant d'un
//! `LASTFM_API_KEY` qui traînerait dans l'environnement, puisqu'un réglage
//! présent gagne sur la variable.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;

const NOM: &str = "Pink Floyd";
const ID: i64 = 1;
const CLE: &str = "metadata.enrich.cleLastfmAbsente";

/// Un serveur avec un artiste et AUCUNE clé Last.fm exploitable.
fn app_sans_cle_lastfm() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&ID as &dyn ToSqlValue, &NOM as &dyn ToSqlValue],
        )
        .expect("insertion de l'artiste");
    SettingsRepo::with_backend(state.backend.clone())
        .set("lastfm_api_key", "")
        .expect("réglage posé vide");
    tune_server::routes::router(state)
}

/// Interroge la route, en joignant `Accept-Language` seulement si `entete` est
/// donné — un `None` doit produire une requête SANS en-tête du tout.
async fn enrichir(app: &axum::Router, entete: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::get(format!("/api/v1/metadata/artists/{ID}/enrich"));
    if let Some(valeur) = entete {
        req = req.header(axum::http::header::ACCEPT_LANGUAGE, valeur);
    }
    let reponse = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

/// Le message doit être celui de la langue demandée — et il est comparé à la
/// table, pas recopié ici : une retouche de formulation ne casse pas le test,
/// mais un repli sur la mauvaise langue le casse.
async fn message(app: &axum::Router, entete: Option<&str>) -> String {
    let (status, corps) = enrichir(app, entete).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "le statut ne change pas : le client déployé lit `data`/`bio`, ne \
         trouve rien et annonce « aucune information trouvée », comme avant"
    );
    assert_eq!(
        corps["error"].as_str(),
        Some("lastfm_cle_absente"),
        "un code stable pour qui programme contre l'API"
    );
    assert_eq!(
        corps["setting"].as_str(),
        Some("lastfm_api_key"),
        "le nom exact du réglage qui active la source"
    );
    corps["message"]
        .as_str()
        .expect("un message doit être rendu")
        .to_string()
}

// ---------------------------------------------------------------------------
// L'en-tête gouverne
// ---------------------------------------------------------------------------

/// Le cas exact du défaut : interface anglaise, aucun `?lang=` — le client
/// n'en envoie sur aucune route « bio ». Sur l'ancien code la poignée
/// n'extrayait pas d'en-tête du tout ; ce cas ne pouvait pas être vu.
#[tokio::test]
async fn en_interface_anglaise_le_serveur_repond_en_anglais() {
    let app = app_sans_cle_lastfm();
    assert_eq!(
        message(&app, Some("en")).await,
        tune_server::i18n::t("en", CLE)
    );
}

/// Interface française : le français, qui est aussi le repli — d'où le cas
/// suivant, qui montre que ce n'est pas le repli qui répond ici.
#[tokio::test]
async fn en_interface_francaise_le_serveur_repond_en_francais() {
    let app = app_sans_cle_lastfm();
    assert_eq!(
        message(&app, Some("fr-FR,fr;q=0.9,en;q=0.8")).await,
        tune_server::i18n::t("fr", CLE)
    );
}

/// Un en-tête pondéré est réduit à sa base, et une langue ni française ni
/// anglaise est servie : c'est bien l'en-tête qui est lu, pas un repli.
#[tokio::test]
async fn un_entete_pondere_est_reduit_a_sa_base() {
    let app = app_sans_cle_lastfm();
    assert_eq!(
        message(&app, Some("de-DE,de;q=0.9,en;q=0.8")).await,
        tune_server::i18n::t("de", CLE)
    );
}

/// Les dix langues de l'interface passent, et deux langues distinctes ne
/// peuvent pas rendre le même texte : sans cela, un serveur qui répondrait
/// toujours pareil passerait les cas ci-dessus.
#[tokio::test]
async fn chaque_langue_de_l_interface_a_son_propre_message() {
    let app = app_sans_cle_lastfm();
    let mut vus: Vec<(String, String)> = Vec::new();
    for langue in tune_server::i18n::SUPPORTED {
        let texte = message(&app, Some(langue)).await;
        assert_eq!(
            texte,
            tune_server::i18n::t(langue, CLE),
            "«{langue}» ne reçoit pas sa propre traduction"
        );
        assert_ne!(
            texte, CLE,
            "«{langue}» n'est pas traduite : la clé partirait à l'écran"
        );
        if let Some((autre, _)) = vus.iter().find(|(_, t)| *t == texte) {
            panic!("«{langue}» rend le même texte que «{autre}» — l'en-tête ne gouverne pas");
        }
        vus.push((langue.to_string(), texte));
    }
}

/// Sans en-tête, le repli reste le français : un client muet garde le
/// comportement d'avant.
#[tokio::test]
async fn sans_entete_le_repli_reste_le_francais() {
    let app = app_sans_cle_lastfm();
    assert_eq!(message(&app, None).await, tune_server::i18n::t("fr", CLE));
}

/// Une locale que l'interface ne parle pas retombe sur le français, comme
/// partout ailleurs — et non sur la clé brute.
#[tokio::test]
async fn une_locale_non_supportee_retombe_sur_le_francais() {
    let app = app_sans_cle_lastfm();
    assert_eq!(
        message(&app, Some("pt-BR,pt;q=0.9")).await,
        tune_server::i18n::t("fr", CLE)
    );
}

/// Un artiste inconnu reste un 404 : la lecture de l'en-tête ne change rien à
/// l'existant.
#[tokio::test]
async fn un_artiste_inconnu_reste_un_404() {
    let app = app_sans_cle_lastfm();
    let reponse = app
        .oneshot(
            Request::get("/api/v1/metadata/artists/4242/enrich")
                .header(axum::http::header::ACCEPT_LANGUAGE, "en")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::NOT_FOUND);
}
