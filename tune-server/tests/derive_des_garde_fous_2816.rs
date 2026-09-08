//! Détecter la DÉRIVE des garde-fous : un témoin qui cesse de témoigner (#2816).
//!
//! Un garde-fou ne meurt pas en rougissant. Il meurt en restant VERT : le
//! fichier n'est plus compilé, la caisse n'est plus nommée, l'étape a perdu la
//! variable qui réveillait le témoin. La porte continue de passer, et plus rien
//! ne garde ce qu'elle prétendait garder. Cinq occurrences relevées le
//! 06/09/2026, toutes trouvées PAR HASARD :
//!
//! | ce qui avait dérivé | découvert par |
//! |---|---|
//! | `tune-output-api` (7 essais) n'était exécuté par aucune porte — membre IMPLICITE, la garde ne lisait que `members` | #3266/#3532 |
//! | sept témoins PostgreSQL sautaient depuis le 03/09 : aucune étape ne leur posait l'adresse de la base | #3519, #3520 |
//! | des fichiers de `tests/` non enregistrés sous `autotests = false` | #2963, puis #3553 |
//! | une garde qui LIT le texte d'un fichier au lieu d'APPELER la fonction gardée | revue du 06/09 |
//! | neutraliser #3233 (porteur DoP) ne fait rougir aucun essai : les témoins couvrent la fonction pure, pas les quatre sites d'appel | contre-épreuve du 06/09 |
//!
//! Ce fichier ferme mécaniquement les deux premières familles. Les deux
//! dernières ne se ferment pas par une porte, et une porte qui prétendrait le
//! faire donnerait une FAUSSE assurance ; la convention qu'elles appellent est
//! écrite plus bas, sans code.
//!
//! ## Ce que ce fichier NE refait pas
//!
//! * `tune-server/tests/tests_orphelins.rs` et son jumeau de `tune-core`
//!   refusent déjà, CHACUN DANS SA CAISSE, un fichier de `tests/` que personne
//!   ne compile. La garde d'ici ne les remplace pas : elle porte le même refus
//!   à l'ÉCHELLE DU WORKSPACE — membres implicites compris, chaînes de `#[path]`
//!   résolues de proche en proche — et exige qu'une caisse à `autotests = false`
//!   emporte son propre refus. Sans elle, une caisse NEUVE naîtrait sans garde
//!   et personne ne le verrait : c'est exactement la dérive du ticket.
//! * `tout_membre_du_workspace_est_execute_par_une_porte_cargo_test` et
//!   `toute_caisse_du_workspace_est_nommee_par_une_porte_clippy`
//!   (`workflows_bornes.rs`) tiennent l'axe des PAQUETS. #3532 y ajoute l'axe
//!   des JEUX DE FONCTIONNALITÉS. Rien ici n'y touche.
//!
//! ## L'axe qui manquait : la variable d'environnement
//!
//! Un essai qui commence par « pas de variable, je saute » ne rougit jamais.
//! Il ne peut pas : il s'annonce vert, `cargo` le compte comme passé, et le
//! journal du job dit `ok` sur une ligne que personne ne relit. C'est le mode
//! de panne le plus silencieux du dépôt — les TRENTE témoins recensés plus bas,
//! dont les quinze épreuves E2E PostgreSQL de `postgres_e2e.rs`, sont dans ce
//! cas AUJOURD'HUI, et rien ne le disait.
//!
//! La garde ne les répare pas : elle les RECENSE. Un témoin sauté doit figurer
//! dans `SAUTS_CONNUS` avec sa raison. Un témoin neuf qui saute fait rougir ;
//! un témoin qui redevient exécuté fait rougir aussi, pour qu'on retire son
//! entrée au lieu de la laisser mentir.
//!
//! ## Convention pour les deux familles que ce fichier ne garde pas
//!
//! 1. **Une garde APPELLE, elle ne LIT pas.** Un test qui vérifie un
//!    comportement en cherchant une chaîne dans le texte d'un fichier source
//!    reste vert quand on désactive le comportement : il garde l'orthographe,
//!    pas la conduite. Deux occurrences le 06/09 (client web, menu de piste).
//!    Lire le texte reste légitime pour garder une FORME — une ligne de
//!    workflow, une entrée de manifeste — jamais pour garder une CONDUITE.
//! 2. **Une garde nomme le SITE D'APPEL, pas seulement la fonction pure.**
//!    Neutraliser #3233 ne fait rougir aucun essai parce que les témoins
//!    appellent la fonction de décision, jamais les quatre endroits qui s'en
//!    servent. La contre-épreuve d'un correctif se joue sur le site, pas sur la
//!    fonction.
//!
//! Ces deux règles relèvent du jugement humain à la revue. Aucune porte ne les
//! décide ici, et c'est délibéré.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Motifs reconnus.
//
// Ils vivent en CONSTANTES DE TÊTE, hors de toute fonction, pour que ce fichier
// ne se recense pas lui-même : le détecteur ne regarde que les corps de
// fonctions, et aucun corps d'ici ne porte le motif entier.
// ---------------------------------------------------------------------------

/// Début d'une lecture d'environnement.
const MARQUEUR_LECTURE: &str = "env::var(";
/// Seules les variables du projet comptent : `PATH` ou `HOME` ne sont pas des
/// témoins, ce sont des lectures d'exploitation.
const PREFIXE_VARIABLE: &str = "TUNE_";
/// Le garde-fou maison de `tune-core/src/db/postgres_e2e.rs`.
const MARQUEUR_PG_OR_SKIP: &str = "pg_or_skip!()";

/// Options de `cargo test` qui prennent une valeur SÉPARÉE : sans cette liste,
/// le mot qui suit passerait pour un filtre positionnel.
const OPTIONS_A_VALEUR: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "--features",
    "--test",
    "--bench",
    "--example",
    "--bin",
    "--manifest-path",
    "--target",
    "--target-dir",
    "--profile",
    "-j",
    "--jobs",
];

/// Sélecteurs de cible qui EXCLUENT les essais unitaires de la bibliothèque.
const SELECTEURS_HORS_LIB: &[&str] = &["--test", "--bench", "--example", "--bin", "--doc"];

// ---------------------------------------------------------------------------
// Le recensement des sauts CONNUS.
//
// Une entrée = un fichier, la variable qui l'endort, les témoins qu'elle
// endort, et la raison. La liste se relit ; un oubli, non.
//
// Mesure du 06/09/2026 sur `batch/bugs-6` (8bfdf484) : trente-neuf témoins,
// neuf exécutés, TRENTE sautés, répartis sur dix fichiers.
// ---------------------------------------------------------------------------

/// `(fichier, variable, témoins, raison)`.
const SAUTS_CONNUS: &[(&str, &str, &[&str], &str)] = &[
    (
        "tune-core/src/db/pg_sqlite_type_parity.rs",
        "TUNE_TEST_PG_URL",
        &["aucune_exception_perimee", "parite_des_types_pg_sqlite"],
        "Parité des types PG/SQLite : aucune étape de `test-postgres.yml` ne \
         nomme un filtre qui les atteigne. À rattacher — une étape de plus, \
         même base jetable.",
    ),
    (
        "tune-core/src/db/postgres_e2e.rs",
        "TUNE_TEST_PG_URL",
        &[
            "pg_1220_numeric_columns_have_numeric_types",
            "pg_1752_l_antislash_de_windows_reste_litteral",
            "pg_2168_facette_profonde_rend_le_meme_ensemble_que_sqlite",
            "pg_2458_empty_mbid_album_artist_repair",
            "pg_2468_runner_heals_bookmarks_position_integer_to_bigint",
            "pg_3039_fenetre_et_decompte_des_ajouts_recents",
            "pg_3101_les_jokers_du_nom_de_dossier_ne_filtrent_pas_plus_large",
            "pg_albums_round_trip",
            "pg_artists_round_trip",
            "pg_history_round_trip",
            "pg_hors_fonds_communautaire_compte_les_artistes_sans_mbid",
            "pg_playlists_round_trip",
            "pg_settings_round_trip",
            "pg_tracks_round_trip",
            "pg_zones_round_trip",
        ],
        "Les E2E historiques restent volontairement sur `scripts/pg-e2e.sh` : \
         `test-postgres.yml` ne pose la base que pour cinq filtres nommés \
         (#1706, #2860, #2441, `pg_config_backup`, `pg_schema_parity`). Quinze \
         épreuves sur dix-neuf ne tournent donc sur AUCUNE porte. Le chiffre \
         est ici pour qu'on le décide, au lieu de le découvrir.",
    ),
    (
        "tune-core/src/orchestrator/tests.rs",
        "TUNE_DIAG_PROBE_URL",
        &["diag_probe_emits_bus_events"],
        "Sonde de diagnostic branchée sur une session proxy VIVANTE : elle ne \
         peut pas tourner sur un runner. Saut assumé.",
    ),
    (
        "tune-core/tests/dsd_streaming_repro.rs",
        "TUNE_DSD_REAL_FILE",
        &["dsd_streaming_local_and_network_paths_valid"],
        "Demande un vrai fichier .dsf/.dff sur le disque : rien à embarquer \
         dans le dépôt. Saut assumé.",
    ),
    (
        "tune-core/tests/migration_on_real_db.rs",
        "TUNE_REAL_DB",
        &["merge_scattered_on_a_real_database"],
        "Demande une base de production copiée à la main. Saut assumé.",
    ),
    (
        "tune-server/tests/compilation_dans_les_reponses_album.rs",
        "TUNE_TEST_PG_URL",
        &["pg_i1957_la_colonne_est_un_entier_et_les_routes_servent_le_drapeau"],
        "#1957 sur PostgreSQL : jamais rattaché. Le témoin SQLite jumeau tourne \
         dans `ci.yml` ; celui-ci attend son étape.",
    ),
    (
        "tune-server/tests/comptes_par_source_2147.rs",
        "TUNE_TEST_PG_URL",
        &["i2147_pg_la_ventilation_par_source_tourne_sur_postgresql"],
        "#2147 sur PostgreSQL : jamais rattaché. Même famille que #3519/#3520.",
    ),
    (
        "tune-server/tests/pg_2372_versions_par_piste.rs",
        "TUNE_TEST_PG_URL",
        &["pg_2372_versions_par_piste_rendent_le_meme_ordre_que_sqlite"],
        "#2372 sur PostgreSQL : jamais rattaché, alors que le fichier porte \
         `postgres` en `#![cfg]` et une cible `[[test]]` à lui.",
    ),
];

// ---------------------------------------------------------------------------
// Lecture du workspace.
// ---------------------------------------------------------------------------

fn racine() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn lire(chemin: &Path) -> String {
    fs::read_to_string(chemin).unwrap_or_else(|e| panic!("{} illisible : {e}", chemin.display()))
}

/// La valeur d'une clef `nom = "…"` dans la section `[package]` — jamais celle
/// d'un `[[bin]]` qui suivrait dans le même fichier.
fn nom_du_paquet(source: &str) -> Option<String> {
    let mut dans_package = false;
    for ligne in source.lines() {
        let t = ligne.trim();
        if t.starts_with('[') {
            dans_package = t == "[package]";
            continue;
        }
        if !dans_package || t.starts_with('#') {
            continue;
        }
        if let Some(reste) = t.strip_prefix("name")
            && let Some(reste) = reste.trim_start().strip_prefix('=')
        {
            return Some(reste.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Résout un chemin `cible` relatif au dossier `depuis`, ramené à la racine du
/// workspace. Rend `None` si le résultat sort du dépôt.
fn resoudre(depuis: &str, cible: &str) -> Option<String> {
    let mut pile: Vec<&str> = depuis.split('/').filter(|e| !e.is_empty()).collect();
    for element in cible.split('/') {
        match element {
            "." | "" => {}
            ".." => {
                if pile.pop().is_none() {
                    return None;
                }
            }
            autre => pile.push(autre),
        }
    }
    Some(pile.join("/"))
}

/// Les membres du workspace : ceux de `members`, PUIS ceux qui y entrent par
/// une dépendance `path` interne — c'est ainsi que `tune-output-api` est
/// membre sans figurer dans la liste (#3266).
fn membres(racine: &Path) -> Vec<(String, String)> {
    let manifeste = lire(&racine.join("Cargo.toml"));
    let debut = manifeste
        .find("members = [")
        .expect("`members = [` absent du Cargo.toml du workspace");
    let reste = &manifeste[debut + "members = [".len()..];
    let fin = reste
        .find(']')
        .expect("la liste `members` du workspace n'est pas fermée");

    let mut a_visiter: Vec<String> = reste[..fin]
        .split(',')
        .map(|m| m.trim().trim_matches('"').to_string())
        .filter(|m| !m.is_empty())
        .collect();
    assert!(
        a_visiter.len() >= 10,
        "seulement {} membre(s) explicite(s) reconnu(s) : la forme de `members` \
         a changé et cette garde ne garde plus rien — {a_visiter:?}",
        a_visiter.len()
    );

    let mut vus: BTreeSet<String> = BTreeSet::new();
    let mut trouves: Vec<(String, String)> = Vec::new();
    while let Some(chemin) = a_visiter.pop() {
        if !vus.insert(chemin.clone()) {
            continue;
        }
        let source = lire(&racine.join(&chemin).join("Cargo.toml"));
        let nom = nom_du_paquet(&source)
            .unwrap_or_else(|| panic!("{chemin}/Cargo.toml : `[package] name` introuvable"));
        trouves.push((chemin.clone(), nom));

        for ligne in source.lines() {
            let t = ligne.trim();
            if t.starts_with('#') {
                continue;
            }
            let Some((_, apres)) = t.split_once("path = \"") else {
                continue;
            };
            let Some((cible, _)) = apres.split_once('"') else {
                continue;
            };
            let Some(resolu) = resoudre(&chemin, cible) else {
                continue;
            };
            if !resolu.is_empty() && racine.join(&resolu).join("Cargo.toml").is_file() {
                a_visiter.push(resolu);
            }
        }
    }
    trouves.sort();
    trouves
}

/// Une caisse porte-t-elle `autotests = false` ?
fn autotests_desactives(source: &str) -> bool {
    source
        .lines()
        .any(|l| l.trim_start().starts_with("autotests") && l.contains("false"))
}

/// Les cibles `[[test]]` déclarées : `(nom, chemin relatif à la caisse)`.
/// Sans `path`, cargo déduit `tests/<nom>.rs`.
fn cibles_declarees(source: &str) -> Vec<(String, String)> {
    let mut trouvees: Vec<(String, String)> = Vec::new();
    let mut dans_bloc = false;
    let mut nom: Option<String> = None;
    let mut chemin: Option<String> = None;
    let ferme = |nom: &mut Option<String>,
                 chemin: &mut Option<String>,
                 trouvees: &mut Vec<(String, String)>| {
        if let Some(n) = nom.take() {
            let c = chemin.take().unwrap_or_else(|| format!("tests/{n}.rs"));
            trouvees.push((n, c));
        } else {
            *chemin = None;
        }
    };
    for ligne in source.lines() {
        let t = ligne.trim();
        if t.starts_with('[') {
            ferme(&mut nom, &mut chemin, &mut trouvees);
            dans_bloc = t == "[[test]]";
            continue;
        }
        if !dans_bloc || t.starts_with('#') {
            continue;
        }
        if let Some(reste) = t.strip_prefix("name")
            && let Some(reste) = reste.trim_start().strip_prefix('=')
        {
            nom = Some(reste.trim().trim_matches('"').to_string());
        } else if let Some(reste) = t.strip_prefix("path")
            && let Some(reste) = reste.trim_start().strip_prefix('=')
        {
            chemin = Some(reste.trim().trim_matches('"').to_string());
        }
    }
    ferme(&mut nom, &mut chemin, &mut trouvees);
    trouvees
}

/// Les fichiers `.rs` posés à la RACINE de `tests/` — les sous-dossiers
/// (`fixtures/`, modules d'un agrégateur) ne sont pas des cibles.
fn fichiers_poses(dossier: &Path) -> Vec<String> {
    let mut noms: Vec<String> = fs::read_dir(dossier)
        .unwrap_or_else(|e| panic!("{} illisible : {e}", dossier.display()))
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".rs"))
        .map(|n| format!("tests/{n}"))
        .collect();
    noms.sort();
    noms
}

/// Ce que le compilateur atteint dans `tests/` d'une caisse :
/// `chemin -> (cible qui l'emporte, chemin de module)`.
///
/// La résolution est TRANSITIVE : un agrégateur qui déclare
/// `#[path = "…"] mod x;` peut lui-même en déclarer d'autres. La garde de
/// `tests_orphelins.rs` ne descend qu'un cran — assez pour aujourd'hui, pas
/// pour demain.
fn atteints(racine: &Path, chemin_caisse: &str) -> BTreeMap<String, (String, String)> {
    let source = lire(&racine.join(chemin_caisse).join("Cargo.toml"));
    let dossier = racine.join(chemin_caisse).join("tests");
    let mut pile: Vec<(String, String, String)> = if autotests_desactives(&source) {
        cibles_declarees(&source)
            .into_iter()
            .map(|(n, c)| (n, c, String::new()))
            .collect()
    } else {
        fichiers_poses(&dossier)
            .into_iter()
            .map(|c| {
                let nom = c
                    .trim_start_matches("tests/")
                    .trim_end_matches(".rs")
                    .to_string();
                (nom, c, String::new())
            })
            .collect()
    };

    let mut table: BTreeMap<String, (String, String)> = BTreeMap::new();
    while let Some((cible, chemin, module)) = pile.pop() {
        if table.contains_key(&chemin) {
            continue;
        }
        table.insert(chemin.clone(), (cible.clone(), module.clone()));
        let fichier = racine.join(chemin_caisse).join(&chemin);
        if !fichier.is_file() {
            continue;
        }
        let source = lire(&fichier);
        let base = match chemin.rsplit_once('/') {
            Some((debut, _)) => debut.to_string(),
            None => String::new(),
        };
        for (sous_chemin, nom_module) in modules_par_chemin(&source) {
            let Some(resolu) = resoudre(&base, &sous_chemin) else {
                continue;
            };
            let module = if module.is_empty() {
                nom_module
            } else {
                format!("{module}::{nom_module}")
            };
            pile.push((cible.clone(), resolu, module));
        }
    }
    table
}

/// Les `#[path = "…"] mod nom;` d'un fichier : `(chemin, nom du module)`.
fn modules_par_chemin(source: &str) -> Vec<(String, String)> {
    let lignes: Vec<&str> = source.lines().collect();
    let mut trouves = Vec::new();
    for (i, ligne) in lignes.iter().enumerate() {
        let t = ligne.trim();
        let Some(reste) = t.strip_prefix("#[path") else {
            continue;
        };
        let Some(reste) = reste.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(reste) = reste.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some((chemin, _)) = reste.split_once('"') else {
            continue;
        };
        // `mod nom;` est sur la même ligne ou sur la suivante (rustfmt).
        let mut nom = None;
        for suivante in lignes.iter().skip(i).take(2) {
            let s = suivante.trim();
            let apres = s
                .split_once("mod ")
                .map(|(_, a)| a)
                .unwrap_or_default()
                .trim();
            if !apres.is_empty() {
                nom = Some(
                    apres
                        .trim_end_matches(';')
                        .trim_end_matches('{')
                        .trim()
                        .to_string(),
                );
                break;
            }
        }
        if let Some(nom) = nom {
            trouves.push((chemin.to_string(), nom));
        }
    }
    trouves
}

// ---------------------------------------------------------------------------
// Lecture du code : fonctions, témoins.
// ---------------------------------------------------------------------------

/// L'indentation d'une ligne, en espaces.
fn indentation(ligne: &str) -> usize {
    ligne.len() - ligne.trim_start().len()
}

/// Le nom déclaré par une ligne `fn …`, `async fn …`, `pub fn …`.
fn nom_de_fonction(ligne: &str) -> Option<String> {
    let mut reste = ligne.trim_start();
    for prefixe in ["pub(crate) ", "pub ", "const ", "async ", "unsafe "] {
        if let Some(sans) = reste.strip_prefix(prefixe) {
            reste = sans.trim_start();
        }
    }
    let reste = reste.strip_prefix("fn ")?;
    let nom: String = reste
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if nom.is_empty() { None } else { Some(nom) }
}

/// `(nom, corps)` de TOUTE fonction du fichier. Le corps court jusqu'à
/// l'accolade fermante posée à la MÊME indentation que la déclaration —
/// le dépôt est passé à `cargo fmt`, cette borne est fiable.
fn toutes_les_fonctions(source: &str) -> Vec<(String, String)> {
    let lignes: Vec<&str> = source.lines().collect();
    let mut trouvees = Vec::new();
    for (i, ligne) in lignes.iter().enumerate() {
        let Some(nom) = nom_de_fonction(ligne) else {
            continue;
        };
        let marge = indentation(ligne);
        let mut corps = Vec::new();
        for suivante in lignes.iter().skip(i + 1) {
            if suivante.trim() == "}" && indentation(suivante) == marge {
                break;
            }
            corps.push(*suivante);
        }
        trouvees.push((nom, corps.join("\n")));
    }
    trouvees
}

/// `(nom, corps)` des seules fonctions annotées `#[test]` / `#[tokio::test…]`.
fn fonctions_de_test(source: &str) -> Vec<(String, String)> {
    let lignes: Vec<&str> = source.lines().collect();
    let mut trouvees = Vec::new();
    for (i, ligne) in lignes.iter().enumerate() {
        let t = ligne.trim();
        if !(t.starts_with("#[test]") || t.starts_with("#[tokio::test")) {
            continue;
        }
        let Some((decalage, nom)) = lignes
            .iter()
            .enumerate()
            .skip(i + 1)
            .take(6)
            .find_map(|(j, l)| nom_de_fonction(l).map(|n| (j, n)))
        else {
            continue;
        };
        let marge = indentation(lignes[decalage]);
        let mut corps = Vec::new();
        for suivante in lignes.iter().skip(decalage + 1) {
            if suivante.trim() == "}" && indentation(suivante) == marge {
                break;
            }
            corps.push(*suivante);
        }
        trouvees.push((nom, corps.join("\n")));
    }
    trouvees
}

/// Les variables du projet lues dans ce fragment.
fn variables_lues(fragment: &str) -> Vec<String> {
    let mut trouvees = Vec::new();
    let mut reste = fragment;
    while let Some(position) = reste.find(MARQUEUR_LECTURE) {
        reste = &reste[position + MARQUEUR_LECTURE.len()..];
        let apres = reste.trim_start();
        let Some(apres) = apres.strip_prefix('"') else {
            continue;
        };
        let Some((nom, _)) = apres.split_once('"') else {
            continue;
        };
        if nom.starts_with(PREFIXE_VARIABLE) {
            trouvees.push(nom.to_string());
        }
    }
    trouvees
}

/// Les fonctions d'aide du fichier qui lisent une variable du projet :
/// `nom -> variable`. C'est par elles que passent les témoins de `tests/`
/// (`let Some(url) = url_pg() else { return; }`).
fn aides(source: &str) -> BTreeMap<String, String> {
    let mut table = BTreeMap::new();
    for (nom, corps) in toutes_les_fonctions(source) {
        if let Some(variable) = variables_lues(&corps).into_iter().next() {
            table.insert(nom, variable);
        }
    }
    table
}

/// La variable dont dépend ce témoin, s'il en dépend d'une.
///
/// L'idiome reconnu est le SAUT : un `let` dont la valeur vient d'une lecture
/// d'environnement (directe ou par une aide du fichier) et dont l'échec rend
/// la main — `else { return; }` ou un bras `_ => { … return; }`. Une lecture
/// posée AILLEURS dans le corps n'est pas un saut : `scan_io_concurrency_env_override`
/// (tune-core/src/scanner/walker.rs) MESURE la variable, il ne s'endort pas
/// sans elle. Confondre les deux ferait crier la garde sur un innocent, et une
/// garde qui crie faux finit désarmée.
fn variable_du_temoin(corps: &str, aides: &BTreeMap<String, String>) -> Option<String> {
    if corps.contains(MARQUEUR_PG_OR_SKIP) {
        return Some("TUNE_TEST_PG_URL".to_string());
    }
    let lignes: Vec<&str> = corps.lines().collect();
    for (i, ligne) in lignes.iter().enumerate() {
        if !ligne.trim_start().starts_with("let ") {
            continue;
        }
        let fenetre: String = lignes
            .iter()
            .skip(i)
            .take(9)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        if !fenetre.contains("else") && !fenetre.contains("return") {
            continue;
        }
        let tete: String = lignes
            .iter()
            .skip(i)
            .take(3)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(variable) = variables_lues(&tete).into_iter().next() {
            return Some(variable);
        }
        for (nom, variable) in aides {
            if tete.contains(&format!("{nom}(")) {
                return Some(variable.clone());
            }
        }
    }
    None
}

/// Les jeux de fonctionnalités qui gardent le fichier ENTIER
/// (`#![cfg(feature = "…")]`, éventuellement dans un `all(…)`).
fn fonctionnalites_de_tete(source: &str) -> BTreeSet<String> {
    let mut trouvees = BTreeSet::new();
    for ligne in source.lines() {
        let t = ligne.trim();
        if !t.starts_with("#![") {
            continue;
        }
        let mut reste = t;
        while let Some(position) = reste.find("feature") {
            reste = &reste[position + "feature".len()..];
            let Some(apres) = reste.trim_start().strip_prefix('=') else {
                continue;
            };
            let Some(apres) = apres.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some((nom, _)) = apres.split_once('"') else {
                continue;
            };
            trouvees.insert(nom.to_string());
        }
    }
    trouvees
}

// ---------------------------------------------------------------------------
// Lecture des workflows.
// ---------------------------------------------------------------------------

/// Une étape de workflow qui lance `cargo test`.
struct Etape {
    origine: String,
    nom: String,
    mots: Vec<String>,
    variables: BTreeSet<String>,
}

impl Etape {
    fn designation(&self) -> String {
        format!("{} : {}", self.origine, self.nom)
    }

    fn paquets(&self) -> Vec<&str> {
        let mut trouves = Vec::new();
        for (i, mot) in self.mots.iter().enumerate() {
            if (mot == "-p" || mot == "--package")
                && let Some(suivant) = self.mots.get(i + 1)
            {
                trouves.push(suivant.as_str());
            }
        }
        trouves
    }

    fn tout_le_workspace(&self) -> bool {
        self.mots.iter().any(|m| m == "--workspace" || m == "--all")
    }

    fn cibles_nommees(&self) -> Vec<&str> {
        let mut trouvees = Vec::new();
        for (i, mot) in self.mots.iter().enumerate() {
            if mot == "--test"
                && let Some(suivant) = self.mots.get(i + 1)
            {
                trouvees.push(suivant.as_str());
            }
        }
        trouvees
    }

    /// Les filtres positionnels : tout mot qui n'est ni une option, ni la
    /// valeur d'une option, avant le `--` qui passe la main au harnais.
    fn filtres(&self) -> Vec<&str> {
        let Some(depart) = self.mots.iter().position(|m| m == "test") else {
            return Vec::new();
        };
        let mut trouves = Vec::new();
        let mut attend_valeur = false;
        for mot in self.mots.iter().skip(depart + 1) {
            if mot == "--" {
                break;
            }
            if attend_valeur {
                attend_valeur = false;
                continue;
            }
            if mot.starts_with('-') {
                if OPTIONS_A_VALEUR.contains(&mot.as_str()) {
                    attend_valeur = true;
                }
                continue;
            }
            trouves.push(mot.as_str());
        }
        trouves
    }

    fn fonctionnalites_nommees(&self) -> BTreeSet<String> {
        let mut trouvees = BTreeSet::new();
        for (i, mot) in self.mots.iter().enumerate() {
            let liste = if mot == "--features" {
                self.mots.get(i + 1).map(String::as_str)
            } else {
                mot.strip_prefix("--features=")
            };
            let Some(liste) = liste else { continue };
            for f in liste.split(',').filter(|f| !f.is_empty()) {
                trouvees.insert(f.to_string());
            }
        }
        trouvees
    }

    fn sans_defauts(&self) -> bool {
        self.mots.iter().any(|m| m == "--no-default-features")
    }
}

/// Les étapes `cargo test` d'un workflow.
///
/// Analyse par indentation : le dépôt n'a pas de dépendance YAML et n'a aucune
/// raison d'en prendre une pour une porte (même choix que `workflows_bornes.rs`).
fn etapes_cargo_test(origine: &str, source: &str) -> Vec<Etape> {
    let lignes: Vec<&str> = source.lines().collect();
    let mut trouvees = Vec::new();
    let mut i = 0;
    while i < lignes.len() {
        let ligne = lignes[i];
        let Some(apres) = ligne.trim_start().strip_prefix("- ") else {
            i += 1;
            continue;
        };
        let marge = indentation(ligne);
        let mut bloc: Vec<String> = vec![apres.to_string()];
        let mut j = i + 1;
        while j < lignes.len() {
            let suivante = lignes[j];
            if suivante.trim().is_empty() {
                bloc.push(String::new());
                j += 1;
                continue;
            }
            if indentation(suivante) <= marge {
                break;
            }
            bloc.push(suivante[marge + 2..].to_string());
            j += 1;
        }

        if bloc.iter().any(|l| l.contains("cargo test")) {
            let nom = bloc
                .iter()
                .find_map(|l| l.strip_prefix("name:"))
                .map(|n| n.trim().to_string())
                .unwrap_or_default();
            let mut variables = BTreeSet::new();
            let mut dans_env = false;
            for l in &bloc {
                if l.trim_end() == "env:" && indentation(l) == 0 {
                    dans_env = true;
                    continue;
                }
                if dans_env {
                    if indentation(l) == 0 && !l.trim().is_empty() {
                        dans_env = false;
                    } else if let Some((clef, _)) = l.split_once(':') {
                        variables.insert(clef.trim().to_string());
                    }
                }
            }
            for l in &bloc {
                let Some(position) = l.find("cargo test") else {
                    continue;
                };
                trouvees.push(Etape {
                    origine: origine.to_string(),
                    nom: if nom.is_empty() {
                        l[position..].trim().to_string()
                    } else {
                        nom.clone()
                    },
                    mots: l[position..]
                        .split_whitespace()
                        .map(str::to_string)
                        .collect(),
                    variables: variables.clone(),
                });
            }
        }
        i = j;
    }
    trouvees
}

/// Les fonctionnalités par défaut d'une caisse, résolues de proche en proche.
fn defauts(racine: &Path, chemin_caisse: &str) -> BTreeSet<String> {
    let source = lire(&racine.join(chemin_caisse).join("Cargo.toml"));
    let mut table: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut dans_features = false;
    for ligne in source.lines() {
        let t = ligne.trim();
        if t.starts_with('[') {
            dans_features = t == "[features]";
            continue;
        }
        if !dans_features || t.starts_with('#') {
            continue;
        }
        let Some((clef, valeur)) = t.split_once('=') else {
            continue;
        };
        let valeur = valeur.trim();
        let Some(valeur) = valeur.strip_prefix('[') else {
            continue;
        };
        let valeur = valeur.trim_end_matches(']');
        table.insert(
            clef.trim().to_string(),
            valeur
                .split(',')
                .map(|v| v.trim().trim_matches('"').to_string())
                .filter(|v| !v.is_empty())
                .collect(),
        );
    }
    let mut actives = BTreeSet::new();
    let mut pile: Vec<String> = table.get("default").cloned().unwrap_or_default();
    while let Some(f) = pile.pop() {
        if f.contains('/') || f.starts_with("dep:") || !actives.insert(f.clone()) {
            continue;
        }
        if let Some(suite) = table.get(&f) {
            pile.extend(suite.iter().cloned());
        }
    }
    actives
}

// ---------------------------------------------------------------------------
// Le témoin, et son exécution.
// ---------------------------------------------------------------------------

/// UN témoin = UNE fonction d'essai. La granularité compte : `cargo test`
/// filtre par nom, et cinq filtres nommés suffisent à faire tourner quatre
/// épreuves d'un fichier qui en porte dix-neuf. Regrouper par fichier
/// rendrait « exécuté » un fichier dont quinze épreuves dorment.
struct Temoin {
    paquet: String,
    caisse: String,
    fichier: String,
    variable: String,
    /// `None` = essai unitaire de la bibliothèque.
    cible: Option<String>,
    module: String,
    fonction: String,
    fonctionnalites: BTreeSet<String>,
}

impl Temoin {
    /// Le nom complet tel que `cargo test` le filtre.
    fn nom_complet(&self) -> String {
        if self.module.is_empty() {
            self.fonction.clone()
        } else {
            format!("{}::{}", self.module, self.fonction)
        }
    }
}

/// Cette étape exécute-t-elle ce témoin AVEC sa variable posée ?
fn execute(etape: &Etape, temoin: &Temoin, defauts: &BTreeSet<String>) -> bool {
    if !etape.variables.contains(&temoin.variable) {
        return false;
    }
    if !etape.tout_le_workspace() && !etape.paquets().contains(&temoin.paquet.as_str()) {
        return false;
    }
    let mut actives = etape.fonctionnalites_nommees();
    if !etape.sans_defauts() {
        actives.extend(defauts.iter().cloned());
    }
    if !temoin.fonctionnalites.is_subset(&actives) {
        return false;
    }
    match &temoin.cible {
        // Essai unitaire : tout sélecteur de cible l'exclut.
        None => {
            if etape
                .mots
                .iter()
                .any(|m| SELECTEURS_HORS_LIB.contains(&m.as_str()))
            {
                return false;
            }
        }
        Some(cible) => {
            if etape.mots.iter().any(|m| m == "--lib") {
                return false;
            }
            let nommees = etape.cibles_nommees();
            if !nommees.is_empty() && !nommees.contains(&cible.as_str()) {
                return false;
            }
        }
    }
    let filtres = etape.filtres();
    if filtres.is_empty() {
        return true;
    }
    let complet = temoin.nom_complet();
    filtres.iter().any(|f| complet.contains(f))
}

/// Tous les témoins du workspace : ceux des arbres `tests/` (atteints par le
/// compilateur) et ceux des `src/` (essais unitaires).
fn temoins(racine: &Path, membres: &[(String, String)]) -> Vec<Temoin> {
    let mut trouves = Vec::new();
    for (caisse, paquet) in membres {
        // 1. Les arbres `tests/`.
        if racine.join(caisse).join("tests").is_dir() {
            for (chemin, (cible, module)) in atteints(racine, caisse) {
                let fichier = racine.join(caisse).join(&chemin);
                if !fichier.is_file() {
                    continue;
                }
                let source = lire(&fichier);
                if !source.contains(MARQUEUR_LECTURE) && !source.contains(MARQUEUR_PG_OR_SKIP) {
                    continue;
                }
                let aides = aides(&source);
                let fonctionnalites = fonctionnalites_de_tete(&source);
                for (nom, corps) in fonctions_de_test(&source) {
                    let Some(variable) = variable_du_temoin(&corps, &aides) else {
                        continue;
                    };
                    trouves.push(Temoin {
                        paquet: paquet.clone(),
                        caisse: caisse.clone(),
                        fichier: format!("{caisse}/{chemin}"),
                        variable,
                        cible: Some(cible.clone()),
                        module: module.clone(),
                        fonction: nom,
                        fonctionnalites: fonctionnalites.clone(),
                    });
                }
            }
        }
        // 2. Les `src/`, essais unitaires de la bibliothèque.
        let dossier = racine.join(caisse).join("src");
        if !dossier.is_dir() {
            continue;
        }
        for fichier in fichiers_rust(&dossier) {
            let source = lire(&fichier);
            if !source.contains(MARQUEUR_LECTURE) && !source.contains(MARQUEUR_PG_OR_SKIP) {
                continue;
            }
            let relatif = fichier
                .strip_prefix(&dossier)
                .expect("chemin sous src/")
                .to_string_lossy()
                .replace('\\', "/");
            let mut module = relatif.trim_end_matches(".rs").replace('/', "::");
            for suffixe in ["::mod", "mod"] {
                if module.ends_with(suffixe) {
                    module = module[..module.len() - suffixe.len()].to_string();
                    break;
                }
            }
            let aides = aides(&source);
            let fonctionnalites = fonctionnalites_de_tete(&source);
            for (nom, corps) in fonctions_de_test(&source) {
                let Some(variable) = variable_du_temoin(&corps, &aides) else {
                    continue;
                };
                trouves.push(Temoin {
                    paquet: paquet.clone(),
                    caisse: caisse.clone(),
                    fichier: format!("{caisse}/src/{relatif}"),
                    variable,
                    cible: None,
                    module: module.clone(),
                    fonction: nom,
                    fonctionnalites: fonctionnalites.clone(),
                });
            }
        }
    }
    trouves.sort_by(|a, b| {
        (&a.fichier, &a.variable, &a.fonction).cmp(&(&b.fichier, &b.variable, &b.fonction))
    });
    trouves
}

fn fichiers_rust(dossier: &Path) -> Vec<PathBuf> {
    let mut trouves = Vec::new();
    let mut pile = vec![dossier.to_path_buf()];
    while let Some(courant) = pile.pop() {
        let Ok(entrees) = fs::read_dir(&courant) else {
            continue;
        };
        for entree in entrees.filter_map(Result::ok) {
            let chemin = entree.path();
            if chemin.is_dir() {
                pile.push(chemin);
            } else if chemin.extension().and_then(|e| e.to_str()) == Some("rs") {
                trouves.push(chemin);
            }
        }
    }
    trouves.sort();
    trouves
}

fn toutes_les_etapes(racine: &Path) -> Vec<Etape> {
    let dossier = racine.join(".github/workflows");
    let mut fichiers: Vec<PathBuf> = fs::read_dir(&dossier)
        .expect("dossier des workflows illisible")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|c| c.extension().and_then(|e| e.to_str()) == Some("yml"))
        .collect();
    fichiers.sort();
    let mut trouvees = Vec::new();
    for chemin in fichiers {
        let origine = chemin
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        trouvees.extend(etapes_cargo_test(&origine, &lire(&chemin)));
    }
    trouvees
}

// ---------------------------------------------------------------------------
// Les deux portes.
// ---------------------------------------------------------------------------

/// Aucun fichier d'essais du workspace n'échappe au compilateur — et toute
/// caisse qui coupe `autotests` emporte son propre refus.
///
/// ⚠️ Sabotage qui doit le faire tomber : retirer une entrée `[[test]]` de
/// `tune-server/Cargo.toml` ou de `tune-core/Cargo.toml`. Le fichier orphelin
/// est alors NOMMÉ dans le message.
#[test]
fn aucun_fichier_d_essais_du_workspace_n_echappe_au_compilateur() {
    let racine = racine();
    let membres = membres(&racine);

    // Contre-épreuve du lecteur de membres, sens NÉGATIF : les deux
    // applications Tauri sont `exclude` du workspace. Un lecteur qui listerait
    // les dossiers les ferait apparaître.
    for absent in ["tune-desktop", "tune-widget"] {
        assert!(
            !membres.iter().any(|(chemin, _)| chemin == absent),
            "lecteur de membres cassé : `{absent}` est `exclude` du workspace"
        );
    }
    // Contre-épreuve, sens POSITIF : `tune-output-api` n'est PAS dans
    // `members` — il entre par la dépendance `path` de `tune-core`. Le rater,
    // c'est rouvrir le trou de #3266.
    assert!(
        membres.iter().any(|(_, nom)| nom == "tune-output-api"),
        "lecteur de membres cassé : `tune-output-api` est membre IMPLICITE et \
         doit être vu — membres résolus : {membres:?}"
    );

    let mut avec_essais = 0usize;
    let mut fichiers_vus = 0usize;
    let mut orphelins: Vec<String> = Vec::new();
    let mut sans_refus: Vec<String> = Vec::new();

    for (caisse, _) in &membres {
        let dossier = racine.join(caisse).join("tests");
        if !dossier.is_dir() {
            continue;
        }
        avec_essais += 1;
        let table = atteints(&racine, caisse);
        let poses = fichiers_poses(&dossier);
        fichiers_vus += poses.len();
        for pose in &poses {
            if !table.contains_key(pose) {
                orphelins.push(format!("{caisse}/{pose}"));
            }
        }
        // La garde des gardes : une caisse qui coupe `autotests` doit emporter
        // son propre refus, sinon la protection meurt avec la prochaine caisse
        // qu'on créera sur ce modèle.
        let manifeste = lire(&racine.join(caisse).join("Cargo.toml"));
        if autotests_desactives(&manifeste) && !table.contains_key("tests/tests_orphelins.rs") {
            sans_refus.push(caisse.clone());
        }
    }

    assert!(
        avec_essais >= 2,
        "seulement {avec_essais} caisse(s) avec un dossier tests/ : le balayage \
         ne voit plus ce qu'il doit lire"
    );
    assert!(
        fichiers_vus >= 100,
        "seulement {fichiers_vus} fichier(s) d'essais vu(s) dans le workspace — \
         un garde qui ne trouve rien doit ÉCHOUER, pas passer à vide"
    );
    assert!(
        orphelins.is_empty(),
        "ces fichiers de tests/ ne sont compilés par personne, donc ne tournent \
         JAMAIS : {orphelins:?}\n\
         Deux issues : les déclarer en cible `[[test]]` du manifeste de leur \
         caisse, ou les tirer par un `#[path = \"…\"] mod …;` d'un agrégateur \
         déjà atteint."
    );
    assert!(
        sans_refus.is_empty(),
        "ces caisses coupent `autotests` sans emporter leur propre refus \
         (`tests/tests_orphelins.rs`) : {sans_refus:?}\n\
         Sous `autotests = false`, un fichier posé dans tests/ n'est compilé \
         par rien. Sans le refus dans la caisse, personne ne le verra."
    );
}

/// Tout témoin qui ne s'exécute que sous une variable d'environnement doit être
/// exécuté par une étape qui la POSE — ou recensé dans `SAUTS_CONNUS`.
///
/// Un essai qui saute s'affiche VERT. C'est le mode de panne le plus silencieux
/// du dépôt : les sept témoins PostgreSQL de #3519/#3520 sautaient depuis le
/// 03/09/2026 sans que rien ne le dise. Cette garde ne les rattache pas — elle
/// les rend VISIBLES, et refuse qu'un nouveau saut s'installe sans qu'on l'ait
/// décidé.
///
/// ⚠️ Sabotage qui doit le faire tomber : retirer `TUNE_TEST_PG_URL` du bloc
/// `env:` d'une étape de `test-postgres.yml`. Les témoins de cette étape
/// deviennent sautés, ne figurent dans aucune entrée, et sont NOMMÉS.
#[test]
fn tout_temoin_sous_variable_d_environnement_est_recense() {
    let racine = racine();
    let membres = membres(&racine);
    let temoins = temoins(&racine, &membres);
    let etapes = toutes_les_etapes(&racine);

    // Contre-épreuves du détecteur : il doit voir, et ne pas voir n'importe quoi.
    assert!(
        temoins.len() >= 30,
        "seulement {} témoin(s) sous variable relevé(s) : le détecteur ne voit \
         plus l'idiome du saut, et cette garde ne garde plus rien",
        temoins.len()
    );
    assert!(
        etapes.len() >= 10,
        "seulement {} étape(s) `cargo test` repérée(s) dans .github/workflows : \
         le lecteur de workflows est cassé",
        etapes.len()
    );
    assert!(
        etapes.iter().filter(|e| !e.variables.is_empty()).count() >= 5,
        "aucune étape ne pose de variable : l'extracteur du bloc `env:` est \
         cassé, et TOUT paraîtrait sauté"
    );
    // Sens NÉGATIF : une fonction qui MESURE une variable n'est pas un témoin
    // qui saute. Confondre les deux ferait crier la garde sur un innocent.
    assert!(
        !temoins
            .iter()
            .any(|t| t.fonction == "scan_io_concurrency_env_override"),
        "détecteur cassé : `scan_io_concurrency_env_override` LIT la variable \
         pour la mesurer, il ne saute pas sans elle"
    );

    let mut defauts_par_caisse: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (caisse, _) in &membres {
        defauts_par_caisse.insert(caisse.clone(), defauts(&racine, caisse));
    }

    // Regroupés par `(fichier, variable)` : c'est la maille de SAUTS_CONNUS,
    // et c'est celle qui se relit.
    let mut sautes: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut executes: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for temoin in &temoins {
        let defauts = defauts_par_caisse
            .get(&temoin.caisse)
            .expect("défauts de la caisse calculés");
        let porteuse = etapes
            .iter()
            .find(|e| execute(e, temoin, defauts))
            .map(Etape::designation);
        let clef = (temoin.fichier.clone(), temoin.variable.clone());
        match porteuse {
            Some(_) => executes
                .entry(clef)
                .or_default()
                .push(temoin.fonction.clone()),
            None => sautes
                .entry(clef)
                .or_default()
                .push(temoin.fonction.clone()),
        }
    }

    // 1. Un saut non recensé est une DÉRIVE.
    //
    // Ce verdict passe AVANT la contre-épreuve de portée : quand une étape perd
    // son bloc `env:`, c'est le nom des témoins qu'elle endort qu'il faut lire,
    // pas un diagnostic d'outillage.
    let mut non_recenses: Vec<String> = Vec::new();
    for ((fichier, variable), fonctions) in &sautes {
        let connu = SAUTS_CONNUS
            .iter()
            .find(|(f, v, _, _)| f == fichier && v == variable);
        match connu {
            None => non_recenses.push(format!("{fichier} [{variable}] {fonctions:?}")),
            Some((_, _, attendus, _)) => {
                let attendus: BTreeSet<&str> = attendus.iter().copied().collect();
                let vus: BTreeSet<&str> = fonctions.iter().map(String::as_str).collect();
                let neufs: Vec<&&str> = vus.difference(&attendus).collect();
                if !neufs.is_empty() {
                    non_recenses.push(format!("{fichier} [{variable}] témoins neufs {neufs:?}"));
                }
            }
        }
    }
    assert!(
        non_recenses.is_empty(),
        "ces témoins ne s'exécutent que sous une variable d'environnement \
         qu'AUCUNE étape ne pose, et ne figurent pas dans SAUTS_CONNUS : \
         {non_recenses:?}\n\
         Un essai qui saute s'affiche VERT : il ne peut ni passer ni échouer. \
         Deux issues :\n\
           1. poser la variable dans le bloc `env:` d'une étape `cargo test` \
         qui atteigne le témoin (paquet, cible, filtre, fonctionnalités) ;\n\
           2. l'inscrire dans SAUTS_CONNUS avec la RAISON du saut."
    );

    // 2. Une entrée périmée ment autant qu'un saut caché.
    let mut perimees: Vec<String> = Vec::new();
    for (fichier, variable, attendus, _) in SAUTS_CONNUS {
        let Some(fonctions) = sautes.get(&((*fichier).to_string(), (*variable).to_string())) else {
            perimees.push(format!(
                "{fichier} [{variable}] n'est plus sauté — retire son entrée de SAUTS_CONNUS"
            ));
            continue;
        };
        let vus: BTreeSet<&str> = fonctions.iter().map(String::as_str).collect();
        let disparus: Vec<&&str> = attendus.iter().filter(|a| !vus.contains(*a)).collect();
        if !disparus.is_empty() {
            perimees.push(format!(
                "{fichier} [{variable}] ces témoins ne sautent plus (ou n'existent plus) : {disparus:?}"
            ));
        }
    }
    assert!(
        perimees.is_empty(),
        "SAUTS_CONNUS ne décrit plus la réalité : {perimees:?}\n\
         Une liste de tolérances qui vieillit en silence est un garde-fou \
         MORT — c'est exactement la dérive que #2816 demande de détecter."
    );

    // 3. Contre-épreuve du calcul de portée, sens POSITIF.
    //
    // Sans elle, un calcul qui rendrait TOUJOURS « sauté » passerait à vide le
    // jour où SAUTS_CONNUS couvrirait tout le monde. Elle vient EN DERNIER :
    // une étape qui perd son `env:` doit d'abord se lire dans le verdict 1,
    // qui nomme les témoins endormis.
    assert!(
        !executes.is_empty(),
        "aucun témoin n'est vu comme exécuté : soit une étape a perdu son bloc \
         `env:`, soit le calcul de portée est cassé. `test-postgres.yml` en \
         porte plusieurs, dont `pg_routes_serveur` (#3123)."
    );
}

/// Les `required-features` de chaque cible `[[test]]` : `nom -> jeu exigé`.
///
/// Un fichier peut être gardé SANS porter le moindre `#![cfg]` : il suffit que
/// sa cible exige une fonctionnalité. `plugin_wasm_contracts.rs` est dans ce
/// cas — rien dans le fichier ne le dit, seul le manifeste le sait. Une garde
/// qui ne lirait que les `#![cfg]` le raterait.
fn fonctionnalites_requises(source: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut trouvees: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut dans_bloc = false;
    let mut nom: Option<String> = None;
    let mut exigees: BTreeSet<String> = BTreeSet::new();
    let mut ferme = |nom: &mut Option<String>, exigees: &mut BTreeSet<String>| {
        if let Some(n) = nom.take()
            && !exigees.is_empty()
        {
            trouvees.insert(n, std::mem::take(exigees));
        }
        exigees.clear();
    };
    for ligne in source.lines() {
        let t = ligne.trim();
        if t.starts_with('[') {
            ferme(&mut nom, &mut exigees);
            dans_bloc = t == "[[test]]";
            continue;
        }
        if !dans_bloc || t.starts_with('#') {
            continue;
        }
        if let Some(reste) = t.strip_prefix("name")
            && let Some(reste) = reste.trim_start().strip_prefix('=')
        {
            nom = Some(reste.trim().trim_matches('"').to_string());
        } else if let Some(reste) = t.strip_prefix("required-features")
            && let Some(reste) = reste.trim_start().strip_prefix('=')
            && let Some(reste) = reste.trim().strip_prefix('[')
        {
            exigees = reste
                .trim_end_matches(']')
                .split(',')
                .map(|f| f.trim().trim_matches('"').to_string())
                .filter(|f| !f.is_empty())
                .collect();
        }
    }
    ferme(&mut nom, &mut exigees);
    trouvees
}

/// Cette étape COMPILE-t-elle cette cible, avec ce jeu de fonctionnalités ?
///
/// La question n'est pas « exécute-t-elle ce témoin » — c'est celle de
/// `execute`, une maille plus fine, réservée aux témoins sous variable
/// d'environnement. Ici on demande seulement au compilateur de PASSER sur le
/// fichier : un fichier que nulle étape ne compile ne peut rien prouver, quel
/// que soit le nom de ses fonctions.
fn compile(
    etape: &Etape,
    paquet: &str,
    cible: &str,
    exigees: &BTreeSet<String>,
    par_defaut: &BTreeSet<String>,
) -> bool {
    if !etape.tout_le_workspace() && !etape.paquets().contains(&paquet) {
        return false;
    }
    let mut actives = etape.fonctionnalites_nommees();
    if !etape.sans_defauts() {
        actives.extend(par_defaut.iter().cloned());
    }
    if !exigees.is_subset(&actives) {
        return false;
    }
    // `--test x` ne construit QUE la cible `x` ; `--lib` n'en construit aucune.
    if etape.mots.iter().any(|m| m == "--lib") {
        return false;
    }
    let nommees = etape.cibles_nommees();
    nommees.is_empty() || nommees.contains(&cible)
}

// ---------------------------------------------------------------------------
// Les fichiers gardés par un jeu de fonctionnalités que NULLE porte n'active.
//
// Même maille que `SAUTS_CONNUS`, autre axe : là c'était la variable
// d'environnement, ici c'est la fonctionnalité. Une entrée = un fichier, le
// jeu qui le garde, et la raison pour laquelle aucune étape ne l'active.
//
// Mesure du 09/09/2026 sur `batch/bugs-9` (764bb053) : DOUZE fichiers gardés
// par un jeu de fonctionnalités, DOUZE compilés par au moins une étape, zéro
// sans porte. La liste est donc VIDE — et la garde ci-dessous refuse qu'elle
// grossisse sans qu'on l'écrive.
// ---------------------------------------------------------------------------
/// `(caisse/chemin, raison)`.
const SANS_PORTE_CONNUE: &[(&str, &str)] = &[];

/// Tout fichier d'essais gardé par un JEU DE FONCTIONNALITÉS est compilé par
/// au moins une étape qui l'active.
///
/// C'est le troisième mode de mort silencieuse d'un garde-fou, et le seul que
/// ce fichier ne fermait pas encore. Les deux autres sont au-dessus : le
/// fichier que personne n'enregistre (`autotests = false`), le témoin que nulle
/// étape ne réveille (variable d'environnement). Celui-ci est plus discret que
/// les deux : le fichier EST enregistré, il porte une cible, la garde des
/// orphelins le voit — et `#![cfg(feature = "dj")]` le vide de sa substance
/// dès que l'étape cesse de nommer `dj`. Le compilateur produit alors un
/// binaire d'essai à ZÉRO test, `cargo` affiche `ok`, et personne ne relit la
/// ligne.
///
/// Ce n'est pas une hypothèse : c'est #1427. Neuf essais de greffons ont dormi
/// depuis la 0.9.61 parce que la CI validait `--features oaat` quand les
/// binaires publiés en embarquaient six. Le dépôt en a tiré le job
/// `test-shipped-features` — et UNE garde, `le_job_test_de_la_ci_active_bandcamp`
/// (`workflows_bornes.rs`), qui nomme DEUX fonctionnalités à la main :
/// `bandcamp`, et `karaoke` au titre de sa contre-épreuve. `concerts`,
/// `plugins-wasm`, `dj` et `postgres` ne sont nommés par AUCUNE garde
/// d'exécution : les retirer de cette ligne ne fait rougir personne. Cette
/// porte-ci compte au lieu de nommer.
///
/// Elle est la jumelle, du côté de l'EXÉCUTION, de
/// `toute_feature_declaree_est_activee_par_une_porte_clippy` (#2865) : cette
/// dernière tient l'axe des lints, celle-ci celui du compilateur d'essais.
///
/// ⚠️ Sabotage qui doit le faire tomber : retirer `concerts,` de la ligne
/// `cargo test` du job `test-shipped-features` (`ci.yml`). `concerts_plugin.rs`
/// est alors NOMMÉ dans le message. `concerts` est choisi exprès : aucune autre
/// garde du dépôt ne le nomme, la contre-épreuve mesure donc ce que CETTE
/// porte-ci ajoute, et rien d'autre.
#[test]
fn tout_fichier_d_essais_derriere_un_jeu_de_fonctionnalites_est_compile_par_une_porte() {
    let racine = racine();
    let membres = membres(&racine);
    let etapes = toutes_les_etapes(&racine);
    assert!(
        !etapes.is_empty(),
        "aucune étape `cargo test` lue dans .github/workflows — le lecteur \
         d'étapes est cassé, et cette garde passerait à vide"
    );

    let mut gardes = 0usize;
    let mut sans_porte: Vec<(String, String)> = Vec::new();
    let mut couverts: BTreeSet<String> = BTreeSet::new();

    for (caisse, paquet) in &membres {
        let dossier = racine.join(caisse).join("tests");
        if !dossier.is_dir() {
            continue;
        }
        let manifeste = lire(&racine.join(caisse).join("Cargo.toml"));
        let requises = fonctionnalites_requises(&manifeste);
        let par_defaut = defauts(&racine, caisse);
        for (chemin, (cible, _)) in atteints(&racine, caisse) {
            let fichier = racine.join(caisse).join(&chemin);
            if !fichier.is_file() {
                continue;
            }
            // Le jeu qui garde le fichier, des DEUX côtés : ce que le fichier
            // dit de lui-même, et ce que le manifeste exige de sa cible.
            let mut exigees = fonctionnalites_de_tete(&lire(&fichier));
            if let Some(du_manifeste) = requises.get(&cible) {
                exigees.extend(du_manifeste.iter().cloned());
            }
            if exigees.is_empty() {
                continue;
            }
            gardes += 1;
            let designation = format!("{caisse}/{chemin}");
            let porteuse = etapes
                .iter()
                .any(|e| compile(e, paquet, &cible, &exigees, &par_defaut));
            if porteuse {
                couverts.insert(designation);
            } else {
                sans_porte.push((
                    designation,
                    exigees.into_iter().collect::<Vec<_>>().join(","),
                ));
            }
        }
    }

    // Plancher : une garde qui ne trouve rien à garder doit ÉCHOUER, pas
    // passer à vide. Douze fichiers gardés au 09/09/2026 ; en voir moins de
    // huit veut dire que la lecture des `#![cfg]` ou des `required-features`
    // s'est cassée, pas que le dépôt a rangé ses greffons.
    assert!(
        gardes >= 8,
        "seulement {gardes} fichier(s) d'essais gardé(s) par un jeu de \
         fonctionnalités : la lecture des `#![cfg(feature)]` ou des \
         `required-features` ne voit plus ce qu'elle doit lire"
    );

    let inattendus: Vec<&(String, String)> = sans_porte
        .iter()
        .filter(|(chemin, _)| !SANS_PORTE_CONNUE.iter().any(|(c, _)| c == chemin))
        .collect();
    assert!(
        inattendus.is_empty(),
        "ces fichiers d'essais sont gardés par un jeu de fonctionnalités que \
         NULLE étape `cargo test` n'active — ils compilent à VIDE et leurs \
         essais ne tournent nulle part : {inattendus:?}\n\
         Deux issues : ajouter la fonctionnalité à une étape de \
         `.github/workflows/`, ou inscrire le fichier dans `SANS_PORTE_CONNUE` \
         avec la RAISON."
    );

    // Une tolérance qui vieillit en silence est un garde-fou mort : une entrée
    // devenue couverte doit rougir pour qu'on la RETIRE, pas dormir.
    let perimees: Vec<&str> = SANS_PORTE_CONNUE
        .iter()
        .filter(|(chemin, _)| couverts.contains(*chemin))
        .map(|(chemin, _)| *chemin)
        .collect();
    assert!(
        perimees.is_empty(),
        "SANS_PORTE_CONNUE ne décrit plus la réalité : {perimees:?} sont \
         désormais compilés par une étape. Retire leur entrée."
    );

    // Contre-épreuve du calcul, sens POSITIF : un `compile` qui rendrait
    // TOUJOURS faux remplirait `sans_porte` et se lirait dans le verdict
    // ci-dessus ; un `compile` qui rendrait TOUJOURS vrai passerait ici à vide
    // sans que rien ne le dise. Cette assertion-là le dit.
    assert!(
        !couverts.is_empty(),
        "aucun fichier gardé n'est vu comme compilé : le calcul de portée est \
         cassé. `ci.yml` en porte plusieurs, dont le job \
         `test-shipped-features` qui active `dj,karaoke,bandcamp,concerts`."
    );
}
