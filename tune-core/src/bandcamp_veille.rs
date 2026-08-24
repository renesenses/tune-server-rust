//! Veille des nouveautés Bandcamp, par comparaison d'empreintes.
//!
//! ## Pourquoi ce détour
//!
//! Les autres services servent un **fil de nouveautés daté** : on le tire une
//! fois, on lit les dates, on a fini. Bandcamp n'en a pas. Sa page publique de
//! discographie donne **titre, lien et pochette — aucune date**.
//!
//! « Nouveau » n'y est donc pas une information qu'on lit : c'est une
//! information qu'on **fabrique**, en comparant ce qu'on voit aujourd'hui à ce
//! qu'on avait vu la fois précédente.
//!
//! ## Ce que cela implique, et qu'il faut dire
//!
//! **Le premier passage ne rend RIEN.** Il enregistre. Un artiste dont on
//! découvre la discographie n'a pas « vingt nouveautés » : il en a zéro, et
//! vingt disques qu'on connaît désormais. Annoncer la discographie entière
//! comme neuve serait le pire résultat possible — la section deviendrait un
//! bruit que personne ne lirait plus.
//!
//! C'est la règle centrale de ce module, et elle est testée en premier.

use std::collections::{BTreeMap, BTreeSet};

/// L'empreinte d'un artiste : les adresses de ses parutions, ordonnées.
///
/// L'**adresse** sert d'identité, pas le titre : un artiste peut renommer un
/// disque, et deux disques peuvent porter le même titre chez deux artistes.
/// L'adresse Bandcamp, elle, est stable et unique.
pub type Empreinte = BTreeSet<String>;

/// Toutes les empreintes connues, par clé d'artiste.
pub type Empreintes = BTreeMap<String, Empreinte>;

/// Combien d'artistes on garde en mémoire.
///
/// Borné : cette table est persistée dans les réglages, et une bibliothèque de
/// plusieurs milliers d'artistes la ferait grossir sans fin.
pub const MAX_ARTISTES: usize = 500;

/// Combien de nouveautés on retient pour un artiste en un passage.
///
/// Un artiste qui publie tout son catalogue d'un coup — une reprise de compte,
/// une migration — ne doit pas remplir la section à lui seul.
pub const MAX_PAR_ARTISTE: usize = 5;

/// Ce qu'un passage de veille produit pour un artiste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    /// Les adresses jamais vues jusqu'ici. **Vide au premier passage.**
    pub nouveautes: Vec<String>,
    /// L'empreinte à conserver pour la prochaine fois.
    pub empreinte: Empreinte,
    /// Vrai quand c'est le premier passage sur cet artiste.
    pub premiere_visite: bool,
}

/// Compare ce qu'on voit à ce qu'on avait vu.
///
/// `precedente` à `None` = premier passage : on enregistre, on n'annonce rien.
pub fn comparer(precedente: Option<&Empreinte>, vues: &[String]) -> Passage {
    let actuelle: Empreinte = vues.iter().cloned().collect();

    let Some(avant) = precedente else {
        return Passage {
            nouveautes: Vec::new(),
            empreinte: actuelle,
            premiere_visite: true,
        };
    };

    // L'ordre de `vues` est celui de la page — les plus récentes d'abord chez
    // Bandcamp. On le conserve plutôt que celui, alphabétique, de l'ensemble.
    let mut nouveautes: Vec<String> = vues
        .iter()
        .filter(|u| !avant.contains(*u))
        .cloned()
        .collect();
    nouveautes.truncate(MAX_PAR_ARTISTE);

    Passage {
        nouveautes,
        empreinte: actuelle,
        premiere_visite: false,
    }
}

/// Range une empreinte, en bornant la table.
///
/// Quand le plafond est atteint, l'artiste le plus « ancien » par ordre de clé
/// saute. Un choix arbitraire assumé : la table ne porte pas de date, et en
/// ajouter une pour départager n'apporterait rien — ce qui compte est que la
/// table reste bornée, pas QUI en sort.
pub fn ranger(empreintes: &mut Empreintes, cle: String, empreinte: Empreinte) {
    if !empreintes.contains_key(&cle)
        && empreintes.len() >= MAX_ARTISTES
        && let Some(premiere) = empreintes.keys().next().cloned()
    {
        empreintes.remove(&premiere);
    }
    empreintes.insert(cle, empreinte);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ens(v: &[&str]) -> Empreinte {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn vec(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// LA regle du module : le premier passage n'annonce rien.
    #[test]
    fn le_premier_passage_nannonce_rien() {
        let p = comparer(None, &vec(&["/album/a", "/album/b", "/album/c"]));
        assert!(
            p.nouveautes.is_empty(),
            "la discographie entiere a ete annoncee comme neuve"
        );
        assert!(p.premiere_visite);
        assert_eq!(p.empreinte.len(), 3, "elle doit tout de meme etre retenue");
    }

    #[test]
    fn seules_les_adresses_jamais_vues_remontent() {
        let avant = ens(&["/album/a", "/album/b"]);
        let p = comparer(Some(&avant), &vec(&["/album/c", "/album/a", "/album/b"]));
        assert_eq!(p.nouveautes, vec!["/album/c".to_string()]);
        assert!(!p.premiere_visite);
    }

    #[test]
    fn rien_de_neuf_ne_rend_rien() {
        let avant = ens(&["/album/a", "/album/b"]);
        let p = comparer(Some(&avant), &vec(&["/album/b", "/album/a"]));
        assert!(p.nouveautes.is_empty());
    }

    /// L'ordre de la PAGE est conserve : Bandcamp met les plus recentes en
    /// tete, et c'est cet ordre qui a du sens a l'ecran.
    #[test]
    fn lordre_de_la_page_est_conserve() {
        let avant = ens(&["/album/vieux"]);
        let p = comparer(
            Some(&avant),
            &vec(&["/album/z", "/album/a", "/album/vieux"]),
        );
        assert_eq!(
            p.nouveautes,
            vec!["/album/z".to_string(), "/album/a".to_string()]
        );
    }

    /// Un artiste qui publie tout d'un coup ne doit pas remplir la section.
    #[test]
    fn un_artiste_ne_peut_pas_remplir_la_section() {
        let avant = ens(&["/album/vieux"]);
        let beaucoup: Vec<String> = (0..40).map(|i| format!("/album/n{i}")).collect();
        let p = comparer(Some(&avant), &beaucoup);
        assert_eq!(p.nouveautes.len(), MAX_PAR_ARTISTE);
        // Mais l'empreinte, elle, retient TOUT : sinon les disques ecartes
        // reviendraient comme « nouveaux » au passage suivant, indefiniment.
        assert_eq!(p.empreinte.len(), 40);
    }

    /// Une parution RETIREE de Bandcamp ne doit pas rendre les autres neuves.
    #[test]
    fn une_parution_retiree_ne_perturbe_rien() {
        let avant = ens(&["/album/a", "/album/b", "/album/c"]);
        let p = comparer(Some(&avant), &vec(&["/album/a", "/album/c"]));
        assert!(p.nouveautes.is_empty());
        assert_eq!(p.empreinte.len(), 2, "l'empreinte suit ce qui existe");
    }

    /// Une page vide — Bandcamp change sa mise en page sans preavis — ne doit
    /// pas effacer ce qu'on savait au point de tout re-annoncer ensuite.
    ///
    /// Ce test documente une LIMITE connue : l'empreinte devient vide, donc le
    /// passage suivant reannoncera la discographie. C'est a l'appelant de ne
    /// pas ranger une empreinte vide ; le module, lui, ne devine pas.
    #[test]
    fn une_page_vide_rend_une_empreinte_vide() {
        let avant = ens(&["/album/a"]);
        let p = comparer(Some(&avant), &[]);
        assert!(p.nouveautes.is_empty());
        assert!(p.empreinte.is_empty());
    }

    #[test]
    fn la_table_reste_bornee() {
        let mut t = Empreintes::new();
        for i in 0..(MAX_ARTISTES + 20) {
            ranger(&mut t, format!("artiste{i:04}"), ens(&["/album/x"]));
        }
        assert_eq!(t.len(), MAX_ARTISTES);
    }

    /// Ranger un artiste DEJA connu ne doit pas evincer quelqu'un.
    #[test]
    fn mettre_a_jour_un_artiste_connu_nevince_personne() {
        let mut t = Empreintes::new();
        for i in 0..MAX_ARTISTES {
            ranger(&mut t, format!("artiste{i:04}"), ens(&["/album/x"]));
        }
        ranger(&mut t, "artiste0000".into(), ens(&["/album/x", "/album/y"]));
        assert_eq!(t.len(), MAX_ARTISTES);
        assert_eq!(t.get("artiste0000").map(|e| e.len()), Some(2));
    }
}
