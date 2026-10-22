//! #6541 — les deux seuils de volume d'une zone, et le pas des boutons + / −.
//!
//! Fil forum 2335 (alec_eiffel) : « une limite de SÉCURITÉ que Tune ne
//! dépasse jamais (70 % ou −10 dB), une limite de CONFORT (30 % ou −25 dB) :
//! sous elle, curseur et boutons agissent ; entre confort et sécurité, seuls
//! les boutons montent ».
//!
//! ## Où elles s'appliquent
//!
//! Côté SERVEUR, au point de passage unique de toute consigne de volume
//! (`PlaybackOrchestrator::set_volume`, et les chemins qui commandent une
//! sortie sans lui) : l'interface web, l'appli Android, le pont LMS, le
//! renderer UPnP de Tune, les alarmes, la minuterie et les clients tiers
//! passent tous par là. Une limite tenue par le seul client web ne protège
//! rien.
//!
//! * La **sécurité** borne tout : la consigne, la valeur réellement envoyée à
//!   l'appareil (trim de gain compris), la graine d'une sortie locale, et le
//!   volume qu'un renderer remonte de lui-même (le sondeur le ramène sous la
//!   limite au lieu de l'adopter).
//! * Le **confort** ne borne que les consignes ABSOLUES (curseur, `PUT
//!   /zones/{id}/volume`, PATCH…) qui MONTENT : au-dessus de lui, seul le pas
//!   (`POST /zones/{id}/volume/step`, les boutons) fait monter. Une consigne
//!   absolue qui descend passe toujours.
//!
//! ## Zones PURE et à volume fixe (décision de Bertrand, 21/10)
//!
//! Une limite ne se POSE pas sur une zone en mode PURE (verrouillé ou non
//! en bit-perfect) ni sur une zone à volume fixe : la route la refuse avec un
//! code stable ([`ModeSansLimite::code`]). Une limite déjà enregistrée sur une
//! zone qui passe ENSUITE dans l'un de ces modes est IGNORÉE tant que la zone
//! y reste ([`lire_effectives`]) : elle n'abaisse rien, ne commande rien, et
//! le 100 % de PURE verrouillé ou du volume fixe part tel qu'avant #6541. Elle
//! reprend effet quand la zone quitte le mode, pour les gestes suivants : elle
//! ne fait jamais MONTER le niveau.
//!
//! ## Stockage
//!
//! Deux réglages clé/valeur, en FRACTION linéaire 0..1 (l'échelle de
//! `zones.volume / 100`) : `zone_{id}_volume_limit_safety` et
//! `zone_{id}_volume_limit_comfort`. Absents par défaut : rien ne change
//! tant que personne ne règle. Le client peut parler en % ou en dB ; la
//! conversion passe par [`super::volume_scale`], le seul endroit qui la fait.
//!
//! Ce module ne fait que de l'arithmétique et de la lecture de réglages : il
//! ne commande aucun appareil.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Pas des boutons + / − quand la requête n'en donne pas : 2 points de
/// l'échelle linéaire (0,02), le pas de la molette du client web.
pub const PAS_PAR_DEFAUT: f64 = 0.02;

/// Tolérance sous laquelle un volume rapporté par un renderer est tenu pour
/// égal à la limite : la plupart parlent en entiers 0..100.
pub const TOLERANCE_RENDERER: f64 = 0.01;

/// Plancher d'une limite : en dessous de 1 %, une « limite » couperait le son
/// sans le dire. Une valeur plus basse est refusée par la route.
pub const LIMITE_MINIMALE: f64 = 0.01;

pub fn cle_securite(zone_id: i64) -> String {
    format!("zone_{zone_id}_volume_limit_safety")
}

pub fn cle_confort(zone_id: i64) -> String {
    format!("zone_{zone_id}_volume_limit_comfort")
}

/// D'où vient une consigne : la règle du confort n'est pas la même.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrigineDuVolume {
    /// Curseur, valeur posée par une API, alarme, PATCH… : borné par le
    /// confort ET la sécurité.
    Absolue,
    /// Bouton + / − : borné par la seule sécurité.
    Pas,
}

/// Les modes d'une zone où une limite n'a pas cours : refusée à la pose,
/// ignorée si elle existait déjà.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSansLimite {
    /// Mode PURE (`zone_{id}_audiophile.enabled`), verrou bit-perfect ou non.
    ZonePure,
    /// `zones.fixed_volume` : le DAC ou l'ampli tient le volume.
    VolumeFixe,
}

impl ModeSansLimite {
    /// Code stable de l'erreur rendue par la route, et publié dans la zone
    /// (`volume_limits_refusal`) : le client s'en sert, pas du message.
    pub fn code(self) -> &'static str {
        match self {
            Self::ZonePure => "limite_refusee_zone_pure",
            Self::VolumeFixe => "limite_refusee_volume_fixe",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::ZonePure => {
                "Cette zone est en mode PURE : Tune n'y touche pas au volume, une limite de sécurité ou de confort n'y a pas cours. Quittez le mode PURE pour en poser une."
            }
            Self::VolumeFixe => {
                "Cette zone est à volume fixe : le volume est tenu par l'ampli ou le DAC, une limite de sécurité ou de confort n'y a pas cours. Désactivez le volume fixe pour en poser une."
            }
        }
    }
}

/// Le mode qui suspend les limites de `zone_id`, s'il y en a un. Le volume
/// fixe passe avant PURE (il court-circuite déjà tout `set_volume`).
pub fn mode_sans_limite(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<ModeSansLimite> {
    let fixe = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .is_some_and(|z| z.fixed_volume);
    if fixe {
        Some(ModeSansLimite::VolumeFixe)
    } else if crate::audio::audiophile::zone_enabled(db, zone_id) {
        Some(ModeSansLimite::ZonePure)
    } else {
        None
    }
}

/// Les deux seuils d'une zone, en fraction linéaire 0..1. `None` = pas de
/// limite.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LimitesDeVolume {
    pub securite: Option<f64>,
    pub confort: Option<f64>,
}

fn lire_une(settings: &SettingsRepo, cle: &str) -> Option<f64> {
    settings
        .get(cle)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<f64>().ok())
        // Une valeur hors échelle en base ne vaut pas limite : on ne
        // l'interprète pas (ni 0, qui couperait, ni > 1, qui ne borne rien).
        .filter(|v| v.is_finite() && (LIMITE_MINIMALE..=1.0).contains(v))
}

/// Les limites enregistrées pour `zone_id`.
pub fn lire(db: &Arc<dyn DbBackend>, zone_id: i64) -> LimitesDeVolume {
    let settings = SettingsRepo::with_backend(db.clone());
    LimitesDeVolume {
        securite: lire_une(&settings, &cle_securite(zone_id)),
        confort: lire_une(&settings, &cle_confort(zone_id)),
    }
}

/// Les limites qui AGISSENT sur `zone_id` : aucune tant que la zone est en
/// PURE ou à volume fixe ([`mode_sans_limite`]), celles enregistrées sinon.
/// Tout chemin qui commande un appareil lit celle-ci, jamais [`lire`].
pub fn lire_effectives(db: &Arc<dyn DbBackend>, zone_id: i64) -> LimitesDeVolume {
    if mode_sans_limite(db, zone_id).is_some() {
        LimitesDeVolume::default()
    } else {
        lire(db, zone_id)
    }
}

/// La limite de sécurité la plus basse parmi les zones qui pilotent
/// `device_id` — pour les routes qui commandent une SORTIE sans passer par
/// une zone (pilotage Sonos direct, agent Tune distant).
pub fn securite_de_l_appareil(db: &Arc<dyn DbBackend>, device_id: &str) -> Option<f64> {
    crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .list()
        .ok()?
        .into_iter()
        .filter(|z| z.output_device_id.as_deref() == Some(device_id))
        .filter_map(|z| lire_effectives(db, z.id?).securite)
        .reduce(f64::min)
}

impl LimitesDeVolume {
    pub fn est_vide(&self) -> bool {
        self.securite.is_none() && self.confort.is_none()
    }

    /// Le confort tel qu'il agit : jamais au-dessus de la sécurité.
    pub fn confort_effectif(&self) -> Option<f64> {
        match (self.confort, self.securite) {
            (Some(c), Some(s)) => Some(c.min(s)),
            (c, _) => c,
        }
    }

    /// Borne la valeur ENVOYÉE à un appareil (trim compris) : seule la
    /// sécurité compte, et elle compte toujours.
    pub fn borner_appareil(&self, valeur: f64) -> f64 {
        match self.securite {
            Some(s) => valeur.min(s),
            None => valeur,
        }
    }

    /// La consigne retenue pour une demande `cible`, la zone étant à
    /// `actuel` (fractions 0..1).
    ///
    /// Le confort ne bloque qu'une MONTÉE absolue au-delà de
    /// `max(confort, actuel)` : une zone montée aux boutons à 50 % au-dessus
    /// d'un confort de 30 % peut redescendre au curseur à 40 %, mais le
    /// curseur ne la fait pas monter à 55 %.
    pub fn borner(&self, cible: f64, actuel: f64, origine: OrigineDuVolume) -> f64 {
        let cible = cible.clamp(0.0, 1.0);
        let cible = match (origine, self.confort_effectif()) {
            (OrigineDuVolume::Absolue, Some(confort)) => cible.min(confort.max(actuel)),
            _ => cible,
        };
        self.borner_appareil(cible)
    }

    /// Le renderer rapporte-t-il un volume au-dessus de la sécurité ?
    pub fn depasse(&self, rapporte: f64) -> bool {
        self.securite
            .is_some_and(|s| rapporte > s + TOLERANCE_RENDERER)
    }
}

/// La cible d'un pas `delta` (fraction, signée) depuis `actuel`.
pub fn cible_du_pas(actuel: f64, delta: f64) -> f64 {
    (actuel + delta).clamp(0.0, 1.0)
}

/// La cible d'un pas de `delta_db` décibels depuis `actuel`. Depuis le
/// silence, un pas en dB ne monte jamais : il part alors du premier pas
/// linéaire ([`PAS_PAR_DEFAUT`]).
pub fn cible_du_pas_db(actuel: f64, delta_db: f64) -> f64 {
    match super::volume_scale::linear_to_db(actuel) {
        None if delta_db > 0.0 => PAS_PAR_DEFAUT,
        None => 0.0,
        Some(db) => super::volume_scale::db_to_linear((db + delta_db).min(0.0))
            .unwrap_or(actuel)
            .clamp(0.0, 1.0),
    }
}

/// Publie les limites ENREGISTRÉES de `zone_id` dans un JSON de zone, en
/// fraction et en dB (même convention que `volume` / `volume_db`), et
/// `volume_limits_refusal` : le code de [`ModeSansLimite`] quand la zone est
/// en PURE ou à volume fixe (les limites y sont alors ignorées, et le client
/// masque le réglage), `null` sinon. Les clés sont TOUJOURS présentes : leur
/// présence dit au client que ce serveur connaît le réglage.
pub fn publier(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
) {
    inserer(obj, &lire(db, zone_id));
    obj.insert(
        "volume_limits_refusal".into(),
        serde_json::json!(mode_sans_limite(db, zone_id).map(ModeSansLimite::code)),
    );
}

fn inserer(obj: &mut serde_json::Map<String, serde_json::Value>, limites: &LimitesDeVolume) {
    use super::volume_scale::linear_to_db;
    obj.insert(
        "volume_limit_safety".into(),
        serde_json::json!(limites.securite),
    );
    obj.insert(
        "volume_limit_safety_db".into(),
        serde_json::json!(limites.securite.and_then(linear_to_db)),
    );
    obj.insert(
        "volume_limit_comfort".into(),
        serde_json::json!(limites.confort),
    );
    obj.insert(
        "volume_limit_comfort_db".into(),
        serde_json::json!(limites.confort.and_then(linear_to_db)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECU: LimitesDeVolume = LimitesDeVolume {
        securite: Some(0.7),
        confort: Some(0.3),
    };

    #[test]
    fn la_securite_borne_toutes_les_origines() {
        let seule = LimitesDeVolume {
            securite: Some(0.7),
            confort: None,
        };
        for origine in [OrigineDuVolume::Absolue, OrigineDuVolume::Pas] {
            assert_eq!(seule.borner(1.0, 0.69, origine), 0.7, "{origine:?}");
            // Une zone déjà au-dessus (base héritée) redescend à la limite.
            assert_eq!(SECU.borner(0.95, 0.9, origine), 0.7, "{origine:?}");
        }
        assert_eq!(SECU.borner_appareil(0.95), 0.7);
    }

    #[test]
    fn le_curseur_ne_monte_pas_au_dessus_du_confort() {
        assert_eq!(SECU.borner(0.6, 0.2, OrigineDuVolume::Absolue), 0.3);
        // …mais les boutons, si.
        assert_eq!(SECU.borner(0.6, 0.2, OrigineDuVolume::Pas), 0.6);
    }

    #[test]
    fn au_dessus_du_confort_le_curseur_descend_mais_ne_monte_pas() {
        assert_eq!(SECU.borner(0.4, 0.5, OrigineDuVolume::Absolue), 0.4);
        assert_eq!(SECU.borner(0.55, 0.5, OrigineDuVolume::Absolue), 0.5);
    }

    #[test]
    fn un_confort_au_dessus_de_la_securite_ne_la_desserre_pas() {
        let l = LimitesDeVolume {
            securite: Some(0.4),
            confort: Some(0.9),
        };
        assert_eq!(l.borner(0.8, 0.1, OrigineDuVolume::Absolue), 0.4);
    }

    #[test]
    fn sans_limite_rien_ne_change() {
        let l = LimitesDeVolume::default();
        assert!(l.est_vide());
        assert_eq!(l.borner(0.93, 0.1, OrigineDuVolume::Absolue), 0.93);
        assert!(!l.depasse(1.0));
    }

    #[test]
    fn le_pas_en_db() {
        // −6 dB + 6 dB = 0 dB.
        assert!((cible_du_pas_db(0.5011872336, 6.0) - 1.0).abs() < 1e-6);
        assert_eq!(cible_du_pas_db(0.0, 1.0), PAS_PAR_DEFAUT);
        assert_eq!(cible_du_pas_db(0.0, -1.0), 0.0);
        assert_eq!(cible_du_pas(0.99, 0.02), 1.0);
        assert_eq!(cible_du_pas(0.01, -0.02), 0.0);
    }

    #[test]
    fn la_tolerance_des_renderers_entiers() {
        assert!(!SECU.depasse(0.705));
        assert!(SECU.depasse(0.72));
    }
}
