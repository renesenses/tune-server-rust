//! Volume des raccourcis globaux, au contrat du serveur (#1274, #6787).
//!
//! `volume` est TOUJOURS un gain linéaire 0..1, en lecture comme en écriture.
//! Le serveur lit tout nombre `> 1.0` comme un pourcentage et le reste comme
//! une fraction (`routes/zones/ecriture.rs::update_volume`) : l'ancien
//! `{"volume": 1}` — 1 % — valait donc le PLEIN volume.
//!
//! Sur une sortie logicielle, la zone publie `volume_curve: "db60"` dans
//! `GET /zones` et le curseur est en dB. Les raccourcis ±5 avancent de cinq
//! points de CE curseur, comme la page web, et non de cinq points de gain
//! (qui, en bas de course, valaient plusieurs dizaines de dB). Mêmes formules
//! que `tune_core::audio::volume_scale` et que `src/contrat.js`.

const PLAGE_PERCEPTIVE_DB: f64 = 60.0;
const GENOU_PERCEPTIF: f64 = 0.1;

fn gain_au_genou() -> f64 {
    10f64.powf(-PLAGE_PERCEPTIVE_DB * (1.0 - GENOU_PERCEPTIF) / 20.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Courbe {
    Lineaire,
    Perceptive,
}

impl Courbe {
    /// `volume_curve` d'une zone ; absent ou inconnu : linéaire.
    pub fn depuis(code: Option<&str>) -> Self {
        match code {
            Some("db60") => Self::Perceptive,
            _ => Self::Lineaire,
        }
    }

    /// Gain 0..1 d'une position de curseur 0..100.
    pub fn gain(self, points: i32) -> f64 {
        let p = f64::from(points.clamp(0, 100)) / 100.0;
        match self {
            Self::Lineaire => p,
            Self::Perceptive => {
                if p <= 0.0 {
                    0.0
                } else if p >= 1.0 {
                    1.0
                } else if p < GENOU_PERCEPTIF {
                    gain_au_genou() * p / GENOU_PERCEPTIF
                } else {
                    10f64.powf(-PLAGE_PERCEPTIVE_DB * (1.0 - p) / 20.0)
                }
            }
        }
    }

    /// Position 0..100 d'un gain, arrondie vers le BAS : renvoyer la position
    /// lue ne monte jamais le son.
    pub fn position(self, gain: f64) -> i32 {
        let g = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
        // Un serveur d'avant #1274 pouvait publier 0..100.
        let g = if g > 1.0 { g / 100.0 } else { g };
        let brute = match self {
            Self::Lineaire => g.min(1.0),
            Self::Perceptive => {
                if g <= 0.0 {
                    0.0
                } else if g >= 1.0 {
                    1.0
                } else if g < gain_au_genou() {
                    (g / gain_au_genou() * GENOU_PERCEPTIF).min(GENOU_PERCEPTIF)
                } else {
                    (1.0 + 20.0 * g.log10() / PLAGE_PERCEPTIVE_DB).clamp(GENOU_PERCEPTIF, 1.0)
                }
            }
        };
        let mut points = ((brute * 100.0) + 1e-9).floor() as i32;
        while points > 0 && self.gain(points) > g + 1e-12 {
            points -= 1;
        }
        points.clamp(0, 100)
    }
}

/// La zone `zone_id` dans la réponse de `GET /api/v1/zones` : son gain et sa
/// courbe. `None` si la zone n'y est pas ou ne publie pas de volume.
///
/// Les raccourcis lisaient `volume` à la RACINE de `GET /widget/data`, où il
/// n'a jamais été (il est dans chaque élément de `zones`) : faute de valeur,
/// ils repartaient de 50 à chaque appui.
pub fn zone_dans(zones: &serde_json::Value, zone_id: i64) -> Option<(f64, Courbe)> {
    let liste = zones
        .as_array()
        .or_else(|| zones.get("zones").and_then(|z| z.as_array()))?;
    let zone = liste
        .iter()
        .find(|z| z.get("id").and_then(|v| v.as_i64()) == Some(zone_id))?;
    let gain = zone.get("volume")?.as_f64()?;
    let courbe = Courbe::depuis(zone.get("volume_curve").and_then(|v| v.as_str()));
    Some((gain, courbe))
}

/// Gain à envoyer après un appui de `delta` points sur le curseur.
pub fn apres_un_pas(gain: f64, courbe: Courbe, delta: i32) -> f64 {
    courbe.gain((courbe.position(gain) + delta).clamp(0, 100))
}

/// Corps de `PUT /zones/{id}/volume` pour une position 0..100 du curseur
/// LINÉAIRE (commande `set_volume`) : une fraction, jamais un pourcentage.
pub fn corps_lineaire(points: i32) -> serde_json::Value {
    serde_json::json!({ "volume": Courbe::Lineaire.gain(points) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_pour_cent_ne_part_plus_en_plein_volume() {
        // Le serveur lit `> 1.0` comme un pourcentage, le reste comme une
        // fraction : 1 doit partir en 0.01, pas en 1.
        let v = corps_lineaire(1)["volume"].as_f64().unwrap();
        assert!(
            v < 0.02,
            "1 % envoyé comme {v} : le serveur le lirait comme le plein volume"
        );
        for p in 0..=100 {
            let v = corps_lineaire(p)["volume"].as_f64().unwrap();
            assert!((0.0..=1.0).contains(&v), "position {p} → {v}");
        }
    }

    #[test]
    fn le_volume_se_lit_dans_la_zone_de_get_zones() {
        let zones = serde_json::json!([
            {"id": 1, "volume": 0.8, "volume_curve": "linear"},
            {"id": 3, "volume": 0.05, "volume_curve": "db60"},
        ]);
        assert_eq!(zone_dans(&zones, 1), Some((0.8, Courbe::Lineaire)));
        assert_eq!(zone_dans(&zones, 3), Some((0.05, Courbe::Perceptive)));
        assert_eq!(zone_dans(&zones, 9), None);
        // La racine de /widget/data n'a pas de `volume` : rien à inventer.
        let widget_data =
            serde_json::json!({"state": "playing", "zones": [{"zone_id": 1, "volume": 0.4}]});
        assert_eq!(zone_dans(&widget_data, 1), None);
    }

    #[test]
    fn un_pas_suit_la_courbe_de_la_zone() {
        // Linéaire : 0.40 → 0.45.
        let v = apres_un_pas(0.40, Courbe::Lineaire, 5);
        assert!((v - 0.45).abs() < 1e-9, "{v}");
        // db60 : 50 % du curseur = −30 dB ; +5 points = −27 dB, pas +5 points
        // de gain (qui feraient −5,6 dB, soit +24 dB d'un coup).
        let depart = Courbe::Perceptive.gain(50);
        let v = apres_un_pas(depart, Courbe::Perceptive, 5);
        let db = 20.0 * v.log10();
        assert!((db + 27.0).abs() < 1e-6, "{db} dB");
        // Bornes.
        assert_eq!(apres_un_pas(1.0, Courbe::Perceptive, 5), 1.0);
        assert_eq!(apres_un_pas(0.0, Courbe::Perceptive, -5), 0.0);
    }

    #[test]
    fn relire_puis_renvoyer_ne_monte_jamais_le_son() {
        for courbe in [Courbe::Lineaire, Courbe::Perceptive] {
            for i in 0..=1000 {
                let g = f64::from(i) / 1000.0;
                let p = courbe.position(g);
                assert!(courbe.gain(p) <= g + 1e-12, "{courbe:?} {g} → {p}");
            }
        }
    }
}
