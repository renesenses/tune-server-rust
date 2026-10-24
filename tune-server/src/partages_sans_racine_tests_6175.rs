//! #6175 (fil 2145) — un partage SMB monté sans dossier déclaré ne laisse plus
//! la bibliothèque vide sans explication.
//!
//! Le rapport de Daniel LEVY (rc2) disait `Music dirs:` vide et rien d'autre,
//! alors que deux partages `…\Music` étaient « Monté ». Ces épreuves tiennent
//! les trois surfaces serveur qui constatent « aucun dossier » : le relevé
//! lui-même, le rapport de bug, et la fin d'un scan sans dossier.

use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn jamais_monte(_: &Path) -> bool {
    false
}

fn toujours_monte(_: &Path) -> bool {
    true
}

#[test]
fn un_partage_sans_aucune_racine_est_nomme() {
    let partages = parmi(&[], &s(&["/mnt/nas_Music"]), toujours_monte);
    assert_eq!(
        partages,
        vec![PartageSansRacine {
            point: "/mnt/nas_Music".into(),
            monte: true,
        }],
        "sans aucun dossier déclaré, le partage monté doit être nommé : c'est la cause \
         de la bibliothèque vide (#6175)"
    );
}

#[test]
fn une_racine_sur_sous_ou_au_dessus_du_partage_le_couvre() {
    let points = s(&["/mnt/nas_Music"]);
    for racine in [
        "/mnt/nas_Music",
        "/mnt/nas_Music/Jazz",
        "/mnt",
        "/mnt/nas_Music/",
    ] {
        assert!(
            parmi(&s(&[racine]), &points, toujours_monte).is_empty(),
            "la racine {racine} couvre le partage /mnt/nas_Music"
        );
    }
}

#[test]
fn un_voisin_de_nom_ne_couvre_pas_le_partage() {
    let partages = parmi(
        &s(&["/mnt/nas_Music2"]),
        &s(&["/mnt/nas_Music"]),
        toujours_monte,
    );
    assert_eq!(
        partages.len(),
        1,
        "/mnt/nas_Music2 ne couvre pas /mnt/nas_Music"
    );
}

#[test]
fn un_meme_point_enregistre_deux_fois_n_est_nomme_qu_une_fois() {
    let partages = parmi(
        &[],
        &s(&["/mnt/nas_Music", "/mnt/nas_Music/", "/mnt/nas_Music_v6"]),
        toujours_monte,
    );
    let points: Vec<_> = partages.iter().map(|p| p.point.as_str()).collect();
    assert_eq!(points, vec!["/mnt/nas_Music", "/mnt/nas_Music_v6"]);
}

#[test]
fn l_explication_dit_la_cause_quand_aucun_dossier_n_est_declare() {
    let partages = parmi(&[], &s(&["/mnt/nas_Music"]), toujours_monte);
    let phrase = explication(&[], &partages).expect("une explication");
    assert!(phrase.contains("la bibliothèque reste vide"), "{phrase}");
    assert!(phrase.contains("/mnt/nas_Music"), "{phrase}");
    assert!(phrase.contains("Ajouter à la bibliothèque"), "{phrase}");
    assert!(!phrase.contains("non monté"), "{phrase}");
}

#[test]
fn l_explication_signale_un_partage_non_monte() {
    let partages = parmi(&[], &s(&["/mnt/a", "/mnt/b"]), jamais_monte);
    let phrase = explication(&[], &partages).expect("une explication");
    assert!(
        phrase.contains("/mnt/a (non monté), /mnt/b (non monté)"),
        "{phrase}"
    );
    assert!(phrase.contains("Les partages SMB"), "{phrase}");
}

#[test]
fn rien_a_expliquer_sans_partage_orphelin() {
    assert_eq!(explication(&[], &[]), None);
    let partages = parmi(&s(&["/mnt/nas"]), &s(&["/mnt/nas"]), toujours_monte);
    assert_eq!(explication(&s(&["/mnt/nas"]), &partages), None);
}

fn etat_avec_un_partage(nom: &str) -> (crate::state::AppState, String) {
    let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let point = tune_core::test_scratch::scratch_dir(nom).join("nas_Music");
    std::fs::create_dir_all(&point).unwrap();
    let point_s = tune_core::scanner::walker::normalize_path(&point.to_string_lossy());
    etat.backend
        .execute(
            "INSERT INTO network_mounts (mount_type, server, share, mount_path) VALUES ('smb', ?, ?, ?)",
            &[
                &"192.168.10.69" as &dyn ToSqlValue,
                &"Music" as &dyn ToSqlValue,
                &point_s as &dyn ToSqlValue,
            ],
        )
        .unwrap();
    (etat, point_s)
}

#[test]
fn le_releve_lit_les_partages_smb_actifs_seulement() {
    let (etat, point) = etat_avec_un_partage("tune_6175_releve");
    etat.backend
        .execute(
            "INSERT INTO network_mounts (mount_type, server, share, mount_path, active) \
             VALUES ('smb', 'nas', 'Ancien', '/mnt/ancien', 0)",
            &[],
        )
        .unwrap();
    let partages = releve_avec(&etat.backend, &[], toujours_monte);
    let points: Vec<_> = partages.iter().map(|p| p.point.clone()).collect();
    assert_eq!(
        points,
        vec![point.clone()],
        "un partage désactivé n'est pas nommé"
    );
    assert!(releve_avec(&etat.backend, &[point], toujours_monte).is_empty());
}

async fn rapport_de_bug(etat: crate::state::AppState) -> serde_json::Value {
    let app = crate::routes::router(etat);
    let rep = app
        .oneshot(
            Request::get("/api/v1/system/bug-report")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rep.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

/// Le symptôme exact du fil 2145 : `Music dirs:` vide, un partage enregistré.
/// Le rapport doit relier les deux, au texte comme au JSON.
#[tokio::test]
async fn le_rapport_de_bug_nomme_le_partage_non_declare() {
    let (etat, point) = etat_avec_un_partage("tune_6175_rapport");
    let rapport = rapport_de_bug(etat).await;
    let md = rapport["markdown"].as_str().unwrap_or_default();
    let ligne = md
        .lines()
        .find(|l| l.starts_with("- Partages SMB sans dossier déclaré:"))
        .unwrap_or_else(|| {
            panic!(
                "#6175 : le rapport dit `Music dirs:` vide sans nommer le partage SMB \
                 enregistré — la bibliothèque reste vide sans explication.\n{md}"
            )
        });
    assert!(ligne.contains(&point), "{ligne}");
    assert!(ligne.contains("la bibliothèque reste vide"), "{ligne}");
    assert_eq!(
        rapport["library"]["mounted_shares_not_declared"][0]["mount_path"],
        point.as_str(),
        "{}",
        rapport["library"]
    );
}

#[tokio::test]
async fn le_rapport_se_tait_quand_le_partage_est_declare() {
    let (etat, point) = etat_avec_un_partage("tune_6175_rapport_declare");
    tune_core::db::settings_repo::SettingsRepo::with_backend(etat.backend.clone())
        .set("music_dirs", &serde_json::to_string(&vec![point]).unwrap())
        .unwrap();
    let rapport = rapport_de_bug(etat).await;
    let md = rapport["markdown"].as_str().unwrap_or_default();
    assert!(!md.contains("Partages SMB sans dossier déclaré"), "{md}");
    assert_eq!(
        rapport["library"]["mounted_shares_not_declared"],
        serde_json::json!([])
    );
}

/// Un scan lancé sans dossier finissait sur `{"no_dirs": true}` : l'écran
/// apprenait qu'il n'y avait rien à lire, pas que le partage monté suffisait.
#[tokio::test]
// Le verrou des scans d'épreuve est tenu à travers les `.await` à dessein :
// c'est lui qui tient les autres scans d'épreuve à l'écart.
#[allow(clippy::await_holding_lock)]
async fn le_scan_sans_dossier_nomme_le_partage_non_declare() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
    let (etat, point) = etat_avec_un_partage("tune_6175_scan");
    let mut rx = etat.event_bus.subscribe();
    assert!(
        crate::routes::system::scan::spawn_library_scan_par(
            etat.clone(),
            false,
            None,
            crate::routes::system::scan::ORIGINE_MANUELLE,
        )
        .await,
        "le scan doit partir"
    );
    let fin = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let e = rx.recv().await.expect("bus ouvert");
            if e.event_type == "library.scan.completed" {
                return e.data;
            }
        }
    })
    .await
    .expect("library.scan.completed attendu");
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
    assert_eq!(fin["no_dirs"], true, "{fin}");
    assert_eq!(
        fin["mounted_shares_not_declared"][0]["mount_path"],
        point.as_str(),
        "#6175 : la fin d'un scan sans dossier doit nommer le partage enregistré mais non \
         déclaré : {fin}"
    );
    let phrase = fin["explanation"].as_str().unwrap_or_default();
    assert!(phrase.contains(&point), "{fin}");
}
