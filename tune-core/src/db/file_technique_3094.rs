//! Fil 3094 (Xavier, rc13.90) — une ligne de file TECHNIQUE (le balayage
//! d'une mesure de calibration, `source = "calibration"`) ne reste pas dans
//! la file une fois la mesure finie.
//!
//! La mesure REMPLACE la file de la zone par son balayage
//! (`set_streaming_queue`) : avant ce correctif, la ligne y restait après la
//! fin, l'échec ou l'annulation, et la file d'avant était perdue. Un appui sur
//! Lecture rejouait ensuite « Calibration — balayage de mesure m3-… », que le
//! fournisseur refusait (« aucune mesure de calibration armée »), à chaque
//! appui.
//!
//! Deux gestes, sans réseau ni orchestrateur, pour qu'un test les prouve sur
//! une vraie base :
//!
//! * [`instantane`] / [`restaurer`] — la file d'avant la mesure, rendue à la
//!   fin si la file ne contient plus rien d'autre (l'auditeur n'y a rien
//!   enfilé entre-temps) ;
//! * [`retirer_les_lignes_techniques`] — ôte les lignes techniques choisies,
//!   renumérote le reste et garde le curseur sur la même piste (ou la
//!   suivante, si c'est la ligne en cours qui part).

use super::listening_resume::input;
use super::play_queue_repo::{PlayQueueRepo, QueueEntry, QueueInput};
use crate::source_pcm::source_technique;

/// Une ligne de la file est-elle TECHNIQUE (voir [`source_technique`]) ?
pub fn ligne_technique(e: &QueueEntry) -> bool {
    e.track_id.is_none() && e.source.as_deref().is_some_and(source_technique)
}

/// La file d'une zone, telle qu'il faudra la rendre.
#[derive(Debug, Clone, Default)]
pub struct FileAvant {
    pub items: Vec<QueueInput>,
    /// Le rang de la ligne en cours, s'il y en avait une.
    pub courant: Option<i64>,
}

/// La file de la zone, lignes techniques exclues.
pub fn instantane(repo: &PlayQueueRepo, zone_id: i64) -> Result<FileAvant, String> {
    let lignes = repo.get_ordered(zone_id)?;
    let mut items = Vec::with_capacity(lignes.len());
    let mut courant = None;
    for e in lignes.iter().filter(|e| !ligne_technique(e)) {
        if e.is_current {
            courant = Some(items.len() as i64);
        }
        items.push(input(e));
    }
    Ok(FileAvant { items, courant })
}

/// Rend la file `avant` à la zone si sa file est VIDE. Une file non vide est
/// celle que l'auditeur a posée pendant la mesure : on n'y touche pas.
///
/// Rend `Some((curseur, longueur))` quand la file a été rendue.
pub fn restaurer(
    repo: &PlayQueueRepo,
    zone_id: i64,
    avant: &FileAvant,
) -> Result<Option<(i64, i64)>, String> {
    if avant.items.is_empty() || repo.count_all(zone_id)? > 0 {
        return Ok(None);
    }
    let bilan = repo.insert_at_bilan(zone_id, &avant.items, None)?;
    let longueur = bilan.inserted() as i64;
    if longueur == 0 {
        return Ok(None);
    }
    let curseur = avant.courant.unwrap_or(0).clamp(0, longueur - 1);
    repo.set_current_pos(zone_id, curseur)?;
    Ok(Some((curseur, longueur)))
}

/// Ce que [`retirer_les_lignes_techniques`] a fait.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Retrait {
    /// Les rangs (d'AVANT le retrait) des lignes ôtées, croissants.
    pub retirees: Vec<i64>,
    /// Les titres des lignes ôtées, dans le même ordre.
    pub titres: Vec<Option<String>>,
    /// Le rang de la ligne en cours après le retrait.
    pub courant: Option<i64>,
    pub longueur: i64,
}

impl Retrait {
    /// Où tombe, après le retrait, la ligne qui était au rang `position` :
    /// une ligne ôtée cède sa place à la suivante.
    pub fn position_apres(&self, position: i64) -> i64 {
        position - self.retirees.iter().filter(|r| **r < position).count() as i64
    }
}

/// Ôte de la file de `zone_id` les lignes TECHNIQUES pour lesquelles
/// `a_retirer` répond vrai (jamais une ligne de musique, quoi que dise le
/// prédicat), renumérote le reste et replace le curseur.
pub fn retirer_les_lignes_techniques(
    repo: &PlayQueueRepo,
    zone_id: i64,
    a_retirer: impl Fn(&QueueEntry) -> bool,
) -> Result<Retrait, String> {
    let lignes = repo.get_ordered(zone_id)?;
    let mut retrait = Retrait::default();
    let mut ids = Vec::new();
    for (rang, e) in lignes.iter().enumerate() {
        if ligne_technique(e) && a_retirer(e) {
            retrait.retirees.push(rang as i64);
            retrait.titres.push(e.title.clone());
            ids.push(e.id);
        }
    }
    let courant_avant = lignes.iter().position(|e| e.is_current).map(|p| p as i64);
    retrait.longueur = lignes.len() as i64 - ids.len() as i64;
    if ids.is_empty() {
        retrait.courant = courant_avant;
        return Ok(retrait);
    }
    repo.retirer_les_lignes(&lignes, &ids)?;
    retrait.courant = match courant_avant {
        _ if retrait.longueur == 0 => None,
        Some(c) => Some(retrait.position_apres(c).min(retrait.longueur - 1)),
        None => None,
    };
    if let Some(c) = retrait.courant {
        repo.set_current_pos(zone_id, c)?;
    }
    Ok(retrait)
}

#[cfg(test)]
#[path = "file_technique_3094_tests.rs"]
mod tests;
