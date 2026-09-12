//! 🔴 #3266 — la PORTEE des lignes `cargo test` de la CI, gardee par le
//! workflow lui-meme.
//!
//! ## Ce que ce fichier garde, et pourquoi il vient en plus des gardes de
//! `workflows_bornes.rs`
//!
//! `tout_membre_du_workspace_est_execute_par_une_porte_cargo_test` repond a
//! « ce membre est-il nomme quelque part ». Il a laisse passer `tune-output-api`
//! pendant tout le temps ou il ne lisait que la liste `members` — et il l'a
//! laisse passer EN VERT, ce qui est pire qu'une absence de garde. Deux angles
//! restaient decouverts une fois ce trou-la rebouche :
//!
//! 1. **Une caisse qui retombe hors de la LIGNE de test** alors que la porte
//!    `clippy` la garde encore. Les deux portes s'arment sur la meme condition
//!    (`needs.impact.outputs.rust == 'true'`, jobs `test` et `clippy` de
//!    `ci.yml`) : elles tournent sur les memes PR, donc toute caisse lintee sur
//!    une PR doit y etre EXECUTEE. `clippy` compile sans executer ; une caisse
//!    presente d'un cote seulement rend un vert qui ne prouve rien de son
//!    comportement. C'etait exactement l'etat de `tune-output-api` au
//!    06/09/2026 : quinze `-p` cote clippy, quatorze cote test.
//! 2. **Un JEU de fonctionnalites qui retombe hors des lignes de test.** Dix
//!    fichiers de `tune-server/tests/` vivent derriere un
//!    `#![cfg(feature = "…")]` : sans la fonctionnalite, ils ne sont pas rouges,
//!    ils sont VIDES — le fichier compile a zero test et la porte reste verte.
//!    C'est la lecon de #1427 (neuf essais de greffons invisibles depuis la
//!    0.9.61), de #2865 et de #2702/#2778.
//!
//! ## `include_str!` plutot qu'une lecture au chemin courant
//!
//! Modele des gardes PostgreSQL #3519/#3520 : un workflow renomme ou deplace
//! devient une erreur de COMPILATION, pas un test qui se saute ou qui panique
//! sur un chemin. Le contenu garde est fige a la compilation du binaire de test,
//! donc il est toujours celui de l'arbre qu'on est en train d'eprouver.

/// Le workflow principal : il porte le job `test` (la seule porte `cargo test`
/// sans condition `full`) et la SEULE ligne `cargo clippy` du depot.
const CI: &str = include_str!("../../.github/workflows/ci.yml");

/// La suite PostgreSQL. Elle porte les seules portes `cargo test` qui activent
/// `postgres`, et trois fichiers d'essais de `tune-server` en dependent.
const POSTGRES: &str = include_str!("../../.github/workflows/test-postgres.yml");

/// Les mots d'une commande `- run:` si la ligne en est une, et si la commande
/// commence par `prefixe`.
fn commande<'a>(ligne: &'a str, prefixe: &str) -> Option<Vec<&'a str>> {
    let nue = ligne.trim();
    if nue.starts_with('#') {
        return None;
    }
    let commande = nue
        .strip_prefix("- run: ")
        .or_else(|| nue.strip_prefix("run: "))?;
    if commande != prefixe && !commande.starts_with(&format!("{prefixe} ")) {
        return None;
    }
    Some(commande.split_whitespace().collect())
}

/// Les paquets qu'une commande deja decoupee designe par `-p` / `--package`.
fn paquets(mots: &[&str]) -> Vec<String> {
    let mut vus: Vec<String> = Vec::new();
    let mut suite = mots.iter();
    while let Some(mot) = suite.next() {
        let nom = match *mot {
            "-p" | "--package" => suite.next().map(|m| (*m).to_string()),
            autre => ["-p=", "--package="]
                .iter()
                .find_map(|p| autre.strip_prefix(p))
                .map(str::to_string),
        };
        if let Some(nom) = nom
            && !vus.contains(&nom)
        {
            vus.push(nom);
        }
    }
    vus
}

/// Les fonctionnalites qu'une commande deja decoupee active par `--features`.
/// `--features a,b` comme `--features=a,b`, et les repetitions s'additionnent —
/// c'est ainsi que cargo les lit.
fn fonctionnalites(mots: &[&str]) -> Vec<String> {
    let mut vues: Vec<String> = Vec::new();
    let mut suite = mots.iter();
    let ajouter = |brut: &str, vues: &mut Vec<String>| {
        for f in brut.split(',') {
            let f = f.trim();
            if !f.is_empty() && !vues.iter().any(|v| v == f) {
                vues.push(f.to_string());
            }
        }
    };
    while let Some(mot) = suite.next() {
        if *mot == "--features" || *mot == "-F" {
            if let Some(valeur) = suite.next() {
                ajouter(valeur, &mut vues);
            }
        } else if let Some(valeur) = mot
            .strip_prefix("--features=")
            .or_else(|| mot.strip_prefix("-F="))
        {
            ajouter(valeur, &mut vues);
        }
    }
    vues
}

/// Le corps d'un job de premier niveau, bornes comprises : de sa ligne
/// `  <nom>:` jusqu'au prochain job de meme indentation.
fn job(source: &str, nom: &str) -> String {
    let entete = format!("  {nom}:");
    let mut dedans = false;
    let mut corps = String::new();
    for ligne in source.lines() {
        if !dedans {
            if ligne.trim_end() == entete {
                dedans = true;
            }
            continue;
        }
        // Un job frere commence a la meme indentation de deux espaces.
        let suivant = ligne.starts_with("  ")
            && !ligne.starts_with("   ")
            && ligne.trim_end().ends_with(':')
            && !ligne.trim_start().starts_with('#');
        if suivant {
            break;
        }
        corps.push_str(ligne);
        corps.push('\n');
    }
    assert!(dedans, "job `{nom}` introuvable dans le workflow");
    corps
}

/// 🔴 #3266 — le job `test` EXECUTE exactement les caisses que le job `clippy`
/// LINTE.
///
/// Les deux jobs de `ci.yml` s'arment sur la meme condition et tournent donc sur
/// les memes PR. Un ecart entre les deux listes n'a que deux formes, et les deux
/// sont un defaut :
///
/// - une caisse lintee mais jamais executee — `clippy` compile, il n'execute
///   rien : c'etait `tune-output-api` et ses sept essais jusqu'au 06/09/2026 ;
/// - une caisse executee mais jamais lintee — la porte `-D clippy::correctness`
///   ne lit alors aucune de ses cibles.
///
/// Le test ne cite AUCUN nom de caisse : il compare deux listes lues dans le
/// workflow. Une caisse ajoutee au workspace demain tombera donc du bon cote
/// toute seule, ou fera rougir cette garde.
///
/// ⚠️ Sabotage qui doit le faire tomber : retirer un `-p` de la ligne
/// `cargo test` du job `test` de `ci.yml`, ou en ajouter un a la seule ligne
/// `cargo clippy`.
#[test]
fn le_job_test_execute_exactement_les_caisses_que_clippy_linte() {
    let corps_test = job(CI, "test");
    let corps_clippy = job(CI, "clippy");

    let mut executees: Vec<String> = Vec::new();
    let mut lignes_test = 0usize;
    for ligne in corps_test.lines() {
        if let Some(mots) = commande(ligne, "cargo test") {
            lignes_test += 1;
            assert!(
                !mots.contains(&"--workspace") && !mots.contains(&"--all"),
                "le job `test` passe `--workspace` : cette garde compare des \
                 listes de `-p` et ne saurait plus rien mesurer. Adapter la \
                 garde plutot que la laisser rassurer."
            );
            for paquet in paquets(&mots) {
                if !executees.contains(&paquet) {
                    executees.push(paquet);
                }
            }
        }
    }
    let mut lintees: Vec<String> = Vec::new();
    let mut lignes_clippy = 0usize;
    for ligne in corps_clippy.lines() {
        if let Some(mots) = commande(ligne, "cargo clippy") {
            lignes_clippy += 1;
            assert!(
                !mots.contains(&"--workspace"),
                "la porte `clippy` passe `--workspace` : cette garde compare des \
                 listes de `-p` et ne saurait plus rien mesurer."
            );
            for paquet in paquets(&mots) {
                if !lintees.contains(&paquet) {
                    lintees.push(paquet);
                }
            }
        }
    }

    // Contre-epreuves du detecteur : sans elles, un extracteur casse rendrait
    // deux listes vides, donc egales, et ce test passerait a vide.
    assert_eq!(
        lignes_test, 1,
        "le job `test` de ci.yml ne porte plus exactement UNE ligne \
         `cargo test` ({lignes_test}) — le detecteur ne voit plus ce qu'il lit"
    );
    assert_eq!(
        lignes_clippy, 1,
        "le job `clippy` de ci.yml ne porte plus exactement UNE ligne \
         `cargo clippy` ({lignes_clippy})"
    );
    assert!(
        executees.len() >= 15,
        "seulement {} caisse(s) relevee(s) sur la ligne `cargo test` : \
         {executees:?}",
        executees.len()
    );
    // Sens NEGATIF : l'extracteur ne doit pas prendre un drapeau pour un paquet.
    for intrus in ["--no-fail-fast", "--no-default-features", "--features"] {
        assert!(
            !executees.iter().any(|c| c == intrus),
            "extracteur casse : `{intrus}` compte comme un paquet — {executees:?}"
        );
    }

    let mut lintees_seulement: Vec<&String> =
        lintees.iter().filter(|c| !executees.contains(c)).collect();
    let mut executees_seulement: Vec<&String> =
        executees.iter().filter(|c| !lintees.contains(c)).collect();
    lintees_seulement.sort();
    executees_seulement.sort();

    assert!(
        lintees_seulement.is_empty(),
        "ces caisses sont LINTEES par le job `clippy` de ci.yml et jamais \
         EXECUTEES par son job `test` : {lintees_seulement:?}\n\
         `clippy` compile sans executer : leurs `#[test]` ne peuvent ni passer \
         ni echouer, et une garde de site qui y vivrait ne pourrait jamais \
         crier (#3266 — c'etait `tune-output-api`, sept essais).\n\
         Ajouter `-p <caisse>` a la ligne `cargo test` du job `test`.\n\
         Ligne `cargo test` aujourd'hui : {executees:?}"
    );
    assert!(
        executees_seulement.is_empty(),
        "ces caisses sont EXECUTEES par le job `test` et jamais LINTEES par le \
         job `clippy` : {executees_seulement:?}\n\
         La porte `-D clippy::correctness` ne lit alors aucune de leurs cibles.\n\
         Ajouter `-p <caisse>` a la ligne `cargo clippy` de ci.yml.\n\
         Ligne `cargo clippy` aujourd'hui : {lintees:?}"
    );
}

/// 🔴 #3266 — tout JEU de fonctionnalites qui garde un fichier d'essais de
/// `tune-server` est active par au moins une porte `cargo test` de la CI.
///
/// Un fichier sous `#![cfg(feature = "X")]` compile a ZERO test quand `X` est
/// absente. Il ne rougit pas : il disparait, et la porte reste verte. C'est ce
/// qui a rendu neuf essais de greffons invisibles depuis la 0.9.61 (#1427), et
/// ce que #2702/#2778 ont failli reintroduire quand `--no-default-features` a
/// retire `bandcamp` de la ligne du job `test`.
///
/// Le test COMPTE au lieu de citer : il balaie `tune-server/tests/` a la
/// recherche des `#![cfg(feature = "…")]` et exige, pour chacun, une ligne
/// `cargo test` qui nomme `-p tune-server` ET active la fonctionnalite. Un
/// nouveau fichier d'essais pose derriere une fonctionnalite qu'aucune porte
/// n'active fera rougir cette garde des sa PR.
///
/// ⚠️ Sabotage qui doit le faire tomber : retirer `bandcamp` du `--features` du
/// job `test`, ou `postgres` des etapes de `test-postgres.yml`.
#[test]
fn tout_jeu_qui_garde_un_fichier_d_essais_est_active_par_une_porte_cargo_test() {
    use std::fs;
    use std::path::Path;

    // Les fonctionnalites qui gardent au moins un fichier d'essais, avec les
    // fichiers qu'elles gardent — pour que le message nomme ce qu'on perd.
    let dossier = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut gardees: Vec<(String, Vec<String>)> = Vec::new();
    let mut fichiers_lus = 0usize;
    let mut entrees: Vec<_> = fs::read_dir(&dossier)
        .expect("tune-server/tests/ illisible")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .collect();
    entrees.sort();
    for chemin in &entrees {
        fichiers_lus += 1;
        let source = fs::read_to_string(chemin)
            .unwrap_or_else(|e| panic!("{} illisible : {e}", chemin.display()));
        let nom_fichier = chemin
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        for ligne in source.lines() {
            let t = ligne.trim();
            // `#![cfg(feature = "X")]` en tete de fichier : c'est la forme qui
            // vide le fichier entier. Un `#[cfg(...)]` sur une fonction ne
            // supprime qu'un test et reste visible dans le compte.
            let Some(reste) = t.strip_prefix("#![cfg(feature") else {
                continue;
            };
            let Some(reste) = reste.trim_start().strip_prefix('=') else {
                continue;
            };
            let Some(reste) = reste.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some((feature, _)) = reste.split_once('"') else {
                continue;
            };
            match gardees.iter_mut().find(|(f, _)| f == feature) {
                Some((_, fichiers)) => fichiers.push(nom_fichier.clone()),
                None => gardees.push((feature.to_string(), vec![nom_fichier.clone()])),
            }
        }
    }

    assert!(
        fichiers_lus >= 50,
        "seulement {fichiers_lus} fichier(s) lu(s) dans tune-server/tests/ — le \
         balayage ne voit plus ce qu'il doit lire"
    );
    assert!(
        gardees.len() >= 5,
        "seulement {} fonctionnalite(s) gardienne(s) relevee(s) : {gardees:?} — \
         le detecteur de `#![cfg(feature = \"…\")]` est casse. Un garde qui ne \
         trouve rien doit ECHOUER, pas passer a vide.",
        gardees.len()
    );

    // Les jeux actives par une porte `cargo test` qui nomme `-p tune-server`.
    // Une porte qui n'inclut pas la caisse n'active rien POUR ELLE, meme si son
    // `--features` cite le nom.
    let mut actives: Vec<String> = Vec::new();
    let mut portes = 0usize;
    for source in [CI, POSTGRES] {
        for ligne in source.lines() {
            let Some(mots) = commande(ligne, "cargo test") else {
                continue;
            };
            if !paquets(&mots).iter().any(|p| p == "tune-server") {
                continue;
            }
            portes += 1;
            // Sans `--no-default-features`, le `default` de tune-server entre
            // aussi — il porte `local-audio`, `oaat`, `cloud-relay`, `bandcamp`.
            if !mots.contains(&"--no-default-features") {
                for f in ["local-audio", "oaat", "cloud-relay", "bandcamp"] {
                    if !actives.iter().any(|a| a == f) {
                        actives.push(f.to_string());
                    }
                }
            }
            for f in fonctionnalites(&mots) {
                if !actives.iter().any(|a| *a == f) {
                    actives.push(f);
                }
            }
        }
    }

    assert!(
        portes >= 3,
        "seulement {portes} porte(s) `cargo test -p tune-server` reperee(s) — le \
         detecteur ne voit plus les lignes qu'il doit lire"
    );

    let nues: Vec<&(String, Vec<String>)> = gardees
        .iter()
        .filter(|(f, _)| !actives.iter().any(|a| a == f))
        .collect();
    assert!(
        nues.is_empty(),
        "ces jeux de fonctionnalites gardent des fichiers d'essais de \
         `tune-server` et ne sont actives par AUCUNE porte `cargo test` : \
         {nues:?}\n\
         Un fichier sous `#![cfg(feature = \"X\")]` sans `X` ne rougit pas : il \
         compile a ZERO test et la porte reste verte (#3266, meme famille que \
         #1427 et #2702/#2778).\n\
         Deux issues seulement :\n\
           1. ajouter la fonctionnalite au `--features` d'une porte \
         `cargo test` qui nomme `-p tune-server` ;\n\
           2. retirer le `#![cfg(feature = …)]` du fichier, s'il n'a plus lieu \
         d'etre.\n\
         Jeux actifs aujourd'hui : {actives:?}"
    );
}
