//! `GET /library/browse/dir` : le compte de pistes d'un sous-dossier, vu de la
//! ROUTE (#3857).
//!
//! La route listait les sous-dossiers par `read_dir`, puis lançait un
//! `SELECT COUNT(*) FROM tracks WHERE file_path LIKE '<sous-dossier>%'` **par
//! sous-dossier**, séquentiellement. Or le commentaire de `like_escape_clause`
//! dit que ce `LIKE` ne peut pas s'appuyer sur l'index de `file_path` : chaque
//! compte parcourt toute la table. Un niveau coûtait donc « nombre de
//! sous-dossiers × toute la bibliothèque ». Pierre M (fil forum 1671, 155 829
//! titres) remonte l'arborescence depuis « Localiser sur le disque » et trouve
//! cela « très lent ».
//!
//! Mesure sur une base SQLite de 155 829 lignes, sur Shrek :
//!
//! ```text
//! coffret de 63 dossiers CDxx   : 63 requêtes = 1 166,3 ms   →  1 requête =  21,3 ms
//! racine de 11 891 artistes     : 11 891 req. = 303 964,7 ms →  1 requête = 187,4 ms
//! ```
//!
//! L'équivalence des comptes est gardée au plus près du SQL par
//! `tune-core/src/db/track_repo.rs` (module `comptes_par_sous_dossier_tests`),
//! qui rejoue la boucle d'origine et exige le même total. CE fichier-ci garde
//! l'autre moitié, celle qui manque le plus souvent : que la route APPELLE
//! réellement la table groupée et publie ses comptes. Débrancher
//! `comptes.get(&name)` fait rougir ici, et nulle part ailleurs.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

/// Une bibliothèque à trois sous-dossiers : un peuplé en profondeur, un peuplé
/// à plat, un vide. Plus une piste posée DIRECTEMENT dans la racine, qui ne
/// doit appartenir à aucun sous-dossier.
fn bibliotheque() -> (tempfile::TempDir, axum::Router, String) {
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let racine = tmp.path().to_string_lossy().to_string();
    for d in [
        "Artiste A/Album 1",
        "Artiste A/Album 2",
        "Artiste B",
        "Vide",
    ] {
        std::fs::create_dir_all(tmp.path().join(d)).expect("sous-dossier");
    }
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("état serveur isolé");
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &format!("[{}]", serde_json::json!(racine)))
        .expect("racines musique");
    let sep = std::path::MAIN_SEPARATOR;
    let chemins = [
        format!("{racine}{sep}Artiste A{sep}Album 1{sep}01.flac"),
        format!("{racine}{sep}Artiste A{sep}Album 1{sep}02.flac"),
        format!("{racine}{sep}Artiste A{sep}Album 2{sep}01.flac"),
        format!("{racine}{sep}Artiste B{sep}01.flac"),
        format!("{racine}{sep}orpheline.flac"),
    ];
    let mut sql = String::new();
    for (i, c) in chemins.iter().enumerate() {
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, file_path, source) VALUES ({}, {}, {}, 'local');",
            100 + i,
            serde_json::json!(format!("piste {i}")),
            serde_json::json!(c),
        ));
    }
    state.backend.execute_batch(&sql).expect("pistes témoins");
    let app = tune_server::routes::router(state);
    (tmp, app, racine)
}

async fn parcourir(app: &axum::Router, chemin: &str) -> (StatusCode, serde_json::Value) {
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Encodage minimal des seuls caractères qu'un chemin temporaire peut porter et
/// qu'une chaîne de requête interprète (même besoin, même remède que
/// `portee_audit_repertoire_3101.rs`).
fn encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '&' => "%26".to_string(),
            '+' => "%2B".to_string(),
            '?' => "%3F".to_string(),
            '\\' => "%5C".to_string(),
            autre => autre.to_string(),
        })
        .collect()
}

fn compte(corps: &serde_json::Value, nom: &str) -> i64 {
    corps["directories"]
        .as_array()
        .expect("la route publie ses sous-dossiers")
        .iter()
        .find(|d| d["name"] == nom)
        .unwrap_or_else(|| panic!("sous-dossier « {nom} » absent de la réponse"))["track_count"]
        .as_i64()
        .expect("track_count est un entier")
}

#[tokio::test]
async fn la_route_publie_le_compte_recursif_de_chaque_sous_dossier() {
    let (_tmp, app, racine) = bibliotheque();
    let (statut, corps) = parcourir(&app, &racine).await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(
        compte(&corps, "Artiste A"),
        3,
        "le compte est RÉCURSIF : deux albums, trois pistes"
    );
    assert_eq!(compte(&corps, "Artiste B"), 1);
    assert_eq!(
        compte(&corps, "Vide"),
        0,
        "un dossier sans piste s'annonce à 0, il ne disparaît pas"
    );
    let total: i64 = corps["directories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["track_count"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(
        total, 4,
        "la piste posée dans la racine n'est comptée dans aucun sous-dossier"
    );
}

/// Le niveau suivant compte lui aussi, et il compte à partir du BON parent :
/// un décalage de coupe rendrait 0 partout sans rien casser d'autre.
#[tokio::test]
async fn le_niveau_suivant_compte_a_partir_de_son_propre_parent() {
    let (_tmp, app, racine) = bibliotheque();
    let sep = std::path::MAIN_SEPARATOR;
    let (statut, corps) = parcourir(&app, &format!("{racine}{sep}Artiste A")).await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(compte(&corps, "Album 1"), 2);
    assert_eq!(compte(&corps, "Album 2"), 1);
}

fn titres(corps: &serde_json::Value) -> Vec<String> {
    corps["tracks"]
        .as_array()
        .expect("la route publie ses pistes")
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("").to_string())
        .collect()
}

/// La liste de pistes d'un dossier ne contient que ses enfants DIRECTS.
///
/// Le `LIKE` étant récursif, la route rapatriait toute la descendance — seize
/// colonnes et deux jointures — puis en jetait la quasi-totalité en Rust. Le
/// « pas de séparateur après le préfixe » est désormais poussé dans le SQL, et
/// `est_enfant_direct` reste l'autorité derrière. Ce témoin garde le RÉSULTAT :
/// un pré-filtre trop strict ferait disparaître des pistes.
#[tokio::test]
async fn la_liste_de_pistes_ne_contient_que_les_enfants_directs() {
    let (_tmp, app, racine) = bibliotheque();
    let sep = std::path::MAIN_SEPARATOR;

    let (_, a_la_racine) = parcourir(&app, &racine).await;
    assert_eq!(
        titres(&a_la_racine),
        vec!["piste 4".to_string()],
        "la racine ne montre que le fichier qui y est POSÉ, pas les 4 autres"
    );

    let (_, chez_a) = parcourir(&app, &format!("{racine}{sep}Artiste A")).await;
    assert!(
        titres(&chez_a).is_empty(),
        "« Artiste A » n'a que des sous-dossiers : aucune piste directe, \
         mais ses sous-dossiers restent comptés"
    );
    assert_eq!(compte(&chez_a, "Album 1"), 2);

    let (_, chez_b) = parcourir(&app, &format!("{racine}{sep}Artiste B")).await;
    assert_eq!(titres(&chez_b), vec!["piste 3".to_string()]);

    let (_, dans_album) = parcourir(&app, &format!("{racine}{sep}Artiste A{sep}Album 1")).await;
    let mut t = titres(&dans_album);
    t.sort();
    assert_eq!(t, vec!["piste 0".to_string(), "piste 1".to_string()]);
}

/// Le pré-filtre coupe `file_path` à une position, et cette position se compte
/// en CARACTÈRES. Un dossier accentué dans le chemin du parent la décalerait
/// d'autant d'octets de continuation, et la liste de pistes deviendrait vide
/// sans que rien ne le dise.
#[tokio::test]
async fn un_dossier_accentue_ne_fait_pas_disparaitre_ses_pistes() {
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let racine = tmp.path().to_string_lossy().to_string();
    let sep = std::path::MAIN_SEPARATOR;
    let accentue = format!("{racine}{sep}Béla Bartók");
    std::fs::create_dir_all(format!("{accentue}{sep}Quatuors")).expect("sous-dossier");
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("état serveur isolé");
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", &format!("[{}]", serde_json::json!(racine)))
        .expect("racines musique");
    let mut sql = String::new();
    for (i, c) in [
        format!("{accentue}{sep}direct.flac"),
        format!("{accentue}{sep}Quatuors{sep}n4.flac"),
        format!("{accentue}{sep}Quatuors{sep}n5.flac"),
    ]
    .iter()
    .enumerate()
    {
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, file_path, source) VALUES ({}, {}, {}, 'local');",
            200 + i,
            serde_json::json!(format!("accent {i}")),
            serde_json::json!(c),
        ));
    }
    state.backend.execute_batch(&sql).expect("pistes témoins");
    let app = tune_server::routes::router(state);

    let (statut, corps) = parcourir(&app, &accentue).await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(
        titres(&corps),
        vec!["accent 0".to_string()],
        "la piste posée dans un dossier accentué doit rester visible"
    );
    assert_eq!(
        compte(&corps, "Quatuors"),
        2,
        "et le compte de son sous-dossier reste juste"
    );
}

// ---------------------------------------------------------------------------
// PostgreSQL — le dialecte de la requête groupée, sur une VRAIE base.
//
// `compter_pistes_par_sous_dossier` et le pré-filtre d'enfant direct ont DEUX
// écritures : `instr` / `?n` sur SQLite, `strpos` / `$n` sur Postgres. Les neuf
// épreuves ci-dessus montent toutes un `AppState` sur `:memory:` : elles
// n'exécutent donc JAMAIS la seconde. « Écrit mais pas branché », appliqué à un
// dialecte SQL — le mode d'échec que ce dépôt connaît par cœur.
//
// ⚠️ Doctrine du saut, reprise de `pg_3181_sections_accueil.rs` :
// `TUNE_TEST_PG_URL` ABSENTE saute (le `cargo test` ordinaire n'a pas de base),
// mais une variable POSÉE dont la connexion échoue fait TOMBER le test. Un banc
// mal branché doit rougir, jamais s'afficher vert.
// ---------------------------------------------------------------------------

#[cfg(feature = "postgres")]
mod pg_3857 {
    use super::{compte, encode, parcourir, titres};
    use axum::http::StatusCode;
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_server::state::AppState;

    /// Les tables vidées avant la semence. `DELETE` et non `TRUNCATE` : la même
    /// instruction doit valoir sur les deux moteurs, et SQLite ne connaît pas
    /// `TRUNCATE`. La base PostgreSQL du banc est partagée entre les étapes.
    const VIDAGE: &str = "DELETE FROM tracks;";

    fn etat_postgres(url: &str) -> AppState {
        let config = tune_server::config::TuneConfig {
            database_url: Some(url.to_string()),
            ..Default::default()
        };
        // Pas de `ok()?` : une connexion qui échoue doit ROUGIR, jamais sauter.
        AppState::new("", 0, config).expect("AppState sur PostgreSQL")
    }

    /// Sème le MÊME scénario que `bibliotheque()`, en SQL littéral (les
    /// marqueurs diffèrent d'un moteur à l'autre).
    fn semer(state: &AppState, racine: &str) {
        let sep = std::path::MAIN_SEPARATOR;
        SettingsRepo::with_backend(state.backend.clone())
            .set("music_dirs", &format!("[{}]", serde_json::json!(racine)))
            .expect("racines musique");
        let mut sql = VIDAGE.to_string();
        for (i, c) in [
            format!("{racine}{sep}Artiste A{sep}Album 1{sep}01.flac"),
            format!("{racine}{sep}Artiste A{sep}Album 1{sep}02.flac"),
            format!("{racine}{sep}Artiste A{sep}Album 2{sep}01.flac"),
            format!("{racine}{sep}Artiste B{sep}01.flac"),
            format!("{racine}{sep}orpheline.flac"),
        ]
        .iter()
        .enumerate()
        {
            sql.push_str(&format!(
                "INSERT INTO tracks (title, file_path, source) VALUES ({}, {}, 'local');",
                serde_json::json!(format!("piste {i}")),
                serde_json::json!(c),
            ));
        }
        state.backend.execute_batch(&sql).expect("pistes témoins");
    }

    fn arborescence() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("dossier temporaire");
        for d in [
            "Artiste A/Album 1",
            "Artiste A/Album 2",
            "Artiste B",
            "Vide",
        ] {
            std::fs::create_dir_all(tmp.path().join(d)).expect("sous-dossier");
        }
        tmp
    }

    /// Le dialecte `strpos` / `$n` rend EXACTEMENT ce que rend `instr` / `?n`.
    ///
    /// Les deux moteurs voient le même disque, la même semence et la même
    /// requête HTTP ; les deux corps sont comparés tels quels. Réparer un moteur
    /// en changeant ce que l'autre rend serait un échange, pas une correction.
    /// ⚠️ Le nom porte `pg_3857` parce que cest le FILTRE de létape CI, et
    /// que la garde `tout_temoin_sous_variable_d_environnement_est_recense`
    /// (`derive_des_garde_fous_2816.rs`) confronte ce filtre au nom COMPLET du
    /// témoin — lequel ne porte pas le module interne. Sans le préfixe ici,
    /// létape existait et nexécutait rien : « écrit mais pas branché », et
    /// cest la garde qui la dit.
    #[tokio::test]
    async fn pg_3857_les_deux_moteurs_rendent_les_memes_comptes_de_sous_dossiers() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let tmp = arborescence();
        let racine = tmp.path().to_string_lossy().to_string();
        let sep = std::path::MAIN_SEPARATOR;

        let pg = etat_postgres(&url);
        semer(&pg, &racine);
        let app_pg = tune_server::routes::router(pg);

        let sqlite = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
        semer(&sqlite, &racine);
        let app_sqlite = tune_server::routes::router(sqlite);

        for chemin in [
            racine.clone(),
            format!("{racine}{sep}Artiste A"),
            format!("{racine}{sep}Artiste A{sep}Album 1"),
            format!("{racine}{sep}Artiste B"),
        ] {
            let (s_pg, c_pg) = parcourir(&app_pg, &chemin).await;
            let (s_lite, c_lite) = parcourir(&app_sqlite, &chemin).await;
            assert_eq!(s_pg, StatusCode::OK, "PostgreSQL sur {chemin} : {c_pg}");
            assert_eq!(s_lite, StatusCode::OK, "SQLite sur {chemin} : {c_lite}");
            assert_eq!(
                c_pg["directories"], c_lite["directories"],
                "les sous-dossiers et leurs comptes diffèrent entre les deux \
                 moteurs sur « {chemin} »"
            );
            assert_eq!(
                titres(&c_pg),
                titres(&c_lite),
                "la liste de pistes diffère entre les deux moteurs sur « {chemin} »"
            );
        }

        // Et les valeurs elles-mêmes, nommées : deux moteurs d'accord sur ZÉRO
        // partout seraient d'accord pour rien.
        let (_, racine_pg) = parcourir(&app_pg, &racine).await;
        assert_eq!(compte(&racine_pg, "Artiste A"), 3, "PostgreSQL, récursif");
        assert_eq!(compte(&racine_pg, "Artiste B"), 1);
        assert_eq!(compte(&racine_pg, "Vide"), 0);
        assert_eq!(
            titres(&racine_pg),
            vec!["piste 4".to_string()],
            "PostgreSQL : la racine ne montre que le fichier qui y est POSÉ"
        );
        let (_, album_pg) =
            parcourir(&app_pg, &format!("{racine}{sep}Artiste A{sep}Album 1")).await;
        let mut t = titres(&album_pg);
        t.sort();
        assert_eq!(t, vec!["piste 0".to_string(), "piste 1".to_string()]);
        // `encode` est utilisé par `parcourir` ; nommé ici pour que le module
        // n'importe rien d'inutile.
        let _ = encode("");
    }
}
