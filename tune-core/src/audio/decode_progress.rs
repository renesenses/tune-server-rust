//! Balise d'avancement du décodage : combien de **secondes d'audio** sont déjà
//! sorties du décodeur, publiées AU FIL du décodage.
//!
//! Elle existe pour une seule raison (#3140) : le budget accordé à un
//! transcodage était indexé sur la TAILLE du fichier, jamais sur la vitesse de
//! la machine. Or `budget(D) = 120 + 0,3154·D` en DSD256 n'est tenable que si
//! l'hôte décode à `× 3,17` temps réel ; Shrek décode à `× 2,2`. Pour borner
//! correctement il faut connaître le **débit réel de cet hôte sur ce
//! fichier-là** — et la seule mesure gratuite est celle du décodage déjà en
//! cours.
//!
//! ## Pourquoi une variable de thread plutôt qu'un paramètre
//!
//! `decode_to_pcm` a huit implémentations de décodeurs derrière elle (DSD,
//! symphonia, AIFF, WavPack, APE, Opus…) et une trentaine d'appelants. Ajouter
//! un paramètre à toute la chaîne pour qu'une poignée de boucles publie un
//! compteur ferait payer la signature à tout le monde. La balise est donc
//! posée par l'appelant qui en a besoin, sur le thread `spawn_blocking` où le
//! décodage se déroule, et les boucles publient sans rien savoir d'elle.
//!
//! **Quand aucune balise n'est posée, `publier` est un `try_with` qui trouve
//! `None` : le décodage est strictement inchangé.** C'est la propriété qui rend
//! ce correctif invisible pour tout ce qui n'attendait pas de mesure.
//!
//! ## Ce qui publie, et ce qui ne publie pas
//!
//! Deux boucles publient : le décodage DSD (`decode_dsd_to_pcm`, le cas mesuré
//! du ticket) et le décodage symphonia entier (`decode_symphonia`, qui couvre
//! FLAC / ALAC / WAV / AIFF-in-symphonia…). Les autres décodeurs ne publient
//! rien ; la balise reste alors à zéro et l'appelant retombe sur le budget
//! historique. **Une absence de mesure ne doit jamais raccourcir un budget.**

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Compteur partagé : millisecondes d'audio déjà décodées.
///
/// Monotone par construction (`fetch_max`) : les boucles publient une valeur
/// CUMULÉE, et un décodeur qui reculerait — un rebond de chaîne Ogg, un
/// `truncate` de fin de fenêtre — ne doit pas faire croire à une régression du
/// débit.
#[derive(Debug, Default)]
pub struct DecodeProgress {
    decoded_ms: AtomicU64,
}

impl DecodeProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Millisecondes d'audio décodées à cet instant. `0` = rien de mesuré
    /// (décodeur muet, ou décodage pas encore démarré) — jamais « instantané ».
    pub fn decoded_ms(&self) -> u64 {
        self.decoded_ms.load(Ordering::Relaxed)
    }

    /// Publier l'avancement CUMULÉ directement sur cette balise, sans passer
    /// par la variable de thread. C'est ce qui permet à un décodeur FEINT — un
    /// test — de publier depuis une tâche asynchrone, qui n'a pas de thread à
    /// elle.
    pub fn publier(&self, ms: u64) {
        self.decoded_ms.fetch_max(ms, Ordering::Relaxed);
    }
}

thread_local! {
    static COURANTE: RefCell<Option<Arc<DecodeProgress>>> = const { RefCell::new(None) };
}

/// Garde RAII : repose la balise précédente en sortant, pour qu'un décodage
/// imbriqué (un `catch_unwind` qui relance, un décodeur qui en appelle un
/// autre) ne laisse jamais une balise étrangère derrière lui.
pub struct Balise {
    precedente: Option<Arc<DecodeProgress>>,
}

impl Drop for Balise {
    fn drop(&mut self) {
        let _ = COURANTE.try_with(|c| {
            *c.borrow_mut() = self.precedente.take();
        });
    }
}

/// Pose `progres` comme balise du thread courant jusqu'à la chute du garde.
///
/// À appeler DANS le `spawn_blocking` : c'est le thread du décodage qui doit
/// porter la balise, pas celui qui l'ordonne.
pub fn installer(progres: Arc<DecodeProgress>) -> Balise {
    let precedente = COURANTE
        .try_with(|c| c.borrow_mut().replace(progres))
        .unwrap_or(None);
    Balise { precedente }
}

/// Publier l'avancement CUMULÉ du décodage en cours, en millisecondes d'audio.
///
/// Sans balise posée : deux accès à une variable de thread et rien d'autre.
pub fn publier(decoded_ms: u64) {
    let _ = COURANTE.try_with(|c| {
        if let Some(p) = c.borrow().as_ref() {
            p.publier(decoded_ms);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sans balise, publier ne fait rien et ne panique pas — c'est l'état de
    /// tous les décodages du serveur qui ne passent pas par le transcodage.
    #[test]
    fn sans_balise_publier_est_inerte() {
        publier(1234);
        publier(0);
    }

    #[test]
    fn la_balise_recoit_ce_qui_est_publie() {
        let p = DecodeProgress::new();
        let _g = installer(p.clone());
        publier(250);
        publier(500);
        assert_eq!(p.decoded_ms(), 500);
    }

    /// Un décodeur qui recule ne doit pas faire croire à un débit qui s'effondre.
    #[test]
    fn la_balise_ne_recule_jamais() {
        let p = DecodeProgress::new();
        let _g = installer(p.clone());
        publier(900);
        publier(400);
        assert_eq!(p.decoded_ms(), 900);
    }

    /// À la chute du garde, la balise précédente revient — et l'ancienne cesse
    /// de recevoir.
    #[test]
    fn le_garde_repose_la_balise_precedente() {
        let externe = DecodeProgress::new();
        let _g = installer(externe.clone());
        publier(100);
        {
            let interne = DecodeProgress::new();
            let _g2 = installer(interne.clone());
            publier(700);
            assert_eq!(interne.decoded_ms(), 700);
        }
        publier(200);
        assert_eq!(externe.decoded_ms(), 200);
    }

    /// Une balise est PAR THREAD : un décodage voisin n'en reçoit rien.
    #[test]
    fn la_balise_ne_traverse_pas_les_threads() {
        let p = DecodeProgress::new();
        let _g = installer(p.clone());
        std::thread::spawn(|| publier(9999)).join().unwrap();
        assert_eq!(p.decoded_ms(), 0);
    }
}
