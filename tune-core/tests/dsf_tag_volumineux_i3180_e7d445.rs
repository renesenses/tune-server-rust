//! #3180 — une pochette trop grosse n'emporte plus le titre et l'artiste.
//!
//! # Le défaut
//!
//! `read_dsf_id3v2_raw` refusait EN BLOC tout tag ID3v2 de plus d'un mégaoctet.
//! Dans un `.dsf`, ce tag porte la pochette (`APIC`), et sur un rip SACD elle
//! dépasse couramment le mégaoctet à elle seule. Le refus rendait `None` pour la
//! TOTALITÉ du tag : plus de titre, plus d'artiste, plus d'album, plus de
//! pochette — et `dsf_dff_fallback` retombait sur `path.file_stem()`.
//!
//! Une seule ligne expliquait les deux plaintes du ticket :
//!
//! * Benjithom, fil 1100 — le titre affiché est le NOM DU FICHIER, numéro de
//!   piste compris, et le numéro de piste vaut 0 ;
//! * Pierre M, fil 920 et 1043e — les tags sont ignorés en bloc, et ses albums
//!   DSD n'ont pas de pochette.
//!
//! Il n'y a aucun filet derrière : **lofty 0.24 ne connaît pas le format DSF**
//! (aucune variante `FileType`, le mot n'apparaît nulle part dans ses sources),
//! donc `Probe::read()` échoue sur tout `.dsf` et ce lecteur maison est la seule
//! source de métadonnées du format.
//!
//! # Ce que ce fichier tient
//!
//! 1. Un tag de PLUS d'un mégaoctet rend toujours titre, artiste, album et
//!    numéro de piste. Rejouer l'ancien `return None` fait tomber ce test.
//! 2. La pochette de ce même tag reste extractible — c'est le second symptôme.
//! 3. **Le témoin** : un tag NORMAL (sous le budget) ne change pas d'un octet,
//!    texte et pochette compris. Il est vert des deux côtés de la contre-épreuve
//!    et c'est tout son intérêt : le correctif ne devait rien déplacer là.
//! 4. Une image plus grosse que le plafond de pochette est écartée, SEULE : le
//!    texte survit. C'est la règle qu'on a voulue, pas un effet de bord.
//!
//! # Où vit la fixture
//!
//! Sous `tune-core/tests/fixtures/`, JAMAIS sous `/tmp`, via `scratch_dir_in` —
//! le seul constructeur de dossier de test du dépôt qui nettoie tout seul
//! (#3030). Le fichier est FABRIQUÉ à l'exécution : un tag volumineux se
//! construit en quinze lignes, alors qu'embarquer un vrai rip SACD mettrait des
//! mégaoctets d'image dans l'historique du dépôt pour toujours.
//!
//! `autotests = false` dans `tune-core/Cargo.toml` : la cible `[[test]]` y est
//! déclarée, sans quoi ce fichier ne serait jamais compilé.

use std::path::{Path, PathBuf};
use tune_core::test_scratch::{ScratchDir, scratch_dir_in};

/// Budget du lecteur, repris en dur (`DSF_TAG_READ_BUDGET`, privé au module).
/// Le recopier est délibéré : si la constante bouge, ce test doit rester une
/// mesure d'un tag d'un mégaoctet, pas se retailler en silence sur elle.
const BUDGET: usize = 1_048_576;

/// Plafond du chemin pochette (`DSF_COVER_FRAME_BUDGET` = `MAX_RETAINED_COVER_BYTES`).
const PLAFOND_POCHETTE: usize = 4 * 1024 * 1024;

/// Le dossier de fixtures, sous `tests/fixtures/`, nettoyé à la sortie de portée.
fn dossier() -> ScratchDir {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    scratch_dir_in(racine, "i3180-e7d445")
}

/// Entier syncsafe ID3v2 : sept bits utiles par octet.
fn syncsafe(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// Une trame ID3v2.3 : identifiant, taille gros-boutiste, deux fanions, corps.
fn trame(id: &[u8; 4], corps: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(10 + corps.len());
    v.extend_from_slice(id);
    v.extend_from_slice(&(corps.len() as u32).to_be_bytes());
    v.extend_from_slice(&[0u8; 2]);
    v.extend_from_slice(corps);
    v
}

/// Trame de texte, encodage ISO-8859-1 (octet 0).
fn texte(id: &[u8; 4], valeur: &str) -> Vec<u8> {
    let mut corps = vec![0u8];
    corps.extend_from_slice(valeur.as_bytes());
    trame(id, &corps)
}

/// Trame `APIC` portant `octets` octets d'image.
///
/// Disposition v2.3 : encodage, type MIME terminé par un nul, type d'image,
/// description terminée par un nul, puis les données. Le test ne décode jamais
/// l'image — il vérifie qu'elle ne tue plus le texte autour d'elle.
fn apic(octets: usize) -> Vec<u8> {
    let mut corps = vec![0u8];
    corps.extend_from_slice(b"image/jpeg\0");
    corps.push(3); // couverture avant
    corps.push(0); // description vide
    let debut = corps.len();
    corps.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]); // en-tête JPEG
    corps.resize(debut + octets, 0x5A);
    trame(b"APIC", &corps)
}

/// Un tag ID3v2.3 complet autour des trames données.
fn tag(trames: &[Vec<u8>]) -> Vec<u8> {
    let corps: Vec<u8> = trames.concat();
    let mut t = Vec::with_capacity(10 + corps.len());
    t.extend_from_slice(b"ID3");
    t.extend_from_slice(&[3, 0, 0]); // v2.3, révision 0, aucun fanion
    t.extend_from_slice(&syncsafe(corps.len()));
    t.extend_from_slice(&corps);
    t
}

/// Un `.dsf` minimal mais VRAI : chunk `DSD ` (offset de métadonnées compris),
/// chunk `fmt ` DSD64 stéréo, puis le tag ID3v2 à l'offset annoncé.
fn dsf(tag_id3: &[u8]) -> Vec<u8> {
    let offset: u64 = 92;
    let mut buf = vec![0u8; 92];
    buf[0..4].copy_from_slice(b"DSD ");
    buf[4..12].copy_from_slice(&28u64.to_le_bytes());
    buf[12..20].copy_from_slice(&(92 + tag_id3.len() as u64).to_le_bytes());
    buf[20..28].copy_from_slice(&offset.to_le_bytes());
    buf[28..32].copy_from_slice(b"fmt ");
    buf[32..40].copy_from_slice(&52u64.to_le_bytes());
    buf[40..44].copy_from_slice(&1u32.to_le_bytes());
    buf[44..48].copy_from_slice(&0u32.to_le_bytes());
    buf[48..52].copy_from_slice(&2u32.to_le_bytes());
    buf[52..56].copy_from_slice(&2u32.to_le_bytes()); // stéréo
    buf[56..60].copy_from_slice(&2_822_400u32.to_le_bytes()); // DSD64
    buf[60..64].copy_from_slice(&1u32.to_le_bytes());
    buf[64..72].copy_from_slice(&(2_822_400u64 * 180).to_le_bytes());
    buf.extend_from_slice(tag_id3);
    buf
}

/// Les trames de texte que Benjithom voit disparaître, dans l'ordre du fichier :
/// le titre AVANT l'image, comme l'écrivent les étiqueteurs courants.
fn trames_texte() -> Vec<Vec<u8>> {
    vec![
        texte(b"TIT2", "Man On The Corner"),
        texte(b"TPE1", "Genesis"),
        texte(b"TALB", "Abacab"),
        texte(b"TRCK", "7/10"),
    ]
}

/// Écrit un `.dsf` nommé comme celui du fil 1100 — numéro de piste dans le nom,
/// pour que le repli `file_stem()` soit reconnaissable à l'œil s'il reprend.
fn ecrire(dir: &ScratchDir, nom: &str, image: usize) -> PathBuf {
    let mut trames = trames_texte();
    if image > 0 {
        trames.push(apic(image));
    }
    let chemin = dir.path().join(nom);
    std::fs::write(&chemin, dsf(&tag(&trames))).expect("écriture de la fixture");
    chemin
}

#[test]
fn un_tag_de_plus_d_un_mio_rend_toujours_le_texte() {
    let dir = dossier();
    // 1,5 Mio d'image : le tag dépasse le budget, ce qui était le seul critère
    // de refus. Taille réaliste d'une pochette de rip SACD.
    let chemin = ecrire(&dir, "07 - Man On The Corner.dsf", 1_536 * 1024);
    let taille = std::fs::metadata(&chemin).unwrap().len() as usize;
    assert!(
        taille > BUDGET,
        "fixture trop petite ({taille} octets) : elle ne franchit pas le budget de {BUDGET}"
    );

    let meta = tune_core::metadata::try_read_metadata(&chemin).expect("le .dsf est lu");

    // Le défaut de Benjithom, mot pour mot : le titre valait le nom du fichier.
    assert_eq!(
        meta.title.as_deref(),
        Some("Man On The Corner"),
        "le titre du tag est perdu — le repli sur file_stem() a repris"
    );
    assert_ne!(
        meta.title.as_deref(),
        Some("07 - Man On The Corner"),
        "le titre affiché est le nom du fichier"
    );
    // Le défaut de Pierre M : artiste et album reconstruits depuis le chemin.
    assert_eq!(meta.artist.as_deref(), Some("Genesis"));
    assert_eq!(meta.album.as_deref(), Some("Abacab"));
    // « le numéro de piste vaut 0 » : 10 ne peut venir que de la trame TRCK,
    // aucun nom de fichier ne le porte.
    assert_eq!(meta.track_number, Some(7));
    assert_eq!(meta.total_tracks, Some(10));
}

#[test]
fn la_pochette_d_un_tag_hors_budget_reste_extractible() {
    let dir = dossier();
    let octets = 1_536 * 1024;
    let chemin = ecrire(&dir, "07 - Man On The Corner.dsf", octets);

    let (donnees, mime) = tune_core::library::artwork::extract_cover_art(&chemin)
        .expect("la pochette d'un tag hors budget est extraite");
    assert_eq!(mime, "image/jpeg");
    assert_eq!(
        donnees.len(),
        octets,
        "la pochette rendue n'a pas la taille écrite"
    );
    assert_eq!(&donnees[..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
}

#[test]
fn temoin_un_tag_normal_ne_change_pas_de_comportement() {
    let dir = dossier();
    // 100 Kio d'image : tag largement SOUS le budget, donc lu d'un bloc comme
    // avant le correctif. Ce test doit rester vert des DEUX côtés de la
    // contre-épreuve — c'est ce qui prouve qu'on n'a rien déplacé ici.
    let octets = 100 * 1024;
    let chemin = ecrire(&dir, "07 - Man On The Corner.dsf", octets);
    let taille = std::fs::metadata(&chemin).unwrap().len() as usize;
    assert!(
        taille < BUDGET,
        "le témoin doit rester SOUS le budget ({taille} >= {BUDGET})"
    );

    let meta = tune_core::metadata::try_read_metadata(&chemin).expect("le .dsf est lu");
    assert_eq!(meta.title.as_deref(), Some("Man On The Corner"));
    assert_eq!(meta.artist.as_deref(), Some("Genesis"));
    assert_eq!(meta.album.as_deref(), Some("Abacab"));
    assert_eq!(meta.track_number, Some(7));
    assert_eq!(meta.total_tracks, Some(10));
    assert_eq!(meta.format.as_deref(), Some("dsf"));

    let (donnees, mime) = tune_core::library::artwork::extract_cover_art(&chemin)
        .expect("la pochette d'un tag normal est extraite");
    assert_eq!(mime, "image/jpeg");
    assert_eq!(donnees.len(), octets);
}

#[test]
fn un_dsf_sans_tag_reste_lisible_et_retombe_sur_son_nom() {
    let dir = dossier();
    // `metadata_offset = 0` : l'en-tête DSD annonce qu'il n'y a pas de tag.
    // C'est un cas NORMAL, pas un rejet — le fichier doit rester dans la
    // bibliothèque, avec ses propriétés audio et son nom pour titre.
    let chemin = dir.path().join("09 - Keep It Dark.dsf");
    let mut buf = dsf(&[]);
    buf[20..28].copy_from_slice(&0u64.to_le_bytes());
    std::fs::write(&chemin, &buf).unwrap();

    let meta = tune_core::metadata::try_read_metadata(&chemin).expect("un .dsf nu reste lu");
    assert_eq!(meta.title.as_deref(), Some("09 - Keep It Dark"));
    assert_eq!(meta.sample_rate, Some(2_822_400));
    assert_eq!(meta.channels, Some(2));
}

#[test]
fn une_image_plus_grosse_que_le_plafond_est_ecartee_seule() {
    let dir = dossier();
    // Au-dessus de `MAX_RETAINED_COVER_BYTES` : Tune refuse déjà de tenir une
    // pochette de cette taille en mémoire, quel que soit le conteneur. Le DSF
    // suit la même règle — et surtout, il n'emporte plus le texte avec elle.
    let chemin = ecrire(&dir, "07 - Man On The Corner.dsf", PLAFOND_POCHETTE + 1024);

    let meta = tune_core::metadata::try_read_metadata(&chemin).expect("le .dsf est lu");
    assert_eq!(meta.title.as_deref(), Some("Man On The Corner"));
    assert_eq!(meta.artist.as_deref(), Some("Genesis"));
    assert_eq!(meta.total_tracks, Some(10));

    assert!(
        tune_core::library::artwork::extract_cover_art(&chemin).is_none(),
        "une image au-dessus du plafond ne doit pas être chargée"
    );
}
