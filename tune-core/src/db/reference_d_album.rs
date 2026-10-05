//! La référence d'album d'une piste Bandcamp, retrouvée là où Tune l'a rangée
//! (fil 2121, migration 114 / PG 078).
//!
//! # Pourquoi
//!
//! Le `source_id` d'une piste Bandcamp EST son URL de flux bcbits, signée
//! (`ts`, `t`, `token`). Bandcamp la refuse au bout de quelques jours : 410 Gone
//! à 2,7 jours dans le journal de FabienM (fil 2121), et le chemin nu, sans
//! signature, rend 403 (voir `streaming::favorites_identity`). Seule une
//! nouvelle lecture de la page album ou piste donne une signature fraîche, et
//! l'adresse de cette page, c'est `StreamTrack.album_id`, que la migration 114
//! range désormais avec la piste dans `queue_items`, `streaming_favorites` et
//! `listen_history` (colonne `album_ref`).
//!
//! # Ce que fait ce module
//!
//! Une demande de lecture n'arrive pas toujours avec sa référence : le client
//! web rejoue un favori ou une ligne d'historique par `source` + `source_id`,
//! sans rien d'autre. [`reference_d_album_bandcamp`] la cherche alors dans les
//! trois tables, par l'IDENTITÉ de la piste — le chemin de l'URL sans sa
//! requête ([`identite_de_favori`]), qui ne change pas d'une signature à
//! l'autre. Une ligne écrite sous la signature du 30/09 est donc retrouvée par
//! une demande qui porte celle du 03/10, ou le chemin nu d'un favori.
//!
//! L'appelant ne la demande que pour une piste Bandcamp (`source ==
//! "bandcamp"`), et la fonction rend `None` pour tout identifiant qui n'a pas
//! la forme d'une URL de flux (`http(s)://…/stream/…`).

use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};

/// L'identité d'une URL de flux Bandcamp : l'URL sans sa requête ni son
/// fragment, ou `None` si ce n'est pas une URL de flux.
///
/// Même coupe que `streaming::favorites_identity::identite_de_favori`, mais
/// sans exiger l'hôte `bcbits.com` : l'appelant a déjà établi que la piste
/// vient de Bandcamp, et c'est ce qui permet au banc d'essai de servir les
/// flux depuis un faux bcbits local. Le chemin `/stream/` reste exigé : un
/// `stream_redirect?enc=…` d'achat porte son identité dans la requête, et la
/// couper confondrait toutes les pistes achetées.
pub(crate) fn identite_de_flux(url: &str) -> Option<&str> {
    let reste = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let chemin = reste.split(['?', '#']).next().unwrap_or(reste);
    let debut_chemin = chemin.find('/')?;
    if !chemin[debut_chemin..].starts_with("/stream/") {
        return None;
    }
    let fin = url.len() - reste.len() + chemin.len();
    Some(&url[..fin])
}

/// Les trois tables qui gardent la référence, dans l'ordre où on les lit :
/// la file d'abord (la plus fraîche), puis les favoris, puis l'historique (le
/// plus récent en premier). `(table, colonne de l'identifiant)`.
const TABLES: &[(&str, &str)] = &[
    ("queue_items", "source_id"),
    ("streaming_favorites", "service_id"),
    ("listen_history", "source_id"),
];

/// Échappe `%`, `_` et `\` pour un `LIKE … ESCAPE '\'` : le motif est un
/// PRÉFIXE littéral, rien de plus.
fn prefixe_like(identite: &str) -> String {
    let mut motif = String::with_capacity(identite.len() + 1);
    for c in identite.chars() {
        if matches!(c, '%' | '_' | '\\') {
            motif.push('\\');
        }
        motif.push(c);
    }
    motif.push('%');
    motif
}

/// La requête d'une table : les lignes dont l'identifiant COMMENCE par
/// l'identité de la piste et qui portent une référence. Le `LIKE` ne fait que
/// réduire les candidats ; l'égalité exacte des identités se tranche en Rust
/// (un `LIKE` SQLite ignore la casse, et un préfixe n'est pas une identité).
fn requete(table: &str, colonne: &str, placeholder: &str) -> String {
    format!(
        "SELECT {colonne}, album_ref FROM {table} \
         WHERE album_ref IS NOT NULL AND album_ref != '' \
           AND {colonne} LIKE {placeholder} ESCAPE '\\' \
         ORDER BY id DESC LIMIT 50"
    )
}

/// La référence d'album connue pour cette piste Bandcamp, ou `None`.
///
/// `None` aussi quand la base ne répond pas : la lecture se poursuit alors
/// comme avant la migration 114, sans nouvelle résolution, et l'échec éventuel
/// est dit par le relais.
pub fn reference_d_album_bandcamp(db: &Arc<dyn DbBackend>, source_id: &str) -> Option<String> {
    let identite = identite_de_flux(source_id)?;
    let motif = prefixe_like(identite);
    let placeholder = match db.engine() {
        super::engine::Engine::Sqlite => "?",
        super::engine::Engine::Postgres => "$1",
    };
    for (table, colonne) in TABLES {
        let sql = requete(table, colonne, placeholder);
        let params: [&dyn ToSqlValue; 1] = [&motif];
        let Ok(lignes) = db.query_many(&sql, &params) else {
            continue;
        };
        let trouvee = lignes.iter().find_map(|cols| {
            let sid = cols.first().and_then(|v| v.as_string())?;
            if identite_de_flux(&sid) != Some(identite) {
                return None;
            }
            cols.get(1).and_then(|v| v.as_string())
        });
        if trouvee.is_some() {
            return trouvee;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::history_repo::{HistoryRepo, ListenRecord};
    use crate::db::migrations::run_migrations;
    use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
    use crate::db::sqlite::SqliteDb;
    use crate::db::streaming_favorites_repo::StreamingFavoritesRepo;
    use crate::db::zone_repo::ZoneRepo;

    const CHEMIN: &str = "https://t4.bcbits.com/stream/e43be2a9/mp3-128/29192493";
    const PAGE: &str = "https://artiste.bandcamp.com/album/disque";

    fn signee(ts: u64) -> String {
        format!("{CHEMIN}?p=0&ts={ts}&t=abcd&token={ts}_x")
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn entree(source_id: &str, album_ref: Option<&str>) -> QueueInput {
        QueueInput::Streaming {
            source: "bandcamp".into(),
            source_id: source_id.into(),
            title: "Meet Her At The Love Parade".into(),
            artist: "Framewerk".into(),
            album: None,
            cover_url: None,
            duration_ms: 300_000,
            track_number: None,
            disc_number: None,
            album_ref: album_ref.map(String::from),
        }
    }

    #[test]
    fn l_identite_d_un_flux_est_l_url_sans_sa_requete() {
        assert_eq!(identite_de_flux(&signee(1)), Some(CHEMIN));
        assert_eq!(identite_de_flux(CHEMIN), Some(CHEMIN));
        assert_eq!(
            identite_de_flux("http://127.0.0.1:9/stream/e4/mp3-128/2?ts=1"),
            Some("http://127.0.0.1:9/stream/e4/mp3-128/2")
        );
        assert_eq!(
            identite_de_flux("https://bandcamp.com/stream_redirect?enc=flac&id=1"),
            None
        );
        assert_eq!(identite_de_flux(PAGE), None);
        assert_eq!(identite_de_flux("123456"), None);
    }

    #[test]
    fn le_motif_like_est_un_prefixe_litteral() {
        assert_eq!(prefixe_like("a_b%c\\d"), "a\\_b\\%c\\\\d%");
    }

    #[test]
    fn une_autre_signature_retrouve_la_reference_rangee_en_file() {
        let db = base();
        let zone = ZoneRepo::with_backend(db.clone())
            .create("Parents", Some("chromecast"), Some("cast-1"))
            .unwrap();
        PlayQueueRepo::with_backend(db.clone())
            .append(zone, &[entree(&signee(1_790_782_809), Some(PAGE))])
            .unwrap();
        assert_eq!(
            reference_d_album_bandcamp(&db, &signee(1_791_020_000)).as_deref(),
            Some(PAGE),
            "la signature change, l'identité de la piste non"
        );
        assert_eq!(
            reference_d_album_bandcamp(&db, CHEMIN).as_deref(),
            Some(PAGE),
            "le chemin nu d'un favori retrouve la même piste"
        );
    }

    #[test]
    fn rien_pour_une_autre_piste_ni_pour_un_autre_service() {
        let db = base();
        let zone = ZoneRepo::with_backend(db.clone())
            .create("Parents", Some("chromecast"), Some("cast-1"))
            .unwrap();
        PlayQueueRepo::with_backend(db.clone())
            .append(zone, &[entree(&signee(1), Some(PAGE))])
            .unwrap();
        // Même empreinte, AUTRE piste : un préfixe n'est pas une identité.
        let voisine = format!("{CHEMIN}0?ts=2");
        assert_eq!(reference_d_album_bandcamp(&db, &voisine), None);
        assert_eq!(reference_d_album_bandcamp(&db, "123456"), None);
    }

    #[test]
    fn une_ligne_sans_reference_ne_rend_rien() {
        let db = base();
        let zone = ZoneRepo::with_backend(db.clone())
            .create("Parents", Some("chromecast"), Some("cast-1"))
            .unwrap();
        PlayQueueRepo::with_backend(db.clone())
            .append(zone, &[entree(&signee(1), None)])
            .unwrap();
        assert_eq!(reference_d_album_bandcamp(&db, &signee(2)), None);
    }

    #[test]
    fn l_historique_et_les_favoris_gardent_la_reference() {
        let db = base();
        HistoryRepo::with_backend(db.clone())
            .record(&ListenRecord {
                title: "Meet Her At The Love Parade".into(),
                source: "bandcamp".into(),
                source_id: Some(signee(1_790_782_809)),
                duration_ms: 300_000,
                album_ref: Some(PAGE.into()),
                ..ListenRecord::default()
            })
            .unwrap();
        assert_eq!(
            reference_d_album_bandcamp(&db, &signee(3)).as_deref(),
            Some(PAGE),
            "l'historique garde la page"
        );

        // Un favori posé APRÈS l'écoute reçoit la référence connue.
        let favoris = StreamingFavoritesRepo::with_backend(db.clone());
        favoris
            .add(
                1,
                "track",
                "bandcamp",
                &signee(4),
                Some("Meet Her"),
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            favoris
                .reference_d_album(1, "track", "bandcamp", CHEMIN)
                .unwrap()
                .as_deref(),
            Some(PAGE),
            "le favori Bandcamp hérite de la page déjà connue"
        );
    }
}
