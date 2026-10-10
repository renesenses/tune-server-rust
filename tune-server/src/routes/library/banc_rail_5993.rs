//! #5993 — BANC du rail des facettes, socle compris (#5977). `#[ignore]` :
//! il bâtit une base FICHIER de ≈ 104 000 pistes et ne se lance qu'à la main,
//! en `--release` :
//!
//! ```text
//! cargo test --release -p tune-server --lib banc_rail_5993 -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Il ne garde rien : il MESURE, médiane de 7 appels après un appel de
//! chauffe, les routes que la vue Oxygen appelle à l'ouverture. Le même
//! fichier sert à mesurer AVANT (sans socle complet) et APRÈS.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::state::AppState;

const ARTISTES: i64 = 600;
const LOCAUX: i64 = 5_100;
const DISTANTS: i64 = 156;
const DOUBLES: i64 = 90;
const PISTES: i64 = 19;
const GENRES: [&str; 12] = [
    "Rock",
    "Jazz",
    "Classical",
    "Electronic",
    "Pop",
    "Metal",
    "Folk",
    "Blues",
    "Soul",
    "Hip-Hop",
    "Ambient",
    "World",
];

fn remplir(state: &AppState) {
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=ARTISTES {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    let artiste_de = |al: i64| al % ARTISTES + 1;
    for al in 1..=LOCAUX + DISTANTS {
        let k = al - LOCAUX;
        let (titre, source, artiste) = if k <= 0 {
            (format!("Album {al}"), "local", artiste_de(al))
        } else if k <= DOUBLES {
            // Le même disque servi par un serveur UPnP : doublé par un local.
            let local = k * 50;
            (format!("Album {local}"), "upnp", artiste_de(local))
        } else {
            (format!("Distant {al}"), "upnp", artiste_de(al))
        };
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({al}, '{titre}', {artiste}, '{source}');\n"
        ));
    }
    let mut id = 0_i64;
    let mut piste = |sql: &mut String, al: Option<i64>, n: i64, format: &str| {
        id += 1;
        let (album, artiste, source, chemin, g, an, lab) = match al {
            Some(al) => {
                let a = if al > LOCAUX {
                    artiste_de((al - LOCAUX) * 50)
                } else {
                    artiste_de(al)
                };
                let source = if al > LOCAUX { "upnp" } else { "local" };
                let chemin = if al > LOCAUX {
                    format!("http://nas:9000/{al}/{n:02}.{format}")
                } else {
                    format!("/banc/Artiste {a}/Album {al}/{n:02}.{format}")
                };
                (
                    al.to_string(),
                    a,
                    source,
                    chemin,
                    GENRES[(al % 12) as usize],
                    1960 + al % 60,
                    format!("Label {}", al % 45),
                )
            }
            None => (
                "NULL".into(),
                n % ARTISTES + 1,
                "local",
                format!("/banc/vrac/{id}.{format}"),
                "Pop",
                2000,
                "Label 0".into(),
            ),
        };
        let (sr, bits) = match id % 5 {
            0 => (96000, 24),
            1 => (192000, 24),
            _ => (44100, 16),
        };
        let bits = if format == "mp3" {
            "NULL".to_string()
        } else {
            bits.to_string()
        };
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, duration_ms, format, sample_rate, bit_depth, source, album_artist, \
             genre, year, label, composer) \
             VALUES ({id}, 'Piste {n}', {album}, {artiste}, 1, {n}, '{chemin}', 240000, \
             '{format}', {sr}, {bits}, '{source}', '', '{g}', {an}, '{lab}', 'Compositeur {c}');\n",
            c = artiste % 120
        ));
        if id % 3 == 0 {
            sql.push_str(&format!(
                "INSERT INTO track_metadata (track_id, key, value) VALUES ({id}, 'release_country', 'C{}');\n",
                id % 20
            ));
        }
        if id % 4 == 0 {
            sql.push_str(&format!(
                "INSERT INTO track_metadata (track_id, key, value) VALUES ({id}, 'mood', 'M{}');\n",
                id % 9
            ));
        }
    };
    for al in 1..=LOCAUX + DISTANTS {
        for n in 1..=PISTES {
            piste(&mut sql, Some(al), n, "flac");
        }
        if al % 500 == 0 {
            sql.push_str("COMMIT;");
            let t = Instant::now();
            state.backend.execute_batch(&sql).unwrap();
            eprintln!("BANC remplissage album {al} lot={:?}", t.elapsed());
            sql = String::from("BEGIN;\n");
        }
    }
    // Copies MP3 de moindre qualité (#4101), un album sur 40.
    for al in (1..=LOCAUX).step_by(40) {
        for n in 1..=PISTES {
            piste(&mut sql, Some(al), n, "mp3");
        }
    }
    for n in 1..=60 {
        piste(&mut sql, None, n, "flac");
    }
    for al in (5..=LOCAUX).step_by(211) {
        sql.push_str(&format!(
            "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', {al});\n"
        ));
    }
    sql.push_str("COMMIT;");
    state.backend.execute_batch(&sql).unwrap();
    state.backend.execute_batch("ANALYZE;").unwrap();
}

async fn une_fois(app: &axum::Router, uri: &str) -> Duration {
    let debut = Instant::now();
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let _ = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    debut.elapsed()
}

async fn mediane(app: &axum::Router, uri: &str) -> (Duration, Duration, Duration) {
    une_fois(app, uri).await;
    let mut d = Vec::new();
    for _ in 0..7 {
        d.push(une_fois(app, uri).await);
    }
    d.sort();
    (d[3], d[0], d[6])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "banc manuel, --release"]
async fn banc_rail_5993() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("banc.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    let t = Instant::now();
    remplir(&state);
    let n = state
        .backend
        .query_many("SELECT COUNT(*) FROM tracks", &[])
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    eprintln!("BANC pistes={n} remplissage={:?}", t.elapsed());
    let app = crate::routes::router(state.clone());
    let p = "/api/v1/library";
    for (nom, uri) in [
        (
            "rail10",
            format!(
                "{p}/facets?fields=genre,label,year,artist,format,sample_rate,bit_depth,composer,country,mood&limit=200"
            ),
        ),
        (
            "rail10_genre",
            format!(
                "{p}/facets?fields=genre,label,year,artist,format,sample_rate,bit_depth,composer,country,mood&limit=200&genre=Jazz"
            ),
        ),
        ("defaut", format!("{p}/facets")),
        ("albums500", format!("{p}/albums-detailed?limit=500")),
        ("dossiers", format!("{p}/folder-facet")),
        ("dossiers_banc", format!("{p}/folder-facet?path=/banc")),
        (
            "dossiers_artiste",
            format!("{p}/folder-facet?path=/banc/Artiste%201"),
        ),
        ("tracks200", format!("{p}/tracks?limit=200")),
    ] {
        let (m, lo, hi) = mediane(&app, &uri).await;
        eprintln!(
            "BANC {nom:<18} mediane={:>7.1} ms  min={:>7.1}  max={:>7.1}",
            m.as_secs_f64() * 1e3,
            lo.as_secs_f64() * 1e3,
            hi.as_secs_f64() * 1e3
        );
        // À FROID : une écriture juste avant chaque appel périme tout ce qui
        // serait gardé en mémoire (#5993).
        let _ = state
            .backend
            .execute_batch("CREATE TABLE IF NOT EXISTS banc_froid (x INTEGER);");
        let mut d = Vec::new();
        for _ in 0..7 {
            state
                .backend
                .execute_batch("INSERT INTO banc_froid VALUES (1);")
                .unwrap();
            d.push(une_fois(&app, &uri).await);
        }
        d.sort();
        eprintln!(
            "BANC {:<18} mediane={:>7.1} ms  min={:>7.1}  max={:>7.1}",
            format!("{nom}_froid"),
            d[3].as_secs_f64() * 1e3,
            d[0].as_secs_f64() * 1e3,
            d[6].as_secs_f64() * 1e3
        );
    }
}

/// Où va le temps du socle résolu (#5993) : chaque morceau mesuré à part.
#[test]
#[ignore = "banc manuel, --release"]
fn profil_socle_5993() {
    use tune_core::db::engine::Engine;
    use tune_core::db::facet_filter as ff;
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("banc.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    remplir(&state);
    let db = &state.backend;
    let chrono = |nom: &str, sql: &str| {
        let _ = db.query_many(sql, &[]).unwrap();
        let mut d = Vec::new();
        let mut n = 0;
        for _ in 0..7 {
            let t = Instant::now();
            n = db.query_many(sql, &[]).unwrap().len();
            d.push(t.elapsed());
        }
        d.sort();
        eprintln!(
            "PROFIL {nom:<34} mediane={:>7.1} ms  lignes={n}",
            d[3].as_secs_f64() * 1e3
        );
    };
    let e = Engine::Sqlite;
    let resolu = ff::sql_pistes_ecartees_par_le_socle(e);
    chrono("socle_resolu (2 branches)", &resolu);
    let (b1, b2) = resolu.split_once(" UNION ").unwrap();
    chrono("branche double distant", b1);
    chrono("branche copie", b2);
    let ids: Vec<String> = db
        .query_many(&resolu, &[])
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .map(|i| i.to_string())
        .collect();
    let facette = |extra: &str| {
        format!(
            "SELECT t.year, COUNT(*) AS n FROM tracks t WHERE t.year IS NOT NULL \
             AND CAST(t.year AS TEXT) <> '' AND {}{extra} GROUP BY t.year ORDER BY n DESC LIMIT 200",
            ff::hidden_tracks_excluded()
        )
    };
    chrono("facette year, masques seuls", &facette(""));
    chrono(
        "facette year + NOT IN ids",
        &facette(&format!(" AND t.id NOT IN ({})", ids.join(","))),
    );
    let [_, p2, p3] = ff::socle_de_la_vue_des_pistes(e);
    chrono(
        "facette year + fragments SQL",
        &facette(&format!(" AND {p2} AND {p3}")),
    );
}

const RAIL10: &str = "genre,label,year,artist,format,sample_rate,bit_depth,composer,country,mood";

/// #5993 — le socle complet (#5977) ne doit pas coûter plus de 20 % au rail
/// d'Oxygen, ni aux cartes d'albums, ni à la facette Dossiers : on compare,
/// sur la MÊME base et en alternance, chaque lecture réelle à la même lecture
/// ne posant que les albums masqués (le coût d'avant #5977) — à chaud (le
/// seuil), puis à froid (une écriture avant chaque appel périme le socle gardé
/// en mémoire ; mesuré, non gardé).
#[test]
#[ignore = "banc manuel, --release"]
fn rail_socle_sous_20_pourcent_5993() {
    use super::albums_detailed::{lire_les_cartes, lire_les_cartes_sur};
    use super::facets::{FacetQuery, SocleResolu, compter_avec_le_socle, compter_les_facettes};
    use super::folder_facet::{FolderPathQuery, lire_les_dossiers, lire_les_dossiers_sur};
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("banc.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    remplir(&state);
    state
        .backend
        .execute_batch("CREATE TABLE IF NOT EXISTS banc_froid (x INTEGER);")
        .unwrap();
    let rail = || {
        serde_json::from_value::<FacetQuery>(serde_json::json!({"fields": RAIL10, "limit": 200}))
            .unwrap()
            .hydrate(None)
            .ok()
            .expect("requête de rail")
    };
    let cartes = || {
        serde_json::from_value::<FacetQuery>(serde_json::json!({"limit": 500}))
            .unwrap()
            .hydrate(None)
            .ok()
            .expect("requête de cartes")
    };
    let dossier_de = |chemin: &str| FolderPathQuery {
        path: Some(chemin.to_string()),
        ..Default::default()
    };
    let masques_seuls = SocleResolu::masques_seuls();
    type Lecture<'a> = Box<dyn Fn() -> serde_json::Value + 'a>;
    let cas: Vec<(&str, Lecture, Lecture)> = vec![
        (
            "rail10",
            Box::new(|| compter_les_facettes(&state, rail())),
            Box::new(|| compter_avec_le_socle(&state, rail(), &masques_seuls)),
        ),
        (
            "albums500",
            Box::new(|| lire_les_cartes(&state, cartes())),
            Box::new(|| lire_les_cartes_sur(&state, cartes(), &masques_seuls)),
        ),
        (
            "dossiers_banc",
            Box::new(|| lire_les_dossiers(&state, FacetQuery::default(), dossier_de("/banc"))),
            Box::new(|| {
                lire_les_dossiers_sur(
                    &state,
                    FacetQuery::default(),
                    dossier_de("/banc"),
                    &masques_seuls,
                )
            }),
        ),
        (
            "dossiers_artiste",
            Box::new(|| {
                lire_les_dossiers(&state, FacetQuery::default(), dossier_de("/banc/Artiste 1"))
            }),
            Box::new(|| {
                lire_les_dossiers_sur(
                    &state,
                    FacetQuery::default(),
                    dossier_de("/banc/Artiste 1"),
                    &masques_seuls,
                )
            }),
        ),
    ];
    let mut depassements = Vec::new();
    for froid in [false, true] {
        for (nom, avec_socle, sans_socle) in &cas {
            let _ = avec_socle();
            let _ = sans_socle();
            let (mut avec, mut sans) = (Vec::new(), Vec::new());
            for _ in 0..9 {
                if froid {
                    state
                        .backend
                        .execute_batch("INSERT INTO banc_froid VALUES (1);")
                        .unwrap();
                }
                let t = Instant::now();
                let _ = sans_socle();
                sans.push(t.elapsed());
                if froid {
                    state
                        .backend
                        .execute_batch("INSERT INTO banc_froid VALUES (1);")
                        .unwrap();
                }
                let t = Instant::now();
                let _ = avec_socle();
                avec.push(t.elapsed());
            }
            avec.sort();
            sans.sort();
            let ratio = avec[4].as_secs_f64() / sans[4].as_secs_f64();
            let etat = if froid { "froid" } else { "chaud" };
            eprintln!(
                "BANC ratio {nom:<17} {etat} socle complet / masques seuls = {ratio:.3} ({:?} / {:?})",
                avec[4], sans[4]
            );
            // Le seuil porte sur le cas courant : le socle gardé en mémoire.
            // À froid, la résolution du socle de toute la bibliothèque
            // (≈ 30 à 45 ms ici) reste due : elle est mesurée et publiée, pas
            // gardée.
            if !froid && ratio >= 1.20 {
                depassements.push(format!("{nom} {etat} +{:.0} %", (ratio - 1.0) * 100.0));
            }
        }
    }
    assert!(
        depassements.is_empty(),
        "le socle complet coûte plus de 20 % : {depassements:?}"
    );
}

/// #5993 — pistes pour la résolution du socle et la sonde par ligne.
#[test]
#[ignore = "banc manuel, --release"]
fn essais_socle_5993() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("banc.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    remplir(&state);
    let db = &state.backend;
    let chrono = |nom: &str, sql: &str| {
        let _ = db.query_many(sql, &[]).unwrap();
        let mut d = Vec::new();
        let mut n = 0;
        for _ in 0..7 {
            let t = Instant::now();
            n = db.query_many(sql, &[]).unwrap().len();
            d.push(t.elapsed());
        }
        d.sort();
        eprintln!(
            "ESSAI {nom:<40} mediane={:>7.1} ms  lignes={n}",
            d[3].as_secs_f64() * 1e3
        );
    };
    let plan = |sql: &str| {
        for r in db
            .query_many(&format!("EXPLAIN QUERY PLAN {sql}"), &[])
            .unwrap()
        {
            eprintln!("PLAN {:?}", r.last().and_then(|v| v.as_string()));
        }
    };
    let cle = "album_id, COALESCE(disc_number, 1), COALESCE(track_number, 0), LOWER(TRIM(COALESCE(title, '')))";
    let a = format!(
        "SELECT album_id FROM tracks WHERE album_id IS NOT NULL GROUP BY {cle} HAVING COUNT(*) > 1"
    );
    plan(&a);
    chrono("A group by cle", &a);
    let a2 = format!(
        "SELECT DISTINCT album_id FROM (SELECT {cle}, COUNT(*) AS n FROM tracks INDEXED BY idx_tracks_cle_de_copie WHERE album_id IS NOT NULL GROUP BY {cle}) WHERE n > 1"
    );
    plan(&a2);
    chrono("A2 indexed by", &a2);
    let c = format!(
        "SELECT DISTINCT album_id FROM (SELECT album_id, \
         ({cle}) IS (LAG({cle}) OVER (ORDER BY {cle})) AS meme FROM tracks WHERE album_id IS NOT NULL) WHERE meme"
    );
    let _ = c;
    let b = "SELECT album_id FROM tracks WHERE album_id IS NOT NULL \
             GROUP BY album_id, COALESCE(disc_number, 1), COALESCE(track_number, 0) HAVING COUNT(*) > 1";
    plan(b);
    chrono("B group by entiers seuls", b);
    let cnt = "SELECT album_id, COUNT(*) FROM tracks WHERE album_id IS NOT NULL GROUP BY album_id";
    plan(cnt);
    chrono("compte par album (idx album)", cnt);
    let base = "SELECT t.year, COUNT(*) FROM tracks t GROUP BY t.year";
    chrono("scan year sans sonde", base);
    let albums: Vec<String> = db
        .query_many(&a, &[])
        .unwrap()
        .iter()
        .filter_map(|r| r[0].as_i64())
        .map(|i| i.to_string())
        .collect();
    chrono(
        "scan year + album NOT IN (cand)",
        &format!(
            "SELECT t.year, COUNT(*) FROM tracks t WHERE t.album_id NOT IN ({}) GROUP BY t.year",
            albums.join(",")
        ),
    );
    let ids: Vec<String> = (0..4142).map(|i| (i * 24 + 7).to_string()).collect();
    chrono(
        "scan year + id NOT IN (4142)",
        &format!(
            "SELECT t.year, COUNT(*) FROM tracks t WHERE t.id NOT IN ({}) GROUP BY t.year",
            ids.join(",")
        ),
    );
    chrono(
        "scan year + col entière = 0",
        "SELECT t.year, COUNT(*) FROM tracks t WHERE COALESCE(t.track_number, 0) >= 0 GROUP BY t.year",
    );
}

/// #5993 — où passe le temps des cartes d'albums corrigées par album. Garde
/// la base dans `BANC_5993_BASE` si la variable est posée.
#[test]
#[ignore = "banc manuel, --release"]
fn essais_cartes_5993() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = std::env::var("BANC_5993_BASE")
        .unwrap_or_else(|_| dossier.path().join("banc.db").display().to_string());
    let _ = std::fs::remove_file(&chemin);
    let state = AppState::new(&chemin, 0, Default::default()).unwrap();
    remplir(&state);
    let db = &state.backend;
    let ids: Vec<String> = db
        .query_many(
            &tune_core::db::facet_filter::sql_pistes_ecartees_par_le_socle(
                tune_core::db::engine::Engine::Sqlite,
            ),
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r[0].as_i64())
        .map(|i| i.to_string())
        .collect();
    let ids = ids.join(",");
    let albums: Vec<String> = db
        .query_many(
            &format!(
                "SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL AND id IN ({ids})"
            ),
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r[0].as_i64())
        .map(|i| i.to_string())
        .collect();
    let albums = albums.join(",");
    let base = format!(
        "{} AND t.album_id IS NOT NULL",
        tune_core::db::facet_filter::hidden_tracks_excluded()
    );
    let chrono = |nom: &str, sql: &str| {
        for r in db
            .query_many(&format!("EXPLAIN QUERY PLAN {sql}"), &[])
            .unwrap()
        {
            eprintln!("PLAN {nom}: {:?}", r.last().and_then(|v| v.as_string()));
        }
        let _ = db.query_many(sql, &[]).unwrap();
        let mut d = Vec::new();
        for _ in 0..7 {
            let t = Instant::now();
            let _ = db.query_many(sql, &[]).unwrap();
            d.push(t.elapsed());
        }
        d.sort();
        eprintln!(
            "ESSAI {nom:<28} mediane={:>7.1} ms",
            d[3].as_secs_f64() * 1e3
        );
    };
    let total = format!("SELECT COUNT(DISTINCT t.album_id) FROM tracks t WHERE {base}");
    chrono("total", &total);
    chrono("touches", &format!("{total} AND t.album_id IN ({albums})"));
    chrono(
        "gardes",
        &format!("{total} AND t.album_id IN ({albums}) AND t.id NOT IN ({ids})"),
    );
    chrono(
        "touches par album",
        &format!(
            "SELECT COUNT(*) FROM albums a WHERE a.id IN ({albums}) AND EXISTS \
             (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND {base})"
        ),
    );
    let sel = "SELECT t.album_id, MAX(al.title), MAX(COALESCE(t.album_artist, ar.name)), \
               SUM(COALESCE(t.duration_ms, 0)), COUNT(*), MAX(t.format) \
               FROM tracks t LEFT JOIN albums al ON al.id = t.album_id \
               LEFT JOIN artists ar ON ar.id = t.artist_id";
    chrono(
        "page masques seuls",
        &format!("{sel} WHERE {base} GROUP BY t.album_id ORDER BY 3, 2 LIMIT 500"),
    );
    chrono(
        "page bras 1 (HAVING)",
        &format!("{sel} WHERE {base} GROUP BY t.album_id HAVING t.album_id NOT IN ({albums})"),
    );
    chrono(
        "page bras 2",
        &format!(
            "{sel} WHERE {base} AND t.album_id IN ({albums}) AND t.id NOT IN ({ids}) GROUP BY t.album_id"
        ),
    );
    chrono(
        "page union",
        &format!(
            "{sel} WHERE {base} GROUP BY t.album_id HAVING t.album_id NOT IN ({albums}) UNION ALL \
             {sel} WHERE {base} AND t.album_id IN ({albums}) AND t.id NOT IN ({ids}) GROUP BY t.album_id \
             ORDER BY 3, 2 LIMIT 500"
        ),
    );
    chrono(
        "candidats",
        &format!(
            "SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL AND id IN ({ids})"
        ),
    );
}
