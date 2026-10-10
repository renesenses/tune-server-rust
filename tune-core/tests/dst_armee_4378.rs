//! 🔴 #4378 — la fonctionnalité `dst` doit être DANS chaque binaire publié.
//!
//! ## Pourquoi cette garde existe
//!
//! Le décodeur DST (DSD compressé des SACD) s'appuie sur le crate
//! `dst-decoder`, qui se déclare Apache-2.0 (crates.io, GitHub, fichier
//! `LICENSE`) ; son README reproduit l'en-tête ISO/Philips du code de
//! référence et un avertissement brevets. Le code est parti DÉSARMÉ le
//! 20/09/2026 en attendant l'arbitrage. Bertrand a accepté la licence le
//! 06/10/2026 : la feature `dst` est désormais LIVRÉE.
//!
//! Cette garde était le verrou « jamais dans un binaire publié ». Elle est
//! retournée, avec le même lecteur de fichiers : une fonctionnalité livrée ne
//! reste pas livrée toute seule. Il suffit qu'une ligne `--features` soit
//! réécrite sans elle — ou qu'une base `plugin-catalog` la perde, et le
//! prochain `--write` l'efface de la ligne — pour qu'un binaire publié refuse
//! de nouveau les DSDIFF DST, sans que rien ne rougisse. C'est le mode de
//! panne exact de #3355 (`cloud-relay` absent de tous les binaires livrés).
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
//! ## Portée : ce qui PUBLIE
//!
//! `ci.yml` ne produit aucun artefact livré : il est hors de cette garde (il
//! compile et teste `dst` de son côté). `test-postgres.yml`, `plugin-sdk.yml`,
//! `Dockerfile.bridge` (tune-bridge) n'ont pas de ligne de build de
//! `tune-server`.
//!
//! Les images Tune OS (`image/build-*.sh`, `tune-os.yml`) et les scripts de
//! déploiement .15/.18 ne compilent RIEN : ils déposent une archive déjà
//! construite par `release.yml`. Ils sont donc couverts par ricochet — et le
//! balayage ci-dessous rougit si l'un d'eux se met à compiler sans être classé.

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

/// 🔴 #4378 — toute ligne de build qui PUBLIE allume `dst`.
///
/// ⚠️ Sabotage qui doit le faire tomber : retirer `dst` de n'importe quelle
/// ligne `--features` ou `features:` de `release.yml`, `docker.yml` ou du
/// `Dockerfile`, ou de n'importe quelle `base` de marqueur `plugin-catalog`.
#[test]
fn dst_est_dans_toute_ligne_de_build_publiee() {
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

    // La garde. Le relais `${{ matrix.features }}` ne porte aucun nom à
    // lui : ce sont les entrées de matrice qu'il relaie, relevées à part, qui
    // doivent nommer `dst`.
    let manquantes: Vec<&str> = toutes
        .iter()
        .filter(|l| !l.relais && !l.features.iter().any(|f| est_dst(f)))
        .map(|l| l.ancre.as_str())
        .collect();
    assert!(
        manquantes.is_empty(),
        "🔴 #4378 — la fonctionnalité `dst` MANQUE à une ligne de build qui \
         PUBLIE : {manquantes:?}.\n\
         Licence de `dst-decoder` acceptée le 06/10/2026 : tout binaire livré \
         lit les DSDIFF compressés DST. Une ligne sans `dst` publierait un \
         binaire qui les refuse — le mode de panne de #3355. Ajouter `dst` à la \
         `base` du marqueur `plugin-catalog` (puis `plugin-catalog.py --write`), \
         ou à la ligne elle-même quand elle n'a pas de marqueur."
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
         RECETTES_PUBLIEES (elle devra alors allumer `dst`) ; si elle ne publie \
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

/// La déclaration de `dst-decoder` dans le manifeste de tune-core.
fn dependance_dst_decoder(manifeste: &str) -> Option<&str> {
    manifeste
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("dst-decoder ") || l.starts_with("dst-decoder="))
}

/// 🔴 #4378 — `dst` est déclarée dans les deux caisses, tune-server la relaie
/// vers tune-core, et `dst-decoder` est FIGÉ à la version relue.
///
/// Sans le relais, `--features dst` sur tune-server n'allumerait rien dans
/// tune-core : toutes les lignes ci-dessus seraient vertes et aucun binaire
/// ne décoderait le DST. Sans la version figée, un `cargo update` ferait
/// entrer une version que personne n'a relue d'un crate jeune, à un seul
/// mainteneur, dont la sortie n'a de contrôle que l'empreinte de référence.
///
/// ⚠️ Sabotages qui doivent le faire tomber : `dst = []` dans tune-server ;
/// `version = "0.1.2"` (sans `=`) dans tune-core.
#[test]
fn dst_est_declaree_relayee_et_figee() {
    let core = table_des_features("tune-core", MANIFESTE_CORE);
    let serveur = table_des_features("tune-server", MANIFESTE_SERVEUR);
    // Contrôle POSITIF : le lecteur voit-il encore une vraie table ?
    for (nom, table, temoin) in [
        ("tune-core", &core, "local-audio"),
        ("tune-server", &serveur, "oaat"),
    ] {
        assert!(
            table.contains_key(temoin),
            "{nom} : `{temoin}` introuvable — lecteur de manifeste cassé ({:?})",
            table.keys().collect::<Vec<_>>()
        );
    }
    assert_eq!(
        core.get("dst").map(Vec::as_slice),
        Some(&["dep:dst-decoder".to_string()][..]),
        "tune-core : la feature `dst` doit tirer `dep:dst-decoder`, et lui seul"
    );
    assert!(
        serveur
            .get("dst")
            .is_some_and(|e| e.iter().any(|f| f == "tune-core/dst")),
        "🔴 #4378 — tune-server doit relayer `dst` vers `tune-core/dst` : sans \
         ce relais, `--features dst` des lignes de build n'allume rien — {:?}",
        serveur.get("dst")
    );
    let declaration = dependance_dst_decoder(MANIFESTE_CORE)
        .expect("tune-core ne déclare plus `dst-decoder` : la feature `dst` ne tire rien");
    assert!(
        declaration.contains("version = \"=0.1.2\"") && declaration.contains("optional = true"),
        "🔴 #4378 — `dst-decoder` doit rester figé (`=0.1.2`) et optionnel : \
         une montée se relit (licence, `unsafe`, empreinte de référence) avant \
         d'entrer dans un binaire livré — `{declaration}`"
    );
}
