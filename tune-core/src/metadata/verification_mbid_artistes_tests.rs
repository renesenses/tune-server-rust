//! #6463 — provenance du MBID d'artiste (A), reprise du passé par
//! vérification MusicBrainz (B), identité détachée (« Détacher l'identité »).
//!
//! Le réseau est une DOUBLURE sur la boucle locale (127.0.0.1), posée par
//! `remplacer_la_base_musicbrainz` : aucun appel réel ne part. Les témoins qui
//! la touchent se sérialisent par [`BANC`], la base remplacée étant globale.
//!
//! Les MBID et les fiches MusicBrainz ci-dessous sont des FIXTURES : ni le
//! MBID réel de « Christophe » chez FabienM ni celui d'« Ange » chez Didier
//! n'ont été relevés.

use super::*;
use crate::db::artist_repo::{ArtistRepo, ProvenanceMbid};
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;
use crate::metadata::musicbrainz_release::{
    CreditArtiste, MBReleaseDetail, remplacer_la_base_musicbrainz,
};
use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

static BANC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn provenance(b: &Arc<dyn DbBackend>, id: i64) -> Option<ProvenanceMbid> {
    ArtistRepo::with_backend(b.clone())
        .provenance_du_mbid(id)
        .unwrap()
}

fn mbid(b: &Arc<dyn DbBackend>, id: i64) -> Option<String> {
    ArtistRepo::with_backend(b.clone())
        .get(id)
        .unwrap()
        .and_then(|a| a.musicbrainz_id)
}

/// La doublure : `GET /artist/{mbid}` rend la fiche rangée (404 sinon),
/// `GET /artist?query=` rend `recherche`. Chaque requête est notée, avec son
/// heure d'arrivée.
struct Doublure {
    requetes: Arc<Mutex<Vec<(String, Instant)>>>,
}

impl Doublure {
    fn chemins(&self) -> Vec<String> {
        self.requetes
            .lock()
            .unwrap()
            .iter()
            .map(|(c, _)| c.clone())
            .collect()
    }
    fn heures(&self) -> Vec<Instant> {
        self.requetes
            .lock()
            .unwrap()
            .iter()
            .map(|(_, h)| *h)
            .collect()
    }
}

async fn doublure(fiches: BTreeMap<String, Value>, recherche: Value) -> Doublure {
    let requetes: Arc<Mutex<Vec<(String, Instant)>>> = Arc::default();
    let (r1, r2) = (requetes.clone(), requetes.clone());
    let fiches = Arc::new(fiches);
    let recherche = Arc::new(recherche);
    let app = axum::Router::new()
        .route(
            "/artist/{mbid}",
            get(move |AxPath(m): AxPath<String>| {
                let fiches = fiches.clone();
                let r = r1.clone();
                async move {
                    r.lock()
                        .unwrap()
                        .push((format!("artist/{m}"), Instant::now()));
                    match fiches.get(&m) {
                        Some(f) => axum::Json(f.clone()).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        )
        .route(
            "/artist",
            get(move || {
                let recherche = recherche.clone();
                let r = r2.clone();
                async move {
                    r.lock()
                        .unwrap()
                        .push(("artist?query".to_string(), Instant::now()));
                    axum::Json((*recherche).clone()).into_response()
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    remplacer_la_base_musicbrainz(Some(format!("http://127.0.0.1:{port}")));
    Doublure { requetes }
}

struct FinDuBanc;
impl Drop for FinDuBanc {
    fn drop(&mut self) {
        remplacer_la_base_musicbrainz(None);
    }
}

// ── A. Chacun des quatre chemins pose sa provenance ─────────────────────────

/// 1. La balise, au scan : `get_or_create(nom, Some(mbid))` (le chemin de
///    `auto_scan.rs` et `scan_import.rs`).
#[test]
fn la_balise_du_scan_pose_balise() {
    let b = base();
    let repo = ArtistRepo::with_backend(b.clone());
    let a = repo
        .get_or_create("Air", Some("mb-air-balise"), None)
        .unwrap();
    assert_eq!(
        provenance(&b, a.id.unwrap()),
        Some(ProvenanceMbid::Balise),
        "un MBID lu dans les fichiers doit porter la provenance `balise`"
    );
    let sans = repo.get_or_create("Sans Balise", None, None).unwrap();
    assert_eq!(
        provenance(&b, sans.id.unwrap()),
        None,
        "pas de MBID, pas de provenance"
    );
}

/// 2. Le nom : la recherche MusicBrainz de `metadata/matcher.rs`, contre la
///    doublure. `library/artwork.rs` écrit par le même `poser_mbid(.., Nom)`.
#[tokio::test]
async fn la_recherche_par_le_nom_pose_nom() {
    let _banc = BANC.lock().await;
    let _fin = FinDuBanc;
    let d = doublure(
        BTreeMap::new(),
        json!({"artists": [{"id": "mb-magma", "name": "Magma", "score": 100}]}),
    )
    .await;
    let b = base();
    b.execute("INSERT INTO artists (id, name) VALUES (1, 'Magma')", &[])
        .unwrap();
    let n = crate::metadata::matcher::batch_match_artist_mbids(b.clone()).await;
    assert_eq!(n, 1);
    assert_eq!(d.chemins(), vec!["artist?query".to_string()]);
    assert_eq!(mbid(&b, 1).as_deref(), Some("mb-magma"));
    assert_eq!(
        provenance(&b, 1),
        Some(ProvenanceMbid::Nom),
        "un MBID trouvé par le nom doit porter la provenance `nom`"
    );
}

/// 3. Le pressage : les crédits d'un album identifié
///    (`metadata/artistes_du_pressage.rs`).
#[test]
fn le_credit_du_pressage_pose_pressage() {
    let b = base();
    b.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Air'); \
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Moon Safari', 1);",
    )
    .unwrap();
    let detail = MBReleaseDetail {
        release_id: "rel".into(),
        artist_credits: vec![CreditArtiste {
            mbid: "mb-air".into(),
            nom: "Air".into(),
            nom_credite: "Air".into(),
            nom_de_tri: String::new(),
        }],
        ..Default::default()
    };
    let bilan = crate::metadata::artistes_du_pressage::rattacher_les_artistes_de_l_album(
        &b,
        1,
        &detail,
        &[],
    )
    .unwrap();
    assert_eq!(bilan.ecrits, 1);
    assert_eq!(
        provenance(&b, 1),
        Some(ProvenanceMbid::Pressage),
        "un MBID tiré du pressage doit porter la provenance `pressage`"
    );
}

/// Les MBID d'avant la colonne restent sans provenance (inconnue).
#[test]
fn un_mbid_existant_reste_de_provenance_inconnue() {
    let b = base();
    b.execute(
        "INSERT INTO artists (id, name, musicbrainz_id) VALUES (1, 'Christophe', 'mb-ancien')",
        &[],
    )
    .unwrap();
    assert_eq!(provenance(&b, 1), None);
}

// ── B. La reprise du passé ──────────────────────────────────────────────────

fn fiche(nom: &str, alias: &[&str]) -> Value {
    json!({
        "name": nom,
        "aliases": alias.iter().map(|a| json!({"name": a})).collect::<Vec<_>>(),
    })
}

/// La bibliothèque de la reprise : deux MBID faux, quatre justes par nom,
/// alias ou crédit, un `manuel`, un `balise`, un `pressage`, un inconnu.
fn bibliotheque() -> Arc<dyn DbBackend> {
    let b = base();
    b.execute_batch(
        "INSERT INTO artists (id, name, musicbrainz_id, musicbrainz_id_source, image_path, image_source, bio, bio_source) \
           VALUES (1, 'Christophe', 'mb-faux-christophe', NULL, 'h-photo-maé', 'auto', 'Chanteur né en 1976.', 'wikipedia'); \
         INSERT INTO artists (id, name, musicbrainz_id, musicbrainz_id_source, image_path, image_source) \
           VALUES (2, 'Ange', 'mb-faux-ange', 'nom', 'h-photo-chanteuse', 'community'); \
         INSERT INTO artists (id, name, musicbrainz_id) VALUES (3, 'Beatles', 'mb-beatles'); \
         INSERT INTO artists (id, name, musicbrainz_id, musicbrainz_id_source) VALUES (4, 'Magma', 'mb-magma', 'nom'); \
         INSERT INTO artists (id, name, musicbrainz_id) VALUES (5, 'Bashung', 'mb-bashung'); \
         INSERT INTO artists (id, name, musicbrainz_id, musicbrainz_id_source) VALUES (6, 'Gong', 'mb-gong-saisi', 'manuel'); \
         INSERT INTO artists (id, name, musicbrainz_id, musicbrainz_id_source) VALUES (7, 'Air', 'mb-air-balise', 'balise'); \
         INSERT INTO artists (id, name, musicbrainz_id) VALUES (8, 'Fantome', 'mb-inconnu'); \
         INSERT INTO artists (id, name, musicbrainz_id, musicbrainz_id_source) VALUES (9, 'Daho', 'mb-daho-pressage', 'pressage'); \
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Fantaisie militaire', 5); \
         INSERT INTO tracks (id, title, album_id, artist_id, track_number, disc_number) \
           VALUES (1, 'Ma petite entreprise', 1, 5, 1, 1); \
         INSERT INTO track_credits (track_id, artist_id, artist_name, role, artist_mbid) \
           VALUES (1, 5, 'Bashung', 'performer', 'mb-bashung');",
    )
    .unwrap();
    b
}

fn fiches_de_la_doublure() -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("mb-faux-christophe".into(), fiche("Christophe Maé", &[])),
        (
            "mb-faux-ange".into(),
            fiche("Angèle", &["Angèle Van Laeken"]),
        ),
        ("mb-beatles".into(), fiche("The Beatles", &["Beatles"])),
        ("mb-magma".into(), fiche("magma ", &[])),
        // Les quatre suivantes NE DOIVENT PAS être demandées : si elles
        // l'étaient, leur nom ne correspondrait pas et le MBID partirait.
        ("mb-bashung".into(), fiche("Quelqu'un d'autre", &[])),
        ("mb-gong-saisi".into(), fiche("Pas Gong", &[])),
        ("mb-air-balise".into(), fiche("Pas Air", &[])),
        ("mb-daho-pressage".into(), fiche("Pas Daho", &[])),
    ])
}

#[tokio::test]
async fn la_reprise_retire_les_faux_garde_les_justes_et_jamais_un_manuel() {
    let _banc = BANC.lock().await;
    let _fin = FinDuBanc;
    let d = doublure(fiches_de_la_doublure(), json!({})).await;
    let b = bibliotheque();

    let etat = executer_un_tour(&b, lire_la_fiche_musicbrainz).await;

    // Jamais un `manuel`, jamais demandé.
    assert_eq!(
        mbid(&b, 6).as_deref(),
        Some("mb-gong-saisi"),
        "un MBID de provenance `manuel` a été retiré par la reprise"
    );
    assert!(
        !d.chemins().contains(&"artist/mb-gong-saisi".to_string()),
        "un MBID de provenance `manuel` a été vérifié sur le réseau : {:?}",
        d.chemins()
    );
    // Les deux faux partent, avec leur portrait et leur bio.
    assert_eq!(mbid(&b, 1), None, "« Christophe » garde son MBID faux");
    assert_eq!(mbid(&b, 2), None, "« Ange » garde son MBID faux");
    let repo = ArtistRepo::with_backend(b.clone());
    let christophe = repo.get(1).unwrap().unwrap();
    assert_eq!(
        christophe.image_path, None,
        "le portrait du MBID retiré reste affiché"
    );
    assert_eq!(christophe.bio, None, "la bio du MBID retiré reste affichée");
    assert_eq!(repo.get(2).unwrap().unwrap().image_path, None);
    // Les justes restent : par alias, par nom, par crédit de pressage.
    assert_eq!(
        mbid(&b, 3).as_deref(),
        Some("mb-beatles"),
        "un MBID confirmé par un ALIAS a été retiré"
    );
    assert_eq!(
        mbid(&b, 4).as_deref(),
        Some("mb-magma"),
        "un MBID confirmé par le NOM a été retiré"
    );
    assert_eq!(
        mbid(&b, 5).as_deref(),
        Some("mb-bashung"),
        "un MBID confirmé par un CRÉDIT DE PRESSAGE a été retiré"
    );
    assert_eq!(mbid(&b, 7).as_deref(), Some("mb-air-balise"));
    assert_eq!(mbid(&b, 9).as_deref(), Some("mb-daho-pressage"));
    assert_eq!(
        mbid(&b, 8).as_deref(),
        Some("mb-inconnu"),
        "un MBID que MusicBrainz ne connaît pas (404) se garde"
    );
    // Seuls les candidats ont été demandés, une fois chacun.
    assert_eq!(
        d.chemins(),
        vec![
            "artist/mb-faux-christophe",
            "artist/mb-faux-ange",
            "artist/mb-beatles",
            "artist/mb-magma",
            "artist/mb-inconnu",
        ],
        "requêtes envoyées"
    );
    assert_eq!(etat.status, "done");
    assert!(etat.fin);
    assert_eq!(
        (etat.verifies, etat.retires, etat.confirmes, etat.inconnus),
        (5, 2, 2, 1)
    );
    // Le journal nomme chaque retrait.
    let journal = lire_le_journal(&b);
    assert_eq!(journal.len(), 2);
    assert_eq!(
        (journal[0].artist_id, journal[0].mbid.as_str()),
        (1, "mb-faux-christophe")
    );
    assert_eq!(journal[0].nom_musicbrainz, "Christophe Maé");
    assert_eq!((journal[1].artist_id, journal[1].nom.as_str()), (2, "Ange"));

    // Débit : au plus une requête par seconde (limiteur partagé).
    let heures = d.heures();
    for paire in heures.windows(2) {
        let ecart = paire[1].duration_since(paire[0]);
        assert!(
            ecart >= std::time::Duration::from_millis(900),
            "deux requêtes MusicBrainz à {ecart:?} d'écart : plus d'une par seconde"
        );
    }

    // Une passe finie ne repart pas : aucune requête de plus.
    let avant = d.chemins().len();
    let encore = executer_un_tour(&b, lire_la_fiche_musicbrainz).await;
    assert!(encore.fin);
    assert_eq!(
        d.chemins().len(),
        avant,
        "une passe finie a refait des requêtes"
    );
}

/// La garde de la base elle-même : `retirer_le_mbid_verifie` refuse un
/// `manuel`, un `balise`, un `pressage`, et un MBID qui a changé.
#[test]
fn le_retrait_est_garde_dans_la_requete() {
    let b = bibliotheque();
    let repo = ArtistRepo::with_backend(b.clone());
    assert!(
        !repo.retirer_le_mbid_verifie(6, "mb-gong-saisi").unwrap(),
        "la base a laissé retirer un MBID `manuel`"
    );
    assert!(!repo.retirer_le_mbid_verifie(7, "mb-air-balise").unwrap());
    assert!(!repo.retirer_le_mbid_verifie(9, "mb-daho-pressage").unwrap());
    assert!(
        !repo.retirer_le_mbid_verifie(1, "mb-autre").unwrap(),
        "un MBID changé depuis la lecture ne se retire pas"
    );
    assert!(
        repo.retirer_le_mbid_verifie(1, "MB-FAUX-CHRISTOPHE ")
            .unwrap()
    );
    assert_eq!(mbid(&b, 6).as_deref(), Some("mb-gong-saisi"));
}

/// Ce que le MBID a rapporté part ; le reste (image locale ou téléversée,
/// bio saisie) reste.
#[test]
fn seuls_le_portrait_et_la_bio_du_mbid_partent() {
    let b = base();
    b.execute_batch(
        "INSERT INTO artists (id, name, image_path, image_source, bio) VALUES (1, 'A', 'h1', 'local', 'Écrite à la main.'); \
         INSERT INTO artists (id, name, image_path, image_source) VALUES (2, 'B', 'h2', 'upload'); \
         INSERT INTO artists (id, name, image_path, image_source) VALUES (3, 'C', 'h3', 'lastfm');",
    )
    .unwrap();
    let repo = ArtistRepo::with_backend(b.clone());
    for id in 1..=3 {
        assert_eq!(
            repo.oublier_ce_que_le_mbid_a_apporte(id).unwrap(),
            (false, false)
        );
    }
    let a = repo.get(1).unwrap().unwrap();
    assert_eq!(a.image_path.as_deref(), Some("h1"));
    assert_eq!(a.bio.as_deref(), Some("Écrite à la main."));
}

/// Reprise après arrêt : douze refus d'affilée arrêtent le tour, le curseur
/// reste avant la série ; le tour suivant repart de là, sans redemander ce
/// qui a été vérifié.
#[tokio::test]
async fn la_reprise_repart_apres_la_derniere_fiche_verifiee() {
    let b = base();
    for id in 1..=14i64 {
        b.execute(
            "INSERT INTO artists (id, name, musicbrainz_id) VALUES (?, ?, ?)",
            &[
                &id as &dyn ToSqlValue,
                &format!("Artiste {id}") as &dyn ToSqlValue,
                &format!("mb-{id}") as &dyn ToSqlValue,
            ],
        )
        .unwrap();
    }
    let demandes: Arc<Mutex<Vec<String>>> = Arc::default();
    let d1 = demandes.clone();
    let etat = executer_un_tour(&b, move |m: String| {
        let mut d = d1.lock().unwrap();
        d.push(m.clone());
        let premier = d.len() == 1;
        std::future::ready(if premier {
            Ok(Some(fiche("Artiste 1", &[])))
        } else {
            Err(RefusMusicBrainz::Statut(503))
        })
    })
    .await;
    assert_eq!(etat.status, "stopped");
    assert_eq!(etat.raison.as_deref(), Some("musicbrainz_injoignable"));
    assert_eq!(
        etat.dernier_artiste_id, 1,
        "le curseur reste avant la série refusée"
    );
    assert_eq!(
        lire_l_etat(&b).dernier_artiste_id,
        1,
        "le curseur est rangé en base"
    );

    let d2 = demandes.clone();
    let etat = executer_un_tour(&b, move |m: String| {
        d2.lock().unwrap().push(m.clone());
        let nom = format!("Artiste {}", m.trim_start_matches("mb-"));
        std::future::ready(Ok(Some(fiche(&nom, &[]))))
    })
    .await;
    assert!(etat.fin);
    let toutes = demandes.lock().unwrap().clone();
    assert_eq!(
        toutes.iter().filter(|m| *m == "mb-1").count(),
        1,
        "la fiche déjà vérifiée a été redemandée après la reprise"
    );
    assert_eq!(etat.verifies, 14);
}

// ── « Détacher l'identité » : plus rien ne réattache ────────────────────────

#[tokio::test]
async fn une_identite_detachee_n_est_reattachee_par_aucune_passe() {
    let _banc = BANC.lock().await;
    let _fin = FinDuBanc;
    // La recherche par le nom rendrait, sûre d'elle, l'homonyme écarté.
    let d = doublure(
        fiches_de_la_doublure(),
        json!({"artists": [{"id": "mb-faux-ange", "name": "Ange", "score": 100}]}),
    )
    .await;
    let b = base();
    b.execute_batch(
        "INSERT INTO artists (id, name, musicbrainz_id, musicbrainz_id_source, image_path, image_source) \
           VALUES (1, 'Ange', 'mb-faux-ange', 'nom', 'h-chanteuse', 'auto'); \
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Le Cimetière des arlequins', 1);",
    )
    .unwrap();
    let repo = ArtistRepo::with_backend(b.clone());
    let lu_avant = repo.get(1).unwrap().unwrap();

    // Le geste.
    repo.poser_mbid_manuel(1, None).unwrap();
    assert_eq!(
        repo.oublier_ce_que_le_mbid_a_apporte(1).unwrap(),
        (true, false)
    );
    assert!(repo.identite_detachee(1).unwrap());
    assert_eq!(provenance(&b, 1), Some(ProvenanceMbid::Manuel));

    // 1. La recherche par le nom (images d'artistes, phase 1).
    let n = crate::metadata::matcher::batch_match_artist_mbids(b.clone()).await;
    assert_eq!(
        n, 0,
        "la recherche par le nom a réattaché une identité détachée"
    );
    assert_eq!(
        mbid(&b, 1),
        None,
        "la recherche par le nom a réattaché une identité détachée"
    );
    assert!(d.chemins().is_empty(), "requête pour une identité détachée");
    // 2. Le pressage.
    let poses = crate::metadata::artistes_du_pressage::poser_les_mbid(
        &b,
        &[(1, "mb-faux-ange".to_string())],
        ProvenanceMbid::Pressage,
    )
    .unwrap();
    assert_eq!(poses, 0, "le pressage a réattaché une identité détachée");
    // 3. La passe artistes par le réseau : hors candidats.
    assert!(
        crate::metadata::artistes_par_le_reseau::candidats(&b, 0, 100)
            .unwrap()
            .is_empty()
    );
    // 4. Le scan : une balise MBID sur le même nom ne la reprend pas.
    let par_le_scan = repo
        .get_or_create("Ange", Some("mb-faux-ange"), None)
        .unwrap();
    assert_eq!(par_le_scan.id, Some(1));
    assert_eq!(mbid(&b, 1), None);
    // 5. Une réécriture de fiche à partir d'une lecture ANCIENNE (image locale
    //    au scan, renommage) ne remet pas l'ancien MBID.
    repo.update(&lu_avant).unwrap();
    assert_eq!(
        mbid(&b, 1),
        None,
        "une réécriture de fiche a remis le MBID détaché"
    );
    assert_eq!(provenance(&b, 1), Some(ProvenanceMbid::Manuel));
    // 6. La reprise de fond : hors candidats.
    assert!(candidats(&b, 0, 100).unwrap().is_empty());
    // 7. L'absorption d'un doublon ne recopie pas son MBID.
    b.execute(
        "INSERT INTO artists (id, name, musicbrainz_id) VALUES (2, 'ANGE', 'mb-faux-ange')",
        &[],
    )
    .unwrap();
    repo.absorber(1, 2).unwrap();
    assert_eq!(
        mbid(&b, 1),
        None,
        "l'absorption a recopié le MBID d'un doublon sur une identité détachée"
    );

    // Une saisie manuelle redonne une identité, `manuel` aussi.
    repo.poser_mbid_manuel(1, Some("mb-groupe-ange")).unwrap();
    assert_eq!(mbid(&b, 1).as_deref(), Some("mb-groupe-ange"));
    assert!(!repo.identite_detachee(1).unwrap());
}

// ── Les mêmes écritures sur PostgreSQL ──────────────────────────────────────

/// Joué contre une vraie base (`TUNE_TEST_PG_URL`, schéma des scripts
/// numérotés jusqu'à la 097) : la provenance de chaque chemin, la garde
/// `manuel`, le retrait vérifié, la sélection de la reprise. SAUT sans URL.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_6463_provenance_du_mbid() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
    let b: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool));
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = b.execute(sql, &[]);
    }
    for table in ["track_credits", "tracks", "albums", "artists"] {
        b.execute(
            &format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE"),
            &[],
        )
        .unwrap();
    }
    let repo = ArtistRepo::with_backend(b.clone());
    // Balise.
    let air = repo
        .get_or_create("Air", Some("mb-air-balise"), None)
        .unwrap()
        .id
        .unwrap();
    assert_eq!(provenance(&b, air), Some(ProvenanceMbid::Balise));
    // Nom, puis une réécriture de fiche qui garde le MBID garde la provenance.
    let magma = repo.get_or_create("Magma", None, None).unwrap().id.unwrap();
    assert!(
        repo.poser_mbid(magma, "mb-magma", ProvenanceMbid::Nom)
            .unwrap()
    );
    let mut fiche_magma = repo.get(magma).unwrap().unwrap();
    fiche_magma.image_path = Some("h".into());
    repo.update(&fiche_magma).unwrap();
    assert_eq!(provenance(&b, magma), Some(ProvenanceMbid::Nom));
    // Pressage.
    let daho = repo.get_or_create("Daho", None, None).unwrap().id.unwrap();
    assert_eq!(
        crate::metadata::artistes_du_pressage::poser_les_mbid(
            &b,
            &[(daho, "mb-daho".to_string())],
            ProvenanceMbid::Pressage,
        )
        .unwrap(),
        1
    );
    assert_eq!(provenance(&b, daho), Some(ProvenanceMbid::Pressage));
    // Manuel : détaché, plus rien ne le touche.
    let ange = repo.get_or_create("Ange", None, None).unwrap().id.unwrap();
    repo.poser_mbid(ange, "mb-faux-ange", ProvenanceMbid::Nom)
        .unwrap();
    let lu_avant = repo.get(ange).unwrap().unwrap();
    repo.poser_mbid_manuel(ange, None).unwrap();
    assert!(repo.identite_detachee(ange).unwrap());
    assert!(
        !repo
            .poser_mbid(ange, "mb-faux-ange", ProvenanceMbid::Nom)
            .unwrap()
    );
    repo.update(&lu_avant).unwrap();
    assert_eq!(
        mbid(&b, ange),
        None,
        "PG : une réécriture a remis le MBID détaché"
    );
    assert!(
        repo.list_without_mbid()
            .unwrap()
            .iter()
            .all(|(id, _)| *id != ange)
    );
    // La reprise : candidats et retrait gardé.
    b.execute(
        "INSERT INTO artists (name, musicbrainz_id, image_path, image_source, bio, bio_source) \
         VALUES ('Christophe', 'mb-faux-christophe', 'h', 'auto', 'x', 'wikipedia')",
        &[],
    )
    .unwrap();
    let ids: Vec<String> = candidats(&b, 0, 100)
        .unwrap()
        .into_iter()
        .map(|c| c.nom)
        .collect();
    assert_eq!(ids, vec!["Magma".to_string(), "Christophe".to_string()]);
    let christophe = candidats(&b, 0, 100).unwrap()[1].id;
    assert!(
        repo.retirer_le_mbid_verifie(christophe, "MB-FAUX-CHRISTOPHE")
            .unwrap()
    );
    let c = repo.get(christophe).unwrap().unwrap();
    assert_eq!((c.musicbrainz_id, c.image_path, c.bio), (None, None, None));
    assert!(!repo.retirer_le_mbid_verifie(air, "mb-air-balise").unwrap());
    for table in ["track_credits", "tracks", "albums", "artists"] {
        b.execute(
            &format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE"),
            &[],
        )
        .unwrap();
    }
}
