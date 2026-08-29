//! Une coupure reseau pendant `notarytool submit` ne doit plus couter le DMG.
//!
//! v0.9.102, job `Build x86_64-apple-darwin`, etape `Notarize DMG (macOS)` :
//!
//! ```text
//! 21:50:06  Submission ID received / id: c9ca5f14-...
//! 21:50:10  Successfully uploaded file
//! 21:50:10  Waiting for processing to complete. Wait timeout is set to 600.0 second(s).
//! 21:50:15  Error: ... Code=-1009 "The Internet connection appears to be offline."
//! 21:50:15  ##[error]Notarisation refusée par Apple — le DMG ne sera pas publié.
//! ```
//!
//! Onze secondes. Apple n'avait rien refuse — interroge apres coup, le service
//! rendait `status: Accepted`. Ce qui a casse, c'est la liaison du runner
//! PENDANT l'attente du verdict. Mais `submit --wait || notarize_failed`
//! declenchait sur n'importe quel code non nul : le DMG Intel a ete supprime,
//! la v0.9.102 est sortie sans lui, et le message accusait Apple.
//!
//! Les reprises de #2329 entourent `stapler`, donc s'executent APRES ce bloc :
//! un `exit 1` parti de la ne les atteint jamais. C'est le constat de #2330.
//!
//! Ce test ne relit pas le YAML a la recherche de mots-cles : il EXECUTE le
//! script de l'etape, tel qu'il est ecrit dans `release.yml`, contre un faux
//! `xcrun` qui rejoue les sorties reelles d'Apple. Sur le code d'avant #2330,
//! `une_coupure_reseau_sur_une_soumission_acceptee_ne_detruit_plus_le_dmg`
//! repond ROUGE — DMG absent, code 1, « REFUSÉE par Apple ».
//!
//! Restreint aux systemes POSIX : l'etape n'est jouee que sur macOS, et le
//! banc a besoin de `bash`.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

/// Faux `xcrun`, et `sleep` neutralise pour que le banc reste instantane.
///
/// Les sorties reproduites viennent du log du run 32666762013 pour la coupure,
/// et du format standard de `notarytool` pour le reste. Trois variables
/// pilotent la scene : `FAUX_SUBMIT`, `FAUX_INFO`, `FAUX_STAPLE`.
const FAUX_XCRUN: &str = r##"
FAUX_ID="c9ca5f14-9896-4df2-b0f8-f0dd98b0bd6c"
sleep() { :; }
xcrun() {
  case "$1 $2" in
    "notarytool submit")
      case "$FAUX_SUBMIT" in
        accepte)
          printf 'Conducting pre-submission checks\nSubmission ID received\n  id: %s\nSuccessfully uploaded file\nWaiting for processing to complete.\nProcessing complete\n  id: %s\n  status: Accepted\n' "$FAUX_ID" "$FAUX_ID"
          return 0 ;;
        coupure)
          printf 'Conducting pre-submission checks\nSubmission ID received\n  id: %s\nSuccessfully uploaded file\nWaiting for processing to complete. Wait timeout is set to 600.0 second(s).\nError: HTTPError(statusCode: nil, error: Error Domain=NSURLErrorDomain Code=-1009 "The Internet connection appears to be offline.")\n' "$FAUX_ID"
          return 1 ;;
        coupure_avant_identifiant)
          printf 'Conducting pre-submission checks\nError: Error Domain=NSURLErrorDomain Code=-1009 "The Internet connection appears to be offline."\n'
          return 1 ;;
      esac ;;
    "notarytool info")
      case "$FAUX_INFO" in
        accepte)     printf 'Successfully received submission info\n  createdDate: 2026-08-23T21:50:07Z\n  id: %s\n  name: tune-server-banc.dmg\n  status: Accepted\n' "$FAUX_ID"; return 0 ;;
        invalide)    printf 'Successfully received submission info\n  id: %s\n  status: Invalid\n' "$FAUX_ID"; return 0 ;;
        injoignable) echo 'Error: Error Domain=NSURLErrorDomain Code=-1009' >&2; return 1 ;;
      esac ;;
    "stapler staple")
      if [ "$FAUX_STAPLE" = ok ]; then echo "The staple and validate action worked!"; return 0; fi
      echo "could not find ticket" >&2
      return 65 ;;
  esac
  echo "BANC : appel xcrun non prevu : $*" >&2
  return 127
}
"##;

fn release_yml() -> String {
    let chemin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows/release.yml");
    fs::read_to_string(&chemin).unwrap_or_else(|e| panic!("{} illisible : {e}", chemin.display()))
}

/// Le corps `run:` de l'etape de notarisation, desindente et debarrasse de la
/// seule expression GitHub qu'il porte.
///
/// Volontairement strict : si l'etape est renommee ou restructuree, le banc
/// s'arrete au lieu de tester un script vide et de repondre vert.
fn script_de_letape() -> String {
    let source = release_yml();
    let debut = source
        .find("- name: Notarize DMG (macOS)")
        .expect("l'etape « Notarize DMG (macOS) » a disparu de release.yml");
    let entete = "\n        run: |\n";
    let apres = source[debut..]
        .find(entete)
        .map(|i| debut + i + entete.len())
        .expect("l'etape de notarisation n'a plus de bloc `run: |`");

    let mut corps = String::new();
    for ligne in source[apres..].lines() {
        if ligne.trim().is_empty() {
            corps.push('\n');
            continue;
        }
        // Le corps d'un `run: |` d'etape vit a dix espaces. La premiere ligne
        // moins indentee marque la cle suivante.
        let Some(reste) = ligne.strip_prefix("          ") else {
            break;
        };
        corps.push_str(reste);
        corps.push('\n');
    }

    assert!(
        corps.contains("xcrun notarytool submit"),
        "extraction du script de notarisation cassee — corps lu :\n{corps}"
    );
    corps.replace("${{ env.ARTIFACT }}", "tune-server-banc")
}

struct Resultat {
    code: i32,
    sortie: String,
    dmg_present: bool,
}

impl Resultat {
    fn contient(&self, extrait: &str) -> bool {
        self.sortie.contains(extrait)
    }
}

/// Joue l'etape reelle contre le faux `xcrun`, dans un bac a sable jetable.
fn jouer(submit: &str, info: &str, staple: &str) -> Resultat {
    let bac = tempfile::tempdir().expect("bac a sable");
    let racine = bac.path().to_path_buf();
    let runner_temp = racine.join("runner-temp");
    fs::create_dir_all(&runner_temp).expect("RUNNER_TEMP");

    let dmg = racine.join("tune-server-banc.dmg");
    fs::write(&dmg, b"faux DMG").expect("DMG de banc");

    let script = racine.join("etape.sh");
    fs::write(&script, format!("{FAUX_XCRUN}\n{}", script_de_letape())).expect("script de banc");

    // GitHub lance les `run:` sous `bash -e {0}` : c'est exactement ce que la
    // course sur `stapler` de #2329 exploitait, donc le banc doit le refaire.
    let sortie = Command::new("bash")
        .arg("-e")
        .arg(&script)
        .current_dir(&racine)
        .env("RUNNER_TEMP", &runner_temp)
        // Chemin par cle API : c'est celui qui a tourne sur la v0.9.102
        // (`ASC_API_KEY_P8` renseigne, mot de passe applicatif vide).
        .env("ASC_API_KEY_P8", "-----BEGIN PRIVATE KEY-----\nfaux\n")
        .env("ASC_API_KEY_ID", "FAUXKEYID")
        .env("ASC_API_ISSUER_ID", "faux-issuer")
        .env("APPLE_ID", "")
        .env("APPLE_APP_SPECIFIC_PASSWORD", "")
        .env("APPLE_TEAM_ID", "")
        .env("FAUX_SUBMIT", submit)
        .env("FAUX_INFO", info)
        .env("FAUX_STAPLE", staple)
        .output()
        .expect("bash introuvable — le banc a besoin d'un shell POSIX");

    let mut texte = String::from_utf8_lossy(&sortie.stdout).into_owned();
    texte.push_str(&String::from_utf8_lossy(&sortie.stderr));

    Resultat {
        code: sortie.status.code().unwrap_or(-1),
        sortie: texte,
        // Lu AVANT que `bac` ne soit detruit avec son arborescence.
        dmg_present: dmg.exists(),
    }
}

/// LE defaut de #2330, rejoue a l'identique.
///
/// Coupure reseau pendant `--wait`, alors qu'Apple a accepte. Attendu : le
/// verdict est relu, le DMG survit, l'etape passe. Sur le code d'avant, le DMG
/// disparait et l'etape sort en 1 sur « REFUSÉE par Apple ».
#[test]
fn une_coupure_reseau_sur_une_soumission_acceptee_ne_detruit_plus_le_dmg() {
    let r = jouer("coupure", "accepte", "ok");

    assert!(
        r.dmg_present,
        "le DMG a ete supprime alors qu'Apple avait ACCEPTÉ la soumission — \
         c'est l'incident v0.9.102, a l'identique.\nSortie :\n{}",
        r.sortie
    );
    assert_eq!(
        r.code, 0,
        "l'etape a echoue alors que le DMG etait notarise et agrafable.\nSortie :\n{}",
        r.sortie
    );
    assert!(
        !r.contient("REFUSÉE par Apple"),
        "l'etape accuse encore Apple d'un refus qu'il n'a pas prononce.\nSortie :\n{}",
        r.sortie
    );
    assert!(
        r.contient("Ticket agrafé"),
        "le rattrapage n'a pas repris jusqu'au tamponnage.\nSortie :\n{}",
        r.sortie
    );
}

/// L'assouplissement ne doit rien assouplir du tout cote securite.
#[test]
fn un_refus_dapple_detruit_toujours_le_dmg_et_le_dit() {
    let r = jouer("coupure", "invalide", "ok");

    assert!(
        !r.dmg_present,
        "un DMG REFUSÉ par Apple a survecu — il ne doit jamais sortir.\nSortie :\n{}",
        r.sortie
    );
    assert_eq!(r.code, 1, "un refus doit faire echouer l'etape");
    assert!(
        r.contient("REFUSÉE par Apple") && r.contient("Invalid"),
        "le refus n'est pas nomme comme tel, avec le verdict relu.\nSortie :\n{}",
        r.sortie
    );
}

/// « Soit on reessaie, soit on echoue bruyamment en nommant la cause. »
///
/// Ici Apple reste injoignable : impossible de conclure. Le DMG part quand
/// meme — non tamponne, il ne doit pas sortir — mais le message doit dire
/// TRANSPORT, pas REFUS, et livrer l'identifiant de soumission. C'est ce
/// mensonge-la qui, sur la v0.9.102, a envoye chercher la panne du cote des
/// secrets `ASC_API_KEY_*`.
#[test]
fn un_verdict_illisible_echoue_bruyamment_sans_accuser_apple() {
    let r = jouer("coupure", "injoignable", "ok");

    assert_eq!(r.code, 1, "un verdict inconnu doit faire echouer l'etape");
    assert!(
        !r.dmg_present,
        "un DMG au verdict inconnu ne doit pas etre publie"
    );
    assert!(
        !r.contient("REFUSÉE par Apple"),
        "l'etape impute a Apple un refus qu'elle n'a pas constate.\nSortie :\n{}",
        r.sortie
    );
    assert!(
        r.contient("INDÉTERMINÉE"),
        "l'etape n'annonce pas que le verdict est indetermine.\nSortie :\n{}",
        r.sortie
    );
    assert!(
        r.contient("c9ca5f14-9896-4df2-b0f8-f0dd98b0bd6c"),
        "le message ne donne pas l'identifiant de soumission, seul point de \
         reprise pour un humain.\nSortie :\n{}",
        r.sortie
    );
    // La relecture doit vraiment INSISTER : une seule tentative rendrait le
    // rattrapage inutile, la coupure etant transitoire par nature.
    assert!(
        r.sortie.matches("nouvel essai dans").count() >= 4,
        "l'etat de la soumission n'est pas redemande assez souvent.\nSortie :\n{}",
        r.sortie
    );
}

/// Coupure si precoce qu'aucun identifiant n'a ete recu : il n'y a rien a
/// interroger, et l'etape ne doit pas pretendre le contraire.
#[test]
fn un_echec_avant_lidentifiant_ne_pretend_pas_interroger_apple() {
    let r = jouer("coupure_avant_identifiant", "injoignable", "ok");

    assert_eq!(r.code, 1);
    assert!(!r.dmg_present);
    assert!(
        r.contient("AVANT d'obtenir un identifiant de soumission"),
        "l'etape ne dit pas qu'Apple n'a jamais vu ce DMG.\nSortie :\n{}",
        r.sortie
    );
    assert!(
        !r.contient("REFUSÉE par Apple"),
        "encore un refus impute a Apple sans verdict.\nSortie :\n{}",
        r.sortie
    );
}

/// Le chemin nominal — celui de toutes les releases qui marchent — ne doit pas
/// avoir bouge d'un pouce.
#[test]
fn la_notarisation_nominale_reste_inchangee() {
    let r = jouer("accepte", "accepte", "ok");

    assert_eq!(r.code, 0, "la notarisation nominale echoue.\n{}", r.sortie);
    assert!(r.dmg_present, "le DMG nominal a disparu.\n{}", r.sortie);
    assert!(r.contient("Ticket agrafé"), "{}", r.sortie);
    assert!(
        !r.contient("::error::"),
        "une release saine emet une erreur.\n{}",
        r.sortie
    );
}

/// Le garde-fou de #2329 reste en place : un DMG accepte mais jamais agrafe ne
/// sort pas, apres cinq essais reels.
#[test]
fn un_ticket_jamais_publie_par_apple_retient_toujours_le_dmg() {
    let r = jouer("accepte", "accepte", "ko");

    assert_eq!(r.code, 1);
    assert!(
        !r.dmg_present,
        "un DMG non tamponne a ete laisse publiable — Gatekeeper le refuserait \
         chez l'utilisateur.\nSortie :\n{}",
        r.sortie
    );
    assert!(
        r.contient("ACCEPTÉE par Apple, mais ticket non agrafé"),
        "{}",
        r.sortie
    );
}
