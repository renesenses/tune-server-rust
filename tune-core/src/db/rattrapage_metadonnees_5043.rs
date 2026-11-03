//! #5043 — quelles pistes n'ont **encore aucune métadonnée étendue**.
//!
//! Le scan ne rangeait les métadonnées étendues (`composer`, `conductor`,
//! `lyricist`, `performer`, `label`, `producer`, `comment`, `bpm`, `isrc`,
//! `mb_*`…) que pour les fichiers de son lot de travail : un fichier
//! **inchangé** n'y entre jamais, donc une bibliothèque constituée avant
//! l'existence de ce bloc restait à zéro. Le « Scan complet » rattrape
//! désormais ces fichiers-là — et **eux seuls**, faute de quoi le surcoût
//! (rouvrir chaque fichier une seconde fois, après la lecture des balises de
//! base) se paierait à chaque scan complet au lieu d'une seule fois.
//!
//! # Le piège, et pourquoi le critère n'est pas « aucune ligne »
//!
//! `track_metadata` ne sert pas qu'au scan. Trois familles de clés y sont
//! posées par d'**autres** écrivains, qui n'ouvrent jamais le fichier pour ses
//! crédits :
//!
//! | préfixe  | écrivain                                   |
//! |----------|--------------------------------------------|
//! | `rg_`    | l'analyse ReplayGain                       |
//! | `dr_`    | la mesure de plage dynamique (et sa gravure) |
//! | `upnp_`  | l'indexation UPnP (`upnp_object_id`, `upnp_res_url`, `upnp_serveur`) |
//!
//! Sur le .18, le 25/09/2026, **15 151 pistes** portaient des lignes `rg_*` /
//! `dr_*` sans la moindre métadonnée étendue. Compter les lignes de
//! `track_metadata` aurait déclaré ces 15 151 pistes « déjà faites » et les
//! aurait sautées à vie. Le critère porte donc sur les **clés**, pas sur leur
//! nombre : une piste est à rattraper tant qu'elle n'a **aucune** clé hors de
//! ces trois préfixes.
//!
//! Les pistes UPnP, elles, n'ont pas de fichier : il n'y a rien à rouvrir. Le
//! scan ne leur présente jamais de chemin, la question ne se pose pas ici.
//!
//! # #6781 — une clé ajoutée APRÈS coup ne se rattrapait jamais
//!
//! « Au moins une métadonnée étendue » ne dit pas QUAND le fichier a été relu.
//! #6232 (rc7) a ajouté la clé `track_artist` (balise ARTIST, critère
//! « Interprète de la piste ») : une piste relue avant la rc7 avait déjà ses
//! crédits, comptait donc comme pourvue, et ne recevait jamais la nouvelle
//! clé — même au « Scan complet ». La règle lisait alors le repli (l'artiste
//! de l'ALBUM), et ne trouvait pas la piste (Didier, fil 2218).
//!
//! Une piste n'est donc pourvue que si elle porte AUSSI chaque clé de
//! [`CLES_DE_GENERATION`]. Le prix : une piste SANS balise ARTIST est relue à
//! chaque scan complet (et à lui seul), faute de pouvoir distinguer « relue,
//! pas de balise » de « jamais relue ».

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Préfixes de clés de `track_metadata` posés par un écrivain qui n'est PAS la
/// lecture des balises du fichier. Voir la note de module.
pub const PREFIXES_RESERVES: [&str; 3] = ["rg_", "dr_", "upnp_"];

/// #6781 — clés que `read_extended_metadata` pose depuis une version
/// RÉCENTE : leur absence dit que le fichier n'a pas été relu depuis. Voir la
/// note de module. Une clé ajoutée à la relecture et que les règles lisent
/// s'ajoute ici.
pub const CLES_DE_GENERATION: [&str; 1] = [crate::metadata::CLE_INTERPRETE_DE_LA_PISTE];

/// Vrai quand cette clé est une métadonnée étendue, c'est-à-dire quelque chose
/// que seule la relecture du FICHIER peut poser.
///
/// `dr_track` et `rg_track_gain` sortent bien de `read_extended_metadata`,
/// mais ils sortent AUSSI de la mesure DR et de l'analyse ReplayGain : leur
/// présence ne prouve donc pas que le fichier ait jamais été relu pour ses
/// crédits. Ils ne comptent pas.
pub fn est_une_metadonnee_etendue(cle: &str) -> bool {
    !PREFIXES_RESERVES.iter().any(|p| cle.starts_with(p))
}

/// Les `track_id` qui portent DÉJÀ au moins une métadonnée étendue.
///
/// `substr` et non `LIKE` : `_` est un joker de `LIKE`, et le neutraliser
/// demande un `ESCAPE` dont l'écriture diffère d'un moteur à l'autre. `substr`
/// se lit pareil sous SQLite et sous PostgreSQL, et ne joker rien.
///
/// La requête ne touche QUE `track_metadata` : `track_id` y est un entier sous
/// SQLite et du texte sous PostgreSQL, et toute jointure avec `tracks.id`
/// imposerait une conversion par moteur.
///
/// #6781 — et chaque clé de [`CLES_DE_GENERATION`] : une sous-requête par
/// clé, sur la même table (`track_id` comparé à `track_id`, même type).
pub fn sql_pistes_deja_pourvues() -> String {
    let mut conditions = String::new();
    for (i, prefixe) in PREFIXES_RESERVES.iter().enumerate() {
        if i > 0 {
            conditions.push_str(" AND ");
        }
        conditions.push_str(&format!("substr(key, 1, {}) <> '{prefixe}'", prefixe.len()));
    }
    for cle in CLES_DE_GENERATION {
        conditions.push_str(&format!(
            " AND track_id IN (SELECT g.track_id FROM track_metadata g WHERE g.key = '{cle}')"
        ));
    }
    format!("SELECT DISTINCT track_id FROM track_metadata WHERE {conditions}")
}

/// Lit en base l'ensemble des pistes déjà pourvues.
///
/// Une seule requête pour toute la bibliothèque : le scan la pose AVANT sa
/// boucle de lots et la consulte en mémoire, plutôt que d'interroger la base
/// une fois par fichier (47 079 requêtes sur le .18).
pub fn pistes_deja_pourvues(db: &Arc<dyn DbBackend>) -> Result<HashSet<i64>, String> {
    let lignes = db.query_many(&sql_pistes_deja_pourvues(), &[])?;
    Ok(lignes
        .into_iter()
        .filter_map(|cols| {
            let v = cols.first()?;
            // PostgreSQL tient `track_id` en TEXT, SQLite en INTEGER.
            v.as_i64().or_else(|| v.as_string()?.parse::<i64>().ok())
        })
        .collect())
}

/// Combien de chemins au plus par requête. SQLite plafonne le nombre de
/// paramètres liés ; un lot de scan en porte 500, alignés sur les dossiers et
/// donc parfois un peu plus.
const CHEMINS_PAR_REQUETE: usize = 400;

fn sql_ids_par_chemin<D: SqlDialect>(d: &D, nb: usize) -> String {
    let placeholders: Vec<String> = (1..=nb).map(|i| d.placeholder(i)).collect();
    format!(
        "SELECT file_path, id FROM tracks WHERE file_path IN ({})",
        placeholders.join(", ")
    )
}

/// `file_path` → `tracks.id`, lu par une lecture **forte**.
///
/// # Pourquoi `query_many_strong` et non le dépôt
///
/// Le bloc de métadonnées étendues du scan tourne DANS la transaction du lot
/// (`BEGIN IMMEDIATE` sur la connexion d'écriture). `TrackRepo::get_by_path`
/// passe, lui, par le pool de lecture : sous SQLite ce sont des connexions
/// SÉPARÉES, qui ne voient pas ce que la transaction en cours vient d'écrire.
/// Elle rendait donc `None` pour chaque piste du lot, le `if let Ok(Some(..))`
/// avalait le cas, et **aucune** métadonnée étendue n'entrait jamais en base.
/// C'est la cause mesurée de #5043 : sur le .18, un scan complet de 46 965
/// fichiers le 23/09/2026 n'a posé pas une seule clé de crédit.
///
/// Le défaut ne se voit pas sur une base `:memory:` : là, les connexions de
/// lecture sont des CLONES de la connexion d'écriture. Les épreuves de ce
/// correctif tournent donc sur une base de FICHIER.
///
/// Une requête par tranche de chemins, et non une par fichier : le scan en
/// présente 500 par lot.
pub fn ids_par_chemin(
    db: &Arc<dyn DbBackend>,
    chemins: &[String],
) -> Result<HashMap<String, i64>, String> {
    let mut ids = HashMap::with_capacity(chemins.len());
    for tranche in chemins.chunks(CHEMINS_PAR_REQUETE) {
        let sql = match db.engine() {
            Engine::Sqlite => sql_ids_par_chemin(&SqliteDialect, tranche.len()),
            Engine::Postgres => sql_ids_par_chemin(&PostgresDialect, tranche.len()),
        };
        let params: Vec<&dyn ToSqlValue> = tranche.iter().map(|c| c as &dyn ToSqlValue).collect();
        for cols in db.query_many_strong(&sql, &params)? {
            let (Some(chemin), Some(id)) = (
                cols.first().and_then(|v| v.as_string()),
                cols.get(1).and_then(|v| v.as_i64()),
            ) else {
                continue;
            };
            ids.insert(chemin, id);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_cles_de_credit_sont_des_metadonnees_etendues() {
        for cle in [
            "composer",
            "conductor",
            "lyricist",
            "performer",
            "remixer",
            "label",
            "producer",
            "comment",
            "bpm",
            "grouping",
            "sort_artist",
            "isrc",
            "mb_track_id",
        ] {
            assert!(
                est_une_metadonnee_etendue(cle),
                "`{cle}` sort de la relecture du fichier"
            );
        }
    }

    /// LE piège de #5043 : 15 151 pistes du .18 portent ces clés-là et RIEN
    /// d'autre. Les compter comme « déjà faites » les sauterait à vie.
    #[test]
    fn les_cles_replaygain_dr_et_upnp_ne_prouvent_aucune_relecture() {
        for cle in [
            "rg_track_gain",
            "rg_track_peak",
            "rg_album_gain",
            "rg_album_peak",
            "dr_track",
            "dr_album",
            "dr_source",
            "upnp_object_id",
            "upnp_res_url",
            "upnp_serveur",
        ] {
            assert!(
                !est_une_metadonnee_etendue(cle),
                "`{cle}` a son propre écrivain : sa présence ne prouve pas que le fichier ait \
                 été relu"
            );
        }
    }

    #[test]
    fn la_requete_ecarte_les_trois_prefixes_sans_joker() {
        let sql = sql_pistes_deja_pourvues();
        for prefixe in PREFIXES_RESERVES {
            assert!(
                sql.contains(&format!("<> '{prefixe}'")),
                "le préfixe `{prefixe}` doit être écarté : {sql}"
            );
        }
        assert!(
            !sql.contains("LIKE"),
            "`LIKE` traiterait `_` comme un joker : {sql}"
        );
    }

    fn base(lignes: &[(i64, &str)]) -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS track_metadata \
             (track_id INTEGER NOT NULL, key TEXT NOT NULL, value TEXT, \
              PRIMARY KEY (track_id, key));",
        )
        .unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        for (id, cle) in lignes {
            backend
                .execute(
                    "INSERT OR REPLACE INTO track_metadata (track_id, key, value) VALUES (?1, ?2, 'v')",
                    &[id as &dyn ToSqlValue, cle as &dyn ToSqlValue],
                )
                .unwrap();
        }
        backend
    }

    /// #6781 (Didier, fil 2218) — TÉMOIN ROUGE avant le correctif : la piste 1,
    /// relue avant la rc7 (crédits, pas de `track_artist`), était déclarée
    /// pourvue, et le scan complet ne lui posait jamais sa balise ARTIST.
    #[test]
    fn une_piste_relue_avant_la_cle_track_artist_reste_a_rattraper_6781() {
        let db = base(&[
            (1, "composer"),
            (1, "mb_track_id"),
            (2, "composer"),
            (2, "track_artist"),
            (3, "rg_track_gain"),
        ]);
        let pourvues = pistes_deja_pourvues(&db).unwrap();
        assert!(
            !pourvues.contains(&1),
            "relue avant #6232 : à relire pour `track_artist` ({pourvues:?})"
        );
        assert!(
            pourvues.contains(&2),
            "relue depuis : rien à refaire ({pourvues:?})"
        );
        assert!(
            !pourvues.contains(&3),
            "#5043 tient toujours ({pourvues:?})"
        );
    }

    /// Contre-cas : une piste qui porte `track_artist`, même sans autre
    /// crédit, a été relue depuis #6232 — le scan complet ne la rouvre pas.
    #[test]
    fn la_cle_track_artist_seule_prouve_une_relecture_recente_6781() {
        let db = base(&[(7, "track_artist"), (7, "dr_track")]);
        assert!(pistes_deja_pourvues(&db).unwrap().contains(&7));
    }
}
