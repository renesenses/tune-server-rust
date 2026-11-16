//! #8483 (go de Bertrand, rc12, 12/11) — la coupure d'un fichier abîmé, telle
//! que le décodage de Tune l'a vue.
//!
//! Fil 2769 : un FLAC dont le conteneur annonce 9 473 856 trames n'en contient
//! que 5 999 616 (2:16 sur 3:34). Le décodage-pour-niveaux du passthrough DLNA
//! le constate une trentaine de secondes AVANT que le renderer n'y arrive
//! (`decodage_incomplet_le_conteneur_annoncait_plus`, #2218), mais ce constat
//! ne servait qu'au journal : le renderer se figeait à 2:15, la zone restait
//! « en lecture » muette, puis la garde #2116/#6702 l'ARRÊTAIT.
//!
//! Ce registre garde, par fichier, l'endroit où le flux s'arrête vraiment.
//! Le sondeur le lit quand la position se fige (ou que la sortie s'arrête)
//! sur cette coupure : c'est la fin de la piste, on passe à la suivante.
//!
//! Il est rempli par le SEUL décodage qui mesure une perte de trames : un
//! décodage complet, depuis le début du fichier, sur le fichier lui-même
//! (`decode_to_pcm_streaming_inner`). Un décodage complet SANS perte efface la
//! note — un fichier réparé ou remplacé ne reste pas marqué.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Au-delà, le registre est vidé : quelques fichiers abîmés par bibliothèque,
/// jamais des milliers. Borne de sûreté, pas de politique d'éviction fine.
const CAPACITE: usize = 512;

/// Où le flux s'arrête, face à ce que le conteneur annonçait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoupureConnue {
    /// Fin réelle du flux décodé, en millisecondes depuis le début du fichier.
    pub fin_ms: u64,
    /// Durée que le conteneur annonçait, en millisecondes.
    pub annonce_ms: u64,
}

static REGISTRE: LazyLock<Mutex<HashMap<String, CoupureConnue>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// La clé d'un chemin : sa forme canonique quand le fichier existe, pour que
/// le décodeur (chemin résolu) et le sondeur (chemin en base) se rejoignent.
fn cle(chemin: &str) -> String {
    std::fs::canonicalize(chemin)
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| chemin.to_owned())
}

/// Les millisecondes de `trames` à `cadence` Hz.
pub fn trames_en_ms(trames: u64, cadence: u32) -> u64 {
    if cadence == 0 {
        return 0;
    }
    (trames as u128 * 1000 / cadence as u128) as u64
}

/// Note le relevé d'un décodage complet du fichier `chemin` : une coupure
/// quand le flux rend moins de trames que le conteneur n'en annonce, l'oubli
/// sinon. Sans annonce (MP3 sans en-tête Xing), rien n'est su : rien ne change.
pub fn noter_le_releve(
    chemin: &str,
    trames_annoncees: Option<u64>,
    trames_rendues: u64,
    cadence: u32,
) {
    let Some(annoncees) = trames_annoncees else {
        return;
    };
    let cle = cle(chemin);
    let Ok(mut registre) = REGISTRE.lock() else {
        return;
    };
    if trames_rendues >= annoncees || cadence == 0 {
        registre.remove(&cle);
        return;
    }
    if registre.len() >= CAPACITE && !registre.contains_key(&cle) {
        registre.clear();
    }
    registre.insert(
        cle,
        CoupureConnue {
            fin_ms: trames_en_ms(trames_rendues, cadence),
            annonce_ms: trames_en_ms(annoncees, cadence),
        },
    );
}

/// La coupure connue du fichier `chemin`, s'il en a une.
pub fn coupure_de(chemin: &str) -> Option<CoupureConnue> {
    if aucune_coupure_connue() {
        return None;
    }
    // La clé (un appel système) se calcule HORS du verrou.
    let cle = cle(chemin);
    let registre = REGISTRE.lock().ok()?;
    registre.get(chemin).or_else(|| registre.get(&cle)).copied()
}

/// Le registre est-il vide ? Le sondeur s'épargne alors toute lecture en base.
pub fn aucune_coupure_connue() -> bool {
    REGISTRE.lock().map(|r| r.is_empty()).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_perte_de_trames_est_notee_puis_oubliee_quand_le_fichier_est_repare_8483() {
        let chemin = "/nexiste/pas/fil-2769-unitaire.flac";
        noter_le_releve(chemin, Some(9_473_856), 5_999_616, 44_100);
        let c = coupure_de(chemin).expect("coupure notée");
        assert_eq!(c.fin_ms, 136_045);
        assert_eq!(c.annonce_ms, 214_826);
        // Un décodage complet sans perte efface la note.
        noter_le_releve(chemin, Some(9_473_856), 9_473_856, 44_100);
        assert_eq!(coupure_de(chemin), None);
    }

    #[test]
    fn sans_annonce_rien_n_est_su_8483() {
        let chemin = "/nexiste/pas/sans-annonce-8483.mp3";
        noter_le_releve(chemin, None, 10, 44_100);
        assert_eq!(coupure_de(chemin), None);
    }
}
