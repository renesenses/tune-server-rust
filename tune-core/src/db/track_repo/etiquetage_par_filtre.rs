//! Poser (ou retirer) une étiquette sur TOUT le résultat d'une vue filtrée
//! d'Oxygen, en un geste (#7128, fil 2465 de r5yk5).
//!
//! # Pourquoi à partir des FILTRES et non d'une liste d'identifiants
//!
//! Oxygen pagine : la page affichée ne porte que 200 pistes, le résultat
//! filtré peut en compter des milliers. Faire envoyer par le navigateur la
//! liste complète des identifiants l'obligerait à tout charger d'abord, et
//! `POST /tags/{id}/items/batch` écrit une ligne par requête SQL. Ici le
//! serveur reçoit les MÊMES paramètres de facette que `GET /library/tracks`
//! et désigne lui-même l'ensemble, avec le `WHERE` de la liste
//! ([`TrackRepo::conditions_du_filtre`], socle de la vue compris : albums
//! masqués, doublons distants, copies de moindre qualité). Ce qui est
//! étiqueté est donc exactement ce que la vue montre.
//!
//! # Une seule instruction, donc une seule transaction
//!
//! La pose est un `INSERT … SELECT … ON CONFLICT DO NOTHING`, le retrait un
//! `DELETE … WHERE item_id IN (SELECT …)`. Une instruction SQL est atomique
//! sous SQLite comme sous PostgreSQL : tout le lot passe, ou rien. Le
//! `ON CONFLICT` sur `UNIQUE(tag_id, item_type, item_id)` rend la pose
//! idempotente : la relancer ne crée aucun doublon et ne réécrit pas la date
//! du premier dépôt.
//!
//! # Deux garde-fous
//!
//! - un filtre INACTIF est refusé : sans prédicat, l'ensemble serait la
//!   bibliothèque entière, et « tout taguer par accident » est exactement ce
//!   qu'il ne faut pas permettre ;
//! - au-delà de [`PLAFOND_ETIQUETAGE_PAR_FILTRE`] objets, rien n'est écrit.

use super::{Placeholders, SqlValue, ToSqlValue, TrackFilter, TrackRepo};
use crate::TuneError;
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Nombre maximal d'objets qu'un seul geste peut étiqueter ou désétiqueter.
///
/// Assez haut pour « tout le jazz vocal » d'une grande bibliothèque, assez
/// bas pour qu'une erreur de filtre ne touche pas 50 000 pistes d'un coup.
pub const PLAFOND_ETIQUETAGE_PAR_FILTRE: i64 = 20_000;

/// Ce que l'on étiquette : les pistes du résultat, ou leurs albums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CibleDuFiltre {
    Pistes,
    Albums,
}

impl CibleDuFiltre {
    /// `track` / `album`, les noms d'`item_tags.item_type`.
    pub fn depuis_item_type(item_type: &str) -> Option<Self> {
        match item_type {
            "track" => Some(Self::Pistes),
            "album" => Some(Self::Albums),
            _ => None,
        }
    }

    pub fn item_type(self) -> &'static str {
        match self {
            Self::Pistes => "track",
            Self::Albums => "album",
        }
    }

    /// La colonne qui désigne l'objet dans la requête des pistes.
    fn colonne(self) -> &'static str {
        match self {
            Self::Pistes => "t.id",
            Self::Albums => "t.album_id",
        }
    }
}

/// Pourquoi un étiquetage par filtre est refusé avant toute écriture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusEtiquetage {
    /// Aucun filtre actif : l'ensemble serait la bibliothèque entière.
    FiltreVide,
    /// Le résultat dépasse [`PLAFOND_ETIQUETAGE_PAR_FILTRE`].
    TropDObjets { nombre: i64, plafond: i64 },
}

/// Le résultat d'un geste : les objets désignés par le filtre, et ceux que
/// le geste a réellement modifiés (posés pour la première fois, ou retirés).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BilanEtiquetage {
    pub designes: i64,
    pub modifies: i64,
}

impl TrackRepo {
    /// Le `SELECT DISTINCT <colonne> FROM tracks t … WHERE …` qui désigne
    /// l'ensemble, ses marqueurs tirés de `ph` APRÈS ceux déjà pris.
    fn sous_requete_du_filtre(
        &self,
        f: &TrackFilter,
        cible: CibleDuFiltre,
        ph: &mut Placeholders,
    ) -> (String, Vec<SqlValue>) {
        let (mut conditions, params) = self.conditions_du_filtre(f, ph);
        if cible == CibleDuFiltre::Albums {
            conditions.push("t.album_id IS NOT NULL".to_string());
        }
        let sql = format!(
            "SELECT DISTINCT {col} AS id FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE {conds}",
            col = cible.colonne(),
            conds = conditions.join(" AND "),
        );
        (sql, params)
    }

    /// Le nombre d'objets que le filtre désigne — l'aperçu « 1 234 morceaux
    /// seront tagués ». Refuse un filtre inactif, comme la pose.
    pub fn compter_pour_etiquetage(
        &self,
        f: &TrackFilter,
        cible: CibleDuFiltre,
    ) -> Result<Result<i64, RefusEtiquetage>, TuneError> {
        if !f.is_active() {
            return Ok(Err(RefusEtiquetage::FiltreVide));
        }
        let mut ph = Placeholders::new(self.db.engine());
        let (sous, params) = self.sous_requete_du_filtre(f, cible, &mut ph);
        let sql = format!("SELECT COUNT(*) FROM ({sous}) x");
        let refs: Vec<&dyn ToSqlValue> = params.iter().map(|v| v as &dyn ToSqlValue).collect();
        let n = self
            .db
            .query_one(&sql, &refs)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(Ok(n))
    }

    /// Pose (`retirer = false`) ou retire l'étiquette `tag_id` sur tout
    /// l'ensemble désigné par `f`, en UNE instruction SQL.
    ///
    /// Le refus (filtre vide, plafond dépassé) est rendu AVANT toute
    /// écriture. L'existence de l'étiquette est à vérifier par l'appelant.
    pub fn etiqueter_par_filtre(
        &self,
        tag_id: i64,
        f: &TrackFilter,
        cible: CibleDuFiltre,
        retirer: bool,
    ) -> Result<Result<BilanEtiquetage, RefusEtiquetage>, TuneError> {
        let designes = match self.compter_pour_etiquetage(f, cible)? {
            Ok(n) => n,
            Err(refus) => return Ok(Err(refus)),
        };
        if designes > PLAFOND_ETIQUETAGE_PAR_FILTRE {
            return Ok(Err(RefusEtiquetage::TropDObjets {
                nombre: designes,
                plafond: PLAFOND_ETIQUETAGE_PAR_FILTRE,
            }));
        }
        if designes == 0 {
            return Ok(Ok(BilanEtiquetage {
                designes: 0,
                modifies: 0,
            }));
        }
        let engine = self.db.engine();
        // Les deux premiers marqueurs (étiquette, type) sont pris AVANT ceux du
        // filtre : en SQLite, les `?` se lient dans l'ordre où ils paraissent,
        // et ils paraissent avant le `WHERE` de la sous-requête.
        let mut ph = Placeholders::new(engine);
        let ph_tag = ph.take();
        let ph_type = ph.take();
        let (sous, params_filtre) = self.sous_requete_du_filtre(f, cible, &mut ph);
        let sql = if retirer {
            format!(
                "DELETE FROM item_tags WHERE tag_id = {ph_tag} AND item_type = {ph_type} \
                 AND item_id IN ({sous})"
            )
        } else {
            let maintenant = match engine {
                Engine::Sqlite => SqliteDialect.now_iso8601(),
                Engine::Postgres => PostgresDialect.now_iso8601(),
            };
            // `WHERE 1 = 1` : sans clause `WHERE` dans le `SELECT`, SQLite
            // lit le `ON` du `ON CONFLICT` comme celui d'une jointure.
            format!(
                "INSERT INTO item_tags (tag_id, item_type, item_id, created_at) \
                 SELECT CAST({ph_tag} AS BIGINT), CAST({ph_type} AS TEXT), x.id, {maintenant} \
                 FROM ({sous}) x WHERE 1 = 1 \
                 ON CONFLICT (tag_id, item_type, item_id) DO NOTHING"
            )
        };
        let mut params = vec![
            SqlValue::Int(tag_id),
            SqlValue::Text(cible.item_type().to_string()),
        ];
        params.extend(params_filtre);
        let refs: Vec<&dyn ToSqlValue> = params.iter().map(|v| v as &dyn ToSqlValue).collect();
        let modifies = self.db.execute(&sql, &refs)? as i64;
        Ok(Ok(BilanEtiquetage { designes, modifies }))
    }
}
