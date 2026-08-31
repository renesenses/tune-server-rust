//! Garde-fou #3030 : plus aucun chemin temporaire construit à la main dans du
//! code de test.
//!
//! # Ce qu'il a coûté d'attendre
//!
//! Mesuré sur la machine de compilation le 31/08/2026 : **3 204 entrées
//! `/tmp/tune-*` pour 1,2 Gio**, dont 636 nées dans la seule matinée et 2 569
//! vieilles de plus de 24 h sans un processus vivant derrière. Aucune fuite
//! n'était un défaut de mécanisme — c'était toujours le même geste, recopié :
//! `std::env::temp_dir().join(format!("tune-…-{}", process::id()))`, un
//! `create_dir_all`, et un `remove_dir_all` en fin de fonction que la panique
//! saute. Or c'est le test **qui échoue** qui laisse le plus de résidus.
//!
//! #2864 avait rendu ces noms uniques ; l'unicité n'est pas le nettoyage.
//! Chaque exécution ajoutait sa couche, et le ménage manuel était à refaire le
//! lendemain.
//!
//! # Pourquoi un garde de SOURCE et pas seulement un compte à l'exécution
//!
//! Un compteur de `/tmp` ne peut pas être un test fiable ici : plusieurs
//! agents travaillent sur la même machine et y écrivent en même temps, donc
//! un delta global mesure le voisin autant que soi. Les témoins d'exécution
//! vivent dans `tune-core/src/test_scratch.rs` — ils sont bornés à
//! l'étiquette et au pid, donc exacts. Celui-ci tient l'autre bord : il
//! refuse le **geste**, avant qu'il ne produise le résidu, et c'est lui qui
//! empêche le prochain test écrit sur le modèle des précédents.
//!
//! # La sortie autorisée
//!
//! `tune_core::test_scratch` : `scratch_dir` pour un dossier, `scratch_file`
//! pour un fichier, `scratch_dir_in` quand la racine doit être `/tmp`
//! littéral. Tous les trois se suppriment par `Drop`, panique comprise.
//!
//! Un cas légitime restant se marque par `// tmp-autorise: <raison>` sur la
//! ligne, ou sur celle qui précède. Le marqueur est délibérément laid : il
//! doit se voir dans une relecture.

use std::path::{Path, PathBuf};

/// Les gestes refusés. En morceaux pour que ce fichier-ci ne se signale pas
/// lui-même.
fn motifs() -> Vec<(String, &'static str)> {
    vec![
        (
            format!("temp{}dir()", '_'),
            "chemin temporaire composé à la main au lieu de passer par test_scratch",
        ),
        (
            format!("from({}/tmp{}){}join(", '"', '"', '.'),
            "sous-dossier de /tmp composé à la main",
        ),
        (
            format!("new({}/tmp{}){}join(", '"', '"', '.'),
            "sous-dossier de /tmp composé à la main",
        ),
    ]
}

/// Le marqueur d'exception, lui aussi en morceaux.
fn marqueur() -> String {
    format!("tmp{}autorise:", '-')
}

/// Un fichier est-il entièrement du code de test ?
///
/// Tout ce qui vit sous un `tests/` l'est. Sous `src/`, le sont aussi les
/// fichiers montés par un `#[cfg(test)] mod …;` d'un module voisin — ils ne
/// portent alors aucun `#[cfg(test)]` en propre, et la détection par région
/// ci-dessous les manquerait.
fn fichier_entierement_de_test(chemin: &Path) -> bool {
    let s = chemin.to_string_lossy().replace('\\', "/");
    if s.contains("/tests/") {
        return true;
    }
    let nom = chemin.file_name().unwrap_or_default().to_string_lossy();
    nom.ends_with("_test.rs") || nom.ends_with("_tests.rs")
}

/// Les lignes (1-indexées) qui appartiennent à du code de test.
///
/// Une région commence à un `#[cfg(test)]` et finit à la première ligne dont
/// l'indentation est la même et dont le contenu est `}` — ce que rustfmt
/// garantit pour la fermeture de l'élément qui suit l'attribut. Compter les
/// accolades serait plus fin et bien plus fragile : les chaînes de format du
/// dépôt en portent partout (`format!("{nom}-{}")`).
fn lignes_de_test(source: &str, chemin: &Path) -> Vec<usize> {
    let lignes: Vec<&str> = source.lines().collect();
    if fichier_entierement_de_test(chemin) {
        return (1..=lignes.len()).collect();
    }
    let mut dedans = Vec::new();
    let mut i = 0;
    while i < lignes.len() {
        if lignes[i].trim() == "#[cfg(test)]" {
            let indent = lignes[i].len() - lignes[i].trim_start().len();
            let fermeture = format!("{}{}", " ".repeat(indent), '}');
            let mut j = i + 1;
            while j < lignes.len() && lignes[j] != fermeture {
                dedans.push(j + 1);
                j += 1;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    dedans
}

fn parcourir(dir: &Path, fichiers: &mut Vec<PathBuf>) {
    let Ok(entrees) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entrees.flatten() {
        let p = e.path();
        if p.is_dir() {
            parcourir(&p, fichiers);
        } else if p.extension().is_some_and(|x| x == "rs") {
            fichiers.push(p);
        }
    }
}

#[test]
fn aucun_chemin_temporaire_compose_a_la_main_dans_du_code_de_test() {
    let manifeste = Path::new(env!("CARGO_MANIFEST_DIR"));
    let racine = manifeste.parent().expect("tune-core a un parent");
    let motifs = motifs();
    let marqueur = marqueur();

    let mut fichiers = Vec::new();
    for caisse in [
        "tune-core",
        "tune-server",
        "tune-cli",
        "tune-bridge",
        "tune-ffi",
        "tune-widget",
    ] {
        parcourir(&racine.join(caisse).join("src"), &mut fichiers);
        parcourir(&racine.join(caisse).join("tests"), &mut fichiers);
    }
    assert!(
        fichiers.len() > 200,
        "le parcours n'a vu que {} fichiers : la racine du dépôt a bougé et ce \
         garde ne garde plus rien",
        fichiers.len()
    );

    let mut fautes = Vec::new();
    for chemin in &fichiers {
        // Le module qui FOURNIT la sortie autorisée compose forcément le
        // chemin lui-même : c'est son travail.
        if chemin.ends_with("test_scratch.rs") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(chemin) else {
            continue;
        };
        let lignes: Vec<&str> = source.lines().collect();
        for n in lignes_de_test(&source, chemin) {
            let ligne = lignes[n - 1];
            if ligne.trim_start().starts_with("//") {
                continue;
            }
            if ligne.contains(&marqueur) || (n >= 2 && lignes[n - 2].contains(&marqueur)) {
                continue;
            }
            for (motif, raison) in &motifs {
                if ligne.contains(motif.as_str()) {
                    let relatif = chemin.strip_prefix(racine).unwrap_or(chemin);
                    fautes.push(format!("{}:{n} — {raison}", relatif.display()));
                    break;
                }
            }
        }
    }

    assert!(
        fautes.is_empty(),
        "{} chemin(s) temporaire(s) composé(s) à la main dans du code de test \
         (#3030). Ils survivent au test, et surtout au test qui échoue : c'est \
         ce geste qui a laissé 3 204 entrées dans /tmp. Passer par \
         `tune_core::test_scratch` — `scratch_dir`, `scratch_file`, ou \
         `scratch_dir_in(\"/tmp\", …)` quand la racine littérale est \
         nécessaire — qui nettoient par `Drop`. Sites :\n  {}",
        fautes.len(),
        fautes.join("\n  ")
    );
}
