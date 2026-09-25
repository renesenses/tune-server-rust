//! #4716 — la permission `library` de l'interface hôte WASM : chercher en
//! bibliothèque et y apparier un titre connu.
//!
//! Ce qui manquait à la tranche 1 : aucune capacité d'appariement LOCAL. Un
//! convertisseur de playlists savait aller de la bibliothèque VERS un service,
//! jamais l'inverse — le greffon « Playlists converter » refusait explicitement
//! « transférer vers la bibliothèque ».
//!
//! Le refus deny-by-default de chaque capacité est gardé dans
//! `tune-plugin-runtime-wasm`, là où la permission se lit. Ici on garde
//! l'AUTRE moitié : ce que ces capacités font vraiment contre la base, par
//! [`AppStateHost`] — l'implémentation que tout greffon chargé atteint.
//!
//! Le cœur de ces essais est le deuxième manque : un appariement qui ne rend
//! qu'UN candidat perd le titre dès que ce candidat rate la tolérance de durée
//! que le greffon applique ensuite (±3 s, règle tranchée le 22/09/2026).

use serde_json::Value;

use tune_core::db::backend::ToSqlValue;
use tune_plugin_runtime_wasm::HostContext;
use tune_server::plugins_host::AppStateHost;
use tune_server::state::AppState;

/// La tolérance que le greffon applique APRÈS l'appariement.
const TOLERANCE_DUREE_MS: i64 = 3_000;

/// Une base en mémoire avec un artiste, un album, et les pistes demandées —
/// `(id, titre, durée)`. Les déclencheurs FTS de `tracks` indexent ces
/// insertions : la recherche de l'hôte les retrouve comme en production.
fn base_avec(pistes: &[(i64, &str, i64)]) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState");
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Charles Aznavour')",
            &[],
        )
        .unwrap();
    state
        .backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    for (id, titre, duree_ms) in pistes {
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) \
                 VALUES (?, ?, 1, 1, ?)",
                &[id as &dyn ToSqlValue, titre, duree_ms],
            )
            .unwrap();
    }
    state
}

// ---------------------------------------------------------------------------
// `host_library_search`
// ---------------------------------------------------------------------------

#[test]
fn library_search_rend_de_quoi_apparier() {
    let state = base_avec(&[(1, "La Bohème", 210_000), (2, "Emmenez-moi", 180_000)]);
    let host = AppStateHost::from_state(&state);

    let rendu = host
        .library_search("Aznavour Boheme", 10)
        .expect("library_search");
    assert_eq!(rendu["count"], 1, "une seule piste répond — {rendu}");
    let piste = &rendu["tracks"][0];
    // La fiche est celle de `host_playlist_tracks` : un greffon n'a pas à
    // apprendre deux dialectes pour lire une piste locale.
    assert_eq!(piste["track_id"], 1);
    assert_eq!(piste["title"], "La Bohème");
    assert_eq!(piste["artist_name"], "Charles Aznavour");
    assert_eq!(piste["duration_ms"], 210_000);
    assert!(piste.get("isrc").is_some(), "l'ISRC doit être porté");
}

#[test]
fn library_search_borne_ce_que_le_greffon_demande() {
    let pistes: Vec<(i64, String, i64)> = (1..=5)
        .map(|i| (i, format!("Bohème numéro {i}"), 200_000))
        .collect();
    let pistes: Vec<(i64, &str, i64)> = pistes
        .iter()
        .map(|(i, t, d)| (*i, t.as_str(), *d))
        .collect();
    let state = base_avec(&pistes);
    let host = AppStateHost::from_state(&state);

    // Une limite absurde ne décide pas de la charge : elle est ramenée dans
    // les bornes, jamais transmise telle quelle.
    let rendu = host.library_search("Bohème", 2).expect("library_search");
    assert_eq!(rendu["count"], 2);
    let rendu = host
        .library_search("Bohème", 0)
        .expect("une limite nulle doit être ramenée à 1, pas rendre du vide");
    assert_eq!(rendu["count"], 1);
}

#[test]
fn library_search_refuse_une_requete_vide() {
    let state = base_avec(&[(1, "La Bohème", 210_000)]);
    let host = AppStateHost::from_state(&state);
    let erreur = host
        .library_search("   ", 10)
        .expect_err("une requête vide balaierait la bibliothèque entière");
    assert!(erreur.contains("vide"), "{erreur}");
}

// ---------------------------------------------------------------------------
// `host_library_match_track`
// ---------------------------------------------------------------------------

#[test]
fn library_match_track_apparie_avec_l_appariement_du_serveur() {
    // Accents et « (Remastered 2014) » : seul l'appariement partagé
    // (`track_matcher`) retrouve « La Boheme » là-dedans.
    let state = base_avec(&[
        (1, "Quelque chose d'autre", 200_000),
        (2, "La Bohème (Remastered 2014)", 210_000),
    ]);
    let host = AppStateHost::from_state(&state);

    let rendu = host
        .library_match_track("La Boheme", "Charles Aznavour", "", 210_000)
        .expect("library_match_track");
    assert_eq!(rendu["matched"]["track_id"], 2, "{rendu}");
    assert_eq!(rendu["approximate"], false);
    // La forme est celle de `host_streaming_match_track` : le verdict, puis le
    // classement dont il est la tête.
    assert_eq!(rendu["candidates"][0]["track"]["track_id"], 2);
}

/// 🔴 Le manque que cette tranche répare.
///
/// Deux prises du même titre en bibliothèque. L'appariement désigne la
/// première (palier exact, qui ne regarde pas la durée), mais c'est la seconde
/// qui tient la tolérance de ±3 s. Avec un seul candidat rendu, le greffon
/// déclarait le titre « introuvable » alors que la piste était là.
#[test]
fn library_match_track_rend_un_second_candidat_quand_le_premier_rate_la_duree() {
    let source_ms = 150_000i64;
    let state = base_avec(&[(1, "La Bohème", 210_000), (2, "La Bohème", source_ms + 500)]);
    let host = AppStateHost::from_state(&state);

    let rendu = host
        .library_match_track("La Boheme", "Charles Aznavour", "", source_ms as u64)
        .expect("library_match_track");

    // Le verdict n'a pas bougé : c'est toujours la première prise.
    assert_eq!(rendu["matched"]["track_id"], 1, "{rendu}");
    let ecart_du_verdict = (rendu["matched"]["duration_ms"].as_i64().unwrap() - source_ms).abs();
    assert!(
        ecart_du_verdict > TOLERANCE_DUREE_MS,
        "le verdict doit bien rater la tolérance, sinon l'essai ne prouve rien"
    );

    // Mais le greffon a maintenant un recours.
    let candidats = rendu["candidates"].as_array().expect("une liste");
    assert_eq!(candidats.len(), 2, "{rendu}");
    let retenu = candidats
        .iter()
        .find(|c| {
            (c["track"]["duration_ms"].as_i64().unwrap_or(0) - source_ms).abs()
                <= TOLERANCE_DUREE_MS
        })
        .expect("un candidat doit tenir la tolérance de durée");
    assert_eq!(retenu["track"]["track_id"], 2);
}

#[test]
fn library_match_track_ne_force_aucun_appariement() {
    // Contre-épreuve : rien de proche en bibliothèque ⇒ `matched: null` et un
    // classement VIDE, jamais une piste de dépit.
    let state = base_avec(&[(1, "Totalement autre chose", 200_000)]);
    let host = AppStateHost::from_state(&state);

    let rendu = host
        .library_match_track("La Boheme", "Charles Aznavour", "", 210_000)
        .expect("library_match_track");
    assert_eq!(rendu["matched"], Value::Null, "{rendu}");
    assert_eq!(rendu["count"], 0);
    assert_eq!(rendu["candidates"], serde_json::json!([]));
}

#[test]
fn library_match_track_refuse_un_titre_vide() {
    let state = base_avec(&[(1, "La Bohème", 210_000)]);
    let host = AppStateHost::from_state(&state);
    let erreur = host
        .library_match_track("", "Charles Aznavour", "", 0)
        .expect_err("sans titre, il n'y a rien à apparier");
    assert!(erreur.contains("vide"), "{erreur}");
}
