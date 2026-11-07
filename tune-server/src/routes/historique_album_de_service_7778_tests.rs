//! #7778, point 6 — fil 2637 (rc10) : dans l'Historique, un titre de service
//! lancé hors de son album (titre phare, playlist, file) n'a plus de lien
//! vers l'album.
//!
//! Le témoin lit le JSON de `GET /library/history`, pas la structure du dépôt
//! de données : `ListenRecord.album_ref` est hors de la sérialisation, la
//! route doit poser elle-même `album_id_service`, comme la file (fil 2143).
//!
//! Contre-épreuve : retirer la ligne `value["album_id_service"] = …` de
//! `recent_history` fait tomber
//! `la_route_historique_sert_l_album_du_service_d_un_titre_lance_hors_album_7778`.

use super::*;

fn ligne<'a>(items: &'a [Value], titre: &str) -> &'a Value {
    items
        .iter()
        .find(|i| i["title"] == titre)
        .unwrap_or_else(|| panic!("écoute « {titre} » absente de {items:?}"))
}

#[tokio::test]
async fn la_route_historique_sert_l_album_du_service_d_un_titre_lance_hors_album_7778() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO listen_history (title, source, source_id, duration_ms, context_type, context_id, context_source, album_ref) VALUES \
             ('Titre phare', 'qobuz', 'titre-qobuz-7778', 240000, 'artist', 'artiste-7778', 'qobuz', 'album-qobuz-7778'), \
             ('Sans album', 'qobuz', 'autre-qobuz-7778', 200000, NULL, NULL, NULL, NULL), \
             ('Titre local', 'local', NULL, 180000, NULL, NULL, NULL, 'ne-pas-servir')",
            &[],
        )
        .unwrap();

    let Json(body) = recent_history(
        State(state),
        Query(HistoryParams {
            limit: Some(20),
            offset: Some(0),
            period: None,
        }),
    )
    .await;
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 3, "{body}");

    let phare = ligne(items, "Titre phare");
    assert_eq!(
        phare["album_id_service"], "album-qobuz-7778",
        "#7778 : GET /library/history doit servir l'album du service d'un titre lancé hors \
         de son album, sans quoi le client ne peut pas proposer « Aller à l'album » : {phare}"
    );
    assert_eq!(phare["context_source"], "qobuz", "{phare}");

    // Rien n'est inventé : ni pour un titre sans référence, ni pour un local.
    assert_eq!(ligne(items, "Sans album")["album_id_service"], Value::Null);
    assert_eq!(ligne(items, "Titre local")["album_id_service"], Value::Null);

    // La référence interne de resignature reste hors du JSON (fil 2121).
    for item in items {
        assert!(
            item.get("album_ref").is_none(),
            "`album_ref` ne doit pas apparaître dans l'historique : {item}"
        );
        assert!(
            item.get("album_id_service").is_some(),
            "clé album_id_service : {item}"
        );
    }
}
