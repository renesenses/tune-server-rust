//! Les collections intelligentes livrées portent une CLÉ, pas seulement un nom
//! français (#fuites-fr).
//!
//! Le semis écrit les seize collections par défaut en français —
//! `tune-core/src/db/migrations.rs:546` (`seed_default_smart_collections`) et
//! `:614` (`reseed_smart_collections`). Quatre d'entre elles sautent aux yeux
//! dans une interface roumaine : « 🖼️ Sans pochette », « 🆕 Récents »,
//! « 🎻 Classique », « 🎬 Bandes Originales ». Les voisines passaient inaperçues
//! parce que leur nom est déjà neutre (Jazz, Rock, Pop, Piano, Audiophile,
//! Soul & Funk, SACD / DSD, World Music).
//!
//! Le client rendait `col.name` brut — `tune-web-client/src/components/`
//! `SmartCollectionsView.svelte:235` — et n'avait RIEN à quoi accrocher une
//! traduction : le type `SmartCollection` (`src/lib/types.ts:810`) ne porte
//! aucune clé.
//!
//! Les essais tiennent quatre propriétés, mesurées sur les ROUTES montées et
//! sur le catalogue RÉELLEMENT semé par les migrations :
//!
//! 1. chaque collection livrée arrive avec `name_key` ;
//! 2. les quatre noms du signalement portent la clé attendue ;
//! 3. **la contre-épreuve** : `name` et `description` sont intacts, et les
//!    champs que cite `docs/contrat-web.json` sont tous là ;
//! 4. une collection renommée par l'utilisateur ne reçoit AUCUNE clé — la
//!    promesse « on ne renomme pas ce que l'utilisateur a renommé ».

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn lister(app: &axum::Router) -> Vec<Value> {
    let reponse = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/library/smart-collections")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&octets)
        .unwrap()
        .as_array()
        .expect("la liste des collections est un tableau")
        .clone()
}

fn par_nom<'a>(items: &'a [Value], nom: &str) -> Option<&'a Value> {
    items.iter().find(|c| c["name"] == nom)
}

#[tokio::test]
async fn les_collections_livrees_arrivent_avec_leur_cle() {
    let (app, _etat) = app_et_etat();
    let items = lister(&app).await;
    assert!(
        !items.is_empty(),
        "le semis des collections par défaut doit avoir tourné"
    );

    let sans_cle: Vec<String> = items
        .iter()
        .filter(|c| c.get("name_key").is_none())
        .map(|c| c["name"].as_str().unwrap_or("?").to_string())
        .collect();
    assert!(
        sans_cle.is_empty(),
        "collections livrées sans clé stable : {sans_cle:?}"
    );
}

#[tokio::test]
async fn les_quatre_noms_francais_du_signalement_portent_la_bonne_cle() {
    let (app, _etat) = app_et_etat();
    let items = lister(&app).await;

    for (nom, cle) in [
        ("🖼️ Sans pochette", "smartCollection.default.noCover"),
        ("🆕 Récents", "smartCollection.default.recent"),
        ("🎻 Classique", "smartCollection.default.classical"),
        (
            "🎬 Bandes Originales",
            "smartCollection.default.soundtracks",
        ),
    ] {
        let collection =
            par_nom(&items, nom).unwrap_or_else(|| panic!("collection « {nom} » absente du semis"));
        assert_eq!(collection["name_key"], cle, "clé de « {nom} »");
        assert!(
            collection["description_key"].is_string(),
            "clé de description de « {nom} »"
        );
    }
}

#[tokio::test]
async fn deux_collections_livrees_ne_partagent_jamais_leur_cle() {
    // Contre-épreuve : une table qui rendrait la même clé pour tout le monde
    // passerait l'essai « toutes ont une clé » sans rien distinguer.
    let (app, _etat) = app_et_etat();
    let items = lister(&app).await;
    let mut cles: Vec<&str> = items
        .iter()
        .filter_map(|c| c["name_key"].as_str())
        .collect();
    let total = cles.len();
    cles.sort_unstable();
    cles.dedup();
    assert_eq!(cles.len(), total, "autant de clés distinctes que de tuiles");
}

#[tokio::test]
async fn les_champs_du_contrat_web_sont_intacts() {
    // `docs/contrat-web.json` cite ces champs pour
    // `GET /library/smart-collections` : on AJOUTE, on ne remplace pas.
    let (app, _etat) = app_et_etat();
    let items = lister(&app).await;
    let recents = par_nom(&items, "🆕 Récents").expect("« 🆕 Récents » semée");

    assert_eq!(recents["name"], "🆕 Récents", "`name` inchangé");
    assert_eq!(
        recents["description"], "Ajoutés dans les 90 derniers jours",
        "`description` inchangée"
    );
    for champ in [
        "id",
        "name",
        "description",
        "icon",
        "color",
        "rules",
        "match_mode",
        "sort_order",
        "created_at",
    ] {
        assert!(
            recents.get(champ).is_some(),
            "champ obligatoire du contrat web : {champ}"
        );
    }
}

#[tokio::test]
async fn une_collection_renommee_par_lutilisateur_ne_recoit_aucune_cle() {
    // LE point de la fiche. L'utilisateur rebaptise « 🆕 Récents » ; le
    // serveur ne doit ni renommer sa collection, ni lui recoller l'étiquette
    // du semis par-dessus son choix.
    let (app, etat) = app_et_etat();
    etat.backend
        .execute(
            "UPDATE smart_collections SET name = $1 WHERE name = $2",
            &[
                &"Mes trouvailles du trimestre" as &dyn ToSqlValue,
                &"🆕 Récents" as &dyn ToSqlValue,
            ],
        )
        .unwrap();

    let items = lister(&app).await;
    assert!(
        par_nom(&items, "🆕 Récents").is_none(),
        "le nom d'origine ne doit pas être réécrit en base"
    );
    let mienne = par_nom(&items, "Mes trouvailles du trimestre")
        .expect("la collection renommée est toujours là, sous SON nom");
    assert!(
        mienne.get("name_key").is_none(),
        "aucune clé posée sur un nom choisi par l'utilisateur"
    );
}

#[tokio::test]
async fn une_description_reecrite_garde_la_cle_du_nom_et_perd_la_sienne() {
    let (app, etat) = app_et_etat();
    etat.backend
        .execute(
            "UPDATE smart_collections SET description = $1 WHERE name = $2",
            &[
                &"Ce que j'ai acheté ce trimestre" as &dyn ToSqlValue,
                &"🆕 Récents" as &dyn ToSqlValue,
            ],
        )
        .unwrap();

    let items = lister(&app).await;
    let recents = par_nom(&items, "🆕 Récents").expect("« 🆕 Récents » semée");
    assert_eq!(
        recents["name_key"], "smartCollection.default.recent",
        "le nom, lui, est toujours celui du semis"
    );
    assert!(
        recents.get("description_key").is_none(),
        "la description de l'utilisateur reste la sienne"
    );
}
