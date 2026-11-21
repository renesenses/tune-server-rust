//! Forum, 21/11/2026 — « Bug collections intelligentes, erreur peut-être liée
//! au nom de plus de 3 mots ». Le nom n'y était pour rien : il était DÉJÀ
//! PRIS. La création rendait 500 (`UNIQUE constraint failed:
//! smart_collections.name`) et le renommage rendait 200 en gardant l'ancien
//! nom. Les deux rendent désormais 409 `smart_collection_name_taken`, sans
//! rien écrire. La même épreuve sur PostgreSQL vit dans
//! `tune-server/tests/pg_nom_de_collection_deja_pris.rs`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::sqlite::SqliteDb;

use crate::SmartHttpState;

const NOM_PRIS: &str = "Jazz vocal des années cinquante";

fn etat() -> SmartHttpState {
    let db = SqliteDb::open_in_memory().expect("base");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    db.execute_batch("DELETE FROM smart_collections;")
        .expect("table vide");
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    SmartHttpState::new(backend)
}

async fn corps(reponse: axum::response::Response) -> (StatusCode, Value) {
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn creer(etat: &SmartHttpState, nom: &str) -> (StatusCode, Value) {
    let charge = serde_json::from_value(json!({"name": nom, "rules": []})).expect("création");
    let reponse = match super::create_collection(State(etat.clone()), Json(charge)).await {
        Ok(r) => r.into_response(),
        Err(e) => e.into_response(),
    };
    corps(reponse).await
}

async fn modifier(etat: &SmartHttpState, id: i64, charge: Value) -> (StatusCode, Value) {
    let charge = serde_json::from_value(charge).expect("modification");
    let reponse = match super::update_collection(State(etat.clone()), Path(id), Json(charge)).await
    {
        Ok(r) => r.into_response(),
        Err(e) => e.into_response(),
    };
    corps(reponse).await
}

fn nom_en_base(etat: &SmartHttpState, id: i64) -> String {
    etat.backend
        .query_one(
            "SELECT name FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .expect("lecture")
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .expect("collection présente")
}

fn nombre_de_collections(etat: &SmartHttpState) -> i64 {
    etat.backend
        .query_one("SELECT COUNT(*) FROM smart_collections", &[])
        .expect("compte")
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

fn identifiant(v: &Value) -> i64 {
    v["id"].as_i64().expect("id de la collection créée")
}

#[tokio::test]
async fn creer_sous_un_nom_deja_pris_rend_409_et_n_ecrit_rien() {
    let etat = etat();
    let (statut, _) = creer(&etat, NOM_PRIS).await;
    assert_eq!(statut, StatusCode::CREATED);

    let (statut, v) = creer(&etat, NOM_PRIS).await;
    assert_eq!(statut, StatusCode::CONFLICT, "doublon : {v}");
    assert_eq!(v["code"], super::CODE_NOM_PRIS, "{v}");
    let message = v["error"].as_str().unwrap_or_default();
    assert!(
        message.contains(NOM_PRIS),
        "le message nomme le nom : {message}"
    );
    assert!(!message.contains("UNIQUE"), "plus de texte SQL : {message}");
    assert_eq!(nombre_de_collections(&etat), 1);
}

#[tokio::test]
async fn renommer_vers_un_nom_pris_rend_409_et_garde_le_nom_d_origine() {
    let etat = etat();
    let (_, a) = creer(&etat, NOM_PRIS).await;
    let (_, b) = creer(&etat, "Piano solo").await;
    let id_b = identifiant(&b);

    let (statut, v) = modifier(
        &etat,
        id_b,
        json!({"name": NOM_PRIS, "description": "ne doit pas s'écrire"}),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "renommage : {v}");
    assert_eq!(v["code"], super::CODE_NOM_PRIS, "{v}");
    assert_eq!(nom_en_base(&etat, id_b), "Piano solo");
    assert_eq!(nom_en_base(&etat, identifiant(&a)), NOM_PRIS);
    // Refusé AVANT toute écriture : les autres champs du même envoi non plus.
    let description: Option<String> = etat
        .backend
        .query_one(
            "SELECT description FROM smart_collections WHERE id = $1",
            &[&id_b as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(description, None);
}

#[tokio::test]
async fn renommer_vers_son_propre_nom_passe() {
    let etat = etat();
    let (_, a) = creer(&etat, NOM_PRIS).await;
    let id = identifiant(&a);
    let (statut, v) = modifier(
        &etat,
        id,
        json!({"name": NOM_PRIS, "description": "même nom, autre description"}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["name"], NOM_PRIS);
    assert_eq!(v["description"], "même nom, autre description");
}

#[tokio::test]
async fn renommer_vers_un_nom_libre_passe() {
    let etat = etat();
    let (_, a) = creer(&etat, NOM_PRIS).await;
    let id = identifiant(&a);
    let (statut, v) = modifier(&etat, id, json!({"name": "Jazz vocal"})).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(nom_en_base(&etat, id), "Jazz vocal");
}

/// La comparaison est celle de l'index SQLite : exacte. Ce test fixe le
/// comportement constaté, il ne le choisit pas.
#[tokio::test]
async fn la_comparaison_est_exacte_casse_et_espaces_compris() {
    let etat = etat();
    let (statut, _) = creer(&etat, "Jazz").await;
    assert_eq!(statut, StatusCode::CREATED);
    let (statut, _) = creer(&etat, "jazz").await;
    assert_eq!(statut, StatusCode::CREATED, "la casse distingue deux noms");
    let (statut, _) = creer(&etat, " Jazz ").await;
    assert_eq!(statut, StatusCode::CREATED, "les espaces autour aussi");
}

/// Le dernier rempart (deux créations simultanées passées toutes deux par
/// la vérification) : la violation de l'index se lit comme un 409.
#[test]
fn la_violation_de_l_index_se_lit_comme_un_409() {
    let e = super::erreur_d_ecriture_du_nom(
        NOM_PRIS,
        "execute: UNIQUE constraint failed: smart_collections.name".into(),
    );
    assert_eq!(e.status, StatusCode::CONFLICT);
    assert_eq!(e.code.as_deref(), Some(super::CODE_NOM_PRIS));
    let e = super::erreur_d_ecriture_du_nom(
        NOM_PRIS,
        "duplicate key value violates unique constraint \"idx_smart_collections_name\"".into(),
    );
    assert_eq!(e.status, StatusCode::CONFLICT);
    let e = super::erreur_d_ecriture_du_nom(NOM_PRIS, "disk I/O error".into());
    assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
}
