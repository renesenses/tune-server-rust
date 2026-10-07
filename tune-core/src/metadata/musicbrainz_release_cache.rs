//! Les réponses `/release/{mbid}` de MusicBrainz, gardées en base (#4805, A → E).
//!
//! Idée : MetaRust (`cache.rs`, `make_key`), de Xavier Joly — une release se
//! demande UNE fois avec tous ses `inc`, et ne se redemande plus. Le stockage
//! est réécrit pour Tune : une table de la base, pas un fichier JSON tenu en
//! mémoire, qui ne tiendrait pas sur une bibliothèque cloud.
//!
//! # Pourquoi
//!
//! Jusqu'ici, un album identifié coûtait DEUX requêtes pour le même pressage :
//! `lookup_release_detail` (`inc=recordings+artist-credits+labels`) à
//! l'identification, puis `lookup_release_credits` (`INC_CREDITS_RELEASE`)
//! à la passe des crédits. Désormais l'identification demande le jeu complet
//! ([`super::musicbrainz_release::INC_RELEASE_COMPLET`]) et le garde ici ; la
//! passe des crédits lit d'abord cette table et ne part sur le réseau que si
//! la release manque, est périmée, ou a été gardée avec des `inc` qui ne
//! couvrent pas sa demande.
//!
//! # Forme
//!
//! `musicbrainz_release_cache(mbid, inc, corps, fetched_at)` :
//! * `inc` : les `inc` de la réponse, **triés et dédoublonnés** (l'idée de
//!   `make_key`) — `recordings+labels` et `labels+recordings` sont la même
//!   demande ;
//! * `corps` : le JSON **compressé** (deflate/zlib). Sans perte : le lot
//!   « identification étape B » lit les mêmes réponses, et un champ retiré ici
//!   lui manquerait. Mesuré sur la réponse réelle de *Déjà vu*
//!   (`tests/fixtures/musicbrainz/release_deja_vu_credits.json`) : 109 765
//!   octets → environ 9,5 Ko ;
//! * `fetched_at` : ISO-8601 UTC. Au-delà de [`DUREE_DE_VALIDITE_JOURS`], la
//!   ligne ne sert plus et se remplace à la lecture suivante ;
//!   [`purger`] retire les périmées et celles qu'aucun album ne porte plus.
//!
//! Aucune écriture dans les fichiers : la base seulement.

use std::io::{Read, Write};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use tracing::{debug, warn};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};

/// Durée de validité d'une réponse gardée. Une release change peu (un crédit
/// ajouté, un label corrigé) ; 90 jours laissent une campagne identification
/// + crédits se faire sur la même réponse, et rafraîchissent au trimestre.
pub const DUREE_DE_VALIDITE_JOURS: i64 = 90;

/// Le format de `fetched_at`, le même que le reste du schéma.
const FORMAT_DATE: &str = "%Y-%m-%dT%H:%M:%SZ";

/// Les `inc` sous leur forme canonique : séparés par `+`, vides retirés,
/// triés, dédoublonnés.
pub fn inc_canonique(inc: &str) -> String {
    let mut v: Vec<&str> = inc
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    v.sort_unstable();
    v.dedup();
    v.join("+")
}

/// `true` si une réponse gardée avec `gardes` contient tout ce que `demandes`
/// réclame. Un jeu plus large convient ; un jeu plus étroit, non.
pub fn inc_couvre(gardes: &str, demandes: &str) -> bool {
    let gardes = inc_canonique(gardes);
    let gardes: Vec<&str> = gardes.split('+').collect();
    inc_canonique(demandes)
        .split('+')
        .filter(|s| !s.is_empty())
        .all(|d| gardes.contains(&d))
}

/// Compresse un JSON (zlib, niveau par défaut).
pub fn compresser(json: &Value) -> Vec<u8> {
    let brut = serde_json::to_vec(json).unwrap_or_default();
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    if enc.write_all(&brut).is_err() {
        return Vec::new();
    }
    enc.finish().unwrap_or_default()
}

/// L'inverse de [`compresser`]. `None` sur un corps illisible : l'appelant le
/// traite comme une absence et redemande.
pub fn decompresser(corps: &[u8]) -> Option<Value> {
    let mut brut = Vec::new();
    flate2::read::ZlibDecoder::new(corps)
        .read_to_end(&mut brut)
        .ok()?;
    serde_json::from_slice(&brut).ok()
}

fn date(t: DateTime<Utc>) -> String {
    t.format(FORMAT_DATE).to_string()
}

fn limite_de_fraicheur(maintenant: DateTime<Utc>) -> String {
    date(maintenant - Duration::days(DUREE_DE_VALIDITE_JOURS))
}

/// La réponse gardée pour `mbid`, si elle est fraîche ET couvre `inc_demandes`.
///
/// Toute erreur (table absente, corps illisible) rend `None` : le cache est
/// une économie, jamais une condition. L'appelant interroge alors le réseau.
pub fn lire(
    backend: &Arc<dyn DbBackend>,
    mbid: &str,
    inc_demandes: &str,
    maintenant: DateTime<Utc>,
) -> Option<Value> {
    let mbid = mbid.trim();
    if mbid.is_empty() {
        return None;
    }
    let limite = limite_de_fraicheur(maintenant);
    let ligne = match backend.query_one(
        "SELECT inc, corps FROM musicbrainz_release_cache \
         WHERE mbid = ? AND fetched_at >= ?",
        &[&mbid as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
    ) {
        Ok(l) => l?,
        Err(e) => {
            debug!(mbid, erreur = %e, "mb_release_cache_lecture_impossible");
            return None;
        }
    };
    let inc = ligne.first().and_then(SqlValue::as_str)?;
    if !inc_couvre(inc, inc_demandes) {
        debug!(mbid, inc, inc_demandes, "mb_release_cache_inc_insuffisants");
        return None;
    }
    let v = decompresser(ligne.get(1).and_then(SqlValue::as_blob)?);
    if v.is_none() {
        warn!(mbid, "mb_release_cache_corps_illisible");
    }
    v
}

/// Garde (ou remplace) la réponse de `mbid`, obtenue avec `inc`.
pub fn ecrire(
    backend: &Arc<dyn DbBackend>,
    mbid: &str,
    inc: &str,
    json: &Value,
    maintenant: DateTime<Utc>,
) {
    let mbid = mbid.trim();
    if mbid.is_empty() {
        return;
    }
    let corps = compresser(json);
    if corps.is_empty() {
        return;
    }
    let corps = SqlValue::Blob(corps);
    let inc = inc_canonique(inc);
    let quand = date(maintenant);
    if let Err(e) = backend.execute(
        "INSERT INTO musicbrainz_release_cache (mbid, inc, corps, fetched_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT (mbid) DO UPDATE SET \
         inc = excluded.inc, corps = excluded.corps, fetched_at = excluded.fetched_at",
        &[
            &mbid as &dyn ToSqlValue,
            &inc as &dyn ToSqlValue,
            &corps as &dyn ToSqlValue,
            &quand as &dyn ToSqlValue,
        ],
    ) {
        warn!(mbid, erreur = %e, "mb_release_cache_ecriture_impossible");
    }
}

/// Retire les réponses périmées, et celles d'un pressage qu'aucun album ne
/// porte plus (ré-identifié ailleurs, album supprimé). Rend le nombre de
/// lignes retirées. La taille de la table reste ainsi bornée par le nombre
/// d'albums identifiés.
pub fn purger(backend: &Arc<dyn DbBackend>, maintenant: DateTime<Utc>) -> usize {
    let limite = limite_de_fraicheur(maintenant);
    match backend.execute(
        "DELETE FROM musicbrainz_release_cache WHERE fetched_at < ? \
         OR NOT EXISTS (SELECT 1 FROM albums a \
                        WHERE a.musicbrainz_release_id = musicbrainz_release_cache.mbid)",
        &[&limite as &dyn ToSqlValue],
    ) {
        Ok(n) => n,
        Err(e) => {
            debug!(erreur = %e, "mb_release_cache_purge_impossible");
            0
        }
    }
}

/// Vide la table : la passe des crédits lancée avec `force` veut TOUT
/// réinterroger, réponses gardées comprises.
pub fn oublier_tout(backend: &Arc<dyn DbBackend>) {
    if let Err(e) = backend.execute("DELETE FROM musicbrainz_release_cache", &[]) {
        debug!(erreur = %e, "mb_release_cache_oubli_impossible");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn les_inc_se_comparent_tries_et_par_inclusion() {
        assert_eq!(
            inc_canonique("recordings+labels+artist-credits+labels"),
            "artist-credits+labels+recordings"
        );
        assert_eq!(inc_canonique(" + "), "");
        assert!(inc_couvre(
            super::super::musicbrainz_release::INC_RELEASE_COMPLET,
            super::super::musicbrainz_release::INC_CREDITS_RELEASE
        ));
        assert!(inc_couvre(
            super::super::musicbrainz_release::INC_RELEASE_COMPLET,
            super::super::musicbrainz_release::INC_DETAIL_RELEASE
        ));
        assert!(inc_couvre("labels+recordings", "recordings+labels"));
        assert!(!inc_couvre(
            "recordings+artist-credits+labels",
            super::super::musicbrainz_release::INC_CREDITS_RELEASE
        ));
    }

    #[test]
    fn la_compression_est_sans_perte_et_reduit_la_reponse_reelle() {
        let reelle: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/musicbrainz/release_deja_vu_credits.json"
        ))
        .unwrap();
        let brut = serde_json::to_vec(&reelle).unwrap().len();
        let corps = compresser(&reelle);
        assert_eq!(decompresser(&corps).as_ref(), Some(&reelle));
        assert!(
            corps.len() * 5 < brut,
            "compression trop faible : {} → {}",
            brut,
            corps.len()
        );
        assert_eq!(decompresser(b"pas du zlib"), None);
    }

    #[test]
    fn lire_rend_la_reponse_fraiche_qui_couvre_la_demande() {
        let b = base();
        let r = json!({"id": "r1", "media": []});
        let maintenant = t("2026-10-05T12:00:00Z");
        assert_eq!(lire(&b, "r1", "recordings", maintenant), None);

        ecrire(&b, "r1", "recordings+labels", &r, maintenant);
        assert_eq!(lire(&b, "r1", "labels", maintenant), Some(r.clone()));
        assert_eq!(
            lire(&b, " r1 ", "labels+recordings", maintenant),
            Some(r.clone())
        );
        // Des `inc` que la réponse gardée n'a pas : on redemande.
        assert_eq!(lire(&b, "r1", "recordings+work-rels", maintenant), None);
        // Fraîche jusqu'à 90 jours, périmée au-delà.
        assert_eq!(
            lire(&b, "r1", "labels", t("2027-01-03T12:00:00Z")),
            Some(r.clone())
        );
        assert_eq!(lire(&b, "r1", "labels", t("2027-01-04T12:00:01Z")), None);

        // Une réécriture remplace, elle n'ajoute pas.
        let r2 = json!({"id": "r1", "media": [1]});
        ecrire(&b, "r1", "recordings+labels+work-rels", &r2, maintenant);
        assert_eq!(lire(&b, "r1", "work-rels", maintenant), Some(r2));
        let n = b
            .query_one("SELECT COUNT(*) FROM musicbrainz_release_cache", &[])
            .unwrap()
            .unwrap()[0]
            .as_i64();
        assert_eq!(n, Some(1));
    }

    #[test]
    fn purger_retire_le_perime_et_l_orphelin() {
        let b = base();
        b.execute(
            "INSERT INTO albums (title, musicbrainz_release_id) VALUES ('A', 'garde'), ('B', 'vieux')",
            &[],
        )
        .unwrap();
        let maintenant = t("2026-10-05T12:00:00Z");
        let r = json!({"id": "x"});
        ecrire(&b, "garde", "recordings", &r, maintenant);
        ecrire(&b, "vieux", "recordings", &r, t("2026-06-01T00:00:00Z"));
        ecrire(&b, "orphelin", "recordings", &r, maintenant);
        assert_eq!(purger(&b, maintenant), 2);
        assert!(lire(&b, "garde", "recordings", maintenant).is_some());
        oublier_tout(&b);
        assert!(lire(&b, "garde", "recordings", maintenant).is_none());
    }

    /// La table n'a pas de migration numérotée (comme `streaming_hidden_items`) :
    /// elle doit donc exister sur les QUATRE chemins de schéma. Une base
    /// PostgreSQL convertie porte `schema_version = 99` et ne verrait jamais
    /// un script numéroté ; seul `ENSURE_TABLES` l'atteint.
    #[test]
    fn la_table_existe_sur_les_quatre_chemins() {
        let racine = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for (fichier, colonne) in [
            ("src/db/sqlite.rs", "corps BLOB NOT NULL"),
            ("src/db/migrations.rs", "corps BLOB NOT NULL"),
            ("src/db/pg_migrate.rs", "corps BYTEA NOT NULL"),
            ("src/db/postgres.rs", "corps BYTEA NOT NULL"),
        ] {
            let src = std::fs::read_to_string(racine.join(fichier)).unwrap();
            let debut = src
                .find("CREATE TABLE IF NOT EXISTS musicbrainz_release_cache (")
                .unwrap_or_else(|| panic!("{fichier} : la table manque"));
            let bloc = &src[debut..debut + 260.min(src.len() - debut)];
            assert!(bloc.contains(colonne), "{fichier} : {colonne} manque");
        }
        let pg = std::fs::read_to_string(racine.join("src/db/pg_migrate.rs")).unwrap();
        assert!(
            pg.contains("\"musicbrainz_release_cache\",\n"),
            "musicbrainz_release_cache n'est classée nulle part pour la bascule"
        );
    }
}

/// La même table sur le VRAI PostgreSQL (`corps` en BYTEA, `ON CONFLICT`,
/// `NOT EXISTS` corrélé). Sautée sans `TUNE_TEST_PG_URL`. La table est posée
/// par l'entrée d'`ENSURE_TABLES` elle-même : c'est ce chemin, joué à chaque
/// démarrage, qui l'amène sur toute base PostgreSQL.
#[cfg(all(test, feature = "postgres"))]
mod tests_pg {
    use super::*;
    use serde_json::json;

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_musicbrainz_release_cache_aller_retour() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
            return;
        };
        let pool = sqlx::PgPool::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
        let b: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool));
        let ddl = crate::db::postgres::ENSURE_TABLES
            .iter()
            .find(|s| s.contains("musicbrainz_release_cache"))
            .expect("ENSURE_TABLES porte la table");
        b.execute(ddl, &[]).unwrap();
        b.execute("DELETE FROM musicbrainz_release_cache", &[])
            .unwrap();

        let maintenant = DateTime::parse_from_rfc3339("2026-10-05T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let reelle: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/musicbrainz/release_deja_vu_credits.json"
        ))
        .unwrap();
        ecrire(
            &b,
            "pg-r1",
            "recordings+labels",
            &json!({"id": "v1"}),
            maintenant,
        );
        ecrire(
            &b,
            "pg-r1",
            "labels+recordings+work-rels",
            &reelle,
            maintenant,
        );
        assert_eq!(
            lire(&b, "pg-r1", "work-rels", maintenant).as_ref(),
            Some(&reelle)
        );
        assert_eq!(lire(&b, "pg-r1", "artist-rels", maintenant), None);
        let n = b
            .query_one("SELECT COUNT(*) FROM musicbrainz_release_cache", &[])
            .unwrap()
            .unwrap()[0]
            .as_i64();
        assert_eq!(n, Some(1));
        // Aucun album ne porte `pg-r1` : la purge la retire.
        assert!(purger(&b, maintenant) >= 1);
        assert_eq!(lire(&b, "pg-r1", "labels", maintenant), None);
    }
}
