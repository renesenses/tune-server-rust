use super::*;

fn etat_cue() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.backend.execute_batch("INSERT INTO tracks (id, title, source, cue_media_path, cue_start_ms) VALUES (1, 'Tranche A', 'local', '/musique/retire/album.flac', 0), (2, 'Tranche B', 'local', '/musique/retire/album.flac', 60000), (3, 'Racine imbriquée', 'local', '/musique/retire/garde/album.flac', 0);").unwrap();
    state
}

#[tokio::test]
async fn les_tranches_cue_sont_comptees_sans_la_racine_conservee_8836() {
    let state = etat_cue();
    let mut ids =
        pistes_orphelines_sous(&state, "/musique/retire", &["/musique/retire/garde".into()]);
    ids.sort();
    assert_eq!(
        ids,
        vec![1, 2],
        "les deux tranches CUE doivent être proposées, pas la racine conservée"
    );
}

#[tokio::test]
async fn la_purge_cue_annonce_le_changement_de_bibliotheque_8836() {
    let state = etat_cue();
    let mut events = state.event_bus.subscribe();
    let bilan = executer_purge(&state, &[1, 2]);
    assert_eq!(bilan.purgees, 2);
    assert!(
        TrackRepo::with_backend(state.backend.clone())
            .get(3)
            .unwrap()
            .is_some()
    );
    let mut updated = false;
    while let Ok(event) = events.try_recv() {
        updated |= event.event_type == "library.updated";
    }
    assert!(
        updated,
        "la purge doit émettre library.updated pour retirer les albums de l'écran"
    );
}

#[tokio::test]
async fn sans_purge_aucun_evenement_de_bibliotheque_8836() {
    let state = etat_cue();
    let mut events = state.event_bus.subscribe();
    let bilan = executer_purge(&state, &[]);
    assert_eq!(bilan.purgees, 0);
    while let Ok(event) = events.try_recv() {
        assert_ne!(event.event_type, "library.updated");
    }
}

#[tokio::test]
async fn le_retrait_confirme_signale_les_suppressions_cue_refusees_8836() {
    let state = etat_cue();
    SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", r#"["/musique/retire", "/musique/conserve"]"#)
        .unwrap();
    state.backend.execute_batch("CREATE TRIGGER refuser_purge_8836 BEFORE DELETE ON tracks BEGIN SELECT RAISE(ABORT, 'suppression factice refusée'); END;").unwrap();
    let response = retirer_un_dossier(&state, "/musique/retire", Some(3))
        .unwrap_or_else(|_| panic!("le retrait doit rendre le bilan des suppressions refusées"));
    assert_eq!(response["orphan_tracks"], 3);
    assert_eq!(response["purged"], 0);
    assert_eq!(
        response["purge_failed"], 3,
        "les erreurs de suppression doivent être visibles dans la réponse"
    );
}

#[tokio::test]
async fn le_bilan_du_scan_compte_chaque_tranche_hors_perimetre_8836() {
    let state = etat_cue();
    let racines = vec!["/musique/retire/garde".to_string()];
    assert_eq!(
        super::super::scan::compter_cue_hors_perimetre_8836(&*state.backend, &racines, None),
        2
    );
    assert_eq!(
        super::super::scan::compter_cue_hors_perimetre_8836(
            &*state.backend,
            &racines,
            Some("/ailleurs")
        ),
        0
    );
    assert_eq!(
        super::super::scan::compter_cue_hors_perimetre_8836(&*state.backend, &[], None),
        0
    );
}
