//! Le livret PDF d'un album de la bibliothèque (#6481).
//!
//! cpadiou77, fil forum 2326 (18/10/2026) : « Dans Audirvana il est possible
//! d'accéder aux PDF des albums stockés dans la bibliothèque […]. Pourrait-on
//! ajouter cette fonctionnalité sur la page de l'album ? » ; Patatorz (réponse
//! 8470) : « Je vote aussi pour cette option ».
//!
//! Deux routes :
//!
//! - `GET /library/albums/{id}/booklets` — les PDF rangés à côté des pistes,
//!   `{"booklets": [{"name": "Livret.pdf", "size": 1234567}]}` ;
//! - `GET /library/albums/{id}/booklets/{name}` — le fichier, en
//!   `application/pdf` et `inline`, pour que le navigateur l'ouvre dans un
//!   nouvel onglet.
//!
//! # Ce que la seconde route accepte de servir — et rien d'autre
//!
//! Elle lit un fichier du disque à partir d'un nom fourni par le client : la
//! garde est donc la raison d'être du module, pas un détail.
//!
//! 1. Le nom est un NOM de fichier, pas un chemin : ni `/`, ni `\`, ni `..`,
//!    ni caractère nul, ni nom caché, et il finit par `.pdf`.
//! 2. Les seuls dossiers regardés sont ceux de l'ALBUM, déduits des pistes en
//!    base (le dossier de chaque piste, et le dossier de l'album quand les
//!    pistes sont rangées par disque, `CD1/`, `CD2/`).
//! 3. Le nom doit figurer dans la liste que la première route rend : une
//!    entrée de ce dossier, fichier ordinaire — un lien symbolique n'en est
//!    pas un, et n'est jamais suivi.
//! 4. Avant d'ouvrir, le chemin canonique du fichier doit avoir pour parent
//!    le chemin canonique du dossier.
//!
//! Tout refus rend 404, sans dire lequel des contrôles a refusé.

use std::collections::BTreeSet;
use std::path::{Path as CheminFs, PathBuf};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_http_types::panne_sql::OuDefautJournalise;

use super::refus;
use crate::state::AppState;

/// Au-delà, ce n'est plus un livret : on ne charge pas un fichier arbitraire
/// en mémoire pour le servir.
const TAILLE_MAX_LIVRET: u64 = 512 * 1024 * 1024;

/// Un PDF du dossier d'un album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Livret {
    pub nom: String,
    pub taille: u64,
    pub chemin: PathBuf,
}

/// Le nom est-il celui d'un livret PDF, et rien de plus qu'un nom ?
pub(crate) fn nom_de_livret_valide(nom: &str) -> bool {
    !nom.is_empty()
        && nom.len() <= 255
        && !nom.starts_with('.')
        && !nom.contains(['/', '\\', '\0'])
        && !nom.contains("..")
        && nom.to_ascii_lowercase().ends_with(".pdf")
}

/// Les dossiers d'un album, à partir des chemins de ses pistes : le dossier
/// de chaque piste, et le dossier de l'album (le parent d'un dossier de
/// disque). Seuls les chemins absolus comptent — une piste UPnP ou de service
/// n'a pas de dossier sur ce disque.
pub(crate) fn dossiers_des_pistes<I, S>(chemins: I) -> BTreeSet<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut dossiers = BTreeSet::new();
    for chemin in chemins {
        let chemin = chemin.as_ref();
        if !CheminFs::new(chemin).is_absolute() {
            continue;
        }
        if let Some(parent) = CheminFs::new(chemin).parent() {
            dossiers.insert(parent.to_path_buf());
        }
        if let Some(album) = tune_core::scanner::album_folder::album_folder(chemin) {
            dossiers.insert(PathBuf::from(album));
        }
    }
    dossiers
}

/// Les PDF de ces dossiers : entrées directes (aucune descente), fichiers
/// ordinaires (un lien symbolique est écarté), au nom valide. Triés par nom ;
/// un même nom dans deux dossiers n'apparaît qu'une fois, le premier dossier
/// dans l'ordre l'emporte.
pub(crate) fn livrets_des_dossiers(dossiers: &BTreeSet<PathBuf>) -> Vec<Livret> {
    let mut vus = BTreeSet::new();
    let mut livrets = Vec::new();
    for dossier in dossiers {
        let Ok(entrees) = std::fs::read_dir(dossier) else {
            continue;
        };
        let mut ici: Vec<Livret> = entrees
            .filter_map(Result::ok)
            .filter_map(|e| {
                // `file_type` ne suit PAS les liens symboliques.
                let type_ = e.file_type().ok()?;
                if !type_.is_file() {
                    return None;
                }
                let nom = e.file_name().into_string().ok()?;
                if !nom_de_livret_valide(&nom) {
                    return None;
                }
                let taille = e.metadata().ok()?.len();
                Some(Livret {
                    nom,
                    taille,
                    chemin: e.path(),
                })
            })
            .collect();
        ici.sort_by_key(|a| a.nom.to_lowercase());
        for l in ici {
            if vus.insert(l.nom.clone()) {
                livrets.push(l);
            }
        }
    }
    livrets
}

/// Le livret `nom` de ces dossiers, s'il passe TOUS les contrôles de l'en-tête.
pub(crate) fn livret_a_servir(dossiers: &BTreeSet<PathBuf>, nom: &str) -> Option<Livret> {
    if !nom_de_livret_valide(nom) {
        return None;
    }
    let livret = livrets_des_dossiers(dossiers)
        .into_iter()
        .find(|l| l.nom == nom)?;
    let dossier = livret.chemin.parent()?.canonicalize().ok()?;
    let fichier = livret.chemin.canonicalize().ok()?;
    if fichier.parent()? != dossier {
        return None;
    }
    let meta = std::fs::symlink_metadata(&livret.chemin).ok()?;
    if !meta.file_type().is_file() || meta.len() > TAILLE_MAX_LIVRET {
        return None;
    }
    Some(livret)
}

/// Les chemins de fichier des pistes de l'album (`cue_media_path` pour une
/// piste d'image CUE).
fn chemins_des_pistes(state: &AppState, album_id: i64) -> Vec<String> {
    let p1 = match state.backend.engine() {
        Engine::Postgres => PostgresDialect.placeholder(1),
        Engine::Sqlite => SqliteDialect.placeholder(1),
    };
    state
        .backend
        .query_many(
            &format!(
                "SELECT COALESCE(file_path, cue_media_path) FROM tracks \
                 WHERE album_id = {p1} AND COALESCE(file_path, cue_media_path) IS NOT NULL"
            ),
            &[&album_id as &dyn ToSqlValue],
        )
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect()
}

/// Un nom de fichier en `attr-char` (RFC 5987) : les octets UTF-8 hors de
/// l'alphabet sûr sont écrits `%XX`.
fn valeur_rfc5987(nom: &str) -> String {
    let mut sortie = String::with_capacity(nom.len() * 3);
    for octet in nom.bytes() {
        if octet.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&octet) {
            sortie.push(octet as char);
        } else {
            sortie.push_str(&format!("%{octet:02X}"));
        }
    }
    sortie
}

fn introuvable() -> Response {
    refus(
        StatusCode::NOT_FOUND,
        "livret_introuvable",
        "aucun livret de ce nom pour cet album".to_string(),
    )
}

/// `GET /library/albums/{id}/booklets` — les PDF du dossier de l'album.
/// Un album sans PDF (ou inconnu) rend une liste vide.
pub(super) async fn livrets_album(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
) -> Json<serde_json::Value> {
    let st = state.clone();
    let livrets = tokio::task::spawn_blocking(move || {
        livrets_des_dossiers(&dossiers_des_pistes(chemins_des_pistes(&st, album_id)))
    })
    .await
    .unwrap_or_default();
    Json(json!({
        "booklets": livrets
            .iter()
            .map(|l| json!({ "name": l.nom, "size": l.taille }))
            .collect::<Vec<_>>(),
    }))
}

/// `GET /library/albums/{id}/booklets/{name}` — le fichier, à ouvrir dans le
/// navigateur.
pub(super) async fn livret_album(
    State(state): State<AppState>,
    Path((album_id, nom)): Path<(i64, String)>,
) -> Response {
    let st = state.clone();
    let lu = tokio::task::spawn_blocking(move || {
        let livret = livret_a_servir(
            &dossiers_des_pistes(chemins_des_pistes(&st, album_id)),
            &nom,
        )?;
        let octets = std::fs::read(&livret.chemin).ok()?;
        Some((livret.nom, octets))
    })
    .await
    .ok()
    .flatten();
    let Some((nom, octets)) = lu else {
        return introuvable();
    };
    // `filename*` en UTF-8 percent-encodé (RFC 6266) : un nom accentué ne
    // passe pas tel quel dans un en-tête.
    let encode = valeur_rfc5987(&nom);
    let disposition = HeaderValue::from_str(&format!("inline; filename*=UTF-8''{encode}"))
        .unwrap_or_else(|_| HeaderValue::from_static("inline"));
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/pdf"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=300"),
            ),
        ],
        octets,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un dossier temporaire nettoyé par `Drop`, même si le test échoue
    /// (#3030 : `tune_core::test_scratch`).
    fn dossier_de_test(nom: &str) -> tune_core::test_scratch::ScratchDir {
        tune_core::test_scratch::scratch_dir(&format!("livrets-6481-{nom}"))
    }

    #[test]
    fn un_nom_de_livret_est_un_nom_pas_un_chemin() {
        assert!(nom_de_livret_valide("Livret.pdf"));
        assert!(nom_de_livret_valide("Booklet Kind of Blue.PDF"));
        assert!(nom_de_livret_valide("Livret numérique.pdf"));
        for refuse in [
            "",
            "../secret.pdf",
            "..pdf",
            "a/b.pdf",
            "a\\b.pdf",
            "/etc/x.pdf",
            ".cache.pdf",
            "x.pdf\0",
            "livret.pdf.exe",
            "notes.txt",
            "a..b.pdf",
        ] {
            assert!(
                !nom_de_livret_valide(refuse),
                "{refuse:?} devait être refusé"
            );
        }
    }

    #[test]
    fn les_dossiers_d_un_album_par_disque_comprennent_le_dossier_de_l_album() {
        let d = dossiers_des_pistes([
            "/musique/Coffret/CD1/01.flac",
            "/musique/Coffret/CD2/01.flac",
            "relatif/02.flac",
            "http://192.168.1.2/upnp/3.flac",
        ]);
        let attendus: BTreeSet<PathBuf> = [
            "/musique/Coffret",
            "/musique/Coffret/CD1",
            "/musique/Coffret/CD2",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(d, attendus);
    }

    #[test]
    fn seuls_les_pdf_du_dossier_sont_listes() {
        let racine_tmp = dossier_de_test("liste");
        let racine = racine_tmp.path();
        let album = racine.join("Album");
        std::fs::create_dir_all(album.join("Scans")).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        std::fs::write(album.join("Livret.pdf"), b"%PDF-1.4 livret").unwrap();
        std::fs::write(album.join("cover.jpg"), b"x").unwrap();
        std::fs::write(album.join(".cache.pdf"), b"x").unwrap();
        std::fs::write(album.join("Scans").join("profond.pdf"), b"x").unwrap();
        std::fs::write(racine.join("voisin.pdf"), b"x").unwrap();
        let dossiers = dossiers_des_pistes([album.join("01.flac").to_string_lossy()]);
        let noms: Vec<String> = livrets_des_dossiers(&dossiers)
            .into_iter()
            .map(|l| l.nom)
            .collect();
        assert_eq!(noms, vec!["Livret.pdf".to_string()]);
    }

    #[test]
    fn rien_hors_du_dossier_de_l_album_ne_se_sert() {
        let racine_tmp = dossier_de_test("garde");
        let racine = racine_tmp.path();
        let album = racine.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        std::fs::write(album.join("Livret.pdf"), b"%PDF-1.4 livret").unwrap();
        std::fs::write(racine.join("voisin.pdf"), b"%PDF voisin").unwrap();
        let dossiers = dossiers_des_pistes([album.join("01.flac").to_string_lossy()]);

        let servi = livret_a_servir(&dossiers, "Livret.pdf").expect("le livret de l'album");
        assert_eq!(std::fs::read(&servi.chemin).unwrap(), b"%PDF-1.4 livret");

        for tentative in [
            "../voisin.pdf",
            "..%2Fvoisin.pdf",
            "voisin.pdf",
            "01.flac",
            "/etc/passwd",
            "Livret.pdf/../../voisin.pdf",
        ] {
            assert!(
                livret_a_servir(&dossiers, tentative).is_none(),
                "{tentative:?} ne doit rien servir"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn un_lien_symbolique_n_est_ni_liste_ni_servi() {
        let racine_tmp = dossier_de_test("lien");
        let racine = racine_tmp.path();
        let album = racine.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        std::fs::write(racine.join("ailleurs.pdf"), b"%PDF ailleurs").unwrap();
        std::os::unix::fs::symlink(racine.join("ailleurs.pdf"), album.join("Lien.pdf")).unwrap();
        let dossiers = dossiers_des_pistes([album.join("01.flac").to_string_lossy()]);
        assert!(livrets_des_dossiers(&dossiers).is_empty());
        assert!(livret_a_servir(&dossiers, "Lien.pdf").is_none());
    }

    #[tokio::test]
    async fn les_routes_listent_et_servent_le_livret_de_l_album() {
        let racine_tmp = dossier_de_test("routes");
        let racine = racine_tmp.path();
        let album = racine.join("Kind of Blue");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        std::fs::write(album.join("Livret numérique.pdf"), b"%PDF-1.4 kob").unwrap();
        std::fs::write(racine.join("voisin.pdf"), b"%PDF voisin").unwrap();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let piste = album.join("01.flac").to_string_lossy().into_owned();
        state
            .backend
            .execute_batch(&format!(
                "INSERT INTO albums (id, title) VALUES (64, 'Kind of Blue');
                 INSERT INTO tracks (id, title, file_path, album_id) VALUES (6401, 'So What', '{}', 64);",
                piste.replace('\'', "''")
            ))
            .unwrap();

        let Json(liste) = livrets_album(State(state.clone()), Path(64)).await;
        assert_eq!(
            liste,
            json!({"booklets": [{"name": "Livret numérique.pdf", "size": 12}]})
        );
        let Json(vide) = livrets_album(State(state.clone()), Path(999)).await;
        assert_eq!(vide, json!({"booklets": []}));

        let r = livret_album(
            State(state.clone()),
            Path((64, "Livret numérique.pdf".into())),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/pdf");
        assert!(
            r.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .starts_with("inline; filename*=UTF-8''Livret%20num")
        );
        let corps = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&corps[..], b"%PDF-1.4 kob");

        for tentative in ["../voisin.pdf", "voisin.pdf", "01.flac"] {
            let r = livret_album(State(state.clone()), Path((64, tentative.into()))).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{tentative:?}");
        }
        // Le livret d'un album n'est pas servi au nom d'un autre album.
        let r = livret_album(State(state), Path((999, "Livret numérique.pdf".into()))).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}
