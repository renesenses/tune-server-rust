//! Canal de mise à jour stable / bêta (#2266).
//!
//! ## Ce qui manquait
//!
//! Le filtrage stable/bêta EXISTAIT déjà dans `tune_core::updater::check` —
//! `let on_beta = self.current_version.contains('-')` — mais il n'était pas
//! *pilotable* : il se déduisait de la version du binaire en cours. Deux
//! impasses en sortaient :
//!
//! * un testeur sur une release stable ne pouvait pas demander les RC : il
//!   fallait en installer une à la main pour « entrer » sur le canal ;
//! * un testeur ayant pris **une RC cassée** ne pouvait plus en sortir. Son
//!   binaire porte un suffixe, donc il reste sur le canal bêta et se voit
//!   proposer la RC suivante — indéfiniment. C'est le « aucun moyen de rester
//!   sur du stable » du ticket.
//!
//! ## Ce que ces tests tiennent
//!
//! 1. **Le témoin.** Un serveur qui n'a jamais touché au réglage rend `auto`,
//!    et `auto` reproduit exactement la déduction historique.
//! 2. **La route montée.** `PUT` puis `GET /system/update/channel` : la valeur
//!    est persistée et relue par le code de production (`update_channel`), pas
//!    par le test.
//! 3. **Le refus.** Une valeur inconnue rend 400 et **ne modifie rien**. Un
//!    « 200 pour rien » laisserait l'écran croire que le choix a été retenu.
//! 4. **L'effet.** `tune_core::updater::select_release` — la fonction que
//!    `UpdateChecker::check` appelle après avoir relevé le catalogue — cesse de
//!    proposer les préversions sur `stable`, et se met à les proposer sur
//!    `beta`.
//!
//! Aucun de ces tests ne touche au réseau : `select_release` est le morceau de
//! `check()` qui décide, une fois la liste obtenue.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::updater::{UpdateChannel, select_release};

fn make_app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
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
    send(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn put_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    send(
        app,
        Request::put(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

const ROUTE: &str = "/api/v1/system/update/channel";

/// LE TÉMOIN. Serveur neuf, réglage jamais posé : la route dit `auto`, et
/// `auto` est la déduction d'avant #2266. Rien ne change pour personne.
#[tokio::test]
async fn sans_reglage_le_canal_est_auto() {
    let app = make_app();
    let (st, corps) = get(&app, ROUTE).await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(corps["channel"], "auto", "défaut attendu: auto — {corps}");

    // `auto` n'invente rien : il rend ce que le binaire en cours donnait déjà.
    let courant = corps["current"].as_str().unwrap_or_default();
    let attendu = if courant.contains('-') {
        "beta"
    } else {
        "stable"
    };
    assert_eq!(
        corps["effective_channel"], attendu,
        "auto doit reproduire la déduction historique sur {courant} — {corps}"
    );
}

/// La route montée écrit, et le code de production relit. Les trois valeurs
/// font l'aller-retour, `auto` compris — sortir du canal bêta doit être
/// réversible.
#[tokio::test]
async fn le_canal_choisi_survit_a_l_aller_retour() {
    let app = make_app();
    for canal in ["stable", "beta", "auto"] {
        let (st, ecrit) = put_json(&app, ROUTE, json!({ "channel": canal })).await;
        assert_eq!(st, StatusCode::OK, "PUT {canal}: {ecrit}");
        assert_eq!(ecrit["channel"], canal, "PUT {canal}: {ecrit}");

        let (st, relu) = get(&app, ROUTE).await;
        assert_eq!(st, StatusCode::OK, "GET après {canal}: {relu}");
        assert_eq!(relu["channel"], canal, "GET après {canal}: {relu}");
    }
}

/// `stable` et `beta` disent ce qu'ils donneront, quel que soit le binaire :
/// c'est ce que l'écran doit afficher, `auto` seul restant ambigu.
#[tokio::test]
async fn le_canal_force_annonce_son_effet() {
    let app = make_app();
    let (_, s) = put_json(&app, ROUTE, json!({"channel": "stable"})).await;
    assert_eq!(s["effective_channel"], "stable", "{s}");
    let (_, b) = put_json(&app, ROUTE, json!({"channel": "beta"})).await;
    assert_eq!(b["effective_channel"], "beta", "{b}");
}

/// Une valeur inconnue est REFUSÉE, et surtout : elle ne remplace pas le choix
/// en place. Un 200 silencieux serait le pire des deux mondes — l'écran
/// afficherait « nightly » pendant que le serveur resterait sur `beta`.
#[tokio::test]
async fn un_canal_inconnu_est_refuse_et_ne_change_rien() {
    let app = make_app();
    put_json(&app, ROUTE, json!({"channel": "beta"})).await;

    let (st, err) = put_json(&app, ROUTE, json!({"channel": "nightly"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(err["error"], "unknown_channel", "{err}");

    let (_, relu) = get(&app, ROUTE).await;
    assert_eq!(
        relu["channel"], "beta",
        "le refus ne doit pas écraser le canal en place — {relu}"
    );
}

/// Le catalogue tel que l'API des releases le sert : un stable et une
/// préversion PLUS HAUTE. C'est la situation qui piège aujourd'hui.
fn catalogue() -> Vec<Value> {
    vec![
        json!({"tag_name": "v0.9.130", "prerelease": false}),
        json!({"tag_name": "v0.9.131-rc1", "prerelease": true}),
    ]
}

fn propose(courant: &str, canal: UpdateChannel) -> Option<String> {
    select_release(&catalogue(), courant, canal)
        .map(|r| r["tag_name"].as_str().unwrap_or_default().to_string())
}

/// L'EFFET, sur la fonction que `UpdateChecker::check` appelle en production.
///
/// `auto` : le comportement historique, inchangé dans les deux sens.
/// `stable` : la sortie de secours d'un testeur coincé sur une RC cassée.
/// `beta` : l'entrée volontaire, sans avoir à installer une RC à la main.
#[test]
fn le_canal_decide_de_ce_qui_est_propose() {
    assert_eq!(
        propose("0.9.129", UpdateChannel::Auto).as_deref(),
        Some("v0.9.130"),
        "témoin — binaire stable en auto : pas de préversion"
    );
    assert_eq!(
        propose("0.9.130-rc1", UpdateChannel::Auto).as_deref(),
        Some("v0.9.131-rc1"),
        "témoin — binaire de préversion en auto : les RC restent visibles"
    );
    assert_eq!(
        propose("0.9.130-rc1", UpdateChannel::Stable).as_deref(),
        Some("v0.9.130"),
        "stable forcé : la finale, jamais la RC plus haute — c'est la sortie du ticket"
    );
    assert_eq!(
        propose("0.9.129", UpdateChannel::Beta).as_deref(),
        Some("v0.9.131-rc1"),
        "beta demandé depuis un binaire stable : la RC devient visible"
    );
}
