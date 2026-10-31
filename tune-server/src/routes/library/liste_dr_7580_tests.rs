//! #7580 (rc10) — la LISTE d'Oxygen filtrée par Dynamic Range
//! (`GET /library/tracks?dr=…`, `SELECT t.id, COUNT(*) OVER () …`) : ~1,5 s
//! sur le .18 une fois le rail réparé (#2144, #7571).
//!
//! Cause (plan lu au banc) : le prédicat DR était la sous-requête
//! `facet_filter::dr_album_in` — non corrélée, donc jouée une fois par
//! requête, mais qui relit À CHAQUE FOIS toutes les lignes DR de
//! `track_metadata`, les regroupe par album et calcule leur DR : 75 ms sur
//! 150 pour la page au banc de 103 500 pistes, et deux fois par page sur le
//! chemin `list_filtered` (le compte, puis la page). Elle se pose désormais
//! en liste d'albums résolue sur la carte DR gardée sous le jeton des
//! données (`facet_filter::condition_dr_par_albums`).
//!
//! * `la_liste_filtree_par_dr_rend_ce_que_rendait_la_sous_requete_7580` :
//!   mêmes pistes, même ordre, même total, mêmes comptes par provenance que
//!   le prédicat d'avant, sous plusieurs combinaisons de filtres ;
//! * `la_liste_filtree_par_dr_ne_relit_plus_les_tags_7580` : le temps (rougit
//!   sans le correctif) ;
//! * `banc_liste_dr_7580` (ignoré) : la mesure à 100 000 pistes.

use std::time::{Duration, Instant};

use super::facettes_lentes_2144_tests::{Profil, remplir};
use crate::state::AppState;
use tune_core::db::engine::Engine;
use tune_core::db::facet_filter as ff;
use tune_core::db::track_repo::{ColonneDeTri, DemandeDePistes, TrackFilter, TrackRepo};

fn base(nom: &str, p: &Profil) -> (tempfile::TempDir, AppState) {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join(format!("{nom}.db"));
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    remplir(&state, p);
    (dossier, state)
}

/// Les pistes que retenait le prédicat d'AVANT #7580 — la sous-requête
/// `dr_album_in`, jouée seule, valeurs inscrites en clair.
fn pistes_de_l_ancien_predicat(state: &AppState, drs: &[i64]) -> Vec<i64> {
    let liste = drs
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let having = format!("{} IN ({liste})", ff::DR_ALBUM_VALUE);
    let sql = format!(
        "SELECT t.id FROM tracks t WHERE {}",
        ff::dr_album_in(Engine::Sqlite, &having)
    );
    state
        .backend
        .query_many(&sql, &[])
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

/// La même demande, le DR remplacé par l'ensemble que rendait l'ancien
/// prédicat (en `t.id IN (…)`, la forme des collections intelligentes) :
/// les autres conditions restent celles du code, ET-combinées comme avant.
fn reference(state: &AppState, d: &DemandeDePistes) -> DemandeDePistes {
    let mut r = d.clone();
    r.filtre.collection_track_ids =
        Some(pistes_de_l_ancien_predicat(state, &d.filtre.dynamic_ranges));
    r.filtre.dynamic_ranges.clear();
    r
}

fn ids(pistes: &[tune_core::db::models::Track]) -> Vec<i64> {
    pistes.iter().filter_map(|t| t.id).collect()
}

fn demande(drs: &[i64]) -> DemandeDePistes {
    DemandeDePistes {
        filtre: TrackFilter {
            dynamic_ranges: drs.to_vec(),
            ..Default::default()
        },
        limit: 1_000_000,
        offset: 0,
        avec_comptes: true,
        ..Default::default()
    }
}

/// Les DR présents, du plus porté au moins porté (en albums).
fn dr_presents(state: &AppState) -> Vec<i64> {
    let carte = ff::dr_par_album(state.backend.as_ref()).unwrap();
    let mut n: std::collections::BTreeMap<i64, usize> = Default::default();
    for dr in carte.values() {
        *n.entry(*dr).or_default() += 1;
    }
    let mut v: Vec<(i64, usize)> = n.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(dr, _)| dr).collect()
}

#[test]
fn la_liste_filtree_par_dr_rend_ce_que_rendait_la_sous_requete_7580() {
    let (_d, state) = base(
        "equivalence",
        &Profil {
            albums: 280,
            pistes: 6,
            copie_tous: 9,
            cles_de_remplissage: false,
        },
    );
    let repo = TrackRepo::with_backend(state.backend.clone());
    let drs = dr_presents(&state);
    assert!(drs.len() >= 6, "trop peu de DR au banc : {drs:?}");
    let (courant, rare) = (drs[0], *drs.last().unwrap());

    let mut cas: Vec<(&str, DemandeDePistes)> = Vec::new();
    cas.push(("un DR", demande(&[courant])));
    cas.push(("deux DR", demande(&[courant, drs[2]])));
    cas.push(("DR rare", demande(&[rare])));
    cas.push(("DR absent", demande(&[99])));
    cas.push(("DR absent et présent", demande(&[99, drs[1]])));
    let mut d = demande(&[courant, rare]);
    d.filtre.genres = vec!["Jazz".into()];
    cas.push(("DR + genre", d));
    let mut d = demande(&[drs[1]]);
    d.filtre.original_years = vec![1990, 1991, 1992, 1993];
    cas.push(("DR + année d'origine", d));
    let mut d = demande(&[courant]);
    d.recherche = Some("Piste 1".into());
    cas.push(("DR + recherche", d));
    let mut d = demande(&[courant, drs[1]]);
    d.provenance = Some("local".into());
    cas.push(("DR + provenance", d));
    let mut d = demande(&[courant, drs[3]]);
    d.tri = Some(ColonneDeTri::Format);
    d.descendant = true;
    cas.push(("DR + tri par format, décroissant", d));
    let mut d = demande(&[courant, drs[3]]);
    d.tri = Some(ColonneDeTri::Annee);
    cas.push(("DR + tri par année", d));

    let mut non_vides = 0;
    for (nom, d) in &cas {
        let r = reference(&state, d);
        let a = repo.page_de_pistes(d).unwrap();
        let b = repo.page_de_pistes(&r).unwrap();
        assert_eq!(ids(&a.pistes), ids(&b.pistes), "{nom} : pistes ou ordre");
        assert_eq!(a.total, b.total, "{nom} : total");
        assert_eq!(a.comptes, b.comptes, "{nom} : comptes par provenance");
        if !a.pistes.is_empty() {
            non_vides += 1;
        }
        // Une fenêtre au milieu : même découpe.
        let (mut dp, mut rp) = (d.clone(), r.clone());
        (dp.limit, dp.offset, rp.limit, rp.offset) = (7, 5, 7, 5);
        assert_eq!(
            ids(&repo.page_de_pistes(&dp).unwrap().pistes),
            ids(&repo.page_de_pistes(&rp).unwrap().pistes),
            "{nom} : fenêtre 5..12"
        );
        // L'autre chemin de la route (`list_filtered`) : son ORDER BY n'est
        // pas total, on compare l'ensemble et le total.
        let (mut la, ta) = repo.list_filtered(&d.filtre, 1_000_000, 0).unwrap();
        let (mut lb, tb) = repo.list_filtered(&r.filtre, 1_000_000, 0).unwrap();
        assert_eq!(ta, tb, "{nom} : total de list_filtered");
        la.sort_by_key(|t| t.id);
        lb.sort_by_key(|t| t.id);
        assert_eq!(ids(&la), ids(&lb), "{nom} : pistes de list_filtered");
    }
    assert!(
        non_vides >= 8,
        "le banc doit rendre des pistes : {non_vides}"
    );

    // Une écriture périme la carte : la liste suit la base, pas la mémoire.
    let album = state
        .backend
        .query_many(
            &format!(
                "SELECT dr.album_id FROM ({}) dr WHERE dr.dr <> {rare} LIMIT 1",
                ff::dr_album_source(Engine::Sqlite)
            ),
            &[],
        )
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    let avant = repo.page_de_pistes(&demande(&[rare])).unwrap().total;
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO track_metadata (track_id, key, value) \
             SELECT id, 'dr_album', '{rare}' FROM tracks WHERE album_id = {album}; \
             DELETE FROM track_metadata WHERE key = 'dr_album' AND value <> '{rare}' \
             AND track_id IN (SELECT id FROM tracks WHERE album_id = {album});"
        ))
        .unwrap();
    let d = demande(&[rare]);
    let apres = repo.page_de_pistes(&d).unwrap();
    assert!(apres.total > avant, "{avant} → {}", apres.total);
    assert_eq!(
        ids(&apres.pistes),
        ids(&repo.page_de_pistes(&reference(&state, &d)).unwrap().pistes),
        "après une écriture"
    );
}

fn meilleur<F: FnMut()>(mut f: F) -> Duration {
    f();
    let mut m = Duration::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        f();
        m = m.min(t.elapsed());
    }
    m
}

/// Le témoin de temps : la page d'un DR RARE (quelques albums) coûtait au
/// moins la sous-requête — la relecture de tous les tags DR de la base ;
/// elle ne coûte plus que ses pistes. Borne : le tiers de cette relecture,
/// mesurée seule sur la même base.
#[test]
fn la_liste_filtree_par_dr_ne_relit_plus_les_tags_7580() {
    let (_d, state) = base(
        "temoin",
        &Profil {
            albums: 3_000,
            pistes: 10,
            copie_tous: 40,
            cles_de_remplissage: false,
        },
    );
    let repo = TrackRepo::with_backend(state.backend.clone());
    let rare = *dr_presents(&state).last().unwrap();
    let sous_requete = format!(
        "SELECT COUNT(*) FROM ({}) dr",
        ff::dr_album_source(Engine::Sqlite)
    );
    let relecture = meilleur(|| {
        let _ = state.backend.query_many(&sous_requete, &[]).unwrap();
    });
    let mut d = demande(&[rare]);
    (d.limit, d.avec_comptes) = (2_000, false);
    let page = meilleur(|| {
        let _ = repo.page_de_pistes(&d).unwrap();
    });
    let borne = relecture / 3;
    eprintln!("#7580 page DR{rare} : {page:?} ; relecture des tags : {relecture:?}");
    assert!(
        page <= borne,
        "la page du DR {rare} prend {page:?}, au-delà de {borne:?} (le tiers de la relecture \
         de tous les tags DR, {relecture:?}) : elle relit encore les tags à chaque page"
    );
}

/// Banc : `BANC_ALBUMS=10000 cargo test --release -p tune-server --lib
/// banc_liste_dr_7580 -- --ignored --nocapture` (le générateur de #2144).
#[test]
#[ignore = "banc manuel, --release"]
fn banc_liste_dr_7580() {
    let albums: i64 = std::env::var("BANC_ALBUMS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let (_d, state) = base(
        "banc",
        &Profil {
            albums,
            pistes: 10,
            copie_tous: 40,
            cles_de_remplissage: true,
        },
    );
    let repo = TrackRepo::with_backend(state.backend.clone());
    let n = state
        .backend
        .query_many("SELECT COUNT(*) FROM tracks", &[])
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    eprintln!("BANC pistes={n}");
    for drs in [vec![], vec![12], vec![8, 12]] {
        let mut d = demande(&drs);
        (d.limit, d.avec_comptes) = (2_000, false);
        let mut total = 0;
        let t = meilleur(|| total = repo.page_de_pistes(&d).unwrap().total);
        eprintln!(
            "BANC page_de_pistes dr={drs:?} total={total} {:.1} ms",
            t.as_secs_f64() * 1e3
        );
        d.avec_comptes = true;
        let t = meilleur(|| total = repo.page_de_pistes(&d).unwrap().total);
        eprintln!(
            "BANC page_de_pistes+comptes dr={drs:?} {:.1} ms",
            t.as_secs_f64() * 1e3
        );
        if !drs.is_empty() {
            let t = meilleur(|| total = repo.list_filtered(&d.filtre, 2_000, 0).unwrap().1);
            eprintln!(
                "BANC list_filtered dr={drs:?} total={total} {:.1} ms",
                t.as_secs_f64() * 1e3
            );
        }
    }
    // La première page après une écriture : la carte DR est relue.
    state
        .backend
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS banc_froid (x INTEGER); INSERT INTO banc_froid VALUES (1);",
        )
        .unwrap();
    let mut d = demande(&[12]);
    (d.limit, d.avec_comptes) = (2_000, false);
    let t = Instant::now();
    let _ = repo.page_de_pistes(&d).unwrap();
    eprintln!(
        "BANC page_de_pistes dr=[12], carte DR relue {:.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
}
