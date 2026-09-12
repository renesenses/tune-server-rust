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
//! # Le second geste : un garde rangé dans un `static`
//!
//! Rust **ne détruit pas** les variables statiques à la fin du processus. Un
//! `TempDir` — ou un `ScratchDir` — placé dans un `static` ne nettoie donc
//! rien, quand bien même il porte le bon `Drop` : le destructeur n'est jamais
//! appelé. C'est l'autre fuite de #3030, et le recensement d'origine l'avait
//! manquée parce qu'il ne comptait que les entrées `tune-*` : le résidu porte
//! ici le préfixe anonyme de `tempfile`. Mesuré le 01/09/2026 sur la machine
//! de compilation : **149 dossiers `/tmp/.tmp*`**, tous porteurs des quatre
//! mêmes fichiers, donc tous nés du même `static`.
//!
//! Quand le dossier doit vraiment vivre plus longtemps que toute portée — une
//! variable d'environnement lue par tous les tests d'un binaire, par exemple —
//! la seule fin de vie qui reste est celle du processus : `libc::atexit`, et
//! le marqueur `tmp-autorise` pour que la relecture voie la reprise.
//!
//! # Le troisième geste : un garde DÉSARMÉ
//!
//! Les deux premiers se voient à l'œil : il manque un garde, ou il est mal
//! rangé. Le troisième est écrit noir sur blanc et ne se voyait pas —
//! `into_path()` et `keep()` rendent le chemin d'un `TempDir` **et lui
//! retirent son nettoyage**, `mem::forget` jette le garde sans le détruire,
//! `Box::leak` le fait vivre jusqu'à la fin du processus. Aucun de ces
//! quatre-là ne ressemble au motif du premier geste : la version précédente
//! de ce garde les laissait tous passer, alors qu'ils fuient *par
//! construction* — l'auteur n'a pas oublié de nettoyer, il a écrit qu'il ne
//! nettoierait pas.
//!
//! # Ce qu'un garde de source ne peut pas voir
//!
//! Une passe complète de la suite dans un `TMPDIR` privé — la seule mesure
//! qui ne compte pas le voisin — laissait encore **un** résidu au 01/09/2026 :
//! `tune-notify-icons`. Le geste n'était pas dans le test : il était dans le
//! code de PRODUCTION que le test appelle (`notifications::icon_cache_dir`,
//! un cache d'icônes légitime sous `temp_dir()`), et aucune relecture de
//! source du côté test ne pouvait le nommer. D'où la forme du correctif :
//! la fonction prend sa racine en paramètre, et le test lui donne un
//! `ScratchDir`. Retenir la limite : ce fichier garde le GESTE, pas le
//! RÉSIDU ; le résidu se mesure en faisant tourner la suite.
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
fn motifs() -> Vec<(String, String)> {
    vec![
        (
            format!("temp{}dir()", '_'),
            "chemin temporaire composé à la main au lieu de passer par test_scratch".to_string(),
        ),
        (
            format!("from({}/tmp{}){}join(", '"', '"', '.'),
            "sous-dossier de /tmp composé à la main".to_string(),
        ),
        (
            format!("new({}/tmp{}){}join(", '"', '"', '.'),
            "sous-dossier de /tmp composé à la main".to_string(),
        ),
    ]
}

/// Les gestes qui **désarment** un garde déjà en place.
///
/// L'autre moitié de #3030. Un `TempDir` correctement construit ne fuit pas
/// — jusqu'à ce qu'on lui retire son `Drop` : `into_path()` et `keep()`
/// rendent le chemin **et renoncent au nettoyage**, `mem::forget` jette le
/// garde sans le détruire. Le résultat est un dossier qui ne partira jamais,
/// et rien dans la ligne ne ressemble au geste banni par [`motifs`] : le
/// garde d'origine les laissait tous passer.
///
/// La différence avec [`motifs`] compte pour le lecteur d'un échec : là, on
/// n'a pas oublié de nettoyer, on a **écrit** qu'on ne nettoierait pas.
///
/// Les raisons sont composées elles aussi : écrire le geste en toutes
/// lettres dans le message ferait se signaler ce fichier-ci. La première
/// version l'a fait, et le garde s'est accusé lui-même dès sa première
/// exécution — l'aveu était dans son propre message d'erreur.
fn renoncements() -> Vec<(String, String)> {
    let chemin_pris = format!("into{}path()", '_');
    let conserve = format!("{}keep()", '.');
    let oubli = format!("mem::forget{}", '(');
    let renonce = format!("renoncer{}au{}nettoyage()", '_', '_');
    vec![
        (
            chemin_pris.clone(),
            format!(
                "`{chemin_pris}` rend le chemin et RETIRE le nettoyage : le dossier ne \
                 partira jamais"
            ),
        ),
        (
            conserve.clone(),
            format!("`{conserve}` retire le nettoyage du garde : le dossier ne partira jamais"),
        ),
        (
            oubli.clone(),
            format!("`{oubli}` jette le garde sans le détruire : son `Drop` ne s'exécutera pas"),
        ),
        (
            renonce.clone(),
            format!(
                "`{renonce}` : renoncement explicite au nettoyage — le justifier par le \
                 marqueur, ou ne pas y renoncer"
            ),
        ),
    ]
}

/// Les types dont le `Drop` EST le nettoyage. Les perdre de vue, c'est la
/// fuite ; les ranger dans un `static` ou les `Box::leak`, c'est la garantir.
const GARDES: [&str; 4] = ["TempDir", "ScratchDir", "ScratchFile", "NamedTempFile"];

fn cite_un_garde(texte: &str) -> bool {
    GARDES.iter().any(|g| texte.contains(g))
}

/// La ligne appelle-t-elle la sortie autorisée ? Sert au plancher de
/// lecture : un garde qui ne voit plus AUCUN appel légitime ne lit plus rien
/// du tout, et doit le dire.
fn appelle_la_sortie_autorisee(ligne: &str) -> bool {
    [
        "scratch_dir(",
        "scratch_dir_in(",
        "scratch_file(",
        "scratch_name(",
    ]
    .iter()
    .any(|a| ligne.contains(a))
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
///
/// `benches/` et `examples/` comptent pareil : ce n'est pas du code livré,
/// ça tourne sur la machine de compilation, et leur fuite s'y accumulerait
/// exactement de la même façon. Aucun n'en porte aujourd'hui — c'est
/// précisément le moment de fermer la porte, avant que le premier ne
/// recopie le geste d'à côté.
fn fichier_entierement_de_test(chemin: &Path) -> bool {
    let s = chemin.to_string_lossy().replace('\\', "/");
    if s.contains("/tests/") || s.contains("/benches/") || s.contains("/examples/") {
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

/// Les caisses à inspecter, **découvertes** et non recopiées.
///
/// Ce garde a d'abord porté une liste en dur de six noms, et il en couvrait
/// en réalité **cinq** : `tune-widget` y figurait, mais sa caisse vit sous
/// `tune-widget/src-tauri`, si bien que `tune-widget/src` n'existe pas et que
/// le parcours y rendait la main sans un mot.
///
/// Le dépôt compte quatorze caisses. Les manquantes — `tune-stream-http`,
/// `tune-streaming-http`, `tune-plugin-runtime-wasm`, `tune-output-api`,
/// `plugins/tune-karaoke`, `plugins/tune-bandcamp` — portent **88 tests** à
/// elles seules. Le geste banni s'y serait écrit sans un mot, et la prochaine
/// caisse ajoutée au dépôt aurait hérité du même angle mort : personne ne
/// pense à revenir éditer un garde le jour où il crée une caisse.
///
/// Chercher les `Cargo.toml` retire la question : une caisse neuve est gardée
/// le jour où elle naît. Les dossiers de construction et le code tiers
/// (`vendor/`) sont écartés — ce garde n'a pas à juger ce qu'il ne peut pas
/// corriger.
fn caisses(dir: &Path, trouvees: &mut Vec<PathBuf>) {
    const IGNORES: [&str; 6] = ["target", ".git", "node_modules", "web", "dist", "vendor"];
    let nom = dir.file_name().unwrap_or_default().to_string_lossy();
    if IGNORES.contains(&nom.as_ref()) {
        return;
    }
    if dir.join("Cargo.toml").is_file() && dir.join("src").is_dir() {
        trouvees.push(dir.to_path_buf());
    }
    let Ok(entrees) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entrees.flatten() {
        let p = e.path();
        if p.is_dir() && !p.is_symlink() {
            caisses(&p, trouvees);
        }
    }
}

#[test]
fn aucun_chemin_temporaire_compose_a_la_main_dans_du_code_de_test() {
    let manifeste = Path::new(env!("CARGO_MANIFEST_DIR"));
    let racine = manifeste.parent().expect("tune-core a un parent");
    let motifs = motifs();
    let renoncements = renoncements();
    let marqueur = marqueur();

    let mut trouvees = Vec::new();
    caisses(racine, &mut trouvees);
    assert!(
        trouvees.len() >= 14,
        "seulement {} caisse(s) découverte(s) sous {} : la racine du dépôt a \
         bougé et ce garde ne garde plus rien",
        trouvees.len(),
        racine.display()
    );

    let mut fichiers = Vec::new();
    for caisse in &trouvees {
        parcourir(&caisse.join("src"), &mut fichiers);
        parcourir(&caisse.join("tests"), &mut fichiers);
        parcourir(&caisse.join("benches"), &mut fichiers);
        parcourir(&caisse.join("examples"), &mut fichiers);
    }
    assert!(
        fichiers.len() > 200,
        "le parcours n'a vu que {} fichiers : la racine du dépôt a bougé et ce \
         garde ne garde plus rien",
        fichiers.len()
    );

    // Le troisième plancher, et le seul qui mesure la LECTURE plutôt que le
    // parcours. Compter les fichiers ne prouve rien : le garde d'origine en
    // voyait des centaines tout en n'ouvrant que cinq caisses sur quatorze,
    // et il répondait vert. Ce compte-ci porte sur les lignes réellement
    // classées « code de test » ET qui appellent la sortie autorisée : si la
    // détection de région se casse, ou si `test_scratch` est renommé sans
    // que ce fichier suive, le nombre s'effondre et le garde le DIT au lieu
    // de passer à vide.
    let mut appels_autorises = 0usize;
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
            if appelle_la_sortie_autorisee(ligne) {
                appels_autorises += 1;
            }
            let relatif = chemin.strip_prefix(racine).unwrap_or(chemin);
            // `Box::leak` sur un garde : la fuite est dans le nom. Elle n'est
            // refusée que là — le dépôt s'en sert légitimement ailleurs pour
            // faire vivre des `&[String]` le temps d'un test.
            //
            // Vient AVANT la règle du `static` : un `Box::leak` s'annote
            // presque toujours `&'static`, et la règle du `static` le
            // signalait alors sous le mauvais motif. La faute est la même,
            // le message ne l'était pas.
            if ligne.contains("Box::leak") && cite_un_garde(ligne) {
                fautes.push(format!(
                    "{}:{n} — garde de nettoyage passé à `Box::leak` : son `Drop` ne \
                     sera jamais appelé",
                    relatif.display()
                ));
                continue;
            }
            // Un garde de nettoyage rangé dans un `static` ne s'exécute
            // JAMAIS : Rust ne détruit pas les variables statiques à la fin
            // du processus. C'est la seconde fuite de #3030, celle que le
            // recensement d'origine n'avait pas vue parce qu'il ne comptait
            // que les entrées `tune-*` : `plugin_contracts.rs` gardait son
            // `TempDir` dans un `OnceLock` statique et laissait un
            // `/tmp/.tmpXXXXXX` par exécution — 149 mesurés le 01/09/2026.
            //
            // La déclaration est relue jusqu'à son `=` ou son `;` : rustfmt
            // coupe un `static` dont le type est long, et le garde d'origine,
            // qui ne regardait QUE la ligne du mot-clé, aurait alors laissé
            // passer exactement la fuite qu'il venait de fermer.
            if ligne.contains("static ") {
                let mut declaration = String::new();
                for l in lignes.iter().skip(n - 1).take(6) {
                    declaration.push_str(l);
                    if l.contains('=') || l.trim_end().ends_with(';') {
                        break;
                    }
                }
                if cite_un_garde(&declaration) {
                    fautes.push(format!(
                        "{}:{n} — garde de nettoyage rangé dans un `static` : son `Drop` \
                         ne sera jamais appelé",
                        relatif.display()
                    ));
                    continue;
                }
            }
            for (motif, raison) in motifs.iter().chain(renoncements.iter()) {
                if ligne.contains(motif.as_str()) {
                    fautes.push(format!("{}:{n} — {raison}", relatif.display()));
                    break;
                }
            }
        }
    }

    assert!(
        appels_autorises >= 40,
        "le garde n'a vu que {appels_autorises} appel(s) à `test_scratch` dans du code \
         de test, alors que le dépôt en compte une cinquantaine. Il ne lit donc plus \
         les fichiers qu'il est censé garder — c'est exactement ainsi que sa première \
         version répondait vert en n'ouvrant que cinq caisses sur quatorze. Réparer la \
         découverte AVANT de baisser ce plancher."
    );

    assert!(
        fautes.is_empty(),
        "{} fuite(s) de répertoire temporaire dans du code de test (#3030). Un chemin \
         composé à la main survit au test, et surtout au test qui ÉCHOUE — c'est ce \
         geste qui a laissé 3 204 entrées dans /tmp ; un garde désarmé \
         (`into_path`, `keep`, `mem::forget`, `static`, `Box::leak`) ne nettoiera, lui, \
         jamais. Passer par `tune_core::test_scratch` — `scratch_dir`, `scratch_file`, \
         ou `scratch_dir_in(\"/tmp\", …)` quand la racine littérale est nécessaire — \
         qui nettoient par `Drop`. Sites :\n  {}",
        fautes.len(),
        fautes.join("\n  ")
    );
}
