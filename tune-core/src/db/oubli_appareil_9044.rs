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
//!
//! ## Les zones masquées qu'on ne touche JAMAIS
//!
//! Un masquage n'est pas toujours un geste de l'utilisateur : `fusion`,
//! `zone_reflet`, `appareil_ignore`, `doublon_local_generique` sont des
//! garde-fous de la découverte (#3688, #4957, #5077). Supprimer leur ligne
//! rouvrirait le défaut qu'ils ferment (la zone reflet renaîtrait, l'appareil
//! ignoré reviendrait). Seules sont oubliables les zones visibles et les zones
//! masquées par l'utilisateur : `suppression_utilisateur`,
//! `suppression_totale`, ou sans motif (masquage d'avant la migration 112).
//!
//! ## Les zones supprimées qui ont lâché leur sortie (#8987)
//!
//! Une zone supprimée dont l'appareil a été repris par une autre zone garde
//! `output_device_id = NULL` et la clé `zone_{id}_sortie_liberee` = l'appareil.
//! Elle appartient à cet appareil : l'oublier la supprime aussi, clé comprise,
//! pour qu'un `POST /zones` ultérieur reparte à neuf au lieu de la ressusciter.

#[allow(unused_imports)]
use super::{DbBackend, Engine, SqlDialect, ZoneRepo};

/// Les motifs de masquage qu'un oubli peut emporter (voir l'en-tête).
pub const MOTIFS_OUBLIABLES: &[&str] = &["suppression_utilisateur", "suppression_totale"];

/// Une zone attachée à un appareil, telle que la base la connaît.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneDAppareil {
    pub id: i64,
    pub name: String,
    pub output_type: Option<String>,
    /// L'appareil de la zone — ou, pour une zone qui a lâché sa sortie
    /// (#8987), celui qu'elle tenait.
    pub output_device_id: String,
    /// La colonne `online` — tenue par la découverte et le relevé des
    /// sorties locales (#1626, #6551).
    pub online: bool,
    /// Zone masquée (supprimée par l'utilisateur, appareil ignoré…).
    pub hidden: bool,
    /// Le motif du masquage (`motif_masquage`), `None` : sans motif.
    pub motif: Option<String>,
    /// Zone supprimée dont la sortie a été reprise par une autre (#8987).
    pub sortie_liberee: bool,
}

impl ZoneDAppareil {
    /// Un oubli peut-il emporter cette zone ? Visible : oui (avec l'accord de
    /// l'utilisateur, à l'appelant de le demander) ; masquée : seulement par
    /// un geste de l'utilisateur.
    pub fn oubliable(&self) -> bool {
        !self.hidden
            || self
                .motif
                .as_deref()
                .is_none_or(|m| MOTIFS_OUBLIABLES.contains(&m))
    }
}

fn requete(avec_motif: bool) -> String {
    let motif = if avec_motif { "motif_masquage" } else { "NULL" };
    format!(
        "SELECT id, name, output_type, output_device_id, COALESCE(online, 0), \
         COALESCE(is_hidden, 0), {motif} FROM zones ORDER BY name, id"
    )
}

fn colonne_absente(e: &str) -> bool {
    e.contains("no such column") || e.contains("does not exist")
}

impl ZoneRepo {
    /// Toutes les zones attachées à un appareil, masquées comprises, et les
    /// zones supprimées qui ont lâché leur sortie (#8987), rattachées à
    /// l'appareil qu'elles tenaient.
    ///
    /// Base antérieure à `motif_masquage` : même liste, sans motif.
    pub fn zones_d_appareils(&self) -> Result<Vec<ZoneDAppareil>, String> {
        let lignes = match self.db.query_many_strong(&requete(true), &[]) {
            Ok(l) => l,
            Err(e) if colonne_absente(&e) => self.db.query_many_strong(&requete(false), &[])?,
            Err(e) => return Err(e),
        };
        Ok(lignes
            .iter()
            .filter_map(|l| {
                let id = l.first()?.as_i64()?;
                let hidden = l.get(5).and_then(|v| v.as_i64()).unwrap_or(0) != 0;
                let appareil = l
                    .get(3)
                    .and_then(|v| v.as_string())
                    .filter(|s| !s.is_empty());
                let (output_device_id, sortie_liberee) = match appareil {
                    Some(a) => (a, false),
                    // Sans sortie : seule une zone supprimée qui l'a lâchée
                    // (#8987) appartient encore à un appareil.
                    None if hidden => (self.sortie_liberee(id)?, true),
                    None => return None,
                };
                Some(ZoneDAppareil {
                    id,
                    name: l.get(1)?.as_string().unwrap_or_default(),
                    output_type: l.get(2).and_then(|v| v.as_string()),
                    output_device_id,
                    online: l.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
                    hidden,
                    motif: l
                        .get(6)
                        .and_then(|v| v.as_string())
                        .filter(|s| !s.is_empty()),
                    sortie_liberee,
                })
            })
            .collect())
    }

    /// Les zones attachées à `device_id`, masquées et libérées comprises.
    pub fn zones_de_l_appareil(&self, device_id: &str) -> Result<Vec<ZoneDAppareil>, String> {
        Ok(self
            .zones_d_appareils()?
            .into_iter()
            .filter(|z| z.output_device_id == device_id)
            .collect())
    }

    /// Supprime DÉFINITIVEMENT la zone `id` (la ligne, pas un masquage), la
    /// retire des groupes enregistrés et oublie sa sortie libérée (#8987).
    /// Rend le nombre de lignes supprimées.
    ///
    /// Le contrôle « en lecture ? » et « motif oubliable ? » appartient à
    /// l'appelant.
    pub fn supprimer_definitivement(&self, id: i64) -> Result<usize, String> {
        let sql = self.dialect_sql(
            |d| format!("DELETE FROM zones WHERE id = {}", d.placeholder(1)),
            |d| format!("DELETE FROM zones WHERE id = {}", d.placeholder(1)),
        );
        let params: [&dyn super::ToSqlValue; 1] = [&id];
        let n = self.db.execute(&sql, &params)?;
        if n > 0 {
            if let Err(e) =
                crate::zones::groupes_enregistres::retirer_des_groupes_enregistres(&self.db, &[id])
            {
                tracing::warn!(zone_id = id, error = %e, "zone_oubliee_groupes_non_reecrits");
            }
            if let Err(e) = self.oublier_la_sortie_liberee(id) {
                tracing::warn!(zone_id = id, error = %e, "zone_oubliee_sortie_liberee_restee");
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
#[path = "oubli_appareil_9044_tests.rs"]
mod tests;
