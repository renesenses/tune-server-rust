//! #1770, annexe 4 — le nom demandé à `POST /zones` quand le périphérique a
//! déjà une zone.
//!
//! Ces témoins verrouillent le comportement **d'aujourd'hui**, pas une
//! préférence : l'arbitrage entre « honorer le nom sur une zone visible » et
//! « refuser en 409 » appartient à Bertrand (`keep-open` sur #1770). Le jour où
//! il est rendu, c'est le témoin `une_zone_locale_visible_garde_son_nom` qui
//! rougit — et c'est voulu : il force la conversation au lieu de laisser un
//! contrat client changer en passant.

use crate::state::AppState;
use tune_core::db::zone_repo::ZoneRepo;

use super::ecriture::{NomDeZoneExistante, nom_de_zone_existante};

#[test]
fn le_nom_deja_porte_ne_decide_rien() {
    assert_eq!(
        nom_de_zone_existante(false, "Salon", "Salon"),
        NomDeZoneExistante::DejaLeBon,
        "le nom demandé est déjà celui de la zone : rien à écrire, et surtout \
         rien à signaler comme perdu"
    );
    assert_eq!(
        nom_de_zone_existante(true, "Salon", "Salon"),
        NomDeZoneExistante::DejaLeBon,
        "masquée ou non, un nom identique ne déclenche aucune écriture"
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
fn une_zone_visible_ecarte_le_nom_demande() {
    assert_eq!(
        nom_de_zone_existante(false, "Salon", "Bureau"),
        NomDeZoneExistante::Ecarte,
        "comportement d'aujourd'hui, inchangé — mais nommé, et désormais dit \
         dans le journal (#1770 annexe 4)"
    );
}

/// La même intention utilisateur — « crée une zone sur ce périphérique et
/// appelle-la X » — n'a pas le même effet selon que la zone était masquée ou
/// visible. C'est l'asymétrie que décrit l'annexe 4, mesurée par la route.
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
        axum::http::StatusCode::OK,
        "le code d'état ne change PAS : trancher entre 200 et 409 est \
         l'arbitrage réservé à Bertrand (#1770, `keep-open`)"
    );
    assert_eq!(
        repo.get(id).unwrap().unwrap().name,
        "Salon",
        "le nom demandé est écarté sur une zone visible — comportement \
         d'aujourd'hui. Si ce témoin rougit, l'arbitrage de l'annexe 4 a été \
         tranché : dites-le sur #1770 avant de le corriger"
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
    let dit = format!("\"zone_existante_nom_demande_{}\"", "ecarte");
    assert!(
        corps.contains(&dit),
        "la perte du nom sur une zone visible est redevenue silencieuse — \
         c'est précisément le défaut de l'annexe 4"
    );
}
