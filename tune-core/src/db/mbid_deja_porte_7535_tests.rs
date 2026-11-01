//! #7535 — « Phil Selway » et « Philip Selway » en deux fiches : la passe des
//! images (`library/artwork.rs`) et la phase MBID (`metadata/matcher.rs`)
//! cherchent le NOM sur MusicBrainz et posaient le MBID trouvé par
//! `ArtistRepo::poser_mbid`, sans regarder si une autre fiche le portait
//! déjà. Les deux fiches finissaient avec le même MBID, donc le même
//! portrait, sans jamais se réunir.
//!
//! Décision de Bertrand (31/10) : bloquer et proposer. Ici le blocage : un
//! MBID déjà porté par une AUTRE fiche n'est pas posé (journalisé). La
//! contre-épreuve : un MBID libre se pose, et une fiche qui porte déjà ce
//! MBID peut se le reposer.

use std::sync::Arc;

use super::artist_repo::{ArtistRepo, ProvenanceMbid};
use super::backend::DbBackend;

const PHILIP: &str = "a2d6f6d8-6a3c-4ec7-9b8d-3c6a0e9f7535";
const LIBRE: &str = "0b0b0b0b-1111-2222-3333-444444447535";

fn mbid(repo: &ArtistRepo, id: i64) -> Option<String> {
    repo.get(id).unwrap().and_then(|a| a.musicbrainz_id)
}

/// Le scénario, joué sur le moteur que porte `repo`.
fn scenario(repo: &ArtistRepo) {
    let philip = repo
        .get_or_create("Philip Selway", Some(PHILIP), None)
        .unwrap()
        .id
        .unwrap();
    let phil = repo
        .get_or_create("Phil Selway", None, None)
        .unwrap()
        .id
        .unwrap();
    assert_ne!(philip, phil, "deux clés différentes : deux fiches");

    // La passe des images : le MBID trouvé par le nom, déjà porté (casse et
    // blancs différents : MusicBrainz et les balises ne s'accordent pas
    // toujours).
    let trouve = format!("  {}  ", PHILIP.to_uppercase());
    assert!(
        !repo.poser_mbid(phil, &trouve, ProvenanceMbid::Nom).unwrap(),
        "#7535 : un MBID déjà porté par « Philip Selway » posé sur « Phil Selway »"
    );
    assert_eq!(mbid(repo, phil), None);
    // La phase MBID du matcher passe par `update_mbid`.
    repo.update_mbid(phil, PHILIP).unwrap();
    assert_eq!(
        mbid(repo, phil),
        None,
        "#7535 : update_mbid a posé un MBID déjà porté"
    );
    // Ce que le journal nomme.
    assert_eq!(
        repo.autre_fiche_du_mbid(phil, PHILIP).unwrap(),
        Some((philip, "Philip Selway".to_string()))
    );
    // La fiche qui le porte reste intacte.
    assert_eq!(mbid(repo, philip).as_deref(), Some(PHILIP));

    // Contre-épreuves : la fiche qui porte déjà le MBID peut se le reposer,
    // et un MBID libre se pose.
    assert!(
        repo.poser_mbid(philip, PHILIP, ProvenanceMbid::Nom)
            .unwrap()
    );
    assert_eq!(repo.autre_fiche_du_mbid(philip, PHILIP).unwrap(), None);
    assert!(repo.poser_mbid(phil, LIBRE, ProvenanceMbid::Nom).unwrap());
    assert_eq!(mbid(repo, phil).as_deref(), Some(LIBRE));
    // Un MBID vide reste une absence, il ne « rencontre » personne.
    let gong = repo.get_or_create("Gong", None, None).unwrap().id.unwrap();
    repo.update_mbid(gong, "   ").unwrap();
    assert_eq!(mbid(repo, gong), None);
}

#[test]
fn un_mbid_trouve_par_le_nom_deja_porte_n_est_pas_pose_sqlite() {
    let db = super::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(db);
    scenario(&ArtistRepo::with_backend(b));
}

/// La même garde sur PostgreSQL (`TUNE_TEST_PG_URL`, schéma des scripts
/// numérotés, comme `pg_6463`) : `$4`/`$5` et le `CAST(... AS TEXT)` de la
/// garde. Le banc est partagé : seules les fiches et les MBID du scénario
/// sont effacés avant. SAUT sans URL.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_un_mbid_trouve_par_le_nom_deja_porte_n_est_pas_pose() {
    use super::backend::ToSqlValue;
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
    let b: Arc<dyn DbBackend> = Arc::new(super::backend::PostgresBackend::new(pool));
    for sql in super::postgres::ENSURE_TABLES
        .iter()
        .chain(super::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = b.execute(sql, &[]);
    }
    for nom in ["Philip Selway", "Phil Selway", "Gong"] {
        b.execute(
            "DELETE FROM artists WHERE name = $1",
            &[&nom as &dyn ToSqlValue],
        )
        .unwrap();
    }
    b.execute(
        "DELETE FROM artists WHERE LOWER(musicbrainz_id) IN ($1, $2)",
        &[&PHILIP as &dyn ToSqlValue, &LIBRE],
    )
    .unwrap();
    scenario(&ArtistRepo::with_backend(b));
}
