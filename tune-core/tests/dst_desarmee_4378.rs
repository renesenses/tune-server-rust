//! 🔴 #4378 — la fonctionnalité `dst` ne doit ENTRER dans aucun binaire publié.
//!
//! ## Pourquoi cette garde existe
//!
//! Le décodeur DST (DSD compressé des SACD) s'appuie sur le crate
//! `dst-decoder`. Sa licence n'est PAS tranchée : le paquet se déclare
//! Apache-2.0 (crates.io, GitHub, fichier `LICENSE`) mais son README reproduit
//! l'en-tête ISO/Philips du code de référence — « Copyright is not released
//! for non MPEG-4 Audio conforming products » — et un avertissement brevets.
//!
//! Décision de Bertrand du 20/09/2026 : le code part **désarmé**, derrière la
//! fonctionnalité de compilation `dst`, éteinte. L'allumer — l'ajouter à un
//! `default` ou à une ligne de build qui produit un artefact — revient à
//! trancher la question ISO/Philips. Ce n'est pas un geste de code : c'est un
//! arbitrage juridique, et il appartient à Bertrand.
//!
//! Une fonctionnalité éteinte ne reste pas éteinte toute seule. Il suffit d'un
//! `--features …,dst` ajouté à une cible pour qu'un binaire publié embarque le
//! crate, sans que rien ne rougisse. C'est exactement le mode de panne de
//! #3355, en miroir : là-bas une fonctionnalité disparaissait des binaires
//! livrés en silence, ici elle y entrerait en silence.
//!
//! ## Ce que la garde lit VRAIMENT
//!
//! Les fichiers LIVRÉS, par `include_str!` : un fichier renommé devient une
//! erreur de COMPILATION, pas un test qui se saute. Aucune liste de
//! fonctionnalités n'est recopiée ici — la garde n'aurait alors gardé que sa
//! propre définition.
//!
//! Trois formes de « ligne de build de publication » sont relevées :
//!
//! 1. la valeur `features:` d'une entrée de matrice (`release.yml`), que la
//!    ligne `cross build … ${{ matrix.features }}` consomme ;
//! 2. le `--features` d'une commande qui construit `tune-server`
//!    (`cargo build`, `cross build`, `cargo install`) ;
//! 3. ⚠️ la `base` déclarée dans le commentaire `# plugin-catalog: {…}`.
//!    `scripts/plugin-catalog.py --write` RÉÉCRIT la ligne `--features` (ou la
//!    ligne `features:`) à partir de cette base : la ligne est une SORTIE, la
//!    base est la source. Une garde qui ne lirait que la ligne serait effacée
//!    au premier `--write`.
//!
//! ## Portée volontairement étroite : ce qui PUBLIE, pas ce qui compile
//!
//! `ci.yml` est hors portée, et c'est délibéré : il ne produit aucun artefact
//! livré. C'est même là qu'il faudra allumer `dst` le jour où on voudra le
//! faire tourner en intégration sans rien publier. `test-postgres.yml`,
//! `plugin-sdk.yml`, `Dockerfile.bridge` (tune-bridge) n'ont pas non plus de
//! ligne de build de `tune-server`.
//!
//! Les images Tune OS (`image/build-*.sh`, `tune-os.yml`) et les scripts de
//! déploiement .15/.18 ne compilent RIEN : ils déposent une archive déjà
//! construite par `release.yml`. Ils sont donc couverts par ricochet — et le
//! balayage ci-dessous rougit si l'un d'eux se met à compiler.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Les recettes qui PRODUISENT un artefact livré. Le nom sert d'ancre dans les
/// messages d'échec ; `include_str!` ancre le chemin à la compilation.
const RECETTES_PUBLIEES: &[(&str, &str)] = &[
    (
        ".github/workflows/release.yml",
        include_str!("../../.github/workflows/release.yml"),
    ),
    (
        ".github/workflows/docker.yml",
        include_str!("../../.github/workflows/docker.yml"),
    ),
    ("Dockerfile", include_str!("../../Dockerfile")),
];

/// Combien de listes de fonctionnalités chaque recette doit AU MOINS rendre.
/// Mesuré le 20/09/2026 : release.yml 10 (2 entrées de matrice, 5 bases, 3
/// lignes `cargo build`), docker.yml 4 (2 bases, 2 lignes), Dockerfile 2.
/// Un extracteur cassé rend zéro et passerait sans ces planchers.
const PLANCHERS: &[(&str, usize)] = &[
    (".github/workflows/release.yml", 9),
    (".github/workflows/docker.yml", 4),
    ("Dockerfile", 2),
];

/// Les fichiers qui portent une ligne de build de `tune-server` SANS rien
/// publier. Le balayage les tolère ; tout autre fichier le fait rougir.
const RECETTES_SANS_PUBLICATION: &[&str] = &[".github/workflows/ci.yml"];

/// Les répertoires où vivent les recettes de ce dépôt, balayés pour qu'une
/// recette NOUVELLE ne passe pas sous le radar de la liste ci-dessus.
const REPERTOIRES_DE_RECETTES: &[&str] = &[".", ".github/workflows", "image", "scripts"];

/// Les manifestes qui déclarent la fonctionnalité `dst`.
const MANIFESTE_CORE: &str = include_str!("../Cargo.toml");
const MANIFESTE_SERVEUR: &str = include_str!("../../tune-server/Cargo.toml");

/// La racine du dépôt, déduite du manifeste de cette caisse.
fn racine() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tune-core doit avoir un parent — la racine du dépôt")
        .to_path_buf()
}

/// `dst` sous toutes ses graphies de ligne de commande : `dst`,
/// `tune-core/dst`, `tune-core?/dst`.
fn est_dst(feature: &str) -> bool {
    let f = feature.trim();
    f == "dst" || f.rsplit_once('/').is_some_and(|(_, nom)| nom == "dst")
}

/// Découpe une valeur `a,b,c` en fonctionnalités, sans les vides.
fn decouper(valeur: &str) -> Vec<String> {
    valeur
        .split(',')
        .map(|f| f.trim().trim_matches('"').to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// Replie les continuations `\` en une seule ligne logique : le `--features`
/// d'un `RUN` de Dockerfile ou d'un `cross build` de workflow tient souvent sur
/// la deuxième ligne physique.
fn lignes_logiques(source: &str) -> Vec<(usize, String)> {
    let mut sorties: Vec<(usize, String)> = Vec::new();
    let mut courante = String::new();
    let mut debut = 1usize;
    for (n, ligne) in source.lines().enumerate() {
        let t = ligne.trim();
        if courante.is_empty() {
            debut = n + 1;
        }
        if let Some(tete) = t.strip_suffix('\\') {
            courante.push_str(tete.trim_end());
            courante.push(' ');
            continue;
        }
        courante.push_str(t);
        sorties.push((debut, std::mem::take(&mut courante)));
    }
    if !courante.is_empty() {
        sorties.push((debut, courante));
    }
    sorties
}

/// La `base` d'un marqueur `# plugin-catalog: {"features":…,"base":[…]}`.
fn base_du_marqueur(json: &str) -> Option<Vec<String>> {
    let cle = json.find("\"base\"")?;
    let ouvre = cle + json[cle..].find('[')?;
    let ferme = ouvre + json[ouvre..].find(']')?;
    Some(decouper(&json[ouvre + 1..ferme]))
}

/// Recolle les interpolations `${{ … }}` d'un YAML en UN seul mot.
///
/// 🔴 Trou mesuré le 20/09/2026, sabotage n°4 de la contre-épreuve : la ligne
/// `--features ${{ matrix.features }},dst` se découpait en `--features`, `${{`,
/// `matrix.features`, `}},dst`. Le lecteur prenait `${{` pour la valeur du
/// drapeau et le `,dst` tombait dans un mot que personne ne regardait — la
/// garde passait au VERT sur le raccourci le plus évident pour allumer `dst`
/// sur les deux cibles ARM d'un coup. Sans interpolation, la ligne est rendue
/// telle quelle.
fn normaliser_interpolations(ligne: &str) -> String {
    let mut sortie = String::with_capacity(ligne.len());
    let mut reste = ligne;
    while let Some(debut) = reste.find("${{") {
        sortie.push_str(&reste[..debut]);
        let apres = &reste[debut..];
        let Some(fin) = apres.find("}}") else {
            sortie.push_str(apres);
            return sortie;
        };
        sortie.extend(apres[..fin + 2].chars().filter(|c| !c.is_whitespace()));
        reste = &apres[fin + 2..];
    }
    sortie.push_str(reste);
    sortie
}

/// Les fonctionnalités passées par `--features` / `-F` dans une commande déjà
/// découpée en mots. `--features a,b` comme `--features=a,b`, et les
/// répétitions s'additionnent : c'est ainsi que cargo les lit.
fn drapeaux_features(mots: &[&str]) -> Vec<String> {
    let mut vues: Vec<String> = Vec::new();
    let mut suite = mots.iter();
    while let Some(mot) = suite.next() {
        let valeur = if *mot == "--features" || *mot == "-F" {
            suite.next().copied()
        } else {
            mot.strip_prefix("--features=").or(mot.strip_prefix("-F="))
        };
        let Some(valeur) = valeur else { continue };
        for f in decouper(valeur) {
            if !vues.contains(&f) {
                vues.push(f);
            }
        }
    }
    vues
}

/// Une commande qui construit `tune-server`. Le filtre exige le PAQUET, pour ne
/// pas prendre le `cargo install librespot --features alsa-backend` du
/// Dockerfile — ni les chaînes de caractères Python qui parlent de
/// `--features` dans les étapes de vérification de `release.yml`.
fn construit_tune_server(ligne: &str) -> bool {
    let t = ligne.trim_start();
    if t.starts_with('#') {
        return false;
    }
    let construit = t.contains("cargo build")
        || t.contains("cross build")
        || t.contains("cargo install")
        || t.contains("cargo check");
    construit && (t.contains("--package tune-server") || t.contains("-p tune-server"))
}

/// Une liste de fonctionnalités relevée dans une recette, avec son ancre.
#[derive(Debug)]
struct Liste {
    ancre: String,
    features: Vec<String>,
    /// La ligne `cross build … --features ${{ matrix.features }}` de
    /// `release.yml` ne porte aucun nom à elle : elle RELAIE les entrées de
    /// matrice, relevées séparément. Ses « fonctionnalités » sont les morceaux
    /// de l'interpolation, pas des noms — elles sont donc écartées des
    /// contrôles de forme. La recherche de `dst` la traverse quand même : un
    /// `--features ${{ matrix.features }},dst` serait le raccourci évident.
    relais: bool,
}

/// Toutes les listes de fonctionnalités d'une recette.
fn listes(fichier: &str, source: &str) -> Vec<Liste> {
    let mut relevees = Vec::new();
    for (n, ligne) in lignes_logiques(source) {
        let t = ligne.trim();

        // 1. La BASE du marqueur : la source de vérité de plugin-catalog.
        if let Some((_, json)) = t.split_once("# plugin-catalog: ") {
            let base = base_du_marqueur(json).unwrap_or_else(|| {
                panic!(
                    "{fichier}:{n} — marqueur plugin-catalog sans `base` lisible : \
                     `{json}`. La forme du marqueur a changé et la garde ne lit \
                     plus la source des lignes `--features`."
                )
            });
            relevees.push(Liste {
                ancre: format!("{fichier}:{n} (base plugin-catalog)"),
                features: base,
                relais: false,
            });
            continue;
        }

        // 2. Une entrée de matrice `features: a,b,c`.
        if let Some(valeur) = t.strip_prefix("features: ") {
            relevees.push(Liste {
                ancre: format!("{fichier}:{n} (entrée de matrice)"),
                features: decouper(valeur),
                relais: false,
            });
            continue;
        }

        // 3. Une commande de build de tune-server.
        if construit_tune_server(t) {
            let recollee = normaliser_interpolations(t);
            let mots: Vec<&str> = recollee.split_whitespace().collect();
            let features = drapeaux_features(&mots);
            if !features.is_empty() {
                let relais = features.iter().any(|f| f.contains("${{"));
                let quoi = if relais {
                    "relais de matrice"
                } else {
                    "ligne de build"
                };
                relevees.push(Liste {
                    ancre: format!("{fichier}:{n} ({quoi})"),
                    features,
                    relais,
                });
            }
        }
    }
    relevees
}

/// 🔴 #4378 — aucune ligne de build qui PUBLIE n'allume `dst`.
///
/// ⚠️ Sabotage qui doit le faire tomber : ajouter `,dst` à n'importe quelle
/// ligne `--features` ou `features:` de `release.yml`, `docker.yml` ou du
/// `Dockerfile`, ou à n'importe quelle `base` de marqueur `plugin-catalog`.
#[test]
fn dst_n_est_dans_aucune_ligne_de_build_publiee() {
    let mut toutes: Vec<Liste> = Vec::new();
    for (nom, source) in RECETTES_PUBLIEES {
        let relevees = listes(nom, source);
        let plancher = PLANCHERS
            .iter()
            .find(|(f, _)| f == nom)
            .map_or(0, |(_, p)| *p);
        assert!(
            relevees.len() >= plancher,
            "{nom} : seulement {} liste(s) de fonctionnalités relevée(s), au \
             moins {plancher} attendue(s). Une garde qui ne trouve plus rien \
             doit ÉCHOUER, pas passer à vide — {relevees:?}",
            relevees.len()
        );
        toutes.extend(relevees);
    }

    // Contrôles POSITIFS : l'extracteur voit-il encore de vraies
    // fonctionnalités ? `oaat` est dans le `default` de tune-server et doit
    // donc figurer dans presque toutes les lignes livrées.
    assert!(
        toutes.len() >= 15,
        "seulement {} liste(s) de fonctionnalités sur l'ensemble des recettes \
         publiées — l'extracteur ne lit plus ce qu'il doit lire",
        toutes.len()
    );
    let avec_oaat = toutes
        .iter()
        .filter(|l| l.features.iter().any(|f| f == "oaat"))
        .count();
    assert!(
        avec_oaat >= 10,
        "seulement {avec_oaat} liste(s) citent `oaat` — l'extracteur rend des \
         listes vides ou tronquées, et la recherche de `dst` ne prouverait rien"
    );
    // La ligne qui relaie `${{ matrix.features }}` doit être VUE, et vue comme
    // un relais : c'est elle qui construit les deux cibles ARM. Si elle
    // disparaissait du relevé, un `,dst` collé derrière l'interpolation ne
    // serait lu par personne.
    assert!(
        toutes.iter().filter(|l| l.relais).count() >= 1,
        "aucun relais `${{{{ matrix.features }}}}` relevé — la ligne `cross \
         build` de release.yml a changé de forme et n'est plus lue"
    );
    let bases = toutes
        .iter()
        .filter(|l| l.ancre.contains("base plugin-catalog"))
        .count();
    assert!(
        bases >= 7,
        "seulement {bases} base(s) `plugin-catalog` relevée(s) — or c'est la \
         base, pas la ligne `--features`, que `plugin-catalog.py --write` \
         recopie. Sans elles la garde serait effacée au premier --write"
    );
    // Sens NÉGATIF : un drapeau n'est pas une fonctionnalité. Hors relais,
    // dont les « fonctionnalités » sont les morceaux de l'interpolation.
    for intrus in ["--no-default-features", "--features", "${{"] {
        let coupable = toutes
            .iter()
            .filter(|l| !l.relais)
            .find(|l| l.features.iter().any(|f| f == intrus));
        assert!(
            coupable.is_none(),
            "extracteur cassé : `{intrus}` compte comme une fonctionnalité — \
             {coupable:?}"
        );
    }

    // La garde.
    let fautives: Vec<&str> = toutes
        .iter()
        .filter(|l| l.features.iter().any(|f| est_dst(f)))
        .map(|l| l.ancre.as_str())
        .collect();
    assert!(
        fautives.is_empty(),
        "🔴 #4378 — la fonctionnalité `dst` est allumée dans une ligne de build \
         qui PUBLIE : {fautives:?}.\n\
         Le crate `dst-decoder` se déclare Apache-2.0 mais son README reproduit \
         l'en-tête ISO/Philips « Copyright is not released for non MPEG-4 Audio \
         conforming products », plus un avertissement brevets. Livrer un binaire \
         qui l'embarque, c'est trancher cette question : c'est un arbitrage \
         JURIDIQUE de Bertrand, pas une ligne de workflow.\n\
         Pour compiler `dst` sans rien publier, c'est `ci.yml` — hors portée de \
         cette garde."
    );
}

/// 🔴 #4378 — le trou du 20/09/2026, figé.
///
/// La contre-épreuve à la main a trouvé que `--features ${{ matrix.features
/// }},dst` passait au VERT. Une contre-épreuve ne se rejoue pas toute seule :
/// ce témoin la garde. Il exerce l'extracteur sur la ligne du sabotage, sans
/// toucher au dépôt.
#[test]
fn un_dst_colle_derriere_une_interpolation_est_vu() {
    let saboteur = "run: cross build --release --package tune-server --target \
                    ${{ matrix.target }} --no-default-features --features \
                    ${{ matrix.features }},dst";
    let relevees = listes("témoin", saboteur);
    assert_eq!(
        relevees.len(),
        1,
        "la ligne du sabotage doit rendre exactement une liste — {relevees:?}"
    );
    assert!(
        relevees[0].relais,
        "la ligne doit être reconnue comme un relais — {:?}",
        relevees[0]
    );
    assert!(
        relevees[0].features.iter().any(|f| est_dst(f)),
        "`dst` collé derrière l'interpolation n'est pas vu — {:?}",
        relevees[0]
    );

    // Sens NÉGATIF : la même ligne, telle qu'elle est écrite aujourd'hui, ne
    // doit rien signaler. Un témoin qui rougit toujours ne prouve rien.
    let propre = "run: cross build --release --package tune-server --target \
                  ${{ matrix.target }} --no-default-features --features \
                  ${{ matrix.features }}";
    let relevees = listes("témoin", propre);
    assert_eq!(relevees.len(), 1, "{relevees:?}");
    assert!(
        !relevees[0].features.iter().any(|f| est_dst(f)),
        "faux positif sur la ligne de relais réelle — {:?}",
        relevees[0]
    );
}

/// 🔴 #4378 — aucune recette de publication n'échappe à la garde.
///
/// La liste ci-dessus est écrite à la main : elle vieillit. Ce balayage lit le
/// dépôt et exige que tout fichier qui construit `tune-server` soit classé,
/// soit comme recette publiée (donc gardée), soit comme recette qui ne publie
/// rien.
///
/// ⚠️ Sabotage qui doit le faire tomber : ajouter un workflow qui construit
/// `tune-server` sans l'inscrire dans l'une des deux listes.
#[test]
fn aucune_recette_de_publication_n_echappe_a_la_garde() {
    let racine = racine();
    let mut trouves: BTreeSet<String> = BTreeSet::new();
    for repertoire in REPERTOIRES_DE_RECETTES {
        let chemin = racine.join(repertoire);
        let entrees = std::fs::read_dir(&chemin)
            .unwrap_or_else(|e| panic!("{} illisible : {e}", chemin.display()));
        for entree in entrees {
            let entree = entree.expect("entrée de répertoire illisible");
            if !entree.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            // Une recette est un fichier EXÉCUTÉ. `MIGRATION.md` et
            // `README.md` citent tous deux une ligne `cargo build --package
            // tune-server` : de la documentation, pas une recette — les
            // compter aurait fait rougir cette garde contre de la prose.
            let nom = entree.file_name().to_string_lossy().into_owned();
            let est_recette = nom.starts_with("Dockerfile")
                || [".yml", ".yaml", ".sh", ".py"]
                    .iter()
                    .any(|e| nom.ends_with(e));
            if !est_recette {
                continue;
            }
            let Ok(contenu) = std::fs::read_to_string(entree.path()) else {
                continue; // binaire : pas une recette
            };
            if !lignes_logiques(&contenu)
                .iter()
                .any(|(_, l)| construit_tune_server(l))
            {
                continue;
            }
            let relatif = if *repertoire == "." {
                nom
            } else {
                format!("{repertoire}/{nom}")
            };
            trouves.insert(relatif);
        }
    }

    // Contrôle POSITIF : le balayage retrouve-t-il ce qu'on sait être là ?
    for attendu in RECETTES_PUBLIEES.iter().map(|(n, _)| *n) {
        assert!(
            trouves.contains(attendu),
            "le balayage n'a pas retrouvé `{attendu}`, qui construit pourtant \
             tune-server — il ne garde plus rien : {trouves:?}"
        );
    }

    let connus: BTreeSet<&str> = RECETTES_PUBLIEES
        .iter()
        .map(|(n, _)| *n)
        .chain(RECETTES_SANS_PUBLICATION.iter().copied())
        .collect();
    let inconnus: Vec<&String> = trouves
        .iter()
        .filter(|f| !connus.contains(f.as_str()))
        .collect();
    assert!(
        inconnus.is_empty(),
        "🔴 #4378 — {inconnus:?} construi(sen)t tune-server sans être classé(s). \
         Si cette recette produit un artefact LIVRÉ, l'ajouter à \
         RECETTES_PUBLIEES (elle sera gardée contre `dst`) ; si elle ne publie \
         rien, à RECETTES_SANS_PUBLICATION, avec la raison."
    );
}

/// La table `[features]` d'un manifeste : nom -> entrées activées.
fn table_des_features(nom: &str, manifeste: &str) -> BTreeMap<String, Vec<String>> {
    let mut table = BTreeMap::new();
    let mut dedans = false;
    for ligne in manifeste.lines() {
        let t = ligne.trim();
        if t.starts_with('[') {
            dedans = t == "[features]";
            continue;
        }
        if !dedans || t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((clef, reste)) = t.split_once('=') else {
            continue;
        };
        let reste = reste.trim();
        assert!(
            reste.starts_with('[') && reste.ends_with(']'),
            "{nom} : la fonctionnalité `{}` n'est pas déclarée sur une seule \
             ligne (`{reste}`) — ce lecteur ne la verrait pas et la garde \
             passerait à côté",
            clef.trim()
        );
        table.insert(
            clef.trim().to_string(),
            decouper(&reste[1..reste.len() - 1]),
        );
    }
    table
}

/// Tout ce qu'un `cargo build` sans `--features` activerait : la fermeture
/// transitive de `default`.
fn fermeture_du_defaut(table: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let mut vus: BTreeSet<String> = BTreeSet::new();
    let mut a_voir: Vec<String> = table.get("default").cloned().unwrap_or_default();
    while let Some(entree) = a_voir.pop() {
        if !vus.insert(entree.clone()) {
            continue;
        }
        if let Some(suite) = table.get(&entree) {
            a_voir.extend(suite.iter().cloned());
        }
    }
    vus
}

/// 🔴 #4378 — `dst` est hors du `default` et n'est tirée par aucune autre
/// fonctionnalité.
///
/// La garde de build ci-dessus lit les lignes `--features`. Elle ne verrait
/// RIEN si `dst` entrait par la porte de derrière : un `default` qui la
/// contient, ou une fonctionnalité déjà livrée qui la tire (`bandcamp =
/// ["dep:tune-bandcamp", "dst"]`). Un `cargo build` sans `--features`
/// l'embarquerait alors dans tous les artefacts.
///
/// ⚠️ Sabotage qui doit le faire tomber : ajouter `"dst"` au `default` de
/// tune-core ou de tune-server, ou à n'importe quelle autre fonctionnalité.
#[test]
fn dst_est_hors_du_defaut_et_tiree_par_aucune_autre_feature() {
    for (nom, manifeste, temoin) in [
        ("tune-core", MANIFESTE_CORE, "local-audio"),
        ("tune-server", MANIFESTE_SERVEUR, "oaat"),
    ] {
        let table = table_des_features(nom, manifeste);

        // Contrôle POSITIF : sans la fonctionnalité, la garde ne garde rien.
        assert!(
            table.contains_key("dst"),
            "{nom} ne déclare plus de fonctionnalité `dst` : cette garde ne \
             garde plus rien. Si le décodeur DST a été retiré, retirer aussi \
             ce fichier ; s'il a été renommé, suivre le nom."
        );
        // Contrôle POSITIF : le lecteur voit-il encore une vraie table ?
        let defauts = table
            .get("default")
            .unwrap_or_else(|| panic!("{nom} n'a plus de `default` — lecteur de manifeste cassé"));
        assert!(
            defauts.len() >= 1 && !defauts.iter().any(|d| d.starts_with('[')),
            "{nom} : `default` mal lu ({defauts:?}) — lecteur de manifeste cassé"
        );

        let fermeture = fermeture_du_defaut(&table);
        assert!(
            fermeture.iter().any(|f| f.contains(temoin)),
            "{nom} : la fermeture du `default` ne contient même pas `{temoin}` \
             ({fermeture:?}) — le parcours transitif est cassé et ne prouverait \
             rien sur `dst`"
        );
        let par_defaut: Vec<&String> = fermeture.iter().filter(|f| est_dst(f)).collect();
        assert!(
            par_defaut.is_empty(),
            "🔴 #4378 — `dst` est atteinte depuis le `default` de {nom} \
             ({par_defaut:?}) : tout binaire construit sans `--features` \
             embarquerait `dst-decoder`. La licence n'est pas tranchée \
             (Apache-2.0 déclarée contre en-tête ISO/Philips) — arbitrage de \
             Bertrand avant d'allumer."
        );

        let tireuses: Vec<&String> = table
            .iter()
            .filter(|(clef, entrees)| clef.as_str() != "dst" && entrees.iter().any(|e| est_dst(e)))
            .map(|(clef, _)| clef)
            .collect();
        assert!(
            tireuses.is_empty(),
            "🔴 #4378 — dans {nom}, {tireuses:?} tire(nt) `dst`. Allumer l'une \
             d'elles allumerait le décodeur DST sans que la garde des lignes de \
             build ne voie passer le nom. Seule `dst` doit tirer `dst`."
        );
    }
}
