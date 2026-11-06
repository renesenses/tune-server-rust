//! 🔴 #6721 / #7119 — la garde contre les montées brusques mesure la hausse
//! depuis ce que l'APPAREIL joue, pas seulement depuis la mémoire de la zone.
//!
//! La garde (#6919, `audio::garde_de_hausse`) plafonne toute consigne à
//! 10 dB perçus au-dessus du volume de la zone EN MÉMOIRE. Or la mémoire peut
//! être plus haute que l'appareil :
//!
//! * au démarrage de Tune, la mémoire est restaurée depuis la base
//!   (`restore_zone_volumes`), alors que l'appareil a pu être baissé entre-temps
//!   (télécommande, molette, autre application, niveau de mise sous tension
//!   d'un ampli-tuner) ;
//! * le sondeur n'adopte un relevé plus bas que sur FRONT (#358 : l'appareil
//!   doit avoir bougé depuis le tour précédent). Un appareil déjà bas au
//!   premier relevé, et qui ne bouge plus, n'est jamais adopté.
//!
//! Dans ces deux cas, le curseur affiche la mémoire (80 %) et l'appareil joue
//! bas (10 %). L'auditeur saisit le curseur pour BAISSER (60 %) : pour la
//! garde, c'est une baisse (80 → 60), elle laisse passer, et l'appareil saute
//! de 10 à 60 d'un coup. C'est le geste du fil 2366 (« je saisis le curseur
//! pour diminuer le volume : résultat volume à fond »), par un autre chemin
//! que PURE (#6723).
//!
//! Règle : le repère de la garde (et de la limite de confort) est le plus BAS
//! de la mémoire et du dernier niveau connu de l'appareil, ramené à l'échelle
//! de la consigne (trim retiré). Le dernier niveau connu est :
//!
//! * noté par le sondeur au PREMIER relevé de l'appareil et à chaque fois
//!   qu'il bouge (jamais pendant la grâce qui suit une commande de Tune, où le
//!   relevé peut être en retard) ;
//! * remplacé par la consigne que Tune vient de faire accepter à l'appareil.
//!
//! Le repère ne monte jamais au-dessus de la mémoire : un relevé plus haut
//! (appareil remonté de lui-même, #6008) ne desserre pas la garde. Rien n'est
//! adopté par cette règle : la mémoire, la base et le curseur ne changent pas
//! (décision #358 / #6739 tenue) ; seule la hausse permise en une consigne
//! se mesure désormais depuis le niveau réel.
use std::collections::HashMap;
use std::sync::Mutex;

/// Le dernier niveau connu de l'appareil de chaque zone, en échelle de
/// consigne (0..1, trim retiré).
#[derive(Debug, Default)]
pub(crate) struct NiveauxDesAppareils(Mutex<HashMap<i64, f64>>);

impl NiveauxDesAppareils {
    /// Retient `niveau` pour `zone_id`. Une valeur illisible est ignorée.
    pub(crate) fn noter(&self, zone_id: i64, niveau: f64) {
        if !niveau.is_finite() {
            return;
        }
        if let Ok(mut m) = self.0.lock() {
            m.insert(zone_id, niveau.clamp(0.0, 1.0));
        }
    }

    /// Le dernier niveau connu, s'il y en a un.
    pub(crate) fn connu(&self, zone_id: i64) -> Option<f64> {
        self.0.lock().ok().and_then(|m| m.get(&zone_id).copied())
    }
}

/// Le repère d'où se mesure une hausse : le plus bas de la mémoire `actuel`
/// et du niveau connu de l'appareil. Jamais au-dessus de la mémoire.
pub(crate) fn repere_de_hausse(actuel: f64, niveau_de_l_appareil: Option<f64>) -> f64 {
    match niveau_de_l_appareil {
        Some(n) if n.is_finite() && n < actuel => n.max(0.0),
        _ => actuel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_repere_est_le_plus_bas_de_la_memoire_et_de_l_appareil_6721() {
        assert_eq!(repere_de_hausse(0.8, Some(0.1)), 0.1);
        // Un appareil plus haut ne desserre jamais la garde.
        assert_eq!(repere_de_hausse(0.3, Some(0.95)), 0.3);
        assert_eq!(repere_de_hausse(0.3, None), 0.3);
        assert_eq!(repere_de_hausse(0.3, Some(f64::NAN)), 0.3);
        assert_eq!(repere_de_hausse(0.3, Some(-1.0)), 0.0);
    }

    #[test]
    fn un_niveau_illisible_n_est_pas_retenu_6721() {
        let n = NiveauxDesAppareils::default();
        n.noter(1, f64::NAN);
        assert_eq!(n.connu(1), None);
        n.noter(1, 1.7);
        assert_eq!(n.connu(1), Some(1.0));
        n.noter(1, 0.2);
        assert_eq!(n.connu(1), Some(0.2));
        assert_eq!(n.connu(2), None);
    }
}
