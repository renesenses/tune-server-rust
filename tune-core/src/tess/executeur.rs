//! L'exécuteur TESS (lot T03) : la chaîne d'étages en `f64`, de bout en bout.
//!
//! - **Ordre libre** : l'exécuteur appelle les étages dans l'ordre de la
//!   tranche qu'on lui donne, sans en connaître aucun. L'ordre vient de la
//!   chaîne de la zone (T02) ; les contraintes de place (`last_plugin`,
//!   `stereo_only`, groupe exclusif) sont vérifiées en amont par
//!   [`ChaineTess::valider`](super::ChaineTess::valider) et la résolution.
//! - **Aucune requantification** entre deux étages : les tampons sont des
//!   `[f64]` et l'exécuteur ne les convertit jamais. La seule quantification
//!   est l'[`EtapeFinale`](super::EtapeFinale) (voir
//!   [`super::quantification`]).
//! - **Aucune allocation dans le fil audio** : [`Executeur::preparer`]
//!   (hors temps réel) propage format et taille de bloc
//!   ([`preparer_chaine`](super::preparer_chaine)) et préalloue un tampon de
//!   sortie pour chaque étage qui ne travaille pas sur place ;
//!   [`Executeur::traiter`], [`Executeur::vidanger`] et
//!   [`Executeur::reinitialiser`] n'allouent jamais. Une préparation qui tient
//!   dans les capacités déjà réservées n'alloue pas non plus.
//! - **« Bypass all »** : la chaîne est préparée SANS ses étages (contrat § 1 :
//!   aucun retard, bit-perfect) ; [`Executeur::traiter`] rend alors l'entrée
//!   intacte, à l'octet.
//!
//! L'exécuteur n'est pas propriétaire des étages : on les lui prête à chaque
//! appel (`&mut [&mut dyn EtageTess]`), dans le même ordre qu'à la
//! préparation. La sortie locale garde ainsi ses moteurs historiques sous
//! leurs verrous (adaptateurs provisoires, `outputs/local/executeur_tess.rs`)
//! tant que T05, T06 et T09b ne les ont pas portés en `f64`.

use super::etage::{
    Disposition, ErreurEtage, EtageTess, FormatFlux, preparer_dans, verifier_tampon,
};

/// Garde-fou de [`Executeur::vidanger`] : un étage qui rendrait une queue
/// sans fin ne bloque pas le fil.
const MAX_TOURS_DE_VIDANGE: usize = 4096;

/// Le résultat d'un passage dans la chaîne.
#[derive(Debug)]
pub struct SortieChaine<'a> {
    /// Les échantillons entrelacés, `trames × format.channels`.
    pub echantillons: &'a [f64],
    pub trames: usize,
    pub format: FormatFlux,
}

/// L'exécuteur d'une chaîne TESS. Voir le module.
#[derive(Debug, Default)]
pub struct Executeur {
    /// `None` tant que rien n'est préparé.
    entree: Option<FormatFlux>,
    sortie: Option<FormatFlux>,
    max_trames: usize,
    max_trames_sortie: usize,
    bypass_all: bool,
    nb_etages: usize,
    dispositions: Vec<Disposition>,
    /// Un tampon par étage ; vide pour un étage sur place.
    tampons: Vec<Vec<f64>>,
    /// Reçoit la queue d'un étage pendant la vidange.
    tampon_vidange: Vec<f64>,
    latence_totale_trames: u64,
}

impl Executeur {
    pub fn new() -> Self {
        Self::default()
    }

    /// HORS TEMPS RÉEL. Prépare la chaîne `etages`, dans cet ordre, pour un
    /// flux `entree` en blocs d'au plus `max_trames` trames.
    ///
    /// Avec `bypass_all`, aucun étage n'est préparé ni appelé ensuite : la
    /// chaîne est reconstruite sans eux (contrat § 1).
    pub fn preparer(
        &mut self,
        etages: &mut [&mut dyn EtageTess],
        entree: FormatFlux,
        max_trames: usize,
        bypass_all: bool,
    ) -> Result<(), ErreurEtage> {
        self.entree = None;
        self.bypass_all = bypass_all;
        self.max_trames = max_trames;
        if bypass_all {
            self.dispositions.clear();
            self.nb_etages = 0;
            self.sortie = Some(entree);
            self.max_trames_sortie = max_trames;
            self.latence_totale_trames = 0;
            self.entree = Some(entree);
            return Ok(());
        }
        let (sortie, max_sortie, latence) =
            preparer_dans(etages, entree, max_trames, &mut self.dispositions)?;
        let n = etages.len();
        if self.tampons.len() < n {
            self.tampons.resize_with(n, Vec::new);
        }
        // Tampon de vidange : seulement pour un étage à latence (seul un tel
        // étage a une queue, contrat § 1), et de la taille utile — sa queue,
        // bornée par ce que l'étage suivant accepte d'un coup.
        let mut vidange = 0;
        for (tampon, d) in self.tampons.iter_mut().zip(&self.dispositions) {
            let capacite = d.capacite_sortie();
            if !d.sur_place && tampon.len() != capacite {
                tampon.resize(capacite, 0.0);
            }
            if d.latence_trames > 0 {
                let trames = d.max_trames_sortie.min(d.latence_trames as usize);
                vidange = vidange.max(trames * usize::from(d.sortie.channels));
            }
        }
        if self.tampon_vidange.len() != vidange {
            self.tampon_vidange.resize(vidange, 0.0);
            self.tampon_vidange.shrink_to(vidange);
        }
        self.nb_etages = n;
        self.sortie = Some(sortie);
        self.max_trames_sortie = max_sortie;
        self.latence_totale_trames = latence;
        self.entree = Some(entree);
        Ok(())
    }

    pub fn est_prepare(&self) -> bool {
        self.entree.is_some()
    }

    pub fn bypass_all(&self) -> bool {
        self.bypass_all
    }

    /// Format de sortie de la chaîne préparée.
    pub fn format_sortie(&self) -> Option<FormatFlux> {
        self.sortie
    }

    /// Latence totale, en trames à la fréquence de sortie (0 en « Bypass all »).
    pub fn latence_totale_trames(&self) -> u64 {
        self.latence_totale_trames
    }

    /// Les dispositions des étages préparés (vide en « Bypass all »).
    pub fn dispositions(&self) -> &[Disposition] {
        &self.dispositions[..self.nb_etages]
    }

    fn verifier_etages(&self, etages: &[&mut dyn EtageTess]) -> Result<FormatFlux, ErreurEtage> {
        let entree = self.entree.ok_or(ErreurEtage::NonPrepare)?;
        if !self.bypass_all && etages.len() != self.nb_etages {
            return Err(ErreurEtage::NonPrepare);
        }
        Ok(entree)
    }

    /// TEMPS RÉEL. Fait passer `trames` trames de `tampon` dans la chaîne.
    ///
    /// Les étages sur place travaillent dans `tampon` (ou dans le tampon de
    /// l'étage précédent qui a changé de format) ; les autres écrivent dans
    /// leur tampon préalloué. N'alloue jamais.
    pub fn traiter<'a>(
        &'a mut self,
        etages: &mut [&mut dyn EtageTess],
        tampon: &'a mut [f64],
        trames: usize,
    ) -> Result<SortieChaine<'a>, ErreurEtage> {
        let entree = self.verifier_etages(etages)?;
        if trames > self.max_trames {
            return Err(ErreurEtage::BlocTropGrand {
                trames,
                max: self.max_trames,
            });
        }
        verifier_tampon(tampon.len(), trames, entree.channels)?;
        let format = self.sortie.unwrap_or(entree);
        if self.bypass_all {
            let n = trames * usize::from(entree.channels);
            return Ok(SortieChaine {
                echantillons: &tampon[..n],
                trames,
                format,
            });
        }
        let nb = self.nb_etages;
        let (ou, trames) = executer_suite(
            &mut etages[..nb],
            &self.dispositions[..nb],
            &mut self.tampons[..nb],
            0,
            tampon,
            trames,
        )?;
        let n = trames * usize::from(format.channels);
        let echantillons: &'a [f64] = match ou {
            None => &tampon[..n],
            Some(k) => &self.tampons[k][..n],
        };
        Ok(SortieChaine {
            echantillons,
            trames,
            format,
        })
    }

    /// TEMPS RÉEL. Fin de flux : rend la queue de chaque étage à latence,
    /// passée par les étages qui le SUIVENT dans la chaîne, et la remet à
    /// `emettre` morceau par morceau (au format de sortie de la chaîne).
    /// N'alloue jamais. Rend le nombre total de trames émises.
    pub fn vidanger(
        &mut self,
        etages: &mut [&mut dyn EtageTess],
        emettre: &mut dyn FnMut(&[f64]),
    ) -> Result<usize, ErreurEtage> {
        self.verifier_etages(etages)?;
        if self.bypass_all {
            return Ok(0);
        }
        let nb = self.nb_etages;
        let canaux_sortie = usize::from(self.sortie.map_or(0, |f| f.channels));
        // Prendre le tampon de vidange hors de `self` (sans allouer : un `Vec`
        // vide ne réserve rien) pour le prêter à côté des tampons d'étage.
        let mut queue = std::mem::take(&mut self.tampon_vidange);
        let mut total = 0;
        let mut resultat = Ok(());
        'etages: for i in 0..nb {
            let canaux = usize::from(self.dispositions[i].sortie.channels);
            let d = self.dispositions[i];
            if d.latence_trames == 0 {
                continue;
            }
            let capacite = (d.max_trames_sortie.min(d.latence_trames as usize)
                * usize::from(d.sortie.channels))
            .min(queue.len());
            for _ in 0..MAX_TOURS_DE_VIDANGE {
                let ecrites = match etages[i].vidanger(&mut queue[..capacite]) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        resultat = Err(e);
                        break 'etages;
                    }
                };
                let suite = executer_suite(
                    &mut etages[..nb],
                    &self.dispositions[..nb],
                    &mut self.tampons[..nb],
                    i + 1,
                    &mut queue[..ecrites * canaux],
                    ecrites,
                );
                match suite {
                    Ok((ou, trames)) => {
                        let n = trames * canaux_sortie;
                        match ou {
                            None => emettre(&queue[..n]),
                            Some(k) => emettre(&self.tampons[k][..n]),
                        }
                        total += trames;
                    }
                    Err(e) => {
                        resultat = Err(e);
                        break 'etages;
                    }
                }
            }
        }
        self.tampon_vidange = queue;
        resultat.map(|()| total)
    }

    /// Vide les mémoires des étages (saut, piste non gapless). N'alloue pas.
    pub fn reinitialiser(&mut self, etages: &mut [&mut dyn EtageTess]) {
        if self.bypass_all {
            return;
        }
        for etage in etages.iter_mut().take(self.nb_etages) {
            etage.reinitialiser();
        }
    }
}

/// Fait passer un bloc dans les étages `debut..` (déjà préparés). Rend où se
/// trouve la sortie (`None` : dans `tampon` ; `Some(k)` : dans le tampon de
/// l'étage `k`) et le nombre de trames. N'alloue jamais.
fn executer_suite(
    etages: &mut [&mut dyn EtageTess],
    dispositions: &[Disposition],
    tampons: &mut [Vec<f64>],
    debut: usize,
    tampon: &mut [f64],
    trames: usize,
) -> Result<(Option<usize>, usize), ErreurEtage> {
    let mut ou: Option<usize> = None;
    let mut trames = trames;
    for i in debut..etages.len() {
        let d = dispositions[i];
        if d.sur_place {
            match ou {
                None => etages[i].traiter_sur_place(tampon, trames)?,
                Some(k) => etages[i].traiter_sur_place(&mut tampons[k], trames)?,
            }
        } else {
            let (avant, apres) = tampons.split_at_mut(i);
            let sortie = &mut apres[0];
            let entree: &[f64] = match ou {
                None => tampon,
                Some(k) => &avant[k],
            };
            let rendues = etages[i].traiter_vers(entree, trames, sortie)?;
            if rendues > d.max_trames_sortie {
                return Err(ErreurEtage::BlocTropGrand {
                    trames: rendues,
                    max: d.max_trames_sortie,
                });
            }
            trames = rendues;
            ou = Some(i);
        }
    }
    Ok((ou, trames))
}

#[cfg(test)]
pub(crate) mod etages_de_test {
    //! Étages réels de test, en `f64` natif, partagés par les preuves de
    //! l'exécuteur et celles de la sortie locale.
    use super::*;

    /// Biquad en forme transposée directe II, `f64` de bout en bout.
    pub struct Biquad {
        pub b: [f64; 3],
        pub a: [f64; 2],
        etat: Vec<[f64; 2]>,
        canaux: usize,
        contourne: bool,
    }

    impl Biquad {
        pub fn new(b: [f64; 3], a: [f64; 2]) -> Self {
            Self {
                b,
                a,
                etat: Vec::new(),
                canaux: 0,
                contourne: false,
            }
        }

        /// Passe-bas RBJ (fréquence de coupure relative `f`, Q 0,707).
        pub fn passe_bas(f: f64) -> Self {
            let w = 2.0 * std::f64::consts::PI * f;
            let alpha = w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
            let cos = w.cos();
            let a0 = 1.0 + alpha;
            Self::new(
                [
                    (1.0 - cos) / 2.0 / a0,
                    (1.0 - cos) / a0,
                    (1.0 - cos) / 2.0 / a0,
                ],
                [-2.0 * cos / a0, (1.0 - alpha) / a0],
            )
        }

        /// Cloche RBJ (+`db` dB à la fréquence relative `f`, Q 1).
        pub fn cloche(f: f64, db: f64) -> Self {
            let a = 10f64.powf(db / 40.0);
            let w = 2.0 * std::f64::consts::PI * f;
            let alpha = w.sin() / 2.0;
            let cos = w.cos();
            let a0 = 1.0 + alpha / a;
            Self::new(
                [
                    (1.0 + alpha * a) / a0,
                    -2.0 * cos / a0,
                    (1.0 - alpha * a) / a0,
                ],
                [-2.0 * cos / a0, (1.0 - alpha / a) / a0],
            )
        }
    }

    impl EtageTess for Biquad {
        fn id(&self) -> &str {
            "biquad"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            self.canaux = usize::from(e.channels);
            self.etat = vec![[0.0; 2]; self.canaux];
            Ok(Disposition::sur_place(e, max, 0))
        }
        fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
            verifier_tampon(t.len(), trames, self.canaux as u16)?;
            if self.contourne {
                return Ok(());
            }
            let [b0, b1, b2] = self.b;
            let [a1, a2] = self.a;
            for trame in t[..trames * self.canaux].chunks_exact_mut(self.canaux) {
                for (x, z) in trame.iter_mut().zip(self.etat.iter_mut()) {
                    let y = b0 * *x + z[0];
                    z[0] = b1 * *x - a1 * y + z[1];
                    z[1] = b2 * *x - a2 * y;
                    *x = y;
                }
            }
            Ok(())
        }
        fn reinitialiser(&mut self) {
            self.etat.iter_mut().for_each(|z| *z = [0.0; 2]);
        }
        fn bypass(&self) -> bool {
            self.contourne
        }
        fn definir_bypass(&mut self, b: bool) {
            self.contourne = b;
        }
    }

    /// Saturation douce (`tanh(k·x)/tanh(k)`) : un étage NON LINÉAIRE, pour
    /// prouver que l'ordre des étages est réellement celui de la chaîne.
    pub struct Saturation {
        pub k: f64,
        canaux: usize,
    }

    impl Saturation {
        pub fn new(k: f64) -> Self {
            Self { k, canaux: 0 }
        }
    }

    impl EtageTess for Saturation {
        fn id(&self) -> &str {
            "saturation"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            self.canaux = usize::from(e.channels);
            Ok(Disposition::sur_place(e, max, 0))
        }
        fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
            verifier_tampon(t.len(), trames, self.canaux as u16)?;
            let norme = self.k.tanh();
            for x in &mut t[..trames * self.canaux] {
                *x = (self.k * *x).tanh() / norme;
            }
            Ok(())
        }
        fn bypass(&self) -> bool {
            false
        }
        fn definir_bypass(&mut self, _: bool) {}
    }

    /// Ce que l'ancien chemin faisait entre deux étages : ranger les
    /// échantillons dans un tampon `f32`. Sert de CONTRE-ÉPREUVE.
    pub struct FrontiereF32 {
        canaux: usize,
    }

    impl FrontiereF32 {
        pub fn new() -> Self {
            Self { canaux: 0 }
        }
    }

    impl EtageTess for FrontiereF32 {
        fn id(&self) -> &str {
            "frontiere-f32"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            self.canaux = usize::from(e.channels);
            Ok(Disposition::sur_place(e, max, 0))
        }
        fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
            for x in &mut t[..trames * self.canaux] {
                *x = f64::from(*x as f32);
            }
            Ok(())
        }
        fn bypass(&self) -> bool {
            false
        }
        fn definir_bypass(&mut self, _: bool) {}
    }

    /// Sonde : compte les échantillons reçus qui portent plus de précision
    /// qu'un `f32` (preuve qu'aucune requantification n'a eu lieu avant elle).
    pub struct Sonde {
        canaux: usize,
        pub vus: usize,
        pub plus_fins_qu_un_f32: usize,
    }

    impl Sonde {
        pub fn new() -> Self {
            Self {
                canaux: 0,
                vus: 0,
                plus_fins_qu_un_f32: 0,
            }
        }
    }

    impl EtageTess for Sonde {
        fn id(&self) -> &str {
            "sonde"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            self.canaux = usize::from(e.channels);
            Ok(Disposition::sur_place(e, max, 0))
        }
        fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
            for x in &t[..trames * self.canaux] {
                self.vus += 1;
                if f64::from(*x as f32) != *x {
                    self.plus_fins_qu_un_f32 += 1;
                }
            }
            Ok(())
        }
        fn bypass(&self) -> bool {
            false
        }
        fn definir_bypass(&mut self, _: bool) {}
    }

    /// Ligne à retard de `n` trames (étage à latence), sur place, dont la
    /// queue se vidange par morceaux.
    pub struct Retard {
        n: usize,
        ligne: Vec<f64>,
        pos: usize,
        a_vidanger: usize,
        canaux: usize,
    }

    impl Retard {
        pub fn new(n: usize) -> Self {
            Self {
                n,
                ligne: Vec::new(),
                pos: 0,
                a_vidanger: 0,
                canaux: 0,
            }
        }
    }

    impl EtageTess for Retard {
        fn id(&self) -> &str {
            "retard"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            self.canaux = usize::from(e.channels);
            self.ligne = vec![0.0; self.n * self.canaux];
            self.pos = 0;
            self.a_vidanger = self.n;
            Ok(Disposition::sur_place(e, max, self.n as u32))
        }
        fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
            verifier_tampon(t.len(), trames, self.canaux as u16)?;
            if self.n == 0 {
                return Ok(());
            }
            for x in &mut t[..trames * self.canaux] {
                std::mem::swap(x, &mut self.ligne[self.pos]);
                self.pos = (self.pos + 1) % self.ligne.len();
            }
            Ok(())
        }
        fn vidanger(&mut self, sortie: &mut [f64]) -> Result<usize, ErreurEtage> {
            let place = sortie.len() / self.canaux.max(1);
            let trames = self.a_vidanger.min(place);
            for x in &mut sortie[..trames * self.canaux] {
                *x = std::mem::take(&mut self.ligne[self.pos]);
                self.pos = (self.pos + 1) % self.ligne.len();
            }
            self.a_vidanger -= trames;
            Ok(trames)
        }
        fn reinitialiser(&mut self) {
            self.ligne.iter_mut().for_each(|x| *x = 0.0);
            self.a_vidanger = self.n;
        }
        fn bypass(&self) -> bool {
            false
        }
        fn definir_bypass(&mut self, _: bool) {}
    }

    /// ×2 en cadence par répétition, tampon de sortie distinct.
    pub struct Doubleur {
        canaux: usize,
    }

    impl Doubleur {
        pub fn new() -> Self {
            Self { canaux: 0 }
        }
    }

    impl EtageTess for Doubleur {
        fn id(&self) -> &str {
            "doubleur"
        }
        fn preparer(&mut self, e: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
            self.canaux = usize::from(e.channels);
            Ok(Disposition {
                sortie: FormatFlux {
                    sample_rate_hz: e.sample_rate_hz * 2,
                    ..e
                },
                max_trames_sortie: max * 2,
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
            let c = self.canaux;
            verifier_tampon(entree.len(), trames, c as u16)?;
            verifier_tampon(sortie.len(), trames * 2, c as u16)?;
            for (i, trame) in entree[..trames * c].chunks_exact(c).enumerate() {
                sortie[2 * i * c..(2 * i + 1) * c].copy_from_slice(trame);
                sortie[(2 * i + 1) * c..(2 * i + 2) * c].copy_from_slice(trame);
            }
            Ok(trames * 2)
        }
        fn bypass(&self) -> bool {
            false
        }
        fn definir_bypass(&mut self, _: bool) {}
    }

    /// Signal de test déterministe : PCM 24 bits (sinus + bruit), en mots.
    pub fn pcm_24_bits(trames: usize, canaux: usize) -> Vec<i32> {
        let mut graine: u64 = 0x5445_5353_0003;
        (0..trames * canaux)
            .map(|i| {
                graine ^= graine << 13;
                graine ^= graine >> 7;
                graine ^= graine << 17;
                let bruit = (graine >> 40) as f64 / (1u64 << 24) as f64 - 0.5;
                let t = (i / canaux) as f64;
                let x = 0.6 * (t * 0.031 + (i % canaux) as f64).sin() + 0.05 * bruit;
                (x * 8_388_607.0).round() as i32
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::etages_de_test::*;
    use super::*;

    const STEREO_48K: FormatFlux = FormatFlux {
        channels: 2,
        sample_rate_hz: 48_000,
    };

    /// Fait passer `source` par la chaîne en blocs de `bloc` trames.
    fn par_blocs(
        ex: &mut Executeur,
        etages: &mut [&mut dyn EtageTess],
        source: &[f64],
        canaux: usize,
        bloc: usize,
    ) -> Vec<f64> {
        let mut sortie = Vec::new();
        for morceau in source.chunks(bloc * canaux) {
            let mut t = morceau.to_vec();
            let trames = t.len() / canaux;
            let s = ex.traiter(etages, &mut t, trames).unwrap();
            sortie.extend_from_slice(s.echantillons);
        }
        sortie
    }

    /// Référence HORS LIGNE en double précision : biquad en forme directe I
    /// (une autre écriture que celle de l'étage), piste entière d'un coup.
    fn biquad_hors_ligne(x: &[f64], canaux: usize, b: [f64; 3], a: [f64; 2]) -> Vec<f64> {
        let mut y = vec![0.0; x.len()];
        for c in 0..canaux {
            let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
            for i in (c..x.len()).step_by(canaux) {
                let v = b[0] * x[i] + b[1] * x1 + b[2] * x2 - a[0] * y1 - a[1] * y2;
                x2 = x1;
                x1 = x[i];
                y2 = y1;
                y1 = v;
                y[i] = v;
            }
        }
        y
    }

    fn ecart_max(a: &[f64], b: &[f64]) -> f64 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f64::max)
    }

    /// « 64 bits réels » (critère du plan § 1.1) : la chaîne exécutée colle à
    /// une référence calculée hors ligne en double précision à moins de 1e-12
    /// (≈ −240 dBFS). CONTRE-ÉPREUVE : le même banc, avec ce que faisait
    /// l'ancien chemin (des tampons `f32` entre les étages), échoue.
    #[test]
    fn soixante_quatre_bits_reels_contre_une_reference_hors_ligne() {
        let pcm = pcm_24_bits(20_000, 2);
        let source: Vec<f64> = pcm.iter().map(|&m| f64::from(m) / 8_388_608.0).collect();
        let (p, c) = (Biquad::passe_bas(0.05), Biquad::cloche(0.01, 6.0));
        let reference = biquad_hors_ligne(&biquad_hors_ligne(&source, 2, p.b, p.a), 2, c.b, c.a);

        let mut ex = Executeur::new();
        let (mut e1, mut e2) = (Biquad::passe_bas(0.05), Biquad::cloche(0.01, 6.0));
        ex.preparer(&mut [&mut e1, &mut e2], STEREO_48K, 512, false)
            .unwrap();
        let sortie = par_blocs(&mut ex, &mut [&mut e1, &mut e2], &source, 2, 512);
        let ecart = ecart_max(&sortie, &reference);
        assert!(
            ecart < 1e-12,
            "écart {ecart:e} : la chaîne n'est pas en 64 bits"
        );

        // Contre-épreuve : l'ancien chemin rangeait l'entrée et chaque sortie
        // d'étage dans un `f32`.
        let mut ex = Executeur::new();
        let (mut e1, mut e2) = (Biquad::passe_bas(0.05), Biquad::cloche(0.01, 6.0));
        let (mut f0, mut f1, mut f2) = (
            FrontiereF32::new(),
            FrontiereF32::new(),
            FrontiereF32::new(),
        );
        let mut ancien: [&mut dyn EtageTess; 5] = [&mut f0, &mut e1, &mut f1, &mut e2, &mut f2];
        ex.preparer(&mut ancien, STEREO_48K, 512, false).unwrap();
        let sortie = par_blocs(&mut ex, &mut ancien, &source, 2, 512);
        let ecart_f32 = ecart_max(&sortie, &reference);
        assert!(
            ecart_f32 >= 1e-12,
            "le banc ne discrimine pas : l'ancien chemin f32 passe ({ecart_f32:e})"
        );
        assert!(
            ecart_f32 > 1e-9 && ecart_f32 < 1e-5,
            "ordre de grandeur f32 : {ecart_f32:e}"
        );
    }

    /// Ordre libre : deux étages qui ne commutent pas (un filtre et une
    /// saturation) donnent deux sorties différentes selon leur ordre, chacune
    /// égale à sa propre composition hors ligne.
    #[test]
    fn l_ordre_des_etages_est_celui_de_la_chaine() {
        let pcm = pcm_24_bits(4_096, 2);
        let source: Vec<f64> = pcm.iter().map(|&m| f64::from(m) / 8_388_608.0).collect();
        let b = Biquad::cloche(0.02, 9.0);
        let sat =
            |v: &[f64]| -> Vec<f64> { v.iter().map(|x| (3.0 * x).tanh() / 3f64.tanh()).collect() };

        let mut ex = Executeur::new();
        let (mut q, mut s) = (Biquad::cloche(0.02, 9.0), Saturation::new(3.0));
        ex.preparer(&mut [&mut q, &mut s], STEREO_48K, 256, false)
            .unwrap();
        let filtre_puis_sat = par_blocs(&mut ex, &mut [&mut q, &mut s], &source, 2, 256);

        let (mut q, mut s) = (Biquad::cloche(0.02, 9.0), Saturation::new(3.0));
        ex.preparer(&mut [&mut s, &mut q], STEREO_48K, 256, false)
            .unwrap();
        let sat_puis_filtre = par_blocs(&mut ex, &mut [&mut s, &mut q], &source, 2, 256);

        assert!(ecart_max(&filtre_puis_sat, &sat_puis_filtre) > 1e-3);
        assert!(
            ecart_max(
                &filtre_puis_sat,
                &sat(&biquad_hors_ligne(&source, 2, b.b, b.a))
            ) < 1e-12
        );
        assert!(
            ecart_max(
                &sat_puis_filtre,
                &biquad_hors_ligne(&sat(&source), 2, b.b, b.a)
            ) < 1e-12
        );
    }

    /// « Bypass all » : aucun étage n'est appelé, la sortie est l'entrée à
    /// l'octet, sans retard. Contre-épreuve : la même chaîne active modifie.
    #[test]
    fn bypass_all_rend_l_entree_a_l_octet_sans_retard() {
        let pcm = pcm_24_bits(1_000, 2);
        let source: Vec<f64> = pcm.iter().map(|&m| f64::from(m) / 8_388_608.0).collect();
        let mut ex = Executeur::new();
        let (mut r, mut q) = (Retard::new(64), Biquad::cloche(0.02, 9.0));
        ex.preparer(&mut [&mut r, &mut q], STEREO_48K, 1_000, true)
            .unwrap();
        assert_eq!(ex.latence_totale_trames(), 0);
        let mut t = source.clone();
        let s = ex.traiter(&mut [&mut r, &mut q], &mut t, 1_000).unwrap();
        let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(s.echantillons), bits(&source));
        let mut queue = 0;
        assert_eq!(
            ex.vidanger(&mut [&mut r, &mut q], &mut |q| queue += q.len()),
            Ok(0)
        );
        assert_eq!(queue, 0);

        ex.preparer(&mut [&mut r, &mut q], STEREO_48K, 1_000, false)
            .unwrap();
        assert_eq!(ex.latence_totale_trames(), 64);
        let mut t = source.clone();
        let s = ex.traiter(&mut [&mut r, &mut q], &mut t, 1_000).unwrap();
        assert_ne!(bits(s.echantillons), bits(&source));
    }

    /// Aucune requantification entre deux étages : une sonde placée après un
    /// étage voit des échantillons plus fins qu'un `f32`. Contre-épreuve :
    /// une frontière `f32` intercalée, et la sonde ne voit plus que des `f32`.
    #[test]
    fn aucune_requantification_entre_deux_etages() {
        let pcm = pcm_24_bits(2_048, 2);
        let source: Vec<f64> = pcm.iter().map(|&m| f64::from(m) / 8_388_608.0).collect();
        let mut ex = Executeur::new();
        let (mut q, mut sonde) = (Biquad::cloche(0.02, 3.0), Sonde::new());
        ex.preparer(&mut [&mut q, &mut sonde], STEREO_48K, 2_048, false)
            .unwrap();
        let mut t = source.clone();
        ex.traiter(&mut [&mut q, &mut sonde], &mut t, 2_048)
            .unwrap();
        assert_eq!(sonde.vus, 4_096);
        assert!(
            sonde.plus_fins_qu_un_f32 > 4_000,
            "{}",
            sonde.plus_fins_qu_un_f32
        );

        let (mut q, mut f, mut sonde) =
            (Biquad::cloche(0.02, 3.0), FrontiereF32::new(), Sonde::new());
        ex.preparer(&mut [&mut q, &mut f, &mut sonde], STEREO_48K, 2_048, false)
            .unwrap();
        let mut t = source.clone();
        ex.traiter(&mut [&mut q, &mut f, &mut sonde], &mut t, 2_048)
            .unwrap();
        assert_eq!(
            sonde.plus_fins_qu_un_f32, 0,
            "la contre-épreuve doit être vue"
        );
    }

    /// Un étage qui change de cadence écrit dans son tampon préalloué ; les
    /// étages qui le suivent travaillent sur place dans ce tampon ; la vidange
    /// rend la queue d'un retard PASSÉE par les étages qui le suivent.
    #[test]
    fn changement_de_format_et_vidange_par_les_etages_suivants() {
        let mut ex = Executeur::new();
        let (mut r, mut d, mut g) = (Retard::new(3), Doubleur::new(), Saturation::new(1e-9));
        let mut etages: [&mut dyn EtageTess; 3] = [&mut r, &mut d, &mut g];
        let mono = FormatFlux {
            channels: 1,
            sample_rate_hz: 44_100,
        };
        ex.preparer(&mut etages, mono, 4, false).unwrap();
        assert_eq!(
            ex.format_sortie(),
            Some(FormatFlux {
                channels: 1,
                sample_rate_hz: 88_200
            })
        );
        assert_eq!(ex.latence_totale_trames(), 6);
        let mut t = [0.1, 0.2, 0.3, 0.4];
        let s = ex.traiter(&mut etages, &mut t, 4).unwrap();
        assert_eq!(s.trames, 8);
        let arrondi = |v: &[f64]| {
            v.iter()
                .map(|x| (x * 1e6).round() / 1e6)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            arrondi(s.echantillons),
            [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.1, 0.1]
        );
        // Bloc trop grand : refus explicite, pas de panique.
        let mut t = [0.0; 6];
        assert_eq!(
            ex.traiter(&mut etages, &mut t, 5).unwrap_err(),
            ErreurEtage::BlocTropGrand { trames: 5, max: 4 }
        );
        let mut queue = Vec::new();
        let n = ex
            .vidanger(&mut etages, &mut |q| queue.extend_from_slice(q))
            .unwrap();
        assert_eq!(n, 6);
        assert_eq!(arrondi(&queue), [0.2, 0.2, 0.3, 0.3, 0.4, 0.4]);
        // Étages prêtés en nombre différent de la préparation : refus.
        assert_eq!(
            ex.traiter(&mut etages[..2], &mut [0.0; 4], 4).unwrap_err(),
            ErreurEtage::NonPrepare
        );
    }

    /// Mémoire : sans étage à latence, aucun tampon de vidange, même pour un
    /// bloc géant ; avec une latence, la taille de la queue et rien de plus.
    #[test]
    fn le_tampon_de_vidange_n_existe_que_pour_une_queue_et_a_sa_taille() {
        let mut ex = Executeur::new();
        let (mut q, mut s) = (Biquad::cloche(0.02, 3.0), Saturation::new(2.0));
        ex.preparer(&mut [&mut q, &mut s], STEREO_48K, 10_000_000, false)
            .unwrap();
        assert_eq!(ex.tampon_vidange.capacity(), 0);
        let mut r = Retard::new(64);
        ex.preparer(&mut [&mut q, &mut r], STEREO_48K, 10_000_000, false)
            .unwrap();
        assert_eq!(ex.tampon_vidange.len(), 64 * 2);
        assert!(ex.tampon_vidange.capacity() < 10_000);
        ex.preparer(&mut [&mut q, &mut s], STEREO_48K, 512, false)
            .unwrap();
        assert_eq!(ex.tampon_vidange.len(), 0);
    }

    #[test]
    fn un_executeur_non_prepare_refuse_sans_paniquer() {
        let mut ex = Executeur::new();
        assert_eq!(
            ex.traiter(&mut [], &mut [0.0; 2], 1).unwrap_err(),
            ErreurEtage::NonPrepare
        );
    }
}
