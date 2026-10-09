//! Côté routes, le réglage « Écrire les modifications dans les fichiers
//! audio » (`tune_core::metadata::ecriture_fichiers`) : une seule lecture, un
//! seul refus, une seule forme de réponse.
//!
//! Deux familles de routes :
//!
//! * **Celles qui modifient la base ET le fichier** (édition d'une piste,
//!   édition en lot, compositeur depuis les crédits, import) : désactivé, elles
//!   enregistrent en base, n'ouvrent pas le fichier, et le disent dans leur
//!   réponse par [`CHAMP_REPONSE`] = `false`. L'interface affiche alors
//!   « enregistré dans Tune, fichiers inchangés ».
//! * **Celles qui n'existent que pour écrire dans les fichiers** (« Écrire
//!   dans les fichiers », gravures DR et compilation, nettoyage des balises) :
//!   désactivé, elles refusent par [`refus`] — `409`, code
//!   [`ecriture_fichiers::CODE_REFUS`] — sans rien toucher.
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tune_core::metadata::ecriture_fichiers;

use crate::state::AppState;

pub(crate) use tune_core::metadata::ecriture_fichiers::CODE_REFUS;

/// Champ des réponses qui dit si l'écriture dans les fichiers était permise.
pub(crate) const CHAMP_REPONSE: &str = "file_writes_enabled";

/// Le réglage, lu en base. Absent : désactivé.
pub(crate) fn autorisee(state: &AppState) -> bool {
    ecriture_fichiers::autorisee(&state.backend)
}

/// Le refus d'une route qui n'a pas d'autre effet que d'écrire dans les
/// fichiers. Rien n'a été écrit, et le corps nomme le réglage à cocher.
pub(crate) fn refus(route: &'static str) -> Response {
    tracing::info!(route, "ecriture_fichiers_desactivee_refus");
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": ecriture_fichiers::CODE_REFUS,
            "code": ecriture_fichiers::CODE_REFUS,
            "setting": ecriture_fichiers::CLE,
            "message": ecriture_fichiers::MOTIF_REFUS,
            CHAMP_REPONSE: false,
        })),
    )
        .into_response()
}

/// Coche le réglage, pour les essais qui éprouvent une écriture réelle.
#[cfg(test)]
pub(crate) fn activer_pour_test(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) {
    tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
        .set(ecriture_fichiers::CLE, "true")
        .expect("réglage d'écriture des fichiers");
}

/// 🔴 LA GARDE — « rien n'écrit dans les fichiers audio sans passer par le
/// réglage ».
///
/// Recense, dans le code de PRODUCTION de `tune-core/src` et
/// `tune-server/src` (modules d'essai et commentaires écartés), chaque appel
/// à un écrivain de balises. Échoue si :
///
/// * un fichier appelle un écrivain sans nommer `ecriture_fichiers` (le
///   réglage) et sans figurer parmi les définitions ou les exemptions
///   justifiées ci-dessous ;
/// * le NOMBRE d'appels d'un fichier change : un nouveau chemin d'écriture
///   dans un fichier déjà gardé ne passe pas inaperçu. Relire le chemin,
///   vérifier qu'il consulte [`autorisee`] (ou
///   `tune_core::metadata::ecriture_fichiers::autorisee`), PUIS seulement
///   mettre à jour [`RECENSEMENT`].
#[cfg(test)]
mod garde_ecriture_fichiers {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// Les écrivains : appels lofty bruts, et les fonctions du dépôt qui
    /// enregistrent des balises dans un fichier audio.
    const ECRIVAINS: &[&str] = &[
        ".save_to_path(",
        ".save_to(",
        "write_tags(",
        "write_metadata(",
        "write_metadata_to_file(",
        "write_metadata_to_file_sync(",
        "graver_dr(",
        "ecrire_balises_edition(",
        "ecrire_atomiquement(",
        "apply_tags_to_file(",
        "ecrire_par_tag(",
        "ecrire_par_tag_generique(",
        // Le `.lrc` voisin (décision du 05/10/2026 : sous le même réglage).
        "write_sidecar_lrc(",
    ];

    /// Où les écrivains sont DÉFINIS : le réglage se lit chez leurs appelants,
    /// qui n'ont pas tous une base sous la main au niveau du `Tag` lofty.
    const DEFINITIONS: &[&str] = &[
        "tune-core/src/metadata/tag_writer.rs",
        "tune-core/src/metadata/mod.rs",
        "tune-core/src/metadata/lyrics.rs",
    ];

    /// Exemptions, chacune avec sa raison.
    const EXEMPTIONS: &[(&str, &str)] = &[
        (
            "tune-server/src/routes/converter.rs",
            "recopie les balises dans le fichier CONVERTI que Tune vient de créer, \
             jamais dans l'original de l'utilisateur",
        ),
        (
            "tune-core/src/queue_persistence.rs",
            "homonyme : `ecrire_atomiquement` y écrit la file d'attente (JSON), pas un fichier audio",
        ),
        (
            "tune-core/src/audio/iso9660/epreuves_5299.rs",
            "module d'essai (`#[cfg(test)] mod epreuves_5299;`)",
        ),
        (
            "tune-server/src/routes/library/rescan_metadata_errors_3816.rs",
            "module d'essai (`#[cfg(test)] #[path = …] mod rescan_metadata_errors_3816;`)",
        ),
    ];

    /// Appels recensés au 05/10/2026, par fichier. Un écart fait échouer.
    const RECENSEMENT: &[(&str, usize)] = &[
        ("tune-core/src/audio/iso9660/epreuves_5299.rs", 2),
        ("tune-core/src/library/lyrics_pass.rs", 2),
        ("tune-core/src/metadata/lyrics.rs", 1),
        ("tune-core/src/metadata/mod.rs", 2),
        ("tune-core/src/metadata/tag_writer.rs", 21),
        ("tune-core/src/queue_persistence.rs", 2),
        ("tune-server/src/routes/converter.rs", 1),
        (
            "tune-server/src/routes/library/compositeur_depuis_credits.rs",
            1,
        ),
        ("tune-server/src/routes/library/edition_balises.rs", 1),
        ("tune-server/src/routes/library/graver_compilation.rs", 1),
        ("tune-server/src/routes/library/graver_dr.rs", 1),
        ("tune-server/src/routes/library/ingest.rs", 1),
        (
            "tune-server/src/routes/library/rescan_metadata_errors_3816.rs",
            1,
        ),
        ("tune-server/src/routes/library/tracks.rs", 1),
        ("tune-server/src/routes/library/write_tags.rs", 2),
        ("tune-server/src/routes/metadata.rs", 1),
        ("tune-server/src/routes/tagger.rs", 8),
    ];

    /// Le texte de production : sans commentaires `//`, sans blocs
    /// `#[cfg(test)] mod … { … }`.
    fn production(texte: &str) -> String {
        let lignes: Vec<&str> = texte.lines().collect();
        let mut garde = String::new();
        let mut i = 0;
        while i < lignes.len() {
            let t = lignes[i].trim();
            if t.starts_with("#[cfg(test)]") || t.starts_with("#[cfg(all(test") {
                let mut j = i + 1;
                while j < lignes.len() && lignes[j].trim().starts_with("#[") {
                    j += 1;
                }
                let tete = lignes.get(j).map(|l| l.trim()).unwrap_or("");
                let est_bloc = tete.contains("mod ") && tete.ends_with('{');
                if est_bloc {
                    let mut profondeur: i64 = 0;
                    let mut k = j;
                    while k < lignes.len() {
                        let l = lignes[k];
                        profondeur += l.matches('{').count() as i64;
                        profondeur -= l.matches('}').count() as i64;
                        if profondeur <= 0 {
                            break;
                        }
                        k += 1;
                    }
                    i = k + 1;
                    continue;
                }
            }
            if !t.starts_with("//") {
                garde.push_str(lignes[i]);
                garde.push('\n');
            }
            i += 1;
        }
        garde
    }

    fn fichiers(dossier: &Path, sortie: &mut Vec<PathBuf>) {
        for entree in std::fs::read_dir(dossier).expect("dossier source lisible") {
            let chemin = entree.expect("entrée").path();
            if chemin.is_dir() {
                fichiers(&chemin, sortie);
            } else if chemin.extension().is_some_and(|e| e == "rs") {
                sortie.push(chemin);
            }
        }
    }

    fn recenser() -> BTreeMap<String, (usize, bool)> {
        let racine = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut vus = BTreeMap::new();
        for base in ["tune-core/src", "tune-server/src"] {
            let mut tous = Vec::new();
            fichiers(&racine.join(base), &mut tous);
            for chemin in tous {
                let nom = chemin.file_name().unwrap().to_string_lossy().into_owned();
                if nom.contains("test") {
                    continue;
                }
                let texte = std::fs::read_to_string(&chemin).expect("source lisible");
                let prod = production(&texte);
                let n: usize = ECRIVAINS.iter().map(|e| prod.matches(e).count()).sum();
                if n == 0 {
                    continue;
                }
                let relatif = chemin
                    .strip_prefix(&racine)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                vus.insert(relatif, (n, prod.contains("ecriture_fichiers")));
            }
        }
        vus
    }

    #[test]
    fn tout_ecrivain_de_fichier_audio_consulte_le_reglage() {
        let vus = recenser();
        let mut fautes = Vec::new();
        for (fichier, (_, consulte)) in &vus {
            let defini = DEFINITIONS.contains(&fichier.as_str());
            let exempt = EXEMPTIONS.iter().any(|(f, _)| f == fichier);
            if !consulte && !defini && !exempt {
                fautes.push(format!(
                    "{fichier} écrit dans des fichiers audio sans consulter le réglage \
                     `ecriture_fichiers` (« Écrire les modifications dans les fichiers audio »)"
                ));
            }
        }
        let attendu: BTreeMap<String, usize> = RECENSEMENT
            .iter()
            .map(|(f, n)| (f.to_string(), *n))
            .collect();
        let mesure: BTreeMap<String, usize> =
            vus.iter().map(|(f, (n, _))| (f.clone(), *n)).collect();
        if attendu != mesure {
            fautes.push(format!(
                "le recensement des écrivains a changé — un chemin d'écriture est apparu ou \
                 a disparu. Vérifier qu'il consulte le réglage, puis mettre RECENSEMENT à \
                 jour.\n  attendu : {attendu:?}\n  mesuré  : {mesure:?}"
            ));
        }
        assert!(fautes.is_empty(), "{}", fautes.join("\n"));
    }

    /// La garde se garde elle-même : elle VOIT un écrivain non gardé.
    #[test]
    fn la_garde_voit_un_ecrivain_hors_reglage() {
        let source = "fn f() {\n    tag.save_to_path(p, o);\n}\n\
                      #[cfg(test)]\nmod tests {\n    fn g() { x.save_to(f); }\n}\n";
        let prod = production(source);
        assert_eq!(prod.matches(".save_to_path(").count(), 1);
        assert_eq!(
            prod.matches(".save_to(").count(),
            0,
            "le module d'essai est écarté"
        );
        assert!(!prod.contains("ecriture_fichiers"));
    }

    /// 🔴 LA CONTRE-ÉPREUVE PERMANENTE de la demande du 05/10/2026 : le défaut
    /// est « désactivé ». Remettre `DEFAUT = true` fait rougir ce test.
    #[test]
    fn le_defaut_est_desactive() {
        assert!(
            !std::hint::black_box(tune_core::metadata::ecriture_fichiers::DEFAUT),
            "le réglage doit être désactivé par défaut"
        );
        assert!(!tune_core::metadata::ecriture_fichiers::depuis_valeur(None));
    }
}
