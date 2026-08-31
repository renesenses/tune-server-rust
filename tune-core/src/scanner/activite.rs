//! Un scan de bibliothèque est-il en cours, dans CE processus ?
//!
//! Le point 3 de #2469 — Thierry Clemont (Tades) : « [que le scan programmé se
//! lance] dans les mêmes conditions que le scan CLAP […] mais de manière
//! prioritaire sur le scan CLAP ». Les deux passes s'effacent déjà devant la
//! lecture ; il manquait la priorité ENTRE elles. Le balayage acoustique décode
//! de l'audio et fait tourner ONNX sur plusieurs fils : lancé en même temps que
//! la mise à jour de bibliothèque, il lui dispute le disque et le CPU, et le
//! scan de 21 h met bien plus longtemps qu'il ne devrait.
//!
//! ## Pourquoi un drapeau de processus, et pas le réglage `scan_status`
//!
//! `spawn_library_scan` écrit déjà `scan_status = "scanning"` en base. Le relire
//! serait tentant — et faux : cette valeur **survit à l'arrêt du serveur**. Un
//! processus tué pendant un scan (coupure, `kill -9`, mise à jour) laisse
//! `scan_status` à `"scanning"` pour toujours, et le balayage acoustique ne
//! repartirait JAMAIS. Un drapeau en mémoire naît à zéro à chaque démarrage :
//! c'est la seule forme qui ne peut pas se coincer.
//!
//! ## Pourquoi ici et pas dans `tune-server`
//!
//! La porte unique du scan (`ScanGate`) vit dans `tune-server`, que `tune-core`
//! ne voit pas. Le drapeau vit donc du côté visible des deux, et `ScanGate` le
//! tient à jour — il n'y a pas de seconde source de vérité : `ScanLease` est le
//! SEUL objet qui puisse le lever, et son `Drop` le baisse, y compris si la
//! tâche de scan panique.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Compteur, et non booléen : le `Drop` d'un jeton ne doit jamais effacer la
/// marque d'un autre. La porte unique n'en délivre qu'un à la fois aujourd'hui,
/// mais un compteur reste juste si cela change — un booléen, non.
static SCANS_EN_COURS: AtomicUsize = AtomicUsize::new(0);

/// Marque « un scan de bibliothèque tourne » tant qu'elle est vivante.
///
/// Non clonable exprès : une marque, un scan. Elle se baisse au `Drop`, donc
/// aussi sur une panique de la tâche qui la portait.
#[derive(Debug)]
pub struct MarqueDeScan {
    _prive: (),
}

impl MarqueDeScan {
    /// À n'appeler que depuis le détenteur de la porte unique du scan.
    pub fn poser() -> Self {
        SCANS_EN_COURS.fetch_add(1, Ordering::SeqCst);
        Self { _prive: () }
    }
}

impl Drop for MarqueDeScan {
    fn drop(&mut self) {
        // `fetch_update` plutôt que `fetch_sub` : un décompte qui passerait sous
        // zéro reboucle sur `usize::MAX` et rendrait le scan éternellement « en
        // cours ». Ici il ne descend simplement pas sous zéro.
        let _ = SCANS_EN_COURS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        });
    }
}

/// Vrai tant qu'un scan de bibliothèque tourne dans ce processus.
///
/// Lu par le balayage acoustique, qui s'efface devant lui (#2469).
pub fn scan_bibliotheque_en_cours() -> bool {
    SCANS_EN_COURS.load(Ordering::SeqCst) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ces tests partagent un compteur de processus : ils ne peuvent pas
    /// tourner en parallèle sans se voler leurs constats.
    static SERIALISE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn au_repos_aucun_scan_ne_tourne() {
        let _s = SERIALISE.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !scan_bibliotheque_en_cours(),
            "sans marque posée, aucun scan ne doit être annoncé"
        );
    }

    #[test]
    fn la_marque_leve_puis_baisse_le_drapeau() {
        let _s = SERIALISE.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!scan_bibliotheque_en_cours());
        {
            let _marque = MarqueDeScan::poser();
            assert!(
                scan_bibliotheque_en_cours(),
                "la marque posée doit rendre le scan visible"
            );
        }
        assert!(
            !scan_bibliotheque_en_cours(),
            "le Drop de la marque doit rendre la main au balayage acoustique"
        );
    }

    #[test]
    fn deux_marques_ne_se_volent_pas_leur_drop() {
        let _s = SERIALISE.lock().unwrap_or_else(|e| e.into_inner());
        let a = MarqueDeScan::poser();
        let b = MarqueDeScan::poser();
        drop(a);
        assert!(
            scan_bibliotheque_en_cours(),
            "la seconde marque tient encore : le drapeau doit rester levé"
        );
        drop(b);
        assert!(!scan_bibliotheque_en_cours());
    }

    #[test]
    fn une_panique_pendant_le_scan_rend_quand_meme_la_main() {
        let _s = SERIALISE.lock().unwrap_or_else(|e| e.into_inner());
        let issue = std::panic::catch_unwind(|| {
            let _marque = MarqueDeScan::poser();
            assert!(scan_bibliotheque_en_cours());
            panic!("le scan explose");
        });
        assert!(issue.is_err(), "la panique doit bien être survenue");
        assert!(
            !scan_bibliotheque_en_cours(),
            "un scan qui panique ne doit pas geler le balayage acoustique pour toujours"
        );
    }
}
