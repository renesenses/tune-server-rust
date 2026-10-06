//! #5885 — « la moitié des albums absente en UPnP » sur une grande
//! bibliothèque FLAC + DSF.
//!
//! Banc : une base synthétique de 6 000 albums, mêlant FLAC, DSF et albums
//! mixtes, avec des albums sans artiste, sans pochette, des titres en double,
//! des compilations. On la parcourt comme un vrai point de contrôle
//! (BubbleUPnP, mconnect, Linn Kazoo) : `Browse(albums)` page par page, en
//! suivant `TotalMatches`, puis `Browse(artists)` et chaque `artist/N`.
//! On compte les albums vus et on les compare à la base.

use super::*;
use std::collections::{BTreeSet, HashSet};

const NB_ALBUMS: usize = 6_000;
const NB_ARTISTES: usize = 1_500;

fn base_de_6000_albums() -> UpnpState {
    base_d_albums(NB_ALBUMS)
}

/// La base du banc, écrite en SQL brut dans UNE transaction : 6 000 albums
/// et 18 000 pistes par les repos prendraient des minutes.
fn base_d_albums(nb_albums: usize) -> UpnpState {
    use crate::db::sqlite::SqliteDb;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch("DELETE FROM radio_stations;").unwrap();

    let mut sql = String::from("BEGIN;\n");
    for a in 1..=NB_ARTISTES {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a:04}');\n"
        ));
    }
    let mut piste = 0usize;
    for i in 1..=nb_albums {
        // Un album sur dix sans artiste d'album.
        let artiste = if i % 10 == 0 {
            "NULL".to_string()
        } else {
            ((i % NB_ARTISTES) + 1).to_string()
        };
        // Une pochette sur trois manque.
        let pochette = if i % 3 == 0 {
            "NULL".to_string()
        } else {
            format!("'{:032x}'", i)
        };
        // Titres en double (« Greatest Hits » ×60), accents, signes de tête.
        let titre = match i % 100 {
            0 => "Greatest Hits".to_string(),
            1 => format!("Été {i}"),
            2 => format!("(Live) {i}"),
            _ => format!("Album {i:05}"),
        };
        // FLAC, DSF ou mixte.
        let formats: [&str; 3] = match i % 3 {
            0 => ["flac", "flac", "flac"],
            1 => ["dsf", "dsf", "dsf"],
            _ => ["flac", "dsf", "flac"],
        };
        let compilation = i32::from(i % 25 == 0);
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, track_count, cover_path, format, is_compilation, folder_path) \
             VALUES ({i}, '{titre}', {artiste}, 3, {pochette}, '{fmt}', {compilation}, '/Musique/{i}');\n",
            titre = titre.replace('\'', "''"),
            fmt = formats[0],
        ));
        for (n, fmt) in formats.iter().enumerate() {
            piste += 1;
            sql.push_str(&format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, track_number, duration_ms, file_path, format, sample_rate, bit_depth) \
                 VALUES ({piste}, 'Piste {n}', {i}, {artiste}, {num}, 200000, '/Musique/{i}/{num:02}.{fmt}', '{fmt}', {sr}, {bd});\n",
                num = n + 1,
                sr = if *fmt == "dsf" { 2_822_400 } else { 44_100 },
                bd = if *fmt == "dsf" { 1 } else { 16 },
            ));
        }
    }
    sql.push_str("COMMIT;\n");
    db.execute_batch(&sql).unwrap();
    UpnpState::new(Arc::new(db), 8888, None)
}

fn soap_browse(object_id: &str, start: u64, count: u64) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope><s:Body>
<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ObjectID>{object_id}</ObjectID>
<BrowseFlag>BrowseDirectChildren</BrowseFlag>
<Filter>*</Filter>
<StartingIndex>{start}</StartingIndex>
<RequestedCount>{count}</RequestedCount>
<SortCriteria></SortCriteria>
</u:Browse></s:Body></s:Envelope>"#
    )
}

fn compteur(reponse: &str, balise: &str) -> u64 {
    let ouvrant = format!("<{balise}>");
    let fermant = format!("</{balise}>");
    let debut = reponse.find(&ouvrant).unwrap_or_else(|| {
        panic!(
            "{balise} absent de : {}",
            &reponse[..reponse.len().min(600)]
        )
    }) + ouvrant.len();
    let fin = debut + reponse[debut..].find(&fermant).unwrap();
    reponse[debut..fin].trim().parse().unwrap()
}

/// Les identifiants `album/N` transportés par une réponse (DIDL échappé).
fn albums_transportes(reponse: &str) -> Vec<i64> {
    const MARQUE: &str = "id=&quot;album/";
    let mut ids = Vec::new();
    let mut reste = reponse;
    while let Some(p) = reste.find(MARQUE) {
        let debut = p + MARQUE.len();
        let fin = debut + reste[debut..].find("&quot;").unwrap();
        ids.push(reste[debut..fin].parse().unwrap());
        reste = &reste[fin..];
    }
    ids
}

fn ids_transportes(reponse: &str, prefixe: &str) -> Vec<String> {
    let marque = format!("id=&quot;{prefixe}");
    let mut ids = Vec::new();
    let mut reste = reponse;
    while let Some(p) = reste.find(&marque) {
        let debut = p + "id=&quot;".len();
        let fin = debut + reste[debut..].find("&quot;").unwrap();
        ids.push(reste[debut..fin].to_string());
        reste = &reste[fin..];
    }
    ids
}

/// Ce que fait un point de contrôle : pages de `taille`, jusqu'à ce que
/// l'index atteigne `TotalMatches` ou qu'une page revienne vide. Un fault
/// arrête le parcours, comme chez BubbleUPnP ou Kazoo.
fn parcourir_comme_un_point_de_controle(
    state: &UpnpState,
    conteneur: &str,
    taille: u64,
) -> (Vec<String>, u64, Option<String>) {
    let mut vus = Vec::new();
    let mut debut = 0u64;
    let mut total_annonce = 0u64;
    loop {
        let reponse = build_browse_response(state, &soap_browse(conteneur, debut, taille));
        if is_soap_fault(&reponse) {
            return (vus, total_annonce, Some(reponse));
        }
        let rendus = compteur(&reponse, "NumberReturned");
        total_annonce = compteur(&reponse, "TotalMatches");
        vus.extend(ids_transportes(&reponse, ""));
        debut += rendus;
        if rendus == 0 || debut >= total_annonce {
            return (vus, total_annonce, None);
        }
    }
}

fn albums_en_base(state: &UpnpState) -> BTreeSet<i64> {
    state
        .backend
        .query_many("SELECT id FROM albums", &[])
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

/// Le parcours du rayon « Albums » par pages de 50, 100, 200, 500 et
/// « tout » (`RequestedCount = 0`) voit chacun des 6 000 albums, une fois.
#[test]
fn le_rayon_albums_parcouru_par_pages_rend_les_6000_albums_5885() {
    let state = base_de_6000_albums();
    let en_base = albums_en_base(&state);
    assert_eq!(en_base.len(), NB_ALBUMS);

    for taille in [50u64, 100, 200, 500, 0] {
        let t0 = std::time::Instant::now();
        let (vus, total, fault) = parcourir_comme_un_point_de_controle(&state, "albums", taille);
        let duree = t0.elapsed();
        assert!(fault.is_none(), "page {taille} : fault {fault:?}");
        let ids: Vec<i64> = vus
            .iter()
            .filter_map(|id| id.strip_prefix("album/")?.parse().ok())
            .collect();
        let uniques: BTreeSet<i64> = ids.iter().copied().collect();
        let manquants: Vec<_> = en_base.difference(&uniques).take(10).collect();
        eprintln!(
            "#5885 pages de {taille} : {} objets, {} albums distincts, TotalMatches {total}, {:?}",
            ids.len(),
            uniques.len(),
            duree
        );
        assert_eq!(total, NB_ALBUMS as u64, "TotalMatches (pages de {taille})");
        assert_eq!(
            ids.len(),
            uniques.len(),
            "doublons entre pages (pages de {taille})"
        );
        assert!(
            manquants.is_empty(),
            "pages de {taille} : {} albums vus sur {}, premiers manquants {manquants:?}",
            uniques.len(),
            en_base.len()
        );
    }
}

/// Le parcours par « Artists » : chaque artiste, puis ses albums. Les albums
/// sans artiste d'album n'y sont pas — c'est attendu, ils n'ont pas
/// d'artiste — mais tous les autres doivent s'y trouver.
#[test]
fn le_rayon_artists_ouvre_tous_les_albums_qui_ont_un_artiste_5885() {
    let state = base_de_6000_albums();
    let (artistes, total, fault) = parcourir_comme_un_point_de_controle(&state, "artists", 100);
    assert!(fault.is_none(), "{fault:?}");
    assert_eq!(total, artistes.len() as u64);
    let mut vus: HashSet<i64> = HashSet::new();
    for artiste in &artistes {
        let reponse = build_browse_response(&state, &soap_browse(artiste, 0, 0));
        assert!(!is_soap_fault(&reponse), "{artiste} : {reponse}");
        vus.extend(albums_transportes(&reponse));
    }
    let avec_artiste: BTreeSet<i64> = state
        .backend
        .query_many("SELECT id FROM albums WHERE artist_id IS NOT NULL", &[])
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let manquants: Vec<_> = avec_artiste
        .iter()
        .filter(|id| !vus.contains(id))
        .take(10)
        .collect();
    eprintln!(
        "#5885 par artistes : {} albums vus sur {} avec artiste",
        vus.len(),
        avec_artiste.len()
    );
    assert!(
        manquants.is_empty(),
        "albums absents de Artists : {manquants:?}"
    );
}

/// Le `childCount` que la racine annonce pour « Albums » est le nombre que
/// le rayon ouvre.
#[test]
fn la_racine_annonce_le_nombre_d_albums_que_le_rayon_ouvre_5885() {
    let state = base_de_6000_albums();
    let racine = browse_direct_children(&state, "0", 0, 0).xml;
    assert!(
        racine.contains(&format!(
            "id=\"albums\" parentID=\"0\" restricted=\"1\" childCount=\"{NB_ALBUMS}\""
        )),
        "{racine}"
    );
}

/// Un `DbBackend` qui laisse le scan écrire pendant le parcours : une mise à
/// jour de pochette (qui fait bouger le `SystemUpdateID`) toutes les
/// `periode` lectures du compteur — la première après `periode - 1`
/// lectures, pour que le parcours ait déjà rendu quelques pages. C'est ce que font
/// le scan d'une grande bibliothèque et ses passes de fond pendant qu'un
/// point de contrôle feuillette « Albums ».
struct ScanConcurrent {
    db: Arc<dyn DbBackend>,
    lectures: std::sync::atomic::AtomicUsize,
    periode: usize,
}

impl DbBackend for ScanConcurrent {
    fn engine(&self) -> crate::db::engine::Engine {
        self.db.engine()
    }
    fn execute(
        &self,
        sql: &str,
        p: &[&dyn crate::db::backend::ToSqlValue],
    ) -> Result<usize, String> {
        self.db.execute(sql, p)
    }
    fn last_insert_rowid(&self) -> i64 {
        self.db.last_insert_rowid()
    }
    fn query_one(
        &self,
        sql: &str,
        p: &[&dyn crate::db::backend::ToSqlValue],
    ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
        self.db.query_one(sql, p)
    }
    fn query_many(
        &self,
        sql: &str,
        p: &[&dyn crate::db::backend::ToSqlValue],
    ) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
        self.db.query_many(sql, p)
    }
    fn execute_batch(&self, sql: &str) -> Result<(), String> {
        self.db.execute_batch(sql)
    }
    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn crate::db::backend::DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        self.db.write_tx(f)
    }
    fn query_one_strong(
        &self,
        sql: &str,
        p: &[&dyn crate::db::backend::ToSqlValue],
    ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
        if sql.contains("upnp_catalog_revision") {
            let n = self
                .lectures
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n % self.periode == self.periode - 1 {
                self.db.execute_batch(&format!(
                    "UPDATE albums SET cover_path = 'scan{n}' WHERE id = 1"
                ))?;
            }
        }
        self.db.query_one_strong(sql, p)
    }
}

/// #5885 — LE défaut : pendant que le catalogue s'écrit, un parcours par
/// pages de 100 recevait un fault 720 à la première page construite pendant
/// une écriture, et le point de contrôle s'arrêtait là. Il doit voir les
/// 6 000 albums, et l'`UpdateID` doit dire que le catalogue a bougé.
#[test]
fn un_scan_concurrent_ne_coupe_pas_le_parcours_des_albums_5885() {
    let mut state = base_de_6000_albums();
    let en_base = albums_en_base(&state);
    state.backend = Arc::new(ScanConcurrent {
        db: state.backend,
        lectures: std::sync::atomic::AtomicUsize::new(0),
        // Une écriture toutes les dix lectures : une page sur cinq environ.
        periode: 10,
    });
    let (vus, total, fault) = parcourir_comme_un_point_de_controle(&state, "albums", 100);
    let uniques: BTreeSet<i64> = vus
        .iter()
        .filter_map(|id| id.strip_prefix("album/")?.parse().ok())
        .collect();
    eprintln!(
        "#5885 pendant un scan : {} albums vus sur {}, TotalMatches {total}, fault {}",
        uniques.len(),
        en_base.len(),
        fault.is_some()
    );
    assert!(
        fault.is_none(),
        "#5885 : le parcours s'est arrêté sur un fault après {} albums sur {} : {}",
        uniques.len(),
        en_base.len(),
        fault.as_deref().unwrap_or_default()
    );
    assert_eq!(uniques, en_base, "albums vus pendant le scan");
}

fn soap_search(container: &str, criteria: &str, start: u64, count: u64) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope><s:Body>
<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ContainerID>{container}</ContainerID>
<SearchCriteria>{criteria}</SearchCriteria>
<Filter>*</Filter>
<StartingIndex>{start}</StartingIndex>
<RequestedCount>{count}</RequestedCount>
<SortCriteria></SortCriteria>
</u:Search></s:Body></s:Envelope>"#
    )
}

/// #5885 — `Search` des albums (le menu « Albums » d'un lecteur réseau)
/// lisait la rubrique sous un plafond de 10 000, après le tri alphabétique :
/// au-delà, la fin de l'alphabet manquait, et `TotalMatches` disait 10 000.
#[test]
fn search_des_albums_rend_toute_une_bibliotheque_de_plus_de_10000_albums_5885() {
    const NB: usize = 10_050;
    let state = base_d_albums(NB);
    let critere =
        "upnp:class derivedfrom &quot;object.container.album&quot; and @refID exists false";
    let mut vus: BTreeSet<i64> = BTreeSet::new();
    let mut debut = 0u64;
    let total = loop {
        let reponse = build_browse_response(&state, &soap_search("0", critere, debut, 500));
        assert!(!is_soap_fault(&reponse), "{reponse}");
        let rendus = compteur(&reponse, "NumberReturned");
        let total = compteur(&reponse, "TotalMatches");
        vus.extend(albums_transportes(&reponse));
        debut += rendus;
        if rendus == 0 || debut >= total {
            break total;
        }
    };
    eprintln!(
        "#5885 Search albums : {} vus, TotalMatches {total}",
        vus.len()
    );
    assert_eq!(total, NB as u64, "TotalMatches de Search(albums)");
    assert_eq!(vus.len(), NB, "albums rendus par Search");
}

/// Même plafond, même défaut, côté artistes.
#[test]
fn search_des_artistes_rend_plus_de_10000_artistes_5885() {
    use crate::db::sqlite::SqliteDb;
    const NB: usize = 10_050;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=NB {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a:05}');\n\
             INSERT INTO albums (id, title, artist_id, track_count) VALUES ({a}, 'Album {a:05}', {a}, 1);\n\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format) \
             VALUES ({a}, 'Piste', {a}, {a}, '/Musique/{a}/01.dsf', 'dsf');\n"
        ));
    }
    sql.push_str("COMMIT;\n");
    db.execute_batch(&sql).unwrap();
    let state = UpnpState::new(Arc::new(db), 8888, None);
    let critere = "upnp:class derivedfrom &quot;object.container.person&quot;";
    let mut vus: HashSet<String> = HashSet::new();
    let mut debut = 0u64;
    let total = loop {
        let reponse = build_browse_response(&state, &soap_search("0", critere, debut, 500));
        assert!(!is_soap_fault(&reponse), "{reponse}");
        let rendus = compteur(&reponse, "NumberReturned");
        let total = compteur(&reponse, "TotalMatches");
        vus.extend(ids_transportes(&reponse, "artist/"));
        debut += rendus;
        if rendus == 0 || debut >= total {
            break total;
        }
    };
    assert_eq!(total, NB as u64, "TotalMatches de Search(artistes)");
    assert_eq!(vus.len(), NB, "artistes rendus par Search");
}
