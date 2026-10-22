//! #4319 — le texte libre cherche CHAQUE MOT, dans le titre, l'artiste,
//! l'artiste de l'album, le compositeur ou le chef d'orchestre.
//!
//! Tades (fils 1817 et 2264, rc4 Linux) : « mahler mehta » dans la portée
//! XX-XXI trouvait la Symphonie n° 2 dirigée par Mehta, jamais la n° 3. La
//! saisie était cherchée d'un seul tenant, dans un seul champ : seule la 2,
//! dont l'artiste s'appelle « Mahler Mehta », contenait les deux mots accolés.
//! Pour la 3, « Mahler » est dans le titre de l'album et « Mehta » dans
//! l'artiste de l'album, ou dans le chef d'orchestre seulement.
//!
//! Le même scénario tourne sur PostgreSQL
//! (`postgres_e2e::pg_4319_texte_libre_mot_a_mot`).

use std::sync::Arc;

use crate::db::album_metadata_repo::AlbumMetadataRepo;
use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::Engine;
use crate::db::facet_filter::{
    CHAMPS_DU_TEXTE_LIBRE, Placeholders, TrackFilter, condition_texte_libre, mots_du_texte_libre,
};
use crate::db::models::{Album, Artist, Track};
use crate::db::sqlite::SqliteDb;
use crate::db::track_metadata_repo::TrackMetadataRepo;
use crate::db::track_repo::TrackRepo;

/// La bibliothèque de Tades, réduite à ce qui départage, et un album par
/// magasin du chef d'orchestre. Les chemins ne portent aucun chiffre : « 3 »
/// ne doit se lire que dans un titre.
pub(crate) fn scenario_texte_libre_mot_a_mot_4319(db: Arc<dyn DbBackend>) {
    let artistes = ArtistRepo::with_backend(db.clone());
    let albums = AlbumRepo::with_backend(db.clone());
    let pistes = TrackRepo::with_backend(db.clone());
    let meta_piste = TrackMetadataRepo::with_backend(db.clone());
    let meta_album = AlbumMetadataRepo::with_backend(db.clone());

    let artiste = |nom: &str| artistes.create(&Artist::new(nom.into())).unwrap();
    let album = |titre: &str, artiste_id: i64| {
        let mut a = Album::new(titre.into());
        a.artist_id = Some(artiste_id);
        albums.create(&a).unwrap()
    };
    let piste = |titre: &str, album_id: i64, artiste_id: i64, dossier: &str| {
        let mut t = Track::new(titre.into());
        t.album_id = Some(album_id);
        t.artist_id = Some(artiste_id);
        t.file_path = Some(format!("/musique/{dossier}/{titre}.flac"));
        t.duration_ms = 1000;
        t.format = Some("flac".into());
        pistes.create(&t).unwrap()
    };

    let mahler_mehta = artiste("Mahler Mehta");
    let zubin = artiste("Zubin Mehta");
    let la_phil = artiste("Los Angeles Philharmonic");
    let berliner = artiste("Berliner Philharmoniker");
    let wiener = artiste("Wiener Philharmoniker");
    let scala = artiste("Filarmonica della Scala");

    // La 2 : l'artiste porte les deux mots accolés — trouvée avant comme après.
    let deux = album("Mahler - Symphony No. 2", mahler_mehta);
    piste("Allegro maestoso", deux, mahler_mehta, "deux");
    // La 3 : « Mehta » n'est que l'ARTISTE DE L'ALBUM ; les pistes sont
    // créditées à l'orchestre.
    let trois = album("Mahler - Symphony No. 3", zubin);
    piste("Kräftig. Entschieden", trois, la_phil, "trois");
    piste("Tempo di Menuetto", trois, la_phil, "trois");
    // La 5 : le chef n'est que dans l'ÉTIQUETTE de fichier (`track_metadata`).
    let cinq = album("Mahler - Symphony No. 5", berliner);
    let p5 = piste("Trauermarsch", cinq, berliner, "cinq");
    meta_piste
        .set(p5, "conductor", "Herbert von Karajan")
        .unwrap();
    // Bruckner 7 : le chef n'est que dans les CRÉDITS (`track_credits`).
    let sept = album("Bruckner - Symphony No. 7", wiener);
    let p7 = piste("Adagio", sept, wiener, "sept");
    let marqueurs = match db.engine() {
        Engine::Sqlite => "?1, ?2",
        Engine::Postgres => "$1, $2",
    };
    db.execute(
        &format!(
            "INSERT INTO track_credits (track_id, artist_name, role, position) \
             VALUES ({marqueurs}, 'conductor', 0)"
        ),
        &[&p7 as &dyn ToSqlValue, &"Claudio Abbado"],
    )
    .unwrap();
    // Brahms 1 : le chef n'est que sur la FICHE D'ALBUM (`album_metadata`).
    let brahms = album("Brahms - Symphony No. 1", scala);
    piste("Un poco sostenuto", brahms, scala, "brahms");
    meta_album
        .set(brahms, "conductor", "Carlos Kleiber")
        .unwrap();

    let trouve = |q: &str| -> Vec<i64> {
        let f = TrackFilter {
            q: Some(q.into()),
            ..Default::default()
        };
        let (items, total) = pistes.list_filtered(&f, 100, 0).unwrap();
        assert_eq!(
            items.len() as i64,
            total,
            "« {q} » : la liste tient son total"
        );
        let mut ids: Vec<i64> = items.into_iter().filter_map(|t| t.album_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };

    // Le cas de Tades.
    assert_eq!(
        trouve("mahler mehta"),
        vec![deux, trois],
        "« mahler mehta » : la 2 (artiste) ET la 3 (titre + artiste de l'album)"
    );
    assert_eq!(trouve("mahler 3 mehta"), vec![trois], "« mahler 3 mehta »");
    assert_eq!(
        trouve("Mehta  Mahler"),
        vec![deux, trois],
        "l'ordre des mots est libre"
    );
    // Le chef d'orchestre, dans chacun de ses trois magasins.
    assert_eq!(
        trouve("mahler karajan"),
        vec![cinq],
        "chef : étiquette de fichier"
    );
    assert_eq!(
        trouve("bruckner abbado"),
        vec![sept],
        "chef : crédits de la piste"
    );
    assert_eq!(
        trouve("brahms kleiber"),
        vec![brahms],
        "chef : fiche d'album"
    );
    // Casse et accents repliés, mot par mot.
    assert_eq!(trouve("MAHLER kraftig"), vec![trois], "accent replié");
    // Une saisie d'un seul tenant, comme avant.
    assert_eq!(trouve("symphony no. 2"), vec![deux]);
    assert_eq!(trouve("karajan"), vec![cinq]);

    // Témoins : CHAQUE mot doit être trouvé — un mot absent exclut.
    assert!(
        trouve("mahler karajan 3").is_empty(),
        "Karajan ne dirige pas la 3"
    );
    assert!(
        trouve("mahler abbado").is_empty(),
        "Abbado ne dirige que Bruckner"
    );
    assert!(trouve("mehta stravinsky").is_empty());
    // Un mot n'est pas un joker : `%` est littéral.
    assert!(trouve("mahler %").is_empty());
}

#[test]
fn le_texte_libre_cherche_chaque_mot_sur_sqlite_4319() {
    // La base de production : `track_metadata`, `track_credits` et
    // `album_metadata` naissent des migrations, pas de `CORE_SCHEMA`.
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    scenario_texte_libre_mot_a_mot_4319(Arc::new(db));
}

/// Un mot, `CHAMPS_DU_TEXTE_LIBRE.len()` marqueurs et autant de valeurs ; deux
/// mots, le double, réunis par ET — sur les DEUX moteurs (piège n°2 : les
/// marqueurs anonymes de SQLite se comptent un par un).
#[test]
fn deux_mots_deux_fois_plus_de_marqueurs_sur_les_deux_moteurs_4319() {
    assert_eq!(
        mots_du_texte_libre("  \"Mahler\"  3 mehta "),
        ["Mahler", "3", "mehta"]
    );
    assert!(mots_du_texte_libre(" \"\" ").is_empty());
    let n = CHAMPS_DU_TEXTE_LIBRE.len();
    for engine in [Engine::Sqlite, Engine::Postgres] {
        let mut ph = Placeholders::resuming_at(engine, 3);
        let (sql, valeurs) = condition_texte_libre(&mut ph, "mahler mehta");
        assert_eq!(valeurs.len(), 2 * n, "{engine:?}");
        assert_eq!(ph.next_index(), 3 + 2 * n, "{engine:?}");
        assert_eq!(
            sql.matches("LIKE LOWER(unaccent(").count(),
            2 * n,
            "{engine:?}"
        );
        assert_eq!(
            sql.matches(") AND (").count(),
            1,
            "{engine:?} : ET entre les mots : {sql}"
        );
        let mut ph = Placeholders::resuming_at(engine, 1);
        let (sql, valeurs) = condition_texte_libre(&mut ph, "  ");
        assert_eq!((sql.as_str(), valeurs.len()), ("1=1", 0), "{engine:?}");
        assert_eq!(ph.next_index(), 1, "{engine:?} : aucun marqueur");
    }
}
