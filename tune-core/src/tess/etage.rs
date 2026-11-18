//! Le trait d'un étage TESS, et l'étape finale qui sort du `f64`.
//!
//! Règles temps réel (lot T03 et tous les moteurs) :
//!
//! 1. **`preparer` est hors temps réel** : l'étage y reçoit le format d'entrée
//!    ET la taille de bloc maximale, alloue tout ce dont il a besoin (FFT,
//!    lignes à retard, anticipation) et rend sa [`Disposition`] : format de
//!    sortie, nombre maximal de trames en sortie, latence, travail sur place ou
//!    non.
//! 2. **L'hôte préalloue** les tampons de chaque étage d'après les
//!    dispositions ([`preparer_chaine`]). Un étage qui garde le format et le
//!    nombre de trames travaille **sur place** ([`EtageTess::traiter_sur_place`]) ;
//!    un étage qui change le nombre de canaux (réaffectation) ou la cadence
//!    (suréchantillonnage) écrit dans un **tampon de sortie distinct**
//!    ([`EtageTess::traiter_vers`]), préalloué par l'hôte.
//! 3. **`traiter_*` et `vidanger` n'allouent jamais** et ne bloquent jamais.
//!    [`ErreurEtage`] est `Copy` et ne porte aucune `String`.
//! 4. Les échantillons sont **entrelacés en `f64`**, pleine échelle ±1,0. Aucune
//!    conversion `f32` ni entière entre deux étages (Q5) : la seule
//!    quantification est l'[`EtapeFinale`].
//!
//! **Contournement (bypass) d'un étage en cours de lecture** : l'étage rend son
//! entrée intacte, **retardée de sa latence** (le retard est conservé), pour que
//! cocher ou décocher un étage ne décale pas le flux, ni la synchronisation
//! multi-points. « Bypass all » et le mode Pure, eux, reconstruisent la chaîne
//! SANS les étages à la relance suivante : aucun retard, bit-perfect.
//!
//! **Réglages à chaud** : [`EtageTess::appliquer_reglages`] est hors temps réel.
//! L'hôte l'appelle sur une instance qui ne joue pas, ou construit une nouvelle
//! instance, la prépare, puis l'échange entre deux blocs (T03 décide du fondu).
//! Un étage qui ne sait pas changer à chaud rend
//! [`ErreurEtage::ReconstructionRequise`].

use serde_json::Value;

use super::chaine::TauxDsd;

/// Le format du flux à l'entrée ou à la sortie d'un étage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FormatFlux {
    pub channels: u16,
    pub sample_rate_hz: u32,
}

/// Ce qu'un étage annonce après sa préparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disposition {
    /// Format de sortie : identique à l'entrée, sauf réaffectation des canaux
    /// ou changement de cadence.
    pub sortie: FormatFlux,
    /// Nombre maximal de trames rendues pour un bloc d'entrée de la taille
    /// maximale (×N pour un suréchantillonneur de rapport N).
    pub max_trames_sortie: usize,
    /// Latence introduite, en trames à la fréquence de SORTIE.
    pub latence_trames: u32,
    /// Vrai : l'hôte appelle [`EtageTess::traiter_sur_place`] ; faux :
    /// [`EtageTess::traiter_vers`]. Un étage sur place garde forcément le
    /// format et le nombre de trames.
    pub sur_place: bool,
}

impl Disposition {
    /// Disposition d'un étage qui garde le format, travaille sur place.
    pub fn sur_place(entree: FormatFlux, max_trames: usize, latence_trames: u32) -> Self {
        Self {
            sortie: entree,
            max_trames_sortie: max_trames,
            latence_trames,
            sur_place: true,
        }
    }

    /// Capacité, en échantillons, du tampon de sortie que l'hôte préalloue.
    pub fn capacite_sortie(&self) -> usize {
        self.max_trames_sortie * usize::from(self.sortie.channels)
    }
}

/// Pourquoi un étage refuse de travailler. `Copy`, sans allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ErreurEtage {
    #[error("format non pris en charge : {channels} canaux à {sample_rate_hz} Hz")]
    FormatNonPrisEnCharge { channels: u16, sample_rate_hz: u32 },
    #[error("bloc de {trames} trames au-delà du maximum préparé ({max})")]
    BlocTropGrand { trames: usize, max: usize },
    #[error("tampon de {capacite} échantillons, {requis} requis")]
    TamponTropPetit { requis: usize, capacite: usize },
    #[error("mode d'appel non pris en charge par cet étage")]
    ModeNonPrisEnCharge,
    #[error("étage non préparé")]
    NonPrepare,
    #[error("réglages refusés : {0}")]
    Reglages(&'static str),
    #[error("cet étage se reconstruit pour changer de réglages")]
    ReconstructionRequise,
}

/// Vérifie qu'un tampon contient au moins `trames` trames de `canaux` canaux.
/// Utilisable dans le fil audio : aucune allocation.
pub fn verifier_tampon(longueur: usize, trames: usize, canaux: u16) -> Result<(), ErreurEtage> {
    let requis = trames * usize::from(canaux);
    if longueur < requis {
        return Err(ErreurEtage::TamponTropPetit {
            requis,
            capacite: longueur,
        });
    }
    Ok(())
}

/// Un étage de traitement TESS (greffon ou étage fixe en flottant).
pub trait EtageTess: Send {
    /// Identifiant stable : celui du descripteur pour un greffon, le code de
    /// l'étape pour un étage fixe.
    fn id(&self) -> &str;

    /// HORS TEMPS RÉEL. Prépare l'étage (allocations comprises).
    fn preparer(
        &mut self,
        entree: FormatFlux,
        max_trames: usize,
    ) -> Result<Disposition, ErreurEtage>;

    /// TEMPS RÉEL. Traite `trames` trames sur place (`tampon` en contient au
    /// moins autant). Seulement si la disposition dit `sur_place`.
    fn traiter_sur_place(&mut self, tampon: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        let _ = (tampon, trames);
        Err(ErreurEtage::ModeNonPrisEnCharge)
    }

    /// TEMPS RÉEL. Lit `trames` trames de `entree`, écrit dans `sortie`
    /// (capacité [`Disposition::capacite_sortie`]) et rend le nombre de trames
    /// écrites. Seulement si la disposition ne dit pas `sur_place`.
    fn traiter_vers(
        &mut self,
        entree: &[f64],
        trames: usize,
        sortie: &mut [f64],
    ) -> Result<usize, ErreurEtage> {
        let _ = (entree, trames, sortie);
        Err(ErreurEtage::ModeNonPrisEnCharge)
    }

    /// TEMPS RÉEL. En fin de flux, écrit la queue retenue par la latence
    /// (fin de convolution, anticipation du Declick) et rend le nombre de
    /// trames écrites ; 0 quand il n'y a plus rien. Un étage sans latence n'a
    /// rien à vidanger.
    fn vidanger(&mut self, sortie: &mut [f64]) -> Result<usize, ErreurEtage> {
        let _ = sortie;
        Ok(0)
    }

    /// Vide les mémoires internes (saut, piste non gapless). Sans allocation.
    fn reinitialiser(&mut self) {}

    fn bypass(&self) -> bool;

    /// Contournement en cours de lecture : retard conservé (voir le module).
    fn definir_bypass(&mut self, bypass: bool);

    /// HORS TEMPS RÉEL. Change les réglages sans reconstruire l'étage.
    fn appliquer_reglages(&mut self, reglages: &Value) -> Result<(), ErreurEtage> {
        let _ = reglages;
        Err(ErreurEtage::ReconstructionRequise)
    }
}

/// La sortie de l'étape finale : le seul endroit où l'on quitte le `f64`.
pub enum SortieFinale<'a> {
    /// PCM entier, cadré à gauche sur 32 bits, `bits` significatifs
    /// (16, 24 ou 32).
    Pcm {
        echantillons: &'a mut [i32],
        bits: u8,
    },
    /// DSD 1 bit, 8 échantillons par octet, bit de poids fort en premier,
    /// octets entrelacés par canal.
    Dsd { octets: &'a mut [u8], taux: TauxDsd },
}

/// L'étape finale de la chaîne : quantification PCM (dither TPDF, mise en
/// forme du bruit ou aucun dither) ou modulateur sigma-delta vers DSD.
///
/// Elle n'est PAS un [`EtageTess`] : sa sortie n'est pas en `f64`. Elle suit
/// les mêmes règles temps réel (préparation hors temps réel, aucune
/// allocation ensuite). Elle se place au rendu (Q9, #7264) : l'anneau entre
/// le producteur et le rendu transporte donc des `f64` (engagement de T03,
/// voir `docs/tess/contrat.md`).
pub trait EtapeFinale: Send {
    /// HORS TEMPS RÉEL. Rend le nombre maximal d'éléments de sortie
    /// (échantillons PCM ou octets DSD) pour `max_trames` trames d'entrée.
    fn preparer(&mut self, entree: FormatFlux, max_trames: usize) -> Result<usize, ErreurEtage>;

    /// TEMPS RÉEL. Rend le nombre d'éléments écrits.
    fn finaliser(
        &mut self,
        entree: &[f64],
        trames: usize,
        sortie: SortieFinale<'_>,
    ) -> Result<usize, ErreurEtage>;

    fn reinitialiser(&mut self) {}
}

/// Le plan préparé d'une suite d'étages : une disposition par étage, et la
/// latence totale à la fréquence de sortie de la suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPrepare {
    pub dispositions: Vec<Disposition>,
    pub latence_totale_trames: u64,
    pub sortie: FormatFlux,
    pub max_trames_sortie: usize,
}

/// HORS TEMPS RÉEL. Prépare une suite d'étages dans l'ordre, en propageant le
/// format et la taille de bloc maximale de l'un à l'autre, et vérifie les
/// dispositions (un étage « sur place » doit garder format et trames). La
/// latence de chaque étage est ramenée à la fréquence de sortie de la suite.
pub fn preparer_chaine(
    etages: &mut [&mut dyn EtageTess],
    entree: FormatFlux,
    max_trames: usize,
) -> Result<PlanPrepare, ErreurEtage> {
    let mut format = entree;
    let mut max = max_trames;
    let mut dispositions = Vec::with_capacity(etages.len());
    for etage in etages.iter_mut() {
        let d = etage.preparer(format, max)?;
        if d.sur_place && (d.sortie != format || d.max_trames_sortie != max) {
            return Err(ErreurEtage::ModeNonPrisEnCharge);
        }
        dispositions.push(d);
        format = d.sortie;
        max = d.max_trames_sortie;
    }
    // Latences ramenées à la fréquence de sortie finale, arrondies au-dessus.
    let latence_totale_trames = dispositions
        .iter()
        .map(|d| {
            let num = u64::from(d.latence_trames) * u64::from(format.sample_rate_hz);
            num.div_ceil(u64::from(d.sortie.sample_rate_hz.max(1)))
        })
        .sum();
    Ok(PlanPrepare {
        dispositions,
        latence_totale_trames,
        sortie: format,
        max_trames_sortie: max,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Retard pur de `n` trames (étage à latence), sur place.
    struct Retard {
        n: usize,
        ligne: Vec<f64>,
        canaux: usize,
        contourne: bool,
    }

    impl EtageTess for Retard {
        fn id(&self) -> &str {
            "retard"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            self.canaux = usize::from(e.channels);
            self.ligne = vec![0.0; self.n * self.canaux];
            Ok(Disposition::sur_place(e, max, self.n as u32))
        }
        fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
            verifier_tampon(t.len(), trames, self.canaux as u16)?;
            // Retard conservé même contourné : le contournement ne change rien
            // à un retard pur, il reste donc identique.
            for e in t[..trames * self.canaux].iter_mut() {
                if self.ligne.is_empty() {
                    break;
                }
                let sortant = self.ligne[0];
                self.ligne.rotate_left(1);
                let n = self.ligne.len();
                self.ligne[n - 1] = *e;
                *e = sortant;
            }
            Ok(())
        }
        fn vidanger(&mut self, sortie: &mut [f64]) -> Result<usize, ErreurEtage> {
            verifier_tampon(sortie.len(), self.n, self.canaux as u16)?;
            let n = self.ligne.len();
            sortie[..n].copy_from_slice(&self.ligne);
            self.ligne.iter_mut().for_each(|x| *x = 0.0);
            Ok(self.n)
        }
        fn bypass(&self) -> bool {
            self.contourne
        }
        fn definir_bypass(&mut self, b: bool) {
            self.contourne = b;
        }
    }

    /// Mono → stéréo, tampon de sortie distinct.
    struct VersStereo;

    impl EtageTess for VersStereo {
        fn id(&self) -> &str {
            "vers-stereo"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            if e.channels != 1 {
                return Err(ErreurEtage::FormatNonPrisEnCharge {
                    channels: e.channels,
                    sample_rate_hz: e.sample_rate_hz,
                });
            }
            Ok(Disposition {
                sortie: FormatFlux { channels: 2, ..e },
                max_trames_sortie: max,
                latence_trames: 0,
                sur_place: false,
            })
        }
        fn traiter_vers(
            &mut self,
            entree: &[f64],
            trames: usize,
            sortie: &mut [f64],
        ) -> Result<usize, ErreurEtage> {
            verifier_tampon(entree.len(), trames, 1)?;
            verifier_tampon(sortie.len(), trames, 2)?;
            for (i, x) in entree[..trames].iter().enumerate() {
                sortie[2 * i] = *x;
                sortie[2 * i + 1] = *x;
            }
            Ok(trames)
        }
        fn bypass(&self) -> bool {
            false
        }
        fn definir_bypass(&mut self, _: bool) {}
    }

    /// ×2 en cadence (répétition), tampon de sortie distinct.
    struct Double;

    impl EtageTess for Double {
        fn id(&self) -> &str {
            "double"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            Ok(Disposition {
                sortie: FormatFlux {
                    sample_rate_hz: e.sample_rate_hz * 2,
                    ..e
                },
                max_trames_sortie: max * 2,
                latence_trames: 8,
                sur_place: false,
            })
        }
        fn bypass(&self) -> bool {
            false
        }
        fn definir_bypass(&mut self, _: bool) {}
    }

    /// Prétend travailler sur place tout en changeant de format : interdit.
    struct Menteur;

    impl EtageTess for Menteur {
        fn id(&self) -> &str {
            "menteur"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            Ok(Disposition {
                sortie: FormatFlux {
                    channels: e.channels + 1,
                    ..e
                },
                max_trames_sortie: max,
                latence_trames: 0,
                sur_place: true,
            })
        }
        fn bypass(&self) -> bool {
            false
        }
        fn definir_bypass(&mut self, _: bool) {}
    }

    const MONO_48K: FormatFlux = FormatFlux {
        channels: 1,
        sample_rate_hz: 48_000,
    };

    #[test]
    fn la_preparation_propage_format_taille_de_bloc_et_latence() {
        let mut r = Retard {
            n: 4,
            ligne: Vec::new(),
            canaux: 0,
            contourne: false,
        };
        let mut s = VersStereo;
        let mut d = Double;
        let plan = preparer_chaine(&mut [&mut r, &mut s, &mut d], MONO_48K, 512).unwrap();
        assert_eq!(
            plan.sortie,
            FormatFlux {
                channels: 2,
                sample_rate_hz: 96_000
            }
        );
        assert_eq!(plan.max_trames_sortie, 1024);
        // Capacités préallouées par l'hôte.
        let caps: Vec<_> = plan
            .dispositions
            .iter()
            .map(Disposition::capacite_sortie)
            .collect();
        assert_eq!(caps, [512, 1024, 2048]);
        // 4 trames à 48 kHz = 8 trames à 96 kHz, plus 8 du doubleur.
        assert_eq!(plan.latence_totale_trames, 16);
        // Contre-épreuve : un étage stéréo seulement reçoit du stéréo et refuse.
        let mut s2 = VersStereo;
        assert_eq!(
            preparer_chaine(&mut [&mut s, &mut s2], MONO_48K, 512).unwrap_err(),
            ErreurEtage::FormatNonPrisEnCharge {
                channels: 2,
                sample_rate_hz: 48_000
            }
        );
    }

    #[test]
    fn un_etage_sur_place_ne_peut_pas_changer_de_format() {
        let mut m = Menteur;
        assert_eq!(
            preparer_chaine(&mut [&mut m], MONO_48K, 64).unwrap_err(),
            ErreurEtage::ModeNonPrisEnCharge
        );
    }

    #[test]
    fn un_tampon_trop_petit_est_refuse_sans_paniquer() {
        let mut s = VersStereo;
        s.preparer(MONO_48K, 4).unwrap();
        let mut sortie = [0.0; 6];
        assert_eq!(
            s.traiter_vers(&[0.1, 0.2, 0.3, 0.4], 4, &mut sortie),
            Err(ErreurEtage::TamponTropPetit {
                requis: 8,
                capacite: 6
            })
        );
        // Mode d'appel inverse : refus explicite, pas de panique.
        assert_eq!(
            s.traiter_sur_place(&mut [0.0; 8], 4),
            Err(ErreurEtage::ModeNonPrisEnCharge)
        );
        let mut sortie = [0.0; 8];
        assert_eq!(s.traiter_vers(&[0.1, 0.2, 0.3, 0.4], 4, &mut sortie), Ok(4));
        assert_eq!(sortie, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3, 0.4, 0.4]);
    }

    #[test]
    fn la_vidange_rend_la_queue_retenue_par_la_latence() {
        let mut r = Retard {
            n: 2,
            ligne: Vec::new(),
            canaux: 0,
            contourne: false,
        };
        r.preparer(MONO_48K, 4).unwrap();
        let mut t = [1.0, 2.0, 3.0, 4.0];
        r.traiter_sur_place(&mut t, 4).unwrap();
        assert_eq!(t, [0.0, 0.0, 1.0, 2.0]);
        let mut queue = [0.0; 2];
        assert_eq!(r.vidanger(&mut queue), Ok(2));
        assert_eq!(queue, [3.0, 4.0]);
        // Contournée en cours de lecture, elle garde son retard.
        r.definir_bypass(true);
        let mut t = [5.0, 6.0, 7.0];
        r.traiter_sur_place(&mut t, 3).unwrap();
        assert_eq!(t, [0.0, 0.0, 5.0]);
        // Réglages à chaud non pris en charge : reconstruction demandée.
        assert_eq!(
            r.appliquer_reglages(&Value::Null),
            Err(ErreurEtage::ReconstructionRequise)
        );
    }

    #[test]
    fn l_erreur_d_etage_n_alloue_pas() {
        // `Copy` interdit toute `String` ou `Vec` dans les variantes.
        fn copiable<T: Copy>() {}
        copiable::<ErreurEtage>();
        assert_eq!(
            verifier_tampon(10, 5, 2),
            Ok(()),
            "10 échantillons = 5 trames stéréo"
        );
        assert!(verifier_tampon(9, 5, 2).is_err());
    }
}
