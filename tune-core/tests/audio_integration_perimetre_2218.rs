//! Aucun fabricant de fichier dans `audio_integration.rs` sans entrée dans la
//! table — pour que le prochain dise, dès sa naissance, où le PCM de son format
//! est prouvé.
//!
//! # Pourquoi
//!
//! `tests/audio_integration.rs` fabrique lui-même toutes ses entrées : six
//! `create_test_*`, zéro référence à `tests/fixtures/`. Pour ce qu'il éprouve —
//! la dégradation propre — c'est la bonne méthode, et son en-tête le dit
//! désormais. Le danger n'est pas là : il est dans le fabricant SUIVANT, posé
//! sans un mot, qui rendra un format vert sans que sa justesse soit mesurée
//! nulle part. C'est ainsi que WavPack a rendu du bruit blanc pendant trois
//! mois derrière un en-tête de 32 octets écrit à la main (#3849).
//!
//! Cette garde ne lit que du TEXTE (`include_str!`). Elle ne décode rien, ne
//! demande aucune caractéristique de compilation, et vit donc dans le foyer qui
//! tourne sur toute PR Rust (`tune-core`, hors `outputs::local`).
//!
//! # Ce qu'elle ne couvre PAS — délibérément
//!
//! Elle mesure le LIEN entre un fabricant et sa ligne de table, rien d'autre.
//! Hors de portée, et assumé :
//!
//! * elle ne vérifie pas qu'une preuve existe, seulement qu'une entrée
//!   l'AFFIRME ou avoue son absence (troisième champ vide). Elle force à
//!   déclarer, pas à mesurer ;
//! * le motif est étroit : une ligne dont la partie utile commence par
//!   `fn create_test_`, précédée au plus de `pub ` ou `pub(crate) `. Un
//!   `async fn`, un `unsafe fn`, une signature étalée sur deux lignes ou un
//!   fabricant engendré par macro lui échappent ;
//! * elle ne regarde QUE `audio_integration.rs`. Les autres fichiers de
//!   `tests/` fabriquent aussi leurs entrées et ne sont pas gardés ici ;
//! * `create_test_` est le seul préfixe reconnu. Un `make_test_wav` ou un
//!   `build_fixture_flac` passe sans un mot ;
//! * elle ne juge pas ce qu'un fabricant produit. Un `create_test_wav` qui
//!   écrirait un DSF reste vert.
//!
//! `tune-core` porte `autotests = false` : ce fichier n'est atteint que parce
//! qu'il est déclaré `mod` dans `tests/integration_contracts.rs`. Sans cette
//! ligne il ne serait JAMAIS compilé et rendrait un vert contre rien — voir
//! `tests/tests_orphelins.rs`, qui garde cette porte-là.
use std::collections::BTreeSet;
use std::path::Path;

/// Le texte gardé, lu au moment de la compilation.
const SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/audio_integration.rs"
));

/// (fabricant, ce qu'il écrit, où la justesse du PCM de ce format est prouvée)
///
/// Le troisième champ est un chemin relatif à `tune-core/`, ou la chaîne vide.
/// **Vide signifie : aucune empreinte contre un décodeur de référence extérieur
/// à ce dépôt, à ce jour.** C'est un constat à corriger, pas un blanc-seing.
/// Au 12/09/2026 les six fabricants pointent tous sur une preuve : le champ
/// vide reste ouvert pour le PROCHAIN format, qui n'en aura pas.
const FABRICANTS: &[(&str, &str, &str)] = &[
    // WAV et AIFF : empreintes posées par la tranche T2 de #2218. Les
    // fabricants ci-dessous restent la bonne méthode pour la DÉGRADATION.
    (
        "create_test_wav",
        "WAV PCM 16 bits / 44,1 kHz stéréo",
        "tests/alac_aiff_wav_empreintes_reference.rs",
    ),
    (
        "create_test_aiff",
        "AIFF PCM 16 bits / 44,1 kHz stéréo",
        "tests/alac_aiff_wav_empreintes_reference.rs",
    ),
    // DSD : empreintes posées par la tranche T3 de #2218, contre
    // `wvunpack --raw`.
    (
        "create_test_dsf",
        "DSF (DSD64, en-tête + données)",
        "tests/dsd_empreintes_reference.rs",
    ),
    (
        "create_test_dff",
        "DFF / DSDIFF (en-tête + données)",
        "tests/dsd_empreintes_reference.rs",
    ),
    // APE : le fixture `.ape` versionné est appairé à son `.wav` de référence,
    // et le témoin compare bit à bit.
    (
        "create_test_ape_header",
        "APE, en-tête seul",
        "tests/ape_fixture_i2505.rs",
    ),
    // WavPack : empreintes `wvunpack 5.6.0`, posées par le correctif du bruit
    // blanc (#3849).
    (
        "create_test_wavpack_header",
        "WavPack, en-tête de 32 octets",
        "src/audio/wavpack.rs",
    ),
];

/// Les `create_test_*` réellement écrits dans `audio_integration.rs`.
fn fabricants_declares() -> BTreeSet<String> {
    SOURCE
        .lines()
        .filter_map(|ligne| {
            let ligne = ligne.trim_start();
            let ligne = ligne
                .strip_prefix("pub(crate) ")
                .or_else(|| ligne.strip_prefix("pub "))
                .unwrap_or(ligne);
            let reste = ligne.strip_prefix("fn create_test_")?;
            let (suffixe, _) = reste.split_once('(')?;
            Some(format!("create_test_{suffixe}"))
        })
        .collect()
}

fn tabules() -> BTreeSet<String> {
    FABRICANTS
        .iter()
        .map(|(nom, _, _)| (*nom).to_owned())
        .collect()
}

fn liste(noms: &[&String]) -> String {
    noms.iter()
        .map(|n| format!("\n  - {n}"))
        .collect::<Vec<_>>()
        .join("")
}

/// La garde : un fabricant sans ligne de table fait rougir ce témoin, et le
/// message le NOMME.
#[test]
fn aucun_fabricant_de_fichier_sans_entree_dans_la_table() {
    let declares = fabricants_declares();
    let tabules = tabules();

    let sans_entree: Vec<&String> = declares.difference(&tabules).collect();
    assert!(
        sans_entree.is_empty(),
        "tests/audio_integration.rs : fabricant(s) de fichier sans entrée dans la \
         table FABRICANTS de tests/audio_integration_perimetre_2218.rs :{}\n\n\
         Ce fichier éprouve la DÉGRADATION PROPRE, pas la justesse du PCM. Un \
         nouveau fabricant doit dire où le PCM de son format est prouvé contre \
         un décodeur de référence extérieur (cf. tests/flac_empreintes_reference.rs, tests/alac_aiff_wav_empreintes_reference.rs), \
         ou laisser le troisième champ vide pour avouer qu'il ne l'est nulle part.",
        liste(&sans_entree)
    );

    let perimees: Vec<&String> = tabules.difference(&declares).collect();
    assert!(
        perimees.is_empty(),
        "table FABRICANTS périmée : entrée(s) qui ne correspondent à aucun \
         fabricant de tests/audio_integration.rs :{}\n\n\
         Le fabricant a été renommé ou retiré : mettre la table à jour.",
        liste(&perimees)
    );
}

/// Une preuve citée doit exister. Sinon la table raconte une couverture qu'elle
/// n'a pas.
#[test]
fn les_preuves_citees_par_la_table_existent() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (fabricant, _, preuve) in FABRICANTS {
        if preuve.is_empty() {
            continue;
        }
        assert!(
            racine.join(preuve).exists(),
            "{fabricant} : la table cite une preuve absente du dépôt : \
             tune-core/{preuve}"
        );
    }
}

/// L'en-tête de `audio_integration.rs` doit continuer à avouer sa limite et à
/// nommer l'endroit où la justesse se prouve.
#[test]
fn l_entete_avoue_ce_que_le_fichier_ne_prouve_pas() {
    let entete: String = SOURCE
        .lines()
        .take_while(|l| l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !entete.is_empty(),
        "tests/audio_integration.rs a perdu son en-tête `//!`"
    );
    for attendu in [
        "dégradation propre",
        "NE prouve PAS",
        "flac_empreintes_reference.rs",
        "audio_integration_perimetre_2218.rs",
    ] {
        assert!(
            entete.contains(attendu),
            "l'en-tête de tests/audio_integration.rs ne porte plus « {attendu} » : \
             il doit dire ce qu'il prouve (la dégradation propre), ce qu'il ne \
             prouve pas (la justesse du PCM), où cela se prouve, et nommer cette garde"
        );
    }
}
