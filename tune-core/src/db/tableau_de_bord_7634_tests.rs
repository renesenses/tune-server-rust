//! #7634 — « Mon écoute » : six widgets en « délai dépassé » sur une grosse
//! base (Tendance, Semaine × heure, Heure d'écoute, Par zone, Par source,
//! Durées). Tous vivent d'UNE requête, `GET /library/history/dashboard`
//! (`HistoryRepo::full_dashboard`), que le client abandonne au bout de 8 s.

use std::sync::Arc;
use std::time::Instant;

use super::backend::DbBackend;
use super::history_repo::HistoryRepo;
use super::sqlite::SqliteDb;

/// Remplit une base de FICHIER migrée au profil d'un gros historique :
/// `pistes` pistes (12 par album, 8 albums par artiste), `ecoutes` écoutes
/// étalées sur trois ans, six zones, cinq sources, trois profils. Les
/// écoutes portent le titre, l'artiste et l'album comme l'orchestrateur les
/// écrit ; une sur trois n'a pas de `track_id`.
pub(super) fn remplir_banc(db: &SqliteDb, pistes: i64, ecoutes: i64) {
    let albums = ((pistes + 11) / 12).max(1);
    let artistes = (albums / 8).max(1);
    let sql = format!(
        "BEGIN;
         INSERT INTO zones (id, name) VALUES (1,'Salon'),(2,'Bureau'),(3,'Cuisine'),
                (4,'Chambre'),(5,'Linn'),(6,'Casque');
         INSERT OR IGNORE INTO profiles (id, username) VALUES (1,'banc1'),(2,'banc2'),(3,'banc3');
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < {artistes})
         INSERT INTO artists (id, name) SELECT i, 'Artiste ' || i FROM n;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < {albums})
         INSERT INTO albums (id, title, artist_id, source)
              SELECT i, 'Album ' || i, (i % {artistes}) + 1, 'local' FROM n;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < {pistes})
         INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, file_path, genre)
              SELECT i, 'Titre ' || (i % 40000), ((i - 1) / 12) + 1,
                     (((i - 1) / 12) % {artistes}) + 1, 120000 + (i % 300) * 1000,
                     '/musique/' || i || '.flac',
                     CASE i % 7 WHEN 0 THEN 'Jazz' WHEN 1 THEN 'Rock' WHEN 2 THEN 'Classique'
                                WHEN 3 THEN 'Pop' WHEN 4 THEN 'Electro' WHEN 5 THEN '' ELSE NULL END
              FROM n;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < {ecoutes})
         INSERT INTO listen_history (track_id, title, artist_name, album_title, source,
                                     duration_ms, listened_at, zone_id, profile_id)
              SELECT CASE WHEN i % 3 = 0 THEN NULL ELSE p END,
                     'Titre ' || (p % 40000),
                     'Artiste ' || ((((p - 1) / 12) % {artistes}) + 1),
                     'Album ' || (((p - 1) / 12) + 1),
                     CASE i % 10 WHEN 0 THEN 'qobuz' WHEN 1 THEN 'qobuz' WHEN 2 THEN 'tidal'
                                 WHEN 3 THEN 'upnp' ELSE 'local' END,
                     CASE WHEN i % 9 = 0 THEN 12000 ELSE 150000 + (i % 200) * 1000 END,
                     strftime('%Y-%m-%dT%H:%M:%SZ', 'now',
                              '-' || ((i * 1123) % (1095 * 86400)) || ' seconds'),
                     CASE WHEN i % 11 = 0 THEN NULL ELSE (i % 6) + 1 END,
                     CASE WHEN i % 5 = 0 THEN NULL ELSE (i % 3) + 1 END
              FROM (SELECT i, ((i * 7919) % {pistes}) + 1 AS p FROM n);
         COMMIT;"
    );
    db.execute_batch(&sql).expect("banc #7634");
}

/// Mesure à la demande : `cargo test -p tune-core --lib banc_7634 --
/// --ignored --nocapture`. Base de fichier neuve de 500 000 écoutes sur
/// 100 000 pistes, remplie par le même générateur à chaque fois (le banc est
/// déterministe, à l'heure de la mesure près) : avant et après se comparent.
#[test]
#[ignore = "banc de mesure #7634 : base de 500 000 écoutes, à la demande"]
fn banc_7634_mesure_le_tableau_de_bord() {
    let t = Instant::now();
    let b = banc("mesure", 100_000, 500_000);
    eprintln!("banc rempli en {:?}", t.elapsed());
    let repo = HistoryRepo::with_backend(b.db.clone());
    for periode in ["7d", "30d", "today", "all"] {
        for essai in 1..=3 {
            let t = Instant::now();
            let d = repo
                .full_dashboard(periode, None, None, 50)
                .expect("tableau de bord");
            eprintln!(
                "MESURE période={periode} essai={essai} durée={:?} écoutes={} tendance={} zones={} sources={}",
                t.elapsed(),
                d.totals.plays,
                d.trend.len(),
                d.by_zone.len(),
                d.by_source.len()
            );
        }
    }
    let t = Instant::now();
    repo.full_dashboard("7d", Some(2), Some(1), 50).unwrap();
    eprintln!("MESURE période=7d zone=2 profil=1 durée={:?}", t.elapsed());
}

struct Banc {
    _dossier: crate::test_scratch::ScratchDir,
    db: Arc<dyn DbBackend>,
}

fn banc(epreuve: &str, pistes: i64, ecoutes: i64) -> Banc {
    let dossier = crate::test_scratch::scratch_dir(&format!("tableau-de-bord-7634-{epreuve}"));
    let chemin = dossier.join("tune-banc.db").to_string_lossy().into_owned();
    let db = SqliteDb::open(&chemin).expect("base de fichier");
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    remplir_banc(&db, pistes, ecoutes);
    Banc {
        _dossier: dossier,
        db: Arc::new(db),
    }
}

/// Les lignes d'une requête, rendues comparables.
fn lignes(db: &Arc<dyn DbBackend>, sql: &str) -> Vec<String> {
    db.query_many(sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .into_iter()
        .map(|cols| format!("{cols:?}"))
        .collect()
}

/// Le SQL d'AVANT #7634 (SQLite), recopié au caractère près pour les blocs
/// réécrits : c'est l'état de référence des résultats.
fn reference_avant(db: &Arc<dyn DbBackend>, jours: Option<i64>) -> [Vec<String>; 6] {
    let periode = jours
        .map(|d| format!("WHERE listened_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{d} days')"))
        .unwrap_or_default();
    let et = if periode.is_empty() { "WHERE" } else { "AND" };
    let mut artistes = lignes(
        db,
        &format!(
            "SELECT lh.artist_name, COUNT(*) as plays, CAST(COALESCE(SUM(lh.duration_ms), 0) AS BIGINT) as ms
             FROM listen_history lh
             {periode} {et} source != 'radio' AND lh.artist_name IS NOT NULL
             GROUP BY lh.artist_name ORDER BY plays DESC LIMIT 100000"
        ),
    );
    let mut albums = lignes(
        db,
        &format!(
            "SELECT h.album_title, h.artist_name, COUNT(*) as plays
             FROM listen_history h
             {periode} {et} h.source != 'radio' AND h.album_title IS NOT NULL
             GROUP BY h.album_title, h.artist_name
             ORDER BY plays DESC LIMIT 100000"
        ),
    );
    let mut titres = lignes(
        db,
        &format!(
            "SELECT lh.title, lh.artist_name, COUNT(*) as plays,
                    CAST(COALESCE(SUM(lh.duration_ms), 0) AS BIGINT) as ms
             FROM listen_history lh
             {periode} {et} source != 'radio' AND lh.title IS NOT NULL
             GROUP BY lh.title, lh.artist_name ORDER BY plays DESC LIMIT 100000"
        ),
    );
    let mut zones = lignes(
        db,
        &format!(
            "SELECT h.zone_id, z.name, COUNT(*) as plays, CAST(COALESCE(SUM(h.duration_ms), 0) AS BIGINT) as ms
             FROM listen_history h
             LEFT JOIN zones z ON z.id = h.zone_id
             {periode}
             GROUP BY h.zone_id, z.name ORDER BY plays DESC"
        ),
    );
    let jours_ecoutes = lignes(
        db,
        "SELECT DISTINCT DATE(listened_at) as d FROM listen_history ORDER BY 1",
    );
    let maintenant: String = db
        .query_one("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", &[])
        .unwrap()
        .unwrap()[0]
        .as_string()
        .unwrap();
    let md = &maintenant[5..10];
    let annee = &maintenant[0..4];
    let mut ce_jour_la = lignes(
        db,
        &format!(
            "SELECT title, artist_name, MAX(album_title), NULL, MAX(listened_at),
                    MAX(CAST(strftime('%Y', listened_at) AS INTEGER)) as yr
             FROM listen_history
             WHERE listened_at LIKE '%-{md}%' AND CAST(strftime('%Y', listened_at) AS INTEGER) < {annee}
             GROUP BY title, artist_name
             ORDER BY yr DESC LIMIT 100000"
        ),
    );
    for l in [
        &mut artistes,
        &mut albums,
        &mut titres,
        &mut zones,
        &mut ce_jour_la,
    ] {
        l.sort();
    }
    [artistes, albums, titres, zones, jours_ecoutes, ce_jour_la]
}

/// Ce que rend `full_dashboard` pour les mêmes blocs, dans la même forme.
fn rendu(db: &Arc<dyn DbBackend>, periode: &str) -> ([Vec<String>; 4], Option<(i64, i64)>, usize) {
    use super::backend::SqlValue as V;
    let d = HistoryRepo::with_backend(db.clone())
        .full_dashboard(periode, None, None, 100_000)
        .expect("tableau de bord");
    let s = |x: &str| V::Text(x.to_string());
    let o = |x: &Option<String>| x.clone().map(V::Text).unwrap_or(V::Null);
    let oi = |x: Option<i64>| x.map(V::Int).unwrap_or(V::Null);
    // `track_id` n'est pas comparé : à défaut d'identifiant dans
    // l'historique, le dépôt le complète depuis la bibliothèque, après coup.
    let mut artistes: Vec<String> = d
        .top_artists
        .iter()
        .map(|a| {
            format!(
                "{:?}",
                vec![s(&a.artist_name), V::Int(a.plays), V::Int(a.listening_ms)]
            )
        })
        .collect();
    let mut albums: Vec<String> = d
        .top_albums
        .iter()
        .map(|a| {
            format!(
                "{:?}",
                vec![s(&a.album_title), s(&a.artist_name), V::Int(a.plays)]
            )
        })
        .collect();
    let mut titres: Vec<String> = d
        .top_tracks
        .iter()
        .map(|t| {
            format!(
                "{:?}",
                vec![
                    s(&t.title),
                    s(&t.artist_name),
                    V::Int(t.plays),
                    V::Int(t.listening_ms)
                ]
            )
        })
        .collect();
    let mut zones: Vec<String> = d
        .by_zone
        .iter()
        .map(|z| {
            format!(
                "{:?}",
                vec![
                    oi(z.zone_id),
                    o(&z.zone_name),
                    V::Int(z.plays),
                    V::Int(z.listening_ms)
                ]
            )
        })
        .collect();
    for l in [&mut artistes, &mut albums, &mut titres, &mut zones] {
        l.sort();
    }
    let serie = d.streak.as_ref().map(|s| (s.current, s.best));
    (
        [artistes, albums, titres, zones],
        serie,
        d.on_this_day.len(),
    )
}

/// Témoin d'identité : sur un petit historique connu (2 400 écoutes sur
/// trois ans), les classements, « Par zone », la série et « Ce jour-là »
/// rendent exactement les lignes de l'ancien SQL.
#[test]
fn le_tableau_de_bord_rend_les_memes_lignes_qu_avant_7634() {
    let b = banc("identite", 1_200, 2_400);
    for (periode, jours) in [("7d", Some(7)), ("30d", Some(30)), ("all", None)] {
        let [artistes, albums, titres, zones, jours_ecoutes, ce_jour_la] =
            reference_avant(&b.db, jours);
        assert!(
            !artistes.is_empty() && !zones.is_empty(),
            "banc vide sur {periode}"
        );
        let ([a, al, t, z], serie, otd) = rendu(&b.db, periode);
        assert_eq!(a, artistes, "{periode} : artistes");
        assert_eq!(al, albums, "{periode} : albums");
        assert_eq!(t, titres, "{periode} : titres");
        assert_eq!(z, zones, "{periode} : zones");
        assert_eq!(otd, ce_jour_la.len().min(10), "{periode} : ce jour-là");
        // La série se recalcule des jours de l'ancienne requête.
        let attendu = {
            let jours: Vec<String> = jours_ecoutes
                .iter()
                .map(|l| l.split('"').nth(1).unwrap_or_default().to_string())
                .collect();
            let mut best = 1;
            let mut cur = 1;
            for w in jours.windows(2) {
                let a = chrono::NaiveDate::parse_from_str(&w[0], "%Y-%m-%d").unwrap();
                let b = chrono::NaiveDate::parse_from_str(&w[1], "%Y-%m-%d").unwrap();
                if b - a == chrono::Duration::days(1) {
                    cur += 1;
                } else {
                    best = best.max(cur);
                    cur = 1;
                }
            }
            best.max(cur)
        };
        assert_eq!(
            serie.map(|s| s.1),
            Some(attendu),
            "{periode} : meilleure série"
        );
    }
}

/// Témoin de durée : sept jours — la période des widgets — doivent coûter
/// une FRACTION de l'historique entier. Avant #7634 les classements,
/// « Par zone » et « Ce jour-là » parcouraient tout l'historique quelle que
/// soit la période : sur le banc de 500 000 écoutes, 3,4 s pour 7 jours
/// contre 6,9 s pour tout (moitié). On compare deux mesures sur la MÊME
/// base plutôt qu'un seuil absolu, pour tenir sur une machine chargée.
#[test]
fn le_tableau_de_bord_de_sept_jours_ne_parcourt_plus_tout_l_historique_7634() {
    let b = banc("duree", 3_000, 150_000);
    let repo = HistoryRepo::with_backend(b.db.clone());
    let mesure = |periode: &str| {
        repo.full_dashboard(periode, None, None, 50).unwrap();
        (0..3)
            .map(|_| {
                let t = Instant::now();
                repo.full_dashboard(periode, None, None, 50).unwrap();
                t.elapsed()
            })
            .min()
            .unwrap()
    };
    let sept_jours = mesure("7d");
    let tout = mesure("all");
    assert!(
        sept_jours * 5 < tout,
        "#7634 : 7 jours prennent {sept_jours:?} contre {tout:?} pour tout l'historique \
         (150 000 écoutes) — une requête du tableau de bord parcourt de nouveau tout \
         l'historique au lieu de la période"
    );
}
