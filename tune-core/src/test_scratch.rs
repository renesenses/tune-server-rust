//! Chemins temporaires **uniques par appel**, pour les tests.
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
//! # Ce qu'il ne faut PAS faire à la place
//!
//! - `--test-threads=1` masque la collision en ralentissant tout le monde.
//! - Un suffixe `process::id()` seul ne sépare que les binaires.
//! - Un nom de fichier déduit des *paramètres* du cas (profondeur, canaux,
//!   cadence…) collisionne dès que deux cas partagent ces paramètres.
//!
//! # Usage
//!
//! ```ignore
//! let dir = crate::test_scratch::scratch_dir("tune-aac-test");
//! std::fs::create_dir_all(&dir).unwrap();
//! // …
//! let _ = std::fs::remove_dir_all(&dir);
//! ```

use std::path::PathBuf;
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
pub fn scratch_name(etiquette: &str) -> String {
    format!(
        "{etiquette}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Un **dossier** à soi, jamais partagé — le cas courant.
///
/// N'est pas créé : l'appelant fait son `create_dir_all`, et son
/// `remove_dir_all` en partant. Comme le dossier lui appartient en propre,
/// le nettoyage ne peut plus emporter le fichier d'un autre test.
pub fn scratch_dir(etiquette: &str) -> PathBuf {
    std::env::temp_dir().join(scratch_name(etiquette))
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
        assert_ne!(a, b, "deux appels de même étiquette partagent un dossier");
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
                        .map(|_| scratch_dir("course"))
                        .collect::<Vec<PathBuf>>()
                })
            })
            .collect();
        let tous: Vec<PathBuf> = fils.into_iter().flat_map(|f| f.join().unwrap()).collect();
        assert_eq!(tous.len(), 1000);
        let uniques: std::collections::HashSet<&PathBuf> = tous.iter().collect();
        assert_eq!(
            uniques.len(),
            1000,
            "{} chemins en double sous charge parallèle",
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
}
