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
    for fichier in [
        "release.yml",
        "docker.yml",
        "trigger-os-images.yml",
        "promote-release.yml",
        // Le paquet Debian est desormais appele par la promotion : ses jobs
        // occupent un runner DANS le train, et aucun ne portait de plafond.
        "deb.yml",
    ] {
        verifier(fichier);
    }
}

#[test]
fn tout_job_de_ci_a_un_plafond() {
    for fichier in [
        "ci.yml",
        "preflight.yml",
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
fn la_release_attend_le_preflight_avant_de_construire() {
    let release = workflow("release.yml");
    let jobs = jobs(&release);
    let corps = |nom: &str| {
        jobs.iter()
            .find(|(candidat, _)| candidat == nom)
            .map(|(_, corps)| corps.as_str())
            .unwrap_or_else(|| panic!("job {nom} absent de release.yml"))
    };

    assert!(
        corps("preflight").contains("uses: ./.github/workflows/preflight.yml"),
        "Release ne reutilise pas le preflight : deux workflows declenches par le tag peuvent diverger"
    );
    assert!(
        corps("web-client")
            .lines()
            .any(|ligne| { cle_de_job(ligne, "needs:") && ligne.trim() == "needs: preflight" }),
        "le premier job de construction peut demarrer sans attendre le preflight"
    );

    let preflight = workflow("preflight.yml");
    assert!(
        preflight.contains("  workflow_call:"),
        "preflight.yml ne peut pas etre appele comme dependance de Release"
    );
    assert!(
        !preflight.contains("  push:\n    tags: [\"v*\"]"),
        "le tag lance encore un second preflight independant et duplique"
    );
}

#[test]
fn le_tag_serveur_est_le_seul_declencheur_et_ne_promeut_rien_directement() {
    let release = workflow("release.yml");
    assert!(release.contains("push:\n    tags: [\"v*\"]"));

    for fichier in ["docker.yml", "trigger-os-images.yml", "changelog.yml"] {
        let source = workflow(fichier);
        assert!(
            !source.contains("push:\n    tags:"),
            "{fichier} publie encore en parallele sur le push du tag"
        );
    }

    let jobs = jobs(&release);
    let corps = |nom: &str| {
        jobs.iter()
            .find(|(candidat, _)| candidat == nom)
            .map(|(_, corps)| corps.as_str())
            .unwrap_or_else(|| panic!("job {nom} absent de release.yml"))
    };
    assert!(corps("stage-docker").contains("uses: ./.github/workflows/docker.yml"));
    assert!(corps("stage-os").contains("uses: ./.github/workflows/trigger-os-images.yml"));
    assert!(corps("staging-complete").contains("needs: [publish, stage-docker, stage-os]"));
    assert!(corps("publish").contains("gh release view"));
    assert!(!corps("publish").contains("--draft=false"));
}

#[test]
fn docker_est_construit_une_fois_en_staging_puis_promu_par_digest() {
    let docker = workflow("docker.yml");
    assert!(docker.contains("push: true"));
    assert!(docker.contains("staging-${{ steps.train.outputs.version }}"));
    assert!(!docker.contains("format('{0}:latest'"));

    let promotion = workflow("promote-release.yml");
    assert!(promotion.contains("docker buildx imagetools inspect"));
    assert!(promotion.contains("docker buildx imagetools create"));
    assert!(promotion.contains("renesenses/tune:staging-$TAG"));
    assert!(promotion.contains("--tag renesenses/tune:latest"));
}

#[test]
fn tune_os_recoit_version_sha_source_et_checksums_immuables() {
    let os = workflow("trigger-os-images.yml");
    for preuve in [
        "server_sha256_x86_64",
        "server_sha256_aarch64",
        "os_sha",
        "os_tag",
        "release OS deja publique",
        "wait_workflow build-iso.yml",
        "wait_workflow build-rpi-image.yml",
        "wait_workflow build-x86-image.yml",
    ] {
        assert!(
            os.contains(preuve),
            "preuve OS absente du workflow: {preuve}"
        );
    }
}

#[test]
fn la_promotion_est_manuelle_armee_et_idempotente() {
    let promotion = workflow("promote-release.yml");
    assert!(promotion.contains("workflow_dispatch:"));
    assert!(!promotion.contains("  push:"));
    assert!(promotion.contains("default: true"));
    assert!(promotion.contains("RELEASE_PROMOTION_ENABLED"));
    assert!(promotion.contains(".ready == true"));
    assert!(promotion.contains("release-dry-run"));
    assert!(promotion.contains("release-promotion"));
    assert!(
        promotion
            .contains("if [ \"$(gh release view \"$TAG\" --json isDraft --jq .isDraft)\" = true ]")
    );
    assert!(promotion.contains("Android inchange : absent du manifeste a quatre composants"));
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
fn la_synchronisation_post_release_est_un_outil_de_reparation_manuel() {
    let garde = workflow("post-release-main-sync.yml");
    assert!(garde.contains("workflow_dispatch:"));
    assert!(!garde.contains("workflow_run:"));
    assert!(garde.contains("pull-requests: write"));
    assert!(garde.contains("scripts/synchroniser-release-main.py --self-test"));

    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = fs::read_to_string(racine.join("../scripts/synchroniser-release-main.py"))
        .expect("garde-fou post-release lisible");
    assert!(script.contains("merge-base"));
    assert!(script.contains("--is-ancestor"));
    assert!(script.contains("\"pr\",\n            \"create\""));
    assert!(script.contains("\"pr\", \"reopen\""));
    assert!(script.contains("refs/heads/{branche}"));
    assert!(script.contains("PR créée sans auto-merge"));
    assert!(!script.contains("git reset"));
    assert!(!script.contains("push --force"));
    assert!(!script.contains("pr merge"));

    let ci = workflow("ci.yml");
    assert!(ci.contains("python3 scripts/synchroniser-release-main.py --self-test"));
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
fn la_voie_rapide_est_reservee_aux_bases_integration() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let profil = fs::read_to_string(racine.join("../scripts/determiner-profil-ci.sh"))
        .expect("scripts/determiner-profil-ci.sh lisible");
    assert!(profil.contains("batch/*|rc/*) printf '%s\\n' rapide"));
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
    assert!(postgres.contains("!startsWith(github.base_ref, 'rc/')"));
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
    // les deux plateformes ne sont differees que pour une base batch/* ou rc/*.
    for nom in ["fmt", "test", "clippy", "audit", "ffi"] {
        assert!(
            !corps(nom).contains("needs.impact.outputs.full"),
            "job du noyau {nom} differe a tort jusqu'a l'integration du lot"
        );
    }

    for nom in ["test", "clippy"] {
        assert!(
            corps(nom)
                .contains("-p tune-core -p tune-http-types -p tune-smart-http -p tune-stream-http -p tune-streaming-http -p tune-server"),
            "job {nom} : les crates HTTP extraites ne sont plus testees explicitement"
        );
    }
    for nom in ["test-shipped-features", "audio-embedding"] {
        assert!(
            corps(nom).contains("needs.impact.outputs.full == 'true'"),
            "suite complete {nom} encore lancee sur chaque correctif du lot"
        );
    }
    // `windows-pr` et `macos-pr` ont QUITTE cette liste (#3123) : voir
    // `les_deux_plateformes_compilent_sur_toute_pr_rust`, qui exige l'inverse.

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

#[test]
fn les_alias_linux_stables_sont_crees_avant_les_sommes() {
    let release = workflow("release.yml");
    let aliases = release
        .find("bash scripts/creer-alias-actifs-release.sh artifacts")
        .expect("release.yml ne cree plus les alias Linux stables");
    let sommes = release
        .find("- name: Checksums")
        .expect("l'etape SHA256SUMS a disparu de release.yml");

    assert!(
        aliases < sommes,
        "les alias sont crees apres SHA256SUMS et ne sont donc pas signes"
    );
    assert!(release.contains("artifacts/**/*.tar.gz"));
}

#[test]
fn le_script_des_alias_release_passe_ses_contre_epreuves() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = racine.join("../scripts/creer-alias-actifs-release.sh");
    let sortie = std::process::Command::new("bash")
        .arg(&script)
        .arg("--autotest")
        .output()
        .expect("impossible d'executer l'autotest des alias de release");

    assert!(
        sortie.status.success(),
        "autotest des alias en echec:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&sortie.stdout),
        String::from_utf8_lossy(&sortie.stderr)
    );
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

/// Le job `test` de `ci.yml` doit NOMMER `bandcamp` dans son `--features`.
///
/// Mesure : run **33702848850**, job `Test`, PR #3257. La ligne y etait
/// `--no-default-features --features oaat,cloud-relay`, et
/// `tune-server/tests/bandcamp_file_de_zone_i2702.rs` — cinq essais de #2702
/// et #2778 — se compilait SANS le service qu'il eprouve : registre a cinq
/// services, cinq rouges, aucun defaut. Le meme jeu de features rendait la
/// meme suite rouge sous `Test (PostgreSQL)`.
///
/// Le fichier porte desormais `#![cfg(feature = "bandcamp")]`, ce qui est la
/// verite : `bandcamp = ["dep:tune-bandcamp"]`, dependance OPTIONNELLE, donc
/// sans la fonctionnalite `BandcampService` n'existe pas. Mais un `cfg` seul
/// aurait remplace cinq rouges par cinq essais INVISIBLES sur toute PR vers
/// `batch/*` — `test-shipped-features`, la seule autre porte qui active
/// `bandcamp` en EXECUTION, est differee jusqu'a `full`.
///
/// Ce garde est donc la moitie qui manque au `cfg` : il refuse qu'on retire
/// la fonctionnalite de la porte qui, elle, tourne sur chaque correctif Rust.
/// Meme role que `toute_feature_declaree_est_activee_par_une_porte_clippy`
/// pour les lints (#2865), applique a l'EXECUTION.
#[test]
fn le_job_test_de_la_ci_active_bandcamp() {
    let ci = workflow("ci.yml");
    let jobs = jobs(&ci);
    let corps = |nom: &str| {
        jobs.iter()
            .find(|(candidat, _)| candidat == nom)
            .map(|(_, corps)| corps.as_str())
            .unwrap_or_else(|| panic!("job {nom} absent de ci.yml"))
    };
    let features = |corps: &str| -> String {
        let ligne = corps
            .lines()
            .find(|l| l.contains("cargo test"))
            .expect("ce job ne lance plus `cargo test`");
        ligne
            .split("--features ")
            .nth(1)
            .expect("ce job ne nomme plus de `--features`")
            .trim()
            .to_owned()
    };

    let test = features(corps("test"));
    assert!(
        test.split(',').any(|f| f.trim() == "bandcamp"),
        "le job `test` de ci.yml n'active plus `bandcamp` : \
         tune-server/tests/bandcamp_file_de_zone_i2702.rs porte \
         `#![cfg(feature = \"bandcamp\")]` et ne serait plus compile — les \
         cinq essais de #2702/#2778 disparaitraient sans un seul rouge.\n  \
         --features {test}"
    );

    // Contre-epreuve du detecteur, dans les deux sens : il doit voir une
    // fonctionnalite ABSENTE de cette porte-ci, et la voir PRESENTE sur la
    // porte du jeu livre. Sans ces deux la, un extracteur casse (qui rendrait
    // toujours la ligne entiere, ou toujours vide) passerait a vide.
    assert!(
        !test.split(',').any(|f| f.trim() == "karaoke"),
        "detecteur casse : il voit `karaoke`, que le job `test` ne porte pas \
         — --features {test}"
    );
    let livre = features(corps("test-shipped-features"));
    assert!(
        livre.split(',').any(|f| f.trim() == "karaoke"),
        "detecteur casse : `karaoke` est bien dans le jeu livre — \
         --features {livre}"
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
                    == "run: cargo test --no-fail-fast -p tune-core -p tune-server --no-default-features --features postgres,oaat"
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

/// Contre-epreuve de #3098 : toute porte `cargo test` va jusqu'au bout.
///
/// Sans `--no-fail-fast`, le PREMIER binaire de test qui echoue arrete la
/// commande : tous les binaires suivants ne sont jamais executes, et la porte
/// n'affiche qu'un echec la ou il y en a peut-etre dix. Mesure sur Shrek le
/// 01/09/2026, jeu de fonctionnalites exact du job `test` (`oaat,cloud-relay`,
/// cinq paquets), avec l'echec IPv6 de `dual_stack_socket_accepts_both_families`
/// au 9e binaire : SANS le drapeau, 9 binaires sur 18 sont executes et les NEUF
/// suivants ne tournent jamais ; AVEC, les 18 tournent, plus quatre lots de
/// doc-tests. Ce qui n'est pas execute ne peut pas etre rouge.
///
/// Le compte minimal en fin de test est delibere : un detecteur qui ne repere
/// plus aucune ligne passerait a vide, exactement le defaut qu'il garde.
#[test]
fn toute_porte_cargo_test_va_jusqu_au_bout() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows");
    let mut fichiers: Vec<_> = fs::read_dir(&racine)
        .expect("dossier des workflows illisible")
        .filter_map(|entree| entree.ok().map(|entree| entree.path()))
        .filter(|chemin| chemin.extension().and_then(|ext| ext.to_str()) == Some("yml"))
        .collect();
    fichiers.sort();

    let mut vues = 0usize;
    for chemin in fichiers {
        let source = fs::read_to_string(&chemin)
            .unwrap_or_else(|e| panic!("{} illisible : {e}", chemin.display()));
        for (numero, ligne) in source.lines().enumerate() {
            let nue = ligne.trim();
            let Some(commande) = nue
                .strip_prefix("- run: ")
                .or_else(|| nue.strip_prefix("run: "))
            else {
                continue;
            };
            if commande != "cargo test" && !commande.starts_with("cargo test ") {
                continue;
            }
            vues += 1;
            assert!(
                commande.contains("--no-fail-fast"),
                "{}:{} lance cargo test sans --no-fail-fast : le premier binaire \
                 en echec emporterait tous les suivants en silence\n  {nue}",
                chemin.display(),
                numero + 1
            );
        }
    }

    assert!(
        vues >= 10,
        "seulement {vues} portes `cargo test` reperees : le detecteur ne voit \
         plus les lignes qu'il doit garder"
    );
}

/// Contre-epreuve de #3123, porte 1 : Windows et macOS compilent sur TOUTE PR
/// qui touche du Rust.
///
/// Ce que la condition `full` a coute, mesure : `rand_core::OsRng` appele depuis
/// `tune-core/src/db/album_repo.rs` (93186f81, #3074) alors que la caisse n'est
/// declaree que sous `[target.'cfg(unix)'.dependencies]`. Une PR vers `batch/*`
/// ne porte pas `full` : le defaut a traverse sa propre PR, le lot ET la RC sans
/// une seule compilation Windows, et n'a rougi qu'a la promotion vers main, ou
/// il a arrete le train de la 0.9.130.
///
/// Le compte minimal est delibere, comme dans le garde de #3098 : un detecteur
/// qui ne repere plus aucun job passerait a vide.
#[test]
fn les_deux_plateformes_compilent_sur_toute_pr_rust() {
    let source = workflow("ci.yml");
    let jobs = jobs(&source);
    let corps = |nom: &str| {
        jobs.iter()
            .find(|(candidat, _)| candidat == nom)
            .map(|(_, corps)| corps.as_str())
            .unwrap_or_else(|| panic!("job {nom} absent de ci.yml"))
    };

    let mut vus = 0usize;
    for nom in ["windows-pr", "macos-pr"] {
        let job = corps(nom);
        vus += 1;
        assert!(
            job.contains("needs.impact.outputs.rust == 'true'"),
            "{nom} ne suit plus le verdict d impact"
        );
        assert!(
            !job.contains("needs.impact.outputs.full"),
            "{nom} est de nouveau reserve aux PR `full` : une PR de lot ne \
             serait plus compilee sur cette plateforme, et c'est exactement \
             comment #3074 a traverse le lot et la RC"
        );
    }
    assert_eq!(
        vus, 2,
        "le detecteur ne voit plus les deux jobs de plateforme qu'il garde"
    );

    // Rien n'est RETIRE : les deux jobs gardent leurs configurations, et
    // `release-gate` continue de les exiger verts pour promouvoir vers main.
    let windows = corps("windows-pr");
    assert!(windows.contains("--features oaat,postgres,dj,karaoke,bandcamp,plugins-wasm"));
    assert!(
        windows
            .contains("--features oaat,local-audio,asio,postgres,dj,karaoke,bandcamp,plugins-wasm")
    );
    assert!(corps("macos-pr").contains("cargo check --package tune-server"));
    let porte = corps("release-gate");
    for nom in ["windows-pr", "macos-pr"] {
        assert!(
            porte.contains(&format!("- {nom}")),
            "release-gate n'exige plus {nom}"
        );
    }
}

/// Contre-epreuve de #3123, porte 2 : PostgreSQL execute aussi `tune-server`,
/// et ne saute plus les PR de lot qui touchent du Rust.
///
/// Ce que le trou a coute, mesure : les deux requetes de « Continuer l'ecoute »
/// (#2441) vivaient dans `tune-server`, que ce workflow ne compilait pas. Elles
/// n'avaient donc jamais tourne sur PostgreSQL. L'une calculait un pourcentage
/// en SQL : `total = 0` rend `NULL` sur SQLite et leve `division by zero` sur
/// PostgreSQL. C'est l'angle mort de #2860, rejoue une release plus tard.
///
/// Compte du 01/09/2026 sur les 100 dernieres executions du workflow : sur 98
/// declenchements de PR, **73 sautes**, 23 reussis, 2 annules.
#[test]
fn postgresql_execute_les_requetes_de_tune_server() {
    let postgres = workflow("test-postgres.yml");

    // a) Les trois clauses de #2808 sont INTACTES — la promotion `rc/* -> main`
    //    sans une ligne de Rust reste couverte — et une quatrieme s'y ajoute.
    assert!(postgres.contains("github.event_name != 'pull_request'"));
    assert!(postgres.contains("!startsWith(github.base_ref, 'batch/')"));
    assert!(postgres.contains("!startsWith(github.base_ref, 'rc/')"));
    assert!(postgres.contains("contains(github.event.pull_request.labels.*.name, 'ci:full')"));
    assert!(
        postgres.contains("|| needs.impact.outputs.rust == 'true'"),
        "une PR de correctif vers batch/* ou rc/* saute encore PostgreSQL en \
         entier, alors que c'est la que le SQL des P2 s'ecrit"
    );
    // Le temoin vert de l'autre cote : la PR qui ne touche aucun Rust ne doit
    // rien declencher de lourd, donc le classifieur reste en place et garde sa
    // propre contre-epreuve.
    assert!(postgres.contains("bash scripts/detecter-impact-ci.sh --autotest"));

    // b) Les paquets reellement exerces sur PostgreSQL, comptes dans le
    //    fichier. Avant #3123 : `tune-core` seul, sur les six etapes.
    let mut lignes = 0usize;
    let mut paquets: Vec<&str> = Vec::new();
    for ligne in postgres.lines() {
        let nue = ligne.trim();
        let Some(commande) = nue.strip_prefix("run: ") else {
            continue;
        };
        if !commande.starts_with("cargo test ") {
            continue;
        }
        lignes += 1;
        let mots: Vec<&str> = commande.split_whitespace().collect();
        for (index, mot) in mots.iter().enumerate() {
            if *mot == "-p" {
                if let Some(paquet) = mots.get(index + 1) {
                    if !paquets.contains(paquet) {
                        paquets.push(paquet);
                    }
                }
            }
        }
    }
    assert!(
        lignes >= 7,
        "seulement {lignes} etapes `cargo test` reperees dans test-postgres.yml : \
         le detecteur ne voit plus ce qu'il doit compter"
    );
    paquets.sort_unstable();
    assert_eq!(
        paquets,
        ["tune-core", "tune-server"],
        "l'inventaire des paquets joues sur PostgreSQL a change : on n'en retire \
         jamais, et `tune-server` doit y rester — c'est la que vivent les \
         requetes des routes"
    );

    // c) L'etape qui EXECUTE ces requetes, avec une base vivante.
    assert!(
        postgres.contains("--test pg_routes_serveur"),
        "l'epreuve des routes de tune-server sur PostgreSQL a disparu"
    );
    let etape = postgres
        .split("--test pg_routes_serveur")
        .nth(1)
        .expect("etape pg_routes_serveur");
    assert!(
        etape.contains("--test-threads=1"),
        "les TRUNCATE CASCADE de l'epreuve s'interbloquent en parallele"
    );
    assert!(
        postgres.matches("TUNE_TEST_PG_URL: postgresql://").count() >= 6,
        "une etape PostgreSQL a perdu sa base vivante : `pg_or_skip!` la sauterait \
         en silence"
    );

    // Le test lui-meme doit exister et etre DECLARE : `tune-server` porte
    // `autotests = false`, donc un fichier non inscrit ne se compile jamais.
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
        racine.join("tests/pg_routes_serveur.rs").is_file(),
        "tune-server/tests/pg_routes_serveur.rs absent"
    );
    let manifeste =
        fs::read_to_string(racine.join("Cargo.toml")).expect("tune-server/Cargo.toml lisible");
    assert!(
        manifeste.contains("name = \"pg_routes_serveur\""),
        "cible de test pg_routes_serveur non declaree : avec autotests = false, \
         le fichier ne serait JAMAIS compile"
    );
    assert!(
        manifeste.contains("required-features = [\"postgres\"]"),
        "pg_routes_serveur doit exiger la feature postgres"
    );
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

/// Garde-fou : toute feature declaree par le serveur est activee par une porte
/// clippy — la liste des features de `ci.yml` est un INVENTAIRE, pas un
/// echantillon.
///
/// La porte clippy est lancee avec `--no-default-features` et une liste
/// explicite. Une feature absente de cette liste n'est donc lue par AUCUN
/// lint : le code qu'elle garde n'est verifie que quand un humain y pense.
///
/// Mesure du 31/08/2026, avant ce garde-fou (#2865) — quatre features nues :
///
/// | feature | fichiers gardes | lignes du plus gros fichier |
/// |---|---|---|
/// | `local-audio` | 16 | `outputs/local.rs`, 10 826 |
/// | `postgres` | 11 | `db/pg_migrate.rs`, 1 285 |
/// | `audio-embedding` | 3 | `audio/embedding.rs`, 2 199 |
/// | `asio` | 3 | (Windows seulement) |
///
/// L'issue #2865 ne citait que `audio-embedding`. Le trou le plus gros etait
/// ailleurs : `--no-default-features` RETIRE `local-audio`, qui est pourtant
/// dans le `default` des deux crates. Corriger le seul cas cite aurait laisse
/// dix mille lignes nues — d'ou ce test, qui compte au lieu de citer.
///
/// Le test ne credite QUE les features nommees explicitement sur une ligne
/// `cargo clippy`. Une feature amenee par `default` ne compte pas : c'est
/// exactement l'illusion qui a coute Bandcamp en 0.9.82 (#1768), ou une
/// feature posee dans `default` n'atteignait aucun binaire publie.
#[test]
fn toute_feature_declaree_est_activee_par_une_porte_clippy() {
    // Une feature hors porte doit etre INSCRITE ici avec sa raison. La liste
    // se relit ; un oubli, non.
    const HORS_PORTE: &[(&str, &str)] = &[
        (
            "asio",
            "`cpal/asio` ne se compile que sous Windows (SDK Steinberg). Les \
             runners de la porte clippy sont ubuntu-latest. Couverte par le job \
             `windows-pr`, qui la compile en `cargo check`.",
        ),
        (
            "plugin-http",
            "activee EN DUR par la declaration de dependance de tune-server \
             (`tune-core = { …, features = [\"plugin-http\"] }`). Toute porte \
             qui compile tune-server la compile : elle ne peut pas etre nue.",
        ),
    ];

    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));

    // 1. Les features declarees par les deux crates qui portent du code garde.
    let mut declarees: Vec<String> = Vec::new();
    for manifeste in ["Cargo.toml", "../tune-core/Cargo.toml"] {
        let source = fs::read_to_string(racine.join(manifeste))
            .unwrap_or_else(|e| panic!("{manifeste} illisible : {e}"));
        let mut dedans = false;
        for ligne in source.lines() {
            let t = ligne.trim();
            if t.starts_with('[') {
                dedans = t == "[features]";
                continue;
            }
            if !dedans || t.starts_with('#') {
                continue;
            }
            let Some((nom, _)) = t.split_once(" = [") else {
                continue;
            };
            let nom = nom.trim();
            if nom == "default" || nom.is_empty() {
                continue;
            }
            if !declarees.iter().any(|d| d.as_str() == nom) {
                declarees.push(nom.to_string());
            }
        }
    }
    assert!(
        declarees.len() >= 8,
        "le garde-fou n'a reconnu que {} feature(s) — la forme `nom = [\"…\"]` \
         des manifestes a change, et ce test ne garde plus rien : {declarees:?}",
        declarees.len()
    );

    // 2. Les features activees explicitement par une ligne `cargo clippy`.
    let ci = workflow("ci.yml");
    let mut lignes_clippy = 0usize;
    let mut couvertes: Vec<String> = Vec::new();
    for ligne in ci.lines() {
        let t = ligne.trim();
        if !t.contains("cargo clippy") {
            continue;
        }
        lignes_clippy += 1;
        let Some(reste) = t.split("--features").nth(1) else {
            continue;
        };
        let Some(liste) = reste.split_whitespace().next() else {
            continue;
        };
        for f in liste.split(',') {
            let f = f.trim();
            if !f.is_empty() && !couvertes.iter().any(|c| c.as_str() == f) {
                couvertes.push(f.to_string());
            }
        }
    }
    assert!(
        lignes_clippy > 0,
        "aucune ligne `cargo clippy` dans ci.yml — le garde-fou ne garde plus rien"
    );

    // 3. Le verdict.
    let nues: Vec<&str> = declarees
        .iter()
        .map(|f| f.as_str())
        .filter(|f| !couvertes.iter().any(|c| c.as_str() == *f))
        .filter(|f| !HORS_PORTE.iter().any(|(nom, _)| *nom == *f))
        .collect();

    assert!(
        nues.is_empty(),
        "ces features ne sont activees par AUCUNE porte clippy de ci.yml : \
         {nues:?}\n\
         Le code qu'elles gardent n'est lu par aucun lint — il n'est verifie \
         que quand un agent y pense a la main (#2865).\n\
         Deux issues seulement :\n\
           1. les ajouter a la liste `--features` du job `clippy` ;\n\
           2. les inscrire dans HORS_PORTE ci-dessus AVEC leur raison, si la \
         porte ne PEUT pas les compiler (OS, materiel).\n\
         Features couvertes aujourd'hui : {couvertes:?}"
    );

    // Une entree de HORS_PORTE qui ne correspond a aucune feature declaree est
    // une justification perimee : elle rassure sans rien couvrir.
    let perimees: Vec<&str> = HORS_PORTE
        .iter()
        .map(|(nom, _)| *nom)
        .filter(|nom| !declarees.iter().any(|d| d.as_str() == *nom))
        .collect();
    assert!(
        perimees.is_empty(),
        "HORS_PORTE justifie des features qui n'existent plus : {perimees:?} — \
         retirer l'entree plutot que la laisser rassurer"
    );
}

/// Lance le `--autotest` d'un script d'outillage et exige un nombre minimum de
/// garanties.
///
/// ⭐ Un garde qui ne trouve rien doit ECHOUER, pas reussir. Un autotest vide
/// de son contenu sortirait en 0 et passerait ici pour vert : on compte donc
/// les lignes `ok: ` qu'il imprime, comme le fait deja la porte des features.
fn autotest(script: &str, minimum: usize) {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let chemin = racine.join("../scripts").join(script);
    let sortie = std::process::Command::new("bash")
        .arg(&chemin)
        .arg("--autotest")
        .output()
        .unwrap_or_else(|e| panic!("scripts/{script} --autotest injouable : {e}"));
    let flux = String::from_utf8_lossy(&sortie.stdout);
    assert!(
        sortie.status.success(),
        "scripts/{script} --autotest en echec\nstdout:\n{}\nstderr:\n{}",
        flux,
        String::from_utf8_lossy(&sortie.stderr)
    );
    let garanties = flux.lines().filter(|l| l.starts_with("ok: ")).count();
    assert!(
        garanties >= minimum,
        "scripts/{script} --autotest n'a verifie que {garanties} garantie(s) au lieu de \
         {minimum} : l'autotest s'est vide, il ne garde plus rien.\nstdout:\n{flux}"
    );
}

/// La relecture d'un tag juste cree ne distinguait pas « absent » de « pas
/// encore visible ».
///
/// Run 33522674458, publication de la v0.9.130 : le tag venait d'etre cree sur
/// `tune-web-client`, la relecture immediate a rendu « neant », et le
/// controleur s'est arrete AVANT `universal`, `os` et `server`. Le tag
/// existait — relu trente secondes plus tard, sur le bon SHA. Un tag orphelin
/// dans un depot, trois depots sans tag, un train a reprendre a la main.
///
/// L'intention de la garde est juste et ne doit pas etre affaiblie : ce test
/// exige la reprise ET le refus immediat d'un tag divergent.
#[test]
fn le_controleur_relit_le_tag_avec_reprise_sans_desarmer_la_garde() {
    autotest("relire-tag-avec-reprise.sh", 8);

    let controleur = workflow("release-controller.yml");
    assert!(
        controleur.contains("source scripts/relire-tag-avec-reprise.sh"),
        "le controleur ne charge plus la reprise de relecture"
    );
    assert!(
        controleur.contains(r#"relu="$(relire_tag_avec_reprise "$sha" cible_tag "$repo" "$tag")""#),
        "la relecture d'apres-creation ne passe plus par la reprise"
    );
    assert!(
        !controleur.contains(r#"relu="$(cible_tag "$repo" "$tag")""#),
        "la relecture immediate SANS reprise est de retour — c'est elle qui a \
         coupe le train de la v0.9.130 (run 33522674458)"
    );

    // L'echec reste DUR des deux cotes : introuvable au bout des tentatives,
    // et tag divergent.
    assert!(
        controleur.contains("n'est pas verifiable apres creation"),
        "l'echec dur apres relecture a disparu"
    );
    assert!(
        controleur.contains("pointe sur $existant au lieu de $sha"),
        "le refus d'un tag deja pose ailleurs a disparu"
    );

    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = fs::read_to_string(racine.join("../scripts/relire-tag-avec-reprise.sh"))
        .expect("scripts/relire-tag-avec-reprise.sh lisible");
    // Un plafond, et une attente qui reste de l'ordre de la seconde : l'echec
    // doit rester rapide. Sans plafond, un tag reellement absent ferait tourner
    // le controleur jusqu'a la borne du job.
    assert!(script.contains("RELIRE_TAG_ESSAIS:-5"));
    assert!(script.contains("RELIRE_TAG_PAUSE:-1"));
    assert!(
        script.contains("return 3"),
        "le verdict immediat sur un tag divergent a disparu du script"
    );
}

/// L'envoi des `.deb` echouait systematiquement, APRES avoir reussi.
///
/// `gh release upload "$TAG" dist/*.deb dist/SHA256SUMS.deb --clobber` nommait
/// le meme actif deux fois — `dist/*.deb` couvre deja `dist/SHA256SUMS.deb` —
/// et l'ordre alphabetique du glob l'envoyait EN PREMIER, avant les paquets
/// qu'il annonce. D'ou le `HTTP 404` du run 33536592140, et surtout l'etat
/// qu'il laissait : au premier passage de la v0.9.130, `amd64` manquait alors
/// que SHA256SUMS.deb, publie, le listait.
#[test]
fn les_deb_partent_un_par_un_les_empreintes_en_dernier_et_l_inventaire_tranche() {
    autotest("attacher-deb-release.sh", 10);

    let deb = workflow("deb.yml");
    assert!(
        !deb.contains(r#"gh release upload "$TAG" dist/*.deb dist/SHA256SUMS.deb"#),
        "l'envoi en lot est de retour : `dist/*.deb` couvre deja \
         `dist/SHA256SUMS.deb`, le meme actif est nomme deux fois et `--clobber` \
         rend HTTP 404 (run 33536592140)"
    );

    let jobs = jobs(&deb);
    let corps = |nom: &str| {
        jobs.iter()
            .find(|(candidat, _)| candidat == nom)
            .map(|(_, corps)| corps.as_str())
            .unwrap_or_else(|| panic!("job {nom} absent de deb.yml"))
    };
    let publication = corps("publish");
    assert!(
        publication.contains(r#"bash scripts/attacher-deb-release.sh "$TAG" dist"#),
        "le job d'envoi n'appelle plus le script qui pose les actifs un par un"
    );
    assert!(
        publication.contains("uses: actions/checkout@v4"),
        "sans checkout, scripts/attacher-deb-release.sh n'existe pas dans ce job"
    );
    // La seule chose que la condition gardait — ne rien publier depuis une PR —
    // reste gardee.
    assert!(
        publication.contains("github.event_name != 'pull_request' && inputs.publish"),
        "la condition d'envoi ne protege plus les PR, ou ne couvre plus l'appel \
         par la promotion"
    );
}

/// Le `.deb` n'est jamais parti tout seul : `deb.yml` ecoute
/// `release: [published]`, et GitHub ne declenche aucun workflow depuis un
/// evenement produit avec le `GITHUB_TOKEN` par defaut (anti-recursion). Or
/// c'est ce jeton qui publie la release dans `promote-release.yml`. Mesure :
/// aucun run `release` dans tout l'historique de deb.yml, jamais.
///
/// Un `uses:` ne passe par aucun evenement — c'est le meme run qui continue.
#[test]
fn la_promotion_emporte_le_paquet_debian_dans_son_propre_run() {
    let deb = workflow("deb.yml");
    assert!(
        deb.contains("  workflow_call:"),
        "deb.yml n'est pas appelable : la promotion ne peut pas emporter le paquet"
    );
    // Le declencheur mort est CONSERVE : il tire encore si un humain publie la
    // release depuis l'interface web. On ne retire pas une porte, on en ajoute.
    assert!(
        deb.contains("  release:\n    types: [published]"),
        "le declencheur `release` a ete retire au lieu d'etre double"
    );

    let promotion = workflow("promote-release.yml");
    let jobs = jobs(&promotion);
    let paquet = jobs
        .iter()
        .find(|(nom, _)| nom == "deb")
        .map(|(_, corps)| corps.as_str())
        .expect("promote-release.yml ne lance plus le paquet Debian");
    assert!(paquet.contains("uses: ./.github/workflows/deb.yml"));
    assert!(
        paquet.contains("needs: promote"),
        "le paquet serait construit avant que la release ne soit publique"
    );
    assert!(paquet.contains("tag: v${{ inputs.version }}"));
    assert!(paquet.contains("publish: true"));
    assert!(
        paquet.contains("if: ${{ !inputs.dry_run }}"),
        "un dry-run de promotion attacherait un paquet pour de vrai"
    );
}

/// Extrait les paquets qu'une ligne `cargo test` selectionne.
///
/// Rend `None` si la ligne n'est pas une porte `cargo test`. Rend un vecteur
/// VIDE si la ligne en est une mais ne nomme aucun paquet — c'est le cas de
/// `widget-ci.yml`, qui lance `cargo test` nu dans un AUTRE workspace
/// (`tune-widget/src-tauri`, `exclude` du notre) : elle ne peut rien couvrir
/// ici, et surtout elle ne doit pas passer pour un `--workspace`.
fn paquets_selectionnes(ligne: &str) -> Option<(Vec<String>, bool)> {
    let nue = ligne.trim();
    let commande = nue
        .strip_prefix("- run: ")
        .or_else(|| nue.strip_prefix("run: "))?;
    if commande != "cargo test" && !commande.starts_with("cargo test ") {
        return None;
    }

    let mut paquets = Vec::new();
    let mut tout = false;
    let mut mots = commande.split_whitespace();
    while let Some(mot) = mots.next() {
        match mot {
            // `--all` est l'ancien nom de `--workspace` ; cargo l'accepte
            // encore, et une porte qui l'emploierait couvrirait tout autant.
            "--workspace" | "--all" => tout = true,
            "-p" | "--package" => {
                if let Some(nom) = mots.next() {
                    paquets.push(nom.to_string());
                }
            }
            _ => {
                for prefixe in ["-p=", "--package="] {
                    if let Some(nom) = mot.strip_prefix(prefixe) {
                        paquets.push(nom.to_string());
                    }
                }
            }
        }
    }
    Some((paquets, tout))
}

/// 🔴 Contre-epreuve de #3266 : tout membre du workspace est EXECUTE par une
/// porte `cargo test` de la CI.
///
/// Les portes de la CI nomment leurs paquets un par un (`-p`). Personne n'y
/// lance `cargo test --workspace`. Un paquet qui n'est nomme NULLE PART n'est
/// donc jamais execute : ses tests ne peuvent ni passer ni echouer, ils
/// n'existent pas pour la CI. Mesure du 03/09/2026, avant ce correctif :
/// `plugins/tune-bandcamp`, `plugins/tune-karaoke`, `tune-plugin-runtime-wasm`
/// et `tune-ffi` etaient dans ce cas — verts sous `cargo test --workspace`,
/// que personne ne joue, et sous rien d'autre.
///
/// Ce que cela coutait precisement : `plugins/tune-bandcamp/src/lib.rs` porte
/// les DEUX gardes de site de #2778 (`aucun_resultat_de_reglage_n_est_jete`,
/// `le_chemin_de_liaison_se_journalise`). Une garde de site existe pour crier
/// quand un motif interdit revient en production ; celles-la ne pouvaient pas
/// crier, faute d'etre executees. Un `let _ = reglages.set(…)` reintroduit
/// serait passe sans un seul rouge.
///
/// Meme famille que #1427 (les tests de greffons derriere une feature jamais
/// activee) et #2865 (les features jamais nommees par une porte clippy). Les
/// deux precedents ont ete refermes par un garde qui COMPTE au lieu de citer ;
/// celui-ci fait de meme, sur l'axe des PAQUETS.
///
/// Le test rend DEUX verdicts, et le second est celui qui mord : « couvert
/// quelque part » laisserait passer un paquet couvert par la seule porte
/// `test-shipped-features`, differee jusqu'a `full` — donc jamais jouee sur une
/// PR vers `batch/*`. C'est exactement le piege de #2702/#2778.
///
/// Sabotage : retirer un `-p` de la ligne `cargo test` du job `test` de
/// `ci.yml` fait tomber ce test en nommant le paquet decouvert.
#[test]
fn tout_membre_du_workspace_est_execute_par_une_porte_cargo_test() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

    // 1. Les membres du workspace, lus dans le manifeste racine. La liste est
    //    un inventaire, pas un echantillon (commentaire de `Cargo.toml`) : on
    //    la relit a chaque passage plutot que d'en recopier une copie ici, qui
    //    vieillirait en silence.
    let manifeste = fs::read_to_string(racine.join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("Cargo.toml racine illisible : {e}"));
    let mut liste = String::new();
    let mut dedans = false;
    for ligne in manifeste.lines() {
        let t = ligne.trim();
        if !dedans {
            let Some(reste) = t.strip_prefix("members") else {
                continue;
            };
            let Some(reste) = reste.trim_start().strip_prefix('=') else {
                continue;
            };
            let Some(reste) = reste.trim_start().strip_prefix('[') else {
                continue;
            };
            dedans = true;
            liste.push_str(reste);
        } else {
            liste.push_str(t);
        }
        if let Some(fin) = liste.find(']') {
            liste.truncate(fin);
            break;
        }
        liste.push(' ');
    }
    assert!(
        dedans,
        "`members = [` introuvable dans le Cargo.toml racine"
    );

    let chemins: Vec<String> = liste
        .split(',')
        .map(|c| c.trim().trim_matches('"').to_string())
        .filter(|c| !c.is_empty())
        .collect();

    // Le nom PUBLIE de chaque membre — c'est lui que `-p` nomme, et il ne se
    // deduit pas du chemin : `plugins/tune-bandcamp` s'appelle `tune-bandcamp`.
    let mut membres: Vec<(String, String)> = Vec::new();
    for chemin in &chemins {
        let sous_manifeste = racine.join(chemin).join("Cargo.toml");
        let source = fs::read_to_string(&sous_manifeste).unwrap_or_else(|e| {
            panic!(
                "membre `{chemin}` declare mais {} illisible : {e}",
                sous_manifeste.display()
            )
        });
        let mut section = "";
        let mut nom = None;
        for ligne in source.lines() {
            let t = ligne.trim();
            if t.starts_with('[') {
                section = if t == "[package]" { "[package]" } else { "" };
                continue;
            }
            if section != "[package]" {
                continue;
            }
            // `name = "tune-cli"` — et surtout pas le `name` du `[[bin]]` qui
            // suit dans le meme fichier et vaut `tune`.
            if let Some(reste) = t.strip_prefix("name") {
                if let Some(reste) = reste.trim_start().strip_prefix('=') {
                    nom = Some(reste.trim().trim_matches('"').to_string());
                    break;
                }
            }
        }
        let nom = nom.unwrap_or_else(|| panic!("`{chemin}/Cargo.toml` ne declare pas de `name`"));
        membres.push((nom, chemin.clone()));
    }

    assert!(
        membres.len() >= 12,
        "le garde-fou n'a reconnu que {} membre(s) : la forme de `members` a \
         change et ce test ne garde plus rien — {membres:?}",
        membres.len()
    );
    // Contre-epreuve du lecteur de membres, sens NEGATIF : les deux
    // applications Tauri sont `exclude` du workspace. Un lecteur qui listerait
    // les dossiers au lieu de lire `members` les ferait apparaitre ici, et le
    // test exigerait une couverture pour des caisses qui ne sont meme pas dans
    // ce workspace.
    for absent in ["tune-desktop", "tune-widget"] {
        assert!(
            !membres.iter().any(|(nom, _)| nom == absent),
            "lecteur de membres casse : `{absent}` est `exclude`, il ne peut \
             pas etre membre — {membres:?}"
        );
    }

    // 2. Les paquets nommes par une ligne `cargo test`, TOUS workflows
    //    confondus : `ci.yml` garde les fusions, `test-postgres.yml` porte la
    //    suite PostgreSQL, et un futur workflow compterait tout autant.
    let dossier = racine.join(".github/workflows");
    let mut fichiers: Vec<_> = fs::read_dir(&dossier)
        .expect("dossier des workflows illisible")
        .filter_map(|entree| entree.ok().map(|entree| entree.path()))
        .filter(|chemin| chemin.extension().and_then(|ext| ext.to_str()) == Some("yml"))
        .collect();
    fichiers.sort();

    let mut portes = 0usize;
    let mut tout_le_workspace = false;
    let mut couverts: Vec<String> = Vec::new();
    for chemin in &fichiers {
        let source = fs::read_to_string(chemin)
            .unwrap_or_else(|e| panic!("{} illisible : {e}", chemin.display()));
        for ligne in source.lines() {
            let Some((paquets, tout)) = paquets_selectionnes(ligne) else {
                continue;
            };
            portes += 1;
            tout_le_workspace |= tout;
            for paquet in paquets {
                if !couverts.contains(&paquet) {
                    couverts.push(paquet);
                }
            }
        }
    }

    assert!(
        portes >= 10,
        "seulement {portes} porte(s) `cargo test` reperee(s) : le detecteur ne \
         voit plus les lignes qu'il doit lire"
    );
    assert!(
        !couverts.is_empty(),
        "aucun `-p` releve sur les portes `cargo test` : l'extracteur est casse"
    );
    // Contre-epreuve de l'extracteur, sens POSITIF : il doit voir un paquet
    // qu'aucune porte ne peut perdre. Sans ce sens-la, un extracteur qui
    // rendrait toujours la liste complete des membres passerait a vide.
    assert!(
        couverts.iter().any(|c| c == "tune-core"),
        "extracteur casse : `tune-core` est nomme par plusieurs portes — {couverts:?}"
    );
    // Contre-epreuve de l'extracteur, sens NEGATIF : il ne doit pas rendre
    // n'importe quel mot de la ligne. `--no-fail-fast` y est partout,
    // `tune-widget` nulle part.
    for intrus in ["--no-fail-fast", "--no-default-features", "tune-widget"] {
        assert!(
            !couverts.iter().any(|c| c == intrus),
            "extracteur casse : il a pris `{intrus}` pour un paquet — {couverts:?}"
        );
    }

    // 3. Premier verdict : couvert quelque part.
    if !tout_le_workspace {
        let nus: Vec<&(String, String)> = membres
            .iter()
            .filter(|(nom, _)| !couverts.iter().any(|c| c == nom))
            .collect();
        assert!(
            nus.is_empty(),
            "ces membres du workspace ne sont nommes par AUCUNE porte `cargo test` \
             de .github/workflows : {nus:?}\n\
             Leurs tests ne sont executes par aucun job : ils ne peuvent ni passer \
             ni echouer, et une garde de site qui y vivrait ne pourrait jamais \
             crier (#3266, meme famille que #1427 et #2865).\n\
             Deux issues seulement :\n\
               1. ajouter `-p <nom>` a une ligne `cargo test` d'un workflow ;\n\
               2. sortir le paquet de `members` dans le Cargo.toml racine, s'il \
             n'a rien a faire dans ce workspace.\n\
             Paquets couverts aujourd'hui : {couverts:?}"
        );
    }

    // 4. Second verdict : couvert par la porte qui tourne sur CHAQUE PR Rust.
    //
    // « Couvert quelque part » ne suffit pas, et c'est la lecon de #2702/#2778 :
    // `test-shipped-features` est differe jusqu'a `full`, donc jamais joue sur
    // une PR vers `batch/*`. Un paquet qui n'y serait couvert QUE la ne verrait
    // aucun de ses essais tourner sur la PR qui le casse — il ne rougirait qu'a
    // la promotion, quand le lot entier est deja construit dessus. Le job
    // `test` de `ci.yml` est la seule porte `cargo test` sans condition `full`.
    let ci = workflow("ci.yml");
    let corps_test = jobs(&ci)
        .into_iter()
        .find(|(nom, _)| nom == "test")
        .map(|(_, corps)| corps)
        .expect("job `test` absent de ci.yml");
    let mut sur_chaque_pr: Vec<String> = Vec::new();
    let mut tout_sur_chaque_pr = false;
    for ligne in corps_test.lines() {
        if let Some((paquets, tout)) = paquets_selectionnes(ligne) {
            tout_sur_chaque_pr |= tout;
            sur_chaque_pr.extend(paquets);
        }
    }
    assert!(
        tout_sur_chaque_pr || !sur_chaque_pr.is_empty(),
        "le job `test` de ci.yml ne selectionne plus aucun paquet — \
         le detecteur ne garde plus rien"
    );
    if tout_sur_chaque_pr {
        return;
    }
    let differes: Vec<&(String, String)> = membres
        .iter()
        .filter(|(nom, _)| !sur_chaque_pr.iter().any(|c| c == nom))
        .collect();
    assert!(
        differes.is_empty(),
        "ces membres ne sont pas nommes par le job `test` de ci.yml : \
         {differes:?}\n\
         Les autres portes `cargo test` sont conditionnees a `full` ou a un \
         autre evenement : leurs essais ne tourneraient donc PAS sur une PR \
         vers `batch/*`, celle qui les casse (#3266, meme piege que #2702).\n\
         Ajouter `-p <nom>` a la ligne `cargo test` du job `test`, ou, si la \
         porte ne PEUT pas le compiler (OS, materiel, dependance lourde), \
         resserrer ce test ici AVEC la raison plutot que le laisser rassurer.\n\
         Paquets du job `test` aujourd'hui : {sur_chaque_pr:?}"
    );
}
