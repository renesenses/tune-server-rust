//! Qui tient le PCM ALSA que nous n'arrivons pas à ouvrir — #3575.
//!
//! # Pourquoi ce module existe
//!
//! Depuis `ee4ec884` (« préférer le PCM matériel `hw:` au greffon qui accepte
//! tout », v0.9.132) une sortie locale Linux ouvre `hw:CARD=…,DEV=…`, qui
//! n'accepte **qu'un seul ouvreur**. Quand quelqu'un d'autre le tient déjà, le
//! noyau rend `EBUSY` — et cpal 0.17.3 replie `ENOENT`, `EPERM`, `ENODEV`,
//! `ENOTSUPP`, `EBUSY` et `EAGAIN` sur un seul `DeviceNotAvailable` dont le
//! `Display` est toujours « The requested device is no longer available. For
//! example, it has been unplugged. » Le motif est **détruit** avant d'atteindre
//! notre code : c'est ce que nomme [`crate::outputs::local::OpenFailure`]
//! `IndisponibleMotifPerdu`.
//!
//! La v0.9.145 a livré la sentinelle qui empêche Tune de se prendre le PCM à
//! **lui-même** (`decider_la_relache_du_peripherique`). Son auteur a nommé sa
//! limite dans la PR #3753 : elle ne connaît que **notre propre fil**. Un PCM
//! tenu par une **instance précédente du processus**, par un `execv` de mise à
//! jour dont le descripteur aurait survécu, ou par un tout autre programme
//! (Lyrion/LMS, `aplay`, PipeWire en mode direct) lui est invisible — et c'est
//! justement l'hypothèse centrale du ticket, celle qui expliquerait
//! « imprenable pour toute la vie du processus ».
//!
//! Depuis le 07/09/2026 la même observation est demandée au testeur à chaque
//! tour, et n'arrive jamais :
//!
//! ```text
//! fuser -v /dev/snd/*        # avant tout redémarrage — le redémarrage efface la preuve
//! ps -ef | grep -c "[t]une-server"
//! ```
//!
//! Ce module la prend **tout seul**, au moment exact de l'échec, depuis
//! `/proc` — c'est ce que fait `fuser`, sans dépendre de sa présence sur la
//! machine ni de la présence d'esprit de quelqu'un.
//!
//! # Ce que ce module ne fait PAS
//!
//! Il ne change **aucun** comportement audio : il n'ouvre rien, ne ferme rien,
//! ne tue personne, n'attend pas. Il lit `/proc` et écrit une ligne de journal.
//! Sur une P0 de sortie audio, c'est délibéré : une rustine qui « libère » un
//! PCM rendrait muette la chaîne d'un testeur.
//!
//! Il n'établit pas non plus la cause du cas de Belkadi Yacine. Il produit la
//! mesure qui la trancherait, au prochain relevé.
//!
//! # Pourquoi ici et pas dans `outputs/local.rs`
//!
//! `outputs/local` vit derrière la feature `local-audio`, que le job `Test` de
//! `ci.yml` n'active pas. Un test posé là-bas serait vert contre rien
//! (#2816, « témoin endormi »). Ce module-ci est compilé par défaut, ses
//! témoins tournent dans le job `Test`, et la racine de `/proc` entre par
//! **paramètre** : la lecture s'éprouve sur une arborescence fabriquée, sur une
//! machine sans carte son — Shrek n'a pas de `/proc/asound`.

use std::path::{Path, PathBuf};

/// Un processus qui tient le nœud PCM, tel que `/proc` le rapporte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeneurDuPcm {
    /// PID du teneur.
    pub pid: u32,
    /// `/proc/<pid>/comm`, c'est-à-dire le nom court de l'exécutable
    /// (`tune-server`, `squeezelite`, `aplay`…). Vide si illisible.
    pub programme: String,
    /// Est-ce NOUS ? Un `true` ici désigne le recouvrement interne déjà traité
    /// par `decider_la_relache_du_peripherique` ; un `false` désigne un teneur
    /// que la sentinelle ne peut pas voir — l'hypothèse du ticket.
    pub nous: bool,
}

/// Le nœud de périphérique que le noyau associe à un PCM ALSA de LECTURE.
///
/// `/dev/snd/pcmC<carte>D<peripherique>p` — le `p` final est la direction
/// « playback ». C'est ce fichier que `fuser -v /dev/snd/*` interroge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoeudPcm {
    /// Index de carte, tel que `/proc/asound/cards` le numérote.
    pub carte: u32,
    /// Index de périphérique dans la carte.
    pub peripherique: u32,
}

impl NoeudPcm {
    /// `pcmC2D0p` — le nom de base sous `/dev/snd`.
    pub fn nom(&self) -> String {
        format!("pcmC{}D{}p", self.carte, self.peripherique)
    }

    /// `/dev/snd/pcmC2D0p` — le chemin absolu, celui que rend `readlink` sur
    /// `/proc/<pid>/fd/<n>`.
    pub fn chemin(&self) -> String {
        format!("/dev/snd/{}", self.nom())
    }
}

/// Index des cartes, lu dans le texte de `/proc/asound/cards`.
///
/// Le fichier ressemble à ceci, et c'est la SEULE forme qu'on lui connaisse :
///
/// ```text
///  0 [PCH            ]: HDA-Intel - HDA Intel PCH
///                       HDA Intel PCH at 0xf7d10000 irq 33
///  2 [V314           ]: USB-Audio - DENAFRIPS USB Audio V3.14
///                       DENAFRIPS USB Audio V3.14 at usb-0000:00:14.0-2, high speed
/// ```
///
/// On ne retient que les lignes qui commencent par un nombre suivi d'un
/// crochet : la seconde ligne de chaque carte est une description libre qui
/// peut, elle, contenir n'importe quoi.
fn index_des_cartes(cartes: &str) -> Vec<(u32, String)> {
    let mut index = Vec::new();
    for ligne in cartes.lines() {
        let reste = ligne.trim_start();
        let Some((numero, apres)) = reste.split_once('[') else {
            continue;
        };
        let numero = numero.trim();
        if numero.is_empty() || !numero.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Some((nom, _)) = apres.split_once(']') else {
            continue;
        };
        if let Ok(n) = numero.parse::<u32>() {
            index.push((n, nom.trim().to_string()));
        }
    }
    index
}

/// Le PCM ALSA porté par un `endpoint_id`, sans le préfixe d'hôte de cpal.
///
/// cpal rend `DeviceId` sous la forme `«hôte»:«pcm»` ; le `pcm` d'ALSA est
/// lui-même préfixé par son greffon. On ne retire donc QUE `alsa:`.
fn pcm_sans_hote(endpoint_id: &str) -> &str {
    match endpoint_id.split_once(':') {
        Some((tete, reste)) if tete.eq_ignore_ascii_case("alsa") => reste,
        _ => endpoint_id,
    }
}

/// De `alsa:hw:CARD=2,DEV=0` au nœud `/dev/snd/pcmC2D0p`.
///
/// Rend `None` — et c'est voulu — dès que le PCM n'est pas un accès **direct**
/// au matériel (`hw:`, `plughw:`). Un `dmix:`, un `default`, un `pipewire`
/// acceptent plusieurs ouvreurs : y chercher un teneur exclusif n'aurait aucun
/// sens et fabriquerait un coupable.
///
/// `CARD=` accepte les deux écritures qu'ALSA reconnaît : l'index (`CARD=2`) et
/// le nom court (`CARD=V314`, celui de Belkadi Yacine). La forme positionnelle
/// `hw:2,0` est acceptée aussi — c'est celle que tapent les gens. `DEV=`
/// absent vaut 0, comme dans ALSA.
pub fn noeud_pcm_du_endpoint(endpoint_id: &str, cartes: &str) -> Option<NoeudPcm> {
    let pcm = pcm_sans_hote(endpoint_id);
    let (greffon, arguments) = pcm.split_once(':')?;
    if !(greffon.eq_ignore_ascii_case("hw") || greffon.eq_ignore_ascii_case("plughw")) {
        return None;
    }

    let mut carte_brute: Option<&str> = None;
    let mut peripherique_brut: Option<&str> = None;
    for (rang, champ) in arguments.split(',').enumerate() {
        let champ = champ.trim();
        if champ.is_empty() {
            continue;
        }
        match champ.split_once('=') {
            Some((cle, valeur)) if cle.trim().eq_ignore_ascii_case("card") => {
                carte_brute = Some(valeur.trim());
            }
            Some((cle, valeur)) if cle.trim().eq_ignore_ascii_case("dev") => {
                peripherique_brut = Some(valeur.trim());
            }
            // `SUBDEV=` et le reste ne nous apprennent rien sur le nœud.
            Some(_) => {}
            None => match rang {
                0 => carte_brute = Some(champ),
                1 => peripherique_brut = Some(champ),
                _ => {}
            },
        }
    }

    let carte_brute = carte_brute?;
    let carte = match carte_brute.parse::<u32>() {
        Ok(n) => n,
        Err(_) => index_des_cartes(cartes)
            .into_iter()
            .find(|(_, nom)| nom.eq_ignore_ascii_case(carte_brute))
            .map(|(n, _)| n)?,
    };
    let peripherique = match peripherique_brut {
        Some(valeur) => valeur.parse::<u32>().ok()?,
        None => 0,
    };
    Some(NoeudPcm {
        carte,
        peripherique,
    })
}

/// Qui tient ce nœud, d'après `/proc` ?
///
/// C'est exactement ce que fait `fuser` : parcourir `/proc/<pid>/fd/` et lire
/// chaque lien symbolique. Un descripteur ouvert sur `/dev/snd/pcmC2D0p` s'y
/// lit tel quel.
///
/// `racine_proc` est un paramètre et non la constante `/proc` pour une raison
/// qui n'est pas cosmétique : Shrek n'a **pas** de carte son (`/proc/asound`
/// n'existe pas), donc aucune mesure audio réelle n'y est possible. En faisant
/// entrer la racine, la lecture s'éprouve sur une arborescence fabriquée avec
/// de vrais liens symboliques — la forme exacte de la charge utile réelle.
///
/// **Tout échec de lecture est silencieux, par pid.** Un `/proc/<pid>/fd`
/// illisible (processus d'un autre compte, processus mort entre deux appels)
/// ne doit ni interrompre le parcours ni faire croire à une absence de teneur :
/// c'est la différence entre « personne ne le tient » et « je n'ai pas pu
/// voir », et [`resume_des_teneurs`] la dit.
pub fn teneurs_du_noeud(
    racine_proc: &Path,
    racine_dev: &Path,
    noeud: &NoeudPcm,
    notre_pid: u32,
) -> Vec<TeneurDuPcm> {
    let attendu = racine_dev.join(noeud.nom());
    let Ok(entrees) = std::fs::read_dir(racine_proc) else {
        return Vec::new();
    };
    let mut teneurs: Vec<TeneurDuPcm> = Vec::new();
    for entree in entrees.flatten() {
        let nom = entree.file_name();
        let Some(nom) = nom.to_str() else { continue };
        let Ok(pid) = nom.parse::<u32>() else {
            continue;
        };
        let dossier_fd: PathBuf = entree.path().join("fd");
        let Ok(descripteurs) = std::fs::read_dir(&dossier_fd) else {
            continue;
        };
        let mut tient = false;
        for descripteur in descripteurs.flatten() {
            let Ok(cible) = std::fs::read_link(descripteur.path()) else {
                continue;
            };
            // Comparaison EXACTE du chemin. Accepter le seul nom de base
            // ferait d'un fichier nommé « pcmC2D0p » posé n'importe où un
            // teneur du DAC — et ce relâchement n'aurait existé que pour
            // arranger le banc d'essai, qui reçoit sa propre racine.
            if cible == attendu {
                tient = true;
                break;
            }
        }
        if !tient {
            continue;
        }
        let programme = std::fs::read_to_string(entree.path().join("comm"))
            .map(|c| c.trim().to_string())
            .unwrap_or_default();
        teneurs.push(TeneurDuPcm {
            pid,
            programme,
            nous: pid == notre_pid,
        });
    }
    teneurs.sort_by_key(|t| t.pid);
    teneurs
}

/// La phrase qui part au journal, pensée pour être lue dans un export.
///
/// Elle distingue les trois états, parce qu'ils commandent trois diagnostics
/// différents et qu'un export ne permet pas de les redemander :
///
/// | sortie | ce que ça veut dire |
/// |---|---|
/// | `aucun_teneur_visible` | personne ne tient le nœud : l'échec n'est PAS un recouvrement, il faut regarder le matériel |
/// | `nous:1234(tune-server)` | c'est nous — le recouvrement interne de #3575, celui que la v0.9.145 traite |
/// | `autre:987(squeezelite)` | un teneur que la sentinelle ne peut pas voir : l'hypothèse du ticket, enfin nommée |
pub fn resume_des_teneurs(teneurs: &[TeneurDuPcm]) -> String {
    if teneurs.is_empty() {
        return "aucun_teneur_visible".to_string();
    }
    teneurs
        .iter()
        .map(|t| {
            let qui = if t.nous { "nous" } else { "autre" };
            let programme = if t.programme.is_empty() {
                "?"
            } else {
                t.programme.as_str()
            };
            format!("{qui}:{}({programme})", t.pid)
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Y a-t-il, parmi les teneurs, quelqu'un qui n'est PAS nous ?
///
/// C'est la seule question à laquelle la sentinelle de la v0.9.145 ne sait pas
/// répondre, et celle dont dépend le verdict de #3575.
pub fn un_teneur_etranger(teneurs: &[TeneurDuPcm]) -> bool {
    teneurs.iter().any(|t| !t.nous)
}

/// Le relevé complet, tel que le chemin d'échec de `outputs/local` l'appelle.
///
/// Rend `None` quand il n'y a rien à dire : PCM non matériel, `/proc/asound`
/// absent, endpoint illisible. Un `None` ne doit **jamais** être journalisé
/// comme « aucun teneur » — ce serait exactement le faux vert que ce module
/// cherche à empêcher.
#[cfg(target_os = "linux")]
pub fn relever_les_teneurs(endpoint_id: &str) -> Option<(NoeudPcm, Vec<TeneurDuPcm>)> {
    let cartes = std::fs::read_to_string("/proc/asound/cards").ok()?;
    let noeud = noeud_pcm_du_endpoint(endpoint_id, &cartes)?;
    let teneurs = teneurs_du_noeud(
        Path::new("/proc"),
        Path::new("/dev/snd"),
        &noeud,
        std::process::id(),
    );
    Some((noeud, teneurs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARTES: &str = "\
 0 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0xf7d10000 irq 33
 2 [V314           ]: USB-Audio - DENAFRIPS USB Audio V3.14
                      DENAFRIPS USB Audio V3.14 at usb-0000:00:14.0-2, high speed
";

    /// La charge utile RÉELLE : l'`endpoint_id` relevé dans l'export de
    /// Belkadi Yacine (#3575), recopié tel quel.
    #[test]
    fn l_endpoint_de_yacine_donne_le_noeud_de_sa_carte() {
        let noeud = noeud_pcm_du_endpoint("alsa:hw:CARD=2,DEV=0", CARTES).expect("PCM matériel");
        assert_eq!(noeud.nom(), "pcmC2D0p");
        assert_eq!(noeud.chemin(), "/dev/snd/pcmC2D0p");
    }

    /// L'autre écriture du MÊME PCM, celle que porte
    /// `local_audio_alsa_hardware_pcm_preferred` : le nom court de la carte.
    /// Sans la table de `/proc/asound/cards`, elle ne se résout pas — et le
    /// module doit alors se taire plutôt que d'inventer un index.
    #[test]
    fn le_nom_court_de_carte_se_resout_par_la_table_et_pas_autrement() {
        assert_eq!(
            noeud_pcm_du_endpoint("alsa:hw:CARD=V314,DEV=0", CARTES)
                .expect("V314 est dans la table")
                .nom(),
            "pcmC2D0p"
        );
        assert!(
            noeud_pcm_du_endpoint("alsa:hw:CARD=V314,DEV=0", "").is_none(),
            "sans table, aucun index ne doit être fabriqué"
        );
    }

    #[test]
    fn les_ecritures_positionnelles_et_le_dev_implicite() {
        assert_eq!(
            noeud_pcm_du_endpoint("hw:2,1", CARTES)
                .expect("forme positionnelle")
                .nom(),
            "pcmC2D1p"
        );
        assert_eq!(
            noeud_pcm_du_endpoint("alsa:hw:CARD=0", CARTES)
                .expect("DEV implicite")
                .nom(),
            "pcmC0D0p"
        );
        assert_eq!(
            noeud_pcm_du_endpoint("alsa:plughw:CARD=2,DEV=0", CARTES)
                .expect("plughw ouvre le même nœud")
                .nom(),
            "pcmC2D0p"
        );
    }

    /// CONTRE-ÉPREUVE de la fonction elle-même : un greffon PARTAGEABLE ne doit
    /// produire aucun nœud. Chercher un teneur exclusif sur un `dmix:` ou un
    /// `default` désignerait un coupable là où la mécanique autorise plusieurs
    /// ouvreurs — c'est le faux positif que ce module doit refuser.
    #[test]
    fn aucun_noeud_pour_un_greffon_partageable() {
        for partageable in [
            "alsa:default",
            "alsa:sysdefault:CARD=2",
            "alsa:dmix:CARD=2,DEV=0",
            "alsa:front:CARD=V314,DEV=0",
            "alsa:pipewire",
            "wasapi:{0.0.0.00000000}",
        ] {
            assert!(
                noeud_pcm_du_endpoint(partageable, CARTES).is_none(),
                "« {partageable} » accepte plusieurs ouvreurs : aucun teneur exclusif à nommer"
            );
        }
    }

    #[test]
    fn la_table_des_cartes_ignore_la_ligne_de_description() {
        // La seconde ligne de chaque carte contient « at 0xf7d10000 » et un
        // crochet n'y est pas exclu : elle ne doit jamais devenir une carte.
        let index = index_des_cartes(CARTES);
        assert_eq!(
            index,
            vec![(0, "PCH".to_string()), (2, "V314".to_string())],
            "seules les lignes « <n> [NOM] » sont des cartes"
        );
    }

    #[cfg(unix)]
    mod procfs {
        use super::*;
        use std::os::unix::fs::symlink;

        /// Fabrique une racine `/proc` avec de VRAIS liens symboliques : c'est
        /// la forme exacte de la charge utile que la fonction lira en
        /// production. Un test qui se contenterait de fichiers ordinaires
        /// éprouverait autre chose que `read_link`.
        fn racine(
            processus: &[(u32, &str, &[&str])],
        ) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc = tmp.path().join("proc");
            let dev = tmp.path().join("dev-snd");
            std::fs::create_dir_all(&dev).expect("dev");
            for (pid, comm, cibles) in processus {
                let base = proc.join(pid.to_string());
                std::fs::create_dir_all(base.join("fd")).expect("fd");
                std::fs::write(base.join("comm"), format!("{comm}\n")).expect("comm");
                for (n, cible) in cibles.iter().enumerate() {
                    let fichier = dev.join(cible);
                    if !fichier.exists() {
                        std::fs::write(&fichier, b"").expect("noeud");
                    }
                    symlink(&fichier, base.join("fd").join(n.to_string())).expect("symlink");
                }
            }
            (tmp, proc, dev)
        }

        /// L'hypothèse CENTRALE de #3575, celle que la sentinelle de la
        /// v0.9.145 ne peut pas voir : le PCM est tenu par un processus qui
        /// n'est pas nous.
        #[test]
        fn un_teneur_etranger_est_nomme_avec_son_programme() {
            let (_tmp, proc, dev) = racine(&[
                (4242, "tune-server", &["pcmC2D0p"]),
                (77, "squeezelite", &[]),
            ]);
            let noeud = NoeudPcm {
                carte: 2,
                peripherique: 0,
            };
            // Nous sommes 99 : le teneur 4242 est donc une AUTRE instance.
            let teneurs = teneurs_du_noeud(&proc, &dev, &noeud, 99);
            assert_eq!(
                teneurs,
                vec![TeneurDuPcm {
                    pid: 4242,
                    programme: "tune-server".to_string(),
                    nous: false,
                }]
            );
            assert!(un_teneur_etranger(&teneurs));
            assert_eq!(resume_des_teneurs(&teneurs), "autre:4242(tune-server)");
        }

        /// Le MÊME relevé, vu par le processus qui tient le nœud : c'est le
        /// recouvrement interne, déjà traité. Les deux cas doivent se
        /// distinguer dans le journal, sinon la ligne ne tranche rien — c'est
        /// la faute du compteur de famine qui disait la même chose pour deux
        /// états opposés.
        #[test]
        fn le_meme_noeud_tenu_par_nous_ne_dit_pas_la_meme_chose() {
            let (_tmp, proc, dev) = racine(&[(4242, "tune-server", &["pcmC2D0p"])]);
            let noeud = NoeudPcm {
                carte: 2,
                peripherique: 0,
            };
            let teneurs = teneurs_du_noeud(&proc, &dev, &noeud, 4242);
            assert!(!un_teneur_etranger(&teneurs));
            assert_eq!(resume_des_teneurs(&teneurs), "nous:4242(tune-server)");
        }

        /// Le cas négatif : un processus qui tient un AUTRE nœud de `/dev/snd`
        /// ne tient pas le nôtre. Sans ce témoin, une implémentation qui
        /// rendrait « tout le monde » resterait verte.
        #[test]
        fn un_descripteur_sur_une_autre_carte_ne_compte_pas() {
            let (_tmp, proc, dev) = racine(&[
                (11, "aplay", &["pcmC0D0p"]),
                (12, "pipewire", &["controlC2"]),
            ]);
            let noeud = NoeudPcm {
                carte: 2,
                peripherique: 0,
            };
            let teneurs = teneurs_du_noeud(&proc, &dev, &noeud, 99);
            assert!(teneurs.is_empty(), "{teneurs:?}");
            assert_eq!(resume_des_teneurs(&teneurs), "aucun_teneur_visible");
        }

        /// Plusieurs teneurs, dont nous : l'ordre est stable (par pid) pour
        /// qu'une ligne de journal se compare d'un relevé à l'autre.
        #[test]
        fn plusieurs_teneurs_sont_tous_nommes_dans_un_ordre_stable() {
            let (_tmp, proc, dev) = racine(&[
                (900, "tune-server", &["pcmC2D0p"]),
                (30, "squeezelite", &["pcmC2D0p"]),
                (1, "systemd", &[]),
            ]);
            let noeud = NoeudPcm {
                carte: 2,
                peripherique: 0,
            };
            let teneurs = teneurs_du_noeud(&proc, &dev, &noeud, 900);
            assert_eq!(
                resume_des_teneurs(&teneurs),
                "autre:30(squeezelite),nous:900(tune-server)"
            );
            assert!(un_teneur_etranger(&teneurs));
        }

        /// Une racine absente ne doit pas paniquer ni mentir.
        #[test]
        fn une_racine_illisible_ne_panique_pas() {
            let noeud = NoeudPcm {
                carte: 2,
                peripherique: 0,
            };
            let teneurs = teneurs_du_noeud(
                Path::new("/n-existe-pas-3575"),
                Path::new("/dev/snd"),
                &noeud,
                1,
            );
            assert!(teneurs.is_empty());
        }
    }
}
