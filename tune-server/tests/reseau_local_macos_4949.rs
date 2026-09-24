//! #4949 — « Tune Server.app » doit se déclarer auprès de macOS comme
//! utilisateur du réseau local.
//!
//! Depuis macOS 15, une app qui touche au réseau local doit y être autorisée
//! dans Réglages Système → Confidentialité et sécurité → Réseau local. Une app
//! dont le `Info.plist` ne porte ni `NSLocalNetworkUsageDescription` ni
//! `NSBonjourServices` n'y apparaît pas : la permission ne peut pas être
//! accordée, et chaque connexion vers le LAN tombe en `No route to host (os
//! error 65)` alors que le même Mac joint les appareils par `ping`.
//!
//! Terrain : Cyrille Moutia, 24/09/2026, Yamaha R-N2000A en `192.168.1.12`,
//! « Tune n'existe pas » dans la liste (dossier #4580 depuis la 0.9.158 : six
//! hôtes DLNA injoignables).
//!
//! Le `Info.plist` n'existe nulle part dans le dépôt : il est écrit par un
//! heredoc de `.github/workflows/release.yml`. Ce test relit ce heredoc tel
//! quel, puis vérifie :
//!
//! * que la description d'usage est présente et non vide ;
//! * que CHAQUE type Bonjour employé par le code du serveur (`tune-core/src`,
//!   `tune-server/src`) figure dans `NSBonjourServices` — un type ajouté au
//!   code sans l'être au plist serait bloqué par macOS sans un mot.
//!
//! Sur macOS, le plist rendu passe en plus par `plutil -lint`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn racine() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("racine du dépôt")
        .to_path_buf()
}

/// Le corps du heredoc qui écrit `Contents/Info.plist`, désindenté.
fn info_plist_du_workflow() -> String {
    let yml = fs::read_to_string(racine().join(".github/workflows/release.yml"))
        .expect("release.yml lisible");
    let ouverture = "cat > \"$APP/Contents/Info.plist\" << PLIST";
    let debut = yml
        .find(ouverture)
        .expect("release.yml n'écrit plus Contents/Info.plist par heredoc");
    let mut corps = Vec::new();
    for ligne in yml[debut..].lines().skip(1) {
        if ligne.trim() == "PLIST" {
            return corps.join("\n");
        }
        corps.push(ligne.trim_start());
    }
    panic!("heredoc Info.plist non refermé dans release.yml");
}

/// La valeur `<string>` qui suit immédiatement `<key>{cle}</key>`.
fn chaine_apres_cle(plist: &str, cle: &str) -> Option<String> {
    let marque = format!("<key>{cle}</key>");
    let apres = &plist[plist.find(&marque)? + marque.len()..];
    let apres = apres.trim_start().strip_prefix("<string>")?;
    Some(apres[..apres.find("</string>")?].to_string())
}

/// Les `<string>` du `<array>` qui suit `<key>NSBonjourServices</key>`.
fn services_declares(plist: &str) -> BTreeSet<String> {
    let marque = "<key>NSBonjourServices</key>";
    let Some(pos) = plist.find(marque) else {
        return BTreeSet::new();
    };
    let apres = &plist[pos + marque.len()..];
    let tableau = &apres[..apres.find("</array>").unwrap_or(0)];
    tableau
        .split("<string>")
        .skip(1)
        .filter_map(|s| s.split("</string>").next())
        .map(|s| s.trim().to_string())
        .collect()
}

/// Chaque `_nom._tcp` / `_nom._udp` écrit dans les sources Rust du serveur.
fn services_du_code() -> BTreeSet<String> {
    let mut trouves = BTreeSet::new();
    for dossier in ["tune-core/src", "tune-server/src"] {
        parcourir(&racine().join(dossier), &mut trouves);
    }
    trouves
}

fn parcourir(dossier: &Path, trouves: &mut BTreeSet<String>) {
    for entree in fs::read_dir(dossier).expect("dossier source lisible") {
        let chemin = entree.expect("entrée lisible").path();
        if chemin.is_dir() {
            parcourir(&chemin, trouves);
        } else if chemin.extension().is_some_and(|e| e == "rs") {
            let texte = fs::read_to_string(&chemin).unwrap_or_default();
            extraire_services(&texte, trouves);
        }
    }
}

fn extraire_services(texte: &str, trouves: &mut BTreeSet<String>) {
    let octets = texte.as_bytes();
    for (i, _) in texte.match_indices("._") {
        let suite = &texte[i + 2..];
        let proto = if suite.starts_with("tcp") {
            "tcp"
        } else if suite.starts_with("udp") {
            "udp"
        } else {
            continue;
        };
        let mut debut = i;
        while debut > 0 {
            let c = octets[debut - 1];
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' {
                debut -= 1;
            } else {
                break;
            }
        }
        if debut == i || debut == 0 || octets[debut - 1] != b'_' {
            continue;
        }
        trouves.insert(format!("_{}._{proto}", &texte[debut..i]));
    }
}

#[test]
fn le_plist_de_l_app_declare_l_usage_du_reseau_local() {
    let plist = info_plist_du_workflow();
    let description =
        chaine_apres_cle(&plist, "NSLocalNetworkUsageDescription").unwrap_or_default();
    assert!(
        !description.trim().is_empty(),
        "Info.plist de Tune Server.app sans NSLocalNetworkUsageDescription : macOS 15+ \
         n'inscrit pas l'app dans « Réseau local », tout le LAN tombe en os error 65 (#4949)"
    );
}

#[test]
fn chaque_type_bonjour_du_serveur_est_declare_dans_le_plist() {
    let plist = info_plist_du_workflow();
    let declares = services_declares(&plist);
    let employes = services_du_code();
    assert!(
        employes.contains("_raop._tcp") && employes.contains("_googlecast._tcp"),
        "le relevé des types Bonjour du code ne trouve plus rien : l'extraction est cassée \
         ({employes:?})"
    );
    let manquants: Vec<_> = employes.difference(&declares).collect();
    assert!(
        manquants.is_empty(),
        "types Bonjour employés par le serveur mais absents de NSBonjourServices dans \
         release.yml — macOS les bloquera sans un mot (#4949) : {manquants:?}"
    );
}

#[test]
fn l_extraction_reconnait_les_formes_du_code() {
    let mut t = BTreeSet::new();
    extraire_services(
        r#"const A: &str = "_raop._tcp.local."; // _sendspin-server._tcp
           let b = format!("{}._tcp", x); let c = "_smb._udp";"#,
        &mut t,
    );
    let attendu: BTreeSet<String> = ["_raop._tcp", "_sendspin-server._tcp", "_smb._udp"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(t, attendu);
}

#[cfg(target_os = "macos")]
#[test]
fn le_plist_rendu_est_valide_pour_macos() {
    let plist = info_plist_du_workflow().replace("${VERSION}", "0.0.0");
    let fichier = tune_core::test_scratch::scratch_file("info-plist-4949", ".plist");
    fs::write(fichier.path(), plist).expect("écriture du plist rendu");
    let sortie = std::process::Command::new("plutil")
        .arg("-lint")
        .arg(fichier.path())
        .output()
        .expect("plutil présent sur macOS");
    assert!(
        sortie.status.success(),
        "plutil -lint refuse l'Info.plist de Tune Server.app : {}",
        String::from_utf8_lossy(&sortie.stdout)
    );
}
