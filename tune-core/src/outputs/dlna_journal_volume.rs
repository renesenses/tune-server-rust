//! #5575 / #5793 — l'acquittement de `SetVolume` au journal INFO, à débit borné.
//!
//! Tades et Sevy (enquête du 05/10) : le curseur bouge, le serveur envoie, et
//! le volume de l'appareil ne change pas. La seule ligne qui disait ce que le
//! renderer avait répondu, `dlna_set_volume_ok`, était au niveau DEBUG : les
//! journaux des testeurs ne prouvaient rien.
//!
//! Un glissement de curseur envoie des dizaines d'ordres par seconde. Les
//! porter tous en INFO noierait le journal ; ce module décide lesquels
//! s'écrivent, sans réseau ni horloge propre (l'instant est passé par
//! l'appelant, ce qui rend la borne vérifiable au millième près) :
//!
//! - le premier ordre d'une fenêtre de [`INTERVALLE`] s'écrit tout de suite ;
//! - les suivants de la même fenêtre se taisent, mais le DERNIER est gardé et
//!   un rattrapage est armé : à la fin de la fenêtre, il s'écrit avec le
//!   nombre d'ordres qu'il résume. La valeur finale d'un glissement est donc
//!   toujours au journal.
//!
//! Borne : sur une rafale de durée `d`, au plus `1 + ⌈d / INTERVALLE⌉` lignes
//! par sortie, c'est-à-dire par zone.

use std::time::{Duration, Instant};

/// Une ligne INFO au plus par zone et par fenêtre de cette durée.
pub(crate) const INTERVALLE: Duration = Duration::from_secs(2);

/// Ce qu'un ordre `SetVolume` a donné, tel qu'il s'écrit au journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcquitVolume {
    /// La consigne reçue par la sortie, en pour-cent (gain de zone déjà
    /// appliqué en amont).
    pub(crate) volume_pct: u32,
    /// Ce qui est parti dans `DesiredVolume`, dans l'unité de l'appareil.
    pub(crate) niveau: u32,
    /// `Master`, `LF,RF`… ou `GroupRenderingControl` pour un Sonos groupé.
    pub(crate) canal: String,
    /// `InstanceID` de l'ordre.
    pub(crate) instance: u32,
    /// `OK`, ou `erreur <errorCode>` quand l'appareil a refusé.
    pub(crate) reponse: String,
}

/// Que faire de l'ordre qui vient d'être noté.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// L'écrire maintenant ; `regroupes` = ordres tus depuis la ligne
    /// précédente.
    Ecrire { regroupes: u32 },
    /// Le taire. `rattrapage_dans` : armer UN rattrapage après ce délai (le
    /// premier ordre tu de la fenêtre seulement).
    Taire { rattrapage_dans: Option<Duration> },
}

/// État du débit, un par sortie DLNA.
#[derive(Debug, Default)]
pub(crate) struct JournalVolume {
    derniere_ecriture: Option<Instant>,
    en_attente: Option<AcquitVolume>,
    tus: u32,
    rattrapage_arme: bool,
}

impl JournalVolume {
    /// Note un ordre acquitté (ou refusé) à l'instant `maintenant`.
    pub(crate) fn noter(&mut self, maintenant: Instant, acquit: AcquitVolume) -> Decision {
        let fenetre_close = self
            .derniere_ecriture
            .is_none_or(|t| maintenant.saturating_duration_since(t) >= INTERVALLE);
        if fenetre_close {
            let regroupes = self.tus;
            self.tus = 0;
            self.en_attente = None;
            self.derniere_ecriture = Some(maintenant);
            return Decision::Ecrire { regroupes };
        }
        self.tus += 1;
        self.en_attente = Some(acquit);
        if self.rattrapage_arme {
            return Decision::Taire {
                rattrapage_dans: None,
            };
        }
        self.rattrapage_arme = true;
        let ecoule = self
            .derniere_ecriture
            .map(|t| maintenant.saturating_duration_since(t))
            .unwrap_or_default();
        Decision::Taire {
            rattrapage_dans: Some(INTERVALLE.saturating_sub(ecoule)),
        }
    }

    /// Fin de fenêtre : rend le dernier ordre tu, s'il en reste un, avec le
    /// nombre d'ordres qu'il résume (lui compris).
    pub(crate) fn rattraper(&mut self, maintenant: Instant) -> Option<(AcquitVolume, u32)> {
        self.rattrapage_arme = false;
        let acquit = self.en_attente.take()?;
        let regroupes = self.tus;
        self.tus = 0;
        self.derniere_ecriture = Some(maintenant);
        Some((acquit, regroupes))
    }
}

/// `OK`, ou `erreur <code>` lu dans la faute SOAP.
pub(crate) fn reponse_soap(corps: &str) -> String {
    if corps.contains("UPnPError") || corps.contains("<errorCode>") {
        let code = super::dlna::extract_tag(corps, "errorCode")
            .map(|c| c.trim().to_string())
            .unwrap_or_else(|| "inconnu".into());
        format!("erreur {code}")
    } else {
        "OK".into()
    }
}

/// Verdict de la relecture `GetVolume` : l'appareil a-t-il appliqué ?
pub(crate) fn verdict_relecture(attendu: u32, lu: Option<u32>) -> &'static str {
    match lu {
        Some(l) if l == attendu => "oui",
        Some(_) => "non",
        None => "illisible",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acquit(pct: u32) -> AcquitVolume {
        AcquitVolume {
            volume_pct: pct,
            niveau: pct,
            canal: "Master".into(),
            instance: 0,
            reponse: "OK".into(),
        }
    }

    /// Rejoue `n` ordres espacés de `pas`, rattrapages compris, et rend les
    /// lignes écrites (valeur, regroupés).
    fn rejouer(n: u32, pas: Duration) -> Vec<(u32, u32)> {
        let t0 = Instant::now();
        let mut j = JournalVolume::default();
        let mut lignes = Vec::new();
        let mut rattrapage: Option<Instant> = None;
        for i in 0..n {
            let t = t0 + pas * i;
            if let Some(r) = rattrapage
                && r <= t
            {
                rattrapage = None;
                if let Some((a, k)) = j.rattraper(r) {
                    lignes.push((a.volume_pct, k));
                }
            }
            match j.noter(t, acquit(i)) {
                Decision::Ecrire { regroupes } => lignes.push((i, regroupes)),
                Decision::Taire {
                    rattrapage_dans: Some(d),
                } => rattrapage = Some(t + d),
                Decision::Taire { .. } => {}
            }
        }
        if let Some(r) = rattrapage
            && let Some((a, k)) = j.rattraper(r)
        {
            lignes.push((a.volume_pct, k));
        }
        lignes
    }

    #[test]
    fn cent_ordres_en_rafale_donnent_deux_lignes_dont_la_derniere_valeur() {
        let lignes = rejouer(100, Duration::from_millis(5));
        assert_eq!(lignes, vec![(0, 0), (99, 99)], "{lignes:?}");
    }

    #[test]
    fn un_glissement_de_dix_secondes_reste_borne() {
        // 100 ordres sur 9,9 s : au plus 1 + ⌈9,9 / 2⌉ = 6 lignes.
        let lignes = rejouer(100, Duration::from_millis(100));
        assert!(lignes.len() <= 6, "{lignes:?}");
        assert_eq!(lignes.last().unwrap().0, 99, "{lignes:?}");
    }

    #[test]
    fn des_ordres_espaces_s_ecrivent_tous() {
        let lignes = rejouer(5, Duration::from_millis(2500));
        assert_eq!(lignes.len(), 5, "{lignes:?}");
    }

    #[test]
    fn la_reponse_dit_ok_ou_le_code() {
        assert_eq!(reponse_soap("<u:SetVolumeResponse/>"), "OK");
        assert_eq!(
            reponse_soap("<s:Fault><UPnPError><errorCode>401</errorCode></UPnPError></s:Fault>"),
            "erreur 401"
        );
        assert_eq!(verdict_relecture(128, Some(128)), "oui");
        assert_eq!(verdict_relecture(128, Some(40)), "non");
        assert_eq!(verdict_relecture(128, None), "illisible");
    }
}
