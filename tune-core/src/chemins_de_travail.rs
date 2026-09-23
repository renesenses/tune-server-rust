//! Racines de travail du serveur : **une par utilisateur qui exécute**, jamais
//! un chemin littéral partagé.
//!
//! # Le défaut, mesuré
//!
//! `tune-server/src/routes/converter.rs` déclarait
//! `const CONVERT_OUTPUT_ROOT: &str = "/tmp/tune-convert"` ; `declick.rs`
//! composait `/tmp/tune-declick/{job_id}` de la même façon. Deux chemins
//! **fixes**, donc partagés par tous les comptes d'une même machine.
//!
//! Sur la machine de compilation, `/tmp/tune-convert` appartient au compte
//! `jp` depuis le 18/09/2026, en `775`. Tout autre utilisateur y échoue :
//! `cargo test -p tune-server` rougit sur
//! `audio_offer_contract::audio_offer_free_eq_and_premium_four_survive_real_startup`
//! avec « failed to create output dir: Permission denied ». Le premier venu
//! crée le dossier, et le suivant ne peut plus rien y écrire — pour toujours,
//! puisque personne ne pense à supprimer un dossier de `/tmp` qui appartient à
//! quelqu'un d'autre.
//!
//! Ce faux rouge se reproduit à **chaque** campagne. Ce n'est pas son coût qui
//! compte, c'est ce qu'il enseigne : un agent qui a vu trois fois ce rouge
//! apprend à l'écarter, et il écartera le jour venu une vraie régression du
//! convertisseur. Cf `feedback_la_contre_epreuve_elle_meme_peut_etre_negative`.
//!
//! # Deux choses sont corrigées, pas une
//!
//! 1. **Le littéral `/tmp`** : il ignorait `TMPDIR`. C'était déjà un défaut en
//!    soi — un exploitant qui pose `TMPDIR=/var/tmp/tune` parce que son `/tmp`
//!    est un tmpfs de 512 Mio voyait le convertisseur le remplir quand même.
//!    On passe par [`std::env::temp_dir`], qui lit `TMPDIR` (`TMP`/`TEMP` sous
//!    Windows) : le réglage existant est désormais respecté.
//! 2. **Le nom fixe** : on y joint l'identifiant numérique de l'utilisateur
//!    courant. Deux comptes de la même machine ne se croisent plus.
//!
//! Le second sans le premier ne suffirait pas, et inversement : sous Linux
//! `temp_dir()` rend `/tmp` sans `TMPDIR`, donc toujours partagé.
//!
//! # Pourquoi l'UID et pas le nom de compte
//!
//! `$USER` et `$LOGNAME` ne sont pas posés dans l'environnement d'un service
//! systemd — c'est précisément le cas de production. L'UID, lui, est toujours
//! là : il se lit au noyau, pas à l'environnement. Et il n'a besoin d'aucune
//! échappée : c'est un entier, donc rien à assainir avant de le coller dans un
//! nom de fichier.
//!
//! # Ce que ça change en production : rien d'observable
//!
//! Les unités systemd livrées (`packaging/deb/tune-server.service`, les trois
//! `image/build-*-image.sh`) portent `PrivateTmp=yes` : le service a **déjà**
//! son `/tmp` à lui, dans un espace de noms de montage. Le dossier y change
//! seulement de nom. Et le serveur est le seul à connaître ce chemin — il le
//! publie dans `/capabilities` (`output_root`) et ne le reçoit jamais d'un
//! client. Le web-client, lui, n'en lit rien ni n'en écrit rien.
//!
//! La vraie différence est pour les installations **hors paquet** — `cargo
//! run`, Homebrew, un binaire lancé à la main — où il n'y a aucun
//! `PrivateTmp`. Ce sont elles qui souffraient du défaut, et la machine de
//! compilation en est un cas particulier.
//!
//! # À ne pas confondre avec `test_scratch`
//!
//! [`crate::test_scratch`] sert au code de **test** : un chemin par appel,
//! supprimé par `Drop`. Ce module-ci sert au code **livré** : un chemin par
//! utilisateur, durable le temps d'un travail. Le premier isole des tests
//! entre eux, le second isole des comptes entre eux. Les deux répondent à la
//! même famille de défaut — un chemin temporaire composé à la main — d'où
//! leur voisinage dans cette caisse.

use std::path::{Path, PathBuf};

/// L'identifiant numérique de l'utilisateur qui exécute ce processus.
///
/// Sous Windows il n'y a pas d'UID, et il n'en faut pas : `temp_dir()` y rend
/// déjà `…\Users\<compte>\AppData\Local\Temp`, propre au compte. On rend `0`,
/// qui ne sert alors qu'à garder une seule forme de nom entre les systèmes.
pub fn uid_courant() -> u32 {
    #[cfg(unix)]
    {
        // `getuid` ne peut pas échouer et ne touche à aucun état partagé.
        unsafe { libc::getuid() as u32 }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// La racine de travail de l'utilisateur courant pour une `etiquette` donnée.
///
/// `etiquette` nomme le chantier (`"tune-convert"`, `"tune-declick"`) ; c'est
/// elle qui dit à qui appartient un résidu retrouvé dans `/tmp` un mois plus
/// tard.
///
/// ```ignore
/// let racine = tune_core::chemins_de_travail::racine_de_travail("tune-convert");
/// // /tmp/tune-convert-1001 sous Linux, $TMPDIR/tune-convert-501 sous macOS.
/// ```
pub fn racine_de_travail(etiquette: &str) -> PathBuf {
    racine_de_travail_sous(std::env::temp_dir(), etiquette, uid_courant())
}

/// La même, avec sa base et son UID **passés** — c'est la forme testable.
///
/// Séparer les deux n'est pas de la décoration : une garde qui appellerait
/// `racine_de_travail()` deux fois dans le même processus obtiendrait deux fois
/// le même UID et ne prouverait donc **rien** sur la séparation de deux
/// comptes. Cf `feedback_une_garde_qui_construit_elle_meme_ne_garde_pas_le_branchement`.
pub fn racine_de_travail_sous(base: impl AsRef<Path>, etiquette: &str, uid: u32) -> PathBuf {
    base.as_ref().join(format!("{etiquette}-{uid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le témoin de #4770 : **deux utilisateurs ne se marchent pas dessus**.
    ///
    /// Reproduit le défaut de Shrek sans avoir besoin de deux comptes. Le
    /// dossier « de l'autre » est créé puis passé en `555` : c'est exactement
    /// ce que voit un second utilisateur devant le `/tmp/tune-convert` de
    /// `jp` — le dossier existe, il est lisible, et on ne peut rien y créer.
    ///
    /// Trois assertions, dans cet ordre, et c'est l'ordre qui fait la preuve :
    /// 1. le chemin FIXE échoue — sans ça le test passerait sur une machine où
    ///    le défaut n'existe pas, et ne prouverait rien ;
    /// 2. les deux racines par utilisateur sont distinctes ;
    /// 3. elles se créent toutes les deux **pour de vrai**.
    #[cfg(unix)]
    #[test]
    fn deux_utilisateurs_ne_partagent_pas_la_racine_de_travail() {
        use std::os::unix::fs::PermissionsExt;

        if uid_courant() == 0 {
            // root ignore les permissions : le témoin ne pourrait pas rougir,
            // donc il ne prouverait rien. On le dit au lieu de passer vert.
            eprintln!("témoin ignoré : exécuté en root, les modes ne mordent pas");
            return;
        }

        let base = crate::test_scratch::scratch_dir("tune-chemins-de-travail");
        let travail = "0f0e0d0c-1111-2222-3333-444444444444";

        // 1. Le geste d'AVANT : une racine au nom fixe, déjà créée par
        //    quelqu'un d'autre. C'est le rouge de Shrek, ici reproduit.
        let partagee = base.join("tune-convert");
        std::fs::create_dir_all(&partagee).expect("racine « de l'autre utilisateur »");
        std::fs::set_permissions(&partagee, std::fs::Permissions::from_mode(0o555))
            .expect("mode 555");
        let echec = std::fs::create_dir_all(partagee.join(travail));
        assert!(
            echec.is_err(),
            "le chemin fixe aurait dû être refusé : sans ce rouge, la suite ne prouve rien"
        );
        assert_eq!(
            echec.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
            "le refus attendu est bien celui des permissions"
        );
        // Rendu inscriptible pour que le ménage de `ScratchDir` l'emporte.
        std::fs::set_permissions(&partagee, std::fs::Permissions::from_mode(0o755))
            .expect("mode 755");

        // 2. Le geste d'APRÈS : deux UID, deux racines.
        let mienne = racine_de_travail_sous(&base, "tune-convert", 1000);
        let sienne = racine_de_travail_sous(&base, "tune-convert", 1001);
        assert_ne!(mienne, sienne, "deux UID rendent le même chemin");
        assert!(
            !mienne.starts_with(&sienne) && !sienne.starts_with(&mienne),
            "une racine est contenue dans l'autre : {mienne:?} / {sienne:?}"
        );
        assert_ne!(
            mienne, partagee,
            "la racine par utilisateur est retombée sur le nom fixe"
        );

        // 3. Et elles se créent toutes les deux, côte à côte.
        for racine in [&mienne, &sienne] {
            std::fs::create_dir_all(racine.join(travail))
                .unwrap_or_else(|e| panic!("création de {racine:?} refusée : {e}"));
            assert!(racine.join(travail).is_dir());
        }
    }

    /// L'étiquette reste lisible dans le nom, et la base est respectée.
    ///
    /// Un résidu retrouvé dans `/tmp` doit dire d'où il vient : `tune-convert`
    /// et `tune-declick` se distinguent, et un suffixe seul ne l'aurait pas
    /// permis.
    #[test]
    fn l_etiquette_et_la_base_sont_conservees() {
        let racine = racine_de_travail_sous("/base/a/soi", "tune-declick", 4242);
        assert_eq!(racine, Path::new("/base/a/soi/tune-declick-4242"));
        assert_ne!(
            racine_de_travail_sous("/base/a/soi", "tune-convert", 4242),
            racine,
            "deux chantiers du même utilisateur partagent leur racine"
        );
    }

    /// `TMPDIR` est respecté : c'est le réglage qui existait déjà et que le
    /// chemin littéral ignorait. On ne le prouve pas en posant la variable
    /// (l'environnement est global au processus, donc aux tests parallèles),
    /// mais en montrant que la racine est bien sous `temp_dir()`, seule
    /// fonction qui la lit.
    ///
    /// ⚠️ Le nom de ce test ne peut pas s'écrire « …_sous_temp_dir » : le
    /// garde `aucune_fuite_de_temporaires` cherche le motif dans la LIGNE, et
    /// la ligne de la signature l'aurait porté. Il a rougi dessus, et il avait
    /// raison de le faire — la même règle vaudra pour le prochain.
    #[test]
    fn la_racine_vit_sous_le_dossier_temporaire() {
        let racine = racine_de_travail("tune-convert");
        // tmp-autorise: rien n'est créé ici, on LIT la racine pour la comparer.
        let attendue = std::env::temp_dir();
        assert!(
            racine.starts_with(&attendue),
            "{racine:?} n'est pas sous {attendue:?}"
        );
        assert_eq!(
            racine.file_name().unwrap().to_string_lossy(),
            format!("tune-convert-{}", uid_courant())
        );
    }
}
