//! Témoins T17 : pas de relances en rafale, la dernière demande servie,
//! aucun arrêt de flux en local.

use super::*;
use crate::playback::PlayState;

/// Simule la tâche d'une zone sur une horloge fictive : rend les instants
/// (en ms depuis le début) des relances, pour des demandes aux instants
/// `demandes` (ms).
fn simuler(demandes: &[u64]) -> Vec<u64> {
    let t0 = Instant::now();
    let a = |ms: u64| t0 + Duration::from_millis(ms);
    let mut f = FileDeRelance::default();
    let mut relances = Vec::new();
    let mut prochaines = demandes.to_vec();
    prochaines.sort();
    let mut i = 0;
    let mut tache_reveil: Option<u64> = None;
    let mut maintenant = 0u64;
    loop {
        // L'événement suivant : une demande ou le réveil de la tâche.
        let demande = prochaines.get(i).copied();
        let suivant = match (demande, tache_reveil) {
            (Some(d), Some(r)) => d.min(r),
            (Some(d), None) => d,
            (None, Some(r)) => r,
            (None, None) => break,
        };
        maintenant = maintenant.max(suivant);
        if demande == Some(suivant) {
            i += 1;
            if f.demander(7, a(maintenant)) {
                tache_reveil = Some(maintenant);
            }
            continue;
        }
        // Réveil de la tâche.
        loop {
            match f.prochaine(7, a(maintenant)) {
                Etape::Finir => {
                    tache_reveil = None;
                    break;
                }
                Etape::Attendre(d) => {
                    tache_reveil = Some(maintenant + d.as_millis() as u64);
                    break;
                }
                Etape::Relancer => {
                    relances.push(maintenant);
                    f.relancee(7, a(maintenant));
                }
            }
        }
    }
    relances
}

#[test]
fn une_rafale_donne_une_seule_relance_apres_l_anti_rebond() {
    // Dix chargements en une seconde (on clique « Load » partout).
    let demandes: Vec<u64> = (0..10).map(|k| k * 100).collect();
    let r = simuler(&demandes);
    assert_eq!(r, vec![900 + 500], "une relance, après la dernière demande");
}

#[test]
fn le_plancher_retarde_mais_ne_perd_jamais_la_derniere_demande() {
    // Une relance à 500 ms, puis une nouvelle demande à 1 s : le plancher
    // de 5 s la reporte à 5,5 s, il ne l'abandonne pas.
    let r = simuler(&[0, 1_000]);
    assert_eq!(r, vec![500, 5_500]);
}

#[test]
fn pas_de_rafale_sur_une_minute_de_chargements() {
    // Un chargement toutes les 300 ms pendant 60 s.
    let demandes: Vec<u64> = (0..200).map(|k| k * 300).collect();
    let r = simuler(&demandes);
    for w in r.windows(2) {
        assert!(w[1] - w[0] >= PLANCHER.as_millis() as u64, "{r:?}");
    }
    assert!(r.len() <= 60_000 / 5_000 + 1, "{} relances", r.len());
    // La dernière demande (59,7 s) est servie.
    assert!(*r.last().unwrap() >= 59_700, "{r:?}");
}

#[test]
fn une_seule_tache_par_zone_et_abandon_propre() {
    let t = Instant::now();
    let mut f = FileDeRelance::default();
    assert!(f.demander(1, t));
    assert!(!f.demander(1, t), "regroupée");
    assert!(f.demander(2, t), "autre zone, autre tâche");
    f.abandonner(1);
    assert_eq!(f.prochaine(1, t), Etape::Finir);
    assert!(f.demander(1, t), "après abandon, une nouvelle tâche");
}

#[test]
fn decider_selon_le_mode_la_sortie_et_l_etat() {
    use GesteTess::*;
    use ModeApplication::*;
    // Au prochain démarrage, ou mode inconnu : jamais d'interruption.
    for mode in [AtNextPlayback, ModeApplication::Unknown] {
        for etat in [PlayState::Playing, PlayState::Paused, PlayState::Stopped] {
            assert_eq!(decider(mode, etat, false, true), AuProchainDemarrage);
            assert_eq!(decider(mode, etat, true, true), AuProchainDemarrage);
        }
    }
    // Local : remplacement en vol, JAMAIS de relance (aucun clic d'arrêt).
    assert_eq!(decider(Immediately, PlayState::Playing, true, true), AChaud);
    assert_eq!(decider(Immediately, PlayState::Paused, true, true), AChaud);
    // Réseau : relance à la position, seulement si elle est mesurée.
    assert_eq!(
        decider(Immediately, PlayState::Playing, false, true),
        Relance
    );
    assert_eq!(
        decider(Immediately, PlayState::Playing, false, false),
        PositionInconnue
    );
    // #7556 — une zone en pause ne repart pas.
    assert_eq!(
        decider(Immediately, PlayState::Paused, false, true),
        EnPause
    );
    assert_eq!(
        decider(Immediately, PlayState::Stopped, false, true),
        RienNeJoue
    );
}
