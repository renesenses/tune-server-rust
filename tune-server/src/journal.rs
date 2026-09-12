//! Un journal dont le plafond tient **pendant** que le serveur tourne.
//!
//! ## Ce que le plafond promettait, et ce qu'il tenait
//!
//! `config::rotate_log_file` borne le fichier à 10 Mio — mais il n'est appelé
//! **qu'au démarrage**, avant que le logger n'ouvre le fichier. La limite est
//! assumée telle quelle dans #539 :
//!
//! > *« Rotation au démarrage seulement : une session qui ne redémarre jamais
//! > grossit jusqu'au prochain lancement. »*
//!
//! Le cas non couvert est précisément celui que le commentaire d'appel invoque
//! pour justifier le plafond — *« so it doesn't grow without bound on a
//! long-running server »*. Un serveur qui tourne longtemps est le seul qui
//! puisse dépasser 10 Mio, et c'est le seul que la rotation au démarrage ne
//! protège pas.
//!
//! ## Pourquoi maintenant
//!
//! Levente Toth (#2156) relève **21,7 Mio/s écrits sur le disque, sans lecture
//! en cours**, sur une machine au repos ; un redémarrage y met fin. La cause de
//! ce régime n'est pas établie et ce module ne la cherche pas. Mais si ce qui
//! écrit est le journal, alors le fichier passe le plafond en une demi-seconde
//! et rien, tant que le processus vit, ne l'arrête — et un redémarrage
//! « répare » aussi parce qu'il rotationne enfin.
//!
//! Borner le journal ne diagnostique rien. Cela borne les dégâts, et retire une
//! explication de la liste : si le disque se remplit encore après ce correctif,
//! ce n'est pas le journal.
//!
//! ## Pourquoi un écrivain, et pas une tâche de fond
//!
//! Renommer le fichier depuis un minuteur ne bornerait **rien** sous Unix : le
//! logger garde son descripteur ouvert et continue d'écrire dans l'inode
//! renommé. Le fichier `.1` grossirait à l'infini à la place du courant, et le
//! contrôle de taille sur le chemin courant ne verrait plus jamais rien —
//! l'usage disque serait non borné *et* invisible. Seul celui qui tient le
//! descripteur peut le refermer et en rouvrir un autre.
//!
//! ## Deux pièges, et ce qu'on en fait
//!
//! **Ne jamais journaliser depuis ici.** Un `warn!` émis pendant une écriture du
//! logger repasserait par la couche fichier, dont le `Mutex` est déjà tenu par
//! l'écriture en cours : interblocage. Les incidents de rotation partent sur
//! `eprintln!`, qui ne traverse pas `tracing`.
//!
//! **Un renommage qui échoue ne doit pas être retenté à chaque octet.** Sur un
//! disque plein ou un dossier en lecture seule, `rename` échoue en boucle ; si
//! le seuil restait franchi, chaque ligne déclencherait un appel système de
//! plus, et le journal deviendrait le goulot du serveur au pire moment. Le
//! seuil est donc **repoussé d'un plafond** à chaque échec : on réessaie une
//! fois par tranche, jamais plus.

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;

/// Écrivain de journal qui se rotationne lui-même dès qu'il dépasse le plafond.
///
/// Compte les octets qu'il écrit plutôt que d'interroger le système : le compte
/// est exact, gratuit, et surtout il ne dépend pas d'un `metadata()` par ligne.
pub struct JournalBorne {
    chemin: PathBuf,
    fichier: File,
    /// Octets dans le fichier courant, **y compris ce qu'il contenait déjà à
    /// l'ouverture** : un fichier laissé à 9,9 Mio par la session précédente
    /// doit tourner à la ligne suivante, pas 10 Mio plus tard.
    ecrits: u64,
    plafond: u64,
    /// Taille à partir de laquelle on tente la rotation. Égale au plafond en
    /// régime normal ; repoussée d'un plafond à chaque échec de renommage.
    seuil: u64,
    /// Le descripteur tenu est-il celui de `<chemin>.1`, adopté après une
    /// réouverture impossible ?
    ///
    /// Cet état n'était pas nommé, et c'est ce qui rendait le plafond inopérant
    /// exactement quand il servait : une fois le renommage fait et la
    /// réouverture échouée, `tourner` repassait par `rename`, qui ne pouvait
    /// que rendre `ENOENT` — le chemin courant n'existe plus. Le seuil était
    /// alors repoussé d'un plafond par tranche et `.1` grossissait **sans
    /// borne**. Mesuré le 11/09/2026 : 654 Mio en 60 s avec un plafond de
    /// 10 Mio, sous la même famine de descripteurs que celle qui emballait
    /// `slimproto::accept` (#2156).
    adopte: bool,
}

impl JournalBorne {
    /// Ouvre `chemin` en ajout et borne le fichier à `plafond` octets.
    ///
    /// `plafond` à zéro désactive la rotation — utile pour un appelant qui veut
    /// explicitement un journal illimité, et évite une division par zéro
    /// implicite dans la logique de seuil.
    pub fn ouvrir(chemin: PathBuf, plafond: u64) -> io::Result<Self> {
        let fichier = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&chemin)?;
        let ecrits = fichier.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            chemin,
            fichier,
            ecrits,
            plafond,
            seuil: plafond,
            adopte: false,
        })
    }

    /// Déplace le fichier courant vers `<chemin>.1` et en rouvre un neuf.
    ///
    /// En cas d'échec on garde le fichier courant : perdre des lignes serait
    /// pire que dépasser le plafond, et c'est justement quand le disque va mal
    /// qu'on veut lire le journal.
    fn tourner(&mut self) {
        if self.plafond == 0 {
            return;
        }
        let _ = self.fichier.flush();

        let mut sauvegarde = self.chemin.clone().into_os_string();
        sauvegarde.push(".1");

        let echec = |e: io::Error, quoi: &str| {
            // `eprintln!` et non `warn!` : voir l'entête du module — un log
            // émis d'ici rentrerait dans la couche qui nous appelle.
            eprintln!("tune-server: rotation du journal impossible ({quoi}) : {e}");
        };

        // Le descripteur tenu EST déjà la sauvegarde : une réouverture a
        // échoué plus tôt. Renommer ne peut plus rien (le chemin courant
        // n'existe pas) et repousser le seuil laisserait `.1` grossir sans
        // borne. On le vide sur place — perdre la tranche précédente est le
        // moindre mal face à un disque qui se remplit — puis on retente la
        // réouverture, qui réussira dès que la cause (EMFILE) aura cédé.
        if self.adopte {
            match self.fichier.set_len(0) {
                Ok(()) => {
                    self.ecrits = 0;
                    self.seuil = self.plafond;
                }
                Err(e) => {
                    echec(e, "vidage de la sauvegarde adoptée");
                    self.seuil = self.seuil.saturating_add(self.plafond);
                }
            }
            if let Ok(neuf) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.chemin)
            {
                self.fichier = neuf;
                self.adopte = false;
                self.ecrits = 0;
                self.seuil = self.plafond;
            }
            return;
        }

        if let Err(e) = std::fs::rename(&self.chemin, &sauvegarde) {
            echec(e, "renommage");
            self.seuil = self.seuil.saturating_add(self.plafond);
            return;
        }

        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.chemin)
        {
            Ok(neuf) => {
                self.fichier = neuf;
                self.ecrits = 0;
                self.seuil = self.plafond;
            }
            Err(e) => {
                // Le renommage a réussi, la réouverture non : le descripteur
                // courant pointe désormais sur `.1`. On continue d'y écrire —
                // les lignes ne sont pas perdues, elles ne sont simplement plus
                // au chemin attendu, et le prochain démarrage remettra tout en
                // place. Le compteur repart de zéro : ce fichier vient d'être
                // adopté vide du point de vue du plafond.
                echec(e, "réouverture");
                // Sans ce drapeau, le tour suivant retournait au `rename`
                // ci-dessus, qui ne pouvait plus qu'échouer : c'est là que le
                // plafond cessait de tenir (#2156).
                self.adopte = true;
                self.ecrits = 0;
                self.seuil = self.plafond;
            }
        }
    }
}

impl Write for JournalBorne {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.fichier.write(buf)?;
        self.ecrits = self.ecrits.saturating_add(n as u64);
        if self.plafond > 0 && self.ecrits > self.seuil {
            self.tourner();
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.fichier.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un dossier temporaire à soi, sans dépendance : deux tests qui
    /// partageraient un chemin se voleraient leur `.1`.
    ///
    /// Le garde est rendu tel quel — il supprime le dossier à la sortie du
    /// test, panique comprise. C'est cette famille qui pesait le plus lourd
    /// dans #3030 : 1 657 des 3 204 résidus de `/tmp` étaient des
    /// `tune-journal-*`.
    fn dossier(nom: &str) -> tune_core::test_scratch::ScratchDir {
        tune_core::test_scratch::scratch_dir(&format!("tune-journal-{nom}"))
    }

    fn taille(p: &std::path::Path) -> u64 {
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
    }

    /// Le plafond tient AUSSI quand la réouverture est impossible.
    ///
    /// C'est l'état dans lequel une famine de descripteurs (EMFILE) met ce
    /// module : le renommage passe, `OpenOptions::open` non. Le descripteur
    /// tenu pointe alors sur `.1`, et avant ce correctif plus rien ne le
    /// bornait — mesuré le 11/09/2026 : 654 Mio en 60 s pour un plafond de
    /// 10 Mio (#2156).
    ///
    /// Le chemin courant est dirigé vers un sous-dossier inexistant : toute
    /// réouverture échoue, comme sous EMFILE, sans avoir à épuiser les
    /// descripteurs du binaire de test.
    #[test]
    fn le_plafond_tient_meme_quand_la_reouverture_est_impossible() {
        let d = dossier("adopte");
        let chemin = d.join("tune-server.log");
        let sauvegarde = d.join("tune-server.log.1");
        let mut j = JournalBorne::ouvrir(chemin.clone(), 100).unwrap();

        // L'état d'après une réouverture échouée, reconstitué : le fichier a
        // été renommé, le descripteur tenu est celui de `.1`, et le chemin
        // courant ne pourra plus jamais être ouvert.
        std::fs::rename(&chemin, &sauvegarde).unwrap();
        j.adopte = true;
        j.chemin = d.join("dossier-absent").join("tune-server.log");

        for _ in 0..500 {
            j.write_all(&[b'x'; 10]).unwrap();
        }
        j.flush().unwrap();

        assert!(
            taille(&sauvegarde) <= 200,
            "5 000 octets écrits avec un plafond de 100 : la sauvegarde adoptée \
             fait {} octets. Le journal grossit sans borne dès que la réouverture \
             échoue — c'est le scénario que l'entête de ce module dit éviter (#2156).",
            taille(&sauvegarde)
        );
    }

    /// Garde de CÂBLAGE : la branche « réouverture impossible » lève bien le
    /// drapeau qui fait entrer dans la branche adoptée.
    ///
    /// Le test ci-dessus pose `adopte` à la main : il mesure ce que fait la
    /// branche adoptée, pas le fait qu'on y entre. Mesuré le 11/09/2026 —
    /// retirer `self.adopte = true;` le laissait VERT pendant que le défaut
    /// revenait entier. Cette garde-là tombe.
    ///
    /// Elle relit le texte du module, et c'est assumé : entrer réellement dans
    /// cette branche demande un `open()` qui échoue alors que le `rename()`
    /// juste avant a réussi — c'est-à-dire `EMFILE`, donc un `setrlimit` qui
    /// contaminerait tout le binaire de test.
    #[test]
    fn la_reouverture_impossible_marque_le_journal_comme_adopte() {
        let source = include_str!("journal.rs");
        let Some(debut) = source.find("echec(e, \"réouverture\");") else {
            panic!(
                "la branche « réouverture impossible » a disparu de `tourner` : \
                 vérifier que le plafond tient toujours quand le descripteur \
                 reste sur `.1` (#2156)"
            );
        };
        assert!(
            source[debut..]
                .chars()
                .take(600)
                .collect::<String>()
                .contains("self.adopte = true;"),
            "la réouverture échouée ne marque plus le journal comme adopté : \
             le tour suivant repassera par `rename`, qui ne peut que rendre \
             ENOENT, et `.1` grossira sans borne — 654 Mio mesurés en 60 s \
             pour un plafond de 10 Mio (#2156)"
        );
    }

    /// Le défaut de #539 tel quel : pendant que le processus vit, rien ne
    /// bornait le fichier. C'est le test qui tombe si l'appel à `tourner` est
    /// retiré de `write`.
    #[test]
    fn le_plafond_tient_sans_redemarrer() {
        let d = dossier("plafond");
        let chemin = d.join("tune-server.log");
        let mut j = JournalBorne::ouvrir(chemin.clone(), 100).unwrap();

        for _ in 0..50 {
            j.write_all(&[b'x'; 10]).unwrap();
        }
        j.flush().unwrap();

        assert!(
            taille(&chemin) <= 100,
            "500 octets écrits avec un plafond de 100 : le fichier courant fait {} octets. \
             Le journal grossit sans borne tant que le serveur tourne — c'est exactement \
             ce que #539 annonçait empêcher.",
            taille(&chemin)
        );
        assert!(
            d.join("tune-server.log.1").exists(),
            "la sauvegarde .1 doit exister : sans elle, la rotation a effacé des lignes \
             au lieu de les archiver."
        );
    }

    /// La rotation déplace, elle ne tronque pas : ce qui précède doit rester
    /// lisible dans `.1`. Un incident se lit à cheval sur les deux fichiers.
    #[test]
    fn ce_qui_precede_la_rotation_est_conserve() {
        let d = dossier("conserve");
        let chemin = d.join("tune-server.log");
        let mut j = JournalBorne::ouvrir(chemin.clone(), 20).unwrap();

        j.write_all(b"AVANT-LA-ROTATION\n").unwrap();
        j.write_all(b"APRES\n").unwrap();
        j.flush().unwrap();

        let sauvegarde = std::fs::read_to_string(d.join("tune-server.log.1")).unwrap();
        assert!(
            sauvegarde.contains("AVANT-LA-ROTATION"),
            "obtenu : {sauvegarde:?}"
        );
    }

    /// Un fichier laissé presque plein par la session précédente doit tourner
    /// à la ligne suivante. Si le compteur repartait de zéro à l'ouverture, le
    /// plafond serait dépassé d'un plafond entier à chaque redémarrage.
    #[test]
    fn le_compteur_part_de_ce_que_le_fichier_contient_deja() {
        let d = dossier("herite");
        let chemin = d.join("tune-server.log");
        std::fs::write(&chemin, vec![b'a'; 95]).unwrap();

        let mut j = JournalBorne::ouvrir(chemin.clone(), 100).unwrap();
        j.write_all(&[b'b'; 10]).unwrap();
        j.flush().unwrap();

        assert!(
            taille(&chemin) < 95,
            "le fichier courant fait {} octets : l'héritage de 95 octets a été ignoré \
             et le plafond ne s'appliquera qu'après 100 octets de PLUS.",
            taille(&chemin)
        );
    }

    /// Sur un disque plein, `rename` échoue à chaque tentative. Le seuil doit
    /// être repoussé, sans quoi chaque ligne déclenche un appel système de plus
    /// — le journal deviendrait le goulot du serveur au moment précis où l'on
    /// a besoin de lui.
    #[test]
    fn un_renommage_impossible_ne_se_retente_pas_a_chaque_octet() {
        let d = dossier("echec");
        let chemin = d.join("tune-server.log");
        let mut j = JournalBorne::ouvrir(chemin.clone(), 50).unwrap();

        // Rendre le renommage impossible : `.1` est un DOSSIER non vide, et
        // `rename` refuse d'écraser cela sur tous nos systèmes.
        let barrage = d.join("tune-server.log.1");
        std::fs::create_dir_all(&barrage).unwrap();
        std::fs::write(barrage.join("occupe"), b"x").unwrap();

        j.write_all(&[b'x'; 60]).unwrap();
        let seuil_apres_echec = j.seuil;
        assert_eq!(
            seuil_apres_echec, 100,
            "après un échec, le seuil doit être repoussé d'un plafond (50 → 100)."
        );

        // Les écritures suivantes, sous le nouveau seuil, ne retentent rien.
        j.write_all(&[b'x'; 10]).unwrap();
        assert_eq!(j.seuil, 100, "aucune tentative ne devait avoir lieu ici.");

        // ... et rien n'est perdu : tout est encore dans le fichier courant.
        j.flush().unwrap();
        assert_eq!(
            taille(&chemin),
            70,
            "un journal qu'on ne peut pas rotationner doit continuer d'écrire : \
             perdre des lignes quand le disque va mal serait le pire moment."
        );
    }

    /// Un plafond nul est un choix explicite d'appelant, pas un accident de
    /// configuration : il ne doit ni rotationner ni diviser par zéro.
    #[test]
    fn un_plafond_nul_desactive_la_rotation() {
        let d = dossier("nul");
        let chemin = d.join("tune-server.log");
        let mut j = JournalBorne::ouvrir(chemin.clone(), 0).unwrap();
        j.write_all(&[b'x'; 500]).unwrap();
        j.flush().unwrap();
        assert_eq!(taille(&chemin), 500);
        assert!(!d.join("tune-server.log.1").exists());
    }
}
