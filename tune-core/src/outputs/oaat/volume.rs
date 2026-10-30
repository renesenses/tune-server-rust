//! Contrat de volume annoncé par OAAT. Aucun traitement des échantillons.
use crate::outputs::traits::{OutputCapabilities, VolumePercentRange};

#[derive(Clone, Copy, Default)]
enum Contrat {
    /// Repli des anciens endpoints sans annonce : ancien pour-cent entier.
    #[default]
    Ancien,
    Interdit,
    Reglable(VolumePercentRange),
}

#[derive(Default)]
pub(super) struct VolumeOaat {
    mdns: Contrat,
    hello: Option<Contrat>,
}

impl VolumeOaat {
    pub(super) fn annoncer_mdns(&mut self, annonce: Option<&str>) {
        self.mdns = match annonce {
            Some("fixed" | "none") => Contrat::Interdit,
            // Le TXT ne donne ni plage ni pas : attendre hello_ack pour eux.
            Some("hw" | "sw") | None => Contrat::Ancien,
            Some(_) => Contrat::Interdit,
        };
    }

    #[cfg(feature = "oaat")]
    pub(super) fn annoncer_hello(
        &mut self,
        annonce: Option<&oaat_core::message::VolumeCapability>,
    ) {
        use oaat_core::message::VolumeType;
        self.hello = annonce.map(|v| match v.vol_type {
            VolumeType::Fixed | VolumeType::None => Contrat::Interdit,
            VolumeType::Hw | VolumeType::Sw
                if v.range[0] <= v.range[1] && v.range[1] <= 100 && v.step > 0 =>
            {
                Contrat::Reglable(VolumePercentRange {
                    min: v.range[0],
                    max: v.range[1],
                    step: v.step,
                })
            }
            // Une annonce incohérente n'autorise pas une commande inventée.
            _ => Contrat::Interdit,
        });
    }

    fn contrat(&self) -> Contrat {
        // Une annonce explicite fixed/none n'est pas levée par une annonce
        // contradictoire : ne pas envoyer de commandes sur cette ambiguïté.
        if matches!(self.mdns, Contrat::Interdit) {
            Contrat::Interdit
        } else {
            self.hello.unwrap_or(self.mdns)
        }
    }

    pub(super) fn pilotable(&self) -> bool {
        !matches!(self.contrat(), Contrat::Interdit)
    }

    pub(super) fn capacites(&self, gapless: bool) -> OutputCapabilities {
        let pilotable = self.pilotable();
        let mut caps = OutputCapabilities::v1(true, true, true, pilotable, pilotable, gapless);
        match self.contrat() {
            Contrat::Ancien => caps = caps.with_percent_volume(),
            Contrat::Reglable(plage) => {
                caps.volume_percent_range = Some(plage);
                // La grille existante est ancrée à zéro. Ne pas mentir pour
                // une plage décalée ; publier sa vraie origine séparément.
                if plage.min == 0 && plage.max == 100 && 100 % u32::from(plage.step) == 0 {
                    caps = caps.with_linear_volume(100 / u32::from(plage.step));
                }
            }
            Contrat::Interdit => {}
        }
        caps
    }

    pub(super) fn normaliser(&self, niveau: u8) -> Result<u8, String> {
        match self.contrat() {
            Contrat::Interdit => {
                Err("OAAT : cet appareil n'annonce pas de volume pilotable à distance".into())
            }
            Contrat::Ancien => Ok(niveau.min(100)),
            Contrat::Reglable(plage) => {
                if niveau < plage.min || niveau > plage.max {
                    return Err(format!(
                        "OAAT : volume {niveau} % hors de la plage annoncée {}–{} %",
                        plage.min, plage.max
                    ));
                }
                let distance = u16::from(niveau - plage.min);
                let pas = u16::from(plage.step);
                let cran = ((distance + pas / 2) / pas).min(u16::from(plage.max - plage.min) / pas);
                Ok(plage.min + (cran * pas) as u8)
            }
        }
    }
}
