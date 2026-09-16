//! End-to-end smoke tests for the `Arc<dyn DbBackend>` path running
//! against a real PostgreSQL instance.
//!
//! Gated on the `postgres` feature AND the `TUNE_TEST_PG_URL` env var.
//! Without that env var the tests are skipped — they're not part of
//! the default `cargo test` run.
//!
//! Run via `scripts/pg-e2e.sh` (spins up a disposable docker pg, applies
//! the migrations, exports the env var, then runs cargo).
//!
//! The tests intentionally focus on exercising the trait boundary —
//! one per repo, hitting `create` + one read path. Comprehensive
//! coverage stays in the SQLite tests; PG E2E proves the bridge.

#![cfg(all(test, feature = "postgres"))]

use std::sync::Arc;

use crate::db::backend::{DbBackend, PostgresBackend};

/// La base d'épreuve désignée par `TUNE_TEST_PG_URL`.
///
/// Rend `None` dans UN SEUL cas : la variable n'est pas posée. Le `cargo test`
/// ordinaire n'a pas de base, et ces épreuves n'y sont pas exécutées — c'est le
/// saut, et il est recensé dans `derive_des_garde_fous_2816.rs`.
///
/// ⚠️ Une variable **posée** dont la connexion échoue ne saute PAS : elle fait
/// TOMBER l'épreuve, en nommant l'adresse et l'erreur. Jusqu'au 09/09/2026, le
/// `.ok()?` d'ici avalait l'échec de connexion et rendait `None` : une étape de
/// `test-postgres.yml` dont la base était mal branchée affichait dix-neuf `ok`
/// sur dix-neuf épreuves qui n'avaient touché aucune base. C'est le « vert
/// contre rien » sous sa forme la plus trompeuse — la variable EST posée, donc
/// la garde de recensement voit le témoin comme exécuté, et rien ne l'a été.
/// Même doctrine que `pg_routes_serveur.rs` et `pg_2372_versions_par_piste.rs`.
async fn pg_backend() -> Option<Arc<dyn DbBackend>> {
    let url = std::env::var("TUNE_TEST_PG_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.unwrap_or_else(|e| {
        panic!(
            "TUNE_TEST_PG_URL est POSÉE ({url}) mais la connexion PostgreSQL \
             échoue : {e}\n\
             Un banc mal branché doit ROUGIR, jamais s'afficher vert : sans ce \
             refus, ces épreuves rendraient `ok` sans avoir touché une base."
        )
    });
    Some(Arc::new(PostgresBackend::new(pool)))
}

/// Garde d'exécution : `let db = pg_or_skip!();` en tête de chaque épreuve.
///
/// Sans base, l'épreuve s'ANNONCE sautée sur la sortie d'erreur puis rend la
/// main. Doit être appelé depuis un exécuteur Tokio : les méthodes de
/// `PostgresBackend` passent par `block_in_place` + `block_on`.
macro_rules! pg_or_skip {
    () => {
        match pg_backend().await {
            Some(db) => db,
            None => {
                eprintln!(
                    "SAUT : TUNE_TEST_PG_URL non posée — une épreuve de {} rend la main sans toucher aucune base.",
                    module_path!()
                );
                return;
            }
        }
    };
}

/// Truncate every table the tests touch so each test starts clean.
/// CASCADE handles the FK chain.
fn reset_schema(db: &Arc<dyn DbBackend>) {
    let tables = [
        "track_credits",
        "play_queue",
        "playlist_tracks",
        "playlists",
        "tracks",
        "albums",
        "artists",
        "zones",
        "listen_history",
    ];
    for table in tables {
        let sql = format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE");
        // ignore errors for tables that don't exist (older migration state)
        let _ = db.execute(&sql, &[]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_artists_round_trip() {
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = ArtistRepo::with_backend(db);

    let id = repo.create(&Artist::new("Miles Davis".into())).unwrap();
    assert!(id > 0);

    let fetched = repo.get(id).unwrap().unwrap();
    assert_eq!(fetched.name, "Miles Davis");

    let by_name = repo.get_by_name("miles davis").unwrap();
    assert_eq!(by_name.and_then(|a| a.id), Some(id));
}

/// #2258 — le compte des artistes hors du fonds communautaire, sur le VRAI
/// moteur PostgreSQL.
///
/// Deux `COUNT(*)` sans paramètre lié : rien à numéroter, donc rien à
/// désaligner entre les `?` de SQLite et les `$n` de PostgreSQL. Ce qui reste
/// à prouver ici, c'est que le `COUNT(*)` revient bien en `i64` à travers le
/// pont `SqlValue` — sur PostgreSQL il arrive en `bigint`, et un `as_i64` qui
/// retomberait sur son `unwrap_or(0)` rendrait deux zéros parfaitement
/// silencieux. C'est exactement le genre de vert contre rien que ce test
/// interdit : les nombres attendus sont NON NULS.
#[tokio::test(flavor = "multi_thread")]
async fn pg_hors_fonds_communautaire_compte_les_artistes_sans_mbid() {
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = ArtistRepo::with_backend(db);

    // Identifié + bio : téléversé, donc PAS compté.
    let mut identifie = Artist::new("Pink Floyd".into());
    identifie.musicbrainz_id = Some("83d91898-7763-47d7-b03b-b92132375c47".into());
    identifie.bio = Some("Groupe de rock anglais.".into());
    repo.create(&identifie).unwrap();

    // Bio SANS MBID : la passe d'envoi ne la verra jamais.
    let mut bio_orpheline = Artist::new("Alan Stivell".into());
    bio_orpheline.bio = Some("Harpiste et chanteur breton.".into());
    repo.create(&bio_orpheline).unwrap();

    // Ni bio ni MBID : même pas candidats au téléchargement.
    for nom in ["Bagad Kemper", "Sonerien Du"] {
        repo.create(&Artist::new(nom.into())).unwrap();
    }

    let hors = repo.hors_fonds_communautaire().unwrap();
    assert_eq!(hors.bios_non_partagees, 1);
    assert_eq!(hors.artistes_non_servis, 2);

    // Témoin : l'artiste identifié reste téléversé, exactement comme avant.
    let televerses = repo.artists_with_bio_and_mbid().unwrap();
    assert_eq!(televerses.len(), 1);
    assert_eq!(televerses[0].0, "Pink Floyd");
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_albums_round_trip() {
    use crate::db::album_repo::AlbumRepo;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Album, Artist};

    let db = pg_or_skip!();
    reset_schema(&db);
    let artist_repo = ArtistRepo::with_backend(db.clone());
    let aid = artist_repo.create(&Artist::new("Coltrane".into())).unwrap();

    let repo = AlbumRepo::with_backend(db);
    let mut album = Album::new("A Love Supreme".into());
    album.artist_id = Some(aid);
    album.year = Some(1965);
    let id = repo.create(&album).unwrap();

    let fetched = repo.get(id).unwrap().unwrap();
    assert_eq!(fetched.title, "A Love Supreme");
    assert_eq!(fetched.artist_name.as_deref(), Some("Coltrane"));

    // get_or_create — the read-then-write path that's specific to album.
    let again = repo
        .get_or_create("A Love Supreme", aid, Some(1965))
        .unwrap();
    assert_eq!(again.id, Some(id));
}

/// Preuve réelle sur le second dialecte pour #2458 : le MBID vide ne sert plus
/// d'identité et la réparation fail-closed exécute sa sélection + son UPDATE
/// dans une transaction PostgreSQL, pas seulement dans le fixture SQLite.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2458_empty_mbid_album_artist_repair() {
    use crate::db::album_repo::AlbumRepo;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Artist, Track};
    use crate::db::track_repo::TrackRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let artist_repo = ArtistRepo::with_backend(db.clone());

    let first = artist_repo
        .get_or_create("Classique - Saint-Saëns", Some(""), None)
        .unwrap();
    let second = artist_repo
        .get_or_create("Anouar Brahem", Some(""), None)
        .unwrap();
    assert_ne!(first.id, second.id, "un MBID vide ne doit pas être partagé");

    let wrong = artist_repo
        .create(&Artist::new("Ancien artiste collé".into()))
        .unwrap();
    let right = artist_repo
        .create(&Artist::new("Artiste unanime des pistes".into()))
        .unwrap();
    db.execute(
        "UPDATE artists SET musicbrainz_id = '' WHERE id = $1",
        &[&wrong],
    )
    .unwrap();

    let album_repo = AlbumRepo::with_backend(db.clone());
    let album = album_repo
        .get_or_create_for_folder("/music/pg2458", "PG 2458", wrong, None, None)
        .unwrap();
    let album_id = album.id.unwrap();
    let track_repo = TrackRepo::with_backend(db.clone());
    for number in 1..=2 {
        let mut track = Track::new(format!("Piste {number}"));
        track.album_id = Some(album_id);
        track.artist_id = Some(right);
        track.track_number = number;
        track.file_path = Some(format!("/music/pg2458/{number:02}.flac"));
        track_repo.create(&track).unwrap();
    }

    assert_eq!(album_repo.repair_empty_mbid_artist_collapses().unwrap(), 1);
    assert_eq!(
        album_repo.get(album_id).unwrap().unwrap().artist_id,
        Some(right)
    );
    reset_schema(&db);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_tracks_round_trip() {
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Artist, Track};
    use crate::db::track_repo::TrackRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let artist_repo = ArtistRepo::with_backend(db.clone());
    let aid = artist_repo
        .create(&Artist::new("Pink Floyd".into()))
        .unwrap();

    let repo = TrackRepo::with_backend(db);
    let mut track = Track::new("Time".into());
    track.artist_id = Some(aid);
    track.file_path = Some("/music/time.flac".into());
    track.duration_ms = 413_000;
    let id = repo.create(&track).unwrap();

    let fetched = repo.get(id).unwrap().unwrap();
    assert_eq!(fetched.title, "Time");
    assert_eq!(fetched.duration_ms, 413_000);

    // get_all_paths used to be sqlite_legacy — now goes through DbBackend.
    let paths = repo.get_all_paths().unwrap();
    assert!(paths.contains("/music/time.flac"));
}

/// #2168 — **la même facette profonde doit rendre le même ensemble sur les
/// DEUX moteurs.**
///
/// Une facette « contient » (genre, label, compositeur) assemble un `OU` de
/// `LIKE`. En chaîne plate, sa profondeur d'arbre vaut son nombre de termes :
/// SQLite refuse au-delà de 1 000 (`Expression tree is too large`) tandis que
/// PostgreSQL accepte la même chaîne. Le filtre rendait donc la bonne liste ici
/// et une liste VIDE là-bas — la divergence entre moteurs que
/// `facet_filter::ou_equilibre` supprime.
///
/// Le jumeau SQLite de cette épreuve est
/// `facettes_multivaleurs::une_facette_a_mille_valeurs_rend_encore_lunion`
/// (crate `tune-server`). Les deux mesurent le MÊME fait — l'ensemble rendu —
/// pour qu'une correction d'un seul côté se voie.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2168_facette_profonde_rend_le_meme_ensemble_que_sqlite() {
    use crate::db::facet_filter::TrackFilter;
    use crate::db::models::Track;
    use crate::db::track_repo::TrackRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = TrackRepo::with_backend(db);

    // 3 Jazz, 2 Rock, 2 Blues.
    for (titre, genre) in [
        ("J1", "Jazz"),
        ("J2", "Jazz"),
        ("J3", "Jazz"),
        ("R1", "Rock"),
        ("R2", "Rock"),
        ("B1", "Blues"),
        ("B2", "Blues"),
    ] {
        let mut t = Track::new(titre.into());
        t.file_path = Some(format!("/music/{titre}.flac"));
        t.duration_ms = 1000;
        t.format = Some("flac".into());
        t.genre = Some(genre.into());
        repo.create(&t).unwrap();
    }

    let titres = |f: &TrackFilter| -> Vec<String> {
        let (items, total) = repo.list_filtered(f, 100, 0).unwrap();
        assert_eq!(items.len() as i64, total, "la liste doit tenir son total");
        let mut v: Vec<String> = items.into_iter().map(|t| t.title).collect();
        v.sort();
        v
    };

    // Le témoin : deux valeurs, l'union, sans doublon.
    let deux = TrackFilter {
        genres: vec!["Jazz".into(), "Rock".into()],
        ..Default::default()
    };
    assert_eq!(titres(&deux), vec!["J1", "J2", "J3", "R1", "R2"]);

    // La même sélection noyée dans 1 500 valeurs qui ne désignent rien : sur
    // SQLite, la chaîne plate échouait ici et rendait zéro piste.
    let mut profond = deux.clone();
    profond.genres.extend((0..1500).map(|i| format!("z{i}")));
    assert_eq!(
        titres(&profond),
        titres(&deux),
        "PostgreSQL doit rendre exactement le même ensemble que la sélection à deux valeurs"
    );

    // Témoin ET : (Jazz OU Rock) ET un format absent ne rend rien — le OU de la
    // facette ne doit pas déborder sur le ET.
    let mut croise = profond.clone();
    croise.formats = vec!["aiff".into()];
    assert!(titres(&croise).is_empty());

    // Témoin ZÉRO : une facette vide ne filtre rien.
    let rien = TrackFilter::default();
    assert!(!rien.is_active());
}

/// #3101 — la portée de répertoire sur le SECOND moteur : sélectionner un
/// dossier rend ce dossier et rien d'autre, jokers compris.
///
/// Le jumeau SQLite est `portee_repertoire_jokers` (crate `tune-server`), qui
/// mesure la même chose à travers la route `GET /library/tracks?folder=`. Cette
/// route-là vit dans `tune-server`, que la matrice `Test (PostgreSQL)` ne
/// compile pas : la mesure sur PostgreSQL porte donc sur la fonction que la
/// route appelle, `TrackRepo::list_filtered`, et sur le même fait de base —
/// **l'ensemble des fichiers rendus**.
///
/// Sans échappement, `%` et `_` restaient les jokers de `LIKE` : `100% Live`
/// ramenait `1000/Best Of Live` (un autre sous-arbre, le `%` traversant les
/// séparateurs) et `Disc_1` ramenait `DiscX1`.
#[tokio::test(flavor = "multi_thread")]
async fn pg_3101_les_jokers_du_nom_de_dossier_ne_filtrent_pas_plus_large() {
    use crate::db::facet_filter::TrackFilter;
    use crate::db::models::Track;
    use crate::db::track_repo::TrackRepo;
    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = TrackRepo::with_backend(db);

    // Mêmes effectifs que l'épreuve SQLite : 2 + 1 + 3 + 4 + 5 = 15, toutes
    // sommes deux à deux distinctes pour qu'aucun compte juste ne le soit par
    // accident.
    let mut n = 0;
    for (dossier, combien) in [
        ("/musique/100% Live", 2),
        ("/musique/100% Live/Bonus", 1),
        ("/musique/1000/Best Of Live", 3),
        ("/musique/Disc_1", 4),
        ("/musique/DiscX1", 5),
    ] {
        for _ in 0..combien {
            n += 1;
            let mut t = Track::new(format!("P{n}"));
            t.file_path = Some(format!("{dossier}/p{n}.flac"));
            t.duration_ms = 1000;
            t.format = Some("flac".into());
            repo.create(&t).unwrap();
        }
    }
    assert_eq!(n, 15);

    let sous = |dossier: &str| -> Vec<String> {
        let f = TrackFilter {
            folder: Some(dossier.to_string()),
            ..Default::default()
        };
        assert!(f.is_active(), "un dossier doit activer le chemin filtré");
        let (items, total) = repo.list_filtered(&f, 500, 0).unwrap();
        assert_eq!(
            items.len() as i64,
            total,
            "le compteur partage le prédicat de la liste"
        );
        let mut v: Vec<String> = items.into_iter().filter_map(|t| t.file_path).collect();
        v.sort();
        v
    };

    // LE FAIT : `%` dans le nom ne fait pas déborder la portée sur un voisin.
    let cent_pour_cent = sous("/musique/100% Live");
    assert!(
        cent_pour_cent
            .iter()
            .all(|f| f.starts_with("/musique/100% Live/")),
        "des fichiers hors du répertoire sélectionné : {cent_pour_cent:?}"
    );
    assert_eq!(cent_pour_cent.len(), 3, "{cent_pour_cent:?}");

    // Et `_`, qui ne vaut qu'un caractère : `Disc_1` n'est pas `DiscX1`.
    let disc = sous("/musique/Disc_1");
    assert!(
        disc.iter().all(|f| f.starts_with("/musique/Disc_1/")),
        "des fichiers hors du répertoire sélectionné : {disc:?}"
    );
    assert_eq!(disc.len(), 4, "{disc:?}");

    // Témoin imbriqué : le sous-dossier rend son contenu, pas celui du parent.
    let bonus = sous("/musique/100% Live/Bonus");
    assert_eq!(bonus, vec!["/musique/100% Live/Bonus/p3.flac".to_string()]);

    // Témoin vide : zéro proprement, pas une erreur.
    assert!(sous("/musique/Vide").is_empty());

    // Témoin sans portée : toute la bibliothèque, toujours.
    let rien = TrackFilter::default();
    assert!(!rien.is_active());
    let (tout, total) = repo.list_filtered(&rien, 500, 0).unwrap();
    assert_eq!(tout.len(), 15);
    assert_eq!(total, 15);
}

/// #1752 — l'antislash de Windows reste LITTÉRAL sur PostgreSQL, maintenant que
/// la clause n'est plus `ESCAPE ''` mais `ESCAPE '\'`.
///
/// C'était le défaut d'origine : Postgres traite l'antislash comme son
/// caractère d'échappement par défaut, donc un motif brut `G:\Blues 2\%` se
/// dégradait en la chaîne littérale `G:Blues 2%` et tous les répertoires
/// annonçaient « 0 piste » (JF, Windows + Postgres). L'ancienne réponse coupait
/// l'échappement ; la nouvelle DOUBLE l'antislash dans la valeur.
///
/// L'épreuve construit le motif à la main parce que `folder_like_pattern` pose
/// le séparateur de l'HÔTE : sur le Linux qui fait tourner cette suite, elle ne
/// produirait jamais d'antislash. Ce qui est écrit ici est, caractère pour
/// caractère, ce qu'elle produit sur un hôte Windows.
#[tokio::test(flavor = "multi_thread")]
async fn pg_1752_l_antislash_de_windows_reste_litteral() {
    use crate::db::backend::ToSqlValue;
    use crate::db::models::Track;
    use crate::db::track_repo::{TrackRepo, echapper_jokers_like, like_escape_clause};
    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = TrackRepo::with_backend(db.clone());

    for (n, chemin) in [
        r"G:\Blues 2\aa.flac",
        r"G:\Blues 2\Sous\bb.flac",
        r"G:\Jazz\cc.flac",
    ]
    .iter()
    .enumerate()
    {
        let mut t = Track::new(format!("W{n}"));
        t.file_path = Some((*chemin).to_string());
        t.duration_ms = 1000;
        repo.create(&t).unwrap();
    }

    // Ce que `folder_like_pattern` produit sur un hôte Windows pour `G:\Blues 2`.
    let motif = format!(
        "{}{}%",
        echapper_jokers_like(r"G:\Blues 2"),
        echapper_jokers_like("\\")
    );
    assert_eq!(motif, r"G:\\Blues 2\\%", "antislashs doublés, `%` final nu");

    let sql = format!(
        "SELECT COUNT(*) FROM tracks WHERE file_path LIKE $1{}",
        like_escape_clause()
    );
    let compte = db
        .query_one(&sql, &[&motif as &dyn ToSqlValue])
        .unwrap()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1);
    assert_eq!(
        compte, 2,
        "le sous-arbre `G:\\Blues 2` porte deux pistes ; 0 = l'antislash a été \
         consommé comme échappement (#1752), 3 = le motif ne filtre plus rien"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_zones_round_trip() {
    use crate::db::zone_repo::ZoneRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = ZoneRepo::with_backend(db);

    let id = repo
        .create("Living Room", Some("dlna"), Some("uuid:1"))
        .unwrap();
    let z = repo.get(id).unwrap().unwrap();
    assert_eq!(z.name, "Living Room");
    assert_eq!(z.volume, 50.0);

    repo.update_volume(id, 75.0).unwrap();
    assert_eq!(repo.get(id).unwrap().unwrap().volume, 75.0);
    // #2886 — la colonne est a virgule des DEUX cotes. Sur PG c'est la
    // migration 048 qui le garantit : sans elle, ecrire un f64 dans une
    // colonne `integer` echoue purement et simplement.
    repo.update_volume(id, 0.398_107_170_553_497_2 * 100.0)
        .unwrap();
    let relu = repo.get(id).unwrap().unwrap().volume / 100.0;
    assert!(
        (relu - 0.398_107_170_553_497_2).abs() < 1e-12,
        "-8 dB persiste puis relu a {relu}"
    );
    repo.update_volume(id, 10f64.powf(-48.0 / 20.0) * 100.0)
        .unwrap();
    assert!(
        repo.get(id).unwrap().unwrap().volume > 0.0,
        "-48 dB : la zone se rallumerait MUETTE sur PostgreSQL"
    );
    repo.update_volume(id, 75.0).unwrap();

    // The WAL fallback `query_many_strong` doesn't change behavior on
    // PG (same pool either way) — confirm list() works.
    let all = repo.list().unwrap();
    assert_eq!(all.len(), 1);
}

/// #3726 — les ECRITURES de zone et de profil, sur une VRAIE base PostgreSQL.
///
/// Le frère `pg_sqlite_type_parity` compare des SCHEMAS. Il ne voit pas un
/// rédacteur : le jour où quelqu'un réécrit `let val: String = if online …`,
/// le schéma ne bouge pas, la porte de parité reste VERTE, et l'écriture
/// redevient muette sur tout le parc. C'est ce témoin-ci qui garde les
/// rédacteurs, et il ne se nourrit pas lui-même — il appelle les méthodes du
/// dépôt, pas le SQL qu'il aurait recopié.
///
/// Ce que mesurait #3726, le 11/09/2026, sur PostgreSQL 16.15 :
///
/// | site | base native | base migrée |
/// |---|---|---|
/// | `update_muted` / `update_online` / `set_online_by_device` | `column … is of type smallint but expression is of type text` | passait |
/// | `update_gapless_enabled` / `update_fixed_volume` / `update_autoplay_enabled` | idem (smallint / integer) | passait |
/// | `update_dsp` | `column "dsp_preset_id" is of type bigint but expression is of type text` | MÊME ERREUR |
/// | `count()` / `count_online()` / `count_active()` / `list()` | `COALESCE types text and integer cannot be matched` | idem |
/// | INSERT profil SSO | `column "is_admin" is of type smallint but expression is of type boolean` | écrivait `true` en toutes lettres |
///
/// Les trois contre-épreuves de la fin REJOUENT les formes d'avant et EXIGENT
/// l'erreur PostgreSQL littérale. Sans elles, ce témoin serait vert le jour où
/// la colonne redeviendrait TEXT — il ne prouverait que l'accord du schéma avec
/// lui-même.
#[tokio::test(flavor = "multi_thread")]
async fn pg_3726_ecritures_de_zone_et_de_profil() {
    use crate::db::backend::ToSqlValue;
    use crate::db::zone_repo::ZoneRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = ZoneRepo::with_backend(db.clone());

    let id = repo
        .create("Salon", Some("dlna"), Some("uuid:3726"))
        .unwrap();

    // ── Les six réglages qui liaient une CHAÎNE dans une colonne numérique ──
    repo.update_muted(id, true).expect("update_muted");
    repo.update_online(id, true).expect("update_online");
    repo.update_gapless_enabled(id, false)
        .expect("update_gapless_enabled");
    repo.update_fixed_volume(id, true)
        .expect("update_fixed_volume");
    repo.update_autoplay_enabled(id, true)
        .expect("update_autoplay_enabled");
    repo.set_online_by_device("uuid:3726", true)
        .expect("set_online_by_device");

    let z = repo.get(id).unwrap().unwrap();
    assert!(z.muted, "muted n'est pas relu à 1");
    assert!(z.online, "online n'est pas relu à 1");
    assert!(!z.gapless_enabled, "gapless_enabled n'est pas relu à 0");
    assert!(z.fixed_volume, "fixed_volume n'est pas relu à 1");
    // `autoplay_enabled` est délibérément absente de `COLS` (le commentaire de
    // `sql::COLS` dit pourquoi) : `row_to_zone` la rend toujours `false`. C'est
    // `get_autoplay_enabled` qui lit la colonne.
    assert!(
        repo.get_autoplay_enabled(id),
        "autoplay_enabled n'est pas relu à 1"
    );

    // ── #3726 point 1 : `update_dsp`, mort sur TOUT PostgreSQL ──────────────
    repo.update_dsp(id, Some(7), true).expect("update_dsp");
    assert_eq!(
        repo.get_dsp_config(id).expect("get_dsp_config"),
        (Some(7), true),
        "le réglage DSP de zone ne se relit pas"
    );
    repo.update_dsp(id, None, false).expect("update_dsp(None)");
    assert_eq!(
        repo.get_dsp_config(id).expect("get_dsp_config"),
        (None, false),
        "effacer le préréglage DSP ne se relit pas"
    );

    // ── #3726 point 3 : les comptes de zones, et la SUPPRESSION ─────────────
    assert_eq!(repo.count().expect("count"), 1);
    assert_eq!(repo.count_online().expect("count_online"), 1);
    assert_eq!(repo.list().expect("list").len(), 1);

    repo.delete(id).expect("delete");
    // Le cœur du défaut : `delete` est un masquage. Tant que `is_hidden` était
    // TEXT, la requête filtrée tombait, le `Err(_)` attrape-tout de `list()` se
    // rabattait sur `list_all()`, et la zone supprimée REPARAISSAIT.
    assert_eq!(
        repo.list().expect("list après delete").len(),
        0,
        "la zone supprimée reparaît : `list()` s'est rabattue sur `list_all()`"
    );
    assert_eq!(repo.count().expect("count après delete"), 0);
    assert_eq!(repo.count_online().expect("count_online après delete"), 0);
    assert!(
        repo.is_device_hidden("uuid:3726"),
        "is_device_hidden ne voit pas la zone masquée"
    );
    repo.unhide(id).expect("unhide");
    assert_eq!(repo.count().expect("count après unhide"), 1);

    // ── #3726 point 2 : le profil SSO ───────────────────────────────────────
    let _ = db.execute("DELETE FROM profiles WHERE username = 'sso@3726'", &[]);
    let is_admin: i64 = i64::from(true);
    let nom = "sso@3726";
    let couleur = "#6366f1";
    db.execute_returning_id(
        "INSERT INTO profiles (username, display_name, email, avatar_path, is_admin) \
         VALUES (?, ?, ?, ?, ?)",
        &[
            &nom as &dyn ToSqlValue,
            &nom as &dyn ToSqlValue,
            &nom as &dyn ToSqlValue,
            &couleur as &dyn ToSqlValue,
            &is_admin as &dyn ToSqlValue,
        ],
    )
    .expect("création de profil SSO");
    let relu = db
        .query_one(
            "SELECT is_admin FROM profiles WHERE username = ?",
            &[&nom as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        relu.first().and_then(|v| v.as_bool()),
        Some(true),
        "`as_bool()` rend None sur un SqlValue::Text : un administrateur se \
         connecterait avec le rôle `user`"
    );
    let _ = db.execute("DELETE FROM profiles WHERE username = 'sso@3726'", &[]);

    // ── CONTRE-ÉPREUVES : les formes d'AVANT doivent encore être refusées ───
    //
    // Elles prouvent que ce témoin garde la réparation et pas seulement
    // l'accord du schéma avec lui-même : si `zones.online` redevenait TEXT, ces
    // trois-là passeraient et le test tomberait ici.
    let texte = "1";
    let erreur = db
        .execute(
            "UPDATE zones SET online = ? WHERE id = ?",
            &[&texte as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        )
        .expect_err("lier une CHAÎNE dans `zones.online` doit être REFUSÉ");
    assert!(
        erreur.contains("is of type smallint") && erreur.contains("is of type text"),
        "erreur inattendue pour `online <- text` : {erreur}"
    );

    let erreur = db
        .execute(
            "UPDATE zones SET dsp_preset_id = ? WHERE id = ?",
            &[&texte as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        )
        .expect_err("lier une CHAÎNE dans `zones.dsp_preset_id` doit être REFUSÉ");
    assert!(
        erreur.contains("is of type bigint") && erreur.contains("is of type text"),
        "erreur inattendue pour `dsp_preset_id <- text` : {erreur}"
    );

    let booleen = true;
    let erreur = db
        .execute(
            "UPDATE profiles SET is_admin = ? WHERE id = 1",
            &[&booleen as &dyn ToSqlValue],
        )
        .expect_err("lier un BOOLÉEN dans `profiles.is_admin` doit être REFUSÉ");
    assert!(
        erreur.contains("is of type smallint") && erreur.contains("is of type boolean"),
        "erreur inattendue pour `is_admin <- boolean` : {erreur}"
    );

    repo.delete(id).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_playlists_round_trip() {
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Artist, Track};
    use crate::db::playlist_repo::PlaylistRepo;
    use crate::db::track_repo::TrackRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let artist_repo = ArtistRepo::with_backend(db.clone());
    let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();

    let track_repo = TrackRepo::with_backend(db.clone());
    let mut t = Track::new("Song".into());
    t.artist_id = Some(aid);
    t.file_path = Some("/song.flac".into());
    let tid = track_repo.create(&t).unwrap();

    let repo = PlaylistRepo::with_backend(db);
    let plid = repo.create("My PL", None, 1).unwrap();
    // add_tracks uses write_tx — exercises the tx bridge.
    let inserted = repo.add_tracks(plid, &[tid], None).unwrap();
    assert_eq!(inserted, vec![tid]);

    let ids = repo.get_track_ids(plid).unwrap();
    assert_eq!(ids, vec![tid]);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_history_round_trip() {
    use crate::db::history_repo::{HistoryRepo, ListenRecord};

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = HistoryRepo::with_backend(db);

    let rec = ListenRecord {
        id: None,
        track_id: None,
        title: "So What".into(),
        artist_name: Some("Miles".into()),
        album_title: Some("Kind of Blue".into()),
        source: "local".into(),
        source_id: None,
        album_id: None,
        duration_ms: 560_000,
        listened_at: None,
        zone_id: None,
        cover_url: None,
        profile_id: None,
        context_type: None,
        context_id: None,
        context_position: None,
    };
    repo.record(&rec).unwrap();
    repo.record(&rec).unwrap();

    let recent = repo.recent(10).unwrap();
    assert_eq!(recent.len(), 2);

    let dashboard = repo.dashboard().unwrap();
    assert_eq!(dashboard.total_listens, 2);

    // listening_history uses the date helpers — confirms PG branch
    // of since_days / date_trunc_day.
    let days = repo.listening_history(7).unwrap();
    assert!(
        !days.is_empty(),
        "expected at least one day in 7-day window"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_settings_round_trip() {
    use crate::db::settings_repo::SettingsRepo;

    let db = pg_or_skip!();
    // settings table not in 001 — but settings_repo handles its own
    // schema bootstrap via the migration runner? No, the schema is
    // expected to be present. Skip if not.
    let exists = db
        .query_one(
            "SELECT 1 FROM information_schema.tables WHERE table_name = 'settings'",
            &[],
        )
        .unwrap_or(None);
    if exists.is_none() {
        eprintln!("settings table missing on PG — skipping");
        return;
    }
    let _ = db.execute("TRUNCATE TABLE settings", &[]);
    let repo = SettingsRepo::with_backend(db);

    repo.set("music_dirs", r#"["/music"]"#).unwrap();
    assert_eq!(
        repo.get("music_dirs").unwrap().as_deref(),
        Some(r#"["/music"]"#)
    );
    repo.delete("music_dirs").unwrap();
    assert!(repo.get("music_dirs").unwrap().is_none());
}

/// Regression for forum #1220 (tester Jean-François, PostgreSQL backend): a
/// SQLite→PG data-migrated database had its numeric columns created as TEXT,
/// so the force-scan album lookup `... WHERE year = $int` threw
/// `operator does not exist: text = bigint` and EVERY album write failed
/// (22841 failures, +0 added). The heal chain (010 albums/tracks, 011
/// listen_history, 013 the rest) converts those columns back to their numeric
/// types at startup. This test asserts the post-migration schema is numeric and
/// that the exact failing query pattern now runs cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn pg_1220_numeric_columns_have_numeric_types() {
    let db = pg_or_skip!();

    // (table, column, acceptable PG data_type). Columns from later migrations
    // may be absent on a partial schema — such rows are skipped, not failed.
    let expected: &[(&str, &str, &[&str])] = &[
        // 010 (albums/tracks)
        ("albums", "year", &["integer"]),
        ("albums", "disc_count", &["integer"]),
        ("albums", "sample_rate", &["integer"]),
        ("tracks", "duration_ms", &["bigint"]),
        ("tracks", "track_number", &["integer"]),
        ("tracks", "bpm", &["double precision"]),
        // 011 (listen_history)
        ("listen_history", "duration_ms", &["bigint"]),
        // 013 (the rest)
        // #2886 — a virgule : l'entier coupait le son sous -46,02 dB.
        ("zones", "volume", &["double precision"]),
        ("zones", "last_position_ms", &["bigint"]),
        // `bigint` accepte AUSSI (#3569). Ce que #1220 refuse, c'est une
        // colonne restee TEXT — la these de l'epreuve est ecrite en tete. Or
        // `queue_items` a un SECOND redacteur legitime : le DDL auto-reparateur
        // de `postgres.rs` (`ensure_schema`), qui declare deliberement TOUT en
        // BIGINT. Des que `pg_1706` laisse ce DDL recreer la table — ce qu'il
        // fait exprès —, la colonne est bigint, et l'ancienne liste faisait
        // tomber cette epreuve-ci sur l'ordre des epreuves. Les deux redacteurs
        // divergent bel et bien, et RIEN ne les compare : `pg_schema_parity`
        // (#2111) confronte les scripts numerotes a `PG_FULL_SCHEMA`, jamais a
        // `ensure_schema`. Cet angle mort est instruit a part.
        ("queue_items", "position", &["integer", "bigint"]),
        ("track_source_links", "confidence", &["double precision"]),
        ("bookmarks", "position_ms", &["bigint"]),
    ];

    for (table, col, ok_types) in expected {
        let t = table.to_string();
        let cc = col.to_string();
        let row = db
            .query_one(
                "SELECT data_type FROM information_schema.columns \
                 WHERE table_name = $1 AND column_name = $2",
                &[&t, &cc],
            )
            .unwrap();
        let Some(cols) = row else {
            continue; // table/column not present in this schema — skip
        };
        let dt = cols[0].as_str().unwrap_or("").to_string();
        assert!(
            ok_types.contains(&dt.as_str()),
            "{table}.{col} is `{dt}`, expected one of {ok_types:?} — heal migration missing/incomplete"
        );
    }

    // The exact #1220 failing pattern: `year` bound as an integer parameter.
    // On a TEXT column this raised `operator does not exist: text = bigint`;
    // after the heal it must execute without error.
    db.query_many("SELECT id FROM albums WHERE year = $1 LIMIT 1", &[&2020i32])
        .expect("WHERE year = $int must not raise `text = bigint` after the heal");
}

/// #2468 — contre le chemin reel d'une base deja installee : 005 a cree
/// `bookmarks.position_ms` en INTEGER et 013 a enregistre son passage sans la
/// toucher. La migration suivante doit etre jouee par le runner du binaire,
/// convertir sans perte, puis permettre une position superieure a i32::MAX.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2468_runner_heals_bookmarks_position_integer_to_bigint() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL not set, skipping PG E2E test");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();

    // `>= 36` et non `= 36` (#3569). Le runner ne rejoue pas une migration
    // manquante : il compare chaque version a `MAX(version)` et saute tout ce
    // qui est <= (voir `run_pg_migrations`). Retirer la SEULE ligne 36 laissait
    // donc le filigrane a 51, le runner sautait 036, et l'epreuve tombait sur
    // `left: "integer"`. Elle passait quand elle a ete ecrite parce que 036
    // etait alors la DERNIERE migration : la premiere 037 l'a cassee en
    // silence, et personne ne l'a vu — aucune etape ne l'executait. Les scripts
    // numerotes sont idempotents (c'est la these de tout ce runner), donc
    // rejouer 036..051 est sans effet de bord.
    sqlx::raw_sql(
        "DELETE FROM bookmarks;
         ALTER TABLE bookmarks
             ALTER COLUMN position_ms TYPE INTEGER
             USING position_ms::integer;
         DELETE FROM schema_version WHERE version >= 36;",
    )
    .execute(&pool)
    .await
    .expect("le drift INTEGER de #2468 doit pouvoir etre reproduit");

    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .expect("le runner doit appliquer la migration 036");

    let data_type: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND table_name = 'bookmarks'
            AND column_name = 'position_ms'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(data_type, "bigint");

    let large_position = i64::from(i32::MAX) + 1;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO bookmarks (position_ms, label)
         VALUES ($1, 'pg-2468-i64')
         RETURNING id",
    )
    .bind(large_position)
    .fetch_one(&pool)
    .await
    .expect("bookmarks.position_ms doit accepter toute valeur i64");
    let stored: i64 = sqlx::query_scalar("SELECT position_ms FROM bookmarks WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, large_position);
    sqlx::query("DELETE FROM bookmarks WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
}

/// #1706 — reproduces the exact .15 production drift and proves `ensure_schema`
/// heals it instead of dying on the first bad statement.
///
/// The drift: `streaming_favorites.id` is BIGINT (migration 012 converts the
/// TEXT ids of a SQLite→PG migrated database back to bigint + sequence), while
/// `ensure_schema` re-imposed a `nextval(...)::text` DEFAULT on it. Because the
/// whole self-healing DDL went out as ONE multi-statement query — one implicit
/// transaction — that single failure discarded everything, including the
/// `CREATE TABLE queue_items`. And that CREATE was itself missing
/// track_number/disc_number, which every streaming queue write names.
/// Net effect on .15: `queue_restore_append_failed` for 9 zones, every boot.
#[tokio::test(flavor = "multi_thread")]
async fn pg_1706_ensure_schema_heals_queue_items_numbering() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL not set, skipping PG E2E test");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();

    // Rebuild the broken pre-fix state.
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS queue_items CASCADE;
         DROP TABLE IF EXISTS streaming_favorites CASCADE;
         DROP SEQUENCE IF EXISTS streaming_favorites_id_seq;
         CREATE TABLE streaming_favorites (
             id BIGINT PRIMARY KEY,
             profile_id TEXT NOT NULL DEFAULT '1',
             item_type TEXT NOT NULL,
             service TEXT NOT NULL,
             service_id TEXT NOT NULL,
             title TEXT,
             artist TEXT,
             album TEXT,
             cover_url TEXT,
             created_at TEXT,
             UNIQUE(profile_id, item_type, service, service_id)
         );",
    )
    .execute(&pool)
    .await
    .expect("seeding the drifted schema must succeed");

    // Boot the backend: connect() runs ensure_schema().
    let db = crate::db::postgres::PostgresDb::connect(&url)
        .await
        .expect("connect must succeed");

    // The statement that used to abort the batch is now guarded, so everything
    // AFTER it ran: queue_items exists…
    let exists: Option<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_name = 'queue_items'",
    )
    .fetch_optional(db.pool())
    .await
    .unwrap();
    assert!(
        exists.is_some(),
        "queue_items was not created: a failing statement still rolls back the batch"
    );

    // …and it carries the numbering columns, as BIGINT (bound as i64).
    for col in ["track_number", "disc_number"] {
        let dt: Option<String> = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'queue_items' AND column_name = $1",
        )
        .bind(col)
        .fetch_optional(db.pool())
        .await
        .unwrap();
        assert_eq!(
            dt.as_deref(),
            Some("bigint"),
            "queue_items.{col} missing or not bigint — streaming queue writes will fail"
        );
    }

    // The failing write from the ticket, verbatim in shape: a streaming row
    // naming track_number/disc_number must now insert.
    sqlx::raw_sql(
        "INSERT INTO queue_items \
         (zone_id, position, source_id, title, artist, album, cover_url, duration_ms, source, track_number, disc_number) \
         VALUES (424242, 0, 'q1', 't', 'a', 'al', NULL, 1000, 'qobuz', 3, 1)",
    )
    .execute(db.pool())
    .await
    .expect("streaming queue insert must succeed once the numbering columns exist");

    // Migration 026 is what repairs an ALREADY installed database: the seeded
    // `streaming_favorites.id` is BIGINT with no DEFAULT at all (the guarded
    // ALTER deliberately leaves a non-text column alone), so an insert that
    // omits `id` — which is what the repo does — fails until 026 re-attaches an
    // integer sequence. Replaying it here also asserts its idempotence: the CI
    // database has already had it applied by the migration step.
    sqlx::raw_sql(include_str!(
        "../../migrations/postgres/026_queue_items_numbering.sql"
    ))
    .execute(db.pool())
    .await
    .expect("migration 026 must be replayable");

    sqlx::raw_sql(
        "INSERT INTO streaming_favorites (profile_id, item_type, service, service_id) \
         VALUES ('1', 'album', 'qobuz', 'a1')",
    )
    .execute(db.pool())
    .await
    .expect("streaming_favorites must stay insertable without an explicit id");

    // Leave the schema in the shape the other tests expect.
    sqlx::raw_sql("DELETE FROM queue_items WHERE zone_id = 424242")
        .execute(db.pool())
        .await
        .ok();
}

/// #2860 — « Continuer l'écoute » et « Ajoutés récemment » étaient vides sur
/// TOUTE installation PostgreSQL, et sans un seul message.
///
/// Les trois défauts, mesurés sur une base réelle. Chacun porte ici sa
/// contre-épreuve : on rejoue la forme d'AVANT et on exige l'erreur exacte,
/// pour qu'un retour en arrière ne puisse pas passer inaperçu.
///
/// 1. `listen_history.album_id` TEXT contre `albums.id` BIGINT —
///    `operator does not exist: text = bigint`. La migration 012 convertit
///    déjà cette colonne, mais elle ne l'a JAMAIS vue : `album_id` n'arrive
///    par aucun script numéroté, seulement par `ENSURE_COLUMNS`, rejoué APRÈS.
///    Réparé par la 047.
/// 2. `GROUP BY a.id` en sélectionnant `ar.name` —
///    `column "ar.name" must appear in the GROUP BY clause…`.
/// 3. `HAVING listened_tracks < …` — un alias de la liste SELECT n'existe pas
///    quand le HAVING est évalué : `column "listened_tracks" does not exist`.
///
/// Les trois erreurs étaient avalées par le `unwrap_or_default()` de
/// `tune-server/src/routes/home.rs` : la section ne s'expliquait pas, elle
/// disparaissait.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2860_continuer_lecoute_et_ajouts_recents() {
    use crate::db::backend::ToSqlValue;
    use crate::db::engine::Engine;
    use crate::db::home_queries::{continue_listening_albums_deduits, recently_added};
    use crate::db::migrations::PG_MIGRATIONS;

    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL not set, skipping PG E2E test");
        return;
    };
    // Le pool brut EN PLUS du backend : `execute_batch` decoupe sur les
    // point-virgules, ce qui hacherait le bloc `DO $migration$ … $migration$`
    // de la 047. Les scripts numerotes passent par `raw_sql`, comme le fait
    // deja le test #1706.
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool.clone()));
    reset_schema(&db);

    let type_album_id = |db: &Arc<dyn DbBackend>| -> String {
        db.query_many(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'listen_history' AND column_name = 'album_id'",
            &[],
        )
        .unwrap()
        .first()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .unwrap_or_else(|| "<absente>".into())
    };

    // ── Reconstituer la dérive : la colonne telle qu'ENSURE_COLUMNS la posait ──
    db.execute(
        "ALTER TABLE listen_history DROP COLUMN IF EXISTS album_id",
        &[],
    )
    .unwrap();
    db.execute("ALTER TABLE listen_history ADD COLUMN album_id TEXT", &[])
        .unwrap();
    assert_eq!(type_album_id(&db), "text", "la dérive n'a pas été reposée");

    let limite: i64 = 10;
    let sql_cl = continue_listening_albums_deduits(Engine::Postgres, "");
    let sql_ra = recently_added(Engine::Postgres);

    // ── CONTRE-ÉPREUVE 1 — sans la migration 047, la requête ne compile même pas.
    let err = db
        .query_many(&sql_cl, &[&limite as &dyn ToSqlValue])
        .expect_err("`text = bigint` doit être refusé par PostgreSQL");
    assert!(
        err.contains("operator does not exist") && err.contains("text") && err.contains("bigint"),
        "erreur attendue « operator does not exist: text = bigint », obtenue : {err}"
    );

    // ── La migration 047, rejouée telle quelle (elle est idempotente) ──
    let (_, _, sql_047) = PG_MIGRATIONS
        .iter()
        .find(|(v, _, _)| *v == 47)
        .expect("la migration 047 doit être inscrite dans PG_MIGRATIONS");
    sqlx::raw_sql(*sql_047)
        .execute(&pool)
        .await
        .expect("la migration 047 doit être rejouable");
    assert_eq!(
        type_album_id(&db),
        "bigint",
        "la 047 n'a pas converti listen_history.album_id"
    );

    // ── Les données : les deux « Live » de Tades, dont un seul est écouté ──
    let id_de = |sql: &str| -> i64 {
        db.query_many(sql, &[])
            .unwrap()
            .first()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap()
    };
    let police = id_de("INSERT INTO artists (name) VALUES ('The Police') RETURNING id");
    let pulp = id_de("INSERT INTO artists (name) VALUES ('Pulp') RETURNING id");
    let live_police = id_de(&format!(
        "INSERT INTO albums (title, artist_id, track_count) \
         VALUES ('Live', {police}, 5) RETURNING id"
    ));
    let live_pulp = id_de(&format!(
        "INSERT INTO albums (title, artist_id, track_count) \
         VALUES ('Live', {pulp}, 5) RETURNING id"
    ));
    for piste in ["Piste 1", "Piste 2"] {
        db.execute(
            &format!(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, listened_at) \
                 VALUES ('{piste}', 'Pulp', 'Live', {live_pulp}, '2026-08-28T22:45:00Z')"
            ),
            &[],
        )
        .unwrap();
    }

    // ── « Continuer l'écoute » rend le disque de Pulp, 2 pistes sur 5 ──
    let lignes = db
        .query_many(&sql_cl, &[&limite as &dyn ToSqlValue])
        .expect("la requête corrigée doit s'exécuter sur PostgreSQL");
    assert_eq!(
        lignes.len(),
        1,
        "un seul album écouté et non fini était attendu, obtenu : {lignes:?}"
    );
    let l = &lignes[0];
    assert_eq!(
        l[0].as_i64(),
        Some(live_pulp),
        "ce n'est pas le Live de Pulp"
    );
    assert_ne!(
        l[0].as_i64(),
        Some(live_police),
        "l'homonyme de Police est remonté (#2731)"
    );
    assert_eq!(l[1].as_string().as_deref(), Some("Live"));
    assert_eq!(
        l[2].as_string().as_deref(),
        Some("Pulp"),
        "`ar.name` doit être rendue — c'est la colonne qui faisait tomber la requête"
    );
    assert_eq!(l[6].as_i64(), Some(2), "2 pistes distinctes écoutées");
    assert_eq!(l[7].as_i64(), Some(5), "sur 5");

    // ── « Ajoutés récemment » : même défaut, même écran ──
    let piste = id_de(&format!(
        "INSERT INTO tracks (title, album_id, artist_id, file_path, file_mtime) \
         VALUES ('Piste 1', {live_pulp}, {pulp}, '/x/1.flac', 9999999999) RETURNING id"
    ));
    assert!(piste > 0);
    let depuis: i64 = 0;
    let recents = db
        .query_many(
            &sql_ra,
            &[&depuis as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
        )
        .expect("« Ajoutés récemment » doit s'exécuter sur PostgreSQL");
    assert_eq!(recents.len(), 1, "obtenu : {recents:?}");
    assert_eq!(recents[0][2].as_string().as_deref(), Some("Pulp"));

    // ── CONTRE-ÉPREUVE 2 — `GROUP BY a.id` seul, la forme d'avant ──
    let avant_group_by = sql_cl.replace(
        "GROUP BY a.id, a.title, ar.name, a.year, a.cover_path, a.genre, a.track_count",
        "GROUP BY a.id",
    );
    assert_ne!(avant_group_by, sql_cl, "la substitution n'a rien remplacé");
    let err = db
        .query_many(&avant_group_by, &[&limite as &dyn ToSqlValue])
        .expect_err("`GROUP BY a.id` avec `ar.name` doit être refusé");
    assert!(
        err.contains("ar.name") && err.contains("GROUP BY"),
        "erreur attendue sur « ar.name », obtenue : {err}"
    );

    // ── CONTRE-ÉPREUVE 3 — l'alias de la liste SELECT dans le HAVING ──
    let avant_having = sql_cl.replace(
        "HAVING COUNT(DISTINCT lh.title) < a.track_count",
        "HAVING listened_tracks < a.track_count",
    );
    assert_ne!(avant_having, sql_cl, "la substitution n'a rien remplacé");
    let err = db
        .query_many(&avant_having, &[&limite as &dyn ToSqlValue])
        .expect_err("un alias de la liste SELECT dans le HAVING doit être refusé");
    assert!(
        err.contains("listened_tracks") && err.contains("does not exist"),
        "erreur attendue « column \"listened_tracks\" does not exist », obtenue : {err}"
    );
}

/// Restauration de sauvegarde : le volume fixe ne se rearme jamais, et le
/// volume d'une zone armee ne se propage pas (#2395, #2477).
///
/// La requete `UPDATE` de `import_zones` est BATIE — elle porte ou non la
/// colonne `volume` selon la sauvegarde. Une requete batie doit rendre le meme
/// resultat sur les deux moteurs : ces scenarios sont exactement ceux que le
/// `mod tests` de `config_backup` joue sur SQLite, rejoues ici sur une VRAIE
/// base PostgreSQL. Sans cette etape, seul le moteur par defaut serait exerce.
#[tokio::test(flavor = "multi_thread")]
async fn pg_config_backup_zones_volume_fixe() {
    use crate::config_backup::scenarios_zones;

    let db = pg_or_skip!();
    reset_schema(&db);

    scenarios_zones::une_zone_armee_ne_revient_ni_armee_ni_a_100(&db);
    scenarios_zones::une_zone_armee_absente_prend_le_defaut_du_schema(&db);
    scenarios_zones::temoin_une_sauvegarde_desarmee_repose_son_volume(&db);
    scenarios_zones::temoin_les_autres_champs_du_bloc_ne_bougent_pas(&db);
}

/// #2441 — « Continuer l'ecoute » sur une VRAIE base PostgreSQL : les
/// contextes, leur ordre, et l'avancement que le client dessine.
///
/// # Pourquoi ce test manquait
///
/// Le correctif de #2441 (PR #2479 puis #2936) a mis « Continuer l'ecoute » a
/// partir de `listen_history` et de son contexte de lecture. Les DEUX requetes
/// qui le portent — la derniere ecoute de chaque contexte, et la resolution
/// des albums locaux avec leur avancement — etaient redigees dans
/// `tune-server/src/routes/home.rs`. Or ce job lance `cargo test -p tune-core`
/// et ne compile PAS `tune-server` : elles n'avaient donc jamais ete jouees
/// sur PostgreSQL, exactement comme les requetes de #2860 avant elles, et pour
/// la meme raison. Leurs erreurs seraient avalees par le
/// `unwrap_or_default()` de l'appelant : pas un message, juste une section
/// vide.
///
/// Elles sont descendues dans `db/home_queries.rs`, et ce test les EXECUTE.
///
/// # Ce qu'il etablit
///
/// 1. Les deux requetes s'executent sur PostgreSQL.
/// 2. Un historique couvrant TROIS albums en rend trois, du plus recent au
///    plus ancien, sans doublon — le fait de base du ticket.
/// 3. `progression_pourcent` rend 60 / 40 / 20 — **les memes nombres** que le
///    test SQLite `plusieurs_albums_entames_rendent_chacun_leur_avancement`
///    (tune-server/src/routes/home.rs). C'est la comparaison des deux moteurs.
/// 4. TEMOIN : le cas a un seul album rend exactement cet album.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2441_continuer_lecoute_contextes_et_progression() {
    use crate::db::backend::ToSqlValue;
    use crate::db::engine::Engine;
    use crate::db::home_queries::{
        continue_listening_albums_du_contexte, continue_listening_contextes, progression_pourcent,
    };

    let db = pg_or_skip!();
    reset_schema(&db);

    let id_de = |sql: &str| -> i64 {
        db.query_many(sql, &[])
            .unwrap()
            .first()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap()
    };

    // ── Trois disques de cinq pistes, entames de 1, 2 et 3 pistes ──
    let mut albums = Vec::new();
    for (rang, nom) in ["Un", "Deux", "Trois"].iter().enumerate() {
        let artiste = id_de(&format!(
            "INSERT INTO artists (name) VALUES ('Artiste {nom}') RETURNING id"
        ));
        let album = id_de(&format!(
            "INSERT INTO albums (title, artist_id, track_count) \
             VALUES ('Disque {nom}', {artiste}, 5) RETURNING id"
        ));
        // Le plus ANCIEN est le moins ecoute : l'ordre attendu est celui de
        // l'ecoute, pas celui de l'avancement.
        for piste in 0..=rang {
            db.execute(
                &format!(
                    "INSERT INTO listen_history \
                     (title, artist_name, album_title, album_id, source, \
                      context_type, context_id, listened_at) \
                     VALUES ('{nom}{piste}', 'Artiste {nom}', 'Disque {nom}', \
                             {album}, 'local', 'album', '{album}', \
                             '2026-08-2{rang}T10:0{piste}:00Z')"
                ),
                &[],
            )
            .unwrap();
        }
        albums.push(album);
    }
    let (un, deux, trois) = (albums[0], albums[1], albums[2]);

    // ── 1. La requete des contextes s'execute, et rend les TROIS ──
    let marge: i64 = 40;
    let sql_ctx = continue_listening_contextes(Engine::Postgres, "");
    let lignes = db
        .query_many(&sql_ctx, &[&marge as &dyn ToSqlValue])
        .expect("`continue_listening_contextes` doit s'executer sur PostgreSQL");

    let contextes: Vec<(String, String)> = lignes
        .iter()
        .filter_map(|c| {
            Some((
                c.first().and_then(|v| v.as_string())?,
                c.get(1).and_then(|v| v.as_string())?,
            ))
        })
        .collect();
    assert_eq!(
        contextes.len(),
        3,
        "les trois contextes album etaient attendus, obtenu : {contextes:?}"
    );

    // 2. Le bon ordre — du plus recent au plus ancien — et sans doublon.
    let ids: Vec<String> = contextes.iter().map(|(_, id)| id.clone()).collect();
    assert_eq!(
        ids,
        vec![trois.to_string(), deux.to_string(), un.to_string()],
        "l'ordre doit etre celui de la derniere ecoute"
    );
    let uniques: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(ids.len(), uniques.len(), "un contexte remonte deux fois");
    assert!(
        contextes.iter().all(|(nature, _)| nature == "album"),
        "toutes les entrees sont de nature album : {contextes:?}"
    );

    // ── 3. L'avancement : les memes nombres que sur SQLite ──
    let sql_alb = continue_listening_albums_du_contexte(&[un, deux, trois]);
    let resolus = db
        .query_many(&sql_alb, &[])
        .expect("`continue_listening_albums_du_contexte` doit s'executer sur PostgreSQL");

    let mut pourcents = std::collections::HashMap::new();
    for cols in &resolus {
        let id = cols.first().and_then(|v| v.as_i64()).unwrap();
        let ecoutees = cols.get(6).and_then(|v| v.as_i64());
        let total = cols.get(7).and_then(|v| v.as_i64());
        pourcents.insert(id, progression_pourcent(ecoutees, total));
    }
    assert_eq!(
        (
            pourcents.get(&trois).copied().flatten(),
            pourcents.get(&deux).copied().flatten(),
            pourcents.get(&un).copied().flatten()
        ),
        (Some(60), Some(40), Some(20)),
        "PostgreSQL doit rendre le MEME avancement que SQLite (3/5, 2/5, 1/5), \
         obtenu : {pourcents:?}"
    );

    // ── 4. TEMOIN — un seul album rend exactement cet album ──
    reset_schema(&db);
    let artiste = id_de("INSERT INTO artists (name) VALUES ('Pulp') RETURNING id");
    let seul = id_de(&format!(
        "INSERT INTO albums (title, artist_id, track_count) \
         VALUES ('Live', {artiste}, 5) RETURNING id"
    ));
    for piste in ["Common People", "Disco 2000"] {
        db.execute(
            &format!(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, source, \
                  context_type, context_id, listened_at) \
                 VALUES ('{piste}', 'Pulp', 'Live', {seul}, 'local', \
                         'album', '{seul}', '2026-08-28T22:45:00Z')"
            ),
            &[],
        )
        .unwrap();
    }

    let lignes = db
        .query_many(&sql_ctx, &[&marge as &dyn ToSqlValue])
        .expect("la requete des contextes doit s'executer");

    // Les deux pistes portent la MEME `listened_at` — a la seconde pres, ce
    // qu'un enchainement produit — et la jointure sur le MAX les rend donc
    // TOUTES LES DEUX. Mesure du 01/09 sur PostgreSQL 15 : la requete rend
    // bien deux lignes ici. Le dedoublonnage est en Rust, chez l'appelant
    // (`contextes_recents`, tune-server/src/routes/home.rs) qui garde la
    // premiere, l'ordre etant deja decroissant. On rejoue cette regle pour
    // verifier le contrat REEL de la requete, pas un contrat imagine.
    let mut vues = std::collections::HashSet::new();
    let distincts: Vec<String> = lignes
        .iter()
        .filter_map(|c| {
            let nature = c.first().and_then(|v| v.as_string())?;
            let id = c.get(1).and_then(|v| v.as_string())?;
            vues.insert((nature, id.clone())).then_some(id)
        })
        .collect();
    assert_eq!(
        distincts,
        vec![seul.to_string()],
        "le temoin doit rendre exactement l'album ecoute : {lignes:?}"
    );

    let resolus = db
        .query_many(&continue_listening_albums_du_contexte(&[seul]), &[])
        .expect("la resolution d'album doit s'executer");
    assert_eq!(resolus.len(), 1);
    assert_eq!(
        progression_pourcent(
            resolus[0].get(6).and_then(|v| v.as_i64()),
            resolus[0].get(7).and_then(|v| v.as_i64())
        ),
        Some(40),
        "2 pistes sur 5, comme sur SQLite : {resolus:?}"
    );
}

/// #3039 — la fenetre des « Ajouts recents » sur une VRAIE base PostgreSQL.
///
/// Deux choses se jouent ici, qu'aucun test SQLite ne peut trancher :
///
/// 1. **Que les requetes s'EXECUTENT.** Elles portent desormais
///    `COALESCE(ffs.first_seen_at, CAST(NULLIF(CAST(t.file_mtime AS TEXT), '')
///    AS DOUBLE PRECISION))` — la forme exacte d'`ADDED_AT_JOIN`, qui existe
///    parce qu'un `COALESCE(double, text)` est une erreur DURE sur les
///    installations ou `tracks.file_mtime` est reste TEXT (#550), et
///    `NULLIF(double, '')` une erreur a l'analyse sur celles ou il est DOUBLE
///    (.15). SQLite avale les deux sans un mot. Le decompte y ajoute
///    `CAST(SUM(...) AS BIGINT)`, parce que `SUM` rend NUMERIC sur PostgreSQL.
///
/// 2. **Que la fenetre est bien LIEE** en `$1` et non ecrite en dur.
///
/// Pas de `reset_schema` : ce test n'a besoin d'aucune base vide et ne doit
/// pas vider celle des autres. Il pose ses propres chemins, prefixes d'un
/// marqueur unique, et ne conclut que sur les lignes qu'il a lui-meme ecrites.
/// Ni `pg_or_skip!` : la variable ABSENTE saute, mais une connexion qui ECHOUE
/// fait TOMBER le test — un banc mal branche ne s'affiche pas vert.
#[tokio::test(flavor = "multi_thread")]
async fn pg_3039_fenetre_et_decompte_des_ajouts_recents() {
    use crate::db::backend::ToSqlValue;
    use crate::db::engine::Engine;
    use crate::db::home_queries::{recently_added, recently_added_totaux};

    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — epreuve PostgreSQL sautee");
        return;
    };
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("TUNE_TEST_PG_URL posee : la connexion doit aboutir");
    let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool));

    // La table de premiere vue arrive par `ENSURE_TABLES` au demarrage, pas
    // par un script numerote (#473) : la poser ici rend le test independant
    // du millesime du banc.
    db.execute(
        "CREATE TABLE IF NOT EXISTS file_first_seen \
         (file_path TEXT PRIMARY KEY, first_seen_at DOUBLE PRECISION NOT NULL)",
        &[],
    )
    .expect("file_first_seen");

    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let il_y_a = |jours: f64| maintenant - jours * 24.0 * 3600.0;
    // Marqueur unique : les tests de ce fichier partagent une base et tournent
    // en parallele. Rien de ce qui suit ne depend des lignes des autres.
    let marque = format!("i3039-{}", maintenant as i64);

    let id_de = |sql: &str| -> i64 {
        db.query_many(sql, &[])
            .unwrap_or_else(|e| panic!("{sql}\n{e}"))
            .first()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap()
    };
    let artiste = id_de(&format!(
        "INSERT INTO artists (name) VALUES ('{marque}') RETURNING id"
    ));
    // Les quatre cas du ticket, tels que le test SQLite les pose.
    let cas: [(&str, f64, Option<f64>); 4] = [
        ("Recent", 2.0, None),
        ("Vieux", 60.0, None),
        ("Restaure", 800.0, Some(3.0)),
        ("Recopie", 1.0, Some(200.0)),
    ];
    let mut ids = Vec::new();
    for (nom, mtime_j, vue_j) in cas {
        let titre = format!("{marque} {nom}");
        let album = id_de(&format!(
            "INSERT INTO albums (title, artist_id, track_count) \
             VALUES ('{titre}', {artiste}, 1) RETURNING id"
        ));
        let chemin = format!("/{marque}/{nom}.flac");
        db.execute(
            &format!(
                "INSERT INTO tracks (title, album_id, artist_id, file_path, file_mtime, duration_ms) \
                 VALUES ('{titre}', {album}, {artiste}, '{chemin}', {}, 60000)",
                il_y_a(mtime_j)
            ),
            &[],
        )
        .expect("piste");
        if let Some(j) = vue_j {
            db.execute(
                &format!(
                    "INSERT INTO file_first_seen (file_path, first_seen_at) \
                     VALUES ('{chemin}', {})",
                    il_y_a(j)
                ),
                &[],
            )
            .expect("premiere vue");
        }
        ids.push((nom, album));
    }

    // ── La requete s'execute sur PostgreSQL, avec la fenetre LIEE en $1 ──
    let sql = recently_added(Engine::Postgres);
    let limite: i64 = 5000;
    let titres = |depuis: f64| -> Vec<String> {
        db.query_many(
            &sql,
            &[&depuis as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
        )
        .unwrap_or_else(|e| panic!("« Ajoutes recemment » doit s'executer sur PostgreSQL : {e}"))
        .iter()
        .filter_map(|r| r.get(1).and_then(|v| v.as_string()))
        .filter(|t| t.starts_with(&marque))
        .collect()
    };

    // ── Fenetre de 7 jours : le temoin, celle d'avant #3039 ──
    let a7 = titres(il_y_a(7.0));
    assert!(
        a7.contains(&format!("{marque} Recent")),
        "7 jours : `Recent` (J-2) manque — obtenu {a7:?}"
    );
    assert!(
        a7.contains(&format!("{marque} Restaure")),
        "7 jours : une sauvegarde remise en place (mtime J-800, premiere vue \
         J-3) doit entrer. La jointure `file_first_seen` ne porte donc pas sur \
         PostgreSQL — obtenu {a7:?}"
    );
    assert!(
        !a7.contains(&format!("{marque} Recopie")),
        "7 jours : un `rsync -a` (mtime J-1, premiere vue J-200) ne doit PAS \
         entrer — obtenu {a7:?}"
    );
    assert!(
        !a7.contains(&format!("{marque} Vieux")),
        "7 jours : `Vieux` (J-60) doit rester dehors — obtenu {a7:?}"
    );

    // ── La fenetre est SERVIE : $1 change, le resultat change ──
    let a61 = titres(il_y_a(61.0));
    assert!(
        a61.contains(&format!("{marque} Vieux")),
        "61 jours : `Vieux` (J-60) doit entrer. S'il n'entre pas, `$1` n'est \
         pas lu et la fenetre reste ecrite en dur (#3039) — obtenu {a61:?}"
    );
    assert!(
        a61.len() > a7.len(),
        "une fenetre plus large doit rendre PLUS d'albums : 7 j → {a7:?}, \
         61 j → {a61:?}"
    );

    // ── Le decompte : il s'execute, et il suit la meme fenetre ──
    let sql_t = recently_added_totaux(Engine::Postgres);
    let compte = |depuis: f64| -> (i64, i64, i64) {
        let lignes = db
            .query_many(&sql_t, &[&depuis as &dyn ToSqlValue])
            .unwrap_or_else(|e| panic!("le decompte doit s'executer sur PostgreSQL : {e}"));
        let l = lignes.first().expect("le decompte rend une ligne");
        (
            l[0].as_i64().expect("albums est un entier"),
            l[1].as_i64().expect("pistes est un entier"),
            // `SUM` rend NUMERIC sur PostgreSQL : sans le CAST explicite de la
            // requete, cette lecture rendrait `None` et le test tomberait ici.
            l[2].as_i64()
                .expect("duration_ms est un entier — CAST … AS BIGINT"),
        )
    };
    let (alb7, pis7, dur7) = compte(il_y_a(7.0));
    let (alb61, pis61, dur61) = compte(il_y_a(61.0));
    assert!(
        alb61 > alb7 && pis61 > pis7 && dur61 > dur7,
        "le decompte doit suivre la fenetre : 7 j → ({alb7}, {pis7}, {dur7}), \
         61 j → ({alb61}, {pis61}, {dur61})"
    );
    assert!(
        dur7 >= 120_000,
        "la duree cumulee doit compter les 60 000 ms de chaque piste, \
         obtenu {dur7}"
    );

    // ── CONTRE-EPREUVE — la forme d'AVANT #3039 se trompe deux fois ──
    //
    // Le filtre sur le seul `mtime`, tel qu'il s'ecrivait, fait entrer la
    // recopie et sortir la restauration. C'est le defaut que le testeur
    // aurait vu ; on verifie ici qu'il etait bien la, sur PostgreSQL aussi.
    let avant = sql
        .replace(
            &format!(
                "LEFT JOIN file_first_seen ffs ON ffs.file_path = t.file_path \
                 WHERE {} > $1",
                crate::db::home_queries::DATE_D_AJOUT
            ),
            "WHERE t.file_mtime IS NOT NULL AND t.file_mtime > $1",
        )
        .replace(
            &format!("MAX({}) as added_at", crate::db::home_queries::DATE_D_AJOUT),
            "MAX(t.file_mtime) as added_at",
        );
    assert_ne!(avant, sql, "la substitution n'a rien remplace");
    let depuis = il_y_a(7.0);
    let anciens: Vec<String> = db
        .query_many(
            &avant,
            &[&depuis as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
        )
        .expect("la forme d'avant doit rester executable")
        .iter()
        .filter_map(|r| r.get(1).and_then(|v| v.as_string()))
        .filter(|t| t.starts_with(&marque))
        .collect();
    assert!(
        anciens.contains(&format!("{marque} Recopie")),
        "la forme d'avant DEVAIT faire entrer la recopie — sans quoi la \
         contre-epreuve ne prouve rien : {anciens:?}"
    );
    assert!(
        !anciens.contains(&format!("{marque} Restaure")),
        "la forme d'avant DEVAIT laisser la restauration dehors : {anciens:?}"
    );

    // ── Menage : ce test ne vide pas les tables des autres, il retire les
    // siennes.
    for (_, album) in &ids {
        let _ = db.execute(&format!("DELETE FROM tracks WHERE album_id = {album}"), &[]);
        let _ = db.execute(&format!("DELETE FROM albums WHERE id = {album}"), &[]);
    }
    let _ = db.execute(
        &format!("DELETE FROM file_first_seen WHERE file_path LIKE '/{marque}/%'"),
        &[],
    );
    let _ = db.execute(&format!("DELETE FROM artists WHERE id = {artiste}"), &[]);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_genres_manquants_3979() {
    let db = pg_or_skip!();
    let predicate = crate::db::facet_filter::untagged_condition_for_engine(
        "genre",
        crate::db::engine::Engine::Postgres,
    )
    .unwrap();
    for (genre, genres, missing) in [
        (None, Some(r#"["Jazz", "Soul"]"#), false),
        (Some("Rock"), None, false),
        (None, None, true),
        (Some(""), Some("[]"), true),
        (None, Some("[ ]"), true),
        (None, Some(r#"["", " "]"#), true),
        (None, Some("broken"), true),
        (None, Some("null"), true),
        (None, Some(r#"{"genre":"Jazz"}"#), true),
        (None, Some(r#"["Jazz", 1]"#), true),
        (Some("Blues"), Some("broken"), false),
    ] {
        let sql = format!(
            "SELECT {predicate} AS missing FROM (SELECT $1::text AS genre, $2::text AS genres) t"
        );
        let row = db.query_one(&sql, &[&genre, &genres]).unwrap().unwrap();
        assert_eq!(
            row[0].as_bool(),
            Some(missing),
            "PostgreSQL compte mal les genres multiples (#3979): {genre:?} / {genres:?}"
        );
    }
}
// #3715: exercise repository bindings on both historical PostgreSQL shapes.
async fn pg_3715_pool(case: &str) -> Option<sqlx::PgPool> {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée — favoris PostgreSQL #3715");
        return None;
    };
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let name = format!("tune_streaming_profile_{case}");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name}"
    )))
    .execute(&admin)
    .await
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap();
    let (base, query) = url.split_once('?').unwrap_or((&url, ""));
    let root = base.rsplit_once('/').unwrap().0;
    let pool = sqlx::PgPool::connect(&format!("{root}/{name}?{query}"))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE schema_version (version INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    admin.close().await;
    Some(pool)
}

async fn pg_3715_ensure_favorites(pool: &sqlx::PgPool) {
    for stmt in crate::db::postgres::ENSURE_TABLES
        .iter()
        .filter(|s| s.contains("streaming_favorites"))
    {
        sqlx::raw_sql(*stmt).execute(pool).await.unwrap();
    }
}

const PG_3715_MIGRATION: &str =
    include_str!("../../migrations/postgres/060_streaming_profile_id.sql");

async fn pg_3715_roundtrip(case: &str) {
    use crate::db::streaming_favorites_repo::StreamingFavoritesRepo;
    use crate::favorites_sort::TriFavoris;
    let Some(pool) = pg_3715_pool(case).await else {
        return;
    };
    pg_3715_ensure_favorites(&pool).await;
    if case == "native" {
        let typ: String = sqlx::query_scalar("SELECT data_type FROM information_schema.columns WHERE table_schema='public' AND table_name='streaming_favorites' AND column_name='profile_id'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(
            typ, "bigint",
            "new native table must use the repository's i64 profile type"
        );
    } else if case == "legacy" {
        sqlx::raw_sql("ALTER TABLE streaming_favorites ALTER COLUMN profile_id DROP DEFAULT; ALTER TABLE streaming_favorites ALTER COLUMN profile_id TYPE TEXT USING profile_id::text; ALTER TABLE streaming_favorites ALTER COLUMN profile_id SET DEFAULT '1'")
            .execute(&pool).await.unwrap();
    } else {
        // The SQLite -> PG path already repaired both IDs via migration 012.
        sqlx::raw_sql("ALTER TABLE streaming_favorites ALTER COLUMN id DROP DEFAULT; ALTER TABLE streaming_favorites ALTER COLUMN id TYPE BIGINT USING id::bigint; ALTER TABLE streaming_favorites ALTER COLUMN id SET DEFAULT nextval('streaming_favorites_id_seq')")
            .execute(&pool).await.unwrap();
    }
    sqlx::raw_sql("INSERT INTO streaming_favorites (id, profile_id, item_type, service, service_id, title, position) VALUES ('500', '9000000001', 'track', 'qobuz', 'kept', 'Existing favorite', '7')")
        .execute(&pool).await.unwrap();
    for _ in 0..2 {
        sqlx::raw_sql(PG_3715_MIGRATION)
            .execute(&pool)
            .await
            .unwrap();
        pg_3715_ensure_favorites(&pool).await;
    }
    let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool.clone()));
    let repo = StreamingFavoritesRepo::with_backend(db);
    let pid = 9_000_000_001_i64;
    let kept = repo
        .list(pid, None)
        .expect("existing favorites must remain readable");
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id, 500);
    assert_eq!(kept[0].profile_id, pid);
    assert_eq!(kept[0].title.as_deref(), Some("Existing favorite"));
    for (profile, kind, id) in [
        (pid, "track", "a"),
        (pid, "track", "b"),
        (pid, "album", "album"),
        (pid + 1, "track", "a"),
    ] {
        repo.add(profile, kind, "qobuz", id, Some(id), None, None, None)
            .expect("add binds an integer profile");
    }
    repo.add(
        pid,
        "track",
        "qobuz",
        "a",
        Some("duplicate"),
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(repo.list(pid, Some("track")).unwrap().len(), 3);
    assert_eq!(repo.list(pid, None).unwrap().len(), 4);
    assert!(repo.is_favorite(pid, "track", "qobuz", "a").unwrap());
    assert!(!repo.is_favorite(pid, "track", "qobuz", "absent").unwrap());
    assert_eq!(
        repo.reorder(
            pid,
            "track",
            &[
                ("qobuz".into(), "b".into()),
                ("qobuz".into(), "a".into()),
                ("qobuz".into(), "b".into()),
                ("qobuz".into(), "absent".into())
            ]
        )
        .unwrap(),
        2
    );
    let manual = TriFavoris::depuis(Some("manual"), None).unwrap();
    let sorted = repo.list_sorted(pid, Some("track"), manual).unwrap();
    assert_eq!(
        sorted
            .iter()
            .map(|f| f.service_id.as_str())
            .collect::<Vec<_>>(),
        ["b", "a", "kept"]
    );
    assert_eq!(repo.list_sorted(pid, None, manual).unwrap().len(), 4);
    repo.remove(pid, "track", "qobuz", "a").unwrap();
    assert!(!repo.is_favorite(pid, "track", "qobuz", "a").unwrap());
    assert!(repo.is_favorite(pid + 1, "track", "qobuz", "a").unwrap());
    assert_eq!(repo.list(pid, Some("album")).unwrap().len(), 1);
    sqlx::raw_sql("INSERT INTO streaming_favorites (item_type, service, service_id) VALUES ('track','tidal','default-profile')")
        .execute(&pool).await.unwrap();
    assert!(
        repo.is_favorite(1, "track", "tidal", "default-profile")
            .unwrap()
    );
    drop(repo);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_native_favorites() {
    pg_3715_roundtrip("native").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_legacy_favorites() {
    pg_3715_roundtrip("legacy").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_migrated_favorites() {
    pg_3715_roundtrip("migrated").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_invalid_profiles_preserve_data_and_version() {
    let Some(pool) = pg_3715_pool("invalid").await else {
        return;
    };
    let mut c = pool.acquire().await.unwrap();
    for value in ["not-an-id", "9223372036854775808"] {
        sqlx::raw_sql("DROP TABLE IF EXISTS streaming_favorites; CREATE TABLE streaming_favorites (profile_id TEXT NOT NULL DEFAULT '1', title TEXT)")
            .execute(&mut *c).await.unwrap();
        sqlx::query("INSERT INTO streaming_favorites VALUES ($1, 'keep me')")
            .bind(value)
            .execute(&mut *c)
            .await
            .unwrap();
        let error = sqlx::raw_sql(PG_3715_MIGRATION)
            .execute(&mut *c)
            .await
            .expect_err("invalid profile must refuse the migration");
        assert!(
            matches!(
                error.as_database_error().and_then(|e| e.code()).as_deref(),
                Some("22P02" | "22003")
            ),
            "{error}"
        );
        sqlx::raw_sql("ROLLBACK").execute(&mut *c).await.unwrap();
        let row: (String, String) =
            sqlx::query_as("SELECT profile_id, title FROM streaming_favorites")
                .fetch_one(&mut *c)
                .await
                .unwrap();
        assert_eq!(row, (value.into(), "keep me".into()));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM schema_version WHERE version=60")
            .fetch_one(&mut *c)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let default: String = sqlx::query_scalar("SELECT column_default FROM information_schema.columns WHERE table_schema='public' AND table_name='streaming_favorites' AND column_name='profile_id'").fetch_one(&mut *c).await.unwrap();
        assert_eq!(default, "'1'::text");
    }
    drop(c);
    pool.close().await;
}

const PG_3715_ID_MIGRATION: &str =
    include_str!("../../migrations/postgres/061_streaming_favorite_ids.sql");

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_id_native_is_numeric_and_empty_sequence_starts_at_one() {
    let Some(pool) = pg_3715_pool("id_native").await else {
        return;
    };
    pg_3715_ensure_favorites(&pool).await;
    let typ: String = sqlx::query_scalar("SELECT data_type FROM information_schema.columns WHERE table_schema='public' AND table_name='streaming_favorites' AND column_name='id'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        typ, "bigint",
        "native favorite IDs must have the same numeric type as migrated IDs"
    );
    for _ in 0..2 {
        sqlx::raw_sql(PG_3715_ID_MIGRATION)
            .execute(&pool)
            .await
            .unwrap();
    }
    let id: i64 = sqlx::query_scalar("INSERT INTO streaming_favorites (item_type, service, service_id) VALUES ('track','qobuz','new') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        id, 1,
        "an empty table must not consume the first sequence value"
    );
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_id_migration_preserves_rows_and_never_rewinds_sequence() {
    use crate::db::streaming_favorites_repo::StreamingFavoritesRepo;
    for (case, textual, sequence, called, expected) in [
        ("id_behind", true, 1_i64, false, 5_000_000_002_i64),
        ("id_ahead", true, 9_000_000_001, true, 9_000_000_002),
        ("id_uncalled", true, 9_000_000_001, false, 9_000_000_001),
        ("id_equal", true, 5_000_000_001, false, 5_000_000_002),
        ("id_migrated", false, 9_000_000_001, true, 9_000_000_002),
    ] {
        let Some(pool) = pg_3715_pool(case).await else {
            return;
        };
        pg_3715_ensure_favorites(&pool).await;
        if textual {
            sqlx::raw_sql("ALTER TABLE streaming_favorites ALTER COLUMN id DROP DEFAULT; ALTER TABLE streaming_favorites ALTER COLUMN id TYPE TEXT USING id::text; ALTER TABLE streaming_favorites ALTER COLUMN id SET DEFAULT nextval('streaming_favorites_id_seq')::text")
                .execute(&pool).await.unwrap();
        }
        sqlx::raw_sql("INSERT INTO streaming_favorites (id,profile_id,item_type,service,service_id,title,position) VALUES ('5000000001',42,'track','qobuz','kept','Favorite to preserve','9')")
            .execute(&pool).await.unwrap();
        sqlx::query("SELECT setval('streaming_favorites_id_seq',$1,$2)")
            .bind(sequence)
            .bind(called)
            .execute(&pool)
            .await
            .unwrap();
        for _ in 0..2 {
            sqlx::raw_sql(PG_3715_ID_MIGRATION)
                .execute(&pool)
                .await
                .unwrap();
            pg_3715_ensure_favorites(&pool).await;
        }
        let typ: String = sqlx::query_scalar("SELECT data_type FROM information_schema.columns WHERE table_schema='public' AND table_name='streaming_favorites' AND column_name='id'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(typ, "bigint", "{case}");
        let saved: (i64, i64, String, String) = sqlx::query_as(
            "SELECT id,profile_id,title,position FROM streaming_favorites WHERE service_id='kept'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            saved,
            (5_000_000_001, 42, "Favorite to preserve".into(), "9".into()),
            "{case}"
        );
        let repo =
            StreamingFavoritesRepo::with_backend(Arc::new(PostgresBackend::new(pool.clone())));
        repo.add(42, "track", "qobuz", "new", None, None, None, None)
            .unwrap();
        let rows = repo.list(42, None).unwrap();
        assert_eq!(rows.len(), 2, "{case}");
        assert_eq!(
            rows.iter().find(|f| f.service_id == "new").unwrap().id,
            expected,
            "{case}: sequence must advance past rows without reusing consumed values"
        );
        // Replaying after an insert is also safe, including a now-consumed
        // sequence that was previously ahead of all rows but not yet called.
        sqlx::raw_sql(PG_3715_ID_MIGRATION)
            .execute(&pool)
            .await
            .unwrap();
        repo.add(42, "track", "qobuz", "next", None, None, None, None)
            .unwrap();
        assert_eq!(
            repo.list(42, None)
                .unwrap()
                .iter()
                .find(|f| f.service_id == "next")
                .unwrap()
                .id,
            expected + 1,
            "{case}"
        );
        drop(repo);
        pool.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_id_invalid_or_colliding_values_preserve_rows_and_sequence() {
    let Some(pool) = pg_3715_pool("id_invalid").await else {
        return;
    };
    let mut c = pool.acquire().await.unwrap();
    sqlx::raw_sql("CREATE SEQUENCE streaming_favorites_id_seq START 50")
        .execute(&mut *c)
        .await
        .unwrap();
    for values in [
        vec!["not-an-id"],
        vec!["9223372036854775808"],
        vec!["01", "1"],
    ] {
        sqlx::raw_sql("DROP TABLE IF EXISTS streaming_favorites; CREATE TABLE streaming_favorites (id TEXT PRIMARY KEY DEFAULT nextval('streaming_favorites_id_seq')::text, title TEXT)")
            .execute(&mut *c).await.unwrap();
        for value in &values {
            sqlx::query("INSERT INTO streaming_favorites VALUES ($1,'keep me')")
                .bind(value)
                .execute(&mut *c)
                .await
                .unwrap();
        }
        let error = sqlx::raw_sql(PG_3715_ID_MIGRATION)
            .execute(&mut *c)
            .await
            .expect_err("bad or colliding IDs must refuse migration");
        assert!(
            matches!(
                error.as_database_error().and_then(|e| e.code()).as_deref(),
                Some("22P02" | "22003" | "23505")
            ),
            "{error}"
        );
        sqlx::raw_sql("ROLLBACK").execute(&mut *c).await.unwrap();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id,title FROM streaming_favorites ORDER BY id")
                .fetch_all(&mut *c)
                .await
                .unwrap();
        assert_eq!(
            rows,
            values
                .iter()
                .map(|v| (v.to_string(), "keep me".into()))
                .collect::<Vec<_>>()
        );
        let seq: (i64, bool) =
            sqlx::query_as("SELECT last_value,is_called FROM streaming_favorites_id_seq")
                .fetch_one(&mut *c)
                .await
                .unwrap();
        assert_eq!(
            seq,
            (50, false),
            "a rejected cast must not change the sequence"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM schema_version WHERE version=61")
            .fetch_one(&mut *c)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let default: String = sqlx::query_scalar("SELECT column_default FROM information_schema.columns WHERE table_schema='public' AND table_name='streaming_favorites' AND column_name='id'").fetch_one(&mut *c).await.unwrap();
        assert!(
            default.contains("nextval") && default.ends_with("::text"),
            "{default}"
        );
    }
    drop(c);
    pool.close().await;
}

const PG_3715_RADIO_MIGRATION: &str =
    include_str!("../../migrations/postgres/062_radio_favorite_integer.sql");

async fn pg_3715_radio_pool(case: &str, typ: &str) -> Option<sqlx::PgPool> {
    let pool = pg_3715_pool(case).await?;
    assert!(matches!(typ, "SMALLINT" | "TEXT"));
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE radio_stations (
        id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL,
        homepage TEXT, logo_url TEXT, country TEXT, language TEXT, genre TEXT,
        codec TEXT, bitrate INTEGER, is_favorite {typ} DEFAULT '0',
        last_played TEXT, play_count INTEGER DEFAULT 0)"
    )))
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_radio_create_list_toggle_on_native_and_migrated_schemas() {
    use crate::db::radio_repo::{RadioRepo, RadioStation};
    for (case, typ) in [("radio_native", "SMALLINT"), ("radio_migrated", "TEXT")] {
        let Some(pool) = pg_3715_radio_pool(case, typ).await else {
            return;
        };
        for _ in 0..2 {
            sqlx::raw_sql(PG_3715_RADIO_MIGRATION)
                .execute(&pool)
                .await
                .unwrap();
        }
        let repo = RadioRepo::with_backend(Arc::new(PostgresBackend::new(pool.clone())));
        let mut ids = Vec::new();
        for favorite in [true, false] {
            let station = RadioStation {
                id: None,
                name: format!("Station {favorite}"),
                url: format!("http://example.invalid/{favorite}"),
                homepage: None,
                logo_url: None,
                country: Some("FR".into()),
                language: None,
                genre: None,
                codec: Some("flac".into()),
                bitrate: Some(900),
                is_favorite: favorite,
                last_played: None,
                play_count: 0,
            };
            let id = repo
                .create(&station)
                .expect("integer favorite flags must be accepted by PostgreSQL");
            assert!(id > 0);
            ids.push(id);
        }
        let all = repo.list().unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            all.iter()
                .find(|r| r.id == Some(ids[0]))
                .unwrap()
                .is_favorite
        );
        assert!(
            !all.iter()
                .find(|r| r.id == Some(ids[1]))
                .unwrap()
                .is_favorite
        );
        assert_eq!(
            repo.favorites()
                .unwrap()
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [Some(ids[0])]
        );
        repo.set_favorite(ids[0], false).unwrap();
        repo.set_favorite(ids[1], true).unwrap();
        assert_eq!(
            repo.favorites()
                .unwrap()
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [Some(ids[1])]
        );
        assert_eq!(repo.favorites().unwrap()[0].codec.as_deref(), Some("flac"));
        drop(repo);
        pool.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_radio_migration_recovers_boolean_text_and_preserves_numeric_values() {
    use crate::db::radio_repo::RadioRepo;
    let Some(pool) = pg_3715_radio_pool("radio_legacy", "TEXT").await else {
        return;
    };
    for (i, flag) in [
        Some("true"),
        Some("false"),
        Some("1"),
        Some("0"),
        None,
        Some("2"),
    ]
    .iter()
    .enumerate()
    {
        sqlx::query("INSERT INTO radio_stations (name,url,is_favorite) VALUES ($1,$2,$3)")
            .bind(format!("Saved {i}"))
            .bind(format!("http://example.invalid/{i}"))
            .bind(flag)
            .execute(&pool)
            .await
            .unwrap();
    }
    for _ in 0..2 {
        sqlx::raw_sql(PG_3715_RADIO_MIGRATION)
            .execute(&pool)
            .await
            .unwrap();
    }
    let flags: Vec<Option<i64>> =
        sqlx::query_scalar("SELECT is_favorite FROM radio_stations ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(flags, [Some(1), Some(0), Some(1), Some(0), None, Some(2)]);
    let repo = RadioRepo::with_backend(Arc::new(PostgresBackend::new(pool.clone())));
    let mut names: Vec<_> = repo
        .favorites()
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["Saved 0", "Saved 2"],
        "the former true text must become visible as a favorite"
    );
    assert_eq!(repo.list().unwrap().len(), 6);
    let flag: i64 = sqlx::query_scalar("INSERT INTO radio_stations (name,url) VALUES ('Default','http://example.invalid/default') RETURNING is_favorite").fetch_one(&pool).await.unwrap();
    assert_eq!(flag, 0);
    drop(repo);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_radio_invalid_flags_preserve_data_and_version() {
    let Some(pool) = pg_3715_radio_pool("radio_invalid", "TEXT").await else {
        return;
    };
    let mut c = pool.acquire().await.unwrap();
    for value in ["unknown", "9223372036854775808"] {
        sqlx::raw_sql("DELETE FROM radio_stations")
            .execute(&mut *c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO radio_stations (name,url,is_favorite) VALUES ('Keep','http://example.invalid/keep',$1)").bind(value).execute(&mut *c).await.unwrap();
        let err = sqlx::raw_sql(PG_3715_RADIO_MIGRATION)
            .execute(&mut *c)
            .await
            .expect_err("invalid flag must refuse migration");
        assert!(
            matches!(
                err.as_database_error().and_then(|e| e.code()).as_deref(),
                Some("22P02" | "22003")
            ),
            "{err}"
        );
        sqlx::raw_sql("ROLLBACK").execute(&mut *c).await.unwrap();
        let row: (String, String, String) =
            sqlx::query_as("SELECT name,url,is_favorite FROM radio_stations")
                .fetch_one(&mut *c)
                .await
                .unwrap();
        assert_eq!(
            row,
            (
                "Keep".into(),
                "http://example.invalid/keep".into(),
                value.into()
            )
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM schema_version WHERE version=62")
            .fetch_one(&mut *c)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    drop(c);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3715_alarm_source_migration_preserves_ids_and_accepts_opaque_strings() {
    for (case, typ) in [("alarm_native", "BIGINT"), ("alarm_import", "TEXT")] {
        let Some(pool) = pg_3715_pool(case).await else {
            return;
        };
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE alarms (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, source_id {typ})"
        )))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql("INSERT INTO alarms (name,source_id) VALUES ('Legacy','9223372036854775807'),('Empty',NULL)").execute(&pool).await.unwrap();
        for _ in 0..2 {
            sqlx::raw_sql(include_str!(
                "../../migrations/postgres/063_alarm_source_text.sql"
            ))
            .execute(&pool)
            .await
            .unwrap();
        }
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name,source_id FROM alarms ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                ("Legacy".into(), Some("9223372036854775807".into())),
                ("Empty".into(), None)
            ]
        );
        let db = PostgresBackend::new(pool.clone());
        for source in ["qobuz:playlist:abc", "000123", "9223372036854775808"] {
            db.execute(
                "INSERT INTO alarms (name,source_id) VALUES ('New',?)",
                &[&source],
            )
            .unwrap();
            let got = db
                .query_one("SELECT source_id FROM alarms ORDER BY id DESC LIMIT 1", &[])
                .unwrap()
                .unwrap();
            assert_eq!(got[0].as_str(), Some(source));
        }
        let version: i64 =
            sqlx::query_scalar("SELECT count(*) FROM schema_version WHERE version=63")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(version, 1);
        drop(db);
        pool.close().await;
    }
}
