//! #2144 (rc10, constat du 31/10 sur le .18 : 96 500 pistes, 9 336 albums) —
//! la facette Dynamic Range du rail d'Oxygen : 208 à 224 s par requête ; la
//! facette « année d'enregistrement » : 11 à 14 s (un DR coché).
//!
//! Cause (plan lu au banc) : la requête des seules pistes repliées du rail
//! compté par soustraction (#5993) pose `t.id IN (…)` à côté d'une jointure
//! par album ; SQLite prend les albums en boucle externe et rejoue la liste
//! par `idx_tracks_id_album (id=? AND album_id=?)` pour CHACUN. Voir
//! `facets::effectifs_par_album`.
//!
//! * `les_facettes_dr_et_annee_rendent_ce_que_rendait_la_jointure_2144` :
//!   mêmes valeurs, mêmes effectifs, même ordre que le SQL d'avant (recopié
//!   ici), sur toutes les formes de DR et d'année ;
//! * `les_facettes_dr_et_annee_ne_rejouent_plus_les_replis_2144` : le temps,
//!   rapporté à une facette de colonne sur la même base (rougit sans le
//!   correctif) ;
//! * `banc_facettes_dr_et_annee_2144` (ignoré) : la mesure à 100 000 pistes.

use std::time::{Duration, Instant};

use super::facets::{FacetQuery, SocleResolu, build_conditions, compter_les_facettes};
use crate::state::AppState;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::Engine;

pub(super) struct Profil {
    pub albums: i64,
    pub pistes: i64,
    /// Un album sur `copie_tous` porte une copie MP3 de chaque piste (repliée).
    pub copie_tous: i64,
    /// Six clés étendues de plus par piste, comme une bibliothèque réelle.
    pub cles_de_remplissage: bool,
}

/// `albums` albums de `pistes` pistes, dont : des DR d'album, des DR de piste
/// seuls (moyenne arrondie), des albums sans DR, des tags d'album divergents
/// (une piste sur deux), des valeurs invalides (« abc », « 1234 », vide) ;
/// des années d'origine nulles, à 0 et réelles ; des albums masqués, des
/// doubles distants et des copies MP3 (le socle replié).
pub(super) fn remplir(state: &AppState, p: &Profil) {
    let artistes = (p.albums / 4).max(5);
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=artistes {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    let doubles = (p.albums / 100).max(1);
    let mut id = 0_i64;
    for al in 1..=p.albums + doubles {
        let (titre, source, artiste) = if al <= p.albums {
            (format!("Album {al}"), "local", al % artistes + 1)
        } else {
            let local = (al - p.albums) * 50;
            (format!("Album {local}"), "upnp", local % artistes + 1)
        };
        let annee = match al % 6 {
            0 => "NULL".to_string(),
            1 => "0".to_string(),
            _ => (1950 + al % 70).to_string(),
        };
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source, original_year) \
             VALUES ({al}, '{titre}', {artiste}, '{source}', {annee});\n"
        ));
        let copies = if al <= p.albums && al % p.copie_tous == 0 {
            2
        } else {
            1
        };
        for copie in 0..copies {
            for n in 1..=p.pistes {
                id += 1;
                let format = if copie == 0 { "flac" } else { "mp3" };
                let chemin = if source == "upnp" {
                    format!("http://nas:9000/{al}/{n:02}.{format}")
                } else {
                    format!("/banc/Artiste {artiste}/Album {al}/{n:02}.{format}")
                };
                sql.push_str(&format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
                     file_path, duration_ms, format, sample_rate, bit_depth, source, album_artist, \
                     genre, year) VALUES ({id}, 'Piste {n}', {al}, {artiste}, 1, {n}, '{chemin}', \
                     240000, '{format}', 44100, 16, '{source}', '', '{g}', {an});\n",
                    g = ["Rock", "Jazz", "Classical", "Pop"][(al % 4) as usize],
                    an = 1960 + al % 60
                ));
                let mut cle = |k: &str, v: String| {
                    sql.push_str(&format!(
                        "INSERT INTO track_metadata (track_id, key, value) VALUES ({id}, '{k}', '{v}');\n"
                    ));
                };
                if p.cles_de_remplissage {
                    for k in [
                        "isrc",
                        "catalog_number",
                        "barcode",
                        "comment",
                        "bpm",
                        "rg_track_gain",
                    ] {
                        cle(k, format!("{k} {}", id % 97));
                    }
                }
                match al % 7 {
                    // Tag d'album sur chaque piste, et un DR de piste à côté.
                    0 => {
                        cle("dr_album", format!("{}", 6 + al % 10));
                        cle("dr_track", format!("{}", 3 + id % 13));
                    }
                    // DR de piste seuls : la moyenne arrondie fait la valeur.
                    1 | 2 => cle("dr_track", format!("{}", 5 + (id * 3 + al) % 11)),
                    // Tag d'album sur une piste sur deux, divergent : le MAX.
                    3 if n % 2 == 0 => cle("dr_album", format!("{}", 8 + n % 5)),
                    // Valeurs que la garde doit écarter, et une valide.
                    4 => {
                        let v = match n % 4 {
                            0 => "abc".to_string(),
                            1 => "1234".to_string(),
                            2 => String::new(),
                            _ => format!("{}", 9 + al % 4),
                        };
                        cle("dr_track", v);
                    }
                    // Le reste : aucun DR.
                    _ => {}
                }
            }
        }
        if al % 500 == 0 {
            sql.push_str("COMMIT;");
            state.backend.execute_batch(&sql).unwrap();
            sql = String::from("BEGIN;\n");
        }
    }
    for al in (5..=p.albums).step_by(211) {
        sql.push_str(&format!(
            "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', {al});\n"
        ));
    }
    sql.push_str("COMMIT;");
    state.backend.execute_batch(&sql).unwrap();
    state.backend.execute_batch("ANALYZE;").unwrap();
}

pub(super) fn requete(fields: &str, filtres: &str) -> FacetQuery {
    serde_json::from_value::<FacetQuery>(serde_json::json!({"fields": fields, "limit": 0}))
        .unwrap()
        .hydrate(Some(filtres))
        .ok()
        .expect("requête de facettes")
}

fn base(nom: &str, p: &Profil) -> (tempfile::TempDir, AppState) {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join(format!("{nom}.db"));
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    remplir(&state, p);
    (dossier, state)
}

/// Le SQL d'AVANT #2144 (rc10), recopié au caractère près, joué en UNE
/// requête sur le socle COMPLET (`t.id NOT IN (…)`), sans soustraction :
/// la référence de ce que la liste rend.
fn reference(state: &AppState, field: &str, filtres: &str) -> Vec<(String, i64)> {
    let q = requete(field, filtres);
    let socle = SocleResolu::resoudre(state);
    let (conds, params) = build_conditions(&q, Engine::Sqlite, field, None, &socle);
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let sql = match field {
        "dr" => format!(
            "SELECT dr.dr, COUNT(*) AS n FROM tracks t \
             JOIN ({source}) dr ON dr.album_id = t.album_id{where_clause} \
             GROUP BY dr.dr ORDER BY dr.dr DESC",
            source = tune_core::db::facet_filter::dr_album_source(Engine::Sqlite)
        ),
        "original_year" => format!(
            "SELECT alo.original_year, COUNT(*) AS n FROM tracks t \
             JOIN albums alo ON alo.id = t.album_id{where_clause} \
             {and_or_where} alo.original_year IS NOT NULL AND alo.original_year > 0 \
             GROUP BY alo.original_year ORDER BY alo.original_year DESC",
            and_or_where = if conds.is_empty() { "WHERE" } else { "AND" }
        ),
        _ => unreachable!(),
    };
    let bound: Vec<&dyn ToSqlValue> = params.iter().map(|v| v as &dyn ToSqlValue).collect();
    state
        .backend
        .query_many(&sql, &bound)
        .unwrap()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let v = it.next()?.as_i64()?;
            Some((v.to_string(), it.next()?.as_i64().unwrap_or(0)))
        })
        .collect()
}

fn rendu(state: &AppState, field: &str, filtres: &str) -> Vec<(String, i64)> {
    compter_les_facettes(state, requete(field, filtres))[field]
        .as_array()
        .expect("la facette est rendue")
        .iter()
        .map(|e| {
            (
                e["value"].as_str().unwrap().to_string(),
                e["count"].as_i64().unwrap(),
            )
        })
        .collect()
}

/// Mêmes tranches, mêmes effectifs, même ordre qu'avant, facette par facette
/// et sous chaque combinaison de filtres qui touche l'une ou l'autre.
#[test]
fn les_facettes_dr_et_annee_rendent_ce_que_rendait_la_jointure_2144() {
    let (_d, state) = base(
        "equivalence",
        &Profil {
            albums: 280,
            pistes: 6,
            copie_tous: 9,
            cles_de_remplissage: false,
        },
    );
    let socle = SocleResolu::resoudre(&state);
    assert!(
        socle.ids().is_some_and(|ids| !ids.is_empty()),
        "le banc doit replier des pistes : c'est le chemin par soustraction qui était lent"
    );
    let dr = rendu(&state, "dr", "");
    let valeurs: Vec<i64> = dr.iter().map(|(v, _)| v.parse().unwrap()).collect();
    // Le banc couvre bien : DR d'album (6..15), moyennes de pistes, tags
    // divergents, valeurs invalides écartées (aucun « 0 » ni « 1234 »).
    assert!(valeurs.len() >= 8, "trop peu de tranches : {dr:?}");
    assert!(valeurs.iter().all(|v| (3..=20).contains(v)), "{dr:?}");
    assert!(valeurs.windows(2).all(|w| w[0] > w[1]), "ordre : {dr:?}");
    let annee = rendu(&state, "original_year", "");
    assert!(annee.len() > 20, "{annee:?}");
    assert!(annee.iter().all(|(v, _)| v != "0"), "{annee:?}");
    let une_annee = annee[3].0.clone();
    let un_dr = dr[2].0.clone();
    for filtres in [
        String::new(),
        "genre=Jazz".to_string(),
        format!("dr={un_dr}"),
        format!("dr={un_dr}&dr={}", dr[0].0),
        format!("original_year={une_annee}"),
        format!("original_year={une_annee}&genre=Rock"),
        "dr=99".to_string(),
        "q=Piste%201".to_string(),
    ] {
        for field in ["dr", "original_year"] {
            assert_eq!(
                rendu(&state, field, &filtres),
                reference(&state, field, &filtres),
                "facette {field}, filtres « {filtres} »"
            );
        }
    }
    // `limit` tronque après l'ordre, comme le `LIMIT` d'avant.
    let q = serde_json::from_value::<FacetQuery>(
        serde_json::json!({"fields": "dr,original_year", "limit": 3}),
    )
    .unwrap()
    .hydrate(None)
    .ok()
    .unwrap();
    let v = compter_les_facettes(&state, q);
    assert_eq!(v["dr"].as_array().unwrap().len(), 3);
    assert_eq!(v["dr"][0]["value"].as_str().unwrap(), dr[0].0);
    assert_eq!(v["original_year"][0]["value"].as_str().unwrap(), annee[0].0);
}

fn duree(state: &AppState, field: &str, filtres: &str) -> Duration {
    let _ = compter_les_facettes(state, requete(field, filtres));
    let mut meilleur = Duration::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let _ = compter_les_facettes(state, requete(field, filtres));
        meilleur = meilleur.min(t.elapsed());
    }
    meilleur
}

/// Le témoin de temps : sans le correctif, DR et année (un DR coché) coûtent
/// albums tagués × pistes repliées ; avec, une lecture de la table des pistes,
/// comme une facette de colonne. On compare à `year` (colonne de `tracks`)
/// sur la MÊME base et le même socle : la borne est large (×15, + 300 ms).
#[test]
fn les_facettes_dr_et_annee_ne_rejouent_plus_les_replis_2144() {
    let (_d, state) = base(
        "temoin",
        &Profil {
            albums: 2_000,
            pistes: 4,
            copie_tous: 2,
            cles_de_remplissage: false,
        },
    );
    let replis = SocleResolu::resoudre(&state)
        .ids()
        .map(|i| i.len())
        .unwrap_or(0);
    assert!(replis > 3_000, "{replis} pistes repliées seulement");
    let repere = duree(&state, "year", "");
    let borne = repere * 15 + Duration::from_millis(300);
    for (field, filtres) in [("dr", ""), ("original_year", "dr=12")] {
        let d = duree(&state, field, filtres);
        eprintln!(
            "#2144 {field} « {filtres} » : {d:?} (repère year : {repere:?}, borne {borne:?})"
        );
        assert!(
            d <= borne,
            "facette {field} « {filtres} » : {d:?}, au-delà de {borne:?} (year : {repere:?})"
        );
    }
}

/// Banc de mesure : `BANC_ALBUMS=10000 cargo test --release -p tune-server --lib
/// banc_facettes_dr_et_annee_2144 -- --ignored --nocapture`.
#[test]
#[ignore = "banc manuel, --release"]
fn banc_facettes_dr_et_annee_2144() {
    let albums: i64 = std::env::var("BANC_ALBUMS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let t = Instant::now();
    let (_d, state) = base(
        "banc",
        &Profil {
            albums,
            pistes: 10,
            copie_tous: 40,
            cles_de_remplissage: true,
        },
    );
    let n = state
        .backend
        .query_many("SELECT COUNT(*) FROM tracks", &[])
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    let replis = SocleResolu::resoudre(&state)
        .ids()
        .map(|i| i.len())
        .unwrap_or(0);
    eprintln!(
        "BANC pistes={n} replis={replis} remplissage={:?}",
        t.elapsed()
    );
    for (field, filtres) in [
        ("dr", ""),
        ("original_year", ""),
        ("dr", "genre=Jazz"),
        ("original_year", "dr=12"),
        ("dr", "original_year=1990"),
        ("year", ""),
    ] {
        let d = duree(&state, field, filtres);
        eprintln!(
            "BANC {field:<14} {filtres:<20} {:>9.1} ms",
            d.as_secs_f64() * 1e3
        );
    }
    // La première ouverture, DR non gardé en mémoire : une écriture périme le jeton.
    state
        .backend
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS banc_froid (x INTEGER); INSERT INTO banc_froid VALUES (1);",
        )
        .unwrap();
    let t = Instant::now();
    let _ = compter_les_facettes(&state, requete("dr", ""));
    eprintln!(
        "BANC dr (DR par album relu)    {:>9.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
}
