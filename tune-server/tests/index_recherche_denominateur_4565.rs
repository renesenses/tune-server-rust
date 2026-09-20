//! #4565 — la ligne « Index de recherche » levait un ⚠ PERMANENT sur les
//! artistes, sur une ligne parfaitement saine.
//!
//! Jean Valjean, fil forum 1855 (19/09/2026), 0.9.158, Windows, SQLite,
//! migration 103 — sa fiche système, mot pour mot :
//!
//! ```text
//! ## Library
//! - Albums: 2806
//! - Artists: 1824
//! - Index de recherche : albums 2806/2806, tracks 32532/32532, artists 1826/1824 ⚠
//! ```
//!
//! Albums et pistes tombent juste ; **seuls les artistes dérivent, de +2**. La
//! cause est arithmétique, pas accidentelle :
//!
//! - le numérateur compte `artists_fts`, alimentée par un déclencheur posé sur
//!   **chaque** insertion dans `artists`, sans condition ;
//! - le dénominateur venait de `ArtistRepo::count()`, qui ne compte que les
//!   artistes **porteurs d'au moins un album**
//!   (`WHERE id IN (SELECT DISTINCT artist_id FROM albums …)`).
//!
//! Deux ensembles non comparables : tout serveur portant un seul artiste sans
//! album — piste seule, compilation, featuring, c'est-à-dire l'ordinaire —
//! affichait un ⚠. `albums` et `tracks` s'en sortaient parce que LEURS
//! `count()` sont, eux, non filtrés.
//!
//! C'était le **premier retour de terrain** de l'instrumentation posée par
//! #4319, dont le seul but est de trancher entre « le mot cherché ne
//! correspond pas » et « la ligne manque à l'index ». Un ⚠ qui s'allume tout
//! seul fait l'inverse : le jour où l'index manquera vraiment des artistes,
//! plus personne ne le croira.
//!
//! Ce que ce fichier cloue :
//!
//! 1. ⭐ **le témoin de Jean Valjean** — une base avec un artiste d'album ET un
//!    artiste sans album : plus aucun ⚠, et le dénominateur est bien le nombre
//!    de lignes de `artists` ;
//! 2. la CONTRE-ÉPREUVE, qui prouve que le ⚠ n'a pas simplement été éteint :
//!    une ligne réellement absente de `artists_fts` le rallume ;
//! 3. la ligne dit ce qu'elle compare, pour qu'on ne la relise plus contre le
//!    compteur `Artists:` du dessus ;
//! 4. le compteur `Artists:` de la bibliothèque, lui, n'a PAS changé — il
//!    répond à une autre question et #4565 ne la touche pas.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré en cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

async fn rapport(state: &AppState) -> Value {
    let app = tune_server::routes::router(state.clone());
    let resp = app
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

fn ligne_de_l_index(corps: &Value) -> String {
    corps["markdown"]
        .as_str()
        .unwrap_or_default()
        .lines()
        .find(|l| l.contains("Index de recherche"))
        .unwrap_or("<pas de ligne Index de recherche>")
        .to_string()
}

/// La base de Jean Valjean, en miniature : **un** artiste porteur d'un album,
/// **un** artiste de piste seule. C'est le seul ingrédient nécessaire — +1 au
/// lieu de son +2, la même arithmétique.
fn base_avec_un_artiste_sans_album() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Zubin Mehta'), (2, 'Kathleen Ferrier');\
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Mahler: Symphony No. 2', 1);",
        )
        .unwrap();
    state
}

// --- 1 : ⭐ le témoin -------------------------------------------------

#[tokio::test]
async fn un_artiste_sans_album_ne_leve_plus_d_alerte() {
    let state = base_avec_un_artiste_sans_album();
    let corps = rapport(&state).await;

    // Préalable : l'index contient bien les DEUX artistes — le déclencheur
    // indexe chaque insertion, c'est ce qui produisait l'écart.
    assert_eq!(
        corps["library"]["search_index"]["artists"].as_i64(),
        Some(2),
        "préalable : `artists_fts` indexe TOUS les artistes"
    );
    // Et le compteur de bibliothèque, lui, n'en voit qu'un — c'est sa
    // définition, elle n'est pas en cause.
    assert_eq!(
        corps["library"]["artists"].as_i64(),
        Some(1),
        "préalable : `ArtistRepo::count()` ne compte que les artistes d'album"
    );

    let ligne = ligne_de_l_index(&corps);
    assert!(
        ligne.contains("artists 2/2"),
        "le dénominateur doit être le nombre de lignes de `artists` (2), pas le \
         compteur filtré (1) ; la ligne porte : {ligne}"
    );
    assert!(
        !ligne.contains('⚠'),
        "⭐ AUCUN ⚠ : rien ne manque à l'index. C'est le faux positif de Jean \
         Valjean. La ligne porte : {ligne}"
    );

    // Le dénominateur est aussi lisible en JSON, sans relire le markdown.
    assert_eq!(
        corps["library"]["search_index_total"]["artists"].as_i64(),
        Some(2),
        "le rapport doit PORTER le dénominateur qu'il a utilisé"
    );
}

// --- 2 : la contre-épreuve --------------------------------------------

/// Le ⚠ n'a pas été éteint : il s'allume toujours quand une ligne manque
/// RÉELLEMENT à l'index.
///
/// Sans ce témoin, le précédent serait vert pour la mauvaise raison — un
/// avertissement supprimé passe tous les tests qui vérifient son absence.
#[tokio::test]
async fn contre_epreuve_une_ligne_absente_de_l_index_rallume_l_alerte() {
    let state = base_avec_un_artiste_sans_album();
    // Le déclencheur retiré, la ligne suivante entre dans `artists` et JAMAIS
    // dans `artists_fts` : c'est mot pour mot « la ligne manque à l'index »,
    // le défaut de Tades que #4319 sert à voir (base ancienne, migration
    // partielle). `artists_fts` est contentless (`content=''`) — on ne la vide
    // pas à la main, on reproduit la panne par sa cause.
    state
        .backend
        .execute_batch(
            "DROP TRIGGER artists_fts_insert;\
             INSERT INTO artists (id, name) VALUES (3, 'Otto Klemperer');",
        )
        .unwrap();

    let corps = rapport(&state).await;
    let ligne = ligne_de_l_index(&corps);

    assert!(
        ligne.contains("artists 2/3 ⚠"),
        "un artiste réellement absent de l'index doit RESTER signalé ; la ligne \
         porte : {ligne}"
    );
}

/// Et la même contre-épreuve du côté des albums, dont la ligne était déjà
/// juste : #4565 ne l'a pas abîmée.
#[tokio::test]
async fn contre_epreuve_les_albums_gardent_leur_alerte() {
    let state = base_avec_un_artiste_sans_album();
    state
        .backend
        .execute_batch(
            "DROP TRIGGER albums_fts_insert;\
             INSERT INTO albums (id, title, artist_id) VALUES (2, 'Mahler: Symphony No. 3', 1);",
        )
        .unwrap();

    let corps = rapport(&state).await;
    let ligne = ligne_de_l_index(&corps);

    assert!(
        ligne.contains("albums 1/2 ⚠"),
        "la ligne albums doit rester sensible ; elle porte : {ligne}"
    );
}

// --- 3 & 4 : la ligne se lit, et l'autre compteur ne bouge pas ---------

/// La ligne DIT ce qu'elle compare. C'est la moitié non arithmétique du
/// ticket : le lecteur de la fiche a sous les yeux `Artists: 1824` et
/// `artists 1826/…` à deux lignes d'écart, et rien ne lui disait que les deux
/// chiffres ne répondent pas à la même question.
#[tokio::test]
async fn la_ligne_dit_ce_qu_elle_compare() {
    let state = base_avec_un_artiste_sans_album();
    let corps = rapport(&state).await;
    let ligne = ligne_de_l_index(&corps);

    assert!(
        ligne.contains("(indexées/en base)"),
        "la ligne doit nommer ses deux ensembles ; elle porte : {ligne}"
    );
}

/// Le compteur `Artists:` de la bibliothèque est INCHANGÉ. #4565 corrige la
/// comparaison, il ne redéfinit pas ce que la bibliothèque affiche — celui-là
/// sert ailleurs, et le changer aurait été un autre ticket.
#[tokio::test]
async fn le_compteur_de_bibliotheque_nest_pas_touche() {
    let state = base_avec_un_artiste_sans_album();
    let corps = rapport(&state).await;

    assert_eq!(
        corps["library"]["artists"].as_i64(),
        Some(1),
        "« Artists: » compte toujours les artistes porteurs d'un album"
    );
    let md = corps["markdown"].as_str().unwrap_or_default();
    assert!(
        md.contains("- Artists: 1"),
        "la fiche affiche toujours le même compteur de bibliothèque"
    );
}
