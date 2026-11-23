//! Forum (relevé rc14 du 21/11/2026) — deux zones pour un même appareil.
//!
//! Une zone pointe une sortie disparue ; la seule sortie vivante du même nom
//! est déjà celle d'une AUTRE zone. Le rebond de #1287 n'a pas lieu (#5464 :
//! deux zones ne partagent pas une sortie) et le serveur écrit une phrase qui
//! nomme l'autre zone.
//!
//! Cette phrase n'atteignait pas l'écran : le refus partait sous le code de
//! famille `zone_output_unavailable` SANS motif, et le client web remplace ce
//! code sans motif par son bandeau générique (« l'appareil de cette zone est
//! introuvable, il se reconnectera tout seul »), faux ici — l'appareil est
//! présent, et il ne se reconnectera jamais à cette zone tant que l'autre le
//! tient. Le journal disait `zone_rebind_target_held_by_other_zone`,
//! l'auditeur ne lisait rien d'utile.
//!
//! Le refus porte désormais son motif, [`MOTIF`], et l'identifiant de la zone
//! qui tient la sortie : un client qui voit un motif affiche la phrase du
//! serveur. Aucune fusion automatique des deux zones : le message dit quoi
//! faire, l'auditeur décide.
//!
//! Forme de la sentinelle :
//! `zone_output_unavailable:output_held_by_other_zone:<id de la zone>:<json>`,
//! où le JSON porte la forme du refus ([`Forme`]), les noms à interpoler et la
//! phrase française du journal. Elle commence toujours par
//! `zone_output_unavailable:` : ce qui reconnaît la famille de refus la
//! reconnaît encore.
//!
//! Rc14 — la phrase française n'est plus ce que lit l'écran : le serveur la
//! recompose dans la langue de la requête (`message_key` et `params`, clés
//! `api.zones.outputHeld…` de `i18n_server.json`), et l'événement
//! `zone.playback_error` porte les mêmes `params` pour que le client la
//! traduise lui-même.

/// Le motif stable (champ `reason` du 409 et de `zone.playback_error`).
pub const MOTIF: &str = "output_held_by_other_zone";

const PREFIXE: &str = "zone_output_unavailable:output_held_by_other_zone:";

/// Les trois formes du refus : le geste utile n'est pas le même.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Forme {
    /// L'autre zone a un autre nom : on la nomme, c'est là qu'il faut lancer.
    Other,
    /// L'autre zone porte le MÊME nom : on dit « une autre zone nommée elle aussi ».
    SameName,
    /// L'autre zone est supprimée (masquée) : on ne propose pas d'y lancer.
    Deleted,
}

impl Forme {
    /// Le code stable de la forme (`params.variant`).
    pub fn code(self) -> &'static str {
        match self {
            Forme::Other => "other",
            Forme::SameName => "same_name",
            Forme::Deleted => "deleted",
        }
    }

    /// La clé de `i18n_server.json` de la phrase de cette forme.
    pub fn cle_de_message(self) -> &'static str {
        match self {
            Forme::Other => "api.zones.outputHeldByOtherZone",
            Forme::SameName => "api.zones.outputHeldBySameNameZone",
            Forme::Deleted => "api.zones.outputHeldByDeletedZone",
        }
    }
}

/// Ce que porte la sentinelle : de quoi recomposer la phrase dans toute langue.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SortieTenue {
    /// L'identifiant de la zone qui tient la sortie (hors JSON : dans le préfixe).
    #[serde(skip)]
    pub zone_qui_tient: i64,
    /// La forme du refus.
    pub variant: Forme,
    /// Le nom de la zone refusée.
    pub zone: String,
    /// Le nom lisible de l'appareil de la sortie vivante.
    pub device: String,
    /// Le nom de la zone qui tient la sortie.
    pub holder: String,
    /// La phrase française (journal, repli).
    pub phrase: String,
}

impl SortieTenue {
    /// Les valeurs interpolées (`params` du 409 et de `zone.playback_error`).
    pub fn params(&self) -> Vec<(&'static str, String)> {
        vec![
            ("zone", self.zone.clone()),
            ("device", self.device.clone()),
            ("holder", self.holder.clone()),
            ("variant", self.variant.code().to_string()),
        ]
    }

    /// Les mêmes valeurs, en objet JSON.
    pub fn params_json(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.params()
                .into_iter()
                .map(|(k, v)| (k.to_string(), serde_json::Value::String(v)))
                .collect(),
        )
    }
}

/// La sentinelle du refus.
pub fn sentinelle(refus: &SortieTenue) -> String {
    let json = serde_json::to_string(refus).unwrap_or_default();
    format!("{PREFIXE}{}:{json}", refus.zone_qui_tient)
}

/// `Some(refus)` si `message` est ce refus.
pub fn depuis_sentinelle(message: &str) -> Option<SortieTenue> {
    let reste = message.strip_prefix(PREFIXE)?;
    let (id, json) = reste.split_once(':')?;
    let mut refus: SortieTenue = serde_json::from_str(json).ok()?;
    refus.zone_qui_tient = id.parse().ok()?;
    Some(refus)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refus(phrase: &str) -> SortieTenue {
        SortieTenue {
            zone_qui_tient: 12,
            variant: Forme::Other,
            zone: "Salon : DLNA".into(),
            device: "NanoPlayer".into(),
            holder: "Salon : NanoPlayer".into(),
            phrase: phrase.into(),
        }
    }

    #[test]
    fn la_sentinelle_se_relit_avec_des_noms_qui_contiennent_des_deux_points() {
        let r = refus("La zone « Salon : NanoPlayer » l'utilise déjà.");
        let s = sentinelle(&r);
        assert!(s.starts_with("zone_output_unavailable:"));
        assert_eq!(depuis_sentinelle(&s), Some(r));
    }

    #[test]
    fn les_params_nomment_la_zone_qui_tient_la_sortie_et_la_forme() {
        let p = refus("x").params_json();
        assert_eq!(p["holder"], "Salon : NanoPlayer");
        assert_eq!(p["device"], "NanoPlayer");
        assert_eq!(p["variant"], "other");
    }

    #[test]
    fn un_refus_ordinaire_n_est_pas_ce_refus() {
        assert_eq!(
            depuis_sentinelle("zone_output_unavailable:La sortie a disparu."),
            None
        );
    }
}
