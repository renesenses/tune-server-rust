//! tune-web-client#1716 — l'onglet Titres paginé CÔTÉ SERVEUR, par la route
//! montée : `GET /library/tracks?sort=…&order=…&search=…&provenance=…&counts=sources`.
//!
//! Ce fichier cloue :
//!
//!  (a) la compatibilité : SANS les nouveaux paramètres, la réponse est celle
//!      d'avant — mêmes clés, mêmes lignes, même ordre ;
//!  (b) le tri, la recherche (titre ou artiste), la provenance et les comptes
//!      par provenance, tels que les calculait le navigateur ;
//!  (c) les pages mises bout à bout : la liste entière, triée, sans doublon ;
//!  (d) le refus d'un paramètre invalide (400), jamais ignoré.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.
use std::collections::{BTreeSet, HashSet};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

const ALBUMS: i64 = 120;
const PISTES_PAR_ALBUM: i64 = 12;

/// 120 albums de 12 pistes : 80 locaux, 40 sur deux serveurs UPnP, des
/// titres accentués, un album masqué.
fn remplir(state: &AppState) {
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=30 {
        let nom = if a % 7 == 0 {
            format!("Éric {a}")
        } else {
            format!("Artiste {a}")
        };
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, '{nom}');\n"
        ));
    }
    let mut id = 0_i64;
    for al in 1..=ALBUMS {
        let artiste = al % 30 + 1;
        let source = if al > 80 { "upnp" } else { "local" };
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({al}, 'Album {al}', {artiste}, '{source}');\n"
        ));
        for n in 1..=PISTES_PAR_ALBUM {
            id += 1;
            let titre = match id % 5 {
                0 => format!("Azur {id}"),
                1 => format!("été {id}"),
                _ => format!("Piste {id}"),
            };
            let source_id = if source == "upnp" {
                let udn = if al % 2 == 0 { "uuid-a" } else { "uuid-b" };
                format!("'{udn}|{id}'")
            } else {
                "NULL".to_string()
            };
            sql.push_str(&format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
                 duration_ms, file_path, format, sample_rate, bit_depth, source, source_id, album_artist) \
                 VALUES ({id}, '{titre}', {al}, {artiste}, 1, {n}, {}, '/banc/{id}.flac', 'flac', \
                 44100, 16, '{source}', {source_id}, '');\n",
                (id * 7919) % 400_000
            ));
        }
    }
    sql.push_str("INSERT INTO hidden_items (item_type, item_id) VALUES ('album', 3);\n");
    sql.push_str("COMMIT;");
    state.backend.execute_batch(&sql).unwrap();
}

async fn etat() -> (tempfile::TempDir, AppState, axum::Router) {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    remplir(&state);
    let app = tune_server::routes::router(state.clone());
    (dossier, state, app)
}

async fn get(app: &axum::Router, requete: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/library/tracks?{requete}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (statut, corps)
}

fn champ(v: &Value, cle: &str) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t[cle].as_str().unwrap_or_default().to_string())
        .collect()
}

fn ids(v: &Value) -> Vec<i64> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect()
}

/// Le pliage de `fold()` côté client, pour l'oracle.
fn plie(s: &str) -> String {
    tune_core::db::engine::fold_diacritics(s).to_lowercase()
}

/// (a) Sans les nouveaux paramètres, rien ne bouge.
#[tokio::test(flavor = "multi_thread")]
async fn sans_les_nouveaux_parametres_la_reponse_est_celle_d_avant() {
    let (_d, state, app) = etat().await;
    let (statut, v) = get(&app, "limit=500&offset=0").await;
    assert_eq!(statut, StatusCode::OK);
    let cles: BTreeSet<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        cles,
        BTreeSet::from(["items", "limit", "offset", "total"]),
        "un ancien client ne doit voir aucune clé nouvelle"
    );
    let repo = TrackRepo::with_backend(state.backend.clone());
    let attendu: Vec<i64> = repo
        .list_visible(500, 0)
        .unwrap()
        .into_iter()
        .filter_map(|t| t.id)
        .collect();
    assert_eq!(ids(&v), attendu);
    assert_eq!(v["total"].as_i64(), Some(repo.count_visible().unwrap()));
    // Une valeur VIDE ne vaut pas paramètre posé.
    let (_, vide) = get(&app, "limit=500&offset=0&sort=&search=&provenance=").await;
    assert_eq!(vide, v);
}

/// (b) + (c) Le tri par titre, dans les deux sens, page après page.
#[tokio::test(flavor = "multi_thread")]
async fn les_pages_triees_mises_bout_a_bout_rendent_la_liste_entiere() {
    let (_d, state, app) = etat().await;
    let total = TrackRepo::with_backend(state.backend.clone())
        .count_visible()
        .unwrap();
    for ordre in ["asc", "desc"] {
        let mut titres = Vec::new();
        let mut vus = HashSet::new();
        let mut offset = 0;
        loop {
            let (statut, v) = get(
                &app,
                &format!("sort=title&order={ordre}&limit=200&offset={offset}"),
            )
            .await;
            assert_eq!(statut, StatusCode::OK);
            assert_eq!(v["sort"], "title");
            assert_eq!(v["order"], ordre);
            assert_eq!(v["total"].as_i64(), Some(total));
            for id in ids(&v) {
                assert!(vus.insert(id), "piste {id} sur deux pages");
            }
            let lot = champ(&v, "title");
            let n = lot.len();
            titres.extend(lot);
            if n < 200 {
                break;
            }
            offset += 200;
        }
        assert_eq!(titres.len() as i64, total);
        let mut attendu = titres.clone();
        attendu.sort_by_key(|t| plie(t));
        if ordre == "desc" {
            attendu.reverse();
        }
        let plies = |l: &[String]| l.iter().map(|t| plie(t)).collect::<Vec<_>>();
        assert_eq!(plies(&titres), plies(&attendu), "ordre {ordre}");
    }
}

/// (b) La recherche, la provenance et les comptes, comme le navigateur.
#[tokio::test(flavor = "multi_thread")]
async fn recherche_provenance_et_comptes_comme_le_navigateur() {
    let (_d, state, app) = etat().await;
    // L'oracle : la liste entière, filtrée comme `LibraryV2` le faisait.
    let toutes = TrackRepo::with_backend(state.backend.clone())
        .list_visible(100_000, 0)
        .unwrap();
    let provenance = |t: &tune_core::db::models::Track| -> String {
        if t.source != "upnp" {
            return t.source.clone();
        }
        let sid = t.source_id.clone().unwrap_or_default();
        let mut morceaux = sid.split('|');
        match (morceaux.next(), morceaux.next()) {
            (Some(u), Some(i)) if !u.trim().is_empty() && !i.is_empty() => {
                format!("upnp:{}", u.trim())
            }
            _ => "upnp".into(),
        }
    };
    let cherche = |t: &tune_core::db::models::Track, aiguille: &str| {
        plie(&t.title).contains(aiguille)
            || plie(t.artist_name.as_deref().unwrap_or_default()).contains(aiguille)
    };

    for saisie in ["ete", "AZUR", "éric"] {
        let aiguille = plie(saisie);
        let trouvees: Vec<_> = toutes.iter().filter(|t| cherche(t, &aiguille)).collect();
        assert!(!trouvees.is_empty(), "le banc doit répondre à {saisie}");
        let (statut, v) = get(
            &app,
            &format!(
                "search={}&counts=sources&limit=5",
                urlencoding::encode(saisie)
            ),
        )
        .await;
        assert_eq!(statut, StatusCode::OK);
        assert_eq!(v["total"].as_i64(), Some(trouvees.len() as i64), "{saisie}");
        assert_eq!(v["total_all_sources"].as_i64(), Some(trouvees.len() as i64));
        let comptes = v["source_counts"].as_object().unwrap();
        for cle in ["local", "upnp:uuid-a", "upnp:uuid-b"] {
            let attendu = trouvees.iter().filter(|t| provenance(t) == cle).count() as i64;
            assert_eq!(
                comptes.get(cle).and_then(Value::as_i64).unwrap_or(0),
                attendu,
                "{saisie} / {cle}"
            );
        }
        let upnp = trouvees
            .iter()
            .filter(|t| provenance(t).starts_with("upnp"))
            .count() as i64;
        assert_eq!(comptes["upnp"].as_i64(), Some(upnp), "agrégat upnp");

        // La provenance filtre la page, pas les comptes.
        for cle in ["local", "upnp", "upnp:uuid-a"] {
            let (_, p) = get(
                &app,
                &format!(
                    "search={}&provenance={}&counts=sources&limit=1000",
                    urlencoding::encode(saisie),
                    urlencoding::encode(cle)
                ),
            )
            .await;
            let attendu: HashSet<i64> = trouvees
                .iter()
                .filter(|t| {
                    let p = provenance(t);
                    p == cle || (cle == "upnp" && p.starts_with("upnp"))
                })
                .filter_map(|t| t.id)
                .collect();
            assert_eq!(ids(&p).into_iter().collect::<HashSet<_>>(), attendu);
            assert_eq!(p["source_counts"], v["source_counts"]);
        }
    }
}

/// (d) Un paramètre invalide est REFUSÉ.
#[tokio::test(flavor = "multi_thread")]
async fn un_parametre_invalide_est_refuse() {
    let (_d, _state, app) = etat().await;
    for requete in ["sort=plays", "sort=nimporte", "order=up", "counts=albums"] {
        let (statut, _) = get(&app, requete).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{requete}");
    }
    let (statut, v) = get(&app, "order=desc&limit=3").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(v["sort"], Value::Null);
    assert_eq!(v["order"], "desc");
}
