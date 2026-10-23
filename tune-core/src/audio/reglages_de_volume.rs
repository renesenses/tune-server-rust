//! #6774 — les réglages de volume d'une zone suivent l'APPAREIL, pas la ligne.
//!
//! `zone_{id}_*` est rangé sous le numéro de la ligne `zones`. Tout chemin qui
//! place un autre appareil sous une zone existante (choix de sortie, profil de
//! zones, rattachement automatique par nom ou par adresse), ou qui réécrit ces
//! clés sous d'autres numéros (restauration d'une sauvegarde), emmène donc le
//! trim, les limites et le verrou PURE avec la ligne. C'est ainsi qu'un trim de
//! +8,5 dB s'est retrouvé sur la zone d'un Marantz qui ne l'avait jamais reçu
//! (#6739).
//!
//! Ce module tient les deux règles communes :
//!
//! - [`ajuster_au_changement_d_appareil`] : la zone change d'appareil pour
//!   un AUTRE appareil — PURE verrouillé et volume fixe sont désarmés, le
//!   volume ne remonte jamais, un trim POSITIF repart de 0 dB (un trim
//!   négatif reste), les limites restent (elles ne font que borner) ;
//!   décisions de Bertrand du 22/10 ;
//! - [`neutraliser`] : la restauration n'a trouvé aucune correspondance sûre
//!   pour une zone — aucun réglage de volume n'y est posé, ceux qui y
//!   traînaient sont retirés.
//!
//! [`meme_appareil`] dit quand un changement d'identifiant n'est PAS un
//! changement d'appareil : identifiant inchangé, preuve d'identité locale, ou
//! même MAC normalisée des deux côtés. Rien d'autre — ni le nom, ni l'adresse.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Les réglages de VOLUME d'une zone, sous leur gabarit (`{id}` = numéro de
/// zone). Ce sont ceux qui peuvent faire monter le niveau d'un appareil :
///
/// - le trim de gain et sa marque « posé à la main » ;
/// - les limites de sécurité et de confort ;
/// - l'objet PURE (`enabled` + `lock_volume`) : verrouillé, il tient la
///   sortie à 100 % ;
/// - le volume mémorisé avant l'armement du volume fixe, reposé au
///   désarmement.
pub const GABARITS_DE_VOLUME: &[&str] = &[
    "zone_{id}_gain_trim_db",
    "zone_{id}_pose_gain_trim_db",
    "zone_{id}_volume_limit_safety",
    "zone_{id}_volume_limit_comfort",
    "zone_{id}_audiophile",
    "zone_{id}_volume_before_fixed",
];

/// Le trim et sa marque : ce que remet à zéro un changement d'appareil.
const GABARITS_DU_TRIM: &[&str] = &["zone_{id}_gain_trim_db", "zone_{id}_pose_gain_trim_db"];

/// Le gabarit est-il un réglage de volume ?
pub fn est_un_reglage_de_volume(gabarit: &str) -> bool {
    GABARITS_DE_VOLUME.contains(&gabarit)
}

fn cle(gabarit: &str, zone_id: i64) -> String {
    gabarit.replace("{id}", &zone_id.to_string())
}

/// Ce qui permet de dire que le nouvel identifiant désigne le MÊME appareil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rattachement<'a> {
    /// Aucune preuve : choix de l'utilisateur, profil de zones, rattachement
    /// par nom ou par adresse. La MAC du nouvel appareil, si on la connaît,
    /// peut encore prouver qu'il s'agit du même.
    SansPreuve {
        origine: &'static str,
        mac: Option<&'a str>,
    },
    /// Même périphérique local prouvé par son identifiant d'endpoint
    /// (`outputs::identite_de_sortie`, décision `Reassocie`).
    MemePeripherique { origine: &'static str },
}

impl Rattachement<'_> {
    pub fn origine(&self) -> &'static str {
        match self {
            Rattachement::SansPreuve { origine, .. }
            | Rattachement::MemePeripherique { origine } => origine,
        }
    }
}

/// Le nouvel identifiant désigne-t-il le même appareil que l'ancien ?
///
/// Fonction pure. Vrai seulement si l'identifiant ne change pas, si
/// l'identité locale est prouvée, ou si les DEUX MAC sont connues, valides
/// et égales une fois normalisées. Une MAC absente d'un côté ne prouve rien.
pub fn meme_appareil(
    ancien_device_id: Option<&str>,
    ancienne_mac: Option<&str>,
    nouveau_device_id: &str,
    rattachement: &Rattachement<'_>,
) -> bool {
    if ancien_device_id == Some(nouveau_device_id) {
        return true;
    }
    match rattachement {
        Rattachement::MemePeripherique { .. } => true,
        Rattachement::SansPreuve { mac, .. } => {
            let normaliser = |m: Option<&str>| m.and_then(crate::discovery::mac::normalize_mac);
            match (normaliser(ancienne_mac), normaliser(*mac)) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        }
    }
}

/// Ce qu'un changement d'appareil a fait aux réglages de volume d'une zone.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChangementDAppareil {
    /// Trim POSITIF retiré (il pouvait faire monter le nouvel appareil).
    pub trim_retire_db: Option<f64>,
    /// Trim NÉGATIF conservé (il ne fait que baisser).
    pub trim_garde_db: Option<f64>,
    /// PURE verrouillé (« Forcer à 100 % ») était armé et a été désarmé.
    pub pure_desarme: bool,
    /// Le volume fixe était armé et a été désarmé.
    pub volume_fixe_desarme: bool,
    /// Volume de la zone (en %) avant / après, quand un désarmement l'a
    /// ramené. `None` : rien n'a été désarmé, le volume n'est pas touché.
    pub volume_ramene: Option<(f64, f64)>,
}

impl ChangementDAppareil {
    /// Un mode qui tenait la sortie à 100 % a-t-il été désarmé ?
    pub fn a_desarme(&self) -> bool {
        self.pure_desarme || self.volume_fixe_desarme
    }
}

fn lire_le_trim(settings: &SettingsRepo, zone_id: i64) -> Option<f64> {
    settings
        .get(&cle(GABARITS_DU_TRIM[0], zone_id))
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|t| t.is_finite())
}

/// La zone vient de passer sur un AUTRE appareil (décisions de Bertrand du
/// 22/10, #6774). Le nouvel appareil ne doit JAMAIS être tenu plus haut du
/// seul fait de l'ancien :
///
/// 1. **PURE verrouillé et volume fixe sont désarmés.** Ils tiennent la
///    sortie à 100 % ; c'était le réglage de l'ancien appareil, l'utilisateur
///    les réarme lui-même sur le nouveau. Le volume de la zone repart alors
///    de la valeur la plus basse entre le volume courant, les limites de
///    confort et de sécurité et le volume mémorisé avant l'armement du volume
///    fixe — jamais une hausse. Sans aucun de ces repères, il repart de 30 %
///    du curseur au plus ([`plafond_sans_repere`], décision du 23/10). Journal
///    `pure_et_volume_fixe_desarmes_changement_d_appareil`. Rien n'est
///    commandé à l'appareil ici : seule la base change.
/// 2. **Le trim NÉGATIF reste** (il ne fait que baisser) ; seul un trim
///    POSITIF repart à 0 dB. Journal
///    `reglages_volume_reinitialises_changement_d_appareil`.
///
/// Les limites restent : elles ne font que borner.
pub fn ajuster_au_changement_d_appareil(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    ancien_device_id: Option<&str>,
    nouveau_device_id: &str,
    origine: &'static str,
) -> ChangementDAppareil {
    let settings = SettingsRepo::with_backend(db.clone());
    let mut rapport = ChangementDAppareil::default();

    // 2. Le trim : seul un trim positif est retiré.
    match lire_le_trim(&settings, zone_id) {
        Some(t) if t < 0.0 => rapport.trim_garde_db = Some(t),
        trim => {
            rapport.trim_retire_db = trim.filter(|t| *t > 0.0);
            for gabarit in GABARITS_DU_TRIM {
                if let Err(e) = settings.delete(&cle(gabarit, zone_id)) {
                    tracing::warn!(zone_id, cle = %cle(gabarit, zone_id), error = %e, "reglage_de_volume_non_retire");
                }
            }
        }
    }
    tracing::info!(
        zone_id,
        ancien = ?ancien_device_id,
        nouveau = nouveau_device_id,
        origine,
        trim_retire_db = ?rapport.trim_retire_db,
        trim_garde_db = ?rapport.trim_garde_db,
        "reglages_volume_reinitialises_changement_d_appareil"
    );

    // 1. PURE verrouillé : désarmé (mode ET « Forcer à 100 % »), les autres
    // champs de l'objet restent.
    if crate::audio::audiophile::zone_enabled(db, zone_id)
        && crate::audio::audiophile::volume_lock_enabled(db, zone_id)
    {
        let k = cle("zone_{id}_audiophile", zone_id);
        let mut objet = settings
            .get(&k)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        objet["enabled"] = serde_json::Value::Bool(false);
        objet["lock_volume"] = serde_json::Value::Bool(false);
        match settings.set(&k, &objet.to_string()) {
            Ok(()) => rapport.pure_desarme = true,
            Err(e) => tracing::warn!(zone_id, cle = %k, error = %e, "pure_non_desarme"),
        }
    }

    // 1 bis. Volume fixe : désarmé, et sa mémoire (celle de l'ancien
    // appareil) ne sera rendue par aucun désarmement ultérieur.
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(db.clone());
    let zone = repo.get(zone_id).ok().flatten();
    let memoire_fixe = crate::audio::fixed_volume::peek(db, zone_id);
    if zone.as_ref().is_some_and(|z| z.fixed_volume) {
        match repo.update_fixed_volume(zone_id, false) {
            Ok(()) => {
                rapport.volume_fixe_desarme = true;
                crate::audio::fixed_volume::forget(db, zone_id);
            }
            Err(e) => tracing::warn!(zone_id, error = %e, "volume_fixe_non_desarme"),
        }
    }

    if rapport.a_desarme()
        && let Some(zone) = zone
    {
        let limites = crate::audio::volume_limites::lire(db, zone_id);
        let avant = zone.volume;
        let mut apres = avant;
        let reperes: Vec<f64> = [
            limites.confort.map(|f| f * 100.0),
            limites.securite.map(|f| f * 100.0),
            memoire_fixe.filter(|_| rapport.volume_fixe_desarme),
        ]
        .into_iter()
        .flatten()
        .filter(|c| c.is_finite())
        .collect();
        // Décision de Bertrand du 23/10 : sans limite de confort, ni de
        // sécurité, ni mémoire du volume fixe, la zone repart de 30 % du
        // CURSEUR au plus (jamais relevée si elle était plus bas).
        let candidats = if reperes.is_empty() {
            vec![plafond_sans_repere(zone.output_type.as_deref(), nouveau_device_id) * 100.0]
        } else {
            reperes
        };
        for candidat in candidats {
            if candidat < apres {
                apres = candidat;
            }
        }
        let apres = apres.clamp(0.0, 100.0).min(avant.max(0.0));
        if apres < avant
            && let Err(e) = repo.update_volume(zone_id, apres)
        {
            tracing::warn!(zone_id, error = %e, "volume_non_ramene_changement_d_appareil");
            rapport.volume_ramene = Some((avant, avant));
        } else {
            rapport.volume_ramene = Some((avant, apres));
        }
        tracing::warn!(
            zone_id,
            ancien = ?ancien_device_id,
            nouveau = nouveau_device_id,
            origine,
            pure_desarme = rapport.pure_desarme,
            volume_fixe_desarme = rapport.volume_fixe_desarme,
            volume_avant = avant,
            volume_apres = ?rapport.volume_ramene.map(|(_, a)| a),
            "pure_et_volume_fixe_desarmes_changement_d_appareil"
        );
    }
    rapport
}

/// Décision de Bertrand du 23/10 (#6774) : position de CURSEUR (0..1) où
/// repart une zone dont PURE verrouillé ou le volume fixe vient d'être
/// désarmé au changement d'appareil, quand aucune limite ni mémoire du volume
/// fixe ne dit plus bas.
pub const CURSEUR_SANS_REPERE: f64 = 0.30;

/// Le gain (0..1) de [`CURSEUR_SANS_REPERE`] pour une zone qui passe de la
/// sortie `ancien_output_type` à l'appareil `nouveau_device_id`.
///
/// 30 % se lisent sur le curseur de la zone (#6787) : sur un appareil réseau,
/// 30 sur son échelle 0..100 ; sur une sortie logicielle (curseur `db60`),
/// −42 dB de gain — et non 30 % de gain (−10,5 dB), qui jouerait 31,5 dB plus
/// fort que le curseur affiché. Au moment du changement, `output_type` n'est
/// pas encore réécrit (les appelants écrivent l'appareil d'abord) : l'échelle
/// retenue est la plus BASSE des deux, celle de l'ancienne sortie ou celle que
/// l'identifiant du nouvel appareil désigne (`local:`, `browser`). Le plafond
/// ne dépasse donc jamais 30 % du curseur de la nouvelle sortie.
pub fn plafond_sans_repere(ancien_output_type: Option<&str>, nouveau_device_id: &str) -> f64 {
    use crate::audio::volume_scale::EchelleDeVolume;
    let nouvelle =
        if nouveau_device_id.starts_with("local:") || nouveau_device_id.starts_with("browser") {
            EchelleDeVolume::Perceptive
        } else {
            EchelleDeVolume::Lineaire
        };
    let ancienne = EchelleDeVolume::de_la_sortie(ancien_output_type);
    nouvelle
        .gain(CURSEUR_SANS_REPERE)
        .min(ancienne.gain(CURSEUR_SANS_REPERE))
}

/// Après un changement d'appareil, l'état de lecture EN MÉMOIRE ne doit pas
/// rester au-dessus de la base : le pas des boutons (`POST
/// /zones/{id}/volume/step`) part de lui, et un 100 % resté en mémoire après
/// le désarmement de PURE ou du volume fixe ferait repartir le nouvel
/// appareil de là. N'abaisse que : jamais une hausse.
pub async fn aligner_la_lecture(
    db: &Arc<dyn DbBackend>,
    playback: &crate::playback::PlaybackManager,
    zone_id: i64,
) {
    let Some(zone) = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get(zone_id)
        .ok()
        .flatten()
    else {
        return;
    };
    let base = (zone.volume / 100.0).clamp(0.0, 1.0);
    if base < playback.get_state(zone_id).await.volume {
        playback.set_volume(zone_id, base).await;
    }
}

/// Retire TOUS les réglages de volume d'une zone : elle revient au neutre
/// (trim 0 dB, aucune limite, PURE inactif). Rend les clés effectivement
/// retirées.
pub fn neutraliser(db: &Arc<dyn DbBackend>, zone_id: i64) -> Vec<String> {
    let settings = SettingsRepo::with_backend(db.clone());
    let mut retirees = Vec::new();
    for gabarit in GABARITS_DE_VOLUME {
        let k = cle(gabarit, zone_id);
        if settings.get(&k).ok().flatten().is_some() {
            match settings.delete(&k) {
                Ok(()) => retirees.push(k),
                Err(e) => {
                    tracing::warn!(zone_id, cle = %k, error = %e, "reglage_de_volume_non_retire")
                }
            }
        }
    }
    retirees
}

#[cfg(test)]
mod tests {
    use super::*;

    const SANS: Rattachement<'static> = Rattachement::SansPreuve {
        origine: "test",
        mac: None,
    };

    #[test]
    fn un_identifiant_inchange_est_le_meme_appareil() {
        assert!(meme_appareil(Some("uuid:a"), None, "uuid:a", &SANS));
    }

    #[test]
    fn un_autre_identifiant_sans_preuve_est_un_autre_appareil() {
        assert!(
            !meme_appareil(Some("uuid:a"), None, "uuid:b", &SANS),
            "sans MAC ni preuve locale, un autre identifiant est un AUTRE appareil (#6774)"
        );
        assert!(!meme_appareil(None, None, "uuid:b", &SANS));
    }

    #[test]
    fn la_meme_mac_normalisee_prouve_le_meme_appareil() {
        let r = Rattachement::SansPreuve {
            origine: "test",
            mac: Some("800A805C2689"),
        };
        assert!(meme_appareil(
            Some("uuid:a"),
            Some("80:0a:80:5c:26:89"),
            "uuid:b",
            &r
        ));
    }

    #[test]
    fn une_autre_mac_ou_une_mac_absente_ne_prouve_rien() {
        let autre = Rattachement::SansPreuve {
            origine: "test",
            mac: Some("80:0a:80:5c:26:90"),
        };
        assert!(!meme_appareil(
            Some("uuid:a"),
            Some("80:0a:80:5c:26:89"),
            "uuid:b",
            &autre
        ));
        let nouvelle_seule = Rattachement::SansPreuve {
            origine: "test",
            mac: Some("80:0a:80:5c:26:89"),
        };
        assert!(!meme_appareil(
            Some("uuid:a"),
            None,
            "uuid:b",
            &nouvelle_seule
        ));
        // Un identifiant opaque (TXT `id` d'un Chromecast) n'est pas une MAC.
        let opaque = Rattachement::SansPreuve {
            origine: "test",
            mac: Some("a8f3c1de9b7045"),
        };
        assert!(!meme_appareil(
            Some("uuid:a"),
            Some("a8f3c1de9b7045"),
            "uuid:b",
            &opaque
        ));
    }

    #[test]
    fn une_preuve_locale_d_endpoint_est_le_meme_appareil() {
        let r = Rattachement::MemePeripherique { origine: "test" };
        assert!(meme_appareil(Some("local:A"), None, "local:B", &r));
    }

    #[test]
    fn les_gabarits_de_volume_couvrent_trim_limites_et_pure() {
        for g in [
            "zone_{id}_gain_trim_db",
            "zone_{id}_volume_limit_safety",
            "zone_{id}_volume_limit_comfort",
            "zone_{id}_audiophile",
        ] {
            assert!(
                est_un_reglage_de_volume(g),
                "{g} doit être un réglage de volume"
            );
        }
        assert!(!est_un_reglage_de_volume("zone_{id}_crossfeed"));
        assert!(!est_un_reglage_de_volume("zone_{id}_eq_profile"));
    }
}
