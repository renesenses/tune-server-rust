//! web#2211 — « Artistes les plus écoutés » : deux fiches homonymes ne
//! rendent plus deux lignes au même nom.
//!
//! Daniel LEVY, fil 2329, rc6 : la page Recherche se figeait, la console de
//! Chrome disait `each_key_duplicate`. La rangée « Artistes les plus écoutés »
//! du client était indexée par le NOM, et `GET /library/history/top-artists`
//! joignait l'historique à TOUTES les fiches de même nom à la casse près :
//! deux fiches « Diana Krall » donnaient deux lignes « Diana Krall », chacune
//! avec toutes les écoutes.
//!
//! Aucun service enregistré, écoutes locales seulement : la résolution
//! d'images de service n'a rien à faire et la route ne touche pas au réseau.
//! Chaque témoin prend des noms qui lui sont propres : la mémoire des images
//! est commune au processus.

use axum::Json;
use axum::extract::{Query, State};
use serde_json::Value;

use super::{HistoryParams, top_artists};
use crate::state::AppState;

fn serveur() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn fiche(state: &AppState, id: i64, nom: &str) {
    state
        .backend
        .execute(
            &format!("INSERT INTO artists (id, name) VALUES ({id}, '{nom}')"),
            &[],
        )
        .unwrap();
}

fn ecoutes(state: &AppState, artiste: &str, n: usize) {
    for _ in 0..n {
        state
            .backend
            .execute(
                &format!(
                    "INSERT INTO listen_history (title, artist_name, source, duration_ms) \
                     VALUES ('Temoin', '{artiste}', 'local', 200000)"
                ),
                &[],
            )
            .unwrap();
    }
}

async fn top(state: AppState, limit: i64) -> Vec<Value> {
    let Json(corps) = top_artists(
        State(state),
        Query(HistoryParams {
            limit: Some(limit),
            offset: None,
            period: None,
        }),
    )
    .await;
    corps.as_array().expect("un tableau").clone()
}

fn lignes_de<'a>(top: &'a [Value], nom: &str) -> Vec<&'a Value> {
    top.iter()
        .filter(|l| {
            l["name"]
                .as_str()
                .is_some_and(|n| n.to_lowercase() == nom.to_lowercase())
        })
        .collect()
}

/// ⭐ Le cas du fil 2329 : deux fiches au même nom, une seule ligne, rattachée
/// à la plus ancienne, avec le compte réel des écoutes (pas le double).
#[tokio::test]
async fn deux_fiches_homonymes_rendent_une_seule_ligne() {
    let state = serveur();
    fiche(&state, 410, "Diana Krall 2211");
    fiche(&state, 411, "Diana Krall 2211");
    ecoutes(&state, "Diana Krall 2211", 3);

    let top = top(state, 20).await;
    let krall = lignes_de(&top, "Diana Krall 2211");
    assert_eq!(
        krall.len(),
        1,
        "deux fiches homonymes donnent {} lignes au même nom (web#2211) : {top:?}",
        krall.len()
    );
    assert_eq!(
        krall[0]["artist_id"], 410,
        "la plus ancienne fiche : {top:?}"
    );
    assert_eq!(krall[0]["id"], 410);
    assert_eq!(krall[0]["plays"], 3, "écoutes comptées une fois : {top:?}");
}

/// Deux fiches qui ne diffèrent que par la CASSE : même règle que la
/// bibliothèque (`LOWER(name)`), une seule ligne.
#[tokio::test]
async fn deux_fiches_qui_ne_different_que_par_la_casse_rendent_une_seule_ligne() {
    let state = serveur();
    fiche(&state, 420, "Keith Jarrett 2211");
    fiche(&state, 421, "KEITH JARRETT 2211");
    ecoutes(&state, "Keith Jarrett 2211", 2);

    let top = top(state, 20).await;
    let jarrett = lignes_de(&top, "Keith Jarrett 2211");
    assert_eq!(jarrett.len(), 1, "{top:?}");
    assert_eq!(jarrett[0]["artist_id"], 420);
    assert_eq!(jarrett[0]["name"], "Keith Jarrett 2211");
    assert_eq!(jarrett[0]["plays"], 2);
}

/// Des écoutes dont l'historique a gardé deux graphies du même nom se
/// comptent ensemble, sous le nom de la fiche de bibliothèque.
#[tokio::test]
async fn les_graphies_d_un_meme_nom_dans_l_historique_se_comptent_ensemble() {
    let state = serveur();
    fiche(&state, 430, "Bill Evans 2211");
    ecoutes(&state, "Bill Evans 2211", 2);
    ecoutes(&state, "BILL EVANS 2211", 1);

    let top = top(state, 20).await;
    let evans = lignes_de(&top, "Bill Evans 2211");
    assert_eq!(evans.len(), 1, "{top:?}");
    assert_eq!(evans[0]["name"], "Bill Evans 2211");
    assert_eq!(evans[0]["artist_id"], 430);
    assert_eq!(evans[0]["plays"], 3);
}

/// Aucune ligne ne répète un nom ni un identifiant, et le classement reste
/// par écoutes décroissantes ; un artiste sans fiche garde `artist_id` nul.
#[tokio::test]
async fn le_classement_ne_repete_ni_nom_ni_identifiant() {
    let state = serveur();
    fiche(&state, 440, "Nina Simone 2211");
    fiche(&state, 441, "Nina Simone 2211");
    fiche(&state, 442, "Chet Baker 2211");
    ecoutes(&state, "Nina Simone 2211", 5);
    ecoutes(&state, "Chet Baker 2211", 4);
    ecoutes(&state, "Sans Fiche 2211", 1);

    let top = top(state, 20).await;
    let mut noms: Vec<String> = top
        .iter()
        .filter_map(|l| l["name"].as_str().map(str::to_lowercase))
        .collect();
    let total = noms.len();
    noms.sort();
    noms.dedup();
    assert_eq!(noms.len(), total, "un nom répété : {top:?}");

    let mut ids: Vec<i64> = top.iter().filter_map(|l| l["artist_id"].as_i64()).collect();
    let total = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), total, "un identifiant répété : {top:?}");

    let plays: Vec<i64> = top.iter().filter_map(|l| l["plays"].as_i64()).collect();
    assert!(plays.windows(2).all(|w| w[0] >= w[1]), "{plays:?}");
    assert_eq!(
        lignes_de(&top, "Sans Fiche 2211")[0]["artist_id"],
        Value::Null
    );
}

/// Le `LIMIT` compte des ARTISTES : deux homonymes n'occupent pas deux places.
#[tokio::test]
async fn la_limite_compte_des_artistes_et_non_des_fiches() {
    let state = serveur();
    fiche(&state, 450, "Ella Fitzgerald 2211");
    fiche(&state, 451, "Ella Fitzgerald 2211");
    ecoutes(&state, "Ella Fitzgerald 2211", 3);
    ecoutes(&state, "Oscar Peterson 2211", 2);

    let top = top(state, 2).await;
    assert_eq!(top.len(), 2, "{top:?}");
    assert_eq!(top[0]["name"], "Ella Fitzgerald 2211");
    assert_eq!(top[1]["name"], "Oscar Peterson 2211");
}
