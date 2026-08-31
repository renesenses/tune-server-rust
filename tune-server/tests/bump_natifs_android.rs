//! Garde-fou : `scripts/bump-all.sh --clients` ne peut pas bumper Flutter sans
//! reconstruire les bibliotheques natives Android.
//!
//! Les trois `libtuneserver.so` sont versionnes dans tune-server-flutter, et le
//! bump ne les reconstruisait pas. Mesure sur cinq versions d'affilee — 0.9.81,
//! 0.9.85, 0.9.89, 0.9.90, 0.9.91 — le commit de bump laissait la CI rouge sur
//! le job « Bibliotheques natives a jour », et il fallait un commit separe de
//! reconstruction pour la repasser au vert : une heure de compilation croisee a
//! la main par version. Une fois, personne n'a vu la derive et les testeurs ont
//! tourne trois semaines et demie sur un moteur du 21 juillet.
//!
//! Le garde-fou cote Flutter refusait deja, mais APRES coup. Ces tests tiennent
//! le maillon d'avant : le script de bump appelle la reconstruction, et quand
//! elle echoue il REMET `pubspec.yaml` comme il etait. Un remaniement qui
//! retirerait ce couplage les fait echouer.
//!
//! `bump-all.sh` lit sa racine dans `TUNE_DEV_DIR`, ce qui permet de le lancer
//! contre un faux `~/DEV` : aucun vrai depot n'est touche, et rien n'est
//! compile — `build-android.sh` est remplace par un leurre qui laisse une trace.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("racine du depot")
        .join("scripts/bump-all.sh")
}

fn ecrire(chemin: &Path, contenu: &str) {
    fs::create_dir_all(chemin.parent().unwrap()).unwrap();
    fs::write(chemin, contenu).unwrap();
}

fn rendre_executable(chemin: &Path) {
    let statut = Command::new("chmod")
        .arg("+x")
        .arg(chemin)
        .status()
        .unwrap();
    assert!(statut.success(), "chmod +x {}", chemin.display());
}

/// Un faux `~/DEV` avec les quatre depots que `bump-all.sh` connait.
///
/// `version_pubspec` et `version_manifeste` sont dissociables a dessein : c'est
/// exactement la derive que le pre-vol doit refuser.
struct FauxDev {
    racine: PathBuf,
}

impl FauxDev {
    fn nouveau(nom: &str, version_pubspec: &str, version_manifeste: &str) -> Self {
        let racine = tune_core::test_scratch::scratch_dir(&format!("tune-bump-natifs-{nom}"));
        let _ = fs::remove_dir_all(&racine);

        let rust = racine.join("tune-server-rust");
        ecrire(
            &rust.join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.9.90\"\n",
        );

        ecrire(
            &racine.join("tune-web-client/package.json"),
            "{\n  \"name\": \"tune-web-client\",\n  \"version\": \"0.9.90\"\n}\n",
        );

        let flutter = racine.join("tune-server-flutter");
        ecrire(
            &flutter.join("pubspec.yaml"),
            &format!("name: tune_server\nversion: {version_pubspec}+502\n"),
        );
        ecrire(
            &flutter.join("android/app/src/main/jniLibs/tune-native.manifest"),
            &format!("version={version_manifeste}\n"),
        );

        // Doublure du garde-fou Flutter. Le vrai vit dans l'autre depot et y est
        // teste ; ici seul son CONTRAT compte — code 0 si les versions
        // s'accordent, 1 sinon — car c'est tout ce dont `bump-all.sh` depend.
        let garde = flutter.join("scripts/check-native-libs.sh");
        ecrire(
            &garde,
            r#"#!/usr/bin/env bash
set -euo pipefail
RACINE="$(cd "$(dirname "$0")/.." && pwd)"
MANIFESTE="$RACINE/android/app/src/main/jniLibs/tune-native.manifest"
VOULUE="$(grep -E '^version:' "$RACINE/pubspec.yaml" | head -1 | sed -e 's/^version:[[:space:]]*//' -e 's/+.*$//')"
if [ "${1:-}" = "--update" ]; then
    echo "version=$VOULUE" > "$MANIFESTE"
    exit 0
fi
PRESENTE="$(grep -E '^version=' "$MANIFESTE" | head -1 | cut -d= -f2)"
[ "$PRESENTE" = "$VOULUE" ] || { echo "derive: $PRESENTE != $VOULUE" >&2; exit 1; }
"#,
        );
        rendre_executable(&garde);

        ecrire(
            &racine.join("tune-server-ipados/Tune/project.yml"),
            IPAD_YML,
        );

        Self { racine }
    }

    /// Leurre de `build-android.sh` : il ecrit une trace, met a jour le
    /// manifeste comme le fait le vrai script, et rend le code demande.
    fn avec_leurre_build(self, code_sortie: i32) -> Self {
        let leurre = self
            .racine
            .join("tune-server-rust/tune-ffi/build-android.sh");
        ecrire(
            &leurre,
            &format!(
                r#"#!/usr/bin/env bash
set -euo pipefail
RACINE="$(cd "$(dirname "$0")/../.." && pwd)"
echo "$*" > "$RACINE/leurre-build-android.trace"
if [ {code_sortie} -eq 0 ]; then
    "$RACINE/tune-server-flutter/scripts/check-native-libs.sh" --update
fi
exit {code_sortie}
"#
            ),
        );
        rendre_executable(&leurre);
        self
    }

    fn lancer(&self, arguments: &[&str]) -> Output {
        self.lancer_avec_identite(arguments, "Jean-Philippe ROBBE", "jp@robbe.net")
    }

    fn lancer_avec_identite(&self, arguments: &[&str], nom: &str, email: &str) -> Output {
        Command::new("bash")
            .arg(script())
            .args(arguments)
            .env("TUNE_DEV_DIR", &self.racine)
            // Le runner GitHub n'a volontairement aucune identite globale. Le
            // faux releaseur doit donc en fournir une, comme une vraie machine
            // de release ; les tests de #2781 peuvent ensuite la corrompre.
            .env("GIT_AUTHOR_NAME", nom)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", nom)
            .env("GIT_COMMITTER_EMAIL", email)
            // `cargo fmt` / `cargo update` sur le faux depot ne servent a rien
            // et le script les tolere deja ; PATH ampute pour ne pas les lancer.
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .output()
            .expect("lancement de bump-all.sh")
    }

    fn pubspec(&self) -> String {
        fs::read_to_string(self.racine.join("tune-server-flutter/pubspec.yaml")).unwrap()
    }

    fn cargo(&self) -> String {
        fs::read_to_string(self.racine.join("tune-server-rust/Cargo.toml")).unwrap()
    }

    fn trace_build(&self) -> Option<String> {
        fs::read_to_string(self.racine.join("leurre-build-android.trace")).ok()
    }
}

impl Drop for FauxDev {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.racine);
    }
}

const IPAD_YML: &str =
    "name: Tune\nsettings:\n  MARKETING_VERSION: \"0.9.90\"\n  CURRENT_PROJECT_VERSION: 90\n";

/// #2781 : l'identite est verifiee avant la premiere reecriture. Le couple qui
/// a fausse les releases 0.9.29 a 0.9.125 doit laisser tous les manifestes
/// byte-for-byte intacts.
#[test]
fn un_couple_nom_email_melange_est_refuse_avant_le_bump() {
    let dev = FauxDev::nouveau("identite-melangee", "0.9.90", "0.9.90").avec_leurre_build(0);
    let cargo_avant = dev.cargo();

    let sortie = dev.lancer_avec_identite(
        &["0.9.91", "--skip-web-drift-check"],
        "Bertrand",
        "jp@robbe.net",
    );
    assert!(!sortie.status.success(), "le couple forge doit etre refuse");
    assert_eq!(dev.cargo(), cargo_avant, "le refus arrive avant le bump");
    assert!(
        String::from_utf8_lossy(&sortie.stdout).contains("mixed Git identity"),
        "le diagnostic doit nommer la vraie cause. stdout: {}",
        String::from_utf8_lossy(&sortie.stdout)
    );
}

/// Le cas nominal : le bump entraine la reconstruction, dans la meme execution.
#[test]
fn un_bump_clients_reconstruit_les_natifs() {
    let dev = FauxDev::nouveau("nominal", "0.9.90", "0.9.90").avec_leurre_build(0);

    let sortie = dev.lancer(&["0.9.91", "--clients", "--skip-web-drift-check"]);
    assert!(
        sortie.status.success(),
        "le bump aurait du reussir.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sortie.stdout),
        String::from_utf8_lossy(&sortie.stderr),
    );

    assert_eq!(
        dev.trace_build().as_deref(),
        Some("--release\n"),
        "le bump doit appeler build-android.sh --release : sans cela il \
         reproduit exactement les cinq bumps rouges"
    );
    assert!(dev.pubspec().contains("version: 0.9.91+503"));
}

/// Le coeur du sujet : quand la reconstruction echoue, le bump Flutter ne
/// survit pas. C'est ce qui rend bump et reconstruction indissociables.
#[test]
fn une_reconstruction_ratee_annule_le_bump_flutter() {
    let dev = FauxDev::nouveau("echec", "0.9.90", "0.9.90").avec_leurre_build(1);
    let avant = dev.pubspec();

    let sortie = dev.lancer(&["0.9.91", "--clients", "--skip-web-drift-check"]);
    assert!(
        !sortie.status.success(),
        "une reconstruction ratee doit faire echouer le bump"
    );
    assert_eq!(
        dev.pubspec(),
        avant,
        "pubspec.yaml doit etre RESTAURE : un pubspec bumpe avec les .so de la \
         version precedente est precisement l'etat casse qu'on supprime"
    );
}

/// Derive heritee : les `.so` en place ne correspondent deja plus a la version
/// courante. Bumper par-dessus enterrerait la preuve une version plus loin.
#[test]
fn un_bump_refuse_de_partir_sur_des_natifs_deja_perimes() {
    let dev = FauxDev::nouveau("herite", "0.9.90", "0.9.89").avec_leurre_build(0);
    let cargo_avant = dev.cargo();

    let sortie = dev.lancer(&["0.9.91", "--clients", "--skip-web-drift-check"]);
    assert!(
        !sortie.status.success(),
        "le pre-vol doit refuser une derive heritee"
    );
    assert!(
        dev.trace_build().is_none(),
        "rien ne doit etre compile avant d'avoir tranche la derive heritee"
    );
    assert_eq!(
        dev.cargo(),
        cargo_avant,
        "le refus doit arriver AVANT toute reecriture : sinon le depot Rust \
         reste bumpe pour un bump qui n'a pas eu lieu"
    );
}

/// La paire de release (serveur + web) ne doit pas payer le prix du chantier
/// Android : sans `--clients`, aucune compilation croisee, aucun garde-fou.
#[test]
fn un_bump_sans_clients_ignore_completement_android() {
    let dev = FauxDev::nouveau("serveur-seul", "0.9.90", "0.9.89").avec_leurre_build(0);

    let sortie = dev.lancer(&["0.9.91", "--skip-web-drift-check"]);
    assert!(
        sortie.status.success(),
        "une derive Android ne doit pas bloquer un bump serveur+web.\nstderr: {}",
        String::from_utf8_lossy(&sortie.stderr),
    );
    assert!(dev.trace_build().is_none());
    assert!(dev.cargo().contains("version = \"0.9.91\""));
}

/// L'echappatoire reste possible, mais elle doit s'annoncer : c'est elle qui
/// remet le depot dans l'etat que le hook et la CI refuseront.
#[test]
fn lechappatoire_dit_ce_quelle_laisse_derriere_elle() {
    let dev = FauxDev::nouveau("echappatoire", "0.9.90", "0.9.90").avec_leurre_build(0);

    let sortie = dev.lancer(&[
        "0.9.91",
        "--clients",
        "--skip-web-drift-check",
        "--skip-android-rebuild",
    ]);
    assert!(sortie.status.success());
    assert!(
        dev.trace_build().is_none(),
        "--skip-android-rebuild ne doit rien compiler"
    );

    let stderr = String::from_utf8_lossy(&sortie.stderr);
    assert!(
        stderr.contains("pre-commit") && stderr.contains("build-android.sh"),
        "l'echappatoire doit nommer le verrou suivant et la commande a lancer, \
         sinon elle rouvre silencieusement la porte.\nstderr: {stderr}"
    );
}

/// Le script doit rester utilisable meme si l'on retire par megarde le
/// garde-fou du depot Flutter : mieux vaut refuser que bumper a l'aveugle.
#[test]
fn sans_garde_fou_flutter_le_bump_clients_refuse() {
    let dev = FauxDev::nouveau("sans-garde", "0.9.90", "0.9.90").avec_leurre_build(0);
    fs::remove_file(
        dev.racine
            .join("tune-server-flutter/scripts/check-native-libs.sh"),
    )
    .unwrap();

    let sortie = dev.lancer(&["0.9.91", "--clients", "--skip-web-drift-check"]);
    assert!(
        !sortie.status.success(),
        "sans garde-fou, rien ne dit de quelle version viennent les .so"
    );
}
