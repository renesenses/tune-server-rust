//! #9043 — la barre de progression « collée à la fin » dès le départ d'une
//! piste (fil 2694 : piste locale vers un renderer UPnP, Fosi S3 Lite puis
//! WiiM Rivo Plus ; TIDAL correct). Défaut intermittent, non reproduit sans
//! le matériel : la lecture du code ne donne pas de cause.
//!
//! Ce module ne corrige rien. Il DIT, une fois par piste, la forme exacte du
//! symptôme quand elle se produit : une position PUBLIÉE au moins égale à la
//! durée de la piste dans les premières secondes de la piste. Le journal
//! porte alors les trois relevés que l'enquête réclame (position brute du
//! renderer, position retenue après la garde de monotonie, durée en base et
//! durée rapportée), et départage les pistes ouvertes dans l'issue :
//!
//! - `position_brute_ms` ≥ `duree_ms` : le renderer rapporte un `RelTime`
//!   faux (ou une durée en base trop courte) ;
//! - `position_brute_ms` < `duree_ms` mais `position_publiee_ms` ≥ `duree_ms` :
//!   c'est le plancher de monotonie (#3229) qui n'a pas été reposé au départ.
//!
//! Aucune ligne n'apparaît quand la barre suit le son : la forme d'onde du
//! client web (`clip-path`), seule autre piste, se départage alors côté client.

use std::collections::HashMap;
use std::sync::Mutex;

/// Fenêtre, depuis le départ de la piste, où une barre pleine est forcément
/// fausse : aucune piste ne se termine si tôt après avoir commencé.
pub(super) const FENETRE_DU_DEPART_S: u64 = 15;

/// La position publiée est-elle au bout de la piste alors que la piste vient
/// de commencer ? Une piste plus courte que la fenêtre (ou sans durée) ne dit
/// rien : sa fin peut réellement tomber dans la fenêtre.
pub(super) fn barre_au_bout_des_le_depart(
    ecoule_depuis_le_depart_s: u64,
    position_publiee_ms: i64,
    duree_ms: i64,
) -> bool {
    duree_ms > (FENETRE_DU_DEPART_S as i64) * 1000
        && ecoule_depuis_le_depart_s < FENETRE_DU_DEPART_S
        && position_publiee_ms >= duree_ms
}

/// La dernière piste signalée par zone : une ligne par piste, pas une par
/// tour de sondage.
static DEJA_SIGNALEE: Mutex<Option<HashMap<i64, String>>> = Mutex::new(None);

/// Vrai la première fois que `piste` est vue au bout pour `zone_id`.
pub(super) fn a_signaler(zone_id: i64, piste: &str) -> bool {
    let Ok(mut garde) = DEJA_SIGNALEE.lock() else {
        return false;
    };
    let carte = garde.get_or_insert_with(HashMap::new);
    if carte.get(&zone_id).map(String::as_str) == Some(piste) {
        return false;
    }
    carte.insert(zone_id, piste.to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_barre_pleine_au_depart_d_une_piste_longue_est_signalee_9043() {
        // Fil 2694 : la piste vient de commencer, la barre est au bout.
        assert!(barre_au_bout_des_le_depart(2, 240_000, 240_000));
        assert!(barre_au_bout_des_le_depart(0, 300_000, 240_000));
        // Elle suit le son : rien à dire.
        assert!(!barre_au_bout_des_le_depart(2, 2_000, 240_000));
        // La vraie fin d'une piste, bien après la fenêtre du départ.
        assert!(!barre_au_bout_des_le_depart(240, 240_000, 240_000));
        // Une piste plus courte que la fenêtre, ou sans durée connue.
        assert!(!barre_au_bout_des_le_depart(10, 12_000, 12_000));
        assert!(!barre_au_bout_des_le_depart(1, 5_000, 0));
    }

    #[test]
    fn une_seule_ligne_par_piste_et_par_zone_9043() {
        let zone = -9043;
        assert!(a_signaler(zone, "local:1:s1"));
        assert!(!a_signaler(zone, "local:1:s1"), "pas une ligne par tour");
        assert!(a_signaler(zone, "local:2:s2"), "piste suivante : à nouveau");
        assert!(a_signaler(zone - 1, "local:2:s2"), "autre zone : à part");
    }

    /// Garde de branchement : `tick` appelle le prédicat sur la position
    /// PUBLIÉE, après la garde de monotonie, avant d'émettre.
    #[test]
    fn tick_signale_la_barre_au_bout_9043() {
        let tick = include_str!("tick.rs");
        let retenue = tick
            .find("let publiee = self.playback.update_position(zone_id, reported).await;")
            .expect("position retenue");
        let appel = retenue
            + tick[retenue..]
                .find("barre_au_bout_9043::barre_au_bout_des_le_depart(")
                .expect("le sondeur doit signaler une barre au bout dès le départ");
        let emission = retenue
            + tick[retenue..]
                .find("self.playback.emit_position(zone_id, publiee);")
                .expect("émission");
        assert!(appel < emission);
        assert!(tick.contains("\"barre_de_progression_au_bout_des_le_depart\""));
    }
}
