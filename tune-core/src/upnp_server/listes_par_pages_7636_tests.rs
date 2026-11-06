//! #7636 — « Playlists » parcourue par Kazoo sur une grosse collection de
//! listes.
//!
//! Banc : 2 000 listes de 20 pistes (40 000 lignes de `playlist_tracks`),
//! écrites en SQL brut dans une transaction. On parcourt le rayon comme un
//! point de contrôle qui suit `TotalMatches`, on mesure, et on vérifie que
//! « tout » (`RequestedCount=0`) ne rend plus une réponse unique sans borne.

use super::*;
use std::collections::HashSet;
use std::time::{Duration, Instant};

const NB_LISTES: usize = 2_000;
const PISTES_PAR_LISTE: usize = 20;

/// Borne large : mesuré à environ 65 ms par page en debug sur la machine des
/// agents. Elle ne juge pas une milliseconde, elle attrape un parcours qui
/// deviendrait quadratique ou relirait les pistes de chaque liste.
const BORNE_PAR_PAGE: Duration = Duration::from_secs(2);

fn base_de_2000_listes() -> UpnpState {
    use crate::db::sqlite::SqliteDb;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();

    let mut sql = String::from("BEGIN;\n");
    for t in 1..=PISTES_PAR_LISTE {
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, track_number, duration_ms, file_path, format) \
             VALUES ({t}, 'Piste {t}', {t}, 200000, '/Musique/{t:03}.flac', 'flac');\n"
        ));
    }
    for l in 1..=NB_LISTES {
        sql.push_str(&format!(
            "INSERT INTO playlists (id, name, profile_id) VALUES ({l}, 'Liste {l:04}', {UPNP_PROFILE_ID});\n"
        ));
        for p in 0..PISTES_PAR_LISTE {
            sql.push_str(&format!(
                "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES ({l}, {t}, {p});\n",
                t = p + 1,
            ));
        }
    }
    sql.push_str("COMMIT;\n");
    db.execute_batch(&sql).unwrap();
    UpnpState::new(Arc::new(db), 8888, None)
}

/// Les identifiants `playlist/N` d'une page DIDL.
fn listes_de_la_page(xml: &str) -> Vec<String> {
    xml.match_indices("id=\"playlist/")
        .map(|(i, _)| {
            let reste = &xml[i + 4..];
            reste[..reste.find('"').unwrap()].to_string()
        })
        .collect()
}

#[test]
fn deux_mille_listes_se_parcourent_par_pages_bornees_et_rapides_7636() {
    let t_base = Instant::now();
    let state = base_de_2000_listes();
    eprintln!(
        "base de {NB_LISTES} listes écrite en {:?}",
        t_base.elapsed()
    );

    // « Tout » : une page bornée, mais le vrai total pour paginer.
    let debut = Instant::now();
    let tout = browse_direct_children(&state, "playlists", 0, 0);
    let duree_tout = debut.elapsed();
    assert_eq!(
        tout.total, NB_LISTES as u64,
        "TotalMatches doit annoncer toutes les listes, pas la taille de la page"
    );
    assert_eq!(
        tout.returned, PAGE_MAX_DES_LISTES,
        "RequestedCount=0 sur {NB_LISTES} listes doit rendre une page bornée à \
         {PAGE_MAX_DES_LISTES}, pas une réponse unique de {} listes",
        tout.returned
    );
    assert!(
        duree_tout < BORNE_PAR_PAGE,
        "Browse(playlists, 0, 0) a pris {duree_tout:?}"
    );

    // Le parcours d'un point de contrôle : pages de 100 jusqu'au total.
    let mut vues = HashSet::new();
    let mut pire = Duration::ZERO;
    let mut depart = 0u64;
    loop {
        let t0 = Instant::now();
        let page = browse_direct_children(&state, "playlists", depart, 100);
        pire = pire.max(t0.elapsed());
        assert_eq!(page.total, NB_LISTES as u64);
        let ids = listes_de_la_page(&page.xml);
        assert_eq!(ids.len() as u64, page.returned);
        if ids.is_empty() {
            break;
        }
        vues.extend(ids);
        depart += page.returned;
    }
    assert_eq!(
        vues.len(),
        NB_LISTES,
        "chaque liste doit apparaître une fois en suivant TotalMatches"
    );
    assert!(
        pire < BORNE_PAR_PAGE,
        "la page la plus lente a pris {pire:?}"
    );

    eprintln!("Browse(playlists, 0, 0) : {duree_tout:?} ; page de 100 la plus lente : {pire:?}");

    // Le conteneur racine annonce ce que le rayon ouvre.
    assert_eq!(
        compter_enfants_racine(&state, "playlists"),
        Some(NB_LISTES as u64)
    );
    // Et une liste s'ouvre sur ses pistes.
    let pistes = browse_direct_children(&state, "playlist/1", 0, 0);
    assert_eq!(pistes.total, PISTES_PAR_LISTE as u64);
}
