//! Les recommandations « IA » sur PostgreSQL (reste de #5005).
//!
//! `tune-core/src/ai/recommendations.rs` écrivait son SQL pour SQLite seul :
//!
//! - `t.artist_id = CAST(a.id AS TEXT)` (et `album_id`) : SQLite compare par
//!   affinité, PostgreSQL refuse `bigint = text` (« operator does not exist ») ;
//! - `listened_at > datetime('now', '-7 days')` : `datetime` n'existe pas sur
//!   PostgreSQL.
//!
//! (`date(h1.listened_at) = date(h2.listened_at)`, dans la co-écoute de la
//! radio, reste tel quel : PostgreSQL le lit comme une conversion `text → date`
//! — mesuré, le témoin reste vert avec lui.)
//!
//! Chaque requête est dans un `if let Ok(..)` ou un `.unwrap_or_default()` :
//! rien ne se voit, sinon que les recommandations retombent sur le tirage au
//! hasard (« discovery »), que les « Daily mixes » sortent vides et que la
//! radio intelligente (`POST /ai/smart-radio`, et l'auto-DJ qui l'appelle)
//! ignore sa graine.
//!
//! Le même scénario tourne sur SQLite (suite ordinaire) et sur PostgreSQL
//! (`TUNE_TEST_PG_URL`).

use tune_core::ai::recommendations::{
    RecommendedTrack, generate_daily_mixes, get_recommendations, smart_radio,
};
use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

const GENRE: &str = "SondeRecoGenre";
const AUTRE_GENRE: &str = "SondeRecoAutre";
const GRAINE: &str = "Sonde Reco Graine";
const VOISIN: &str = "Sonde Reco Voisin";

fn scalaire(state: &AppState, sql: &str) -> i64 {
    state
        .backend
        .query_one(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"))
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

fn exec(state: &AppState, sql: &str, params: &[&dyn ToSqlValue]) {
    state
        .backend
        .execute(sql, params)
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

/// Une piste de l'artiste `artiste`, dans l'album `album`, au genre `genre`.
fn piste(state: &AppState, titre: &str, artiste: i64, album: i64, genre: &str) -> i64 {
    let chemin = format!("/sonde-reco/{titre}.flac");
    exec(
        state,
        "INSERT INTO tracks (title, album_id, artist_id, genre, duration_ms, file_path, source) \
         VALUES (?, ?, ?, ?, 200000, ?, 'local')",
        &[&titre, &album, &artiste, &genre, &chemin],
    );
    scalaire(
        state,
        &format!("SELECT MAX(id) FROM tracks WHERE file_path = '{chemin}'"),
    )
}

fn ecoute(state: &AppState, piste: i64, titre: &str, artiste: &str, quand: &str) {
    exec(
        state,
        "INSERT INTO listen_history (track_id, title, artist_name, source, listened_at) \
         VALUES (?, ?, ?, 'local', ?)",
        &[&piste, &titre, &artiste, &quand],
    );
}

fn iso(il_y_a: chrono::Duration) -> String {
    (chrono::Utc::now() - il_y_a)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn resume(pistes: &[RecommendedTrack]) -> Vec<String> {
    pistes
        .iter()
        .map(|p| format!("{} {:?} [{}]", p.track_id, p.artist, p.reason))
        .collect()
}

fn scenario(state: &AppState) -> Vec<String> {
    let mut ecarts = Vec::new();
    let backend = &state.backend;

    // ── Bibliothèque : une graine (6 pistes, un genre), un voisin (4 pistes,
    // un autre genre) ──
    for nom in [GRAINE, VOISIN] {
        exec(state, "INSERT INTO artists (name) VALUES (?)", &[&nom]);
    }
    let graine = scalaire(
        state,
        &format!("SELECT MAX(id) FROM artists WHERE name = '{GRAINE}'"),
    );
    let voisin = scalaire(
        state,
        &format!("SELECT MAX(id) FROM artists WHERE name = '{VOISIN}'"),
    );
    for (titre, artiste) in [
        ("Sonde Reco Album G", graine),
        ("Sonde Reco Album V", voisin),
    ] {
        exec(
            state,
            "INSERT INTO albums (title, artist_id, source) VALUES (?, ?, 'local')",
            &[&titre, &artiste],
        );
    }
    let album_g = scalaire(
        state,
        "SELECT MAX(id) FROM albums WHERE title = 'Sonde Reco Album G'",
    );
    let album_v = scalaire(
        state,
        "SELECT MAX(id) FROM albums WHERE title = 'Sonde Reco Album V'",
    );
    let g: Vec<i64> = (1..=6)
        .map(|i| piste(state, &format!("sonde-reco-g{i}"), graine, album_g, GENRE))
        .collect();
    let v: Vec<i64> = (1..=4)
        .map(|i| {
            piste(
                state,
                &format!("sonde-reco-v{i}"),
                voisin,
                album_v,
                AUTRE_GENRE,
            )
        })
        .collect();

    // ── Historique : g1 écoutée il y a une heure (donc « récente », à exclure
    // des recommandations) ; g1..g4 et v1 écoutées il y a 60 jours, le même
    // jour (« Rediscover », et co-écoute graine → voisin). Assez d'écoutes pour
    // que le genre et l'artiste tiennent le haut du classement même dans une
    // base partagée par d'autres étapes. ──
    let recente = iso(chrono::Duration::hours(1));
    ecoute(state, g[0], "sonde-reco-g1", GRAINE, &recente);
    for jour in 0..10 {
        let ancienne = iso(chrono::Duration::days(60) + chrono::Duration::minutes(jour));
        for (i, id) in g.iter().take(4).enumerate() {
            ecoute(
                state,
                *id,
                &format!("sonde-reco-g{}", i + 1),
                GRAINE,
                &ancienne,
            );
        }
        ecoute(state, v[0], "sonde-reco-v1", VOISIN, &ancienne);
    }

    // ── 1. get_recommendations : le genre et l'artiste écoutés, sans la
    // piste récente ──
    let recos = get_recommendations(backend, None, 20);
    if !recos.iter().any(|r| r.reason == "genre match") {
        ecarts.push(format!(
            "recommandations sans « genre match » : {:?}",
            resume(&recos)
        ));
    }
    if recos.iter().any(|r| r.track_id == g[0]) {
        ecarts.push(format!(
            "recommandations : la piste écoutée il y a une heure revient ({}) : {:?}",
            g[0],
            resume(&recos)
        ));
    }
    for r in recos.iter().filter(|r| g.contains(&r.track_id)) {
        if r.artist.as_deref() != Some(GRAINE) || r.album.as_deref() != Some("Sonde Reco Album G") {
            ecarts.push(format!(
                "recommandation {} sans son artiste ou son album : {:?} / {:?}",
                r.track_id, r.artist, r.album
            ));
        }
    }

    // ── 2. generate_daily_mixes : un mix du genre, et « Rediscover » ──
    let mixes = generate_daily_mixes(backend);
    let noms: Vec<String> = mixes.iter().map(|m| m.name.clone()).collect();
    match mixes.iter().find(|m| m.name == format!("{GENRE} Mix")) {
        None => ecarts.push(format!("pas de « {GENRE} Mix » : {noms:?}")),
        Some(m) => {
            if m.tracks.iter().any(|t| t.artist.as_deref() != Some(GRAINE)) {
                ecarts.push(format!(
                    "« {GENRE} Mix » sans son artiste : {:?}",
                    resume(&m.tracks)
                ));
            }
        }
    }
    match mixes.iter().find(|m| m.name == "Rediscover") {
        None => ecarts.push(format!("pas de mix « Rediscover » : {noms:?}")),
        Some(m) => {
            if m.tracks.iter().any(|t| t.track_id == g[0]) {
                ecarts.push(format!(
                    "« Rediscover » reprend la piste écoutée il y a une heure : {:?}",
                    resume(&m.tracks)
                ));
            }
            if !m.tracks.iter().any(|t| t.track_id == g[1]) {
                ecarts.push(format!(
                    "« Rediscover » sans g2, écoutée il y a 60 jours : {:?}",
                    resume(&m.tracks)
                ));
            }
        }
    }

    // ── 3. smart_radio depuis g2 : même genre, puis l'artiste co-écouté ──
    let radio = smart_radio(backend, Some(g[1]), None, None, 30);
    let meme_genre: Vec<i64> = radio
        .iter()
        .filter(|r| r.reason == "same genre")
        .map(|r| r.track_id)
        .collect();
    if meme_genre.is_empty() || meme_genre.iter().any(|id| !g.contains(id)) {
        ecarts.push(format!(
            "radio depuis g2 : « same genre » absent ou hors du genre : {:?}",
            resume(&radio)
        ));
    }
    if !radio
        .iter()
        .any(|r| r.reason == "artist co-occurrence" && v.contains(&r.track_id))
    {
        ecarts.push(format!(
            "radio depuis g2 : l'artiste co-écouté manque : {:?}",
            resume(&radio)
        ));
    }

    ecarts
}

#[tokio::test(flavor = "multi_thread")]
async fn recommandations_ia_sur_sqlite() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    let ecarts = scenario(&state);
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_recommandations_ia() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    // Une base partagée par les étapes : on repart sans les lignes « sonde ».
    for sql in [
        "DELETE FROM listen_history WHERE title LIKE 'sonde-reco-%'",
        "DELETE FROM tracks WHERE file_path LIKE '/sonde-reco/%'",
        "DELETE FROM albums WHERE title LIKE 'Sonde Reco Album %'",
        "DELETE FROM artists WHERE name LIKE 'Sonde Reco %'",
    ] {
        state.backend.execute(sql, &[]).unwrap();
    }
    let ecarts = scenario(&state);
    assert!(ecarts.is_empty(), "PostgreSQL : {ecarts:#?}");
}
