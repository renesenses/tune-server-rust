//! #8486 — constat de la sortie OAAT, par récepteur, sans déduire le natif
//! du WAV préparé par l'orchestrateur. Les références faibles suivent la
//! sortie qui joue ; une ré-annonce mDNS ne remplace pas son état.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Raison {
    #[serde(rename = "no_dsd_capability")]
    SansDsd,
    #[serde(rename = "dsd_rate_limit")]
    Plafond,
    #[serde(rename = "unsupported_container")]
    Conteneur,
    #[serde(rename = "unknown_dsd_rate")]
    Cadence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Repli {
    #[serde(rename = "reason")]
    pub raison: Raison,
    pub dsd_rate: Option<u16>,
    pub dsd_max_rate: Option<u16>,
}

#[derive(Clone, Default, Debug)]
pub struct Observation {
    pub annonce_recue: bool,
    pub dsd_max_rate: Option<u16>,
    pub repli: Option<Repli>,
}
impl Observation {
    pub fn annoncer(&mut self, maximum: Option<u16>) {
        if self.annonce_recue && self.dsd_max_rate != maximum {
            self.repli = None;
        }
        self.annonce_recue = true;
        self.dsd_max_rate = maximum;
    }
}

type Etat = Mutex<Observation>;
fn table() -> &'static Mutex<HashMap<String, Weak<Etat>>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Weak<Etat>>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}
pub fn inscrire(device: &str, etat: &Arc<Etat>) {
    if let Ok(mut t) = table().lock() {
        t.retain(|_, v| v.strong_count() > 0);
        t.insert(device.into(), Arc::downgrade(etat));
    }
}
pub fn lire(device: &str) -> Option<Observation> {
    let etat = table().lock().ok()?.get(device)?.upgrade()?;
    etat.lock().ok().map(|v| v.clone())
}
pub fn effacer_repli(etat: &Etat) {
    if let Ok(mut e) = etat.lock() {
        e.repli = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn un_motif_ne_survit_pas_au_changement_de_capacite_8486() {
        let etat = Arc::new(Mutex::new(Observation::default()));
        inscrire("oaat:capacite-8486", &etat);
        {
            let mut e = etat.lock().unwrap();
            e.annoncer(Some(64));
            e.repli = Some(Repli {
                raison: Raison::Plafond,
                dsd_rate: Some(128),
                dsd_max_rate: Some(64),
            });
            e.annoncer(Some(64));
        }
        assert!(lire("oaat:capacite-8486").unwrap().repli.is_some());
        etat.lock().unwrap().annoncer(Some(512));
        assert!(lire("oaat:capacite-8486").unwrap().repli.is_none());
        assert_eq!(lire("oaat:capacite-8486").unwrap().dsd_max_rate, Some(512));
        drop(etat);
        assert!(
            lire("oaat:capacite-8486").is_none(),
            "une sortie détruite ne conserve aucun constat"
        );
    }
}
