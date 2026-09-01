//! Chemins temporaires **uniques par appel et nettoyés tout seuls**, pour les
//! tests.
//!
//! # Pourquoi ce module existe
//!
//! Les tests d'un même binaire tournent **en parallèle** : `cargo test`
//! ouvre un fil par cœur. Un chemin temporaire construit à partir du seul
//! `std::process::id()` isole donc les **processus**, jamais les tests
//! entre eux — deux tests du même binaire visent alors le même fichier, et
//! l'un le supprime pendant que l'autre le lit.
//!
//! C'est le défaut mesuré de l'issue #2864 : `round_trip_16_bit_stereo_is_bit_exact`
//! et `exact_packet_boundary_has_no_tail_garbage` partageaient le triplet
//! `(16, 2, 44100)`, donc le même `rt-16-2-44100.m4a`. Reproduit sur Shrek
//! avant correctif : le cas à 8192 trames décodait 150000 échantillons,
//! c'est-à-dire exactement les 75000 trames de l'AUTRE test.
//!
//! Un test instable est pire qu'un test absent : il apprend à relancer la
//! CI au lieu de lire l'échec. Ce module existe pour que la collision soit
//! **impossible** plutôt qu'improbable — et pour que le reste du dépôt ne
//! refasse pas le défaut chemin par chemin.
//!
//! # L'unicité n'est pas le nettoyage (#3030)
//!
//! #2864 a rendu les noms uniques ; il n'a rien supprimé. Chaque exécution
//! ajoutait donc sa couche : mesuré sur Shrek le 31/08/2026, **3 204 entrées
//! `/tmp/tune-*` pour 1,2 Gio**, dont 636 nées dans la seule matinée, et
//! 2 569 vieilles de plus de 24 h sans aucun processus vivant derrière. Le
//! `remove_dir_all` posé en fin de fonction ne rattrape rien : c'est
//! précisément le test **qui échoue** qui laisse le plus de résidus, et la
//! panique saute la dernière ligne.
//!
//! D'où [`ScratchDir`] : le dossier est supprimé par `Drop`, donc aussi
//! pendant le déroulage de pile d'une panique. L'appelant n'a plus rien à
//! écrire — et n'a plus rien à oublier.
//!
//! # Ce qu'il ne faut PAS faire à la place
//!
//! - `--test-threads=1` masque la collision en ralentissant tout le monde.
//! - Un suffixe `process::id()` seul ne sépare que les binaires.
//! - Un nom de fichier déduit des *paramètres* du cas (profondeur, canaux,
//!   cadence…) collisionne dès que deux cas partagent ces paramètres.
//! - `std::env::temp_dir().join(format!(…))` à la main : le chemin survit au
//!   test. Le garde-fou `tune-core/tests/aucune_fuite_de_temporaires.rs`
//!   refuse désormais ce motif dans du code de test.
//!
//! # Pourquoi pas `tempfile::TempDir`
//!
//! `tempfile` est une **dev-dependency** du dépôt, alors que ce module-ci est
//! compilé dans la bibliothèque livrée (`tune-server` s'en sert depuis ses
//! propres `#[cfg(test)]`, qui lient un `tune_core` construit hors mode test).
//! Le faire passer en dépendance de production pour un défaut d'hygiène de
//! tests serait payer trop cher ; le garde tient en trente lignes et conserve
//! l'**étiquette** dans le nom, ce que le suffixe aléatoire de `TempDir` ne
//! donne pas — or c'est l'étiquette qui dit à qui appartient un résidu.
//!
//! # Usage
//!
//! ```ignore
//! let dir = crate::test_scratch::scratch_dir("tune-aac-test");
//! std::fs::write(dir.join("rt.m4a"), &octets).unwrap();
//! // Rien à supprimer : `dir` emporte le dossier en sortant de portée,
//! // panique comprise.
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Le compteur qui rend le nom unique. `Relaxed` suffit : on ne demande
/// aucun ordre entre fils, seulement que deux `fetch_add` rendent deux
/// valeurs distinctes — ce que l'atomicité garantit à elle seule.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Un **nom** unique, à poser où l'appelant veut.
///
/// Utile quand le test exige que le fichier vive à la racine de
/// `temp_dir()` — l'éviction du cache de transcodage y balaie, par
/// exemple : lui donner un sous-dossier lui retirerait sa substance.
///
/// Le `process::id()` sépare les binaires concurrents (plusieurs agents
/// sur la même machine), le compteur sépare les tests d'un même binaire.
/// Il faut les DEUX.
///
/// ⚠️ Ce n'est qu'un **nom** : rien n'est créé, donc rien n'est nettoyé.
/// Pour un dossier, prendre [`scratch_dir`], qui se supprime tout seul.
pub fn scratch_name(etiquette: &str) -> String {
    format!(
        "{etiquette}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Un dossier temporaire à soi, **supprimé à la sortie de portée**.
///
/// Le nettoyage est en `Drop`, donc il a lieu aussi quand le test panique :
/// c'est tout l'objet de #3030. Un `remove_dir_all` écrit en fin de fonction
/// ne s'exécute, lui, que si le test réussit — c'est-à-dire jamais dans le
/// cas qui laisse le plus de résidus.
///
/// Se déréférence en [`Path`] : `dir.join("x")`, `&dir`, `dir.display()`
/// s'écrivent comme sur un `PathBuf`.
pub struct ScratchDir {
    chemin: PathBuf,
}

impl ScratchDir {
    /// Le chemin du dossier. `Deref` rend l'appel rarement nécessaire.
    pub fn path(&self) -> &Path {
        &self.chemin
    }

    /// Renonce **explicitement** au nettoyage automatique et rend le chemin.
    ///
    /// À n'employer que si quelque chose d'autre supprime le dossier. Le nom
    /// est délibérément visible dans un diff : une fuite doit se décider,
    /// pas se produire.
    pub fn renoncer_au_nettoyage(self) -> PathBuf {
        let chemin = self.chemin.clone();
        std::mem::forget(self);
        chemin
    }
}

impl std::ops::Deref for ScratchDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.chemin
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.chemin
    }
}

/// Pour que le garde passe tel quel là où l'on passait un `PathBuf` : une
/// variable d'environnement de sous-processus, un argument de commande.
impl AsRef<std::ffi::OsStr> for ScratchDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.chemin.as_os_str()
    }
}

impl std::fmt::Debug for ScratchDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.chemin, f)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Le résultat est ignoré à dessein : un test qui a déjà supprimé son
        // dossier lui-même reste légitime, et une panique en cours de
        // déroulage ne doit pas être doublée d'une seconde.
        let _ = std::fs::remove_dir_all(&self.chemin);
    }
}

/// Un **fichier** temporaire à la racine de `temp_dir()`, supprimé à la
/// sortie de portée.
///
/// Certains tests exigent que le fichier vive à la racine et non dans un
/// sous-dossier : l'éviction du cache de transcodage y balaie, un
/// sous-dossier lui retirerait sa substance. `ScratchFile` leur donne le
/// même garde que [`ScratchDir`] sans leur imposer un dossier.
///
/// Le fichier n'est **pas** créé — l'appelant l'écrit, le copie ou le laisse
/// absent. Seule sa suppression est prise en charge.
pub struct ScratchFile {
    chemin: PathBuf,
}

impl ScratchFile {
    /// Le chemin du fichier. `Deref` rend l'appel rarement nécessaire.
    pub fn path(&self) -> &Path {
        &self.chemin
    }

    /// Le chemin en `&str` — la forme que réclament la plupart des API de
    /// décodage du dépôt.
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        self.chemin.to_string_lossy()
    }
}

impl std::ops::Deref for ScratchFile {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.chemin
    }
}

impl AsRef<Path> for ScratchFile {
    fn as_ref(&self) -> &Path {
        &self.chemin
    }
}

impl AsRef<std::ffi::OsStr> for ScratchFile {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.chemin.as_os_str()
    }
}

impl std::fmt::Debug for ScratchFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.chemin, f)
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.chemin);
    }
}

/// Un **dossier** à soi, jamais partagé, créé tout de suite et supprimé tout
/// seul — le cas courant.
pub fn scratch_dir(etiquette: &str) -> ScratchDir {
    scratch_dir_in(std::env::temp_dir(), etiquette)
}

/// Un **fichier** unique à la racine de `temp_dir()`, supprimé à la sortie de
/// portée.
///
/// `suffixe` est collé tel quel derrière le nom unique : il porte
/// l'extension, que les analyseurs de format lisent (`".flac"`, `".ogg"`),
/// et elle doit donc rester en dernier.
pub fn scratch_file(etiquette: &str, suffixe: &str) -> ScratchFile {
    ScratchFile {
        chemin: std::env::temp_dir().join(format!("{}{suffixe}", scratch_name(etiquette))),
    }
}

/// Le même, sous une racine imposée.
///
/// Sert aux tests qui exigent `/tmp` littéral et non `std::env::temp_dir()` :
/// sous macOS ce dernier vit sous `/private/var`, hors du périmètre que
/// certaines gardes de chemin acceptent — le refus attendu tomberait alors
/// pour la mauvaise raison.
pub fn scratch_dir_in(racine: impl AsRef<Path>, etiquette: &str) -> ScratchDir {
    let chemin = racine.as_ref().join(scratch_name(etiquette));
    std::fs::create_dir_all(&chemin)
        .unwrap_or_else(|e| panic!("création du dossier de test {} : {e}", chemin.display()));
    ScratchDir { chemin }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La contre-épreuve permanente : deux appels de **même étiquette** ne
    /// doivent jamais rendre le même chemin. Rejouer cette assertion avec
    /// l'ancien motif (`format!("truc-{}", process::id())`) la fait échouer
    /// sur-le-champ, puisque le pid ne bouge pas d'un appel à l'autre.
    #[test]
    fn deux_appels_de_meme_etiquette_ne_partagent_jamais_de_chemin() {
        let a = scratch_dir("meme-etiquette");
        let b = scratch_dir("meme-etiquette");
        assert_ne!(a.path(), b.path(), "deux appels partagent un dossier");
        assert_ne!(
            scratch_name("meme-etiquette"),
            scratch_name("meme-etiquette"),
            "deux appels de même étiquette partagent un nom"
        );
    }

    /// Et sous charge parallèle, pas seulement en séquence : mille appels
    /// répartis sur huit fils doivent rendre mille chemins distincts. C'est
    /// exactement la condition dans laquelle la collision #2864 se
    /// produisait.
    #[test]
    fn mille_appels_sur_huit_fils_rendent_mille_chemins_distincts() {
        let fils: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..125)
                        .map(|_| scratch_name("course"))
                        .collect::<Vec<String>>()
                })
            })
            .collect();
        let tous: Vec<String> = fils.into_iter().flat_map(|f| f.join().unwrap()).collect();
        assert_eq!(tous.len(), 1000);
        let uniques: std::collections::HashSet<&String> = tous.iter().collect();
        assert_eq!(
            uniques.len(),
            1000,
            "{} noms en double sous charge parallèle",
            1000 - uniques.len()
        );
    }

    /// Le chemin reste sous `temp_dir()` et porte son étiquette : sans quoi
    /// on ne saurait plus à quel test appartient un résidu dans `/tmp`.
    #[test]
    fn le_chemin_reste_sous_temp_dir_et_porte_son_etiquette() {
        let d = scratch_dir("etiquette-lisible");
        assert!(d.starts_with(std::env::temp_dir()));
        assert!(
            d.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("etiquette-lisible-"),
            "étiquette perdue : {d:?}"
        );
    }

    /// Compte les résidus portant une étiquette donnée dans `temp_dir()`.
    /// Étiquette + pid + compteur : aucun autre binaire de test, aucun autre
    /// agent de la machine ne peut entrer dans ce compte.
    fn residus(etiquette: &str) -> usize {
        let prefixe = format!("{etiquette}-{}-", std::process::id());
        std::fs::read_dir(std::env::temp_dir())
            .map(|entrees| {
                entrees
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with(&prefixe))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Le crochet de panique est un état **de processus**. Deux témoins le
    /// remplacent le temps d'une panique attendue ; sans ce verrou, ils
    /// pourraient s'entrelacer et laisser le crochet muet en place — les
    /// échecs des tests voisins du même binaire deviendraient alors
    /// silencieux, ce qui est exactement le genre de dégât qu'on répare ici.
    static CROCHET: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Joue une panique en silence et rend l'issue de `catch_unwind`.
    fn panique_silencieuse(corps: impl FnOnce() + std::panic::UnwindSafe) -> bool {
        let _garde = CROCHET.lock().unwrap_or_else(|e| e.into_inner());
        let precedent = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let issue = std::panic::catch_unwind(corps);
        std::panic::set_hook(precedent);
        issue.is_err()
    }

    /// Le dossier existe **pendant** le test : sans quoi le garde protégerait
    /// un dossier que personne ne pourrait utiliser.
    #[test]
    fn le_dossier_est_cree_et_utilisable_immediatement() {
        let d = scratch_dir("temoin-cree");
        assert!(d.is_dir(), "dossier non créé : {d:?}");
        std::fs::write(d.join("fichier"), b"contenu").expect("écriture dans le dossier");
        assert_eq!(residus("temoin-cree"), 1);
    }

    /// LE témoin de #3030, cas nominal : rien ne survit à la sortie de portée.
    ///
    /// Rejoué sur l'arbre d'avant correctif — `scratch_dir` rendant un
    /// `PathBuf` nu — ce compte vaut 1 au lieu de 0 : c'est exactement la
    /// couche que chaque exécution ajoutait à `/tmp`.
    #[test]
    fn rien_ne_survit_a_la_sortie_de_portee() {
        assert_eq!(residus("temoin-portee"), 0, "résidu antérieur au test");
        {
            let d = scratch_dir("temoin-portee");
            std::fs::write(d.join("f"), b"x").unwrap();
            std::fs::create_dir_all(d.join("sous/dossier")).unwrap();
            assert_eq!(residus("temoin-portee"), 1);
        }
        assert_eq!(
            residus("temoin-portee"),
            0,
            "le dossier a survécu à la sortie de portée"
        );
    }

    /// LE témoin de #3030, cas qui compte vraiment : **la panique aussi**.
    ///
    /// C'est le test qui échoue qui laisse le plus de résidus, et c'est
    /// précisément celui qu'un `remove_dir_all` de fin de fonction ne
    /// nettoie jamais. Le garde étant en `Drop`, il s'exécute pendant le
    /// déroulage de pile.
    #[test]
    fn rien_ne_survit_a_une_panique() {
        assert_eq!(residus("temoin-panique"), 0, "résidu antérieur au test");
        let a_panique = panique_silencieuse(|| {
            let d = scratch_dir("temoin-panique");
            std::fs::write(d.join("f"), b"x").unwrap();
            panic!("échec simulé");
        });
        assert!(a_panique, "la panique simulée n'a pas eu lieu");
        assert_eq!(
            residus("temoin-panique"),
            0,
            "le dossier a survécu à la panique — c'est la fuite de #3030"
        );
    }

    /// Même témoin pour le fichier : il ne survit ni à la portée ni à la
    /// panique, et son extension reste en dernier.
    #[test]
    fn le_fichier_ne_survit_ni_a_la_portee_ni_a_la_panique() {
        {
            let f = scratch_file("temoin-fichier", ".flac");
            assert!(
                f.extension().is_some_and(|e| e == "flac"),
                "extension perdue : {f:?}"
            );
            std::fs::write(&*f, b"x").unwrap();
            assert_eq!(residus("temoin-fichier"), 1);
        }
        assert_eq!(residus("temoin-fichier"), 0, "le fichier a survécu");

        let a_panique = panique_silencieuse(|| {
            let f = scratch_file("temoin-fichier-panique", ".bin");
            std::fs::write(&*f, b"x").unwrap();
            panic!("échec simulé");
        });
        assert!(a_panique, "la panique simulée n'a pas eu lieu");
        assert_eq!(
            residus("temoin-fichier-panique"),
            0,
            "le fichier a survécu à la panique"
        );
    }

    /// La sortie de secours reste possible, mais elle doit se voir.
    #[test]
    fn renoncer_au_nettoyage_conserve_le_dossier() {
        let chemin = scratch_dir("temoin-renonce").renoncer_au_nettoyage();
        assert!(chemin.is_dir(), "le dossier conservé a été supprimé");
        std::fs::remove_dir_all(&chemin).unwrap();
    }
}
