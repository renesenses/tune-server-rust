//! #8920 — les boutons −1 dB / +1 dB d'une zone Devialet AIR.
//!
//! Un testeur Devialet Expert : « il manque des boutons pour avancer par pas
//! de 1 dB, y compris entre confort et sécurité : sans ça on ne dépasse
//! jamais confort ». Le pas part du niveau que l'AMPLI rapporte dans son
//! annonce (la télécommande le change sans aucun message réseau : c'est lui
//! qui fait foi), sinon de la consigne de la zone.
//!
//! Il suit ensuite le chemin de tout bouton + / − (#6541,
//! [`PlaybackOrchestrator::set_volume_au_pas_avec_garde`]) : le confort ne le
//! retient pas, la SÉCURITÉ le borne toujours, et la sortie ne laisse rien
//! partir au-dessus de son plafond de −10 dB. Deux règles en plus :
//!
//! - un + ne fait JAMAIS baisser le son : ampli déjà au-dessus de la sécurité
//!   (réglé à la télécommande), le + ne commande rien ;
//! - un − ne fait jamais monter le son.

use std::time::Duration;

use super::PlaybackOrchestrator;
use crate::audio::garde_de_hausse::Hausse;
use crate::audio::volume_limites;
use crate::outputs::devialet_air::DevialetAirOutput;
use crate::outputs::devialet_air::reglages::BornesDb;
use crate::outputs::traits::OutputCommandResult;

/// Ce qu'un pas a donné.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PasAir {
    /// La consigne visée (fraction de la course), avant toute limite.
    pub cible: f64,
    /// Le niveau d'ampli d'où part le pas, en dB.
    pub depart_db: f64,
    /// La consigne retenue (fraction), ou celle d'avant si rien n'est parti.
    pub retenu: f64,
    /// Une commande est-elle partie ?
    pub envoye: bool,
}

/// La consigne d'un pas de `pas_db` (> 0) depuis `depart_db`, sur `course`,
/// bornée par la `securite` (fraction) de la zone. `None` : le pas ne
/// changerait rien dans le bon sens (un + qui baisserait, un − qui
/// monterait, ou une butée) — rien ne doit partir.
#[must_use]
pub fn consigne_du_pas(
    course: BornesDb,
    depart_db: f64,
    monter: bool,
    pas_db: f64,
    securite: Option<f64>,
) -> Option<(f64, f64)> {
    if !depart_db.is_finite() || !pas_db.is_finite() || pas_db <= 0.0 {
        return None;
    }
    let cible_db = if monter {
        depart_db + pas_db
    } else {
        depart_db - pas_db
    };
    let cible = course.db_vers_volume(cible_db);
    let bornee = securite.map_or(cible, |s| cible.min(s));
    let envoye_db = course.volume_vers_db(bornee);
    let dans_le_bon_sens = if monter {
        envoye_db > depart_db
    } else {
        envoye_db < depart_db
    };
    dans_le_bon_sens.then_some((cible, bornee))
}

impl PlaybackOrchestrator {
    /// Le niveau que l'ampli Devialet AIR de `device_id` rapporte, en dB,
    /// s'il s'annonce encore. Une seconde au plus pour prendre la sortie.
    pub(crate) async fn volume_rapporte_air(&self, device_id: Option<&str>) -> Option<f64> {
        let arc = { self.outputs.lock().await.get(device_id?) }?;
        let sortie = tokio::time::timeout(Duration::from_secs(1), arc.lock())
            .await
            .ok()?;
        sortie
            .as_any()
            .downcast_ref::<DevialetAirOutput>()?
            .observation_volume()
            .map(|o| o.volume_db)
    }

    /// #8920 — un pas de `pas_db` dB d'ampli, vers le haut ou le bas, sur
    /// une zone Devialet AIR. Voir le module.
    pub async fn pas_de_volume_air(
        &self,
        zone_id: i64,
        device_id: Option<&str>,
        monter: bool,
        pas_db: f64,
    ) -> OutputCommandResult<PasAir> {
        let course = BornesDb::lues(&self.db);
        let actuel = self.playback.get_state(zone_id).await.volume;
        let rapporte = self.volume_rapporte_air(device_id).await;
        let depart_db = rapporte.unwrap_or_else(|| course.volume_vers_db(actuel));
        let securite = volume_limites::lire_effectives(&self.db, zone_id).securite;
        let Some((cible, bornee)) = consigne_du_pas(course, depart_db, monter, pas_db, securite)
        else {
            tracing::info!(
                zone_id,
                depart_db,
                monter,
                ?securite,
                "devialet_air_pas_sans_effet_8920"
            );
            return Ok(PasAir {
                cible: course.db_vers_volume(if monter {
                    depart_db + pas_db
                } else {
                    depart_db - pas_db
                }),
                depart_db,
                retenu: actuel,
                envoye: false,
            });
        };
        // Un pas d'un dB depuis ce que l'ampli JOUE n'est pas une montée
        // brusque, même si la mémoire de la zone est restée plus bas (hausse
        // faite à la télécommande, que #6912 n'adopte pas) : la garde #6721
        // mesurerait sinon le saut depuis la mémoire et retiendrait le pas.
        // Sans relevé de l'ampli, la garde reste entière.
        let hausse = if rapporte.is_some() {
            Hausse::Confirmee
        } else {
            Hausse::Gardee
        };
        let retenu = self
            .set_volume_au_pas_avec_garde(zone_id, bornee, device_id, hausse)
            .await?;
        Ok(PasAir {
            cible,
            depart_db,
            retenu: retenu.retenu,
            envoye: true,
        })
    }
}
