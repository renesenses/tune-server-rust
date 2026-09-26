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
//! doit se voir dans une relecture. Il porte une raison d'au moins quinze
//! caractères : un marqueur nu rougit.
//!
//! # Le code LIVRÉ (#4770)
//!
//! Le même mécanisme de lecture garde aussi le code qui part dans le binaire.
//! Là, le défaut n'est pas la fuite mais le **nom fixe** : `/tmp/tune-convert`
//! créé par un compte de la machine de compilation, et plus aucun autre compte
//! n'y écrivait. La garde refuse dans le code livré tout `temp_dir()` et tout
//! littéral `/tmp`, et renvoie vers `tune_core::chemins_de_travail::
//! racine_de_travail` — un dossier par compte, sous `TMPDIR`. Un usage sûr par
//! construction (nom porteur de l'UID, nom aléatoire, racine seulement lue) se
//! justifie par le même marqueur.
//!
//! Hors de sa portée : les scripts d'image `image/build-*.sh`
//! (`WORK_DIR=/tmp/tune-os-build`), qui tournent en root et ne sont pas du Rust.

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

/// Longueur minimale d'une justification. Une raison, pas un mot : `ok`,
/// `voulu` ou `sûr` ne disent pas à la relecture POURQUOI le chemin ne fuit
/// pas, ni à qui il appartient.
const JUSTIFICATION_MIN: usize = 15;

#[derive(Debug, PartialEq)]
enum Exemption {
    /// Aucun marqueur sur la ligne ni sur celle qui précède.
    Aucune,
    /// Marqueur suivi d'une raison.
    Justifiee,
    /// Marqueur nu : refusé. Sans raison, l'exemption n'est plus relisible —
    /// c'est un interrupteur qu'on pose pour faire taire la garde.
    SansRaison,
}

/// Le marqueur vaut sur la ligne même, ou sur celle qui précède — la place
/// que rustfmt lui laisse au-dessus d'une instruction longue.
fn exemption(lignes: &[&str], n: usize, marqueur: &str) -> Exemption {
    let candidates = [Some(lignes[n - 1]), (n >= 2).then(|| lignes[n - 2])];
    for l in candidates.into_iter().flatten() {
        if let Some(i) = l.find(marqueur) {
            let raison = l[i + marqueur.len()..].trim();
            return if raison.chars().count() >= JUSTIFICATION_MIN {
                Exemption::Justifiee
            } else {
                Exemption::SansRaison
            };
        }
    }
    Exemption::Aucune
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

/// Un attribut ouvre-t-il une région de test ?
///
/// `#[cfg(test)]`, et aussi `#[cfg(all(test, …))]` — un témoin propre à une
/// plateforme (`all(test, target_os = "macos")`) est du code de test comme un
/// autre. `any(test, …)`, lui, se compile aussi hors test : c'est du code
/// livré, et il le reste.
fn ouvre_une_region_de_test(ligne: &str) -> bool {
    let t = ligne.trim();
    t == "#[cfg(test)]" || t.starts_with("#[cfg(all(test,")
}

/// Les lignes (1-indexées) qui appartiennent à du code de test.
///
/// Une région commence à un attribut de test et finit à la première ligne
/// dont l'indentation est la même et dont le contenu est `}` (ou `};`) — ce
/// que rustfmt garantit pour la fermeture de l'élément qui suit l'attribut.
/// Compter les accolades serait plus fin et bien plus fragile : les chaînes de
/// format du dépôt en portent partout (`format!("{nom}-{}")`).
///
/// ⚠️ Un élément tenu sur UNE ligne — `mod témoin;`, `use …;`, `fn f() {}` —
/// n'a pas de fermeture à lui. La version précédente cherchait quand même la
/// prochaine `}` de même indentation, et classait « test » tout ce qui suivait
/// jusqu'à elle : derrière un `#[cfg(test)] mod x;` de premier niveau, c'est
/// du code LIVRÉ qui échappait à la relecture.
fn lignes_de_test(source: &str, entier: bool) -> Vec<usize> {
    let lignes: Vec<&str> = source.lines().collect();
    if entier {
        return (1..=lignes.len()).collect();
    }
    let mut dedans = Vec::new();
    let mut i = 0;
    while i < lignes.len() {
        if !ouvre_une_region_de_test(lignes[i]) {
            i += 1;
            continue;
        }
        let indent = lignes[i].len() - lignes[i].trim_start().len();
        let fermeture = format!("{}{}", " ".repeat(indent), '}');
        let fermeture_pv = format!("{fermeture};");
        // Les attributs et la doc qui suivent appartiennent à l'élément.
        let mut j = i + 1;
        while j < lignes.len() {
            let t = lignes[j].trim_start();
            if t.starts_with("#[") || t.starts_with("//") {
                dedans.push(j + 1);
                j += 1;
            } else {
                break;
            }
        }
        if j < lignes.len() {
            let t = lignes[j].trim_end();
            if t.ends_with(';') || t.ends_with('}') {
                dedans.push(j + 1);
                i = j + 1;
                continue;
            }
        }
        while j < lignes.len() && lignes[j] != fermeture && lignes[j] != fermeture_pv {
            dedans.push(j + 1);
            j += 1;
        }
        i = j + 1;
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

/// La racine du dépôt et tous ses fichiers `.rs` gardés, avec les deux
/// planchers de parcours. Partagé par les deux gardes de ce fichier : le code
/// de test et le code livré se lisent par le MÊME mécanisme, donc un défaut
/// de découverte les aveugle ensemble — et les fait rougir ensemble.
fn fichiers_du_depot() -> Depot {
    let manifeste = Path::new(env!("CARGO_MANIFEST_DIR"));
    let racine = manifeste
        .parent()
        .expect("tune-core a un parent")
        .to_path_buf();
    let racine = racine.as_path();

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
    let montes = montes_en_test(&fichiers);
    Depot {
        racine: racine.to_path_buf(),
        fichiers,
        montes,
    }
}

struct Depot {
    racine: PathBuf,
    fichiers: Vec<PathBuf>,
    /// Fichiers et dossiers de modules montés par un `#[cfg(test)] mod …;`.
    montes: Vec<PathBuf>,
}

impl Depot {
    /// Le fichier est-il du code de test d'un bout à l'autre ?
    fn entier(&self, chemin: &Path) -> bool {
        fichier_entierement_de_test(chemin) || self.montes.iter().any(|m| chemin.starts_with(m))
    }
}

/// Les fichiers qu'un `#[cfg(test)] mod nom;` monte : ils ne portent aucun
/// attribut de test en propre, et seule la déclaration du parent dit qu'ils
/// n'existent pas dans le binaire livré.
///
/// Rend, pour chaque déclaration, `nom.rs` et le dossier `nom/` (sous-modules
/// compris) à côté du parent — ou le chemin d'un `#[path = "…"]`. Sans elle,
/// la garde du code livré accuserait des témoins (`orchestrator/
/// refus_amont_4366.rs`), et la garde du code de test ne les lirait pas.
fn montes_en_test(fichiers: &[PathBuf]) -> Vec<PathBuf> {
    let mut montes = Vec::new();
    for chemin in fichiers {
        let Ok(source) = std::fs::read_to_string(chemin) else {
            continue;
        };
        let Some(dossier) = chemin.parent() else {
            continue;
        };
        let nom_fichier = chemin.file_name().unwrap_or_default().to_string_lossy();
        let racine_de_module = ["mod.rs", "lib.rs", "main.rs"].contains(&nom_fichier.as_ref());
        let dossier_des_modules = if racine_de_module {
            dossier.to_path_buf()
        } else {
            dossier.join(chemin.file_stem().unwrap_or_default())
        };
        let lignes: Vec<&str> = source.lines().collect();
        for (i, l) in lignes.iter().enumerate() {
            if !ouvre_une_region_de_test(l) {
                continue;
            }
            let mut chemin_force = None;
            let mut j = i + 1;
            while j < lignes.len() {
                let t = lignes[j].trim();
                if let Some(p) = t.strip_prefix("#[path = \"") {
                    chemin_force = p.strip_suffix("\"]").map(str::to_string);
                } else if !(t.starts_with("#[") || t.starts_with("//")) {
                    break;
                }
                j += 1;
            }
            let Some(item) = lignes.get(j).map(|l| l.trim()) else {
                continue;
            };
            let item = item
                .strip_prefix("pub(crate) ")
                .or_else(|| item.strip_prefix("pub "))
                .unwrap_or(item);
            let Some(nom) = item.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')) else {
                continue;
            };
            match chemin_force {
                Some(p) => montes.push(dossier.join(p)),
                None => {
                    montes.push(dossier_des_modules.join(format!("{nom}.rs")));
                    montes.push(dossier_des_modules.join(nom));
                }
            }
        }
    }
    montes
}

#[test]
fn aucun_chemin_temporaire_compose_a_la_main_dans_du_code_de_test() {
    let depot = fichiers_du_depot();
    let racine = depot.racine.as_path();
    let fichiers = &depot.fichiers;
    let motifs = motifs();
    let renoncements = renoncements();
    let marqueur = marqueur();

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
    for chemin in fichiers {
        // Le module qui FOURNIT la sortie autorisée compose forcément le
        // chemin lui-même : c'est son travail.
        if chemin.ends_with("test_scratch.rs") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(chemin) else {
            continue;
        };
        let lignes: Vec<&str> = source.lines().collect();
        for n in lignes_de_test(&source, depot.entier(chemin)) {
            let ligne = lignes[n - 1];
            if ligne.trim_start().starts_with("//") {
                continue;
            }
            let relatif = chemin.strip_prefix(racine).unwrap_or(chemin);
            match exemption(&lignes, n, &marqueur) {
                Exemption::Justifiee => continue,
                Exemption::SansRaison => {
                    fautes.push(format!(
                        "{}:{n} — marqueur `{marqueur}` sans justification : dire POURQUOI \
                         ce chemin ne fuit pas",
                        relatif.display()
                    ));
                    continue;
                }
                Exemption::Aucune => {}
            }
            if appelle_la_sortie_autorisee(ligne) {
                appels_autorises += 1;
            }
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

// ---------------------------------------------------------------------------
// Le code LIVRÉ (#4770)
// ---------------------------------------------------------------------------

/// Les gestes refusés dans le code livré : une racine temporaire prise telle
/// quelle, ou le littéral `/tmp`. En morceaux, comme [`motifs`], pour que ce
/// fichier ne se signale pas lui-même.
///
/// Le défaut de #4770 n'est pas le dossier temporaire, c'est son **nom
/// fixe** : `/tmp/tune-convert` créé par le compte `jp` sur la machine de
/// compilation, en `775`, et plus aucun autre compte n'y écrivait. Or un nom
/// fixe ne se reconnaît pas à la syntaxe — `format!("tune-{}", x)` est fixe
/// ou non selon ce qu'est `x`. La garde refuse donc le GESTE qui le permet,
/// et renvoie vers le module qui ne le permet pas.
fn motifs_livres() -> Vec<(String, &'static str)> {
    vec![
        (
            format!("temp{}dir()", '_'),
            "racine temporaire prise telle quelle dans du code livré",
        ),
        // Passée en valeur : `.unwrap_or_else(std::env::temp_dir)`.
        (
            format!("::temp{}dir)", '_'),
            "racine temporaire prise telle quelle dans du code livré",
        ),
        (
            format!("{}/tmp{}", '"', '"'),
            "`/tmp` littéral dans du code livré : ignore `TMPDIR` et se partage entre comptes",
        ),
        (
            format!("{}/tmp/", '"'),
            "chemin sous `/tmp` littéral dans du code livré : nom fixe, partagé entre comptes",
        ),
    ]
}

/// Les modules qui FOURNISSENT la sortie autorisée : ils composent la racine
/// eux-mêmes, c'est leur travail.
fn fournit_la_sortie(chemin: &Path) -> bool {
    chemin.ends_with("tune-core/src/chemins_de_travail.rs")
        || chemin.ends_with("tune-core/src/test_scratch.rs")
}

/// Ce que la garde du code livré trouve dans UN fichier.
///
/// Fonction pure, séparée de la découverte : c'est elle que la contre-épreuve
/// ci-dessous nourrit d'une source fabriquée, pour prouver qu'un chemin fixe
/// réintroduit rougit en nommant son fichier et sa ligne.
struct Constat {
    fautes: Vec<String>,
    /// Lignes de code livré effectivement relues (plancher de lecture).
    lignes_livrees: usize,
    /// Appels à `racine_de_travail` relus dans du code livré.
    appels_a_la_sortie: usize,
}

fn constat_du_code_livre(relatif: &str, chemin: &Path, source: &str, entier: bool) -> Constat {
    let marqueur = marqueur();
    let motifs = motifs_livres();
    let lignes: Vec<&str> = source.lines().collect();
    let mut constat = Constat {
        fautes: Vec::new(),
        lignes_livrees: 0,
        appels_a_la_sortie: 0,
    };
    if entier || fournit_la_sortie(chemin) {
        return constat;
    }
    let de_test: std::collections::HashSet<usize> =
        lignes_de_test(source, false).into_iter().collect();
    for n in 1..=lignes.len() {
        if de_test.contains(&n) {
            continue;
        }
        let ligne = lignes[n - 1];
        // Les attributs `#[cfg(test)]` eux-mêmes et les commentaires, doc
        // comprise : on garde ce qui s'exécute.
        if ligne.trim_start().starts_with("//") {
            continue;
        }
        constat.lignes_livrees += 1;
        if ligne.contains("racine_de_travail") {
            constat.appels_a_la_sortie += 1;
        }
        let Some((_, raison)) = motifs.iter().find(|(m, _)| ligne.contains(m.as_str())) else {
            continue;
        };
        match exemption(&lignes, n, &marqueur) {
            Exemption::Justifiee => {}
            Exemption::SansRaison => constat.fautes.push(format!(
                "{relatif}:{n} — marqueur `{marqueur}` sans justification (au moins \
                 {JUSTIFICATION_MIN} caractères) : dire pourquoi ce chemin ne se partage pas"
            )),
            Exemption::Aucune => constat.fautes.push(format!("{relatif}:{n} — {raison}")),
        }
    }
    constat
}

#[test]
fn aucun_dossier_temporaire_au_nom_fixe_dans_le_code_livre() {
    let depot = fichiers_du_depot();
    let (racine, fichiers) = (&depot.racine, &depot.fichiers);
    let mut fautes = Vec::new();
    let mut lignes_livrees = 0usize;
    let mut appels_a_la_sortie = 0usize;
    for chemin in fichiers {
        let Ok(source) = std::fs::read_to_string(chemin) else {
            continue;
        };
        let relatif = chemin
            .strip_prefix(racine)
            .unwrap_or(chemin)
            .display()
            .to_string();
        let c = constat_du_code_livre(&relatif, chemin, &source, depot.entier(chemin));
        fautes.extend(c.fautes);
        lignes_livrees += c.lignes_livrees;
        appels_a_la_sortie += c.appels_a_la_sortie;
    }

    // Deux planchers, pour la même raison que la garde du code de test : une
    // garde qui ne lit plus rien répond vert. Le premier dit que le code livré
    // est bien relu (le dépôt en compte des centaines de milliers de lignes),
    // le second qu'il est relu AU BON ENDROIT — là où la sortie autorisée est
    // appelée.
    assert!(
        lignes_livrees > 100_000,
        "la garde n'a relu que {lignes_livrees} ligne(s) de code livré : la détection \
         des régions de test ou la découverte des caisses est cassée"
    );
    assert!(
        appels_a_la_sortie >= 5,
        "la garde n'a vu que {appels_a_la_sortie} appel(s) à `racine_de_travail` dans du \
         code livré : `chemins_de_travail` a été renommé, ou la garde ne lit plus les \
         fichiers qui l'appellent. Réparer AVANT de baisser ce plancher."
    );

    assert!(
        fautes.is_empty(),
        "{} dossier(s) temporaire(s) sans racine par compte dans du code livré (#4770). \
         Un nom fixe sous `/tmp` est créé par le premier compte venu, et plus aucun autre \
         n'y écrit — c'est le `/tmp/tune-convert` de `jp` qui a fait rougir le \
         convertisseur à chaque campagne. Passer par \
         `tune_core::chemins_de_travail::racine_de_travail(\"<étiquette>\")`, un dossier \
         par compte. Si le chemin est sûr PAR CONSTRUCTION (nom porteur de l'UID, nom \
         aléatoire de `tempfile`, chemin seulement LU), le dire sur la ligne ou celle du \
         dessus : `// tmp-autorise: <raison>`. Sites :\n  {}",
        fautes.len(),
        fautes.join("\n  ")
    );
}

/// Contre-épreuve de la garde du code livré, sans toucher au dépôt : un
/// chemin fixe réintroduit rougit, en nommant le fichier et la ligne ; la
/// même ligne dans une région de test, commentée, ou justifiée ne rougit pas ;
/// un marqueur nu rougit.
#[test]
fn la_garde_du_code_livre_nomme_le_fichier_et_la_ligne() {
    let chemin = Path::new("tune-core/src/exemple_4770.rs");
    let racine_tmp = format!("std::env::temp{}dir()", '_');
    let litteral = format!("{}/tmp/tune-exemple{}", '"', '"');
    let m = marqueur();
    let source = [
        "pub fn cache() -> std::path::PathBuf {".to_string(),
        format!("    {racine_tmp}.join(\"tune-exemple\")"),
        "}".to_string(),
        format!("const RACINE: &str = {litteral};"),
        format!("// {m} nom porteur de l'UID, borné au compte courant"),
        format!("fn a() {{ let _ = {racine_tmp}; }}"),
        format!("fn b() {{ let _ = {racine_tmp}; }} // {m} ok"),
        format!("// doc : {racine_tmp} n'est pas du code"),
        "#[cfg(test)]".to_string(),
        "mod tests {".to_string(),
        format!("    fn c() {{ let _ = {racine_tmp}; }}"),
        "}".to_string(),
    ]
    .join("\n");

    let c = constat_du_code_livre("tune-core/src/exemple_4770.rs", chemin, &source, false);
    assert_eq!(c.fautes.len(), 3, "fautes : {:#?}", c.fautes);
    assert!(
        c.fautes[0].starts_with("tune-core/src/exemple_4770.rs:2 — racine temporaire"),
        "{}",
        c.fautes[0]
    );
    assert!(
        c.fautes[1].starts_with("tune-core/src/exemple_4770.rs:4 — `/tmp` littéral")
            || c.fautes[1].starts_with("tune-core/src/exemple_4770.rs:4 — chemin sous"),
        "{}",
        c.fautes[1]
    );
    assert!(
        c.fautes[2].starts_with("tune-core/src/exemple_4770.rs:7 — marqueur")
            && c.fautes[2].contains("sans justification"),
        "{}",
        c.fautes[2]
    );

    // Un fichier sous `tests/` n'est pas du code livré : c'est l'autre garde
    // qui le tient.
    let de_test = constat_du_code_livre(
        "tune-core/tests/exemple.rs",
        Path::new("tune-core/tests/exemple.rs"),
        &source,
        fichier_entierement_de_test(Path::new("tune-core/tests/exemple.rs")),
    );
    assert!(de_test.fautes.is_empty());

    // Un `#[cfg(test)] mod témoin;` d'une seule ligne ne fait pas une région :
    // le code livré qui le suit reste relu. La première détection de région
    // l'avalait jusqu'à la prochaine `}` de premier niveau.
    let apres_un_mod = [
        "#[cfg(test)]".to_string(),
        "mod temoin;".to_string(),
        "pub fn f() -> std::path::PathBuf {".to_string(),
        format!("    {racine_tmp}.join(\"tune-fixe\")"),
        "}".to_string(),
    ]
    .join("\n");
    let c = constat_du_code_livre("tune-core/src/apres_mod.rs", chemin, &apres_un_mod, false);
    assert_eq!(
        c.fautes,
        vec!["tune-core/src/apres_mod.rs:4 — racine temporaire prise telle quelle dans du code livré"
            .to_string()]
    );
}
