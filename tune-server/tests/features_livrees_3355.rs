//! 🔴 #3355 — les fonctionnalites PAR DEFAUT de `tune-server` sont-elles dans
//! les binaires qu'on LIVRE ?
//!
//! ## Le fait qui a motive ce fichier
//!
//! `cloud-relay` — le relais cloud, fonction Premium — est dans le `default`
//! de `tune-server` depuis toujours. Toutes les lignes de build passent
//! `--no-default-features` et aucune ne le listait : il n'etait compile dans
//! AUCUN binaire Linux ni Windows publie. Un client premium qui l'activait
//! obtenait `enabled: true`, un jeton, une URL — et rien. Pas d'erreur, pas de
//! ligne de journal : le code n'existait pas dans son binaire.
//!
//! Mesure sur les artefacts PUBLIES, marqueur par marqueur
//! (`strings tune-server | grep -c`) :
//!
//! | marqueur                       | .135 lin | .135 win | .135 mac | .141 lin | .141 arm | .141 win | .141 mac | .141 docker |
//! |--------------------------------|---------|---------|---------|---------|---------|---------|---------|---------|
//! | `connecting to relay`          | **0**   | **0**   | 1       | 1       | 1       | 1       | 1       | **0**   |
//! | `cloud relay client spawned`   | **0**   | **0**   | 3       | 3       | 3       | 3       | 3       | **0**   |
//! | `bridge relay disabled`        | **0**   | **0**   | 1       | 1       | 1       | 1       | 1       | **0**   |
//! | `cloud_relay_requires_premium` | **0**   | **0**   | 3       | 3       | 3       | 3       | 3       | **0**   |
//! | `bcsearch_public_api`          | —       | —       | —       | 3       | 3       | 3       | 3       | **0**   |
//!
//! macOS etait vert par ACCIDENT : sa ligne de build est la seule sans
//! `--no-default-features`, elle a donc garde les defauts. Et l'image Docker
//! est restee en dehors de la reparation du lot bugs-1 : elle perdait DEUX
//! fonctionnalites par defaut, `cloud-relay` et `bandcamp`.
//!
//! ## Pourquoi une garde
//!
//! Rien n'a rougi pendant tout ce temps. Ni la CI, ni la release, ni les
//! portes `clippy` : `toute_feature_declaree_est_activee_par_une_porte_clippy`
//! (`workflows_bornes.rs`) garde la ligne `cargo clippy`, c'est-a-dire ce
//! qu'on LINTE — pas ce qu'on EXPEDIE. Une feature peut sortir de toutes les
//! lignes de build sans qu'aucune porte ne le voie.
//!
//! Meme famille que #1427, #2865 et #3266 : une porte dont la portee est plus
//! etroite que ce qu'on croit, et qui rend un vert qui ne couvre pas ce qu'on
//! pense.
//!
//! ## Portee VOLONTAIREMENT etroite
//!
//! La garde ne lit QUE le `default` de `tune-server`, et QUE les lignes de
//! build des deux workflows qui PUBLIENT un artefact : `release.yml` (les
//! archives, l'installeur, les .deb) et `docker.yml` (l'image). Elle ne dit
//! rien des features optionnelles (`postgres`, `dj`, `audio-embedding`…) :
//! celles-la sont des choix par plateforme, pas une promesse. Elle ne lit pas
//! non plus les lignes de `ci.yml`, qui ne livrent rien. Une garde qui
//! exigerait l'inventaire complet sur toute la matrice rougirait des le
//! premier arbitrage et finirait desarmee.
//!
//! ## `include_str!` plutot qu'une lecture au chemin courant
//!
//! Modele des gardes #3519/#3520 et de `portee_ligne_de_test_3266.rs` : un
//! workflow renomme devient une erreur de COMPILATION, pas un test qui se
//! saute ou qui panique sur un chemin.

/// Le workflow qui produit les archives, l'installeur Windows et les .deb.
const RELEASE: &str = include_str!("../../.github/workflows/release.yml");

/// Le workflow qui produit l'image Docker publiee. C'est un artefact livre au
/// meme titre qu'un tar.gz — et c'est celui qui etait encore casse le
/// 08/09/2026, alors que les archives etaient reparees.
const DOCKER: &str = include_str!("../../.github/workflows/docker.yml");

/// Le manifeste qui declare le `default` — la promesse a tenir.
const MANIFESTE: &str = include_str!("../Cargo.toml");

/// Une fonctionnalite par defaut absente d'une ligne de build LIVREE doit etre
/// inscrite ici avec sa raison, MESUREE. La liste se relit ; un oubli, non.
///
/// Le couple est (fonctionnalite, identifiant de la ligne de build) :
/// l'identifiant est `<workflow> / <plateforme ou nom d'etape>`.
const HORS_PORTE: &[(&str, &str, &str)] = &[
    // L'exemption `release.yml / linux-aarch64` a ete LEVEE par #3613 : les
    // en-tetes ALSA de la cible sont desormais installees dans le conteneur
    // `cross` (`Cross.toml`, `pre-build`), et un garde-fou de release lit
    // l'ELF publie pour verifier que `libasound.so` y est bien declaree en
    // NEEDED. C'etait la cible que l'image Tune OS du Raspberry Pi installe.
    (
        "local-audio",
        "release.yml / linux-aarch64-musl",
        "IMPOSSIBLE sans forker `alsa-sys`, pas seulement couteux (#3613). Son \
         `build.rs` est `pkg_config::Config::new().statik(false).probe(\"alsa\")` \
         : le `statik(false)` est un LITTERAL, pas une variable d'environnement \
         comme le `LIBOPUS_STATIC` de #1288. La caisse emet donc toujours un \
         lien DYNAMIQUE vers `libasound`, ce que l'etape « Verify musl binary \
         is statically linked » de `release.yml` refuse par construction — et \
         cette garantie statique est la raison d'etre de la cible (NAS a vieille \
         userland, Synology DSM). Un NAS n'a par ailleurs pas de DAC.",
    ),
    (
        "local-audio",
        "docker.yml / Build tune-server (amd64, native)",
        "L'image d'execution (Dockerfile.dist) ne porte pas libasound2. MESURE \
         sur l'image publiee `renesenses/tune:v0.9.141`, couche \
         `app/tune-server` : `grep -c cpal` rend 0. Constat, pas arbitrage \
         (#3355).",
    ),
    (
        "local-audio",
        "docker.yml / Build tune-server (arm64, cross)",
        "Meme image d'execution sans libasound2, et en plus la compilation \
         croisee sans en-tetes ALSA pour la cible (#3355).",
    ),
];

/// Les fonctionnalites listees par `--features` dans une commande deja
/// decoupee. `--features a,b` comme `--features=a,b`, et les repetitions
/// s'additionnent — c'est ainsi que cargo les lit.
fn fonctionnalites(mots: &[&str]) -> Vec<String> {
    let mut vues: Vec<String> = Vec::new();
    let ajouter = |brut: &str, vues: &mut Vec<String>| {
        for f in brut.split(',') {
            let f = f.trim();
            if !f.is_empty() && !vues.iter().any(|v| v == f) {
                vues.push(f.to_string());
            }
        }
    };
    let mut suite = mots.iter();
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

/// La liste `default = [...]` d'un manifeste, sans les guillemets.
fn defauts(manifeste: &str) -> Vec<String> {
    let debut = manifeste
        .find("default = [")
        .expect("`default = [` absent du Cargo.toml de tune-server");
    let reste = &manifeste[debut + "default = [".len()..];
    let fin = reste
        .find(']')
        .expect("la liste `default` de tune-server n'est pas fermee");
    reste[..fin]
        .split(',')
        .map(|m| m.trim().trim_matches('"').to_string())
        .filter(|m| !m.is_empty())
        .collect()
}

/// Replie les continuations `\` d'un YAML en une seule ligne logique.
///
/// La ligne `cross build` de `docker.yml` tient sur deux lignes physiques et
/// porte son `--features` sur la SECONDE : sans ce repliage, le detecteur
/// lisait une commande sans aucune feature et se croyait devant une ligne nue.
fn lignes_logiques(source: &str) -> Vec<String> {
    let mut sorties: Vec<String> = Vec::new();
    let mut courante = String::new();
    for ligne in source.lines() {
        let t = ligne.trim();
        if t.starts_with('#') && courante.is_empty() {
            continue;
        }
        if let Some(debut) = t.strip_suffix('\\') {
            courante.push_str(debut.trim_end());
            courante.push(' ');
            continue;
        }
        courante.push_str(t);
        sorties.push(std::mem::take(&mut courante));
    }
    if !courante.is_empty() {
        sorties.push(courante);
    }
    sorties
}

/// Une ligne de build livree : son identifiant, ses fonctionnalites explicites,
/// et si elle coupe les defauts.
#[derive(Debug)]
struct LigneLivree {
    identifiant: String,
    features: Vec<String>,
    coupe_les_defauts: bool,
}

/// La commande de build de `tune-server` que porte une ligne logique, s'il y en
/// a une. `run: cargo build …` comme la ligne nue d'un bloc `run: |`.
fn commande_de_build(ligne: &str) -> Option<&str> {
    let t = ligne.trim();
    if t.starts_with('#') {
        return None;
    }
    let commande = t
        .strip_prefix("- run: ")
        .or_else(|| t.strip_prefix("run: "))
        .unwrap_or(t);
    let est_build = commande.starts_with("cargo build --release")
        || commande.starts_with("cross build --release");
    (est_build && commande.contains("--package tune-server")).then_some(commande)
}

/// Les lignes de build d'un workflow qui produisent un binaire publie.
///
/// Deux formes :
///
/// 1. les entrees de matrice de `release.yml` (`cross: true`), dont le
///    `features:` alimente l'unique ligne `cross build … ${{ matrix.features }}`
///    — identifiees par leur `platform:` ;
/// 2. les etapes qui portent une commande de build en dur — identifiees par le
///    `- name:` qui les precede.
fn lignes_livrees(workflow: &str, source: &str) -> Vec<LigneLivree> {
    let mut lignes: Vec<LigneLivree> = Vec::new();
    let logiques = lignes_logiques(source);

    // --- 1. Les commandes de build en dur, et le relais de matrice. --------
    let mut nom_courant = String::new();
    let mut relais_matrice: Option<bool> = None;
    for ligne in &logiques {
        let t = ligne.trim();
        if let Some(nom) = t.strip_prefix("- name: ") {
            nom_courant = nom.trim().to_string();
            continue;
        }
        let Some(commande) = commande_de_build(ligne) else {
            continue;
        };
        let mots: Vec<&str> = commande.split_whitespace().collect();
        let coupe = mots.contains(&"--no-default-features");
        // La ligne qui relaie `matrix.features` ne porte pas de features a
        // elle : ce sont les entrees de matrice qui les portent.
        if commande.contains("${{ matrix.features }}") {
            assert!(
                relais_matrice.is_none(),
                "{workflow} porte plusieurs lignes de build qui relaient \
                 `matrix.features` — le detecteur ne sait plus laquelle lit quoi"
            );
            relais_matrice = Some(coupe);
            continue;
        }
        lignes.push(LigneLivree {
            identifiant: format!("{workflow} / {nom_courant}"),
            features: fonctionnalites(&mots),
            coupe_les_defauts: coupe,
        });
    }

    // --- 2. Les entrees de matrice, chacune avec sa plateforme. ------------
    let Some(relais_coupe) = relais_matrice else {
        return lignes;
    };
    // Le balayage est BORNE au bloc `include:` : `runs-on:` apparait dans
    // chaque job du workflow, et s'en servir comme fin sans avoir d'abord
    // trouve `include:` arretait le detecteur des le premier job — il rendait
    // alors zero entree de matrice. La contre-epreuve `lignes.len() >= 5` a
    // fait rougir cette version-la.
    let mut plateforme: Option<String> = None;
    let mut features_entree: Option<Vec<String>> = None;
    let mut est_cross = false;
    let mut dans_matrice = false;
    let clore = |plateforme: &mut Option<String>,
                 features: &mut Option<Vec<String>>,
                 cross: &mut bool,
                 lignes: &mut Vec<LigneLivree>| {
        if let (Some(p), Some(f)) = (plateforme.clone(), features.clone())
            && *cross
        {
            lignes.push(LigneLivree {
                identifiant: format!("{workflow} / {p}"),
                features: f,
                coupe_les_defauts: relais_coupe,
            });
        }
        *plateforme = None;
        *features = None;
        *cross = false;
    };
    for ligne in &logiques {
        let t = ligne.trim();
        if t.starts_with('#') {
            continue;
        }
        if !dans_matrice {
            dans_matrice = t == "include:";
            continue;
        }
        if t.starts_with("- target: ") {
            clore(
                &mut plateforme,
                &mut features_entree,
                &mut est_cross,
                &mut lignes,
            );
            continue;
        }
        if let Some(p) = t.strip_prefix("platform: ") {
            plateforme = Some(p.trim().to_string());
        } else if let Some(f) = t.strip_prefix("features: ") {
            features_entree = Some(fonctionnalites(&["--features", f.trim()]));
        } else if t == "cross: true" {
            est_cross = true;
        } else if t.starts_with("runs-on:") {
            // Fin du bloc `include:`.
            break;
        }
    }
    clore(
        &mut plateforme,
        &mut features_entree,
        &mut est_cross,
        &mut lignes,
    );
    assert!(
        dans_matrice,
        "{workflow} relaie `matrix.features` mais n'a pas de bloc `include:` — \
         la matrice de build a change de forme et ce detecteur ne lit plus les \
         cibles croisees"
    );
    lignes
}

/// 🔴 #3355 — toute fonctionnalite du `default` de `tune-server` est activee
/// par toute ligne de build qui LIVRE un artefact et qui coupe les defauts.
///
/// Le `default` d'un manifeste est une PROMESSE : « ce serveur sait faire
/// ca ». Chaque ligne qui passe `--no-default-features` reprend cette promesse
/// a zero et doit la retenir explicitement. Une fonctionnalite oubliee ne
/// rougit nulle part — elle disparait du binaire publie, en silence, et le
/// client la voit pourtant activee dans son ecran.
///
/// La garde ne cite aucun nom de feature : elle compare le `default` lu dans le
/// manifeste aux `--features` lus dans les workflows. Une feature ajoutee au
/// `default` demain tombera donc du bon cote toute seule, ou fera rougir cette
/// garde.
///
/// ⚠️ Sabotage qui doit le faire tomber : retirer `cloud-relay` du
/// `--features` d'une ligne de build de `release.yml` ou de `docker.yml` —
/// c'est litteralement l'etat du depot jusqu'a la v0.9.141 incluse.
///
/// ⚠️ Sabotage inverse, qui doit le faire tomber aussi : inscrire dans
/// `HORS_PORTE` une fonctionnalite qui n'est plus dans le `default`, ou une
/// ligne de build qui n'existe plus. Une justification perimee rassure sans
/// rien couvrir.
#[test]
fn toute_feature_par_defaut_est_livree_par_toute_ligne_de_build_de_release() {
    let defauts = defauts(MANIFESTE);
    assert!(
        defauts.len() >= 4,
        "seulement {} fonctionnalite(s) par defaut reconnue(s) : {defauts:?} — \
         la forme `default = [\"…\"]` du manifeste a change et cette garde ne \
         garde plus rien",
        defauts.len()
    );
    // Sens NEGATIF : l'extracteur ne doit pas prendre un drapeau pour une
    // fonctionnalite.
    for intrus in ["--no-default-features", "--features", "${{"] {
        assert!(
            !defauts.iter().any(|d| d == intrus),
            "extracteur casse : `{intrus}` compte comme une fonctionnalite — \
             {defauts:?}"
        );
    }

    let mut lignes = lignes_livrees("release.yml", RELEASE);
    let docker = lignes_livrees("docker.yml", DOCKER);
    assert!(
        docker.len() >= 2,
        "seulement {} ligne(s) de build relevee(s) dans docker.yml — l'image \
         publiee n'est plus gardee : {docker:?}",
        docker.len()
    );
    lignes.extend(docker);
    assert!(
        lignes.len() >= 7,
        "seulement {} ligne(s) de build relevee(s) — le detecteur ne voit plus \
         ce qu'il doit lire. Un garde qui ne trouve rien doit ECHOUER, pas \
         passer a vide : {lignes:?}",
        lignes.len()
    );
    assert!(
        lignes.iter().filter(|l| l.coupe_les_defauts).count() >= 6,
        "moins de six lignes de build passent `--no-default-features` — c'est \
         justement la condition qui rend cette garde necessaire, et le \
         detecteur ne la voit plus : {:?}",
        lignes
            .iter()
            .map(|l| (&l.identifiant, l.coupe_les_defauts))
            .collect::<Vec<_>>()
    );
    for ligne in &lignes {
        assert!(
            !ligne.identifiant.ends_with(" / "),
            "une ligne de build livree n'a pas de nom d'etape : la garde ne \
             saurait pas quoi inscrire dans HORS_PORTE — {ligne:?}"
        );
        // Sens NEGATIF : aucune ligne de build livree n'est nue aujourd'hui.
        // Une ligne relevee sans aucune feature signalerait un extracteur
        // casse, pas une ligne nue.
        assert!(
            !ligne.features.is_empty(),
            "aucune fonctionnalite relevee sur la ligne `{}` — extracteur casse",
            ligne.identifiant
        );
        assert!(
            !ligne.features.iter().any(|f| f.starts_with("${{")),
            "la ligne `{}` relaie une expression GitHub comme feature : le \
             detecteur lit un modele, pas une valeur — {ligne:?}",
            ligne.identifiant
        );
    }

    let mut manquantes: Vec<String> = Vec::new();
    for ligne in &lignes {
        // Sans `--no-default-features`, cargo ajoute le `default` : la promesse
        // est tenue par construction. C'est le cas — accidentel — de la ligne
        // macOS, seule raison pour laquelle le relais cloud a jamais marche
        // quelque part.
        if !ligne.coupe_les_defauts {
            continue;
        }
        for defaut in &defauts {
            if ligne.features.iter().any(|f| f == defaut) {
                continue;
            }
            if HORS_PORTE
                .iter()
                .any(|(f, id, _)| *f == defaut.as_str() && *id == ligne.identifiant.as_str())
            {
                continue;
            }
            manquantes.push(format!("`{defaut}` absente de « {} »", ligne.identifiant));
        }
    }
    assert!(
        manquantes.is_empty(),
        "ces fonctionnalites sont dans le `default` de tune-server et ne sont \
         activees par AUCUN `--features` de la ligne de build qui les livre :\n  \
         {}\n\
         La ligne passe `--no-default-features` : le code qu'elles gardent \
         n'entre PAS dans l'artefact publie, sans erreur ni ligne de journal \
         (#3355 — c'etait `cloud-relay`, fonction Premium, absente de tous les \
         binaires Linux et Windows jusqu'a la v0.9.141, et de l'image Docker \
         y compris en v0.9.141).\n\
         Deux issues seulement :\n\
           1. ajouter la fonctionnalite au `--features` de cette ligne ;\n\
           2. l'inscrire dans HORS_PORTE ci-dessus AVEC sa raison MESUREE, si \
         la ligne ne PEUT pas la compiler (compilation croisee, image \
         d'execution, OS, materiel).\n\
         Defauts a tenir : {defauts:?}",
        manquantes.join("\n  ")
    );

    // Une entree de HORS_PORTE qui ne correspond plus a rien est une
    // justification perimee : elle rassure sans rien couvrir.
    let perimees: Vec<String> = HORS_PORTE
        .iter()
        .filter(|(f, id, _)| {
            !defauts.iter().any(|d| d == f) || !lignes.iter().any(|l| l.identifiant == *id)
        })
        .map(|(f, id, _)| format!("({f}, {id})"))
        .collect();
    assert!(
        perimees.is_empty(),
        "HORS_PORTE justifie des couples qui n'existent plus : {perimees:?} — \
         retirer l'entree plutot que la laisser rassurer.\n\
         Defauts aujourd'hui : {defauts:?}\n\
         Lignes de build aujourd'hui : {:?}",
        lignes.iter().map(|l| &l.identifiant).collect::<Vec<_>>()
    );
}
