//! Greffon de banc. Il duplique chaque trame et quantifie sur une grille fixe.
use tune_plugin_abi::finish::{FinalPcm, FinalPcmFactory};
use tune_plugin_sdk::{Error, Settings, audio::*, rate::*};

pub struct Fabrique;
struct Rendu {
    requete: RateRequest,
    profil: String,
}
struct Grille;

impl RateChangerFactory for Fabrique {
    fn assess(&self, ctx: &PlaybackContext, _: &Settings) -> Result<Applicability, Error> {
        Ok(policy_bypass(ctx, true).map_or(
            Applicability::Process { requires_pcm: true },
            Applicability::Bypass,
        ))
    }
    fn negotiate(
        &self,
        input: AudioFormat,
        rates: &[u32],
        _: &Settings,
    ) -> Result<Option<u32>, Error> {
        Ok(rates
            .iter()
            .copied()
            .find(|&r| r == 2 * input.sample_rate()))
    }
    fn prepare(
        &self,
        requete: RateRequest,
        settings: &Settings,
    ) -> Result<Box<dyn RateChanger>, Error> {
        Ok(Box::new(Rendu {
            requete,
            profil: settings["profile"]
                .as_str()
                .ok_or(Error::InvalidSettings)?
                .into(),
        }))
    }
}

impl RateChanger for Rendu {
    fn request(&self) -> RateRequest {
        self.requete
    }
    fn process(
        &mut self,
        entree: &AudioBlock<'_>,
        sortie: &mut AudioBlock<'_>,
        _: BlockContext,
    ) -> Result<RateReport, Error> {
        let n = entree.frames();
        let ch = usize::from(entree.format().channels());
        if sortie.frames() < 2 * n {
            return Err(Error::BlockTooLarge);
        }
        let (Samples::F32(src), SamplesMut::F32(dst)) = (entree.samples(), sortie.samples_mut())
        else {
            return Err(Error::InvalidFormat);
        };
        for (s, d) in src.chunks_exact(ch).zip(dst.chunks_exact_mut(2 * ch)) {
            let (a, b) = d.split_at_mut(ch);
            a.copy_from_slice(s);
            b.copy_from_slice(s);
        }
        Ok(RateReport {
            frames_consumed: n,
            frames_written: 2 * n,
            ..Default::default()
        })
    }
    fn update(&mut self, settings: &Settings) -> Result<(), Error> {
        let profil = settings["profile"].as_str().ok_or(Error::InvalidSettings)?;
        if profil == "refus-banc" {
            return Err(Error::InvalidSettings);
        }
        self.profil = profil.into();
        Ok(())
    }
    fn diagnostics(&self) -> Settings {
        serde_json::json!({"headroom_db":0.0,"decision":{"chosen":{"depth":20,"design":self.profil},"trace":[{"code":"banc"}]}})
    }
    fn reset(&mut self, _: ResetReason) {}
    fn latency_frames(&self) -> u32 {
        0
    }
    fn drain(&mut self, _: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        Ok(DrainReport {
            frames_written: 0,
            complete: true,
        })
    }
}
impl FinalPcm for Grille {
    fn process(&mut self, mots: &mut [f64]) {
        for v in mots {
            *v = (*v * 524288.0).round() / 524288.0;
        }
    }
}
impl FinalPcmFactory for Fabrique {
    fn prepare_final(
        &self,
        _: AudioFormat,
        _: u32,
        bits: u16,
        _: &Settings,
    ) -> Result<Box<dyn FinalPcm>, Error> {
        if bits < 20 {
            return Err(Error::UnsupportedFormat);
        }
        Ok(Box::new(Grille))
    }
}
tune_plugin_abi::export_rate!(
    super::Fabrique,
    include_str!("../manifest.json"),
    finalizer = super::Fabrique
);
