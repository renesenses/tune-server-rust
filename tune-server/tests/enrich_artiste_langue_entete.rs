//! `GET /metadata/artists/{id}/enrich` lit bien `Accept-Language` (#1849),
//! de bout en bout, contre un FAUX Wikipédia (#6691).
//!
//! ## Pourquoi ce banc a été réécrit
//!
//! Il pinçait la langue par le message `lastfm_cle_absente`, que la route
//! rendait AVANT tout appel quand la clé Last.fm était vide. Depuis #6462, la
//! route interroge Wikipédia sans clé : ce message ne sort plus que si
//! Wikipédia n'a rien rendu. Le banc partait donc vers la vraie Wikipédia
//! (réseau en essai) et ne vérifiait plus le comportement actuel.
//!
//! ## Ce qui est pincé ici
//!
//! La route lit l'adresse de Wikipédia dans le réglage `wikipedia_base_url`
//! (`{lang}` remplacé par la langue). Le faux serveur local note chaque
//! langue demandée et rend une notice propre à cette langue. On vérifie, par
//! le vrai routeur :
//!
//! - que l'en-tête `Accept-Language` choisit la Wikipédia interrogée EN
//!   PREMIER et la langue de la biographie rendue (`bio_lang`) ;
//! - que l'anglais n'est qu'un repli, demandé seulement quand la langue de
//!   l'interface n'a pas de notice ;
//! - que sans rien de Wikipédia et sans clé, le message `lastfm_cle_absente`
//!   est dans la langue demandée.
//!
//! ## Hermétique : aucun appel réseau
//!
//! `lastfm_api_key` est posé VIDE (un réglage présent gagne sur la variable
//! d'environnement) : Last.fm n'est pas appelée. Wikipédia est le faux
//! serveur de `127.0.0.1`.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;

const NOM: &str = "Pink Floyd";
const ID: i64 = 1;
const CLE: &str = "metadata.enrich.cleLastfmAbsente";

/// La notice que le faux Wikipédia rend dans `langue` — assez longue pour
/// que la route ne cherche pas plus loin (seuil de 50 caractères).
fn notice(langue: &str) -> String {
    format!("[{langue}] Pink Floyd est un groupe de rock progressif britannique fondé en 1965.")
}

/// Un faux Wikipédia : `/{langue}/api/rest_v1/page/summary/{titre}`. Il note
/// chaque langue demandée, et ne connaît l'article que dans `connues`.
async fn faux_wikipedia(connues: &'static [&'static str]) -> (String, Arc<Mutex<Vec<String>>>) {
    let journal = Arc::new(Mutex::new(Vec::new()));
    let j = journal.clone();
    let app = axum::Router::new().route(
        "/{langue}/api/rest_v1/page/summary/{titre}",
        axum::routing::get(move |Path((langue, titre)): Path<(String, String)>| {
            j.lock().unwrap().push(langue.clone());
            async move {
                if titre != NOM || !connues.contains(&langue.as_str()) {
                    return (
                        StatusCode::NOT_FOUND,
                        axum::Json(json!({"type": "not_found", "title": "Not found."})),
                    )
                        .into_response();
                }
                axum::Json(json!({
                    "title": NOM,
                    "extract": notice(&langue),
                    "content_urls": {"desktop": {"page": format!("https://{langue}.wikipedia.org/wiki/Pink_Floyd")}}
                }))
                .into_response()
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (format!("http://{adresse}/{{lang}}"), journal)
}

/// Un serveur avec un artiste, AUCUNE clé Last.fm exploitable, et Wikipédia
/// pointée sur le faux serveur.
async fn serveur(
    connues: &'static [&'static str],
) -> (
    axum::Router,
    Arc<Mutex<Vec<String>>>,
    tune_server::state::AppState,
) {
    let (base, journal) = faux_wikipedia(connues).await;
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&ID as &dyn ToSqlValue, &NOM as &dyn ToSqlValue],
        )
        .expect("insertion de l'artiste");
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages
        .set("lastfm_api_key", "")
        .expect("réglage posé vide");
    reglages
        .set("wikipedia_base_url", &base)
        .expect("Wikipédia pointée sur le faux serveur");
    (tune_server::routes::router(state.clone()), journal, state)
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

/// La biographie rendue : sa langue et son texte. Le statut ne change pas
/// (200) : le client lit `data.bio`.
async fn biographie(app: &axum::Router, entete: Option<&str>) -> (String, String) {
    let (status, corps) = enrichir(app, entete).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    let data = &corps["data"];
    (
        data["bio_lang"]
            .as_str()
            .unwrap_or_else(|| panic!("aucune biographie rendue : {corps}"))
            .to_string(),
        data["bio"].as_str().unwrap_or_default().to_string(),
    )
}

fn demandes(journal: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    journal.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// L'en-tête choisit la Wikipédia, l'anglais n'est qu'un repli
// ---------------------------------------------------------------------------

/// Le cas exact du défaut #1849 : interface anglaise, aucun `?lang=`.
#[tokio::test]
async fn en_interface_anglaise_la_bio_vient_de_la_wikipedia_anglaise() {
    let (app, journal, _) = serveur(&["en", "fr"]).await;
    assert_eq!(
        biographie(&app, Some("en")).await,
        ("en".to_string(), notice("en"))
    );
    assert_eq!(demandes(&journal), vec!["en"]);
}

/// Interface française : fr.wikipedia d'abord, et l'anglais n'est PAS
/// demandé quand la notice française suffit.
#[tokio::test]
async fn en_interface_francaise_la_bio_vient_de_la_wikipedia_francaise() {
    let (app, journal, state) = serveur(&["en", "fr"]).await;
    assert_eq!(
        biographie(&app, Some("fr-FR,fr;q=0.9,en;q=0.8")).await,
        ("fr".to_string(), notice("fr"))
    );
    assert_eq!(demandes(&journal), vec!["fr"]);
    // Et la notice est gardée avec sa provenance.
    let artiste = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone())
        .get(ID)
        .unwrap()
        .unwrap();
    assert_eq!(artiste.bio.as_deref(), Some(notice("fr").as_str()));
}

/// Un en-tête pondéré est réduit à sa base, et une langue ni française ni
/// anglaise est servie : c'est bien l'en-tête qui est lu, pas un repli.
#[tokio::test]
async fn un_entete_pondere_est_reduit_a_sa_base() {
    let (app, journal, _) = serveur(&["en", "de"]).await;
    assert_eq!(
        biographie(&app, Some("de-DE,de;q=0.9,en;q=0.8")).await,
        ("de".to_string(), notice("de"))
    );
    assert_eq!(demandes(&journal), vec!["de"]);
}

/// Pas d'article dans la langue de l'interface : l'anglais, en second.
#[tokio::test]
async fn sans_notice_dans_la_langue_l_anglais_vient_en_repli() {
    let (app, journal, _) = serveur(&["en"]).await;
    assert_eq!(
        biographie(&app, Some("fr")).await,
        ("en".to_string(), notice("en"))
    );
    assert_eq!(demandes(&journal), vec!["fr", "en"]);
}

/// Chaque langue de l'interface (`SUPPORTED`) interroge SA Wikipédia en
/// premier et rend sa notice.
#[tokio::test]
async fn chaque_langue_de_l_interface_interroge_sa_wikipedia() {
    let (app, journal, _) = serveur(tune_server::i18n::SUPPORTED).await;
    for langue in tune_server::i18n::SUPPORTED {
        journal.lock().unwrap().clear();
        assert_eq!(
            biographie(&app, Some(langue)).await,
            (langue.to_string(), notice(langue)),
            "«{langue}»"
        );
        assert_eq!(demandes(&journal), vec![langue.to_string()], "«{langue}»");
    }
}

/// Sans en-tête, le repli est l'anglais, comme le client web — jamais le
/// français par défaut.
#[tokio::test]
async fn sans_entete_le_repli_est_l_anglais() {
    let (app, journal, _) = serveur(&["en", "fr"]).await;
    assert_eq!(biographie(&app, None).await.0, "en");
    assert_eq!(demandes(&journal), vec!["en"]);
}

/// Une locale que l'interface ne parle pas retombe sur l'anglais, comme
/// partout ailleurs — et non sur le français.
#[tokio::test]
async fn une_locale_non_supportee_retombe_sur_l_anglais() {
    let (app, journal, _) = serveur(&["en", "fr", "pt"]).await;
    assert_eq!(biographie(&app, Some("pt-BR,pt;q=0.9")).await.0, "en");
    assert_eq!(demandes(&journal), vec!["en"]);
}

// ---------------------------------------------------------------------------
// Wikipédia muette, sans clé : le message dans la langue demandée
// ---------------------------------------------------------------------------

/// Aucune notice nulle part et aucune clé Last.fm : `lastfm_cle_absente`,
/// dans la langue de l'en-tête — comparé à la table, pas recopié ici.
#[tokio::test]
async fn sans_rien_de_wikipedia_le_message_suit_l_entete() {
    let (app, journal, _) = serveur(&[]).await;
    let mut vus: Vec<(String, String)> = Vec::new();
    for langue in tune_server::i18n::SUPPORTED {
        let (status, corps) = enrichir(&app, Some(langue)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            corps["error"].as_str(),
            Some("lastfm_cle_absente"),
            "{corps}"
        );
        assert_eq!(corps["setting"].as_str(), Some("lastfm_api_key"));
        let texte = corps["message"].as_str().expect("un message").to_string();
        assert_eq!(texte, tune_server::i18n::t(langue, CLE), "«{langue}»");
        assert_ne!(texte, CLE, "«{langue}» n'est pas traduite");
        if let Some((autre, _)) = vus.iter().find(|(_, t)| *t == texte) {
            panic!("«{langue}» rend le même texte que «{autre}» — l'en-tête ne gouverne pas");
        }
        vus.push((langue.to_string(), texte));
    }
    assert!(
        !demandes(&journal).is_empty(),
        "Wikipédia a bien été interrogée avant de renoncer"
    );
}

/// Un artiste inconnu reste un 404, sans aucune requête sortante.
#[tokio::test]
async fn un_artiste_inconnu_reste_un_404() {
    let (app, journal, _) = serveur(&["en"]).await;
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
    assert!(demandes(&journal).is_empty());
}
