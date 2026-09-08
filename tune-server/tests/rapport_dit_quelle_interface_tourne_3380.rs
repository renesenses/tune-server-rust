//! Le rapport de bogue dit quelle INTERFACE tourne (#3380).
//!
//! ## Ce qui manquait
//!
//! Quand un testeur décrit un bogue d'écran, on connaît la version de son
//! **serveur** ; on ignore celle de son **interface**. `web/` est déployé
//! SÉPARÉMENT du binaire — cas consigné, un correctif fusionné sur
//! `release/v0.9` a disparu du .18 du jour au lendemain, écrasé à 06:09 par un
//! déploiement basé sur `main`. Plusieurs tickets de la semaine n'ont pas pu
//! être arbitrés faute de savoir quel écran tournait, et il y en a deux en
//! parallèle.
//!
//! Mesuré : zéro occurrence de `ui_version` ou `web_version` dans
//! `tune-server/src` et `tune-core/src`. Elle existe pourtant côté client —
//! `package.json` la porte, `vite.config.ts` la compile — mais personne ne la
//! remonte, et sur le .18 elle n'est lisible que noyée dans un
//! `assets/index-<hash>.js` minifié.
//!
//! ## Ce que ce fichier cloue, par la ROUTE
//!
//! 1. `GET /system/bug-report` porte `ui_version` dans son corps JSON et une
//!    ligne « **Interface (web)** » dans le markdown que le testeur colle ;
//! 2. 🔴 la PORTE DE SORTIE du ticket : faute de `web/version.json`, cette
//!    ligne ne se rabat **pas** sur la version du serveur. Le repli, ce serait
//!    deux numéros identiques et l'écart invisible — c'est-à-dire le défaut
//!    lui-même. Un témoin qui ne vérifierait que le cas aligné ne garderait
//!    rien ;
//! 3. les deux faces disent la même chose : quand le fichier EST lisible, la
//!    ligne markdown porte le numéro du champ JSON, et pas un autre.
//!
//! La lecture elle-même — fichier décalé, fichier absent, document illisible —
//! est éprouvée là où la règle vit, dans `tune_core::interface_web`.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

fn banc() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn rapport(app: &axum::Router) -> Value {
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/system/bug-report")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// La ligne d'en-tête, celle qui se lit à côté de « **Version** ».
fn ligne_interface(md: &str) -> &str {
    md.lines()
        .find(|l| l.starts_with("**Interface (web)**"))
        .expect("le rapport doit porter une ligne « Interface (web) » (#3380)")
}

#[tokio::test]
async fn le_rapport_porte_la_version_de_l_interface() {
    let (app, _state) = banc();
    let r = rapport(&app).await;

    assert!(
        r.get("ui_version").is_some(),
        "le corps JSON doit porter `ui_version` : c'est lui que la télémétrie \
         reprend et que l'admin mozaiklabs affichera à côté de `version` — {r}"
    );

    let md = r["markdown"].as_str().unwrap_or_default();
    let ligne = ligne_interface(md);

    match r["ui_version"].as_str() {
        // 🔴 LA PORTE DE SORTIE. Aucun `web/version.json` lisible : le rapport
        // ne doit RIEN affirmer, et surtout pas la version du serveur.
        None => {
            assert!(
                !ligne.contains(tune_core::version()),
                "aucun `web/version.json` n'est lisible, et la ligne \
                 d'interface affiche pourtant la version du SERVEUR : c'est \
                 exactement le mensonge de #3380 — deux numéros identiques et \
                 l'écart invisible — {ligne}"
            );
            assert!(
                ligne.contains("inconnue"),
                "elle doit dire qu'elle ne sait pas — {ligne}"
            );
        }
        // Un `web/` est présent sur la machine d'essai : alors les deux faces
        // du rapport doivent porter le MÊME numéro.
        Some(v) => assert!(
            ligne.contains(v),
            "la ligne markdown et le champ JSON doivent dire le même numéro — \
             {ligne}"
        ),
    }
}

/// La ligne d'interface se lit COLLÉE à celle du serveur : c'est la paire qui
/// tranche un ticket, pas l'un des deux numéros pris seul. Si elle finissait
/// vingt lignes plus bas, personne ne les comparerait.
#[tokio::test]
async fn les_deux_versions_se_lisent_ensemble_en_tete_du_rapport() {
    let (app, _state) = banc();
    let r = rapport(&app).await;
    let md = r["markdown"].as_str().unwrap_or_default();

    let serveur = md
        .lines()
        .position(|l| l.starts_with("**Version**"))
        .expect("le rapport porte la version du serveur");
    let interface = md
        .lines()
        .position(|l| l.starts_with("**Interface (web)**"))
        .expect("le rapport porte la version de l'interface");

    assert_eq!(
        interface,
        serveur + 1,
        "la version de l'interface doit suivre IMMÉDIATEMENT celle du \
         serveur : c'est leur écart qui explique un bogue d'écran"
    );
}
