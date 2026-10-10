//! #4969 — la carte des canaux de SORTIE : ce que devient chaque canal de la
//! source une fois arrivé au périphérique.
//!
//! Les niveaux de `playback.audio_levels` sont mesurés sur le PCM du décodeur,
//! donc dans l'ordre des canaux de la SOURCE. Or, sur une sortie locale, la
//! source traverse encore l'adaptation des canaux : la matrice du greffon
//! « Réaffectation des canaux » (#6044), sinon le routage par la disposition
//! que déclare le fichier (#6057), sinon l'adaptation par défaut
//! (`audio/channels`). Un bargraphe tracé dans l'ordre de la source montrait
//! donc un 4.0 sur quatre barres alors que l'ampli en reçoit six, et une
//! réaffectation FL ↔ FR n'y changeait rien.
//!
//! La carte est la matrice `sorties × entrees` de cette adaptation. Elle n'est
//! pas recopiée de ses règles : elle est MESURÉE, en faisant passer une
//! impulsion par canal dans la fonction même qu'emploie la sortie
//! ([`adapter_vers_la_sortie`]). Toutes ces adaptations sont linéaires, donc
//! la réponse aux impulsions est la matrice, au coefficient près, et elle ne
//! peut pas diverger de ce qui sort.
use std::sync::Arc;

use super::disposition_canaux::Disposition;
use super::reaffectation_canaux::Matrice;

/// L'adaptation des canaux de l'étage flottant de la sortie locale, en un seul
/// endroit : la matrice de la zone ou de l'album prime ; à défaut, la
/// disposition déclarée par le fichier (si elle compte les canaux de la
/// source) ; à défaut, l'adaptation par défaut ; à nombre de canaux égal et
/// sans rien de posé, le tampon ressort tel quel.
///
/// `matrice` et `declaree` sont ce que la sortie a retenu pour la piste
/// (`None` en DoP, et la matrice `None` en PURE) : la décision de les poser
/// reste à la sortie, celle de les appliquer est ici.
pub fn adapter_vers_la_sortie(
    samples: &[f32],
    source: u16,
    sortie: u16,
    matrice: Option<&Matrice>,
    declaree: Option<&Disposition>,
) -> Result<Vec<f32>, String> {
    if let Some(m) = matrice {
        return Ok(m.appliquer_f32(samples));
    }
    if let Some(d) = declaree.filter(|d| d.canaux() == source) {
        return super::channels::adapt_channels_f32_disposee(samples, source, sortie, Some(d));
    }
    super::channels::adapt_channels_f32(samples, source, sortie)
}

/// La matrice `sorties × entrees` de l'adaptation des canaux d'une sortie.
#[derive(Debug, Clone, PartialEq)]
pub struct CarteDesCanaux {
    entrees: u16,
    sorties: u16,
    /// Une ligne par canal de sortie, une colonne par canal de la source.
    coefficients: Vec<f64>,
}

/// Ce qu'une sortie locale branchée sait dire de sa carte, relu à chaque
/// fenêtre de niveaux. `None` hors lecture.
pub type SondeDesCanaux = Arc<dyn Fn() -> Option<CarteDesCanaux> + Send + Sync>;

impl CarteDesCanaux {
    /// Mesurer la carte d'une adaptation : une trame par canal de la source,
    /// une impulsion à 1,0 sur ce canal, et ce qui en ressort. `None` si
    /// l'adaptation refuse ou ne rend pas une trame de `sorties` canaux par
    /// impulsion.
    pub fn depuis_adaptation(
        entrees: u16,
        sorties: u16,
        adapter: impl FnOnce(&[f32]) -> Result<Vec<f32>, String>,
    ) -> Option<Self> {
        let (n, m) = (usize::from(entrees), usize::from(sorties));
        if n == 0 || m == 0 {
            return None;
        }
        let mut impulsions = vec![0.0f32; n * n];
        for c in 0..n {
            impulsions[c * n + c] = 1.0;
        }
        let reponse = adapter(&impulsions).ok()?;
        if reponse.len() != n * m {
            return None;
        }
        let mut coefficients = vec![0.0f64; m * n];
        for (i, trame) in reponse.chunks_exact(m).enumerate() {
            for (o, v) in trame.iter().enumerate() {
                coefficients[o * n + i] = f64::from(*v);
            }
        }
        Some(Self {
            entrees,
            sorties,
            coefficients,
        })
    }

    /// La carte d'une sortie qui recopie la source, canal pour canal.
    pub fn identite(canaux: u16) -> Self {
        let n = usize::from(canaux);
        let mut coefficients = vec![0.0; n * n];
        for c in 0..n {
            coefficients[c * n + c] = 1.0;
        }
        Self {
            entrees: canaux,
            sorties: canaux,
            coefficients,
        }
    }

    pub fn entrees(&self) -> u16 {
        self.entrees
    }

    pub fn sorties(&self) -> u16 {
        self.sorties
    }

    /// Les coefficients, une ligne par canal de sortie.
    pub fn coefficients(&self) -> &[f64] {
        &self.coefficients
    }

    /// Le gain du canal `entree` de la source sur le canal `sortie`.
    pub fn coefficient(&self, sortie: usize, entree: usize) -> f64 {
        self.coefficients[sortie * usize::from(self.entrees) + entree]
    }

    /// Chaque canal de sortie recopie le canal de même rang ?
    pub fn est_identite(&self) -> bool {
        *self == Self::identite(self.entrees)
    }

    /// Ce qui sort est-il multicanal ? Plus de deux voies ouvertes, ET du
    /// signal routé au-delà de la paire avant : une source stéréo ouverte en
    /// six voies par l'adaptation par défaut (FL FR puis quatre silences) reste
    /// une écoute stéréo, et l'écran garde ses deux aiguilles.
    pub fn est_multicanale(&self) -> bool {
        let n = usize::from(self.entrees);
        self.sorties > 2
            && self
                .coefficients
                .chunks_exact(n.max(1))
                .skip(2)
                .any(|ligne| ligne.iter().any(|c| *c != 0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::disposition_canaux::{BL, BR, FL, FR};
    use crate::audio::reaffectation_canaux::prereglage;

    fn matrice(id: &str) -> Matrice {
        Matrice::du_reglage_arme(&prereglage(id).expect("préréglage")).expect("matrice")
    }

    #[test]
    fn la_carte_d_une_matrice_est_la_matrice() {
        let m = matrice("quad_to_5_1");
        let carte = CarteDesCanaux::depuis_adaptation(4, 6, |s| {
            adapter_vers_la_sortie(s, 4, 6, Some(&m), None)
        })
        .expect("carte");
        assert_eq!((carte.entrees(), carte.sorties()), (4, 6));
        for o in 0..6 {
            for i in 0..4 {
                assert!(
                    (carte.coefficient(o, i) - m.coefficient(o, i)).abs() < 1e-6,
                    "sortie {o}, entrée {i}"
                );
            }
        }
        assert!(carte.est_multicanale());
    }

    #[test]
    fn un_5_1_a_l_identique_est_l_identite_et_reste_multicanal() {
        let carte = CarteDesCanaux::depuis_adaptation(6, 6, |s| {
            adapter_vers_la_sortie(s, 6, 6, None, None)
        })
        .expect("carte");
        assert!(carte.est_identite());
        assert!(carte.est_multicanale());
    }

    #[test]
    fn une_source_stereo_ouverte_en_six_voies_reste_stereo() {
        let carte = CarteDesCanaux::depuis_adaptation(2, 6, |s| {
            adapter_vers_la_sortie(s, 2, 6, None, None)
        })
        .expect("carte");
        assert!(!carte.est_multicanale());
        // Repli 5.1 → stéréo : deux voies, ce n'est plus du multicanal.
        let repli = CarteDesCanaux::depuis_adaptation(6, 2, |s| {
            adapter_vers_la_sortie(s, 6, 2, None, None)
        })
        .expect("carte");
        assert!(!repli.est_multicanale());
    }

    #[test]
    fn la_disposition_declaree_route_par_position() {
        // Un 4 canaux déclaré FL FR BL BR… dans un ordre inhabituel : BL BR FL FR.
        let d = Disposition::depuis_masque(FL | FR | BL | BR, 4).expect("masque");
        assert!(d.est_par_defaut(), "le masque suit l'ordre des bits");
        // Ouvert en 6 voies, l'arrière de la source (rangs 2, 3) arrive sur BL/BR (4, 5).
        let carte = CarteDesCanaux::depuis_adaptation(4, 6, |s| {
            adapter_vers_la_sortie(s, 4, 6, None, Some(&d))
        })
        .expect("carte");
        assert_eq!(carte.coefficient(4, 2), 1.0);
        assert_eq!(carte.coefficient(5, 3), 1.0);
        assert_eq!(carte.coefficient(2, 2), 0.0, "rien sur FC");
        assert_eq!(carte.coefficient(3, 3), 0.0, "rien sur le LFE");
    }

    #[test]
    fn une_adaptation_qui_refuse_ne_donne_pas_de_carte() {
        assert!(CarteDesCanaux::depuis_adaptation(6, 6, |_| Err("non".into())).is_none());
        assert!(CarteDesCanaux::depuis_adaptation(6, 6, |_| Ok(vec![0.0; 3])).is_none());
        assert!(CarteDesCanaux::depuis_adaptation(0, 6, |s| Ok(s.to_vec())).is_none());
    }
}
