//! #6019 (relance de Tades, 10/10, serveur Windows) — le scan MANUEL se tait
//! pendant le préfiltre `stat` qui suit le parcours, et la phase `files` ne
//! nomme plus le dossier en cours. Le vrai `spawn_library_scan_avec_lecteur`,
//! des WAV et une base SQLite de fichier.
use super::*;
use std::time::Duration;

fn wav(i: u8) -> Vec<u8> {
    let data = vec![i; 2048];
    let mut wav = b"RIFF".to_vec();
    wav.extend((36u32 + data.len() as u32).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(44100u32.to_le_bytes());
    wav.extend(88200u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend((data.len() as u32).to_le_bytes());
    wav.extend(data);
    wav
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_scan_manuel_annonce_son_prefiltre_et_le_dossier_en_cours_6019() {
    let _seul = serialiser_les_scans_de_test_sans_bloquer().await;
    let dir =
        tune_core::test_scratch::scratch_dir_in(std::env::current_dir().unwrap(), "scan-6019");
    let musique = dir.join("musique");
    let branche = musique.join("Jazz").join("Album");
    std::fs::create_dir_all(&branche).unwrap();
    for i in 0..3u8 {
        std::fs::write(branche.join(format!("piste-{i}.wav")), wav(i)).unwrap();
    }
    let state =
        AppState::new(dir.join("tune.db").to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "music_dirs",
            &serde_json::to_string(&[musique.to_str().unwrap()]).unwrap(),
        )
        .unwrap();
    settings.set("enrich_on_scan", "false").unwrap();

    let mut rx = state.event_bus.subscribe();
    let lecteur: LecteurMetadonnees = std::sync::Arc::new(|_| Default::default());
    assert!(
        spawn_library_scan_avec_lecteur(
            state.clone(),
            false,
            None,
            None,
            lecteur,
            DELAI_LECTURE_CREDITS
        )
        .await
    );
    // (type, données) dans l'ordre d'émission.
    let mut evenements: Vec<(String, Value)> = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(90), rx.recv())
            .await
            .expect("le scan #6019 doit rendre la main")
            .unwrap();
        let fini = event.event_type == "library.scan.completed";
        evenements.push((event.event_type.clone(), event.data.clone()));
        if fini {
            attendre_que_le_droit_de_scanner_soit_libre().await;
            break;
        }
    }

    // La fin du préfiltre est marquée par le second `library.scan.started`,
    // celui qui porte `to_scan`.
    let fin_du_prefiltre = evenements
        .iter()
        .position(|(t, d)| t == "library.scan.started" && d.get("to_scan").is_some())
        .expect("le scan doit annoncer la fin du préfiltre");
    let pendant_le_prefiltre: Vec<&Value> = evenements[..fin_du_prefiltre]
        .iter()
        .filter(|(t, d)| t == "library.scan.progress" && d["stage"] == "verification")
        .map(|(_, d)| d)
        .collect();
    assert!(
        pendant_le_prefiltre
            .iter()
            .any(|d| d["total"] == 3 && d["scanned"] == 0),
        "#6019 : le préfiltre `stat` doit annoncer son total dès son départ, \
         sinon l'écran reste figé sur le dernier message du parcours : {evenements:?}"
    );
    assert!(
        pendant_le_prefiltre
            .iter()
            .any(|d| d["total"] == 3 && d["scanned"] == 3),
        "#6019 : le préfiltre doit compter les fichiers qu'il a vérifiés : {evenements:?}"
    );

    // Les annonces `stage: extended_metadata` (lecture_bornee.rs) nommaient
    // déjà le fichier lu ; ce sont les annonces de LOT et d'ouverture de la
    // phase qui ne le faisaient pas. On les isole.
    let branche_txt = branche.to_string_lossy().into_owned();
    assert!(
        evenements.iter().any(|(t, d)| t == "library.scan.progress"
            && d["phase"] == "files"
            && d.get("stage").is_none()
            && d["total"] == 3
            && d["current_dir"] == branche_txt.as_str()),
        "#6019 : la phase `files` doit nommer la branche en cours ({branche_txt}) : {evenements:?}"
    );
}
