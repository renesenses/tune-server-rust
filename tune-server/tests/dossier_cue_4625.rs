//! #4625 — un dossier dont les pistes viennent d'une feuille CUE affichait
//! « 0 » dans la vue par dossiers, et s'ouvrait sur une liste vide.
//!
//! JeromeQ (fil 1866) : `Willy DeVille - Live In Paris And New York DR 12`
//! compte 0 piste alors que ses fichiers FLAC sont là. Le compte est un
//! `COUNT` en base (`compter_pistes_par_sous_dossier`), fait sur `file_path`.
//! Or une piste découpée par une feuille CUE a `file_path = NULL` par
//! construction — son fichier vit dans `cue_media_path` — et le scan retire de
//! la bibliothèque les fichiers que la feuille découpe (`images_cue`). Un
//! dossier rangé en CUE, y compris une feuille « un FILE par piste » posée à
//! côté de FLAC ordinaires, était donc INVISIBLE à la vue par dossiers alors
//! que ses pistes sont en base.
//!
//! ⚠️ Ce banc prouve le défaut pour un dossier CUE. Il ne prouve PAS que le
//! dossier de JeromeQ porte une feuille CUE : c'est la question posée dans le
//! ticket (le `ls -la` du dossier).
//!
//! Doctrine du saut, reprise de `pg_3182_moteur_annonce_dans_le_rapport.rs` :
//! `TUNE_TEST_PG_URL` absente ⇒ l'épreuve PostgreSQL saute ; posée mais
//! injoignable ⇒ elle ROUGIT.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SEP: char = std::path::MAIN_SEPARATOR;

/// Un littéral de chaîne SQL, apostrophes doublées (valable sur les deux
/// moteurs, contrairement aux guillemets doubles de `serde_json::json!`).
fn litteral(valeur: &str) -> String {
    format!("'{}'", valeur.replace('\'', "''"))
}

/// Deux albums frères sous « Willy DeVille » : l'un en FLAC ordinaires, l'autre
/// découpé par une feuille CUE (deux tranches d'une même image).
fn semer(state: &AppState, racine: &str) {
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &format!("[{}]", serde_json::json!(racine)))
        .expect("racines musique");
    let berlin = format!("{racine}{SEP}Willy DeVille{SEP}In Berlin (CD1) DR 9");
    let paris = format!("{racine}{SEP}Willy DeVille{SEP}Live In Paris And New York DR 12");
    let image = format!("{paris}{SEP}Live In Paris And New York.flac");
    let mut sql = String::from("DELETE FROM tracks;");
    for (titre, chemin) in [
        ("Spanish Stroll", format!("{berlin}{SEP}01.flac")),
        ("Heaven Stood Still", format!("{berlin}{SEP}02.flac")),
    ] {
        sql.push_str(&format!(
            "INSERT INTO tracks (title, file_path, source) VALUES ({}, {}, 'local');",
            litteral(titre),
            litteral(&chemin),
        ));
    }
    for (n, (titre, debut, fin)) in [
        ("Lilly's Daddy's Cadillac", 0, 240_000),
        ("This Must Be the Night", 240_000, 480_000),
    ]
    .into_iter()
    .enumerate()
    {
        sql.push_str(&format!(
            "INSERT INTO tracks (title, track_number, file_path, cue_media_path, cue_start_ms, cue_end_ms, source) \
             VALUES ({}, {}, NULL, {}, {debut}, {fin}, 'local');",
            litteral(titre),
            n + 1,
            litteral(&image),
        ));
    }
    state.backend.execute_batch(&sql).expect("pistes témoins");
}

fn arborescence() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    for d in [
        "Willy DeVille/In Berlin (CD1) DR 9",
        "Willy DeVille/Live In Paris And New York DR 12",
    ] {
        std::fs::create_dir_all(tmp.path().join(d)).expect("sous-dossier");
    }
    tmp
}

fn encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '&' => "%26".to_string(),
            '+' => "%2B".to_string(),
            '?' => "%3F".to_string(),
            '(' => "%28".to_string(),
            ')' => "%29".to_string(),
            '\\' => "%5C".to_string(),
            autre => autre.to_string(),
        })
        .collect()
}

async fn parcourir(app: &axum::Router, chemin: &str) -> serde_json::Value {
    let uri = format!("/api/v1/library/browse/dir?path={}", encode(chemin));
    let resp = app
        .clone()
        .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
        .await
        .expect("la route doit répondre");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("corps");
    let corps: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "{chemin} : {corps}");
    corps
}

fn compte(corps: &serde_json::Value, nom: &str) -> i64 {
    corps["directories"]
        .as_array()
        .expect("la route publie ses sous-dossiers")
        .iter()
        .find(|d| d["name"] == nom)
        .unwrap_or_else(|| panic!("sous-dossier « {nom} » absent de la réponse : {corps}"))
        ["track_count"]
        .as_i64()
        .expect("track_count est un entier")
}

fn titres(corps: &serde_json::Value) -> Vec<String> {
    let mut t: Vec<String> = corps["tracks"]
        .as_array()
        .expect("la route publie ses pistes")
        .iter()
        .filter_map(|p| p["title"].as_str().map(String::from))
        .collect();
    t.sort();
    t
}

async fn verifier(state: AppState, racine: &str, moteur: &str) {
    let app = tune_server::routes::router(state);
    let artiste = format!("{racine}{SEP}Willy DeVille");
    let niveau = parcourir(&app, &artiste).await;
    assert_eq!(
        compte(&niveau, "In Berlin (CD1) DR 9"),
        2,
        "{moteur} : l'album en FLAC ordinaires"
    );
    assert_eq!(
        compte(&niveau, "Live In Paris And New York DR 12"),
        2,
        "{moteur} : le dossier découpé par une feuille CUE est compté 0 — ses pistes \
         (file_path NULL, cue_media_path renseigné) sont invisibles à la vue par dossiers"
    );
    let album = parcourir(
        &app,
        &format!("{artiste}{SEP}Live In Paris And New York DR 12"),
    )
    .await;
    assert_eq!(
        titres(&album),
        vec![
            "Lilly's Daddy's Cadillac".to_string(),
            "This Must Be the Night".to_string()
        ],
        "{moteur} : le dossier CUE s'ouvre sur une liste de pistes vide"
    );
    // Et le dossier parent ne s'approprie pas les tranches de son enfant.
    assert!(
        titres(&niveau).is_empty(),
        "{moteur} : les tranches CUE remontent dans le dossier parent : {:?}",
        titres(&niveau)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn un_dossier_cue_est_compte_et_liste_dans_la_vue_par_dossiers() {
    let tmp = arborescence();
    let racine = tmp.path().to_string_lossy().to_string();
    let sqlite = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    semer(&sqlite, &racine);
    verifier(sqlite, &racine, "SQLite").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_4625_un_dossier_cue_est_compte_et_liste_sur_postgresql() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de #4625 SAUTÉE");
        return;
    };
    let tmp = arborescence();
    let racine = tmp.path().to_string_lossy().to_string();
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    // Pas de `ok()?` : une base posée mais injoignable doit ROUGIR.
    let pg = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    semer(&pg, &racine);
    verifier(pg, &racine, "PostgreSQL").await;
}
