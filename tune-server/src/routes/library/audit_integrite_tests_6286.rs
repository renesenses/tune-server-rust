//! #6286 (suite de #6828) — témoins de bout en bout, sur de vrais FLAC :
//! l'audit suit le périmètre des analyses (racines exclues ET dossiers inclus),
//! une relance « fichiers changés seulement » ne relit que les fichiers changés,
//! et le rappel est éteint par défaut.
use super::*;
use md5::{Digest, Md5};
use std::{collections::HashMap, path::PathBuf, time::SystemTime};

fn flac() -> Vec<u8> {
    let pcm: Vec<u8> = (0..24000i32)
        .flat_map(|n| ((n * 97) as i16).to_le_bytes())
        .collect();
    let mut encodeur = tune_core::audio::encoder::AudioEncoder::new("flac", 44100, 16, 2);
    encodeur.start_sync().unwrap();
    encodeur.write_sync(&pcm).unwrap();
    let mut fichier = encodeur.finish_sync().unwrap();
    // L'encodeur laisse le MD5 de STREAMINFO nul : la fixture lui donne celui
    // du PCM d'origine.
    fichier[26..42].copy_from_slice(&Md5::digest(&pcm));
    fichier
}
fn octet_modifie() -> Vec<u8> {
    let mut f = flac();
    f[26] ^= 1;
    f
}
fn tronque() -> Vec<u8> {
    let mut f = flac();
    f.truncate(f.len() / 2);
    f
}
fn md5_nul() -> Vec<u8> {
    let mut f = flac();
    f[26..42].fill(0);
    f
}

fn date(secondes: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secondes)
}

struct Banc {
    dossier: tempfile::TempDir,
    db: Arc<dyn DbBackend>,
    suivant: i64,
}

impl Banc {
    fn new() -> Self {
        let db: Arc<dyn DbBackend> =
            Arc::new(tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap());
        db.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT, updated_at TEXT); CREATE TABLE tracks (id INTEGER PRIMARY KEY, file_path TEXT, cue_media_path TEXT, file_size INTEGER, file_mtime REAL, source TEXT)").unwrap();
        Self {
            dossier: tempfile::tempdir().unwrap(),
            db,
            suivant: 1,
        }
    }
    fn chemin(&self, relatif: &str) -> PathBuf {
        self.dossier.path().join(relatif)
    }
    /// Écrit le fichier à une date de modification fixée.
    fn ecrire(&self, relatif: &str, octets: &[u8], le: SystemTime) {
        let chemin = self.chemin(relatif);
        std::fs::create_dir_all(chemin.parent().unwrap()).unwrap();
        std::fs::write(&chemin, octets).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&chemin)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(le))
            .unwrap();
    }
    /// Écrit le fichier et l'inscrit à la bibliothèque, comme le scan.
    fn piste(&mut self, relatif: &str, octets: &[u8], le: SystemTime) {
        self.ecrire(relatif, octets, le);
        let date_scan = le
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        self.db
            .execute(
                "INSERT INTO tracks (id, file_path, file_size, file_mtime, source) VALUES (?, ?, ?, ?, 'local')",
                &[
                    &self.suivant,
                    &self.chemin(relatif).to_string_lossy().into_owned(),
                    &(octets.len() as i64),
                    &date_scan,
                ],
            )
            .unwrap();
        self.suivant += 1;
    }
    fn regler(&self, cle: &str, dossiers: &[&str]) {
        let valeur: Vec<String> = dossiers
            .iter()
            .map(|d| self.chemin(d).to_string_lossy().into_owned())
            .collect();
        SettingsRepo::with_backend(self.db.clone())
            .set(cle, &serde_json::to_string(&valeur).unwrap())
            .unwrap();
    }
    fn auditer(&self, changed_only: bool) -> Etat {
        let mut etat = preparer(
            &self.db,
            &Lancement {
                resume: false,
                changed_only,
            },
        )
        .unwrap();
        executer(&self.db, &mut etat).unwrap();
        assert_eq!(
            etat.status, "completed",
            "audit inachevé : {:?}",
            etat.erreur
        );
        etat
    }
    /// Le rapport de l'audit, par nom de fichier.
    fn rapport(&self, etat: &Etat) -> HashMap<String, Ligne> {
        lignes(&self.db, etat, 0, 1000, None)
            .unwrap()
            .into_iter()
            .map(|l| {
                let nom = Path::new(&l.chemin)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (nom, l)
            })
            .collect()
    }
}

/// Les quatre témoins de l'issue sous le dossier inclus ; un FLAC tronqué
/// HORS du dossier inclus, un autre sous une racine exclue DANS le dossier
/// inclus. Aucun des deux hors périmètre n'est ni compté ni lu.
#[tokio::test]
async fn l_audit_ne_lit_que_les_dossiers_inclus_hors_racines_exclues() {
    let _pauses = crate::pauses_de_fond_tests::isoler().await;
    let mut banc = Banc::new();
    let le = date(1_700_000_000);
    banc.piste("inclus/sain.flac", &flac(), le);
    banc.piste("inclus/octet.flac", &octet_modifie(), le);
    banc.piste("inclus/tronque.flac", &tronque(), le);
    banc.piste("inclus/md5nul.flac", &md5_nul(), le);
    banc.piste("dehors/hors_perimetre.flac", &tronque(), le);
    banc.piste("inclus/exclu/sous_racine_exclue.flac", &tronque(), le);
    banc.regler(perimetre::CLE_DOSSIERS_INCLUS, &["inclus"]);
    banc.regler(perimetre::CLE_RACINES_EXCLUES, &["inclus/exclu"]);

    let etat = banc.auditer(false);
    let rapport = banc.rapport(&etat);
    assert!(
        !rapport.contains_key("hors_perimetre.flac"),
        "un fichier hors des dossiers inclus a été lu par l'audit"
    );
    assert!(
        !rapport.contains_key("sous_racine_exclue.flac"),
        "un fichier d'une racine exclue a été lu par l'audit"
    );
    assert_eq!(
        etat.total, 4,
        "le total doit compter les seuls fichiers du périmètre"
    );
    assert_eq!(
        (etat.sains, etat.corrompus, etat.non_verifiables),
        (1, 2, 1)
    );
    assert_eq!(rapport["sain.flac"].resultat.verdict, Verdict::Sain);
    assert_eq!(rapport["octet.flac"].resultat.code, "md5_incorrect");
    assert_eq!(rapport["tronque.flac"].resultat.verdict, Verdict::Corrompu);
    assert_eq!(rapport["md5nul.flac"].resultat.code, "md5_nul");
    assert_eq!(
        rapport["md5nul.flac"].resultat.verdict,
        Verdict::NonVerifiable
    );
}

/// Relance « fichiers changés seulement » : un fichier modifié à taille égale
/// (date changée), un autre tronqué en gardant sa date, et un nouveau sont
/// relus ; les deux inchangés reprennent le résultat précédent sans être
/// décodés. Le rapport de l'audit précédent est ensuite supprimé.
#[tokio::test]
async fn la_relance_partielle_ne_relit_que_les_fichiers_changes() {
    let _pauses = crate::pauses_de_fond_tests::isoler().await;
    let mut banc = Banc::new();
    let le = date(1_700_000_000);
    banc.piste("sain.flac", &flac(), le);
    banc.piste("octet.flac", &octet_modifie(), le);
    banc.piste("tronque.flac", &flac(), le);
    banc.piste("md5nul.flac", &md5_nul(), le);

    let premier = banc.auditer(false);
    assert_eq!(premier.reutilises, 0);
    assert!(premier.dernier_audit_termine.is_some());

    // Un octet audio altéré, même taille, nouvelle date.
    let mut altere = flac();
    let milieu = altere.len() / 2;
    altere[milieu] ^= 1;
    banc.ecrire("sain.flac", &altere, date(1_700_000_500));
    // Tronqué en gardant sa date : seule la taille le trahit.
    banc.ecrire("tronque.flac", &tronque(), le);
    banc.piste("nouveau.flac", &flac(), le);

    let second = banc.auditer(true);
    assert_eq!(
        second.reference.as_deref(),
        Some(premier.generation.as_str())
    );
    let rapport = banc.rapport(&second);
    let relu_par = |nom: &str| rapport[nom].verifie_par.clone().unwrap_or_default();
    for inchange in ["octet.flac", "md5nul.flac"] {
        assert_eq!(
            relu_par(inchange),
            premier.generation,
            "{inchange} inchangé a été relu par la relance partielle"
        );
    }
    for change in ["sain.flac", "tronque.flac", "nouveau.flac"] {
        assert_eq!(
            relu_par(change),
            second.generation,
            "{change} changé ou nouveau n'a pas été relu"
        );
    }
    assert_eq!(second.reutilises, 2, "deux fichiers inchangés à reprendre");
    assert_eq!(second.traites, 5);
    assert_eq!(rapport["sain.flac"].resultat.verdict, Verdict::Corrompu);
    assert_eq!(rapport["tronque.flac"].resultat.verdict, Verdict::Corrompu);
    assert_eq!(rapport["octet.flac"].resultat.code, "md5_incorrect");
    assert_eq!(rapport["md5nul.flac"].resultat.code, "md5_nul");
    assert_eq!(
        (second.sains, second.corrompus, second.non_verifiables),
        (1, 3, 1)
    );
    let restes = banc
        .db
        .query_one(
            "SELECT COUNT(*) FROM settings WHERE key LIKE ?",
            &[&format!("audit_integrite_ligne:{}:%", premier.generation)],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        restes[0].as_i64(),
        Some(0),
        "le rapport de référence doit être supprimé"
    );

    // Un audit complet relit tout, sans référence.
    let complet = banc.auditer(false);
    assert_eq!(complet.reutilises, 0);
    assert!(
        banc.rapport(&complet)
            .values()
            .all(|l| l.verifie_par.as_deref() == Some(complet.generation.as_str()))
    );
}

/// Sans audit précédent, la relance partielle relit tout.
#[tokio::test]
async fn sans_audit_precedent_la_relance_partielle_relit_tout() {
    let _pauses = crate::pauses_de_fond_tests::isoler().await;
    let mut banc = Banc::new();
    banc.piste("sain.flac", &flac(), date(1_700_000_000));
    let etat = banc.auditer(true);
    assert_eq!(etat.reference, None);
    assert_eq!((etat.traites, etat.reutilises, etat.sains), (1, 0, 1));
}

/// Le rappel publié par `GET /library/integrity` : éteint par défaut ; réglé,
/// dû quand le dernier audit terminé est plus ancien que l'intervalle, jamais
/// pendant un audit.
#[test]
fn le_rappel_est_eteint_par_defaut_puis_du_apres_l_intervalle() {
    let banc = Banc::new();
    let il_y_a_dix_jours = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
    let etat = Etat {
        status: "completed".into(),
        dernier_audit_termine: Some(il_y_a_dix_jours),
        ..Default::default()
    };
    let publie = serde_json::to_value(vue(&banc.db, etat.clone())).unwrap();
    assert_eq!(publie["rappel"]["jours"], 0, "{publie}");
    assert_eq!(
        publie["rappel"]["du"], false,
        "rappel dû alors qu'il est éteint par défaut : {publie}"
    );
    // L'état reste à plat à côté du rappel : les champs du contrat web.
    for champ in ["status", "total", "reutilises", "modifies_seulement"] {
        assert!(publie.get(champ).is_some(), "{champ} absent : {publie}");
    }

    SettingsRepo::with_backend(banc.db.clone())
        .set(rappel::CLE_RAPPEL_JOURS, "7")
        .unwrap();
    assert!(vue(&banc.db, etat.clone()).rappel.du);
    SettingsRepo::with_backend(banc.db.clone())
        .set(rappel::CLE_RAPPEL_JOURS, "30")
        .unwrap();
    assert!(!vue(&banc.db, etat.clone()).rappel.du);
    let en_cours = Etat {
        status: "running".into(),
        ..Default::default()
    };
    ACTIF.store(true, Ordering::Release);
    let du_pendant = vue(&banc.db, en_cours).rappel.du;
    ACTIF.store(false, Ordering::Release);
    assert!(!du_pendant, "un audit en cours n'a pas à être rappelé");
}
