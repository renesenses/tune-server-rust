//! Témoins de l'étape C (#4805) : la règle, le filtre des noms, la
//! confirmation, et la garde anti-écrasement en base.

use std::cell::RefCell;
use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const MB_A: &str = "aaaaaaaa-0000-0000-0000-00000000000a";
const MB_B: &str = "bbbbbbbb-0000-0000-0000-00000000000b";
const MB_C: &str = "cccccccc-0000-0000-0000-00000000000c";

fn artiste(id: &str, nom: &str, score: i64, alias: &[&str]) -> Value {
    json!({
        "id": id,
        "name": nom,
        "sort-name": nom,
        "score": score,
        "aliases": alias.iter().map(|a| json!({"name": a, "sort-name": a})).collect::<Vec<_>>(),
    })
}

fn resultats(cle: &str, titres: &[(&str, &str)]) -> Value {
    json!({ cle: titres.iter().map(|(t, mbid)| json!({
        "title": t,
        "artist-credit": [{"name": "x", "artist": {"id": mbid}}],
    })).collect::<Vec<_>>() })
}

/// Un faux MusicBrainz : une réponse par entité, et le journal des requêtes.
struct Faux {
    artistes: Value,
    /// `(mbid, réponse release-group, réponse recording)`.
    confirmations: Vec<(&'static str, Value, Value)>,
    refus_sur: Option<Entite>,
    journal: RefCell<Vec<(Entite, String)>>,
}

impl Faux {
    fn new(artistes: Vec<Value>) -> Self {
        Faux {
            artistes: json!({ "artists": artistes }),
            confirmations: Vec::new(),
            refus_sur: None,
            journal: RefCell::new(Vec::new()),
        }
    }

    fn confirme_album(mut self, mbid: &'static str, titre: &str) -> Self {
        self.confirmations.push((
            mbid,
            resultats("release-groups", &[(titre, mbid)]),
            json!({ "recordings": [] }),
        ));
        self
    }

    fn confirme_piste(mut self, mbid: &'static str, titre: &str) -> Self {
        self.confirmations.push((
            mbid,
            json!({ "release-groups": [] }),
            resultats("recordings", &[(titre, mbid)]),
        ));
        self
    }

    fn interroger(
        &self,
    ) -> impl FnMut(Entite, String) -> std::future::Ready<Result<Value, RefusMusicBrainz>> + '_
    {
        move |entite, requete| {
            self.journal.borrow_mut().push((entite, requete.clone()));
            if self.refus_sur == Some(entite) {
                return std::future::ready(Err(RefusMusicBrainz::Statut(503)));
            }
            let v = match entite {
                Entite::Artiste => self.artistes.clone(),
                _ => {
                    let trouve = self
                        .confirmations
                        .iter()
                        .find(|(m, _, _)| requete.starts_with(&format!("arid:{m} ")));
                    match (trouve, entite) {
                        (Some((_, rg, _)), Entite::GroupeDeSortie) => rg.clone(),
                        (Some((_, _, rec)), _) => rec.clone(),
                        (None, Entite::GroupeDeSortie) => json!({ "release-groups": [] }),
                        (None, _) => json!({ "recordings": [] }),
                    }
                }
            };
            std::future::ready(Ok(v))
        }
    }

    fn nombre(&self) -> usize {
        self.journal.borrow().len()
    }
}

fn matiere_de(nom: &str, albums: &[&str], pistes: &[&str]) -> Matiere {
    Matiere {
        nom: nom.into(),
        titres_d_album: albums.iter().map(|s| s.to_string()).collect(),
        titres_de_piste: pistes.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn la_regle_nettement_devant_est_celle_de_5866() {
    assert!(score_nettement_devant(100, 90));
    assert!(score_nettement_devant(90, 80));
    assert!(!score_nettement_devant(100, 91));
    assert!(score_nettement_devant(95, 89));
    assert!(!score_nettement_devant(94, 89));
    assert!(!score_nettement_devant(89, 0));
}

#[test]
fn la_requete_echappe_le_guillemet_lucene() {
    assert_eq!(
        requete_artiste("  Le \"Grand\"   Orchestre "),
        r#"artist:"Le \"Grand\" Orchestre" OR alias:"Le \"Grand\" Orchestre" OR sortname:"Le \"Grand\" Orchestre""#
    );
    assert_eq!(
        requete_de_confirmation(
            Entite::GroupeDeSortie,
            MB_A,
            &["Zombie".into(), "A\\B".into()]
        ),
        format!(r#"arid:{MB_A} AND (releasegroup:"Zombie" OR releasegroup:"A\\B")"#)
    );
}

#[test]
fn un_alias_et_le_nom_de_tri_a_l_endroit_nomment_l_artiste() {
    let fela = candidats_d_artiste(&json!({"artists": [
        artiste(MB_A, "Fela Kuti", 100, &["Fela Anikulapo Kuti"]),
    ]}));
    assert!(nomme(&fela[0], &cle_artiste("Fela Anikulapo-Kuti")));
    let sakamoto = CandidatArtiste {
        mbid: MB_B.into(),
        nom: "坂本龍一".into(),
        score: 100,
        noms: vec!["坂本龍一".into(), "Sakamoto, Ryuichi".into()],
    };
    assert!(nomme(&sakamoto, &cle_artiste("Ryuichi Sakamoto")));
    assert!(!nomme(&sakamoto, &cle_artiste("Ryuichi")));
}

#[tokio::test]
async fn seul_nettement_devant_et_confirme_par_un_album_il_est_pose() {
    let faux = Faux::new(vec![
        artiste(MB_A, "Fela Kuti", 100, &["Fela Anikulapo Kuti"]),
        artiste(MB_B, "Fela", 60, &[]),
    ])
    .confirme_album(MB_A, "Zombie");
    let e = examiner(
        &matiere_de("Fela Anikulapo Kuti", &["Zombie (Remastered)"], &[]),
        faux.interroger(),
    )
    .await;
    assert_eq!(
        e.verdict,
        Verdict::Pose {
            mbid: MB_A.into(),
            departage: false
        }
    );
    assert_eq!(e.requetes, 2);
}

#[tokio::test]
async fn sans_confirmation_par_la_bibliotheque_rien_n_est_pose() {
    let faux = Faux::new(vec![artiste(MB_A, "Daniel Guichard", 100, &[])]);
    let e = examiner(
        &matiere_de("Daniel Guichard", &["Vacances 1998"], &["Piste 01"]),
        faux.interroger(),
    )
    .await;
    assert_eq!(e.verdict, Verdict::NonConfirme);
    // Album, puis pistes : deux confirmations tentées.
    assert_eq!(e.requetes, 3);
}

#[tokio::test]
async fn la_confirmation_passe_par_les_pistes_quand_l_album_ne_dit_rien() {
    let faux = Faux::new(vec![artiste(MB_A, "Nina Simone", 100, &[])])
        .confirme_piste(MB_A, "Feeling Good");
    let e = examiner(
        &matiere_de("Nina Simone", &["Best of perso"], &["feeling good"]),
        faux.interroger(),
    )
    .await;
    assert!(matches!(e.verdict, Verdict::Pose { .. }), "{e:?}");
}

#[tokio::test]
async fn un_titre_credite_a_un_autre_artiste_ne_confirme_pas() {
    let mut faux = Faux::new(vec![artiste(MB_A, "Air", 100, &[])]);
    // MusicBrainz rendrait, malgré `arid:`, un groupe de sortie d'un autre.
    faux.confirmations.push((
        MB_A,
        resultats("release-groups", &[("Moon Safari", MB_B)]),
        json!({"recordings": []}),
    ));
    let e = examiner(&matiere_de("Air", &["Moon Safari"], &[]), faux.interroger()).await;
    assert_eq!(e.verdict, Verdict::NonConfirme);
}

#[tokio::test]
async fn un_candidat_qui_ne_nomme_pas_l_artiste_ne_compte_pas() {
    // « Karajan » : MusicBrainz rend Herbert von Karajan, sans alias
    // « Karajan ». Le score ne suffit pas à le rattacher.
    let faux = Faux::new(vec![artiste(MB_A, "Herbert von Karajan", 100, &[])]);
    let e = examiner(&matiere_de("Karajan", &["Adagio"], &[]), faux.interroger()).await;
    assert_eq!(e.verdict, Verdict::SansCorrespondance);
    assert_eq!(e.requetes, 1);
}

#[tokio::test]
async fn des_homonymes_a_egalite_se_departagent_par_la_bibliotheque() {
    let faux = Faux::new(vec![
        artiste(MB_A, "Prince", 100, &[]),
        artiste(MB_B, "Prince", 100, &[]),
        artiste(MB_C, "Prince", 95, &[]),
    ])
    .confirme_album(MB_B, "Purple Rain");
    let e = examiner(
        &matiere_de("Prince", &["Purple Rain"], &[]),
        faux.interroger(),
    )
    .await;
    assert_eq!(
        e.verdict,
        Verdict::Pose {
            mbid: MB_B.into(),
            departage: true
        }
    );
}

#[tokio::test]
async fn deux_homonymes_confirmes_restent_ambigus() {
    let faux = Faux::new(vec![
        artiste(MB_A, "Nirvana", 100, &[]),
        artiste(MB_B, "Nirvana", 100, &[]),
    ])
    .confirme_album(MB_A, "Nevermind")
    .confirme_album(MB_B, "Nevermind");
    let e = examiner(
        &matiere_de("Nirvana", &["Nevermind"], &[]),
        faux.interroger(),
    )
    .await;
    assert_eq!(e.verdict, Verdict::Ambigu);
}

#[tokio::test]
async fn un_peloton_trop_large_est_ambigu_sans_rien_demander_de_plus() {
    let faux = Faux::new(
        (0..=PELOTON_MAX)
            .map(|i| {
                artiste(
                    &format!("eeeeeeee-0000-0000-0000-00000000000{i}"),
                    "John Williams",
                    100,
                    &[],
                )
            })
            .collect(),
    );
    let e = examiner(
        &matiere_de("John Williams", &["Mes musiques de films"], &[]),
        faux.interroger(),
    )
    .await;
    assert_eq!(e.verdict, Verdict::Ambigu);
    assert_eq!(faux.nombre(), 1);
}

#[tokio::test]
async fn un_pseudo_artiste_n_est_jamais_candidat() {
    // « Various » n'est pas un alias de compilation écarté d'office (Lucene
    // le retrouve déjà) : la requête part, et le pseudo-artiste qui revient
    // n'est pas retenu, même confirmé.
    let faux = Faux::new(vec![artiste(
        "89ad4ac3-39f7-470e-963a-56509c546377",
        "Various Artists",
        100,
        &["Various"],
    )])
    .confirme_album("89ad4ac3-39f7-470e-963a-56509c546377", "Compil");
    let e = examiner(&matiere_de("Various", &["Compil"], &[]), faux.interroger()).await;
    assert_eq!(e.verdict, Verdict::SansCorrespondance);
}

#[tokio::test]
async fn un_artiste_fictif_ou_une_compilation_ne_coute_aucune_requete() {
    for nom in [
        "Unknown Artist",
        "Artiste inconnu",
        "VA",
        "Various Artists",
        "  ",
    ] {
        let faux = Faux::new(vec![]);
        let e = examiner(&matiere_de(nom, &["X"], &[]), faux.interroger()).await;
        assert_eq!(e.verdict, Verdict::Ecarte, "{nom}");
        assert_eq!(faux.nombre(), 0, "{nom}");
    }
}

#[tokio::test]
async fn un_refus_de_musicbrainz_est_rendu_tel_quel() {
    let mut faux = Faux::new(vec![artiste(MB_A, "Air", 100, &[])]);
    faux.refus_sur = Some(Entite::GroupeDeSortie);
    let e = examiner(&matiere_de("Air", &["Moon Safari"], &[]), faux.interroger()).await;
    assert_eq!(e.verdict, Verdict::Refus(RefusMusicBrainz::Statut(503)));
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn exec(b: &Arc<dyn DbBackend>, sql: &str) {
    b.execute(sql, &[]).unwrap();
}

fn mbid_de(b: &Arc<dyn DbBackend>, id: i64) -> Option<String> {
    b.query_one(
        "SELECT musicbrainz_id FROM artists WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    )
    .unwrap()
    .and_then(|l| l.first().and_then(|v| v.as_string()))
}

#[test]
fn la_selection_ne_prend_que_les_fiches_vides_et_locales_apres_le_curseur() {
    let b = base();
    for sql in [
        "INSERT INTO artists (id, name, musicbrainz_id) VALUES (1, 'Sans MBID', NULL)",
        "INSERT INTO artists (id, name, musicbrainz_id) VALUES (2, 'Déjà muni', 'x-y')",
        "INSERT INTO artists (id, name, musicbrainz_id) VALUES (3, 'Vide', '  ')",
        "INSERT INTO artists (id, name) VALUES (4, 'Sans rien de local')",
        "INSERT INTO artists (id, name) VALUES (5, 'Seulement UPnP')",
        "INSERT INTO artists (id, name) VALUES (6, 'Piste à source NULL')",
        "INSERT INTO albums (id, title, artist_id, source) VALUES (1, 'A', 1, 'local')",
        "INSERT INTO albums (id, title, artist_id, source) VALUES (2, 'B', 2, 'local')",
        "INSERT INTO albums (id, title, artist_id, source) VALUES (3, 'C', 3, 'local')",
        "INSERT INTO albums (id, title, artist_id, source) VALUES (5, 'E', 5, 'upnp')",
        "INSERT INTO tracks (id, title, album_id, artist_id, source) VALUES (60, 'T', 1, 6, NULL)",
    ] {
        exec(&b, sql);
    }
    let ids = |apres, n| -> Vec<i64> {
        candidats(&b, apres, n)
            .unwrap()
            .into_iter()
            .map(|(i, _)| i)
            .collect()
    };
    assert_eq!(ids(0, 100), vec![1, 3, 6]);
    assert_eq!(ids(1, 100), vec![3, 6]);
    assert_eq!(ids(0, 2), vec![1, 3]);
}

#[tokio::test]
async fn en_base_un_mbid_est_pose_sans_jamais_ecraser_ni_doubler() {
    let b = base();
    for sql in [
        "INSERT INTO artists (id, name) VALUES (1, 'Fela Anikulapo Kuti')",
        "INSERT INTO artists (id, name) VALUES (2, 'Fela Kuti')",
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Zombie', 1)",
        "INSERT INTO albums (id, title, artist_id) VALUES (2, 'Zombie', 2)",
    ] {
        exec(&b, sql);
    }
    let faux = Faux::new(vec![artiste(
        MB_A,
        "Fela Kuti",
        100,
        &["Fela Anikulapo Kuti"],
    )])
    .confirme_album(MB_A, "Zombie");
    let mut bilan = BilanReseau::default();
    let refus = traiter_un_artiste(&b, 1, "Fela Anikulapo Kuti", faux.interroger(), &mut bilan)
        .await
        .unwrap();
    assert_eq!(refus, None);
    assert_eq!(bilan.poses, 1);
    assert_eq!(mbid_de(&b, 1).as_deref(), Some(MB_A));

    // La seconde fiche, doublon de la première : la base refuse le même MBID.
    traiter_un_artiste(&b, 2, "Fela Kuti", faux.interroger(), &mut bilan)
        .await
        .unwrap();
    assert_eq!(bilan.refuses_par_la_base, 1);
    assert_eq!(mbid_de(&b, 2), None);
}

#[tokio::test]
async fn une_fiche_remplie_entre_la_selection_et_l_ecriture_n_est_pas_ecrasee() {
    let b = base();
    exec(&b, "INSERT INTO artists (id, name) VALUES (1, 'Air')");
    exec(
        &b,
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Moon Safari', 1)",
    );
    let selection = candidats(&b, 0, 10).unwrap();
    assert_eq!(selection.len(), 1);
    // Une autre passe (l'étape B, la passe d'images) pose un MBID entre-temps.
    exec(
        &b,
        "UPDATE artists SET musicbrainz_id = 'mb-local' WHERE id = 1",
    );
    let faux = Faux::new(vec![artiste(MB_A, "Air", 100, &[])]).confirme_album(MB_A, "Moon Safari");
    let mut bilan = BilanReseau::default();
    traiter_un_artiste(&b, 1, "Air", faux.interroger(), &mut bilan)
        .await
        .unwrap();
    assert_eq!(bilan.poses, 0);
    assert_eq!(bilan.refuses_par_la_base, 1);
    assert_eq!(mbid_de(&b, 1).as_deref(), Some("mb-local"));
}

#[tokio::test]
async fn un_refus_est_compte_comme_une_panne_et_n_ecrit_rien() {
    let b = base();
    exec(&b, "INSERT INTO artists (id, name) VALUES (1, 'Air')");
    exec(
        &b,
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Moon Safari', 1)",
    );
    let mut faux = Faux::new(vec![]);
    faux.refus_sur = Some(Entite::Artiste);
    let mut bilan = BilanReseau::default();
    let refus = traiter_un_artiste(&b, 1, "Air", faux.interroger(), &mut bilan)
        .await
        .unwrap();
    assert_eq!(refus, Some(RefusMusicBrainz::Statut(503)));
    assert_eq!(bilan.pannes, 1);
    assert_eq!(mbid_de(&b, 1), None);
}
