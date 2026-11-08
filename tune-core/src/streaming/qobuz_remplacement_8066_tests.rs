//! #8066 — remplacer un titre d'une playlist Qobuz à sa place, contre un faux
//! Qobuz sur la boucle locale qui tient l'état de la playlist (aucun appel à
//! Qobuz, jeton factice).

use super::*;
use serde_json::json;
use std::sync::{Arc, Mutex as StdMutex};

/// La playlist simulée : `(identifiant du titre, playlist_track_id)` dans
/// l'ordre, et le journal des écritures.
#[derive(Default)]
struct Etat {
    lignes: Vec<(u64, u64)>,
    prochain_ptid: u64,
    ecritures: Vec<String>,
    /// Qobuz ignore `updateTracksPosition` (contre-épreuve du contrôle final).
    ignorer_le_deplacement: bool,
}

fn params(texte: &str) -> HashMap<String, String> {
    texte
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            (
                k.to_string(),
                urlencoding::decode(&v.replace('+', " "))
                    .map(|s| s.into_owned())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

async fn faux_qobuz(
    lignes: &[u64],
    ignorer_le_deplacement: bool,
) -> (QobuzService, Arc<StdMutex<Etat>>) {
    let etat = Arc::new(StdMutex::new(Etat {
        lignes: lignes
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, 9000 + i as u64))
            .collect(),
        prochain_ptid: 9500,
        ignorer_le_deplacement,
        ..Default::default()
    }));
    let e = etat.clone();
    let app = axum::Router::new().fallback(
        move |uri: axum::http::Uri, corps: String| {
            let e = e.clone();
            async move {
                let mut s = e.lock().unwrap();
                let q = params(uri.query().unwrap_or(""));
                let f = params(&corps);
                match uri.path() {
                    "/playlist/get" => {
                        let items: Vec<_> = s
                            .lignes
                            .iter()
                            .map(|(id, ptid)| json!({"id": id, "playlist_track_id": ptid, "title": format!("t{id}")}))
                            .collect();
                        let _ = q;
                        axum::Json(json!({"id": 77, "tracks": {"items": items, "total": items.len()}}))
                    }
                    "/playlist/addTracks" => {
                        s.ecritures.push(format!("add {}", f["track_ids"]));
                        for id in f["track_ids"].split(',') {
                            let ptid = s.prochain_ptid;
                            s.prochain_ptid += 1;
                            s.lignes.push((id.parse().unwrap(), ptid));
                        }
                        axum::Json(json!({"status": "success"}))
                    }
                    "/playlist/updateTracksPosition" => {
                        s.ecritures.push(format!(
                            "move {} before {}",
                            f["playlist_track_ids"], f["insert_before"]
                        ));
                        if !s.ignorer_le_deplacement {
                            let ptid: u64 = f["playlist_track_ids"].parse().unwrap();
                            let avant: usize = f["insert_before"].parse().unwrap();
                            let i = s.lignes.iter().position(|(_, p)| *p == ptid).unwrap();
                            let ligne = s.lignes.remove(i);
                            let fin = s.lignes.len();
                            s.lignes.insert(avant.min(fin), ligne);
                        }
                        axum::Json(json!({"status": "success"}))
                    }
                    "/playlist/deleteTracks" => {
                        s.ecritures.push(format!("delete {}", f["playlist_track_ids"]));
                        let ptid: u64 = f["playlist_track_ids"].parse().unwrap();
                        s.lignes.retain(|(_, p)| *p != ptid);
                        axum::Json(json!({"status": "success"}))
                    }
                    autre => {
                        s.ecritures.push(format!("inattendu {autre}"));
                        axum::Json(json!({}))
                    }
                }
            }
        },
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (
        QobuzService::connecte_a_base_forcee(format!("http://{adresse}")),
        etat,
    )
}

fn ids(etat: &Arc<StdMutex<Etat>>) -> Vec<u64> {
    etat.lock()
        .unwrap()
        .lignes
        .iter()
        .map(|(id, _)| *id)
        .collect()
}

#[tokio::test]
async fn le_titre_est_remplace_a_sa_place_8066() {
    let (svc, etat) = faux_qobuz(&[1, 2, 3, 4], false).await;
    svc.replace_playlist_track("77", 1, "2", "20")
        .await
        .expect("remplacement");
    assert_eq!(
        ids(&etat),
        [1, 20, 3, 4],
        "position conservée, ancien retiré"
    );
    assert_eq!(
        etat.lock().unwrap().ecritures,
        ["add 20", "move 9500 before 1", "delete 9001"],
        "ajout, déplacement, puis retrait de l'ancien : rien n'est perdu en route"
    );
}

#[tokio::test]
async fn une_playlist_qui_a_change_n_est_pas_ecrite_8066() {
    let (svc, etat) = faux_qobuz(&[1, 2, 3], false).await;
    let e = svc.replace_playlist_track("77", 1, "3", "20").await;
    assert!(e.is_err(), "le rang 1 porte 2, pas 3");
    assert!(etat.lock().unwrap().ecritures.is_empty(), "aucune écriture");
    assert_eq!(ids(&etat), [1, 2, 3]);
}

/// Si Qobuz n'applique pas le déplacement, l'ajout est annulé : l'ancien
/// titre reste à sa place, et le remplacement ne se déclare pas réussi.
#[tokio::test]
async fn une_position_non_confirmee_annule_l_ajout_8066() {
    let (svc, etat) = faux_qobuz(&[1, 2, 3], true).await;
    let e = svc.replace_playlist_track("77", 0, "1", "10").await;
    assert!(
        matches!(&e, Err(err) if err.to_string().contains("position non confirmée")),
        "{e:?}"
    );
    assert_eq!(
        ids(&etat),
        [1, 2, 3],
        "playlist inchangée : rien n'est perdu"
    );
    assert_eq!(
        etat.lock().unwrap().ecritures,
        ["add 10", "move 9500 before 0", "delete 9500"],
        "c'est le titre AJOUTÉ qui est retiré, jamais l'ancien"
    );
}
