//! #8668 — témoins de la passe « pays d'édition ». Réponses MusicBrainz
//! FACTICES, aucune requête.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const R_FR: &str = "aaaaaaaa-0000-4000-8000-000000008668";
const R_EVENEMENT: &str = "aaaaaaaa-0000-4000-8000-000000018668";
const R_EUROPE: &str = "aaaaaaaa-0000-4000-8000-000000028668";
const R_RIEN: &str = "aaaaaaaa-0000-4000-8000-000000038668";
const R_INTROUVABLE: &str = "aaaaaaaa-0000-4000-8000-000000048668";
const R_PANNE: &str = "aaaaaaaa-0000-4000-8000-000000058668";
const R_BALISE: &str = "aaaaaaaa-0000-4000-8000-000000068668";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn album(b: &Arc<dyn DbBackend>, titre: &str, release: Option<&str>) -> i64 {
    b.execute_returning_id(
        "INSERT INTO albums (title, musicbrainz_release_id) VALUES (?, ?)",
        &[&titre as &dyn ToSqlValue, &release as &dyn ToSqlValue],
    )
    .unwrap()
}

fn pays(b: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
    let l = b
        .query_one(
            "SELECT release_country, release_country_mbid FROM albums WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    (
        l.first().and_then(|v| v.as_string()),
        l.get(1).and_then(|v| v.as_string()),
    )
}

fn release_mb(mbid: &str) -> Result<Value, RefusMusicBrainz> {
    match mbid {
        R_FR => Ok(json!({"id": mbid, "country": "FR"})),
        // Pas de `country`, mais un événement de sortie dans un pays.
        R_EVENEMENT => Ok(json!({"id": mbid, "country": null, "release-events": [
            {"date": "1972", "area": {"name": "Japan", "iso-3166-1-codes": ["jp"]}}]})),
        R_EUROPE => Ok(json!({"id": mbid, "country": "XE", "release-events": [
            {"area": {"name": "Europe", "iso-3166-1-codes": ["XE"]}}]})),
        R_RIEN => Ok(json!({"id": mbid, "country": "XU"})),
        R_INTROUVABLE => Err(RefusMusicBrainz::Statut(404)),
        R_PANNE => Err(RefusMusicBrainz::Statut(503)),
        _ => Ok(json!({"id": mbid, "country": "US"})),
    }
}

#[test]
fn le_pays_se_lit_dans_country_puis_dans_les_evenements_8668() {
    let lire = |m| pays_de_la_release(&release_mb(m).unwrap());
    assert_eq!(lire(R_FR).as_deref(), Some("FR"));
    assert_eq!(lire(R_EVENEMENT).as_deref(), Some("JP"));
    // Europe : pas un pays de la carte, mais une réponse (« hors carte »).
    assert_eq!(lire(R_EUROPE).as_deref(), Some("XE"));
    // Inconnu : rien.
    assert_eq!(lire(R_RIEN), None);
    // Un vrai pays dans les événements passe devant le pseudo-code.
    assert_eq!(
        pays_de_la_release(&json!({"country": "XW", "release-events": [
            {"area": {"iso-3166-1-codes": ["DE"]}}]}))
        .as_deref(),
        Some("DE")
    );
}

/// Les albums identifiés SANS balise reçoivent le pays de leur release ; un
/// album balisé n'est jamais interrogé (la balise prime) ; une release
/// introuvable range `''` ; une panne arrête le tour sans rien écrire, et le
/// tour suivant la reprend. Un identifiant CHANGÉ rend le pays caduc.
#[tokio::test]
async fn la_passe_range_le_pays_de_l_edition_et_suit_la_release_8668() {
    let b = base();
    let fr = album(&b, "Fr", Some(R_FR));
    let ev = album(&b, "Ev", Some(&R_EVENEMENT.to_uppercase()));
    let eu = album(&b, "Eu", Some(R_EUROPE));
    let rien = album(&b, "Rien", Some(R_RIEN));
    let introuvable = album(&b, "Introuvable", Some(R_INTROUVABLE));
    let sans_mbid = album(&b, "Sans", None);
    let balise = album(&b, "Balise", Some(R_BALISE));
    let piste = b
        .execute_returning_id(
            "INSERT INTO tracks (title, album_id, file_path, duration_ms, format, source) \
             VALUES ('p', ?, '/m/8668/p.flac', 1000, 'flac', 'local')",
            &[&balise as &dyn ToSqlValue],
        )
        .unwrap();
    b.execute(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'release_country', 'GB')",
        &[&piste as &dyn ToSqlValue],
    )
    .unwrap();

    let appels = Arc::new(AtomicUsize::new(0));
    let interroger = |m: String| {
        let appels = appels.clone();
        async move {
            appels.fetch_add(1, Ordering::SeqCst);
            release_mb(&m)
        }
    };
    let bilan = rattraper_les_editions(&b, 0, 50, interroger).await.unwrap();
    assert_eq!(bilan.traitees, 5, "{bilan:?}");
    assert_eq!(bilan.poses, 3, "{bilan:?}");
    assert_eq!(bilan.sans_pays, 2, "{bilan:?}");
    assert!(bilan.fin_de_liste);
    assert_eq!(
        appels.load(Ordering::SeqCst),
        5,
        "l'album balisé n'est pas interrogé"
    );
    assert_eq!(pays(&b, fr), (Some("FR".into()), Some(R_FR.into())));
    assert_eq!(pays(&b, ev), (Some("JP".into()), Some(R_EVENEMENT.into())));
    assert_eq!(pays(&b, eu).0.as_deref(), Some("XE"));
    assert_eq!(pays(&b, rien).0.as_deref(), Some(""));
    assert_eq!(pays(&b, introuvable).0.as_deref(), Some(""));
    assert_eq!(pays(&b, sans_mbid), (None, None));
    assert_eq!(pays(&b, balise), (None, None));

    // Un second tour ne relit rien.
    let bilan = rattraper_les_editions(&b, 0, 50, |m| async move { release_mb(&m) })
        .await
        .unwrap();
    assert_eq!(bilan.traitees, 0);

    // Une panne : rien n'est écrit, le curseur reste AVANT l'album en panne.
    let panne = album(&b, "Panne", Some(R_PANNE));
    let apres = album(&b, "Après", Some("aaaaaaaa-0000-4000-8000-000000098668"));
    let bilan = rattraper_les_editions(&b, 0, 50, |m| async move { release_mb(&m) })
        .await
        .unwrap();
    assert_eq!(bilan.pannes, 1);
    assert!(!bilan.fin_de_liste);
    assert!(bilan.dernier_id < panne, "{bilan:?}");
    assert_eq!(pays(&b, panne), (None, None));
    assert_eq!(pays(&b, apres), (None, None));

    // L'identifiant de release change : le pays est relu.
    b.execute(
        "UPDATE albums SET musicbrainz_release_id = 'aaaaaaaa-0000-4000-8000-0000000a8668' WHERE id = ?",
        &[&fr as &dyn ToSqlValue],
    )
    .unwrap();
    let ids: Vec<i64> = candidats(&b, 0, 50)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(ids.contains(&fr), "{ids:?}");
}
