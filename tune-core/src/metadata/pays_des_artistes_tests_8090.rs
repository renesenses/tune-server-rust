//! #8090 — témoins de la passe « pays des artistes ». Réponses MusicBrainz
//! FACTICES, aucune requête.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const MB_BREL: &str = "0b5b9b6c-6c1a-4c55-9a0b-7a3f4a608090";
const MB_STROMAE: &str = "0b5b9b6c-6c1a-4c55-9a0b-7a3f4a618090";
const MB_SANS_PAYS: &str = "0b5b9b6c-6c1a-4c55-9a0b-7a3f4a628090";
const MB_VARIOUS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn artiste(b: &Arc<dyn DbBackend>, nom: &str, mbid: Option<&str>) -> i64 {
    b.execute_returning_id(
        "INSERT INTO artists (name, musicbrainz_id) VALUES (?, ?)",
        &[&nom as &dyn ToSqlValue, &mbid as &dyn ToSqlValue],
    )
    .unwrap()
}

fn pays(b: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
    let l = b
        .query_one(
            "SELECT country, country_mbid FROM artists WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    (
        l.first().and_then(|v| v.as_string()),
        l.get(1).and_then(|v| v.as_string()),
    )
}

fn fiche_mb(mbid: &str) -> Value {
    match mbid {
        MB_BREL => json!({"id": mbid, "name": "Jacques Brel", "country": "BE",
            "area": {"name": "Belgium", "iso-3166-1-codes": ["BE"]}}),
        // Pas de `country`, mais une zone qui est un pays.
        MB_STROMAE => json!({"id": mbid, "name": "Stromae", "country": null,
            "area": {"name": "Belgium", "iso-3166-1-codes": ["be"]}}),
        // Une ville pour zone : aucun code, rien n'est déduit.
        _ => json!({"id": mbid, "name": "Quelqu'un", "country": null,
            "area": {"name": "Bruxelles"}}),
    }
}

#[test]
fn le_pays_se_lit_dans_country_puis_dans_la_zone_8090() {
    assert_eq!(pays_de_la_fiche(&fiche_mb(MB_BREL)).as_deref(), Some("BE"));
    assert_eq!(
        pays_de_la_fiche(&fiche_mb(MB_STROMAE)).as_deref(),
        Some("BE")
    );
    assert_eq!(pays_de_la_fiche(&fiche_mb(MB_SANS_PAYS)), None);
    // Les pseudo-codes de MusicBrainz ne sont pas des pays.
    for code in ["XW", "XE", "XU", "", "FRA", "1A"] {
        assert_eq!(pays_de_la_fiche(&json!({"country": code})), None, "{code}");
    }
}

/// Un artiste au MBID résolu reçoit le pays de sa fiche ; une fiche sans pays
/// range `''` et n'est plus interrogée ; un pseudo-artiste n'est jamais
/// interrogé ; un artiste sans MBID n'a rien. Un MBID CHANGÉ rend le pays
/// caduc : la passe relit la nouvelle fiche.
#[tokio::test]
async fn la_passe_range_le_pays_de_l_artiste_et_suit_son_mbid_8090() {
    let b = base();
    let brel = artiste(&b, "Jacques Brel", Some(MB_BREL));
    let stromae = artiste(&b, "Stromae", Some(&MB_STROMAE.to_uppercase()));
    let sans_pays = artiste(&b, "Quelqu'un", Some(MB_SANS_PAYS));
    let various = artiste(&b, "Various Artists", Some(MB_VARIOUS));
    let sans_mbid = artiste(&b, "Inconnu", None);

    let requetes = std::sync::Mutex::new(Vec::new());
    let interroger = |mbid: String| {
        requetes.lock().unwrap().push(mbid.clone());
        let f = fiche_mb(&mbid);
        async move { Ok(f) }
    };
    let bilan = rattraper_les_pays(&b, 0, 100, &interroger).await.unwrap();
    assert_eq!((bilan.poses, bilan.sans_pays, bilan.pannes), (2, 2, 0));
    assert!(bilan.fin_de_liste);
    assert_eq!(pays(&b, brel), (Some("BE".into()), Some(MB_BREL.into())));
    assert_eq!(
        pays(&b, stromae),
        (Some("BE".into()), Some(MB_STROMAE.into()))
    );
    assert_eq!(
        pays(&b, sans_pays),
        (Some(String::new()), Some(MB_SANS_PAYS.into()))
    );
    assert_eq!(
        pays(&b, various),
        (Some(String::new()), Some(MB_VARIOUS.into()))
    );
    assert_eq!(pays(&b, sans_mbid), (None, None));
    assert!(
        !requetes.lock().unwrap().iter().any(|m| m == MB_VARIOUS),
        "un pseudo-artiste n'est pas interrogé"
    );

    // Second tour : rien à relire.
    requetes.lock().unwrap().clear();
    let second = rattraper_les_pays(&b, 0, 100, &interroger).await.unwrap();
    assert_eq!(second.traitees, 0);
    assert!(requetes.lock().unwrap().is_empty());

    // Le MBID de Brel change : son pays est relu sur la nouvelle fiche.
    b.execute(
        "UPDATE artists SET musicbrainz_id = ? WHERE id = ?",
        &[&MB_SANS_PAYS as &dyn ToSqlValue, &brel as &dyn ToSqlValue],
    )
    .unwrap();
    let troisieme = rattraper_les_pays(&b, 0, 100, &interroger).await.unwrap();
    assert_eq!(troisieme.traitees, 1);
    assert_eq!(
        pays(&b, brel),
        (Some(String::new()), Some(MB_SANS_PAYS.into()))
    );
}

/// Une panne n'écrit rien et arrête le tour SUR la fiche en panne.
#[tokio::test]
async fn une_panne_arrete_le_tour_sans_rien_ecrire_8090() {
    let b = base();
    let brel = artiste(&b, "Jacques Brel", Some(MB_BREL));
    let stromae = artiste(&b, "Stromae", Some(MB_STROMAE));
    let bilan = rattraper_les_pays(&b, 0, 100, |mbid: String| async move {
        if mbid == MB_STROMAE {
            Err(RefusMusicBrainz::Statut(503))
        } else {
            Ok(fiche_mb(&mbid))
        }
    })
    .await
    .unwrap();
    assert_eq!(bilan.dernier_id, brel);
    assert_eq!(bilan.pannes, 1);
    assert!(!bilan.fin_de_liste);
    assert_eq!(pays(&b, stromae), (None, None));
}

/// Le pays n'est pas écrit si le MBID a changé pendant la lecture.
#[test]
fn le_pays_n_est_pas_range_sur_un_autre_mbid_8090() {
    let b = base();
    let brel = artiste(&b, "Jacques Brel", Some(MB_BREL));
    assert!(!ranger_le_pays(&b, brel, MB_STROMAE, "BE").unwrap());
    assert_eq!(pays(&b, brel), (None, None));
    assert!(ranger_le_pays(&b, brel, MB_BREL, "BE").unwrap());
}
