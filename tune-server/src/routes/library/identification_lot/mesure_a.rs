//! Mesure A de #4805 : `identify-all` rejoué, tel quel, sur une COPIE de
//! bibliothèque — jamais sur une base en service.
//!
//! C'est la boucle de la route ([`super::executer_le_lot`]) : même sélection,
//! même cadence MusicBrainz, même disjoncteur, mêmes écritures. Seules la
//! garde Premium et la réponse HTTP sont sautées : la mesure porte sur ce que
//! la passe identifie, pas sur le droit de la lancer.
//!
//! ```text
//! HOME=<dossier de mesure> TUNE_MESURE_A_DB=<copie>.db \
//!   TUNE_MESURE_A_SORTIE=<résultat>.json [TUNE_MESURE_A_LIMITE=N] \
//!   <binaire de test> --ignored --exact \
//!   routes::library::identification_lot::mesure_a::mesure_a_sur_une_copie
//! ```
//!
//! Refus d'ouvrir un chemin qui ressemble à une base en service (`/opt/`,
//! `/var/lib/`, `/srv/tune/`) : la copie se fait AVANT, à la main.

use std::time::Instant;

use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use super::{CLE_ETAT, executer_le_lot, sql_candidats_identification};
use crate::routes::library::reidentify::Identification;
use crate::state::AppState;

/// Le verdict de chaque album traité par la passe, dans l'ordre. Rempli par
/// [`noter`] (appelé par `executer_le_lot`, en test seulement).
static JOURNAL: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());

/// Garde le verdict d'un album : ce que la passe a cherché, et ce qu'elle a
/// conclu.
pub(super) fn noter(album_id: i64, issue: &Identification) {
    if let Ok(mut j) = JOURNAL.lock() {
        j.push(json!({
            "album_id": album_id,
            "verdict": issue.verdict,
            "titre": issue.searched_title,
            "artiste": issue.searched_artist,
            "pistes": issue.tracks_total,
            "raison_ambigu": issue.raison_ambigu,
            "refus": issue.refus_musicbrainz,
        }));
    }
}

fn compter(b: &std::sync::Arc<dyn DbBackend>, sql: &str) -> i64 {
    b.query_one(sql, &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

/// Le relevé, avant et après : les chiffres du §1 de #4805.
fn releve(b: &std::sync::Arc<dyn DbBackend>) -> Value {
    const LOCAL_AL: &str = "COALESCE(source, 'local') = 'local'";
    json!({
        "albums_locaux": compter(b, &format!("SELECT COUNT(*) FROM albums WHERE {LOCAL_AL}")),
        "albums_locaux_avec_release": compter(b, &format!(
            "SELECT COUNT(*) FROM albums WHERE {LOCAL_AL} \
             AND musicbrainz_release_id IS NOT NULL AND musicbrainz_release_id <> ''")),
        "albums_locaux_avec_release_group": compter(b, &format!(
            "SELECT COUNT(*) FROM albums WHERE {LOCAL_AL} \
             AND musicbrainz_release_group_id IS NOT NULL AND musicbrainz_release_group_id <> ''")),
        "albums_marques_tentes": compter(b,
            "SELECT COUNT(*) FROM albums WHERE identification_tentee_le IS NOT NULL"),
        "pistes_locales": compter(b, &format!("SELECT COUNT(*) FROM tracks WHERE {LOCAL_AL}")),
        "pistes_locales_avec_recording": compter(b, &format!(
            "SELECT COUNT(*) FROM tracks WHERE {LOCAL_AL} \
             AND musicbrainz_recording_id IS NOT NULL AND musicbrainz_recording_id <> ''")),
        "artistes_avec_mbid": compter(b,
            "SELECT COUNT(*) FROM artists WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id <> ''"),
        "artistes": compter(b, "SELECT COUNT(*) FROM artists"),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "mesure réseau de plusieurs heures, sur une copie : TUNE_MESURE_A_DB"]
async fn mesure_a_sur_une_copie() {
    let chemin = std::env::var("TUNE_MESURE_A_DB").expect("TUNE_MESURE_A_DB : la COPIE à mesurer");
    for interdit in ["/opt/", "/var/lib/", "/srv/tune/"] {
        assert!(
            !chemin.starts_with(interdit),
            "{chemin} ressemble à une base en service : copiez-la d'abord"
        );
    }
    let sortie = std::env::var("TUNE_MESURE_A_SORTIE").expect("TUNE_MESURE_A_SORTIE");
    let limite: Option<usize> = std::env::var("TUNE_MESURE_A_LIMITE")
        .ok()
        .and_then(|v| v.parse().ok());

    let state = AppState::new(&chemin, 0, Default::default()).expect("ouverture de la copie");
    let avant = releve(&state.backend);
    let mut albums: Vec<i64> = state
        .backend
        .query_many(sql_candidats_identification(), &[])
        .expect("sélection")
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let candidats = albums.len();
    if let Some(n) = limite {
        albums.truncate(n);
    }
    let lances = albums.len();
    eprintln!("mesure_a : {candidats} candidats, {lances} lancés");

    let debut = Instant::now();
    executer_le_lot(state.clone(), "mesure-a".into(), albums).await;
    let duree_s = debut.elapsed().as_secs_f64();

    let apres = releve(&state.backend);
    let etat: Value = SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_ETAT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let resultat = json!({
        "candidats": candidats,
        "lances": lances,
        "duree_s": duree_s.round(),
        "secondes_par_album": if lances > 0 { duree_s / lances as f64 } else { 0.0 },
        "avant": avant,
        "apres": apres,
        "etat_de_la_passe": etat,
        "albums": JOURNAL.lock().map(|j| j.clone()).unwrap_or_default(),
    });
    std::fs::write(&sortie, serde_json::to_string_pretty(&resultat).unwrap()).unwrap();
    let mut court = resultat.clone();
    court["albums"] = json!(court["albums"].as_array().map_or(0, Vec::len));
    eprintln!("{court:#}");
}

// ── Diagnostic des introuvables ─────────────────────────────────────────────

/// Les minuscules, lettres et chiffres seulement, espaces réduits (la
/// normalisation de `musicbrainz_release::normalize`, recopiée : elle n'est
/// pas publique hors de `tune-core`).
fn normaliser(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Le titre d'un pressage concorde-t-il avec celui cherché ? Égalité,
/// inclusion, ou égalité sans espaces : la règle de `plausible`.
fn titre_concorde(trouve: &str, cherche: &str) -> bool {
    let (a, b) = (normaliser(trouve), normaliser(cherche));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.contains(&b) || b.contains(&a) || a.replace(' ', "") == b.replace(' ', "")
}

/// Les mots qui signalent une édition ou un disque dans un titre.
const MARQUES_D_EDITION: &[&str] = &[
    "remaster",
    "deluxe",
    "edition",
    "version",
    "anniversary",
    "expanded",
    "bonus",
    "live",
    "mono",
    "stereo",
    "khz",
    "bit",
    "disc",
    "disk",
    "cd ",
    "cd1",
    "cd2",
    "vol",
    "volume",
    "part",
    "box",
    "collection",
    "original",
    "soundtrack",
    "ost",
    "hd",
    "hi res",
    "mqa",
];

/// Le titre porte-t-il encore une marque d'édition (parenthèse, crochet,
/// numéro de disque, mot d'édition) ?
fn drapeau_edition(titre: &str) -> bool {
    let t = format!("{} ", titre.to_lowercase());
    t.contains('(') || t.contains('[') || MARQUES_D_EDITION.iter().any(|m| t.contains(m))
}

/// Le titre ou l'artiste portent-ils des caractères qui gênent la recherche :
/// ponctuation de Lucene, apostrophes et guillemets typographiques, écriture
/// non latine ?
fn drapeau_caracteres(texte: &str) -> bool {
    texte.chars().any(|c| {
        "/:&+!?*~;#@’‘“”…\"".contains(c)
            || (c.is_alphabetic() && !(c.is_ascii() || ('\u{00C0}'..='\u{024F}').contains(&c)))
    })
}

/// Le relevé d'un album sur un tableau de pressages rendu par une recherche :
/// le premier dont le titre concorde, et son artiste.
fn premier_titre_concordant(data: &Value, titre: &str) -> Option<(String, String)> {
    data.get("releases")?.as_array()?.iter().find_map(|r| {
        let t = r.get("title")?.as_str()?;
        if !titre_concorde(t, titre) {
            return None;
        }
        let artiste = r
            .get("artist-credit")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join(" & ")
            })
            .unwrap_or_default();
        Some((t.to_string(), artiste))
    })
}

/// Classe les albums INTROUVABLES d'une mesure A par cause.
///
/// Sans réseau : compilation, marque d'édition, caractères. Avec deux sondes
/// MusicBrainz par album, au limiteur partagé : le titre seul (sans artiste)
/// rend-il un pressage du même titre ? Sinon, l'artiste seul a-t-il des
/// pressages chez MusicBrainz ?
///
/// Cause première, dans cet ordre : `compilation`, `artiste` (le titre existe
/// sous un autre crédit d'artiste), `titre` (l'artiste existe, pas ce titre),
/// `absence` (ni l'un ni l'autre).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "sondes MusicBrainz sur le résultat d'une mesure A : TUNE_MESURE_A_RESULTAT"]
async fn diagnostic_des_introuvables() {
    use tune_core::metadata::musicbrainz_release as mb;
    let entree = std::env::var("TUNE_MESURE_A_RESULTAT").expect("TUNE_MESURE_A_RESULTAT");
    let sortie = std::env::var("TUNE_MESURE_A_DIAGNOSTIC").expect("TUNE_MESURE_A_DIAGNOSTIC");
    let resultat: Value = serde_json::from_str(&std::fs::read_to_string(&entree).unwrap()).unwrap();
    let introuvables: Vec<&Value> = resultat["albums"]
        .as_array()
        .expect("le résultat ne porte pas la liste des albums")
        .iter()
        .filter(|a| a["verdict"] == "not_found" && a["refus"] != true)
        .collect();
    eprintln!("diagnostic : {} introuvables", introuvables.len());

    let mut causes: std::collections::BTreeMap<String, u64> = Default::default();
    let mut croisement: std::collections::BTreeMap<String, u64> = Default::default();
    let mut albums = Vec::new();
    let mut refus = 0u64;
    for a in introuvables {
        let titre = a["titre"].as_str().unwrap_or_default();
        let artiste = a["artiste"].as_str().unwrap_or_default();
        let cherche =
            mb::titre_de_requete_pour(titre, artiste).unwrap_or_else(|| titre.to_string());
        let compilation =
            mb::est_un_artiste_fictif(artiste) || normaliser(artiste) == "various artists";
        let edition = drapeau_edition(&cherche);
        let caracteres = drapeau_caracteres(titre) || drapeau_caracteres(artiste);

        mb::rate_limit_delay().await;
        let sonde_titre = mb::rechercher_sur_musicbrainz(
            format!(
                "release:\"{}\"",
                cherche.replace('\\', "\\\\").replace('"', "\\\"")
            ),
            25,
        )
        .await;
        let concordant = match &sonde_titre {
            Ok(d) => premier_titre_concordant(d, &cherche),
            Err(_) => {
                refus += 1;
                None
            }
        };
        let mut artiste_connu = None;
        if concordant.is_none() && !artiste.trim().is_empty() {
            mb::rate_limit_delay().await;
            artiste_connu = match mb::rechercher_sur_musicbrainz(
                format!(
                    "artist:\"{}\"",
                    artiste.replace('\\', "\\\\").replace('"', "\\\"")
                ),
                1,
            )
            .await
            {
                Ok(d) => Some(d["count"].as_u64().unwrap_or(0) > 0),
                Err(_) => {
                    refus += 1;
                    None
                }
            };
        }
        let cause = if compilation {
            "compilation"
        } else if concordant.is_some() {
            "artiste"
        } else if artiste_connu == Some(true) {
            "titre"
        } else if artiste_connu == Some(false) {
            "absence"
        } else {
            "indetermine"
        };
        *causes.entry(cause.to_string()).or_default() += 1;
        for (drapeau, vrai) in [("edition", edition), ("caracteres", caracteres)] {
            if vrai {
                *croisement.entry(format!("{cause}+{drapeau}")).or_default() += 1;
            }
        }
        albums.push(json!({
            "album_id": a["album_id"],
            "titre": titre,
            "cherche": cherche,
            "artiste": artiste,
            "pistes": a["pistes"],
            "cause": cause,
            "edition": edition,
            "caracteres": caracteres,
            "titre_trouve_sous": concordant.map(|(t, ar)| json!({"titre": t, "artiste": ar})),
            "artiste_connu": artiste_connu,
        }));
    }
    let bilan = json!({
        "introuvables": albums.len(),
        "causes": causes,
        "croisement": croisement,
        "refus_des_sondes": refus,
        "albums": albums,
    });
    std::fs::write(&sortie, serde_json::to_string_pretty(&bilan).unwrap()).unwrap();
    let mut court = bilan.clone();
    court["albums"] = json!(albums.len());
    eprintln!("{court:#}");
}

/// Rejoue la passe sur les seuls INTROUVABLES d'une mesure A, sur la même
/// copie, avec le code du moment : la mesure « après » des corrections de
/// recherche (#4805). Même boucle que la route (`executer_le_lot`), mêmes
/// écritures, sur la copie seulement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "rejeu réseau sur une copie : TUNE_MESURE_A_DB, TUNE_MESURE_A_RESULTAT"]
async fn rejeu_des_introuvables() {
    let chemin = std::env::var("TUNE_MESURE_A_DB").expect("TUNE_MESURE_A_DB : la COPIE");
    for interdit in ["/opt/", "/var/lib/", "/srv/tune/"] {
        assert!(
            !chemin.starts_with(interdit),
            "{chemin} : copiez-la d'abord"
        );
    }
    let entree = std::env::var("TUNE_MESURE_A_RESULTAT").expect("TUNE_MESURE_A_RESULTAT");
    let sortie = std::env::var("TUNE_MESURE_A_REJEU").expect("TUNE_MESURE_A_REJEU");
    let resultat: Value = serde_json::from_str(&std::fs::read_to_string(&entree).unwrap()).unwrap();
    let albums: Vec<i64> = resultat["albums"]
        .as_array()
        .expect("le résultat ne porte pas la liste des albums")
        .iter()
        .filter(|a| a["verdict"] == "not_found" && a["refus"] != true)
        .filter_map(|a| a["album_id"].as_i64())
        .collect();
    eprintln!("rejeu : {} introuvables", albums.len());
    let state = AppState::new(&chemin, 0, Default::default()).expect("ouverture de la copie");
    let avant = releve(&state.backend);
    let debut = Instant::now();
    executer_le_lot(state.clone(), "rejeu-introuvables".into(), albums.clone()).await;
    let journal = JOURNAL.lock().map(|j| j.clone()).unwrap_or_default();
    let mut verdicts: std::collections::BTreeMap<String, u64> = Default::default();
    for a in &journal {
        let cle = if a["refus"] == true {
            "refus".to_string()
        } else {
            a["verdict"].as_str().unwrap_or("?").to_string()
        };
        *verdicts.entry(cle).or_default() += 1;
    }
    let bilan = json!({
        "rejoues": albums.len(),
        "duree_s": debut.elapsed().as_secs_f64().round(),
        "verdicts": verdicts,
        "avant": avant,
        "apres": releve(&state.backend),
        "albums": journal,
    });
    std::fs::write(&sortie, serde_json::to_string_pretty(&bilan).unwrap()).unwrap();
    let mut court = bilan.clone();
    court["albums"] = json!(albums.len());
    eprintln!("{court:#}");
}
