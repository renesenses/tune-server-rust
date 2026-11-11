//! #8325 — la passe de relecture : inscrite, pausable avec l'enrichissement,
//! une seule fois, armée au démarrage.

use super::*;

#[test]
fn la_relecture_se_suspend_avec_l_enrichissement_8325() {
    assert_eq!(
        crate::background_tasks::tache_suspendable(TACHE),
        Some(Tache::Enrichissement),
        "la passe doit se garer quand l'utilisateur suspend l'enrichissement"
    );
}

#[test]
fn la_relecture_est_armee_au_demarrage_8325() {
    let source = include_str!("../../background.rs");
    assert!(
        source.contains("relecture_classique::armer(state)"),
        "le démarrage n'arme plus la relecture des balises classiques"
    );
}

#[tokio::test]
async fn la_passe_s_execute_une_fois_et_se_clot_8325() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO tracks (title, file_path) VALUES ('A', '/inexistant/a.flac')",
            &[],
        )
        .unwrap();
    assert!(executer(&state).await, "la première passe tourne");
    assert!(rc::est_faite(&state.backend), "la passe est close");
    assert!(
        state
            .background_tasks
            .snapshot()
            .iter()
            .all(|t| t.id != TACHE),
        "la tâche quitte le registre en finissant"
    );
    assert!(!executer(&state).await, "une passe close ne repart pas");
}
