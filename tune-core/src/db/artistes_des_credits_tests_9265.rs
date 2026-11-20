//! #9265 — règle stricte (pure), puis grille et page artiste sur une vraie
//! base SQLite, et PostgreSQL (`pg_9265_…`, saute sans `TUNE_TEST_PG_URL`).

use super::*;
use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::DbBackend;
use crate::db::track_repo::TrackRepo;

fn fiches(noms: &[&str]) -> Vec<(i64, String)> {
    noms.iter()
        .enumerate()
        .map(|(i, n)| (i as i64 + 1, n.to_string()))
        .collect()
}

/// Les crédits reliés, par nom, avec leurs artistes dans l'ordre.
fn relies(noms: &[&str], liste_blanche: &[String]) -> Vec<(String, Vec<String>)> {
    let f = fiches(noms);
    let nom = |id: i64| f.iter().find(|(i, _)| *i == id).unwrap().1.clone();
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for l in planifier(&f, liste_blanche) {
        let credit = nom(l.credit_id);
        match out.iter_mut().find(|(c, _)| *c == credit) {
            Some((_, v)) => v.push(nom(l.artiste_id)),
            None => out.push((credit, vec![nom(l.artiste_id)])),
        }
    }
    out
}

#[test]
fn un_credit_dont_tous_les_artistes_ont_une_fiche_est_relie() {
    let r = relies(
        &[
            "Bill Evans",
            "Jim Hall",
            "Bill Evans & Jim Hall",
            "Stan Getz",
            "João Gilberto",
            "Antonio Carlos Jobim",
            "Stan Getz and João Gilberto featuring Antonio Carlos Jobim",
            "Fryderyk Chopin",
            "Jacques Loussier",
            "Fryderyk Chopin; Jacques Loussier",
        ],
        &[],
    );
    assert_eq!(
        r,
        vec![
            (
                "Bill Evans & Jim Hall".to_string(),
                vec!["Bill Evans".to_string(), "Jim Hall".to_string()]
            ),
            (
                "Stan Getz and João Gilberto featuring Antonio Carlos Jobim".to_string(),
                vec![
                    "Stan Getz".to_string(),
                    "João Gilberto".to_string(),
                    "Antonio Carlos Jobim".to_string()
                ]
            ),
            (
                "Fryderyk Chopin; Jacques Loussier".to_string(),
                vec![
                    "Fryderyk Chopin".to_string(),
                    "Jacques Loussier".to_string()
                ]
            ),
        ]
    );
}

/// Le rapprochement est celui de `cle_artiste` : accents, casse, article.
#[test]
fn les_morceaux_se_rapprochent_par_la_cle_du_nom() {
    let r = relies(
        &["Stan Getz", "Joao Gilberto", "stan getz & JOÃO GILBERTO"],
        &[],
    );
    assert_eq!(r.len(), 1, "{r:?}");
}

/// Décision de Bertrand du 20/11 : règle stricte, « feat. » compris. Un
/// artiste sans fiche laisse le crédit entier, et aucune fiche n'est créée.
#[test]
fn un_seul_artiste_sans_fiche_laisse_le_credit_entier() {
    assert!(relies(&["Dave Brubeck", "Dave Brubeck feat. Paul Desmond"], &[]).is_empty());
    assert!(
        relies(
            &["Ella Fitzgerald", "Ella Fitzgerald & Louis Armstrong"],
            &[]
        )
        .is_empty()
    );
    assert!(relies(&["Art Blakey", "Art Blakey & Jazz Messengers"], &[]).is_empty());
}

/// Les crédits sales relevés sur le .18 le 20/11 (aperçu de découpage), avec
/// les fiches voisines que cette bibliothèque porte réellement : aucun ne se
/// relie, faute de fiche pour CHACUN de ses morceaux.
#[test]
fn les_credits_sales_du_18_ne_se_relient_pas() {
    let sales = [
        "Davis, Miles",
        "Cochran, Wayne C.C. Riders",
        "Carl Orff; Robert Shaw, Atlanta Symphony Orchestra & Chorus",
        "Verdi; Susan Dunn, Diane Curry, Jerry Hadley, Paul Plishka, Atlanta Symphony Orchestra & Chorus, Robert Shaw",
        "Jordi Savall, Sergi Casademunt, Eunice Brandão - violes de gambe",
        "Christophe Coin, Jordi Savall - violes de gambe",
        "Ariane Maurette, Jordi Savall - violes de gambe",
        "Diego Fernández Magdaleno, piano",
        "Hespèrion XX: Christophe Coin, Sergi Casademunt, Lorenz Duftschmid, Palo Pandolfo, Jose-Miguel Moreno, Jordi Savall",
        "Hespèrion XX : Sergi Casademunt, Eunice Brandão, Lorenz Duftschmid, Andrew Lawrence-King, Jordi Savall",
        "Montserrat Figueras, Maria Cristina Kiehr, Hespèrion XX - Jordi Savall",
        "Academy of Ancient Music, Richard Egarr harpsichord & direction",
        "Academy of Ancient Music, Richard Egarr dir.",
        "Soloists, choir and orchestra of The Netherlands Bach Society",
        "Berliner Philharmoniker, Ferenc Fricsay - Beethoven symphonie n°5, n°7 et n°9",
        "Jacobs, Lisa The String Soloists",
        "Jordi Savall, Alfia Bakieva & Les Musiciennes du Concert des Nations",
    ];
    let voisins = [
        "Miles Davis",
        "Robert Shaw",
        "Atlanta Symphony Orchestra",
        "Carl Orff",
        "Verdi",
        "Jordi Savall",
        "Sergi Casademunt",
        "Christophe Coin",
        "Lorenz Duftschmid",
        "Hespèrion XX",
        "Montserrat Figueras",
        "Academy of Ancient Music",
        "Richard Egarr",
        "Berliner Philharmoniker",
        "Ferenc Fricsay",
        "Alfia Bakieva",
        "Diego Fernández Magdaleno",
    ];
    let noms: Vec<&str> = voisins.iter().chain(sales.iter()).copied().collect();
    assert_eq!(relies(&noms, &[]), vec![], "aucun crédit sale ne se relie");
}

/// La liste blanche prime, même quand chaque morceau aurait une fiche.
#[test]
fn la_liste_blanche_garde_les_groupes_entiers() {
    let noms = [
        "Simon",
        "Garfunkel",
        "Simon & Garfunkel",
        "Earth",
        "Wind",
        "Fire",
        "Earth, Wind & Fire",
        "Crosby",
        "Stills",
        "Nash",
        "Young",
        "Crosby, Stills, Nash & Young",
        "Iron",
        "Wine",
        "Iron & Wine",
    ];
    let r = relies(&noms, &[]);
    assert_eq!(
        r.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(),
        vec!["Iron & Wine"],
        "seul le groupe hors liste blanche se découpe"
    );
    // Le réglage `artist_split_allowlist` le protège à son tour.
    assert!(relies(&noms, &["Iron & Wine".to_string()]).is_empty());
}

#[test]
fn various_artists_et_noms_simples_ne_se_relient_jamais() {
    assert!(
        relies(
            &["Various Artists", "Various", "Artists", "Miles Davis"],
            &[]
        )
        .is_empty()
    );
}

/// Les 594 noms du Core Roon d'un testeur (`tests/fixtures`), tous des
/// fiches : seules les deux vraies collaborations dont les deux artistes ont
/// leur fiche se relient. Les cinq noms de groupe que le découpeur risqué
/// coupe à tort (`RISQUE_CONNUS` du corpus) restent entiers.
#[test]
fn corpus_roon_seules_les_vraies_collaborations_se_relient() {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/corpus_artistes_roon.json"
    ))
    .unwrap();
    let mut noms: Vec<&str> = Vec::new();
    for cle in ["propres", "a_separateur_mais_entiers"] {
        for n in v[cle].as_array().unwrap() {
            noms.push(n.as_str().unwrap());
        }
    }
    assert_eq!(noms.len(), 594);
    let r = relies(&noms, &[]);
    let mut credits: Vec<&str> = r.iter().map(|(c, _)| c.as_str()).collect();
    credits.sort();
    assert_eq!(
        credits,
        vec!["Dave Gahan & Soulsavers", "Jimmy Page & Robert Plant"]
    );
}

// ---------------------------------------------------------------------------
// Base réelle : grille, page artiste, idempotence.

struct Base {
    a: i64,
    b: i64,
    a_et_b: i64,
    a_feat_x: i64,
    album_a: i64,
    album_ab: i64,
    album_ax: i64,
}

fn inserer_album(db: &Arc<dyn DbBackend>, titre: &str, artiste: i64) -> i64 {
    let titre_v = titre.to_string();
    let (id, sql_piste) = match db.engine() {
        super::super::engine::Engine::Postgres => (
            db.query_one(
                "INSERT INTO albums (title, artist_id) VALUES ($1, $2) RETURNING id",
                &[&titre_v as &dyn ToSqlValue, &artiste],
            )
            .unwrap()
            .unwrap()[0]
                .as_i64()
                .unwrap(),
            "INSERT INTO tracks (title, album_id, artist_id) VALUES ($1, $2, $3)",
        ),
        super::super::engine::Engine::Sqlite => (
            db.execute_returning_id(
                "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
                &[&titre_v as &dyn ToSqlValue, &artiste],
            )
            .unwrap(),
            "INSERT INTO tracks (title, album_id, artist_id) VALUES (?, ?, ?)",
        ),
    };
    let piste = format!("{titre} — piste");
    db.execute(sql_piste, &[&piste as &dyn ToSqlValue, &id, &artiste])
        .unwrap();
    id
}

fn peupler(db: &Arc<dyn DbBackend>, suffixe: &str) -> Base {
    let repo = ArtistRepo::with_backend(db.clone());
    let fiche = |n: &str| {
        repo.get_or_create(&format!("{n}{suffixe}"), None, None)
            .unwrap()
            .id
            .unwrap()
    };
    // Les noms portent le suffixe en fin de CHAQUE artiste, pour que le
    // découpage retombe sur les fiches.
    let a = fiche("Alpha Quintet");
    let b = fiche("Bravo Trio");
    let a_et_b = repo
        .get_or_create(
            &format!("Alpha Quintet{suffixe} & Bravo Trio{suffixe}"),
            None,
            None,
        )
        .unwrap()
        .id
        .unwrap();
    let a_feat_x = repo
        .get_or_create(
            &format!("Alpha Quintet{suffixe} feat. Xray Sans Fiche{suffixe}"),
            None,
            None,
        )
        .unwrap()
        .id
        .unwrap();
    let album_a = inserer_album(db, &format!("Album A{suffixe}"), a);
    let album_ab = inserer_album(db, &format!("Album AB{suffixe}"), a_et_b);
    let album_ax = inserer_album(db, &format!("Album AX{suffixe}"), a_feat_x);
    Base {
        a,
        b,
        a_et_b,
        a_feat_x,
        album_a,
        album_ab,
        album_ax,
    }
}

fn verifier(db: &Arc<dyn DbBackend>, base: &Base) {
    let bilan = recalculer(db, &[]).unwrap();
    assert!(bilan.liaisons >= 2, "{bilan:?}");
    let artistes = ArtistRepo::with_backend(db.clone());
    let grille: Vec<i64> = artistes
        .list(100_000, 0)
        .unwrap()
        .into_iter()
        .filter_map(|a| a.id)
        .collect();
    assert!(grille.contains(&base.a));
    assert!(
        grille.contains(&base.b),
        "B n'a que l'album « A & B » : il entre"
    );
    assert!(
        !grille.contains(&base.a_et_b),
        "le crédit relié sort de la grille"
    );
    assert!(
        grille.contains(&base.a_feat_x),
        "« feat. » d'un artiste sans fiche : crédit entier, gardé"
    );
    assert_eq!(
        artistes.count().unwrap() as usize,
        grille.len(),
        "le total suit la page"
    );

    let albums = AlbumRepo::with_backend(db.clone());
    let ids =
        |v: Vec<crate::db::models::Album>| v.into_iter().filter_map(|a| a.id).collect::<Vec<_>>();
    let de_a = ids(albums.list_by_artist(base.a).unwrap());
    assert!(de_a.contains(&base.album_a) && de_a.contains(&base.album_ab));
    assert!(!de_a.contains(&base.album_ax), "crédit non relié");
    assert_eq!(
        ids(albums.list_by_artist(base.b).unwrap()),
        vec![base.album_ab]
    );
    assert!(
        !ids(albums.list_appearances_of_artist(base.a).unwrap()).contains(&base.album_ab),
        "pas deux fois le même album sur la page"
    );
    // Le crédit d'origine garde sa fiche et son album.
    assert_eq!(
        ids(albums.list_by_artist(base.a_et_b).unwrap()),
        vec![base.album_ab]
    );

    let pistes = TrackRepo::with_backend(db.clone());
    let albums_des_pistes = |id: i64| {
        pistes
            .list_by_artist(id)
            .unwrap()
            .into_iter()
            .filter_map(|t| t.album_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(albums_des_pistes(base.b), vec![base.album_ab]);
    let de_a = albums_des_pistes(base.a);
    assert!(de_a.contains(&base.album_a) && de_a.contains(&base.album_ab));

    // Idempotente : la seconde passe n'écrit rien.
    let encore = recalculer(db, &[]).unwrap();
    assert!(!encore.ecrit, "{encore:?}");
    assert_eq!(encore.liaisons, bilan.liaisons);
}

fn sqlite() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

#[test]
fn grille_et_page_artiste_sqlite() {
    let db = sqlite();
    let base = peupler(&db, "");
    verifier(&db, &base);
    // Le crédit d'origine reste en base, intact.
    let credit = ArtistRepo::with_backend(db.clone())
        .get(base.a_et_b)
        .unwrap()
        .unwrap();
    assert_eq!(credit.name, "Alpha Quintet & Bravo Trio");
}

/// Sans aucune liaison, la grille est celle d'avant : tout artiste d'album.
#[test]
fn sans_recalcul_la_grille_ne_change_pas() {
    let db = sqlite();
    let base = peupler(&db, "");
    let grille: Vec<i64> = ArtistRepo::with_backend(db.clone())
        .list(100_000, 0)
        .unwrap()
        .into_iter()
        .filter_map(|a| a.id)
        .collect();
    assert!(grille.contains(&base.a_et_b));
    assert!(!grille.contains(&base.b));
}

/// Une fiche renommée ou supprimée : la passe suivante retire la liaison.
#[test]
fn une_fiche_disparue_defait_la_liaison() {
    let db = sqlite();
    let base = peupler(&db, "");
    recalculer(&db, &[]).unwrap();
    db.execute(
        "UPDATE artists SET name = 'Bravo Renomme' WHERE id = ?",
        &[&base.b as &dyn ToSqlValue],
    )
    .unwrap();
    let bilan = recalculer(&db, &[]).unwrap();
    assert!(bilan.ecrit);
    assert_eq!(bilan.liaisons, 0);
    let grille: Vec<i64> = ArtistRepo::with_backend(db.clone())
        .list(100_000, 0)
        .unwrap()
        .into_iter()
        .filter_map(|a| a.id)
        .collect();
    assert!(grille.contains(&base.a_et_b), "le crédit revient, entier");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_9265_grille_et_page_artiste() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = sqlx::raw_sql(*sql).execute(&pool).await;
    }
    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let base = tokio::task::spawn_blocking({
        let db = db.clone();
        move || {
            let base = peupler(&db, " pg9265");
            verifier(&db, &base);
            base
        }
    })
    .await
    .unwrap();
    let db2 = db.clone();
    tokio::task::spawn_blocking(move || {
        db2.execute("DELETE FROM artistes_des_credits", &[])
            .unwrap();
        for album in [base.album_a, base.album_ab, base.album_ax] {
            db2.execute(
                "DELETE FROM tracks WHERE album_id = $1",
                &[&album as &dyn ToSqlValue],
            )
            .unwrap();
            db2.execute(
                "DELETE FROM albums WHERE id = $1",
                &[&album as &dyn ToSqlValue],
            )
            .unwrap();
        }
        for id in [base.a, base.b, base.a_et_b, base.a_feat_x] {
            db2.execute(
                "DELETE FROM artists WHERE id = $1",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap();
        }
    })
    .await
    .unwrap();
}
