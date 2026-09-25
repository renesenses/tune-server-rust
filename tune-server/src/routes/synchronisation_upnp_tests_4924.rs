//! Témoins de la CAUSE de #4924 : la synchronisation horaire des sources UPnP
//! tenait le verrou d'écriture SQLite plus de 4 min 30 sur le .18.
//!
//! Relevé de la sentinelle de #4945, le 24/09/2026 à 20 h 25 :
//! `ecriture_sqlite_toujours_tenue lieu=tune-core/src/db/backend.rs:602:42
//! fil=tokio-rt-worker tenue_ms=271295`. Une seule `write_tx` bouclait sur
//! 49 440 identités et, pour chacune, cherchait
//! `tracks WHERE source = 'upnp' AND source_id = ?` — colonne SANS index :
//! chaque recherche parcourait toutes les pistes UPnP.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Une base avec `n` pistes UPnP d'identités `u|<i>`.
fn base(n: usize) -> (AppState, Vec<String>) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let identites: Vec<String> = (0..n).map(|i| format!("u|{i:06}")).collect();
    state
        .backend
        .write_tx(&mut |tx| {
            for (i, s) in identites.iter().enumerate() {
                let titre = format!("Piste {i}");
                tx.execute(
                    "INSERT INTO tracks (title, source, source_id) VALUES (?, 'upnp', ?)",
                    &[&titre.as_str(), &s.as_str()],
                )?;
            }
            Ok(())
        })
        .unwrap();
    (state, identites)
}

/// La plus longue détention du verrou d'écriture VUE PAR UN AUTRE ÉCRIVAIN
/// pendant `travail` : un fil tente une écriture vide toutes les 2 ms et
/// garde son pire temps d'attente. Indépendant du moteur.
fn pire_attente_d_un_autre_ecrivain(db: Arc<dyn DbBackend>, travail: impl FnOnce()) -> Duration {
    let fin = Arc::new(AtomicBool::new(false));
    let (sonde_db, sonde_fin) = (db.clone(), fin.clone());
    let sonde = std::thread::spawn(move || {
        let mut pire = Duration::ZERO;
        while !sonde_fin.load(Ordering::Relaxed) {
            let t = Instant::now();
            sonde_db.write_tx(&mut |_| Ok(())).unwrap();
            pire = pire.max(t.elapsed());
            std::thread::sleep(Duration::from_millis(2));
        }
        pire
    });
    // La sonde doit être en place avant la première écriture.
    std::thread::sleep(Duration::from_millis(20));
    travail();
    fin.store(true, Ordering::Relaxed);
    sonde.join().unwrap()
}

fn membres(state: &AppState, key: &str) -> Vec<(i64, String)> {
    state
        .backend
        .query_many_strong(
            "SELECT track_id, generation FROM upnp_library_members WHERE source_key = ? ORDER BY track_id",
            &[&key],
        )
        .unwrap()
        .iter()
        .map(|r| (r[0].as_i64().unwrap(), r[1].as_str().unwrap().to_string()))
        .collect()
}

fn source_de_test(key: &str, generation: &str, pending: Vec<i64>) -> Source {
    Source {
        key: key.into(),
        udn: "u".into(),
        container: "0".into(),
        name: "NAS".into(),
        enabled: true,
        status: "ready".into(),
        last_attempt: now_seconds(),
        last_success: None,
        report: json!({}),
        generation: generation.into(),
        pending,
    }
}

/// 20 000 pistes UPnP synchronisées par le chemin de `run_one`. Aucun autre
/// écrivain ne doit attendre le verrou plus de `SEUIL`.
///
/// Sur le code d'avant : une seule transaction, 20 000 recherches non
/// indexées, soit 4 × 10⁸ lignes lues sous le verrou.
#[test]
fn la_synchronisation_de_20000_pistes_ne_tient_pas_le_verrou_d_ecriture() {
    const N: usize = 20_000;
    const SEUIL: Duration = Duration::from_millis(250);
    let (state, identites) = base(N);
    save(state.backend.as_ref(), &source_de_test("s", "g0", vec![])).unwrap();
    let debut = Instant::now();
    let mut rendu = None;
    let pire = pire_attente_d_un_autre_ecrivain(state.backend.clone(), || {
        rendu = Some(
            enregistrer_les_membres(state.backend.as_ref(), "s", "g1", &identites, true).unwrap(),
        );
    });
    let total = debut.elapsed();
    eprintln!(
        "gel4924_cause N={N} duree_totale_ms={} pire_attente_ecrivain_ms={}",
        total.as_millis(),
        pire.as_millis()
    );
    let rendu = rendu.unwrap();
    assert_eq!(rendu.avant, 0);
    assert_eq!(rendu.pending, Some(vec![]));
    assert_eq!(membres(&state, "s").len(), N);
    assert!(
        pire < SEUIL,
        "un autre écrivain a attendu le verrou {} ms (seuil {} ms) : la synchronisation le tient encore",
        pire.as_millis(),
        SEUIL.as_millis()
    );
}

/// Les membres, `pending` et le retrait : mêmes résultats qu'avant.
///
/// Jeu : pistes t1..t5 ; source `s` possède t1, t2, t3, t5 (génération `g0`) ;
/// t3 est ensuite supprimée (membre orphelin) ; une seconde source `s2`
/// possède aussi t2. La nouvelle passe voit t1 et t4.
#[test]
fn memes_membres_meme_pending_meme_retrait_qu_avant() {
    let (state, identites) = base(5);
    let ids: Vec<i64> = identites
        .iter()
        .map(|s| {
            state
                .backend
                .query_one_strong(
                    "SELECT id FROM tracks WHERE source = 'upnp' AND source_id = ?",
                    &[&s.as_str()],
                )
                .unwrap()
                .unwrap()[0]
                .as_i64()
                .unwrap()
        })
        .collect();
    let (t1, t2, t3, t4, t5) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
    save(state.backend.as_ref(), &source_de_test("s", "g0", vec![])).unwrap();
    save(state.backend.as_ref(), &source_de_test("s2", "h0", vec![])).unwrap();
    for (k, id, g) in [
        ("s", t1, "g0"),
        ("s", t2, "g0"),
        ("s", t3, "g0"),
        ("s", t5, "g0"),
        ("s2", t2, "h0"),
    ] {
        state
            .backend
            .execute(
                "INSERT INTO upnp_library_members (source_key, track_id, generation) VALUES (?, ?, ?)",
                &[&k, &id, &g],
            )
            .unwrap();
    }
    state
        .backend
        .execute("DELETE FROM tracks WHERE id = ?", &[&t3])
        .unwrap();

    // Une identité inconnue : erreur, et AUCUN membre ne passe à `g1`.
    let inconnue = vec![identites[0].clone(), "u|absente".to_string()];
    let e =
        enregistrer_les_membres(state.backend.as_ref(), "s", "g1", &inconnue, true).unwrap_err();
    assert_eq!(e, "piste indexée introuvable");
    assert!(
        membres(&state, "s").iter().all(|(_, g)| g != "g1"),
        "une passe en échec n'écrit aucun membre de sa génération"
    );

    // Passe incomplète : membres rafraîchis, pas de `pending`.
    let vues = vec![identites[0].clone(), identites[3].clone()];
    let r = enregistrer_les_membres(state.backend.as_ref(), "s", "g1", &vues, false).unwrap();
    assert_eq!(
        r,
        MembresDeLaPasse {
            avant: 3,
            pending: None
        },
        "l'orphelin t3 ne compte pas"
    );
    assert_eq!(
        membres(&state, "s"),
        vec![
            (t1, "g1".into()),
            (t2, "g0".into()),
            (t4, "g1".into()),
            (t5, "g0".into())
        ]
    );

    // Passe complète, même vue : `pending` = les membres d'une autre génération.
    let r = enregistrer_les_membres(state.backend.as_ref(), "s", "g2", &vues, true).unwrap();
    let mut pending = r.pending.clone().unwrap();
    pending.sort();
    assert_eq!(r.avant, 4);
    assert_eq!(pending, vec![t2, t5]);

    // Retrait : t2 reste (s2 la possède), t5 part ; s2 intacte.
    let source = source_de_test("s", "g2", pending);
    assert_eq!(
        remove_missing(state.backend.as_ref(), &source, false).unwrap(),
        1
    );
    assert_eq!(
        membres(&state, "s"),
        vec![(t1, "g2".into()), (t4, "g2".into())]
    );
    assert_eq!(membres(&state, "s2"), vec![(t2, "h0".into())]);
    let restantes: Vec<i64> = state
        .backend
        .query_many_strong("SELECT id FROM tracks ORDER BY id", &[])
        .unwrap()
        .iter()
        .filter_map(|r| r[0].as_i64())
        .collect();
    assert_eq!(restantes, vec![t1, t2, t4]);
}
