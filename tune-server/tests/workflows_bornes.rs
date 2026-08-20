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
//! Ce test relit les deux fichiers qui comptent. `ci.yml` garde les fusions
//! ouvertes ; `release.yml` produit ce qui est LIVRE. Les confondre a deja
//! donne un faux vert (#1768) — d'ou la verification des deux.

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
        jobs.len() >= 5,
        "{fichier} : {} jobs trouves — l'analyse est cassee, pas le fichier",
        jobs.len()
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
    verifier("ci.yml");
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
