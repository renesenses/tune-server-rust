//! Passe manuelle, curseur et rapport persistants, sans migration. Refs #6286.
use crate::state::AppState;
use axum::{
    Json,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tune_core::{
    audio::audit_integrite::{self, Resultat, Verdict},
    db::{
        backend::DbBackend,
        engine::{Engine, PostgresDialect, SqliteDialect},
        settings_repo::{SettingsRepo, sql},
    },
    taches_de_fond::{self, Tache, perimetre, priorite},
};

const CLE: &str = "audit_integrite_etat";
static ACTIF: AtomicBool = AtomicBool::new(false);
struct Garde;
impl Drop for Garde {
    fn drop(&mut self) {
        ACTIF.store(false, Ordering::Release);
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Etat {
    generation: String,
    status: String,
    total: u64,
    traites: u64,
    sains: u64,
    corrompus: u64,
    non_verifiables: u64,
    alertes_taille: u64,
    curseur: i64,
    dernier_id: i64,
    erreur: Option<String>,
    #[serde(default)]
    lecture_prioritaire: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Ligne {
    id: i64,
    chemin: String,
    taille: Option<u64>,
    taille_sans_nouvelle_date: bool,
    resultat: Resultat,
}

fn lire(db: &Arc<dyn DbBackend>) -> Result<Etat, String> {
    SettingsRepo::with_backend(db.clone())
        .get(CLE)?
        .map_or_else(
            || {
                Ok(Etat {
                    status: "idle".into(),
                    ..Default::default()
                })
            },
            |s| serde_json::from_str(&s).map_err(|e| e.to_string()),
        )
}
fn sauver(db: &Arc<dyn DbBackend>, etat: &Etat, ligne: Option<&Ligne>) -> Result<(), String> {
    let requete = match db.engine() {
        Engine::Sqlite => sql::upsert(&SqliteDialect),
        Engine::Postgres => sql::upsert(&PostgresDialect),
    };
    let date = chrono::Utc::now().to_rfc3339();
    let json = serde_json::to_string(etat).map_err(|e| e.to_string())?;
    let ligne = ligne
        .map(|l| {
            serde_json::to_string(l).map(|v| {
                (
                    format!("audit_integrite_ligne:{}:{:020}", etat.generation, l.id),
                    v,
                )
            })
        })
        .transpose()
        .map_err(|e| e.to_string())?;
    // Le curseur et le résultat sont validés ensemble : un redémarrage ne
    // saute aucun fichier et ne compte jamais deux fois le même résultat.
    db.write_tx(&mut |tx| {
        if let Some((cle, valeur)) = &ligne {
            tx.execute(&requete, &[cle, valeur, &date])?;
        }
        tx.execute(&requete, &[&CLE, &json, &date])?;
        Ok(())
    })
}
fn candidats(db: &Arc<dyn DbBackend>) -> String {
    let chemin = tune_core::db::track_repo::sql::CHEMIN_OUVRABLE;
    let scope = perimetre::clause_hors_racines(chemin, &perimetre::racines_exclues(db));
    // file_path est UNIQUE : la branche ordinaire peut avancer par la clé
    // primaire sans regrouper toute la bibliothèque à chaque page. Seules
    // les images CUE sont regroupées, une fois par fichier physique.
    format!(
        "SELECT t.id, t.file_path AS chemin, t.file_size AS taille, t.file_mtime AS date \
         FROM tracks t WHERE COALESCE(t.source, 'local') = 'local' \
         AND t.file_path IS NOT NULL AND t.file_path <> '' {scope} \
         UNION ALL \
         SELECT MIN(t.id) AS id, t.cue_media_path AS chemin, MAX(t.file_size) AS taille, MAX(t.file_mtime) AS date \
         FROM tracks t WHERE COALESCE(t.source, 'local') = 'local' \
         AND (t.file_path IS NULL OR t.file_path = '') \
         AND t.cue_media_path IS NOT NULL AND t.cue_media_path <> '' {scope} \
         AND NOT EXISTS (SELECT 1 FROM tracks p WHERE p.file_path = t.cue_media_path \
                         AND COALESCE(p.source, 'local') = 'local') \
         GROUP BY t.cue_media_path"
    )
}

async fn bloquant<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, Response> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| erreur(e.to_string()))?
        .map_err(erreur)
}
fn erreur(e: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": e})),
    )
        .into_response()
}

pub(super) async fn status(State(state): State<AppState>) -> Response {
    match bloquant(move || lire(&state.backend)).await {
        Ok(mut etat) => {
            if etat.status == "running" && !ACTIF.load(Ordering::Acquire) {
                etat.status = "paused".into();
            }
            etat.lecture_prioritaire = priorite::lecture_en_cours();
            Json(etat).into_response()
        }
        Err(e) => e,
    }
}

#[derive(Default, Deserialize)]
pub(super) struct Lancement {
    #[serde(default)]
    resume: bool,
}

pub(super) async fn start(
    State(state): State<AppState>,
    Query(options): Query<Lancement>,
) -> Response {
    if ACTIF
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return (StatusCode::CONFLICT, "Un audit est déjà en cours").into_response();
    }
    let garde = Garde;
    let db = state.backend.clone();
    let resultat = bloquant(move || {
        let mut etat = lire(&db)?;
        if !options.resume || !matches!(etat.status.as_str(), "running" | "paused" | "error") {
            let selection = candidats(&db);
            let ligne = db
                .query_one(
                    &format!("SELECT COUNT(*), COALESCE(MAX(id), 0) FROM ({selection}) candidats"),
                    &[],
                )?
                .ok_or("Comptage indisponible")?;
            etat = Etat {
                generation: uuid::Uuid::new_v4().to_string(),
                total: ligne[0].as_i64().unwrap_or(0) as u64,
                dernier_id: ligne[1].as_i64().unwrap_or(0),
                ..Default::default()
            };
        }
        etat.status = "running".into();
        etat.erreur = None;
        taches_de_fond::reprendre(&db, Tache::Integrite)?;
        sauver(&db, &etat, None)?;
        let prefixe = format!("audit_integrite_ligne:{}:%", etat.generation);
        db.execute(
            "DELETE FROM settings WHERE key LIKE 'audit_integrite_ligne:%' AND key NOT LIKE ?",
            &[&prefixe],
        )?;
        Ok(etat)
    })
    .await;
    let etat = match resultat {
        Ok(e) => e,
        Err(e) => return e,
    };
    let reponse = etat.clone();
    let suivi = state.background_tasks.begin(
        "integrity",
        "Audit d’intégrité de la bibliothèque",
        "integrity",
    );
    tokio::task::spawn_blocking(move || {
        let _garde = garde;
        let _suivi = suivi;
        let mut etat = etat;
        if let Err(e) = executer(&state.backend, &mut etat) {
            etat.status = "error".into();
            etat.erreur = Some(e.clone());
            tracing::error!(error = %e, "audit_integrite_echoue");
            if let Err(e) = sauver(&state.backend, &etat, None) {
                tracing::error!(error = %e, "audit_integrite_etat_non_persisté");
            }
        }
    });
    (StatusCode::ACCEPTED, Json(reponse)).into_response()
}

pub(super) async fn pause(State(state): State<AppState>) -> Response {
    match bloquant(move || taches_de_fond::mettre_en_pause(&state.backend, Tache::Integrite)).await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e,
    }
}

fn continuer() -> bool {
    while priorite::lecture_en_cours() {
        if taches_de_fond::est_en_pause(Tache::Integrite) {
            return false;
        }
        priorite::noter_cedee("integrity");
        std::thread::sleep(Duration::from_millis(200));
    }
    !taches_de_fond::est_en_pause(Tache::Integrite)
}
fn taille_changee_sans_date(
    meta: Option<&std::fs::Metadata>,
    taille_scan: Option<i64>,
    date_scan: Option<f64>,
) -> bool {
    let Some(meta) = meta else {
        return false;
    };
    let date = meta
        .modified()
        .ok()
        .and_then(|d| d.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64());
    taille_scan.is_some_and(|taille| taille >= 0 && meta.len() != taille as u64)
        && date
            .zip(date_scan)
            .is_some_and(|(a, b)| (a - b).abs() < 0.001)
}

fn executer(db: &Arc<dyn DbBackend>, etat: &mut Etat) -> Result<(), String> {
    loop {
        if !continuer() {
            etat.status = "paused".into();
            return sauver(db, etat, None);
        }
        // Relire le périmètre à chaque fichier : une exclusion nouvelle prend
        // effet dès la frontière suivante, sans relancer l'audit.
        let selection = candidats(db);
        let lot = db.query_many(&format!("SELECT id, chemin, taille, date FROM ({selection}) candidats WHERE id > ? AND id <= ? ORDER BY id LIMIT 100"), &[&etat.curseur, &etat.dernier_id])?;
        if lot.is_empty() {
            etat.status = "completed".into();
            return sauver(db, etat, None);
        }
        let racines = perimetre::racines_exclues(db);
        for ligne in lot {
            if racines != perimetre::racines_exclues(db) {
                break;
            }
            if !continuer() {
                etat.status = "paused".into();
                return sauver(db, etat, None);
            }
            let id = ligne[0].as_i64().ok_or("Identifiant de fichier absent")?;
            let chemin = ligne[1].as_string().ok_or("Chemin absent")?;
            let avant = std::fs::metadata(&chemin).ok();
            let taille = avant.as_ref().map(|m| m.len());
            let date = avant.as_ref().and_then(|m| m.modified().ok());
            let taille_sans_nouvelle_date =
                taille_changee_sans_date(avant.as_ref(), ligne[2].as_i64(), ligne[3].as_f64());
            let Some(mut resultat) = audit_integrite::verifier(Path::new(&chemin), continuer)
            else {
                etat.status = "paused".into();
                return sauver(db, etat, None);
            };
            let change = match std::fs::metadata(&chemin) {
                Ok(apres) => Some(apres.len()) != taille || apres.modified().ok() != date,
                Err(_) => avant.is_some(),
            };
            if change {
                resultat = Resultat {
                    verdict: Verdict::NonVerifiable,
                    code: "modifie_pendant_audit".into(),
                    erreur: None,
                    position_trames: None,
                    trames_decodees: resultat.trames_decodees,
                };
            }
            enregistrer(
                db,
                etat,
                &Ligne {
                    id,
                    chemin,
                    taille,
                    taille_sans_nouvelle_date,
                    resultat,
                },
            )?;
        }
    }
}

fn enregistrer(db: &Arc<dyn DbBackend>, etat: &mut Etat, ligne: &Ligne) -> Result<(), String> {
    let mut suivant = etat.clone();
    match ligne.resultat.verdict {
        Verdict::Sain => suivant.sains += 1,
        Verdict::Corrompu => suivant.corrompus += 1,
        Verdict::NonVerifiable => suivant.non_verifiables += 1,
    }
    suivant.alertes_taille += u64::from(ligne.taille_sans_nouvelle_date);
    suivant.traites += 1;
    suivant.curseur = ligne.id;
    sauver(db, &suivant, Some(ligne))?;
    // Même le statut d'erreur doit conserver le dernier curseur VALIDÉ.
    *etat = suivant;
    Ok(())
}

#[derive(Default, Deserialize)]
pub(super) struct Page {
    #[serde(default)]
    offset: u64,
    verdict: Option<Verdict>,
}
fn lignes(
    db: &Arc<dyn DbBackend>,
    etat: &Etat,
    offset: u64,
    limite: u64,
    verdict: Option<&Verdict>,
) -> Result<Vec<Ligne>, String> {
    let prefixe = format!("audit_integrite_ligne:{}:%", etat.generation);
    let filtre = verdict
        .map(|v| {
            let valeur = match v {
                Verdict::Sain => "sain",
                Verdict::Corrompu => "corrompu",
                Verdict::NonVerifiable => "non_verifiable",
            };
            let colonne = match db.engine() {
                Engine::Sqlite => "json_extract(value, '$.resultat.verdict')",
                Engine::Postgres => "(value::jsonb -> 'resultat' ->> 'verdict')",
            };
            format!(" AND {colonne} = '{valeur}'")
        })
        .unwrap_or_default();
    let rows = db.query_many(
        &format!(
            "SELECT value FROM settings WHERE key LIKE ? {filtre} ORDER BY key LIMIT ? OFFSET ?"
        ),
        &[
            &prefixe,
            &(limite as i64),
            &(offset.min(i64::MAX as u64) as i64),
        ],
    )?;
    rows.into_iter()
        .map(|r| {
            serde_json::from_str(&r[0].as_string().unwrap_or_default()).map_err(|e| e.to_string())
        })
        .collect()
}
pub(super) async fn report(State(state): State<AppState>, Query(page): Query<Page>) -> Response {
    match bloquant(move || {
        let etat = lire(&state.backend)?;
        let rows = lignes(
            &state.backend,
            &etat,
            page.offset,
            100,
            page.verdict.as_ref(),
        )?;
        Ok(serde_json::json!({"etat": etat, "lignes": rows}))
    })
    .await
    {
        Ok(r) => Json(r).into_response(),
        Err(e) => e,
    }
}
fn cellule(s: &str) -> String {
    // Échapper les guillemets ET neutraliser les formules des tableurs.
    let prefixe = if s.trim_start().starts_with(['=', '+', '-', '@', '\t', '\r']) {
        "'"
    } else {
        ""
    };
    format!("\"{prefixe}{}\"", s.replace('"', "\"\""))
}
fn ligne_csv(l: &Ligne) -> String {
    [
        l.chemin.clone(),
        match l.resultat.verdict {
            Verdict::Sain => "sain",
            Verdict::Corrompu => "corrompu",
            Verdict::NonVerifiable => "non vérifiable",
        }
        .into(),
        l.resultat.code.clone(),
        l.resultat.erreur.clone().unwrap_or_default(),
        l.resultat
            .position_trames
            .map(|n| n.to_string())
            .unwrap_or_default(),
        l.resultat.trames_decodees.to_string(),
        l.taille_sans_nouvelle_date.to_string(),
    ]
    .iter()
    .map(|s| cellule(s))
    .collect::<Vec<_>>()
    .join(",")
        + "\r\n"
}
pub(super) async fn csv(State(state): State<AppState>) -> Response {
    match bloquant(move || {
        let etat = lire(&state.backend)?;
        let mut csv = String::from("\u{feff}chemin,état,code,erreur,position_trames,trames_décodées,taille_sans_nouvelle_date\r\n");
        let mut offset = 0;
        loop { let rows = lignes(&state.backend, &etat, offset, 1000, None)?; if rows.is_empty() { break; } offset += rows.len() as u64; for l in rows { csv.push_str(&ligne_csv(&l)); } }
        Ok(csv)
    }).await {
        Ok(csv) => ([(header::CONTENT_TYPE, "text/csv; charset=utf-8"), (header::CONTENT_DISPOSITION, "attachment; filename=integrite-bibliotheque.csv")], csv).into_response(), Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Arc<dyn DbBackend> {
        let db: Arc<dyn DbBackend> =
            Arc::new(tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap());
        db.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT, updated_at TEXT); CREATE TABLE tracks (id INTEGER PRIMARY KEY, file_path TEXT, cue_media_path TEXT, file_size INTEGER, file_mtime REAL, source TEXT)").unwrap();
        db
    }
    #[test]
    fn rapport_et_curseur_survivent_a_la_relecture() {
        let db = base();
        let etat = Etat {
            generation: "temoin".into(),
            status: "paused".into(),
            curseur: 42,
            traites: 1,
            corrompus: 1,
            ..Default::default()
        };
        let ligne = Ligne {
            id: 42,
            chemin: "/musique/tronque.flac".into(),
            taille: Some(40),
            taille_sans_nouvelle_date: true,
            resultat: Resultat::defaut("decodage", "tronqué", Some(4096), 4096),
        };
        sauver(&db, &etat, Some(&ligne)).unwrap();
        let relu = lire(&db).unwrap();
        assert_eq!(
            relu.curseur, 42,
            "la reprise doit repartir après le fichier validé"
        );
        let rapport = lignes(&db, &relu, 0, 100, None).unwrap();
        assert_eq!(
            rapport.len(),
            1,
            "le curseur ne doit pas avancer sans rapport"
        );
        assert_eq!(rapport[0].resultat.position_trames, Some(4096));
        assert!(
            lignes(&db, &relu, 0, 100, Some(&Verdict::Sain))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            lignes(&db, &relu, 0, 100, Some(&Verdict::Corrompu))
                .unwrap()
                .len(),
            1
        );
        assert!(ligne_csv(&rapport[0]).contains("tronqué"));
        // Réécrire le même résultat reste idempotent.
        sauver(&db, &etat, Some(&ligne)).unwrap();
        assert_eq!(lignes(&db, &relu, 0, 100, None).unwrap().len(), 1);
    }
    #[test]
    fn echec_du_rapport_ne_valide_pas_le_curseur() {
        let db = base();
        let mut etat = Etat {
            generation: "temoin".into(),
            ..Default::default()
        };
        sauver(&db, &etat, None).unwrap();
        db.execute_batch("CREATE TRIGGER refuser_rapport BEFORE INSERT ON settings WHEN NEW.key LIKE 'audit_integrite_ligne:%' BEGIN SELECT RAISE(ABORT, 'disque plein'); END").unwrap();
        let ligne = Ligne {
            id: 42,
            chemin: "x.flac".into(),
            taille: None,
            taille_sans_nouvelle_date: false,
            resultat: Resultat::defaut("illisible", "absent", None, 0),
        };
        assert!(enregistrer(&db, &mut etat, &ligne).is_err());
        assert_eq!(
            etat.curseur, 0,
            "le statut d’erreur doit garder le curseur validé"
        );
        assert_eq!(
            lire(&db).unwrap().curseur,
            0,
            "un résultat perdu ne doit pas être sauté"
        );
    }
    #[test]
    fn selection_respecte_perimetre_source_et_image_cue_unique() {
        let db = base();
        db.execute_batch("INSERT INTO tracks VALUES (1, '/local/a.flac', NULL, 10, 1, 'local'), (2, NULL, '/local/image.flac', 20, 1, NULL), (3, NULL, '/local/image.flac', 20, 1, 'local'), (4, '/exclu/b.flac', NULL, 20, 1, 'local'), (5, '/distant/c.flac', NULL, 30, 1, 'qobuz')").unwrap();
        SettingsRepo::with_backend(db.clone())
            .set(perimetre::CLE_RACINES_EXCLUES, "[\"/exclu\"]")
            .unwrap();
        let rows = db
            .query_many(
                &format!("SELECT id FROM ({}) candidats ORDER BY id", candidats(&db)),
                &[],
            )
            .unwrap();
        let ids: Vec<_> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
        assert_eq!(
            ids,
            [1, 2],
            "exclusion, source distante ou dédoublonnage CUE ignoré"
        );
    }
    #[test]
    fn taille_changee_detectee_meme_si_date_preservee() {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("fichier.flac");
        std::fs::write(&chemin, [0u8; 100]).unwrap();
        let avant = std::fs::metadata(&chemin).unwrap();
        let date = avant.modified().unwrap();
        let date_scan = date
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let fichier = std::fs::OpenOptions::new()
            .write(true)
            .open(&chemin)
            .unwrap();
        fichier.set_len(50).unwrap();
        fichier
            .set_times(std::fs::FileTimes::new().set_modified(date))
            .unwrap();
        let apres = fichier.metadata().unwrap();
        assert!(
            taille_changee_sans_date(Some(&apres), Some(100), Some(date_scan)),
            "troncature avec date conservée invisible"
        );
        assert!(!taille_changee_sans_date(
            Some(&apres),
            Some(50),
            Some(date_scan)
        ));
    }
    #[test]
    fn csv_echappe_chemin_et_formules() {
        assert_eq!(cellule("=1+1"), "\"'=1+1\"");
        assert_eq!(
            cellule("/musique/\"été\",1.flac"),
            "\"/musique/\"\"été\"\",1.flac\""
        );
    }
}
