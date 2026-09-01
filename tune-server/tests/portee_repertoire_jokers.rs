//! #3101 — sélectionner un répertoire rend CE répertoire et rien d'autre.
//!
//! Le signalement (Sevy Tabroc, fil 1637) dit « l'entièreté de la bibliothèque
//! s'affiche et non pas celle du répertoire sélectionné ». C'est la signature
//! d'un filtre qui ne filtre pas : il rend PLUS que demandé.
//!
//! Le défaut mesuré ici est dans le motif `LIKE` que TOUTE portée de répertoire
//! partage (`folder_like_pattern`) : un nom de dossier peut contenir `%` ou `_`,
//! qui sont précisément les deux jokers de `LIKE`. Le préfixe partait donc en
//! base comme un MOTIF au lieu d'un texte.
//!
//! * `100% Live` → motif `…/100% Live/%` : le `%` du milieu avale n'importe
//!   quelle suite, **séparateurs compris**, et le répertoire ramène le contenu
//!   d'un tout autre sous-arbre — ici `…/1000/Best Of Live/`.
//! * `Disc_1` → motif `…/Disc_1/%` : le `_` vaut n'importe quel caractère, et
//!   le répertoire ramène aussi `…/DiscX1/`.
//!
//! Ces épreuves tournent contre le VRAI routeur et une VRAIE base SQLite, au
//! niveau où le testeur regarde : **l'ensemble des fichiers rendus**, jamais un
//! code HTTP. Un 200 avec la mauvaise liste est exactement le défaut signalé.
//!
//! Les chemins sont construits avec `MAIN_SEPARATOR`, jamais écrits en dur :
//! `folder_like_pattern` pose le séparateur de l'hôte, et une épreuve écrite
//! avec `/` passerait sur un CI Linux tout en laissant Windows nu.
//!
//! La moitié PostgreSQL de la même preuve vit dans
//! `tune-core/src/db/postgres_e2e.rs`
//! (`pg_3101_les_jokers_du_nom_de_dossier_ne_filtrent_pas_plus_large`) : la
//! route `/library/tracks` vit dans `tune-server`, que la matrice `Test
//! (PostgreSQL)` ne compile pas — la mesure sur le second moteur porte donc sur
//! `TrackRepo::list_filtered`, la fonction que cette route appelle.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::path::MAIN_SEPARATOR as SEP;
use tower::ServiceExt;
use tune_server::state::AppState;

/// Encodage `application/x-www-form-urlencoded` du strict nécessaire — c'est ce
/// que `URLSearchParams` produit côté navigateur, et `%` comme l'espace DOIVENT
/// y passer : ce sont justement les caractères que cette épreuve manipule.
fn encode(v: &str) -> String {
    let mut out = String::new();
    for b in v.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Les `file_path` rendus par `GET /library/tracks`, triés — le fait de base.
async fn fichiers_rendus(app: &axum::Router, requete: &str) -> (StatusCode, i64, Vec<String>) {
    let resp = app
        .clone()
        .oneshot(Request::get(requete).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let total = json.get("total").and_then(|v| v.as_i64()).unwrap_or(-1);
    let mut fichiers: Vec<String> = json
        .get("items")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    t.get("file_path")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    fichiers.sort();
    (status, total, fichiers)
}

async fn portee(app: &axum::Router, dossier: &str) -> (StatusCode, i64, Vec<String>) {
    let requete = format!(
        "/api/v1/library/tracks?folder={}&limit=500",
        encode(dossier)
    );
    fichiers_rendus(app, &requete).await
}

fn joindre(segments: &[&str]) -> String {
    let mut s = String::new();
    for seg in segments {
        s.push(SEP);
        s.push_str(seg);
    }
    s
}

/// Une bibliothèque d'épreuve dont deux dossiers portent un joker de `LIKE` et
/// deux autres sont les voisins que ce joker attrape à tort.
///
/// | dossier              | pistes | pourquoi |
/// |----------------------|--------|----------|
/// | `100% Live`          | 2      | le `%` du nom |
/// | `100% Live/Bonus`    | 1      | témoin imbriqué |
/// | `1000/Best Of Live`  | 3      | ce que le `%` avalait, dans un autre sous-arbre |
/// | `Disc_1`             | 4      | le `_` du nom |
/// | `DiscX1`             | 5      | ce que le `_` avalait |
/// | `Vide`               | 0      | témoin du répertoire vide |
///
/// Quinze pistes. Les effectifs et leurs sommes deux à deux sont tous
/// distincts, pour qu'aucun compte juste ne puisse l'être par accident.
fn bibliotheque() -> (axum::Router, String) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let racine = joindre(&["musique"]);
    let mut n = 0;
    for (dossier, combien) in [
        (joindre(&["musique", "100% Live"]), 2),
        (joindre(&["musique", "100% Live", "Bonus"]), 1),
        (joindre(&["musique", "1000", "Best Of Live"]), 3),
        (joindre(&["musique", "Disc_1"]), 4),
        (joindre(&["musique", "DiscX1"]), 5),
    ] {
        for _ in 0..combien {
            n += 1;
            let chemin = format!("{dossier}{SEP}p{n}.flac");
            // Le chemin est lié, jamais interpolé : il porte des `%` et des `_`
            // et il n'est pas question qu'une épreuve sur l'échappement `LIKE`
            // repose sur une interpolation de chaîne.
            state
                .backend
                .execute(
                    "INSERT INTO tracks (title, artist_id, file_path, duration_ms, format, sample_rate) \
                     VALUES ('Piste', NULL, ?1, 200000, 'flac', 44100)",
                    &[&chemin as &dyn tune_core::db::backend::ToSqlValue],
                )
                .expect("insertion de piste");
        }
    }
    assert_eq!(n, 15, "la bibliothèque d'épreuve doit porter 15 pistes");
    (tune_server::routes::router(state), racine)
}

/// LE FAIT : sélectionner `100% Live` rend `100% Live`, et rien d'autre.
///
/// Avant le correctif la réponse portait aussi les trois pistes de
/// `1000/Best Of Live` — un sous-arbre voisin, six fichiers au lieu de trois,
/// et un 200 OK pour annoncer la mauvaise liste.
#[tokio::test]
async fn un_pourcent_dans_le_nom_ne_rend_pas_le_dossier_voisin() {
    let (app, racine) = bibliotheque();
    let vise = format!("{racine}{SEP}100% Live");
    let (st, total, fichiers) = portee(&app, &vise).await;
    assert_eq!(st, StatusCode::OK);

    let prefixe = format!("{vise}{SEP}");
    assert!(
        fichiers.iter().all(|f| f.starts_with(&prefixe)),
        "des fichiers hors du répertoire sélectionné : {fichiers:?}"
    );
    assert_eq!(
        fichiers.len(),
        3,
        "2 pistes du dossier + 1 du sous-dossier, et rien de « 1000/Best Of Live » : {fichiers:?}"
    );
    assert_eq!(
        total, 3,
        "le compteur partage le prédicat de la liste, sinon la vue pagine faux"
    );
}

/// Le second joker, qui ne vaut qu'UN caractère : `Disc_1` ne doit pas rendre
/// `DiscX1`. Avant le correctif : neuf fichiers au lieu de quatre.
#[tokio::test]
async fn un_souligne_dans_le_nom_ne_rend_pas_le_dossier_voisin() {
    let (app, racine) = bibliotheque();
    let vise = format!("{racine}{SEP}Disc_1");
    let (st, total, fichiers) = portee(&app, &vise).await;
    assert_eq!(st, StatusCode::OK);

    let prefixe = format!("{vise}{SEP}");
    assert!(
        fichiers.iter().all(|f| f.starts_with(&prefixe)),
        "des fichiers hors du répertoire sélectionné : {fichiers:?}"
    );
    assert_eq!(fichiers.len(), 4, "les 4 pistes de Disc_1 : {fichiers:?}");
    assert_eq!(total, 4);
}

/// Témoin : aucune portée rend TOUJOURS toute la bibliothèque. Un correctif qui
/// se contenterait de restreindre plus fort casserait ici.
#[tokio::test]
async fn sans_portee_la_bibliotheque_entiere_est_rendue() {
    let (app, _) = bibliotheque();
    let (st, total, fichiers) = fichiers_rendus(&app, "/api/v1/library/tracks?limit=500").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(fichiers.len(), 15);
    assert_eq!(total, 15);
}

/// Témoin : un répertoire IMBRIQUÉ rend son contenu, pas celui de son parent.
/// Le parent, lui, reste récursif — c'est le contrat du fil d'Ariane.
#[tokio::test]
async fn un_repertoire_imbrique_rend_son_contenu_pas_celui_du_parent() {
    let (app, racine) = bibliotheque();
    let parent = format!("{racine}{SEP}100% Live");
    let enfant = format!("{parent}{SEP}Bonus");

    let (_, total_enfant, de_l_enfant) = portee(&app, &enfant).await;
    assert_eq!(
        de_l_enfant.len(),
        1,
        "le sous-dossier ne porte qu'une piste : {de_l_enfant:?}"
    );
    assert_eq!(total_enfant, 1);
    let prefixe = format!("{enfant}{SEP}");
    assert!(
        de_l_enfant.iter().all(|f| f.starts_with(&prefixe)),
        "le sous-dossier a rendu du contenu du parent : {de_l_enfant:?}"
    );

    let (_, _, du_parent) = portee(&app, &parent).await;
    assert!(
        du_parent.len() > de_l_enfant.len(),
        "le parent doit rester récursif : {du_parent:?}"
    );
}

/// Témoin : un répertoire vide rend zéro PROPREMENT — 200, liste vide, total 0.
/// Pas une erreur, et surtout pas la bibliothèque entière.
#[tokio::test]
async fn un_repertoire_vide_rend_zero_proprement() {
    let (app, racine) = bibliotheque();
    let (st, total, fichiers) = portee(&app, &format!("{racine}{SEP}Vide")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(fichiers.is_empty(), "{fichiers:?}");
    assert_eq!(total, 0);
}
