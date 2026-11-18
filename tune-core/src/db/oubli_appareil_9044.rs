//! #9044 — OUBLIER un appareil qui n'est plus sur le réseau.
//!
//! Chaque appareil vu reçoit une zone ; quand il quitte le réseau, la zone
//! reste listée, grisée, à vie. Supprimer la zone (`ZoneRepo::delete`) la
//! MASQUE seulement : la ligne reste, et si l'appareil revient, la zone
//! masquée bloque sa recréation — il reste invisible au lieu de revenir.
//!
//! Oublier, c'est donc SUPPRIMER la ligne : l'appareil, s'il revient un jour,
//! revient comme neuf. Les identifiants de zone ne sont jamais réutilisés
//! (`AUTOINCREMENT` côté SQLite, `BIGSERIAL` côté PostgreSQL) : une ligne
//! d'une autre table qui garderait cet identifiant ne retombe sur aucune zone.

#[allow(unused_imports)]
use super::{DbBackend, Engine, SqlDialect, ZoneRepo};

/// Une zone attachée à un appareil, telle que la base la connaît.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneDAppareil {
    pub id: i64,
    pub name: String,
    pub output_type: Option<String>,
    pub output_device_id: String,
    /// La colonne `online` — tenue par la découverte et le relevé des
    /// sorties locales (#1626, #6551). Seule source pour une sortie locale.
    pub online: bool,
    /// Zone masquée (supprimée par l'utilisateur, appareil ignoré…).
    pub hidden: bool,
    /// Dernière réponse datée (ISO 8601 UTC), `None` : jamais vue depuis la
    /// pose de la colonne.
    pub last_seen_at: Option<String>,
    /// Âge, en secondes, de `last_seen_at`, calculé par la base.
    pub age_secs: Option<i64>,
}

fn requete(engine: Engine, avec_date: bool) -> String {
    let (date, age) = match (avec_date, engine) {
        (false, _) => ("NULL", "NULL"),
        (true, Engine::Sqlite) => (
            "last_seen_at",
            "CASE WHEN last_seen_at IS NULL THEN NULL ELSE \
             CAST(strftime('%s', 'now') AS INTEGER) - CAST(strftime('%s', last_seen_at) AS INTEGER) END",
        ),
        (true, Engine::Postgres) => (
            "last_seen_at",
            "CASE WHEN last_seen_at IS NULL THEN NULL ELSE \
             CAST(EXTRACT(EPOCH FROM (now() - last_seen_at::timestamptz)) AS BIGINT) END",
        ),
    };
    format!(
        "SELECT id, name, output_type, output_device_id, COALESCE(online, 0), \
         COALESCE(is_hidden, 0), {date}, {age} FROM zones \
         WHERE output_device_id IS NOT NULL AND output_device_id <> '' ORDER BY name, id"
    )
}

fn colonne_absente(e: &str) -> bool {
    e.contains("no such column") || e.contains("does not exist")
}

impl ZoneRepo {
    /// Toutes les zones attachées à un appareil, masquées comprises.
    ///
    /// Base antérieure à `last_seen_at` : même liste, sans date.
    pub fn zones_d_appareils(&self) -> Result<Vec<ZoneDAppareil>, String> {
        let engine = self.db.engine();
        let lignes = match self.db.query_many_strong(&requete(engine, true), &[]) {
            Ok(l) => l,
            Err(e) if colonne_absente(&e) => {
                self.db.query_many_strong(&requete(engine, false), &[])?
            }
            Err(e) => return Err(e),
        };
        Ok(lignes
            .iter()
            .filter_map(|l| {
                Some(ZoneDAppareil {
                    id: l.first()?.as_i64()?,
                    name: l.get(1)?.as_string().unwrap_or_default(),
                    output_type: l.get(2).and_then(|v| v.as_string()),
                    output_device_id: l.get(3)?.as_string()?,
                    online: l.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
                    hidden: l.get(5).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
                    last_seen_at: l.get(6).and_then(|v| v.as_string()),
                    age_secs: l.get(7).and_then(|v| v.as_i64()),
                })
            })
            .collect())
    }

    /// Les zones attachées à `device_id`, masquées comprises.
    pub fn zones_de_l_appareil(&self, device_id: &str) -> Result<Vec<ZoneDAppareil>, String> {
        Ok(self
            .zones_d_appareils()?
            .into_iter()
            .filter(|z| z.output_device_id == device_id)
            .collect())
    }

    /// Supprime DÉFINITIVEMENT la zone `id` (la ligne, pas un masquage) et la
    /// retire des groupes enregistrés. Rend le nombre de lignes supprimées.
    ///
    /// Le contrôle « en lecture ? » appartient à l'appelant, qui seul voit
    /// l'état de lecture.
    pub fn supprimer_definitivement(&self, id: i64) -> Result<usize, String> {
        let sql = self.dialect_sql(
            |d| format!("DELETE FROM zones WHERE id = {}", d.placeholder(1)),
            |d| format!("DELETE FROM zones WHERE id = {}", d.placeholder(1)),
        );
        let params: [&dyn super::ToSqlValue; 1] = [&id];
        let n = self.db.execute(&sql, &params)?;
        if n > 0
            && let Err(e) =
                crate::zones::groupes_enregistres::retirer_des_groupes_enregistres(&self.db, &[id])
        {
            tracing::warn!(zone_id = id, error = %e, "zone_oubliee_groupes_non_reecrits");
        }
        Ok(n)
    }
}

#[cfg(test)]
#[path = "oubli_appareil_9044_tests.rs"]
mod tests;
