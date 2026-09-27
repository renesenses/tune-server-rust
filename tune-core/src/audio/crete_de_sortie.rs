//! #4384 — la crête des échantillons tels qu'ils partent vers le DAC.
//!
//! Le crête-mètre d'une zone locale est alimenté par des fenêtres prélevées
//! AU DÉCODEUR (`resolve_local::transcoder_en_session`). Les gains scalaires
//! situés en aval — volume, ReplayGain, préampli — y sont déjà reportés
//! exactement (`compute_levels_avec_gain`), et le niveau MOYEN de l'égaliseur
//! et du crossfeed en approximation (#4685). Mais un égaliseur n'est pas un
//! scalaire : une bande à −12 dB sur la fréquence qui porte la crête baisse la
//! crête de 12 dB, alors que le gain moyen de la courbe bouge à peine. Le
//! convolveur, le crossfeed et le repli mono non plus ne sont pas des
//! scalaires. L'aiguille montrait donc encore autre chose que ce qui sort.
//!
//! Ce registre est tenu par la boucle producteur de la sortie locale : à
//! chaque bloc qui vient de traverser `apply_local_dsp`, elle y relève la
//! crête par canal, par tranches de [`TRANCHE_MS`], datées dans le référentiel
//! de la PISTE — le même que les fenêtres du forwarder de niveaux. Le
//! forwarder y lit la crête de la fenêtre qu'il publie et n'a plus qu'à la
//! multiplier par le gain de rendu (volume × ReplayGain × compensation), seul
//! facteur que les rappels appliquent encore après ce point.
//!
//! Aucune tranche pour une fenêtre (chemin compressé décodé d'un bloc, bras
//! exclusifs ASIO et CoreAudio, DoP, sortie non locale) : le forwarder garde
//! son calcul d'avant. Le registre ne peut donc que préciser la mesure, jamais
//! la faire disparaître.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Finesse des tranches relevées, en millisecondes. Un bloc producteur couvre
/// jusqu'à plusieurs centaines de ms ; une seule crête pour tout le bloc
/// tiendrait l'aiguille haute bien après le transitoire. 10 ms laisse quatre
/// tranches par fenêtre de 40 ms.
pub const TRANCHE_MS: u32 = 10;

/// Nombre de tranches gardées : ~10 s à [`TRANCHE_MS`]. Il en faut au moins
/// l'anneau de la sortie (deux secondes) plus la fin de la piste précédente
/// pendant un enchaînement gapless.
const CAPACITE: usize = 1024;

#[derive(Debug, Clone, Copy)]
struct Tranche {
    epoque: u64,
    debut_ms: f64,
    fin_ms: f64,
    gauche: f32,
    droite: f32,
}

#[derive(Debug, Default)]
struct Registre {
    tranches: VecDeque<Tranche>,
    epoque: u64,
}

/// Voir le module.
#[derive(Debug, Default)]
pub struct CretesDeSortie {
    registre: Mutex<Registre>,
}

impl CretesDeSortie {
    pub fn new() -> Self {
        Self::default()
    }

    fn registre(&self) -> std::sync::MutexGuard<'_, Registre> {
        self.registre.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Relève un bloc d'échantillons entrelacés, APRÈS le DSP de la sortie,
    /// qui commence à `debut_ms` dans la piste et compte `trames` trames à
    /// `cadence` Hz.
    ///
    /// Le nombre de canaux se déduit du bloc (`échantillons / trames`) : le
    /// repli mono garde deux voies, identiques. Canal 0 → gauche, canal 1 →
    /// droite, un flux mono alimente les deux.
    ///
    /// Un bloc qui recule dans la piste (saut arrière, piste enchaînée qui
    /// repart de zéro) ou qui saute en avant ouvre une nouvelle ÉPOQUE : les
    /// tranches de l'ancienne restent lisibles (la fin de la piste précédente
    /// joue encore depuis l'anneau), mais une fenêtre trouvée dans les deux
    /// est lue dans la plus récente.
    pub fn relever(&self, echantillons: &[f32], trames: u64, cadence: u32, debut_ms: f64) {
        if trames == 0 || cadence == 0 || !debut_ms.is_finite() {
            return;
        }
        let canaux = echantillons.len() / trames as usize;
        if canaux == 0 {
            return;
        }
        let par_tranche = ((u64::from(cadence) * u64::from(TRANCHE_MS)) / 1000).max(1) as usize;
        let ms_par_trame = 1000.0 / f64::from(cadence);

        let mut registre = self.registre();
        if let Some(derniere) = registre.tranches.back() {
            let ecart = debut_ms - derniere.fin_ms;
            if !(-1.0..=1000.0).contains(&ecart) {
                registre.epoque += 1;
            }
        }
        let epoque = registre.epoque;
        for (i, tranche) in echantillons.chunks(par_tranche * canaux).enumerate() {
            let (mut gauche, mut droite) = (0.0_f32, 0.0_f32);
            for trame in tranche.chunks(canaux) {
                let g = trame[0].abs();
                let d = trame.get(1).map_or(g, |s| s.abs());
                // Un NaN ne gagne jamais un `max` : il ne peut pas empoisonner
                // la crête.
                gauche = gauche.max(g);
                droite = droite.max(d);
            }
            let debut = debut_ms + (i * par_tranche) as f64 * ms_par_trame;
            let fin = debut + (tranche.len() / canaux) as f64 * ms_par_trame;
            if registre.tranches.len() == CAPACITE {
                registre.tranches.pop_front();
            }
            registre.tranches.push_back(Tranche {
                epoque,
                debut_ms: debut,
                fin_ms: fin,
                gauche,
                droite,
            });
        }
    }

    /// Crête linéaire `(gauche, droite)` relevée sur `[debut_ms, fin_ms)` de
    /// la piste, dans l'époque la plus récente qui couvre cet intervalle.
    /// `None` quand aucune tranche ne le touche.
    pub fn crete_entre(&self, debut_ms: f64, fin_ms: f64) -> Option<(f64, f64)> {
        let registre = self.registre();
        let mut epoque = None;
        let (mut gauche, mut droite) = (0.0_f32, 0.0_f32);
        for t in registre.tranches.iter().rev() {
            if t.fin_ms <= debut_ms || t.debut_ms >= fin_ms {
                continue;
            }
            match epoque {
                None => epoque = Some(t.epoque),
                Some(e) if e != t.epoque => continue,
                Some(_) => {}
            }
            gauche = gauche.max(t.gauche);
            droite = droite.max(t.droite);
        }
        epoque.map(|_| (f64::from(gauche), f64::from(droite)))
    }

    /// Oublie tout — une sortie qui change de zone ne doit pas prêter les
    /// crêtes d'une autre lecture.
    pub fn oublier(&self) {
        let mut registre = self.registre();
        registre.tranches.clear();
        registre.epoque += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sinus(amplitude: f32, trames: usize) -> Vec<f32> {
        (0..trames)
            .flat_map(|i| {
                let s = amplitude * (i as f32 * 0.3).sin();
                [s, s * 0.5]
            })
            .collect()
    }

    #[test]
    fn la_crete_d_une_fenetre_est_celle_de_ses_tranches() {
        let c = CretesDeSortie::new();
        // 100 ms à 48 kHz, crête 0,25 à gauche, 0,125 à droite.
        c.relever(&sinus(0.25, 4800), 4800, 48_000, 1000.0);
        let (g, d) = c.crete_entre(1040.0, 1080.0).expect("fenêtre couverte");
        assert!((g - 0.25).abs() < 0.01, "gauche {g}");
        assert!((d - 0.125).abs() < 0.01, "droite {d}");
        assert!(c.crete_entre(0.0, 900.0).is_none(), "hors relevé ⇒ None");
    }

    #[test]
    fn un_transitoire_ne_deborde_pas_sur_les_fenetres_voisines() {
        let c = CretesDeSortie::new();
        let mut bloc = vec![0.01_f32; 4800 * 2];
        // Un clic plein échelle à 90 ms.
        bloc[4320 * 2] = 1.0;
        c.relever(&bloc, 4800, 48_000, 0.0);
        let (avant, _) = c.crete_entre(0.0, 40.0).unwrap();
        let (pendant, _) = c.crete_entre(80.0, 100.0).unwrap();
        assert!(
            avant < 0.02,
            "le clic de 90 ms ne doit pas monter la fenêtre 0-40 ms : {avant}"
        );
        assert!(pendant > 0.99);
    }

    #[test]
    fn apres_un_retour_en_arriere_la_lecture_la_plus_recente_gagne() {
        let c = CretesDeSortie::new();
        c.relever(&sinus(0.9, 4800), 4800, 48_000, 0.0);
        // Piste enchaînée (ou saut arrière) : on repart de zéro, plus bas.
        c.relever(&sinus(0.1, 4800), 4800, 48_000, 0.0);
        let (g, _) = c.crete_entre(20.0, 60.0).unwrap();
        assert!((g - 0.1).abs() < 0.01, "époque récente attendue, lu {g}");
    }

    #[test]
    fn oublier_vide_le_registre() {
        let c = CretesDeSortie::new();
        c.relever(&sinus(0.5, 480), 480, 48_000, 0.0);
        c.oublier();
        assert!(c.crete_entre(0.0, 10.0).is_none());
    }
}
