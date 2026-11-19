//! #9089 (Jon, fil 2757) — le décalage manuel d'une zone qui SUIT la meneuse
//! de son groupe.
//!
//! Un groupe de récepteurs réseau (DLNA, OAAT distant, AirPlay, Chromecast)
//! est un groupe de COMMANDES (`independent_renderers`) : la suiveuse rejoint
//! la meneuse au `started` de celle-ci, à sa position, et chaque appareil
//! part au mieux. Rien ne compense l'écart de latence entre deux familles de
//! récepteurs (le testeur entend ses appareils DLNA en avance sur son OAAT).
//! #8437 ne retarde que les sorties LOCALES (`sync_delay_ms`).
//!
//! En attendant une vraie synchronisation, chaque zone porte un décalage
//! signé, en ms, borné à [`DECALAGE_MAX_MS`], réglé à l'oreille. Il ne vaut
//! que lorsque la zone suit une meneuse (le réglage de la meneuse elle-même
//! n'a pas d'effet tant qu'elle mène) :
//!
//! - **positif** : la suiveuse joue plus TARD. Son départ attend le reste du
//!   décalage, puis elle rejoint la meneuse `d` ms en arrière dans la piste ;
//!   pause et reprise lui parviennent `d` ms après la meneuse ;
//! - **négatif** : la suiveuse joue plus TÔT. Elle rejoint la meneuse `|d|` ms
//!   plus loin dans la piste (avance du flux). Un départ de piste sous 3 s
//!   repart du début (même seuil que le transfert, #4442) : l'avance ne vaut
//!   alors qu'au prochain déplacement ou à la prochaine jonction en cours de
//!   piste. Pour décaler un départ de piste, mieux vaut retarder (valeur
//!   positive) la zone qui joue en premier.
//!
//! Les déplacements dans la piste donnés sur la meneuse sont relayés à la
//! suiveuse avec le même décalage.
//!
//! Autres limites connues :
//! - un groupe formé quand la meneuse est entre `d` et `d + 3 s` : la position
//!   `p − d` passe sous le seuil de 3 s du transfert, la suiveuse repart de
//!   0:00 et l'écart vaut `p`, pas `d` ;
//! - une pause de la meneuse plus courte que `d` n'est pas relayée : au
//!   réveil la meneuse rejoue, rien n'est à faire, et la suiveuse prend
//!   l'avance de la durée de la pause ;
//! - sans durée connue (flux), un décalage négatif n'avance rien.
//!
//! Rangé dans le réglage `zone_{id}_decalage_de_groupe_ms` : pas de colonne,
//! donc ni migration SQLite ni PostgreSQL.

use crate::db::settings_repo::SettingsRepo;

/// La borne du réglage, dans les deux sens.
pub const DECALAGE_MAX_MS: i64 = 2_000;

/// La clé du réglage de la zone.
pub fn cle(zone_id: i64) -> String {
    format!("zone_{zone_id}_decalage_de_groupe_ms")
}

/// Le décalage enregistré de la zone (0 sans réglage, borné à la lecture).
pub fn lire(settings: &SettingsRepo, zone_id: i64) -> i64 {
    settings
        .get(&cle(zone_id))
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(-DECALAGE_MAX_MS, DECALAGE_MAX_MS)
}

/// Pourquoi un décalage est refusé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusDuDecalage {
    HorsBornes(i64),
    Ecriture(String),
}

/// Enregistre le décalage de la zone. Hors de `±DECALAGE_MAX_MS`, refusé (rien
/// n'est écrit) ; `0` efface le réglage.
pub fn ecrire(
    settings: &SettingsRepo,
    zone_id: i64,
    decalage_ms: i64,
) -> Result<i64, RefusDuDecalage> {
    if !(-DECALAGE_MAX_MS..=DECALAGE_MAX_MS).contains(&decalage_ms) {
        return Err(RefusDuDecalage::HorsBornes(decalage_ms));
    }
    let resultat = if decalage_ms == 0 {
        settings.delete(&cle(zone_id))
    } else {
        settings.set(&cle(zone_id), &decalage_ms.to_string())
    };
    resultat
        .map(|()| decalage_ms)
        .map_err(RefusDuDecalage::Ecriture)
}

/// La position de la suiveuse quand la meneuse est à `position_meneuse_ms` :
/// `d` ms en arrière (positif) ou en avant (négatif), jamais avant le début,
/// et jamais au-delà de la fin quand la durée est connue (`duree_ms > 0`).
/// Sans durée connue (flux), aucune avance : la fin ne peut pas être bornée.
pub fn position_de_la_suiveuse(position_meneuse_ms: u64, decalage_ms: i64, duree_ms: i64) -> u64 {
    if decalage_ms < 0 && duree_ms <= 0 {
        return position_meneuse_ms;
    }
    let decalee = (position_meneuse_ms as i64)
        .saturating_sub(decalage_ms)
        .max(0);
    let decalee = if duree_ms > 0 {
        decalee.min((duree_ms - 1).max(0))
    } else {
        decalee
    };
    decalee as u64
}

/// L'attente avant que la suiveuse ne REJOIGNE une meneuse arrivée à
/// `position_meneuse_ms` : la part du décalage positif que la meneuse n'a pas
/// encore jouée. Nulle pour un décalage négatif ou déjà couvert.
pub fn attente_avant_de_rejoindre_ms(position_meneuse_ms: u64, decalage_ms: i64) -> u64 {
    if decalage_ms <= 0 {
        return 0;
    }
    (decalage_ms as u64).saturating_sub(position_meneuse_ms)
}

/// L'attente avant de faire suivre une pause ou une reprise : tout le
/// décalage positif, rien pour un décalage négatif (on ne devance pas un
/// geste).
pub fn attente_avant_un_geste_ms(decalage_ms: i64) -> u64 {
    decalage_ms.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_suiveuse_retardee_rejoint_en_arriere() {
        // Meneuse à 1:00 : une suiveuse à +400 ms part à 0:59.6.
        assert_eq!(position_de_la_suiveuse(60_000, 400, 300_000), 59_600);
        // Aucune attente : la meneuse a déjà joué plus que le décalage.
        assert_eq!(attente_avant_de_rejoindre_ms(60_000, 400), 0);
    }

    #[test]
    fn au_depart_de_la_piste_la_suiveuse_retardee_attend() {
        // La meneuse vient de partir (120 ms) : la suiveuse à +400 ms attend
        // les 280 ms qui restent, puis part du début.
        assert_eq!(attente_avant_de_rejoindre_ms(120, 400), 280);
        assert_eq!(position_de_la_suiveuse(120, 400, 300_000), 0);
    }

    #[test]
    fn une_suiveuse_avancee_rejoint_en_avant_sans_attendre() {
        assert_eq!(position_de_la_suiveuse(60_000, -500, 300_000), 60_500);
        assert_eq!(attente_avant_de_rejoindre_ms(0, -500), 0);
        assert_eq!(attente_avant_un_geste_ms(-500), 0);
        // Jamais au-delà de la fin de la piste.
        assert_eq!(position_de_la_suiveuse(299_800, -500, 300_000), 299_999);
    }

    #[test]
    fn sans_duree_connue_aucune_avance() {
        assert_eq!(position_de_la_suiveuse(60_000, -500, 0), 60_000);
        // Le retard, lui, reste possible.
        assert_eq!(position_de_la_suiveuse(60_000, 500, 0), 59_500);
    }

    #[test]
    fn sans_decalage_rien_ne_change() {
        assert_eq!(position_de_la_suiveuse(60_000, 0, 300_000), 60_000);
        assert_eq!(attente_avant_de_rejoindre_ms(0, 0), 0);
        assert_eq!(attente_avant_un_geste_ms(0), 0);
    }

    #[test]
    fn pause_et_reprise_attendent_tout_le_decalage_positif() {
        assert_eq!(attente_avant_un_geste_ms(750), 750);
    }
}
