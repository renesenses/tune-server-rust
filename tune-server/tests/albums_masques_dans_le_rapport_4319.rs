//! #4319 — l'album MASQUÉ, le mécanisme que le rapport ne savait pas nommer.
//!
//! Tades, fil forum 1817 (16/09/2026), 212 372 pistes : « quand je regarde
//! répertoire j'ai bien 2 albums Mahler par Mehta la 2 et la 3 ; quand je fais
//! une recherche je ne trouve que la 2 ».
//!
//! L'instruction du ticket a réduit le champ à quatre mécanismes, tous côté
//! données. L'un d'eux produit **littéralement** cette phrase :
//!
//! - `hidden_albums_excluded()` (`tune-core/src/db/facet_filter.rs`) retire
//!   l'album de la liste de bibliothèque **et** de `AlbumRepo::search_page` ;
//! - `browse_directory` (`tune-server/src/routes/library/browse.rs`) liste les
//!   sous-dossiers depuis le **disque** et n'applique aucun de ces filtres.
//!
//! Un album masqué est donc invisible à la recherche et toujours visible dans
//! Répertoires. La ligne « Index de recherche » posée par #4428 ne peut pas
//! l'attraper : elle compte des lignes INDEXÉES, or ce filtre s'applique
//! **après** l'index — un album masqué reste parfaitement indexé.
//!
//! Ce fichier cloue la ligne qui manquait. Il ne corrige aucun défaut de
//! recherche : il rend une des quatre causes lisible dans le rapport que le
//! testeur colle, sans aller-retour avec lui.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier a sa propre cible
//! `[[test]]` dans `tune-server/Cargo.toml`, sans quoi il ne serait JAMAIS
//! compilé.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

async fn rapport(state: &tune_server::state::AppState) -> Value {
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

/// Bibliothèque sans rien de masqué : la ligne est là et dit **zéro**.
///
/// Zéro mesuré n'est pas la même chose qu'aucune mesure : sans la ligne, le
/// lecteur du rapport ne peut pas écarter la piste, il peut seulement
/// l'ignorer.
#[tokio::test]
async fn le_rapport_annonce_les_albums_masques_meme_a_zero() {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let corps = rapport(&state).await;

    assert_eq!(
        corps["library"]["hidden_albums"].as_i64(),
        Some(0),
        "le rapport doit porter le compte d'albums masqués : {corps}"
    );

    let md = corps["markdown"].as_str().unwrap_or_default();
    assert!(
        md.contains("- Albums masqués : 0"),
        "le rapport COLLÉ doit porter la ligne ; il porte : {}",
        md.lines()
            .find(|l| l.contains("Albums masqués"))
            .unwrap_or("<aucune ligne « Albums masqués »>")
    );
}

/// ⭐ Le cœur du ticket : un album masqué DISPARAÎT de la recherche, et le
/// rapport le dit désormais.
///
/// Le test mesure les deux faits dans la même base, l'un après l'autre :
/// d'abord que masquer retire bien l'album de `/library/albums` (la propriété
/// qui rend le mécanisme plausible chez Tades), ensuite que le rapport donne le
/// nombre qui permet de le soupçonner.
#[tokio::test]
async fn un_album_masque_sort_de_la_bibliotheque_et_le_rapport_le_compte() {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Mahler Mehta');\
             INSERT INTO albums (id, title, artist_id) VALUES \
               (1, 'Mahler: Symphony No. 2', 1), (2, 'Mahler: Symphony No. 3', 1);",
        )
        .unwrap();

    // Les deux albums sont là, et la recherche les voit.
    let avant = rapport(&state).await;
    assert_eq!(avant["library"]["albums"].as_i64(), Some(2));
    assert_eq!(
        avant["library"]["hidden_albums"].as_i64(),
        Some(0),
        "rien n'est masqué à ce stade"
    );

    // La Symphonie n° 3 est masquée — le geste exact de `hidden_items`.
    state
        .backend
        .execute_batch(
            "INSERT INTO hidden_items (item_type, item_id) \
             SELECT 'album', id FROM albums WHERE title = 'Mahler: Symphony No. 3';",
        )
        .unwrap();

    let apres = rapport(&state).await;
    assert_eq!(
        apres["library"]["hidden_albums"].as_i64(),
        Some(1),
        "le rapport doit compter l'album masqué : {apres}"
    );

    let md = apres["markdown"].as_str().unwrap_or_default();
    assert!(
        md.contains("- Albums masqués : 1"),
        "le rapport COLLÉ doit porter le compte ; il porte : {}",
        md.lines()
            .find(|l| l.contains("Albums masqués"))
            .unwrap_or("<aucune ligne « Albums masqués »>")
    );
    // Le libellé doit nommer la CONSÉQUENCE : un nombre nu ne dit pas au
    // lecteur pourquoi il explique « visible dans Répertoires, introuvable à la
    // recherche ».
    assert!(
        md.contains("toujours visibles dans Répertoires"),
        "la ligne doit dire ce que masquer implique ; markdown : {}",
        md.lines()
            .find(|l| l.contains("Albums masqués"))
            .unwrap_or("<aucune ligne « Albums masqués »>")
    );
}
