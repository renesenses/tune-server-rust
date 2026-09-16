//! Réparer le drapeau « compilation » des albums DÉJÀ indexés — phase 4 du
//! chantier « gestion du tag compilation ».
//!
//! ## Pourquoi une passe, et pas le scan
//!
//! La phase 1 (#4157) a fait du tag la source de vérité (C1) et de l'artiste
//! d'album tagué du dossier l'artiste d'une compilation (C2). Mais elle ne
//! vaut que pour ce qui est scanné APRÈS : `mark_compilation()` ne baisse
//! jamais le drapeau, et l'amorçage depuis la base fait entrer un album déjà
//! marqué comme `Some(true)`. La base ne garde pas ce que le fichier disait —
//! seulement le verdict d'alors. Une bibliothèque indexée sous l'ancienne
//! règle reste donc telle quelle, coffrets sous « Various Artists » compris.
//!
//! La seule autre voie était le « Scan complet », qui repart d'un `DELETE
//! FROM albums` — et emporte avec lui tout ce que l'utilisateur a corrigé.
//!
//! ## C3 : le marqueur d'abord
//!
//! Arbitrage de Bertrand (14/09/2026) : « d'abord poser le marqueur d'édition
//! manuelle, la réparation ensuite ». Cette passe SAUTE tout album dont
//! l'artiste ou le drapeau est tenu par une édition manuelle
//! (`album_metadata.edition_manuelle`, posé par les routes d'édition). Elle
//! ne défait donc jamais une correction de l'utilisateur — c'est ce qui
//! l'autorise à toucher aux autres.
//!
//! ## Ce qu'elle fait, album par album
//!
//! 1. relit le tag `compilation` et l'artiste d'album DANS LES FICHIERS
//!    (`read_metadata`, le lecteur du scan) — pas dans la base, qui ne les a
//!    pas ;
//! 2. décide selon C1 : un tag qui parle tranche dans les deux sens ; sinon la
//!    forme (un « Various Artists », ou deux artistes d'album distincts dans
//!    un même dossier) ;
//! 3. décide l'artiste selon C2 : compilation ⇒ l'unique artiste d'album
//!    tagué, sinon « Various Artists » ; pas compilation ⇒ l'unique artiste
//!    d'album tagué s'il y en a exactement un, sinon on ne touche pas ;
//! 4. n'écrit que si quelque chose CHANGE, et le journalise
//!    (`compilation_reparee`, avec le motif).
//!
//! Elle ne touche ni aux titres, ni au regroupement des lignes : un album
//! reste la ligne qu'il est. Réunir ou couper des lignes est une autre
//! question (C4, coffrets), traitée au scan.
use std::collections::{BTreeSet, HashMap};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tracing::{debug, info, warn};
use tune_core::db::album_metadata_repo::AlbumMetadataRepo;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::scan_import::is_various_artists;
use crate::state::AppState;

pub(crate) const TACHE: &str = "reparer_compilations";
const REGLAGE_STATUT: &str = "reparer_compilations_status";
const JALON: usize = 50;
const VARIOUS_ARTISTS: &str = "Various Artists";

/// Les pistes LOCALES de chaque album, avec ce que la base sait déjà.
const SQL_PISTES: &str = "SELECT al.id, al.title, al.artist_id, COALESCE(ar.name, ''), al.is_compilation, t.file_path \
     FROM albums al \
     JOIN tracks t ON t.album_id = al.id \
     LEFT JOIN artists ar ON ar.id = al.artist_id \
     WHERE t.source = 'local' AND t.file_path IS NOT NULL AND t.file_path != '' \
     ORDER BY al.id, t.file_path";

/// Ce que les FICHIERS d'un album disent, réduit à ce que C1 et C2 lisent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Temoignage {
    /// Un fichier au moins porte le tag à VRAI.
    pub tag_vrai: bool,
    /// Un fichier au moins porte le tag à FAUX.
    pub tag_faux: bool,
    /// Un artiste d'album « Various Artists » quelque part.
    pub va: bool,
    /// Les artistes d'album tagués, par dossier, repliés en minuscules ; la
    /// graphie d'origine est gardée pour pouvoir l'écrire.
    pub artistes_par_dossier: HashMap<String, BTreeSet<String>>,
    pub graphies: HashMap<String, String>,
    /// Fichiers relus / illisibles ou absents.
    pub lus: usize,
    pub illisibles: usize,
}

impl Temoignage {
    fn ajouter(&mut self, dossier: &str, meta: &tune_core::metadata::TrackMetadata) {
        self.lus += 1;
        match meta.compilation {
            Some(true) => self.tag_vrai = true,
            Some(false) => self.tag_faux = true,
            None => {}
        }
        if let Some(aa) = meta
            .album_artist
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if is_various_artists(aa) {
                self.va = true;
            } else {
                let cle = aa.to_lowercase();
                self.artistes_par_dossier
                    .entry(dossier.to_string())
                    .or_default()
                    .insert(cle.clone());
                self.graphies.entry(cle).or_insert_with(|| aa.to_string());
            }
        }
    }

    /// Tous les artistes d'album tagués, tous dossiers confondus.
    fn artistes(&self) -> BTreeSet<&str> {
        self.artistes_par_dossier
            .values()
            .flat_map(|s| s.iter().map(String::as_str))
            .collect()
    }
}

/// Le verdict de la passe pour un album : le drapeau, son motif, et l'artiste
/// à poser — `None` quand rien ne permet d'en nommer un.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub compilation: bool,
    pub motif: &'static str,
    pub artiste: Option<String>,
}

/// C1 puis C2, sur ce que les fichiers témoignent. Fonction PURE : c'est elle
/// que les témoins éprouvent.
pub(crate) fn decider(t: &Temoignage) -> Verdict {
    // C1 — le tag parle, dans les deux sens ; un VRAI l'emporte sur un FAUX
    // (même règle que `decide_compilation_albums`).
    let forme = t.va || t.artistes_par_dossier.values().any(|s| s.len() >= 2);
    let (compilation, motif) = match (t.tag_vrai, t.tag_faux) {
        (true, _) => (true, "tag"),
        (false, true) => (false, "tag"),
        (false, false) if forme => (true, "forme_des_dossiers"),
        (false, false) => (false, "aucun"),
    };
    let artistes = t.artistes();
    let unique = (artistes.len() == 1)
        .then(|| {
            artistes
                .iter()
                .next()
                .and_then(|k| t.graphies.get(*k))
                .cloned()
        })
        .flatten();
    // C2 — compilation : l'unique artiste d'album tagué, sinon la convention.
    // Pas compilation : l'unique artiste tagué s'il existe, sinon on n'invente
    // rien et l'album garde l'artiste qu'il a.
    let artiste = if compilation {
        Some(unique.unwrap_or_else(|| VARIOUS_ARTISTS.to_string()))
    } else {
        unique
    };
    Verdict {
        compilation,
        motif,
        artiste,
    }
}

fn en_cours(state: &AppState) -> bool {
    state
        .background_tasks
        .snapshot()
        .iter()
        .any(|t| t.id == TACHE)
}

fn lire_statut(state: &AppState) -> Value {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_STATUT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({"status": "idle"}))
}

fn ecrire_statut(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>, v: &Value) {
    tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
        .set(REGLAGE_STATUT, &v.to_string())
        .ok();
}

/// GET /library/compilations/reparation — le dernier état, et « running » si
/// la passe tourne.
pub(crate) async fn statut(State(state): State<AppState>) -> Json<Value> {
    let mut v = lire_statut(&state);
    if en_cours(&state) {
        v["status"] = json!("running");
    }
    Json(v)
}

/// Bilan de la passe, publié au réglage et rendu à la fin.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Bilan {
    pub albums: usize,
    pub reparees: usize,
    pub inchangees: usize,
    pub manuelles: usize,
    pub sans_fichier: usize,
    pub erreurs: usize,
}

impl Bilan {
    fn json(&self, status: &str) -> Value {
        json!({
            "status": status,
            "total": self.albums,
            "repaired": self.reparees,
            "unchanged": self.inchangees,
            "manual_skipped": self.manuelles,
            "unreadable": self.sans_fichier,
            "errors": self.erreurs,
        })
    }
}

/// Une ligne album telle que la base la tient, avec ses fichiers locaux.
struct LigneAlbum {
    id: i64,
    titre: String,
    artist_id: Option<i64>,
    artist_name: String,
    /// Le drapeau tel qu'il est en base — le verdict d'alors.
    deja: bool,
    chemins: Vec<String>,
}

/// Le cœur de la passe, synchrone — appelé depuis la tâche de fond, et
/// directement par les témoins.
pub(crate) fn reparer(state: &AppState, avancement: &dyn Fn(usize, usize)) -> Bilan {
    let backend = state.backend.clone();
    let album_repo = AlbumRepo::with_backend(backend.clone());
    let artist_repo = ArtistRepo::with_backend(backend.clone());
    let meta_repo = AlbumMetadataRepo::with_backend(backend.clone());

    // Regrouper les chemins par album, en gardant l'état de la ligne.
    let rows = backend.query_many(SQL_PISTES, &[]).ou_defaut_journalise();
    let mut albums: Vec<LigneAlbum> = Vec::new();
    for r in &rows {
        let (Some(id), Some(chemin)) = (
            r.first().and_then(|v| v.as_i64()),
            r.get(5).and_then(|v| v.as_string()),
        ) else {
            continue;
        };
        match albums.last_mut() {
            Some(a) if a.id == id => a.chemins.push(chemin),
            _ => albums.push(LigneAlbum {
                id,
                titre: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                artist_id: r.get(2).and_then(|v| v.as_i64()),
                artist_name: r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                deja: tune_core::db::album_repo::drapeau_compilation(r.get(4)),
                chemins: vec![chemin],
            }),
        }
    }

    let mut bilan = Bilan {
        albums: albums.len(),
        ..Default::default()
    };
    avancement(0, bilan.albums);

    for (i, ligne) in albums.iter().enumerate() {
        let LigneAlbum {
            id,
            titre,
            artist_id,
            artist_name,
            deja,
            chemins,
        } = ligne;
        if i % JALON == 0 {
            avancement(i, bilan.albums);
        }
        // C3 — tenu par l'utilisateur : on ne regarde même pas les fichiers.
        let tenus = meta_repo.champs_edites_a_la_main(*id).unwrap_or_default();
        if tenus.iter().any(|c| c == "artist" || c == "is_compilation") {
            bilan.manuelles += 1;
            continue;
        }

        let mut temoignage = Temoignage::default();
        for chemin in chemins {
            let reel = match tune_core::library::local_path::resolve_local_path(chemin) {
                tune_core::library::local_path::LocalPath::Found(p) => p,
                tune_core::library::local_path::LocalPath::Missing => {
                    temoignage.illisibles += 1;
                    continue;
                }
            };
            let dossier = std::path::Path::new(chemin)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default();
            match tune_core::metadata::read_metadata(std::path::Path::new(&reel)) {
                Some(meta) => temoignage.ajouter(&dossier, &meta),
                None => temoignage.illisibles += 1,
            }
        }
        if temoignage.lus == 0 {
            // Aucun fichier lisible : rien ne permet de juger, on ne touche pas.
            bilan.sans_fichier += 1;
            continue;
        }

        let verdict = decider(&temoignage);
        let artiste_change = verdict
            .artiste
            .as_deref()
            .is_some_and(|voulu| !voulu.eq_ignore_ascii_case(artist_name));
        if verdict.compilation == *deja && !artiste_change {
            bilan.inchangees += 1;
            continue;
        }
        let nouvel_artiste = if artiste_change {
            verdict
                .artiste
                .as_deref()
                .and_then(|nom| artist_repo.get_or_create(nom, None, None).ok())
                .and_then(|a| a.id)
        } else {
            None
        };
        match album_repo.reparer_compilation(*id, verdict.compilation, nouvel_artiste) {
            Ok(()) => {
                bilan.reparees += 1;
                info!(
                    album_id = id,
                    album = %titre,
                    avant = deja,
                    apres = verdict.compilation,
                    motif = verdict.motif,
                    ancien_artiste = %artist_name,
                    nouvel_artiste = ?verdict.artiste.as_deref().filter(|_| artiste_change),
                    ancien_artiste_id = ?artist_id,
                    fichiers_lus = temoignage.lus,
                    "compilation_reparee"
                );
            }
            Err(e) => {
                bilan.erreurs += 1;
                warn!(album_id = id, error = %e, "compilation_reparation_echouee");
            }
        }
    }
    avancement(bilan.albums, bilan.albums);
    debug!(?bilan, "reparer_compilations_termine");
    bilan
}

/// POST /library/compilations/reparation — lance la passe en tâche de fond.
/// 202 ; 409 si elle tourne déjà.
pub(crate) async fn lancer(State(state): State<AppState>) -> impl IntoResponse {
    if en_cours(&state) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"status": "running", "error": "already running"})),
        );
    }
    let garde = state.background_tasks.begin(
        TACHE,
        "Réparation du drapeau « compilation »…",
        "maintenance",
    );
    ecrire_statut(&state.backend, &Bilan::default().json("running"));
    let etat = state.clone();
    tokio::spawn(async move {
        let _garde = garde;
        // La passe relit des fichiers : hors du réacteur.
        let etat2 = etat.clone();
        let bilan = tokio::task::spawn_blocking(move || {
            let taches = etat2.background_tasks.clone();
            reparer(&etat2, &|fait, total| {
                taches.update_progress(TACHE, fait as u64, total as u64, "Compilations");
            })
        })
        .await
        .unwrap_or_default();
        ecrire_statut(&etat.backend, &bilan.json("done"));
        info!(?bilan, "reparer_compilations_fini");
    });
    (StatusCode::ACCEPTED, Json(json!({"status": "accepted"})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::backend::ToSqlValue;

    fn temoignage(fichiers: &[(&str, Option<&str>, Option<bool>)]) -> Temoignage {
        let mut t = Temoignage::default();
        for (dossier, aa, tag) in fichiers {
            let m = tune_core::metadata::TrackMetadata {
                album_artist: aa.map(str::to_string),
                compilation: *tag,
                ..Default::default()
            };
            t.ajouter(dossier, &m);
        }
        t
    }

    /// C1 — le tag tranche dans les DEUX sens, et un vrai l'emporte.
    #[test]
    fn le_tag_tranche_dans_les_deux_sens() {
        // Coffret : deux graphies du chef, tag à FAUX ⇒ PAS une compilation,
        // et l'artiste reste à nommer — deux graphies ⇒ aucun artiste unique.
        let t = temoignage(&[
            ("/c/cd1", Some("Fritz Reiner"), Some(false)),
            ("/c/cd1", Some("Reiner, Fritz"), Some(false)),
        ]);
        let v = decider(&t);
        assert!(!v.compilation);
        assert_eq!(v.motif, "tag");
        assert_eq!(v.artiste, None, "deux graphies : on n'invente pas");

        // Anthologie taguée : compilation, Various Artists.
        let t = temoignage(&[
            ("/va", Some("Aretha Franklin"), Some(true)),
            ("/va", Some("Otis Redding"), None),
        ]);
        let v = decider(&t);
        assert!(v.compilation);
        assert_eq!(v.motif, "tag");
        assert_eq!(v.artiste.as_deref(), Some("Various Artists"));

        // Un FAUX et un VRAI : le vrai l'emporte.
        let t = temoignage(&[("/x", None, Some(false)), ("/x", None, Some(true))]);
        assert!(decider(&t).compilation);
    }

    /// C1 repli — sans tag, la forme des dossiers ; C2 — l'artiste tagué
    /// unique d'une compilation est gardé.
    #[test]
    fn sans_tag_la_forme_decide_et_c2_nomme_l_artiste() {
        let t = temoignage(&[("/d", Some("A"), None), ("/d", Some("B"), None)]);
        let v = decider(&t);
        assert!(v.compilation);
        assert_eq!(v.motif, "forme_des_dossiers");
        assert_eq!(v.artiste.as_deref(), Some("Various Artists"));

        // Compilation par tag mais UN seul artiste d'album tagué : C2 le garde.
        let t = temoignage(&[
            ("/d", Some("Fritz Reiner"), Some(true)),
            ("/d", Some("fritz reiner"), Some(true)),
        ]);
        let v = decider(&t);
        assert!(v.compilation);
        assert_eq!(v.artiste.as_deref(), Some("Fritz Reiner"));

        // Rien du tout : pas une compilation, rien à poser.
        let t = temoignage(&[("/d", Some("Solo"), None)]);
        let v = decider(&t);
        assert!(!v.compilation);
        assert_eq!(v.motif, "aucun");
        assert_eq!(v.artiste.as_deref(), Some("Solo"));
    }

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    /// Une fixture FLAC copiée sous un nom, avec le tag compilation voulu et
    /// l'artiste d'album voulu, écrits par lofty dans le fichier lui-même.
    fn fichier(
        dir: &std::path::Path,
        nom: &str,
        album_artist: &str,
        compilation: Option<bool>,
    ) -> String {
        use lofty::config::{ParseOptions, WriteOptions};
        use lofty::file::AudioFile;
        use lofty::flac::FlacFile;
        use std::io::Seek;
        let cible = dir.join(nom);
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../tune-core/tests/fixtures/test.flac"),
            &cible,
        )
        .unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cible)
            .unwrap();
        let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
        if flac.vorbis_comments().is_none() {
            flac.set_vorbis_comments(Default::default());
        }
        let vc = flac.vorbis_comments_mut().unwrap();
        vc.insert("ALBUM".into(), "Le Coffret".into());
        vc.insert("ALBUMARTIST".into(), album_artist.into());
        if let Some(c) = compilation {
            vc.insert("COMPILATION".into(), if c { "1" } else { "0" }.into());
        }
        f.rewind().unwrap();
        flac.save_to(&mut f, WriteOptions::default()).unwrap();
        cible.to_string_lossy().into_owned()
    }

    fn album(state: &AppState, id: i64, artiste: &str, compilation: bool, chemins: &[&str]) {
        let b = &state.backend;
        b.execute(
            "INSERT INTO artists (name) VALUES (?1)",
            &[&artiste as &dyn ToSqlValue],
        )
        .unwrap();
        let aid = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (id, title, artist_id, is_compilation) VALUES (?1, 'Le Coffret', ?2, ?3)",
            &[&id as &dyn ToSqlValue, &aid, &(compilation as i64)],
        )
        .unwrap();
        for c in chemins {
            b.execute(
                "INSERT INTO tracks (title, album_id, file_path, source) VALUES ('t', ?1, ?2, 'local')",
                &[&id as &dyn ToSqlValue, &c.to_string()],
            )
            .unwrap();
        }
    }

    fn ligne(state: &AppState, id: i64) -> (bool, String) {
        let r = state
            .backend
            .query_one(
                "SELECT al.is_compilation, ar.name FROM albums al LEFT JOIN artists ar ON ar.id = al.artist_id WHERE al.id = ?1",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap();
        (
            tune_core::db::album_repo::drapeau_compilation(r.first()),
            r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        )
    }

    /// Le cas de #3855 tel que l'ancienne règle l'a laissé en base : coffret
    /// sous « Various Artists », drapeau levé, alors que les fichiers disent
    /// `COMPILATION=0` et un seul chef. La passe le répare — et BAISSE le
    /// drapeau, ce que le scan ne sait pas faire.
    #[test]
    fn la_passe_baisse_le_drapeau_et_rend_son_chef_au_coffret() {
        let dir = tempfile::tempdir().unwrap();
        let a = fichier(dir.path(), "01.flac", "Fritz Reiner", Some(false));
        let b = fichier(dir.path(), "02.flac", "Fritz Reiner", Some(false));
        let s = etat();
        album(&s, 1, "Various Artists", true, &[&a, &b]);

        let bilan = reparer(&s, &|_, _| {});
        assert_eq!(
            (bilan.albums, bilan.reparees, bilan.erreurs),
            (1, 1, 0),
            "{bilan:?}"
        );
        assert_eq!(ligne(&s, 1), (false, "Fritz Reiner".to_string()));

        // Seconde passe : plus rien à faire.
        let bilan = reparer(&s, &|_, _| {});
        assert_eq!((bilan.reparees, bilan.inchangees), (0, 1));
    }

    /// C3 — un album dont l'artiste est tenu par une édition manuelle n'est
    /// pas touché, même quand les fichiers diraient autre chose.
    #[test]
    fn un_album_edite_a_la_main_n_est_jamais_repare() {
        let dir = tempfile::tempdir().unwrap();
        let a = fichier(dir.path(), "01.flac", "Fritz Reiner", Some(false));
        let s = etat();
        album(&s, 1, "Various Artists", true, &[&a]);
        AlbumMetadataRepo::with_backend(s.backend.clone())
            .marquer_edition_manuelle(1, &["artist"])
            .unwrap();

        let bilan = reparer(&s, &|_, _| {});
        assert_eq!((bilan.manuelles, bilan.reparees), (1, 0), "{bilan:?}");
        assert_eq!(ligne(&s, 1), (true, "Various Artists".to_string()));
    }

    /// Le sens montant existe aussi : une anthologie indexée sous son premier
    /// artiste, dont les fichiers portent le tag, passe en compilation.
    #[test]
    fn la_passe_leve_le_drapeau_d_une_anthologie_taguee() {
        let dir = tempfile::tempdir().unwrap();
        let a = fichier(dir.path(), "01.flac", "Aretha Franklin", Some(true));
        let b = fichier(dir.path(), "02.flac", "Otis Redding", Some(true));
        let s = etat();
        album(&s, 1, "Aretha Franklin", false, &[&a, &b]);

        let bilan = reparer(&s, &|_, _| {});
        assert_eq!(bilan.reparees, 1, "{bilan:?}");
        assert_eq!(ligne(&s, 1), (true, "Various Artists".to_string()));
    }

    /// Un album dont aucun fichier n'est lisible n'est pas jugé.
    #[test]
    fn sans_fichier_lisible_on_ne_juge_pas() {
        let s = etat();
        album(&s, 1, "Various Artists", true, &["/nulle/part/01.flac"]);
        let bilan = reparer(&s, &|_, _| {});
        assert_eq!((bilan.sans_fichier, bilan.reparees), (1, 0), "{bilan:?}");
        assert_eq!(ligne(&s, 1), (true, "Various Artists".to_string()));
    }

    /// La route s'inscrit au registre et refuse un doublon.
    #[tokio::test]
    async fn la_route_s_inscrit_au_registre_et_refuse_un_doublon() {
        let s = etat();
        let r = lancer(State(s.clone())).await.into_response();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        assert!(en_cours(&s));
        let r2 = lancer(State(s.clone())).await.into_response();
        assert_eq!(r2.status(), StatusCode::CONFLICT);
    }
}
