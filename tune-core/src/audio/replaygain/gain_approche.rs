//! Gain ReplayGain APPROCHÉ, tiré d'un rapport `foo_dr.txt` (#5519).
//!
//! Tades (fils 2044, 2050, 2079, 2173) : « si la pointe est à −4,93 dB on
//! peut sans risque appliquer un Replay Gain de 4,9 dB », et éviter ainsi
//! des dizaines de milliers d'analyses. Décision de Bertrand (11/10, go le
//! 12/10) : quand une piste n'a AUCUN ReplayGain et qu'un rapport DR voisin
//! la désigne, Tune calcule un gain approché, **plafonné par la crête** pour
//! ne jamais écrêter, le dit « approché (foo_dr) » dans le chemin du signal,
//! et ne l'écrit JAMAIS dans les fichiers.
//!
//! # Ce que vaut l'approximation
//!
//! ReplayGain 2 vise une sonie EBU R128 / BS.1770 : niveau pondéré K (filtre
//! de tête + coupe-bas), intégré avec deux portes. Le `RMS` du DR Meter n'est
//! ni pondéré ni fenêtré : c'est le RMS de toute la piste, avec la
//! convention du TT DR où un sinus pleine échelle lit 0 dB (soit
//! `√(2·moyenne(x²))`). Un sinus de 1 kHz pleine échelle valant −3,01 LUFS,
//! on prend :
//!
//! ```text
//! sonie estimée (LUFS) ≈ RMS_foo_dr − 3,01
//! gain estimé          = −18 − sonie estimée            (référence RG 2)
//! ```
//!
//! **C'est une approximation, et elle le reste** : sur de la musique, l'écart
//! entre ce RMS et la sonie intégrée dépend du spectre (la pondération K
//! relève l'aigu d'environ 4 dB et coupe le grave) et des silences (la porte
//! les écarte, le RMS non). On peut s'attendre à quelques dB d'écart, dans
//! un sens ou dans l'autre — non mesuré sur un corpus.
//!
//! # Le plafond, lui, est exact
//!
//! Le gain est ensuite borné par la crête que le même rapport donne :
//!
//! ```text
//! gain approché = min(gain estimé, plafond_dBTP − crête_dBFS)
//! ```
//!
//! Sur la piste 1 du Mahler de Tades (`DR13  −4.93 дБ  −24.80 дБ`), le gain
//! estimé vaut +9,81 dB ; la crête le borne à +4,93 dB avec le plafond par
//! défaut (0 dBTP), +3,93 dB avec −1 dBTP. C'est exactement la proposition
//! du testeur, quand la piste est plus douce que la cible — et la valeur du
//! RMS quand elle est plus forte (gain négatif, la crête ne borne rien).
//!
//! La crête du rapport est une crête d'ÉCHANTILLON : elle est remise à la
//! lecture comme [`super::PeakKind::SamplePeak`], et l'anti-écrêtage
//! existant (`prevent_clipping`, réserve de crête vraie) s'applique par-dessus
//! comme pour tout pic d'échantillon tagué.
//!
//! # Où il vit, et pourquoi pas dans `rg_track_gain`
//!
//! Les deux mesures du rapport sont rangées sous une clé À ELLE,
//! [`CLE_MESURE_FOO_DR`], et le gain est calculé à la LECTURE
//! (`stored_gain_with_peak`), avec le plafond réglé à ce moment-là. Il n'est
//! jamais écrit sous `rg_track_gain`, pour trois raisons :
//!
//! 1. **une mesure réelle le remplace** : la passe d'analyse ne mesure que
//!    les pistes SANS `rg_track_gain` ; un gain approché rangé là la tiendrait
//!    à l'écart à jamais ;
//! 2. **le gain d'album** se dérive des `rg_track_gain` des pistes : une
//!    approximation n'a pas à s'y mêler ;
//! 3. **rien ne l'écrit dans un fichier** : l'éditeur de tags ne connaît que
//!    les clés `rg_*` (`tag_writer::tune_key_to_lofty`), et cette clé-ci n'y a
//!    aucune correspondance.

/// La clé de `track_metadata` qui garde la crête et le RMS lus dans le
/// rapport DR voisin. Format : `v1;<crête dBFS>;<RMS dB>`, ou `v1;-` quand le
/// rapport a été relu sans désigner la piste (la passe de rattrapage ne
/// revient alors pas dessus).
pub const CLE_MESURE_FOO_DR: &str = "foo_dr_mesure";

/// Valeur de `CLE_MESURE_FOO_DR` quand rien n'a été trouvé.
pub const MESURE_ABSENTE: &str = "v1;-";

/// Sonie d'un sinus pleine échelle (BS.1770), que le RMS du TT DR lit 0 dB.
pub const DECALAGE_RMS_VERS_LUFS_DB: f64 = 3.01;

/// Au-delà, ce n'est plus une crête plausible en dBFS (cf. `PEAK_MAX_PLAUSIBLE`
/// = +12 dBFS) ; en deçà, le rapport décrit un silence.
const CRETE_PLAUSIBLE_DB: std::ops::RangeInclusive<f64> = -120.0..=12.0;

/// Encode la mesure à ranger sous [`CLE_MESURE_FOO_DR`].
pub fn encoder(crete_db: f64, rms_db: f64) -> String {
    format!("v1;{crete_db};{rms_db}")
}

/// Relit [`CLE_MESURE_FOO_DR`] : `(crête dBFS, RMS dB)`, ou `None` pour
/// [`MESURE_ABSENTE`] et toute valeur illisible ou implausible.
pub fn decoder(brut: &str) -> Option<(f64, f64)> {
    let mut p = brut.trim().splitn(3, ';');
    if p.next()? != "v1" {
        return None;
    }
    let crete: f64 = p.next()?.trim().parse().ok()?;
    let rms: f64 = p.next()?.trim().parse().ok()?;
    let plausible = crete.is_finite()
        && rms.is_finite()
        && CRETE_PLAUSIBLE_DB.contains(&crete)
        && rms <= crete + 0.01;
    plausible.then_some((crete, rms))
}

/// Le gain approché, en dB, plafonné par la crête (voir le module).
///
/// `plafond_db` est le plafond dBTP réglé (`replaygain_true_peak_ceiling_db`,
/// borné à [−1, 0]) : `crête + gain ≤ plafond`, toujours.
pub fn gain_approche_db(crete_db: f64, rms_db: f64, plafond_db: f64) -> f64 {
    let sonie_estimee = rms_db - DECALAGE_RMS_VERS_LUFS_DB;
    let estime = super::track_gain_db(sonie_estimee);
    estime.min(plafond_db - crete_db)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'exemple de Tades (fil 2050, rapport russe réel) : crête −4,93 dB,
    /// RMS −24,80 dB. Le RMS seul demanderait +9,81 dB ; la crête borne.
    #[test]
    fn la_crete_borne_le_gain_de_tades_5519() {
        let g = gain_approche_db(-4.93, -24.80, 0.0);
        assert!((g - 4.93).abs() < 1e-9, "gain {g}");
        let g = gain_approche_db(-4.93, -24.80, -1.0);
        assert!((g - 3.93).abs() < 1e-9, "gain {g}");
    }

    /// Une piste plus forte que la cible : gain négatif, tiré du RMS, et la
    /// crête ne borne rien.
    #[test]
    fn une_piste_forte_recoit_une_attenuation_tiree_du_rms_5519() {
        let g = gain_approche_db(-0.1, -9.0, 0.0);
        assert!((g - (-18.0 + 9.0 + 3.01)).abs() < 1e-9, "gain {g}");
        assert!(g < 0.0);
    }

    #[test]
    fn l_encodage_fait_l_aller_retour_5519() {
        assert_eq!(decoder(&encoder(-4.93, -24.8)), Some((-4.93, -24.8)));
        assert_eq!(decoder(MESURE_ABSENTE), None);
        assert_eq!(decoder("v2;-4;-20"), None);
        assert_eq!(decoder("v1;nan;-20"), None);
        // Un RMS au-dessus de la crête n'existe pas : valeur rejetée.
        assert_eq!(decoder("v1;-20;-4"), None);
    }
}
