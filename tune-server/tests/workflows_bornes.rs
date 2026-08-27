//! Garde-fou : tout job de CI doit avoir un plafond de duree.
//!
//! Sans `timeout-minutes`, un job bloque tourne jusqu'au plafond GitHub de SIX
//! HEURES, puis meurt en emportant ce qu'il produisait. C'est arrive trois fois
//! en trois jours — 0.9.86, 0.9.87, 0.9.88 — toujours sur `apt-get`, que les
//! miroirs Ubuntu laissaient pendre sans jamais rendre la main. githubstatus
//! declarait « All Systems Operational » : rien ne prevenait.
//!
//! Le correctif de #1937 bornait LA COMMANDE fautive. Celui-ci borne la CLASSE
//! de panne : un `cargo` qui ne rend pas la main, une notarisation Apple qui
//! pend, un `gh api` sans reponse. Un plafond ne peut que faire echouer plus
//! TOT — il ne change aucune logique de build.
//!
//! Ce test relit les workflows qui peuvent occuper les runners. `ci.yml` garde
//! les fusions ouvertes ; `release.yml` produit ce qui est LIVRE. Les confondre
//! a deja donne un faux vert (#1768) — d'ou la verification des deux.

use std::fs;
use std::path::Path;

/// Extrait les jobs d'un workflow : `(nom, corps)`.
///
/// Analyse par indentation plutot qu'avec un lecteur YAML — le depot n'a pas de
/// dependance YAML et n'a aucune raison d'en prendre une pour ce test. Un job
/// est une cle a exactement deux espaces sous `jobs:`.
fn jobs(source: &str) -> Vec<(String, String)> {
    let mut dedans = false;
    let mut trouves: Vec<(String, String)> = Vec::new();

    for ligne in source.lines() {
        if ligne.trim_start().starts_with('#') {
            continue;
        }
        if ligne == "jobs:" {
            dedans = true;
            continue;
        }
        if !dedans {
            continue;
        }
        // Retour a l'indentation zero sur autre chose : on a quitte `jobs:`.
        if !ligne.is_empty() && !ligne.starts_with(' ') {
            break;
        }

        let est_entete_de_job = ligne.starts_with("  ")
            && !ligne.starts_with("   ")
            && ligne.trim_end().ends_with(':')
            && !ligne.trim().is_empty();

        if est_entete_de_job {
            let nom = ligne.trim().trim_end_matches(':').to_string();
            trouves.push((nom, String::new()));
        } else if let Some(dernier) = trouves.last_mut() {
            dernier.1.push_str(ligne);
            dernier.1.push('\n');
        }
    }

    trouves
}

/// Une cle du job lui-meme, et non d'une de ses etapes : quatre espaces
/// exactement, pas de tiret.
fn cle_de_job(ligne: &str, cle: &str) -> bool {
    ligne.starts_with("    ") && !ligne.starts_with("     ") && ligne.trim().starts_with(cle)
}

fn appelle_un_workflow(ligne: &str) -> bool {
    cle_de_job(ligne, "uses:")
}

fn pose_un_plafond(ligne: &str) -> bool {
    cle_de_job(ligne, "timeout-minutes:")
}

fn verifier(fichier: &str) {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(racine.join("../.github/workflows").join(fichier))
        .unwrap_or_else(|e| panic!("{fichier} illisible : {e}"));

    let jobs = jobs(&source);
    assert!(
        !jobs.is_empty(),
        "{fichier} : aucun job trouve — l'analyse est cassee, pas le fichier"
    );

    let fautifs: Vec<&str> = jobs
        .iter()
        // Un job qui appelle un workflow reutilisable porte `uses:` a SON
        // niveau (quatre espaces). GitHub y refuse `timeout-minutes` : le
        // plafond doit vivre dans le workflow appele (tune-os.yml a le sien).
        //
        // ⚠️ Viser `corps.contains("uses:")` tout court rendrait ce test
        // AVEUGLE : presque chaque job contient `- uses: actions/checkout@v4`
        // dans ses etapes, donc presque chaque job serait exclu. Premiere
        // version de ce garde-fou, prise en defaut par sa propre contre-
        // epreuve — le plafond d'un job retire, il repondait vert.
        .filter(|(_, corps)| !corps.lines().any(appelle_un_workflow))
        .filter(|(_, corps)| !corps.lines().any(pose_un_plafond))
        .map(|(nom, _)| nom.as_str())
        .collect();

    assert!(
        fautifs.is_empty(),
        "ces jobs de {fichier} n'ont pas de `timeout-minutes` : {fautifs:?}\n\
         Sans plafond, un blocage y tourne SIX HEURES avant que GitHub ne le tue \
         — trois releases l'ont paye (0.9.86, 0.9.87, 0.9.88).\n\
         Ajouter `timeout-minutes: <N>` sous le `runs-on:` du job, avec une \
         valeur large (2 a 20x la duree observee) : le but n'est pas de serrer \
         au plus juste, c'est d'empecher les six heures."
    );
}

#[test]
fn tout_job_de_release_a_un_plafond() {
    verifier("release.yml");
}

#[test]
fn tout_job_de_ci_a_un_plafond() {
    for fichier in [
        "ci.yml",
        "test-postgres.yml",
        "refs-issues.yml",
        "widget-ci.yml",
    ] {
        verifier(fichier);
    }
}

fn workflow(fichier: &str) -> String {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(racine.join("../.github/workflows").join(fichier))
        .unwrap_or_else(|e| panic!("{fichier} illisible : {e}"))
}

#[test]
fn les_runs_obsoletes_de_pr_sont_annules() {
    for fichier in [
        "ci.yml",
        "test-postgres.yml",
        "refs-issues.yml",
        "widget-ci.yml",
    ] {
        let source = workflow(fichier);
        assert!(
            source.contains("github.event.pull_request.number || github.ref"),
            "{fichier} ne groupe pas les runs d'une meme PR"
        );
        assert!(
            source.contains("cancel-in-progress: true"),
            "{fichier} laisse tourner les SHA devenus obsoletes"
        );
    }
}

#[test]
fn les_pr_empilees_declenchent_la_ci_rapide() {
    let source = workflow("ci.yml");
    let declencheurs = source
        .split_once("env:")
        .map(|(avant, _)| avant)
        .expect("ci.yml garde un bloc env apres ses declencheurs");

    assert!(declencheurs.contains("  pull_request:\n"));
    assert!(
        !declencheurs.contains("  pull_request:\n    branches:"),
        "la CI principale exclut encore les PR dont la base est une branche de travail"
    );
}

#[test]
fn la_voie_rapide_est_reservee_aux_bases_batch() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let profil = fs::read_to_string(racine.join("../scripts/determiner-profil-ci.sh"))
        .expect("scripts/determiner-profil-ci.sh lisible");
    assert!(profil.contains("batch/*) printf '%s\\n' rapide"));
    assert!(profil.contains("*) printf '%s\\n' complet"));
    assert!(profil.contains("FORCER_COMPLET"));

    let ci = workflow("ci.yml");
    assert!(ci.contains("bash scripts/determiner-profil-ci.sh --autotest"));
    assert!(ci.contains("PROFIL_CI=complet"));
    assert!(ci.contains(
        "FORCER_COMPLET: ${{ contains(github.event.pull_request.labels.*.name, 'ci:full') }}"
    ));

    let postgres = workflow("test-postgres.yml");
    assert!(postgres.contains("!startsWith(github.base_ref, 'batch/')"));
    assert!(postgres.contains("contains(github.event.pull_request.labels.*.name, 'ci:full')"));
}

#[test]
fn les_pr_compilent_vite_et_la_branche_de_livraison_compile_tout() {
    let source = workflow("ci.yml");
    let jobs = jobs(&source);
    let corps = |nom: &str| {
        jobs.iter()
            .find(|(candidat, _)| candidat == nom)
            .map(|(_, corps)| corps.as_str())
            .unwrap_or_else(|| panic!("job {nom} absent de ci.yml"))
    };

    let windows = corps("windows-pr");
    assert!(windows.contains("if: github.event_name == 'pull_request'"));
    assert!(windows.contains("--features oaat,postgres,dj,karaoke,bandcamp,plugins-wasm"));
    assert!(
        windows
            .contains("--features oaat,local-audio,asio,postgres,dj,karaoke,bandcamp,plugins-wasm")
    );

    let macos = corps("macos-pr");
    assert!(macos.contains("if: github.event_name == 'pull_request'"));
    assert!(macos.contains("cargo check --package tune-server"));

    // Le noyau reste execute sur chaque correctif Rust. Les suites longues et
    // les deux plateformes ne sont differees que pour une base batch/*.
    for nom in ["fmt", "test", "clippy", "audit", "ffi"] {
        assert!(
            !corps(nom).contains("needs.impact.outputs.full"),
            "job du noyau {nom} differe a tort jusqu'a l'integration du lot"
        );
    }

    for nom in ["test", "clippy"] {
        assert!(
            corps(nom).contains("-p tune-core -p tune-stream-http -p tune-server"),
            "job {nom} : les tests du transport HTTP extrait ne sont plus executes explicitement"
        );
    }
    for nom in [
        "test-shipped-features",
        "audio-embedding",
        "windows-pr",
        "macos-pr",
    ] {
        assert!(
            corps(nom).contains("needs.impact.outputs.full == 'true'"),
            "suite complete {nom} encore lancee sur chaque correctif du lot"
        );
    }

    let livraison = corps("build");
    assert!(livraison.contains("if: github.event_name != 'pull_request'"));
    let dependances_livraison: Vec<&str> = livraison
        .lines()
        .filter(|ligne| cle_de_job(ligne, "needs:"))
        .map(str::trim)
        .collect();
    assert_eq!(
        dependances_livraison,
        ["needs: impact"],
        "la matrice de livraison ne doit attendre que le classifieur leger, \
         jamais les compilations ou tests Linux"
    );
    for cible in [
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
    ] {
        assert!(
            livraison.contains(cible),
            "cible de livraison perdue : {cible}"
        );
    }

    // La voie rapide doit etre un PREALABLE commun, pas une suppression locale
    // oubliee sur un des dix jobs couteux. Le moindre job sans cette condition
    // ferait encore payer un runner sur une PR de documentation.
    for nom in [
        "fmt",
        "test",
        "test-shipped-features",
        "audio-embedding",
        "windows-pr",
        "macos-pr",
        "build",
        "clippy",
        "audit",
        "ffi",
    ] {
        let job = corps(nom);
        assert!(
            job.lines()
                .any(|ligne| cle_de_job(ligne, "needs:") && ligne.trim() == "needs: impact"),
            "job {nom} non relie au classifieur d impact"
        );
        assert!(
            job.contains("needs.impact.outputs.rust == 'true'"),
            "job {nom} ignore encore le verdict d impact"
        );
    }

    let impact = corps("impact");
    assert!(impact.contains("bash scripts/detecter-impact-ci.sh --autotest"));
    assert!(impact.contains("bash scripts/determiner-profil-ci.sh --autotest"));
    assert!(impact.contains("full: ${{ steps.classer.outputs.full }}"));
    assert!(impact.contains("bash scripts/verifier-fermeture.sh --autotest"));
    assert!(impact.contains("bash scripts/verifier-refs-issues.sh --autotest"));
    assert!(impact.contains("python3 scripts/preflight-check.py --self-test"));
}

/// `setup-rust-toolchain` active son propre `Swatinem/rust-cache` par defaut.
/// En poser un second juste apres restaure deux fois `target/` ; le second peut
/// meme remplacer un cache exact par un ancien match partiel. Le run
/// 32935964507 l'a fait sur macOS (618 Mio puis 508 Mio) avant de recompiler
/// 32 min 32, sans pouvoir sauvegarder depuis `release/v0.9` (#2439).
#[test]
fn la_ci_n_utilise_qu_un_cache_rust_et_la_release_peut_le_renouveler() {
    let source = workflow("ci.yml");
    let jobs = jobs(&source);
    let corps = |nom: &str| {
        jobs.iter()
            .find(|(candidat, _)| candidat == nom)
            .map(|(_, corps)| corps.as_str())
            .unwrap_or_else(|| panic!("job {nom} absent de ci.yml"))
    };
    let setup = "uses: actions-rust-lang/setup-rust-toolchain@v1";
    let cache = "uses: Swatinem/rust-cache@v2";
    let condition_de_confiance =
        "github.ref == 'refs/heads/main' || github.ref == 'refs/heads/release/v0.9'";

    for nom in [
        "fmt",
        "test",
        "test-shipped-features",
        "audio-embedding",
        "windows-pr",
        "macos-pr",
        "build",
        "clippy",
        "ffi",
    ] {
        let job = corps(nom);
        assert_eq!(
            job.matches(setup).count(),
            1,
            "job {nom} : installation Rust absente ou dupliquee"
        );
    }

    let configuration_setup = |nom: &str| {
        corps(nom)
            .split(setup)
            .nth(1)
            .and_then(|suite| suite.split("\n      - ").next())
            .unwrap_or_else(|| panic!("job {nom} : bloc setup-rust-toolchain illisible"))
    };

    let fmt = corps("fmt");
    assert!(
        configuration_setup("fmt").contains("cache: false"),
        "rustfmt doit desactiver le cache Cargo implicite"
    );
    assert!(
        !fmt.contains(cache),
        "rustfmt ne compile rien : lui ajouter un cache Cargo ne fait que payer son transfert"
    );

    // Ces jobs n'ont rien a partager avec un AUTRE nom de job : le cache
    // integre possede deja la bonne partition. En ajouter un explicite apres
    // lui est exactement le double chargement observe. La condition d'ecriture
    // reste cependant bornee aux deux branches de confiance.
    for nom in [
        "test",
        "test-shipped-features",
        "audio-embedding",
        "clippy",
        "ffi",
    ] {
        let job = corps(nom);
        let configuration = configuration_setup(nom);
        assert!(
            configuration.contains("cache-save-if:")
                && configuration.contains(condition_de_confiance),
            "job {nom} : son cache integre doit etre renouvelable depuis main et release/v0.9"
        );
        assert!(
            !configuration.contains("cache: false"),
            "job {nom} : son unique cache integre est desactive"
        );
        assert!(
            !job.contains(cache),
            "job {nom} : un second cache explicite recharge encore target/"
        );
    }

    // Ces trois jobs partagent volontairement la MEME partition par cible,
    // malgre des noms de jobs differents. Eux gardent donc le cache explicite
    // et coupent celui de setup-rust-toolchain.
    for nom in ["windows-pr", "macos-pr", "build"] {
        let job = corps(nom);
        assert!(
            configuration_setup(nom).contains("cache: false"),
            "job {nom} : le cache implicite double encore la partition partagee"
        );
        assert_eq!(
            job.matches(cache).count(),
            1,
            "job {nom} : il faut exactement un cache explicite partage par cible"
        );
    }

    let livraison = corps("build");
    assert!(
        livraison.contains("save-if:") && livraison.contains(condition_de_confiance),
        "la matrice doit renouveler le cache partage depuis main et release/v0.9"
    );
    for nom in ["windows-pr", "macos-pr"] {
        let job = corps(nom);
        assert!(
            job.contains("save-if: false"),
            "job {nom} : une PR ne doit jamais ecrire le cache de livraison"
        );
        assert!(
            !job.contains(condition_de_confiance),
            "job {nom} : la politique d'ecriture des branches remplace le veto PR"
        );
    }

    assert!(
        !source.contains("save-if: ${{ github.ref == 'refs/heads/main' }}")
            && !source.contains("cache-save-if: ${{ github.ref == 'refs/heads/main' }}"),
        "un cache de ci.yml exclut encore la vraie branche de livraison release/v0.9"
    );
}

#[test]
fn postgres_et_widget_ne_sont_plus_doubles_dans_la_ci_generale() {
    let ci = workflow("ci.yml");
    let noms: Vec<String> = jobs(&ci).into_iter().map(|(nom, _)| nom).collect();
    assert!(!noms.iter().any(|nom| nom == "test-postgres"));
    assert!(!noms.iter().any(|nom| nom == "widget"));

    let postgres = workflow("test-postgres.yml");
    assert_eq!(
        postgres
            .lines()
            .filter(|ligne| {
                ligne.trim()
                    == "run: cargo test -p tune-core --no-default-features --features postgres,oaat"
            })
            .count(),
        1,
        "la suite generale PostgreSQL doit compiler une seule fois"
    );
    assert!(postgres.contains("pg_1706 -- --nocapture"));
    assert!(postgres.contains("pg_schema_parity -- --nocapture"));

    let widget = workflow("widget-ci.yml");
    assert!(widget.contains("      - \"tune-widget/**\""));
    assert!(widget.contains("cargo check --release"));
    assert!(widget.contains("cargo test"));
}

/// Le plafond de l'etape apt doit laisser passer ses TROIS essais.
///
/// La boucle fait au pire 3 x (100 + 200) + 2 x 20 = 940 s, soit 15 min 40. Le
/// plafond etait a 6 min : la marche se faisait tuer PENDANT le deuxieme essai
/// — on payait le cout de la boucle sans jamais en avoir les trois essais, et
/// le message d'erreur final ne pouvait meme pas s'afficher (mesure du 19/08 :
/// tuee a 6 min 07).
#[test]
fn le_plafond_de_letape_apt_laisse_passer_les_trois_essais() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));

    for fichier in ["release.yml", "ci.yml"] {
        let source = fs::read_to_string(racine.join("../.github/workflows").join(fichier))
            .unwrap_or_else(|e| panic!("{fichier} illisible : {e}"));

        // Decoupe sur l'EN-TETE d'etape, pas sur le libelle seul : « Install
        // ALSA dev » apparait aussi dans un commentaire de `Build
        // airplay-daemon` qui renvoie a cette etape. Un garde-fou qui compte
        // les commentaires se declenche sur lui-meme.
        for (i, bloc) in source.split("- name: Install ALSA dev").enumerate().skip(1) {
            let entete: String = bloc.chars().take(400).collect();
            let plafond = entete
                .split("timeout-minutes:")
                .nth(1)
                .and_then(|reste| reste.split_whitespace().next())
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or_else(|| {
                    panic!("{fichier} : etape « Install ALSA dev » n{i} sans timeout-minutes")
                });

            assert!(
                plafond >= 16,
                "{fichier} : « Install ALSA dev » n{i} plafonnee a {plafond} min, alors que \
                 ses trois essais demandent 15 min 40 au pire.\n\
                 En dessous de 16, la marche est tuee pendant un essai : on paie le \
                 cout de la reprise sans en avoir le benefice."
            );
        }
    }
}

/// Le tamponnage du DMG macOS doit REESSAYER.
///
/// Apple repond « Accepted » avant que le ticket ne soit publie sur son CDN.
/// Pendant ces quelques secondes, `xcrun stapler staple` rend « could not find
/// ticket ». L'etape tournant sous `bash -e`, un unique appel la faisait mourir
/// — et le garde-fou supprimait alors un DMG PARFAITEMENT notarise.
///
/// Vecu sur v0.9.102 : soumission acceptee par Apple a 21:50:07, etape morte a
/// 21:50:15. Onze secondes : ni un refus, ni le plafond de 600 s.
///
/// Ce test verrouille la boucle. Retirer les reprises le rend ROUGE.
#[test]
fn le_tamponnage_du_dmg_reessaie() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(racine.join("../.github/workflows/release.yml"))
        .expect("release.yml lisible");

    let debut = source
        .find("- name: Notarize DMG (macOS)")
        .expect("l'etape « Notarize DMG (macOS) » a disparu de release.yml");
    // L'etape s'arrete au prochain element de la liste, au meme niveau.
    let reste = &source[debut + 1..];
    let fin = reste
        .find("\n      - ")
        .map(|i| debut + 1 + i)
        .unwrap_or(source.len());
    let etape = &source[debut..fin];

    assert!(
        etape.contains("stapler staple"),
        "l'etape de notarisation ne tamponne plus le DMG"
    );
    assert!(
        etape.contains("for essai in"),
        "`stapler staple` n'est plus dans une boucle de reprises : une course \
         de quelques secondes chez Apple recommencera a couter le DMG macOS a \
         chaque release.\n\
         L'etape lue :\n{etape}"
    );
    // Une boucle qui n'attend pas entre deux essais ne sert a rien : le ticket
    // met quelques secondes a apparaitre, pas quelques microsecondes.
    assert!(
        etape.contains("sleep"),
        "la boucle de reprises n'attend pas entre deux essais"
    );
}
