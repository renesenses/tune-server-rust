//! #7093 — publier ce que l'étage exclusif flottant fait réellement.
//! Le transport et les échantillons restent ceux de l'étage intérieur.
#[cfg(any(target_os = "macos", test))]
use super::chaine_native::FrontiereExclusive;
#[cfg(any(target_os = "macos", test))]
use super::chaine_par_la_boucle::EtageDeLaChaine;
use super::*;

pub(super) struct EtageObserve<'a> {
    pub(super) etage: EtageDeConversion<'a>,
    creneau: &'a std::sync::Mutex<Option<TransformationsReelles>>,
}

impl<'a> EtageObserve<'a> {
    pub(super) fn nouveau(
        etage: EtageDeConversion<'a>,
        creneau: &'a std::sync::Mutex<Option<TransformationsReelles>>,
    ) -> Self {
        let observe = Self { etage, creneau };
        observe.publier();
        observe
    }

    fn publier(&self) {
        publier_les_transformations(self.creneau, &self.etage);
    }
}

impl Etage for EtageObserve<'_> {
    type Puits<'p> = dyn PuitsDEchantillons + 'p;
    type Bloc = <EtageDeConversion<'static> as Etage>::Bloc;

    fn recevoir(&mut self, octets: &[u8]) {
        self.etage.recevoir(octets);
    }
    fn cadence_source(&self) -> u32 {
        self.etage.cadence_source()
    }
    fn pousser(
        &mut self,
        puits: &mut Self::Puits<'_>,
        refuser: &mut dyn FnMut(bool, u32, u16) -> bool,
        observer: &mut dyn FnMut(&Self::Bloc),
    ) -> PousseeVersLePuits {
        let resultat = self.etage.pousser(puits, refuser, observer);
        // La première sonde peut avoir établi le DoP : publier APRÈS le bloc.
        self.publier();
        resultat
    }
    fn rendre_la_queue_du_dsp(&mut self, puits: &mut Self::Puits<'_>) -> bool {
        self.etage.rendre_la_queue_du_dsp(puits)
    }
    fn vider(&mut self, puits: &mut Self::Puits<'_>) -> bool {
        self.etage.vider(puits)
    }
    fn transformations(&self) -> TransformationsReelles {
        self.etage.transformations()
    }
}

#[cfg(any(target_os = "macos", test))]
impl FrontiereExclusive for EtageObserve<'_> {
    fn spec_courante(&self) -> AudioSpec {
        self.etage.spec_courante()
    }
    fn enchainer_a_format_egal(
        &mut self,
        suivante: AudioSpec,
        amorce: &[u8],
    ) -> Result<(), enchainement_exclusif::EnchainementNatif> {
        self.etage.enchainer_a_format_egal(suivante, amorce)?;
        self.publier();
        Ok(())
    }
}

#[cfg(any(target_os = "macos", test))]
impl EtageDeLaChaine for EtageObserve<'_> {
    fn finir_la_piste(&mut self, puits: &mut Self::Puits<'_>) -> u64 {
        self.etage.finir_la_piste(puits)
    }
}
