//! Contre-epreuve du garde-fou « version publiee sans note de version » (#2328).
//!
//! Un workflow ne se teste pas avec `cargo test` — mais le sien tient dans un
//! script, et un script s'execute. Ces tests lancent
//! `.github/scripts/notes-de-version-watch.sh` sous `bash`, avec un faux `gh` et
//! un faux `curl` en tete de PATH, et verifient ce qu'il conclut. Meme methode
//! que `notarisation_bornes.rs`.
//!
//! Ce qui est reellement prouve ici : la LOGIQUE de rapprochement (fenetre,
//! delai de grace, brouillons, bornes de numero de version, forum injoignable).
//! Ce qui ne l'est PAS : que GitHub Actions declenche bien le cron, que le
//! secret `FORUM_TOKEN` soit valide, ni que l'API forum reponde le meme JSON
//! demain. Ces trois-la ne se verifient qu'en production.
//!
//! Le test qui compte est `un_numero_voisin_ne_vaut_pas_annonce` : c'est le seul
//! qui separe ce garde-fou d'un `grep` naif. Remplacer les bornes du motif par
//! une simple recherche de sous-chaine, dans le script, le fait passer au ROUGE
//! — et lui seul.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn racine_depot() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("le crate a un parent")
        .to_path_buf()
}

fn lire(chemin: &str) -> String {
    let complet = racine_depot().join(chemin);
    fs::read_to_string(&complet).unwrap_or_else(|e| panic!("{complet:?} illisible : {e}"))
}

/// Ce que le script a repondu.
struct Verdict {
    code: i32,
    sortie: String,
}

impl Verdict {
    fn signale(&self, tag: &str) -> bool {
        self.sortie.contains(tag)
    }
}

/// Monte un bac a sable : faux `gh`, faux `curl`, et les deux fixtures JSON.
///
/// Le faux `curl` respecte la seule forme que le script utilise : `-o FICHIER`
/// pour le corps, `-w '%{http_code}'` pour le code sur la sortie standard.
fn executer(releases_json: &str, fils_json: &str, http: &str, env: &[(&str, &str)]) -> Verdict {
    let bac_temporaire = tempfile::Builder::new()
        .prefix("i2328-")
        .tempdir()
        .expect("bac a sable temporaire unique");
    let bac = bac_temporaire.path();
    let outils = bac.join("outils");
    fs::create_dir_all(&outils).expect("bac a sable");

    fs::write(bac.join("releases.json"), releases_json).unwrap();
    fs::write(bac.join("fils.json"), fils_json).unwrap();

    let faux_gh = format!(
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = \"release\" ]; then cat {bac}/releases.json; exit 0; fi\n\
         if [ \"$1\" = \"issue\" ]; then echo \"GH_ISSUE $*\"; exit 0; fi\n\
         exit 0\n",
        bac = bac.display()
    );
    let faux_curl = format!(
        "#!/usr/bin/env bash\n\
         cible=\"\"\n\
         while [ $# -gt 0 ]; do\n\
           case \"$1\" in -o) cible=\"$2\"; shift 2;; *) shift;; esac\n\
         done\n\
         [ -n \"$cible\" ] && cp {bac}/fils.json \"$cible\"\n\
         printf '%s' '{http}'\n",
        bac = bac.display(),
        http = http
    );

    for (nom, corps) in [("gh", faux_gh), ("curl", faux_curl)] {
        let chemin = outils.join(nom);
        fs::write(&chemin, corps).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&chemin, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    let mut commande = Command::new("bash");
    commande
        .arg(racine_depot().join(".github/scripts/notes-de-version-watch.sh"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                outils.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GITHUB_REPOSITORY", "renesenses/tune-server-rust")
        .env("FORUM_TOKEN", "jeton-de-test")
        .env("SANS_ISSUE", "1");
    for (cle, valeur) in env {
        commande.env(cle, valeur);
    }

    let issue = commande.output().expect("le script s'execute");

    Verdict {
        code: issue.status.code().unwrap_or(-1),
        sortie: format!(
            "{}{}",
            String::from_utf8_lossy(&issue.stdout),
            String::from_utf8_lossy(&issue.stderr)
        ),
    }
}

/// Une release publiee, telle que `gh release list --json` la rend.
fn publiee(tag: &str, quand: &str) -> String {
    format!(r#"{{"tagName":"{tag}","isDraft":false,"isPrerelease":false,"publishedAt":"{quand}"}}"#)
}

/// Un fil de notes, tel que `GET /api/v1/forum/threads` le rend.
///
/// `created_at` volontairement tres ancien : la plupart des scenarios veulent
/// une page qui couvre toute la fenetre. Ceux qui testent le contraire posent
/// leur propre date.
fn fil(titre: &str) -> String {
    format!(
        r#"{{"id":1,"type":"release","title":"{titre}","is_pinned":false,"created_at":"2026-08-01T00:00:00+02:00"}}"#
    )
}

fn fils(entrees: &[String]) -> String {
    format!(r#"{{"threads":[{}]}}"#, entrees.join(","))
}

const MAINTENANT: &str = "2026-08-27T12:00:00Z";

fn a_midi() -> Vec<(&'static str, &'static str)> {
    vec![("MAINTENANT_ISO", MAINTENANT)]
}

#[test]
fn une_version_publiee_sans_fil_est_signalee() {
    let v = executer(
        &format!("[{}]", publiee("v0.9.115", "2026-08-27T06:00:00Z")),
        &fils(&[fil("Tune v0.9.114 — Notes de version")]),
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 1,
        "une lacune doit faire echouer la sonde :\n{}",
        v.sortie
    );
    assert!(
        v.signale("v0.9.115"),
        "la version manquante doit etre nommee :\n{}",
        v.sortie
    );
}

#[test]
fn une_version_annoncee_ne_declenche_rien() {
    let v = executer(
        &format!("[{}]", publiee("v0.9.115", "2026-08-27T06:00:00Z")),
        &fils(&[fil("Tune v0.9.115 — Notes de version")]),
        "200",
        &a_midi(),
    );
    assert_eq!(v.code, 0, "rien a signaler ici :\n{}", v.sortie);
}

/// LE test. Un `grep` sans bornes prend « v0.9.101 » pour l'annonce de la
/// « v0.9.10 » et laisse passer la lacune en silence.
#[test]
fn un_numero_voisin_ne_vaut_pas_annonce() {
    let v = executer(
        &format!("[{}]", publiee("v0.9.10", "2026-08-27T06:00:00Z")),
        &fils(&[fil("Tune v0.9.101 — Notes de version")]),
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 1,
        "v0.9.101 n'annonce pas la v0.9.10 — bornes de numero perdues :\n{}",
        v.sortie
    );
    assert!(v.signale("v0.9.10"), "{}", v.sortie);
}

/// L'inverse du precedent : les bornes ne doivent pas couper les fils groupes,
/// qui sont la regle des jours denses (fil 1542, « v0.9.103 et v0.9.104 »).
#[test]
fn un_fil_groupe_couvre_chacune_de_ses_versions() {
    let v = executer(
        &format!(
            "[{},{}]",
            publiee("v0.9.103", "2026-08-27T04:00:00Z"),
            publiee("v0.9.104", "2026-08-27T05:00:00Z")
        ),
        &fils(&[fil("Tune v0.9.103 et v0.9.104 — Notes de version")]),
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 0,
        "un fil groupe annonce bien les deux versions :\n{}",
        v.sortie
    );
}

#[test]
fn le_delai_de_grace_laisse_le_temps_d_ecrire() {
    // Publiee il y a 20 minutes : les huit dernieres versions ont ete annoncees
    // en moins de 22 minutes. Crier ici serait crier a chaque release.
    let v = executer(
        &format!("[{}]", publiee("v0.9.115", "2026-08-27T11:40:00Z")),
        &fils(&[]),
        "200",
        &a_midi(),
    );
    assert_eq!(v.code, 0, "trop tot pour accuser :\n{}", v.sortie);
}

#[test]
fn passe_le_delai_de_grace_la_meme_version_est_signalee() {
    // Meme scenario que ci-dessus, quatre heures plus tard. C'est bien le delai
    // qui protege, pas une incapacite a voir la version.
    let v = executer(
        &format!("[{}]", publiee("v0.9.115", "2026-08-27T08:00:00Z")),
        &fils(&[]),
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 1,
        "quatre heures sans note, il faut crier :\n{}",
        v.sortie
    );
    assert!(v.signale("v0.9.115"), "{}", v.sortie);
}

#[test]
fn un_brouillon_n_est_pas_une_version_publiee() {
    // Cas reel : la v0.9.105 est restee en brouillon, sans `publishedAt`. Elle
    // n'a jamais eu de fil, et c'est correct.
    let v = executer(
        r#"[{"tagName":"v0.9.105","isDraft":true,"isPrerelease":false,"publishedAt":"0001-01-01T00:00:00Z"}]"#,
        &fils(&[]),
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 0,
        "un brouillon n'a rien a annoncer :\n{}",
        v.sortie
    );
}

#[test]
fn au_dela_de_la_fenetre_on_ne_remue_pas_le_passe() {
    let v = executer(
        &format!("[{}]", publiee("v0.9.40", "2026-06-01T06:00:00Z")),
        &fils(&[]),
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 0,
        "la sonde regarde 72 h en arriere, pas trois mois :\n{}",
        v.sortie
    );
}

/// Un forum injoignable est une panne — elle a sa propre sonde. L'imputer aux
/// notes de version produirait une fausse alerte a chaque incident reseau.
#[test]
fn un_forum_injoignable_n_accuse_personne() {
    let v = executer(
        &format!("[{}]", publiee("v0.9.115", "2026-08-27T06:00:00Z")),
        &fils(&[]),
        "502",
        &a_midi(),
    );
    assert_eq!(
        v.code, 0,
        "pas de conclusion sur un forum muet :\n{}",
        v.sortie
    );
    assert!(
        v.sortie.contains("injoignable"),
        "le silence doit rester visible dans le journal :\n{}",
        v.sortie
    );
}

/// Un fil qui n'est pas de type `release` n'annonce rien : un rapport de bug
/// intitule « v0.9.115 plante » ne doit pas eteindre la sonde.
#[test]
fn seuls_les_fils_de_type_release_comptent() {
    let v = executer(
        &format!("[{}]", publiee("v0.9.115", "2026-08-27T06:00:00Z")),
        r#"{"threads":[{"id":1,"type":"bug","title":"Tune v0.9.115 plante au demarrage"}]}"#,
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 1,
        "un fil de bug n'est pas une note de version :\n{}",
        v.sortie
    );
}

/// L'API rend UNE PAGE, pas l'histoire du forum. Au-dela du fil non epingle le
/// plus ancien, l'absence d'annonce peut n'etre que l'absence de la page.
///
/// Ce n'est pas theorique : avec la pagination par defaut (50), la page ne
/// redescend qu'a trois jours et laisse tomber le fil 1533 — celui qui annonce
/// les v0.9.98, .99 et .101. Sans ce garde-fou, la sonde accusait trois
/// versions parfaitement annoncees.
#[test]
fn une_page_trop_courte_ne_condamne_pas_ce_qu_elle_ne_voit_pas() {
    let page_courte = r#"{"threads":[
      {"id":9,"type":"release","title":"Tune v0.9.115 — Notes de version","is_pinned":false,"created_at":"2026-08-27T08:00:00+02:00"},
      {"id":7,"type":"discussion","title":"Epingle tres ancien","is_pinned":true,"created_at":"2026-05-20T13:33:38+02:00"}
    ]}"#;
    let v = executer(
        &format!("[{}]", publiee("v0.9.98", "2026-08-25T06:00:00Z")),
        page_courte,
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 0,
        "la v0.9.98 est hors de ce que la page couvre — s'en taire, pas l'accuser :\n{}",
        v.sortie
    );
    assert!(
        v.sortie.contains("redescend"),
        "la portee reduite doit rester visible dans le journal :\n{}",
        v.sortie
    );
}

/// Corollaire du precedent : un fil epingle de mai ne doit pas faire croire que
/// la page couvre trois mois.
#[test]
fn un_fil_epingle_ne_gonfle_pas_la_couverture() {
    let page = r#"{"threads":[
      {"id":7,"type":"discussion","title":"Epingle de mai","is_pinned":true,"created_at":"2026-05-20T13:33:38+02:00"},
      {"id":9,"type":"release","title":"Tune v0.9.115 — Notes de version","is_pinned":false,"created_at":"2026-08-27T08:00:00+02:00"}
    ]}"#;
    let v = executer(
        &format!("[{}]", publiee("v0.9.90", "2026-08-25T06:00:00Z")),
        page,
        "200",
        &a_midi(),
    );
    assert_eq!(
        v.code, 0,
        "l'epingle de mai ne prouve pas que la page voit le 25/08 :\n{}",
        v.sortie
    );
}

// --- Amarrage : les fichiers disent-ils bien ce que les tests supposent ? -----

#[test]
fn le_workflow_de_veille_appelle_le_script_teste() {
    let workflow = lire(".github/workflows/notes-de-version-watch.yml");
    assert!(
        workflow.contains("bash .github/scripts/notes-de-version-watch.sh"),
        "la sonde testee n'est pas celle que le workflow lance"
    );
    assert!(
        workflow.contains("FORUM_TOKEN: ${{ secrets.FORUM_TOKEN }}"),
        "sans jeton, la sonde conclura « injoignable » a chaque tour"
    );
    assert!(
        workflow.contains("issues: write"),
        "la sonde ouvre une issue : sans ce droit elle echouerait en silence"
    );
    assert!(
        workflow.contains("schedule:") && workflow.contains("cron:"),
        "une sonde sans cron ne surveille rien"
    );
    assert!(
        workflow.contains("timeout-minutes:"),
        "tout job doit porter un plafond de duree (cf workflows_bornes)"
    );
}

/// Garde-fou sur le garde-fou : le job `forum` de `release.yml` doit rester
/// eteint, et la raison doit rester ecrite a cote. Le rallumer en l'etat
/// posterait publiquement des liens vers le depot prive.
#[test]
fn le_job_forum_de_release_yml_reste_eteint_et_motive() {
    let release = lire(".github/workflows/release.yml");
    let debut = release
        .find("  forum:\n")
        .expect("le job `forum` existe toujours dans release.yml");
    let entete = &release[debut..(debut + 400).min(release.len())];
    assert!(
        entete.contains("if: false"),
        "le job forum a ete rallume — lire #2328 avant, il poste des liens vers le depot prive"
    );
    assert!(
        release.contains("#2328"),
        "la raison de l'extinction doit rester lisible a cote du `if: false`"
    );
}
