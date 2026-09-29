//! Tune Circle T2 (#5325) : la liaison du serveur au compte, selon le contrat
//! de site-mozaiklabs#233 (« Contrat pour le greffon »), contre le faux cloud.
//!
//! `POST /api/v1/cloud-library/{server_id}/link` délivre un jeton ; chaque
//! `sync` de `library_sync` le présente dans `X-Tune-Server-Token`. Sur 401
//! `server_token_invalid` ou 403 `server_not_linked`, le lien est refait puis
//! la synchro réessayée UNE fois ; 403 `premium_required` est attendu.

mod commun;

use std::sync::Arc;
use std::time::Duration;

use tune_core::cloud::library_sync;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use commun::*;

fn api(faux: &Serveur) -> String {
    format!("{}/api/v1/cloud-library", faux.base)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Un serveur connecté, avec une piste en attente de synchro.
fn serveur(faux: &Serveur) -> Arc<dyn DbBackend> {
    let backend = base(&faux.base, Some(JETON));
    SettingsRepo::with_backend(backend.clone())
        .set("server_id", SERVEUR_DU_COMPTE)
        .unwrap();
    backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (20, 'Miles Davis');\
             INSERT INTO albums (id, title, artist_id) VALUES (10, 'Kind of Blue', 20);\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path) \
             VALUES (1, 'So What', 10, 20, '/tmp/so-what.flac');",
        )
        .unwrap();
    library_sync::populate_changelog_after_scan(&backend);
    backend
}

fn jeton_range(backend: &Arc<dyn DbBackend>) -> Option<String> {
    SettingsRepo::with_backend(backend.clone())
        .get(library_sync::CLE_JETON_DE_LIAISON)
        .unwrap()
}

fn ranger_un_jeton(backend: &Arc<dyn DbBackend>, jeton: &str) {
    SettingsRepo::with_backend(backend.clone())
        .set(library_sync::CLE_JETON_DE_LIAISON, jeton)
        .unwrap();
}

async fn pousser(faux: &Serveur, backend: &Arc<dyn DbBackend>) -> library_sync::SyncReport {
    // Borné : une synchro qui tournerait en rond sur un refus doit rougir,
    // pas pendre la porte.
    tokio::time::timeout(
        Duration::from_secs(20),
        library_sync::push_changes_vers(backend, &client(), &api(faux), SERVEUR_DU_COMPTE, JETON),
    )
    .await
    .expect("la synchro tourne en rond sur un refus du cloud")
    .unwrap()
}

#[tokio::test]
async fn la_liaison_range_le_jeton_du_cloud_sans_rien_envoyer() {
    let faux = demarrer().await;
    let backend = serveur(&faux);

    library_sync::lier_le_serveur(&backend, &client(), &api(&faux), SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    let premier = jeton_range(&backend).expect("le jeton doit être rangé");
    {
        let f = faux.etat.lock().unwrap();
        assert_eq!(f.jeton_de_liaison.as_deref(), Some(premier.as_str()));
        assert_eq!(f.liaisons, 1);
        assert_eq!(
            f.dernier_corps_liaison.as_deref(),
            Some(&b""[..]),
            "sans corps"
        );
    }

    // Chaque appel renouvelle : le jeton rangé suit.
    library_sync::lier_le_serveur(&backend, &client(), &api(&faux), SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    let second = jeton_range(&backend).unwrap();
    assert_ne!(second, premier);
    assert_eq!(
        faux.etat.lock().unwrap().jeton_de_liaison.as_deref(),
        Some(second.as_str())
    );
}

#[tokio::test]
async fn un_refus_de_liaison_ne_range_rien_et_ne_dit_que_le_statut() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().liaison_refusee = Some(404);
    let backend = serveur(&faux);
    let e =
        library_sync::lier_le_serveur(&backend, &client(), &api(&faux), SERVEUR_DU_COMPTE, JETON)
            .await
            .unwrap_err();
    assert_eq!(e, "cloud link HTTP 404");
    assert_eq!(jeton_range(&backend), None);
}

#[tokio::test]
async fn chaque_sync_presente_le_jeton_de_liaison() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().sync_exige_liaison = true;
    let backend = serveur(&faux);

    // Aucun jeton rangé : la synchro lie d'abord, une fois.
    let rapport = pousser(&faux, &backend).await;
    assert!(rapport.errors.is_empty(), "{:?}", rapport.errors);
    assert_eq!(library_sync::pending_count(&backend), 0);
    let jeton = jeton_range(&backend).unwrap();
    {
        let f = faux.etat.lock().unwrap();
        assert_eq!(f.liaisons, 1);
        assert!(!f.entetes_de_synchro.is_empty());
        assert!(
            f.entetes_de_synchro
                .iter()
                .all(|e| e.as_deref() == Some(jeton.as_str())),
            "{:?}",
            f.entetes_de_synchro
        );
    }

    // Un second passage réutilise le jeton rangé, sans relier.
    library_sync::record_change(&backend, "track", 1, "upsert");
    pousser(&faux, &backend).await;
    let f = faux.etat.lock().unwrap();
    assert_eq!(f.liaisons, 1);
    assert_eq!(
        f.entetes_de_synchro.last().unwrap().as_deref(),
        Some(jeton.as_str())
    );
}

#[tokio::test]
async fn un_jeton_ancien_fait_relier_puis_reessayer_une_fois() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().sync_exige_liaison = true;
    let backend = serveur(&faux);
    // Le cloud tient un jeton neuf ; le serveur, un ancien.
    library_sync::lier_le_serveur(&backend, &client(), &api(&faux), SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    ranger_un_jeton(&backend, "jeton-ancien-SECRET-5325");
    let liaisons_avant = faux.etat.lock().unwrap().liaisons;

    let rapport = pousser(&faux, &backend).await;
    assert!(rapport.errors.is_empty(), "{:?}", rapport.errors);
    assert_eq!(library_sync::pending_count(&backend), 0);
    let f = faux.etat.lock().unwrap();
    assert_eq!(f.liaisons, liaisons_avant + 1, "un seul nouveau lien");
    assert_eq!(
        f.entetes_de_synchro.len(),
        2,
        "une tentative refusée, un réessai : {:?}",
        f.entetes_de_synchro
    );
    assert_eq!(
        f.entetes_de_synchro[0].as_deref(),
        Some("jeton-ancien-SECRET-5325")
    );
    assert_eq!(f.entetes_de_synchro[1], f.jeton_de_liaison);
}

#[tokio::test]
async fn un_401_persistant_ne_reessaie_qu_une_fois() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().jeton_toujours_refuse = true;
    let backend = serveur(&faux);
    ranger_un_jeton(&backend, "jeton-ancien-SECRET-5325");
    let en_attente = library_sync::pending_count(&backend);

    let rapport = pousser(&faux, &backend).await;
    let f = faux.etat.lock().unwrap();
    assert_eq!(f.liaisons, 1, "un seul nouveau lien");
    assert_eq!(f.entetes_de_synchro.len(), 2, "un seul réessai");
    assert_eq!(rapport.errors.len(), 1, "{:?}", rapport.errors);
    assert!(rapport.errors[0].contains("401"), "{:?}", rapport.errors);
    assert_eq!(
        library_sync::pending_count(&backend),
        en_attente,
        "rien n'est marqué poussé"
    );
}

#[tokio::test]
async fn un_serveur_non_lie_fait_le_lien_puis_pousse() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().sync_exige_liaison = true;
    let backend = serveur(&faux);
    // Un jeton rangé que le cloud ne connaît pas : aucune liaison de son côté.
    ranger_un_jeton(&backend, "jeton-orphelin-SECRET-5325");

    let rapport = pousser(&faux, &backend).await;
    assert!(rapport.errors.is_empty(), "{:?}", rapport.errors);
    let f = faux.etat.lock().unwrap();
    assert_eq!(f.liaisons, 1);
    assert_eq!(f.entetes_de_synchro.len(), 2);
    assert_eq!(library_sync::pending_count(&backend), 0);
}

#[tokio::test]
async fn premium_requis_est_attendu_et_ne_fait_pas_relier() {
    let faux = demarrer().await;
    {
        let mut f = faux.etat.lock().unwrap();
        f.sync_exige_liaison = true;
        f.premium_requis = true;
    }
    let backend = serveur(&faux);
    library_sync::lier_le_serveur(&backend, &client(), &api(&faux), SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    let en_attente = library_sync::pending_count(&backend);

    let rapport = pousser(&faux, &backend).await;
    let f = faux.etat.lock().unwrap();
    assert_eq!(f.liaisons, 1, "aucun nouveau lien sur premium_required");
    assert_eq!(f.entetes_de_synchro.len(), 1, "aucun réessai");
    assert_eq!(rapport.errors.len(), 1);
    assert!(rapport.errors[0].contains("premium_required"));
    assert_eq!(library_sync::pending_count(&backend), en_attente);
}
