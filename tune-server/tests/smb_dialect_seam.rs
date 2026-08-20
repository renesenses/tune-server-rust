//! Garde-fou : les DEUX sites de montage SMB partagent la meme echelle de
//! dialectes.
//!
//! Ils ne la partageaient pas. La route interactive (`routes/network.rs`) avait
//! appris a negocier — negociation libre, puis 2.0, puis 1.0 — pendant que le
//! remontage au demarrage (`startup.rs`) imposait toujours `vers=3.0`, recopie
//! d'une version anterieure. La divergence etait invisible a la lecture : deux
//! fichiers eloignes, deux commentaires plausibles, aucune erreur de
//! compilation.
//!
//! Ce que ca coutait, chez Philippe Landes (Tune OS 0.9.81, disque expose par
//! un streamer Rose qui ne parle que SMB 1.0) : l'assistant montait son
//! partage, il retrouvait sa bibliotheque, et **le premier redemarrage la lui
//! reprenait**. L'interface continuait pourtant d'afficher le partage comme
//! monte, et la lecture rendait une erreur reseau qui ne nommait jamais la
//! cause (#1834, #1916).
//!
//! Un test fonctionnel ne peut pas garder ce contrat : il faudrait un serveur
//! SMB 1.0 en CI. Ce test lit donc les SOURCES — sur le modele
//! d'`output_provider_seam.rs`, ecrit apres #1676 pour la meme raison : une
//! couture que rien ne relie mecaniquement doit etre gardee explicitement.

use std::fs;
use std::path::PathBuf;

fn source(chemin: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(chemin);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("lecture de {} : {e}", p.display()))
}

/// `vers=3.0` en dur, ou toute autre version figee, est exactement le defaut
/// corrige. Qu'il reparaisse par un copier-coller doit faire echouer la suite.
#[test]
fn aucun_dialecte_smb_n_est_code_en_dur() {
    for fichier in ["src/startup.rs", "src/routes/network.rs"] {
        let texte = source(fichier);
        for (n, ligne) in texte.lines().enumerate() {
            // Les commentaires NOMMENT le defaut corrige — « ce code imposait
            // encore `vers=3.0` » — et raconter l'histoire ne doit pas faire
            // echouer la suite. Seul le code compte.
            if ligne.trim_start().starts_with("//") {
                continue;
            }
            // `crate::smb` construit l'option par `,vers={v}` a partir de
            // l'echelle : c'est le seul `vers=` legitime, et il est variable.
            let brut = ligne.contains("vers=") && !ligne.contains("vers={v}");
            assert!(
                !brut,
                "{fichier}:{} impose un dialecte SMB en dur :\n    {}\n\
                 L'echelle vit dans tune-server/src/smb.rs — les deux sites de \
                 montage doivent l'utiliser, sans quoi un partage monte par \
                 l'assistant se perd au redemarrage (#1834).",
                n + 1,
                ligne.trim()
            );
        }
    }
}

/// Les deux sites doivent passer par le module commun. Un site qui
/// reimplementerait sa propre liste rouvrirait la divergence sans qu'aucun
/// `vers=` en dur ne le trahisse.
#[test]
fn les_deux_sites_de_montage_utilisent_l_echelle_commune() {
    for (fichier, attendu) in [
        ("src/startup.rs", "crate::smb::echelle"),
        ("src/routes/network.rs", "smb::DIALECTES"),
    ] {
        let texte = source(fichier);
        assert!(
            texte.contains(attendu),
            "{fichier} n'utilise plus `{attendu}` : l'echelle de dialectes a ete \
             recopiee ou contournee. C'est precisement la divergence de #1834."
        );
    }
}

/// Le garde-fou « deja monte » testait la PRESENCE DE FICHIERS. Un point de
/// montage non monte mais portant des residus faisait donc sauter le remontage
/// sans un mot, et l'utilisateur se retrouvait avec une bibliotheque a moitie
/// lisible que rien n'expliquait (#1916).
#[test]
fn le_garde_fou_deja_monte_ne_se_fie_pas_au_contenu_du_repertoire() {
    let texte = source("src/startup.rs");
    // Le corps de la fonction seul : `read_dir` a des usages parfaitement
    // legitimes ailleurs dans ce fichier, et une recherche globale rendrait ce
    // test faux au premier d'entre eux.
    let corps = texte
        .split_once("pub async fn remount_network_shares")
        .map(|(_, apres)| apres)
        .expect("remount_network_shares a disparu ou a ete renommee")
        .split_once("\n}\n")
        .map(|(corps, _)| corps)
        .expect("fin de remount_network_shares introuvable");

    assert!(
        !corps.contains("read_dir"),
        "remount_network_shares deduit a nouveau « c'est monte » du contenu du \
         repertoire. Un point de montage tombe mais portant des residus fait \
         alors sauter le remontage sans un mot — utiliser \
         smb::est_un_point_de_montage (#1916)."
    );
    assert!(
        corps.contains("est_un_point_de_montage"),
        "remount_network_shares ne verifie plus s'il s'agit reellement d'un \
         point de montage."
    );
    // L'echec doit etre ECRIT, pas seulement journalise : c'est tout l'objet de
    // #1916. Un remontage qui echoue en silence laisse l'interface afficher le
    // partage comme monte, et la lecture rendre une erreur reseau generique.
    assert!(
        corps.contains("noter_montage"),
        "remount_network_shares n'ecrit plus le constat du montage : l'echec \
         redevient invisible (#1916)."
    );
}

/// Le mot de passe est concatene dans les options passees a `mount.cifs`. Une
/// trace qui les journalise le publierait dans `journalctl`, lisible par tout
/// utilisateur autorise a lire le journal — et joint a chaque rapport de bogue.
#[test]
fn les_options_de_montage_ne_partent_jamais_au_journal() {
    for fichier in ["src/startup.rs", "src/routes/network.rs"] {
        let texte = source(fichier);
        for (n, ligne) in texte.lines().enumerate() {
            if ligne.trim_start().starts_with("//") {
                continue;
            }
            let trace = ligne.contains("info!(") || ligne.contains("warn!(");
            let fuite = ligne.contains("opts") || ligne.contains("password");
            assert!(
                !(trace && fuite),
                "{fichier}:{} journalise les options de montage, qui portent le \
                 mot de passe :\n    {}",
                n + 1,
                ligne.trim()
            );
        }
    }
}
