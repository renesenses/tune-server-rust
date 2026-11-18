//! #1770, annexe 4 — le nom demandé à `POST /zones` quand le périphérique a
//! déjà une zone.
//!
//! L'arbitrage est rendu (Bertrand, 17/11, #8987) : une zone VISIBLE refuse
//! la création en `409 zone_existe_deja`, en la nommant ; une zone MASQUÉE
//! (supprimée) ressuscite sous le nom demandé. Ces témoins verrouillent ce
//! contrat.

use crate::state::AppState;
use tune_core::db::zone_repo::ZoneRepo;

use super::ecriture::{NomDeZoneExistante, nom_de_zone_existante};

#[test]
fn le_nom_deja_porte_ne_decide_rien() {
    assert_eq!(
        nom_de_zone_existante(true, "Salon", "Salon"),
        NomDeZoneExistante::DejaLeBon,
        "zone masquée sous le même nom : elle ressuscite, aucun nom à écrire"
    );
    assert_eq!(
        nom_de_zone_existante(false, "Salon", "Salon"),
        NomDeZoneExistante::Refuse,
        "#8987 — une zone visible refuse la création, même sous le même nom"
    );
}

#[test]
fn une_zone_masquee_reprend_le_nom_demande() {
    assert_eq!(
        nom_de_zone_existante(true, "Salon", "Bureau"),
        NomDeZoneExistante::Honore,
        "c'est la branche décidée de longue date (« Update name in case device \
         was renamed ») : elle ne change pas ici"
    );
}

#[test]
fn une_zone_visible_refuse_la_creation() {
    assert_eq!(
        nom_de_zone_existante(false, "Salon", "Bureau"),
        NomDeZoneExistante::Refuse,
        "#8987 — décision de Bertrand : 409 qui nomme la zone existante, plus \
         de nom écarté en silence"
    );
}

/// #8987 — « crée une zone sur ce périphérique et appelle-la X » quand une zone
/// VISIBLE le porte : `409`, la zone existante nommée, rien d'écrit.
#[tokio::test]
async fn une_zone_locale_visible_garde_son_nom() {
    use axum::response::IntoResponse;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Salon", Some("local"), Some("local:Haut-Parleurs"))
        .unwrap();

    let resp = super::create_zone(
        axum::extract::State(state.clone()),
        axum::Json(super::CreateZone {
            name: "Bureau".into(),
            output_type: Some("local".into()),
            output_device_id: Some("local:Haut-Parleurs".into()),
        }),
    )
    .await
    .into_response();

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::CONFLICT,
        "#8987 — une zone visible refuse la création (409), elle n'est plus \
         rendue en 200 avec le nom demandé écarté"
    );
    assert_eq!(
        repo.get(id).unwrap().unwrap().name,
        "Salon",
        "le refus n'écrit rien : la zone garde son nom"
    );
    assert_eq!(
        repo.list().unwrap().len(),
        1,
        "aucune seconde zone n'est née : le doublon de jfpaquet ne vient pas \
         d'ici"
    );
}

/// L'autre moitié de l'asymétrie, et celle qui est décidée : une zone masquée
/// ressuscite **sous le nom demandé**. Ce témoin garde la branche que le
/// correctif rend faillible — si `update_name` échoue, la route rend 500 et
/// non plus `200 OK` avec l'ancienne fiche.
#[tokio::test]
async fn une_zone_locale_masquee_ressuscite_sous_le_nom_demande() {
    use axum::response::IntoResponse;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Salon", Some("local"), Some("local:Haut-Parleurs"))
        .unwrap();
    repo.delete(id).unwrap();
    assert!(
        repo.is_device_hidden("local:Haut-Parleurs"),
        "préalable du témoin : la zone doit être masquée"
    );

    let resp = super::create_zone(
        axum::extract::State(state.clone()),
        axum::Json(super::CreateZone {
            name: "Bureau".into(),
            output_type: Some("local".into()),
            output_device_id: Some("local:Haut-Parleurs".into()),
        }),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let zone = repo.get(id).unwrap().unwrap();
    assert_eq!(
        zone.name, "Bureau",
        "une zone masquée reprend le nom demandé : c'est la branche décidée"
    );
    assert!(
        !repo.is_device_hidden("local:Haut-Parleurs"),
        "et elle redevient visible"
    );
}

/// Le verrou de BRANCHEMENT. Sans lui, les témoins ci-dessus valident la règle
/// sans prouver que la route s'en sert : un `let _ = repo.update_name(...)`
/// remis en place les laisserait tous verts.
///
/// L'aiguille est assemblée à l'exécution : écrite en clair, elle figurerait
/// dans ce fichier-ci, et un témoin qui se trouve lui-même ne mesure rien.
#[test]
fn create_zone_n_avale_plus_l_echec_du_renommage() {
    let src = std::fs::read_to_string(std::path::Path::new("src/routes/zones/ecriture.rs"))
        .expect("zones/ecriture.rs doit être lisible depuis la racine du crate");
    let debut = src
        .find("async fn create_zone(")
        .expect("create_zone doit exister");
    let fin = [
        "\nasync fn ",
        "\npub async fn ",
        "\npub(super) async fn ",
        "\npub(crate) async fn ",
        "\nfn ",
        "\npub fn ",
        "\npub(super) fn ",
        "\npub(crate) fn ",
    ]
    .iter()
    .filter_map(|m| src[debut + 1..].find(m))
    .min()
    .map(|i| debut + 1 + i)
    .expect("une fonction doit suivre create_zone — sinon ce test ne borne plus rien");
    let corps = &src[debut..fin];

    let avale = format!("let _ = repo.{}(", "update_name");
    assert!(
        !corps.contains(&avale),
        "le renommage d'une zone ressuscitée est de nouveau avalé : un échec \
         d'écriture ressortirait en `200 OK` avec l'ancienne fiche (#1770, \
         annexe 4)"
    );
    let decide = format!("{}(", "nom_de_zone_existante");
    assert!(
        corps.contains(&decide),
        "create_zone ne passe plus par la règle : l'arbitrage de l'annexe 4 \
         n'a plus d'endroit unique où se poser"
    );
    let dit = format!("{}(", "refus_zone_existe_deja");
    assert!(
        corps.contains(&dit),
        "create_zone ne refuse plus nommément la création sur une zone \
         visible : le silence de #8987 revient"
    );
}
