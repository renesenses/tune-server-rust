//! Sources dont l'URL de lecture est fournie par un greffon, AU MOMENT de
//! jouer (Tune Circle T4, #5327 : « Lire l'album » chez un contact).
//!
//! Sur le modèle de [`crate::source_pcm`] (#4863, le CD) : un greffon inscrit
//! un fournisseur sous un nom de `source`. Une ligne de file qui porte cette
//! source ne garde qu'une RÉFÉRENCE dans `source_id` (pour `circle` :
//! `{user_id}:{track_id}`), jamais l'URL. À la résolution — lecture,
//! avancement de file, pré-armement gapless, reprise — l'orchestrateur demande
//! l'URL au fournisseur ([`FournisseurDUrl::url`]) et joue cette URL comme un
//! flux distant (`upnp`). L'URL n'entre ni dans la file, ni dans la lecture en
//! cours, ni dans l'historique : ils gardent la référence.
//!
//! Deux sortes de refus, que la boucle d'avancement traite différemment
//! (`poller/fin_de_piste.rs`, `avancer_avec_reprises`) :
//!
//! * [`RefusDUrl::Piste`] : cette piste-là ne se joue pas (retirée, plus
//!   partagée…). Le message porte [`MOTIF_PISTE_REFUSEE`], reconnu par
//!   `refus_propre_a_la_piste` : la piste est sautée (`playback.track_skipped`)
//!   sans entamer le budget des pannes systémiques.
//! * [`RefusDUrl::ArretDeLaFile`] : plus rien de cette file ne se jouera (le
//!   serveur qui sert les pistes est éteint). Le message porte
//!   [`MOTIF_ARRET_DE_LA_FILE`] : la boucle s'arrête aussitôt, sans essayer
//!   les pistes suivantes.
//!
//! Toute autre erreur ([`RefusDUrl::Panne`]) suit la règle commune des pannes.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

/// Marque d'un refus propre à la piste, dans le message d'erreur.
pub const MOTIF_PISTE_REFUSEE: &str = "source_url: piste refusee";
/// Marque d'un arrêt de la file, dans le message d'erreur.
pub const MOTIF_ARRET_DE_LA_FILE: &str = "source_url: arret de la file";

/// Pourquoi le fournisseur ne rend pas d'URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusDUrl {
    /// Cette piste seulement : on la saute.
    Piste(String),
    /// Toute la file : on s'arrête.
    ArretDeLaFile(String),
    /// Une panne (réseau, service) : règle commune.
    Panne(String),
}

impl RefusDUrl {
    /// Le message d'erreur que porte l'orchestrateur, marque comprise.
    pub fn en_message(&self) -> String {
        match self {
            RefusDUrl::Piste(m) => format!("{MOTIF_PISTE_REFUSEE} ({m})"),
            RefusDUrl::ArretDeLaFile(m) => format!("{MOTIF_ARRET_DE_LA_FILE} ({m})"),
            RefusDUrl::Panne(m) => format!("source_url: {m}"),
        }
    }
}

/// Ce message demande-t-il d'arrêter la file ?
pub fn arret_de_la_file(message: &str) -> bool {
    message.contains(MOTIF_ARRET_DE_LA_FILE)
}

/// Ce que le fournisseur rend : l'URL, et le format du fichier quand il le
/// connaît — une URL sans extension ne dit pas son MIME, et un renderer DLNA
/// à qui l'on annonce `audio/mpeg` pour du FLAC joue du silence
/// (`orchestrator/mime_upnp.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlFournie {
    pub url: String,
    pub media_format: Option<String>,
}

/// Un greffon qui fournit l'URL de lecture d'une référence.
#[async_trait]
pub trait FournisseurDUrl: Send + Sync {
    /// L'URL http(s) à jouer, pour `source_id`, sur la zone `zone_id`.
    async fn url(&self, zone_id: i64, source_id: &str) -> Result<UrlFournie, RefusDUrl>;
}

/// Le registre des fournisseurs, par nom de `source`.
#[derive(Default, Clone)]
pub struct SourcesUrl(Arc<RwLock<HashMap<String, Arc<dyn FournisseurDUrl>>>>);

impl SourcesUrl {
    pub fn inscrire(&self, source: &str, fournisseur: Arc<dyn FournisseurDUrl>) {
        if let Ok(mut m) = self.0.write() {
            m.insert(source.to_string(), fournisseur);
        }
    }

    pub fn retirer(&self, source: &str) {
        if let Ok(mut m) = self.0.write() {
            m.remove(source);
        }
    }

    pub fn fournisseur(&self, source: &str) -> Option<Arc<dyn FournisseurDUrl>> {
        self.0.read().ok().and_then(|m| m.get(source).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_marques_se_reconnaissent_et_ne_se_confondent_pas() {
        let piste = RefusDUrl::Piste("not_found".into()).en_message();
        let arret = RefusDUrl::ArretDeLaFile("owner_offline".into()).en_message();
        let panne = RefusDUrl::Panne("cloud".into()).en_message();
        assert!(arret_de_la_file(&arret));
        assert!(!arret_de_la_file(&piste));
        assert!(!arret_de_la_file(&panne));
        assert!(crate::poller::refus_de_piste::refus_propre_a_la_piste(
            &piste
        ));
        assert!(!crate::poller::refus_de_piste::refus_propre_a_la_piste(
            &arret
        ));
        assert!(!crate::poller::refus_de_piste::refus_propre_a_la_piste(
            &panne
        ));
    }
}
