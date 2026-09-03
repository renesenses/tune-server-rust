//! #3180 — un tag DSF écarté laisse désormais une trace qui NOMME son motif.
//!
//! # Ce que le silence a coûté
//!
//! Les trois `return None` de `read_dsf_id3v2_raw` ne journalisaient rien. Un
//! fichier dont le tag entier était écarté — parce que la pochette faisait
//! passer le tag au-dessus du mégaoctet — ne laissait AUCUNE trace : ni au scan,
//! ni dans l'export de diagnostic qu'un testeur joint à son fil. Benjithom
//! (fil 1100, 19/07) et Pierre M (fil 920, 04/07) décrivaient le même défaut
//! sans que rien ne les relie, et il est resté invisible deux mois.
//!
//! `lofty` ne connaissant pas le format DSF, il n'y a aucun second lecteur
//! derrière : le rejet est une perte sèche, et il doit se dire.
//!
//! # Les deux bords, ensemble
//!
//! 1. **Un tag réellement écarté PARLE**, au niveau WARN, en nommant son motif.
//! 2. **Un fichier ordinaire est MUET à ce niveau.** C'est l'autre moitié du
//!    contrat, et la plus facile à casser : ce lecteur est appelé une fois par
//!    fichier pendant un scan de dizaines de milliers de fichiers. Un `warn!`
//!    par fichier LU, et non par fichier écarté, noierait l'export de
//!    diagnostic — l'export borne chaque module à un quart de sa fenêtre
//!    (`QUOTA_PAR_MODULE`, #1974), donc un émetteur bavard arrache ses lignes à
//!    tous les autres. Le bord 2 passe EN PREMIER, avant que la capture ne
//!    contienne quoi que ce soit.
//!
//! # Pourquoi un binaire de test à lui seul
//!
//! Même leçon que #2665 et #2890, déjà payée deux fois : `tracing` met en cache
//! POUR TOUT LE PROCESSUS la décision « ce point d'appel intéresse-t-il
//! quelqu'un ? ». Un abonné posé au milieu d'une suite qui tourne en parallèle
//! se voit priver d'évènements de façon imprévisible. Ici l'abonné est GLOBAL et
//! ce fichier ne contient QU'UN test. `autotests = false` dans
//! `tune-core/Cargo.toml` — la cible y est déclarée, sans quoi ce fichier ne
//! serait jamais compilé.

use std::path::Path;
use std::sync::{Arc, Mutex};
use tune_core::test_scratch::scratch_dir_in;

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qu'on aura
/// entre les mains la prochaine fois.
#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn vider(&self) -> String {
        let mut tampon = self.0.lock().unwrap();
        let texte = String::from_utf8_lossy(&tampon).into_owned();
        tampon.clear();
        texte
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Les lignes WARN attribuées au lecteur de métadonnées — l'unité qui compte,
/// puisque le quota de l'export de diagnostic se prend PAR MODULE.
fn lignes_warn(journal: &str) -> Vec<&str> {
    journal
        .lines()
        .filter(|l| l.contains("WARN") && l.contains("tune_core::metadata"))
        .collect()
}

fn syncsafe(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// Un tag ID3v2.3 minimal mais valide : titre, artiste, album.
fn tag_valide() -> Vec<u8> {
    let mut corps = Vec::new();
    for (id, valeur) in [
        (b"TIT2", "Man On The Corner"),
        (b"TPE1", "Genesis"),
        (b"TALB", "Abacab"),
    ] {
        let mut trame = vec![0u8];
        trame.extend_from_slice(valeur.as_bytes());
        corps.extend_from_slice(id);
        corps.extend_from_slice(&(trame.len() as u32).to_be_bytes());
        corps.extend_from_slice(&[0u8; 2]);
        corps.extend_from_slice(&trame);
    }
    let mut t = Vec::new();
    t.extend_from_slice(b"ID3");
    t.extend_from_slice(&[3, 0, 0]);
    t.extend_from_slice(&syncsafe(corps.len()));
    t.extend_from_slice(&corps);
    t
}

/// Un `.dsf` dont l'en-tête DSD annonce un tag à l'offset 92.
fn dsf(charge: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 92];
    buf[0..4].copy_from_slice(b"DSD ");
    buf[4..12].copy_from_slice(&28u64.to_le_bytes());
    buf[12..20].copy_from_slice(&(92 + charge.len() as u64).to_le_bytes());
    buf[20..28].copy_from_slice(&92u64.to_le_bytes());
    buf[28..32].copy_from_slice(b"fmt ");
    buf[32..40].copy_from_slice(&52u64.to_le_bytes());
    buf[40..44].copy_from_slice(&1u32.to_le_bytes());
    buf[44..48].copy_from_slice(&0u32.to_le_bytes());
    buf[48..52].copy_from_slice(&2u32.to_le_bytes());
    buf[52..56].copy_from_slice(&2u32.to_le_bytes());
    buf[56..60].copy_from_slice(&2_822_400u32.to_le_bytes());
    buf[60..64].copy_from_slice(&1u32.to_le_bytes());
    buf[64..72].copy_from_slice(&(2_822_400u64 * 180).to_le_bytes());
    buf.extend_from_slice(charge);
    buf
}

#[test]
fn un_tag_ecarte_parle_et_un_fichier_ordinaire_se_tait() {
    let capture = JournalCapture::default();
    // Niveau WARN : c'est ce qu'un journal ORDINAIRE laisse passer, et donc ce
    // qui atterrit dans l'export qu'un testeur joint à un fil de forum.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let dir = scratch_dir_in(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        "i3180-e7d445-journal",
    );

    // ── Bord 2 d'abord : le fichier ORDINAIRE ────────────────────────────────
    // Un tag valide et petit, plus un `.dsf` sans tag du tout. Ce sont les deux
    // formes que porte une bibliothèque réelle ; ni l'une ni l'autre n'a quoi
    // que ce soit à dire au niveau WARN.
    let sain = dir.path().join("07 - Man On The Corner.dsf");
    std::fs::write(&sain, dsf(&tag_valide())).unwrap();
    let _ = tune_core::metadata::try_read_metadata(&sain);

    let nu = dir.path().join("09 - Keep It Dark.dsf");
    let mut buf = dsf(&[]);
    buf[20..28].copy_from_slice(&0u64.to_le_bytes()); // metadata_offset = 0
    std::fs::write(&nu, &buf).unwrap();
    let _ = tune_core::metadata::try_read_metadata(&nu);

    let journal_ordinaire = capture.vider();
    let ordinaire = lignes_warn(&journal_ordinaire);
    assert!(
        ordinaire.is_empty(),
        "un .dsf ordinaire écrit {} ligne(s) WARN — ce lecteur est appelé une \
         fois par fichier, un scan de 50 000 pistes noierait l'export :\n{}",
        ordinaire.len(),
        ordinaire.join("\n")
    );

    // ── Bord 1 : un tag réellement écarté ────────────────────────────────────
    // L'en-tête DSD annonce un tag à l'offset 92, mais ce qui s'y trouve n'est
    // pas un ID3v2. C'était l'un des trois `return None` muets.
    let casse = dir.path().join("11 - Another Record.dsf");
    std::fs::write(&casse, dsf(b"NOPE-pas-un-tag-id3v2-du-tout")).unwrap();
    let _ = tune_core::metadata::try_read_metadata(&casse);

    let journal = capture.vider();
    let ecarte = lignes_warn(&journal);
    assert_eq!(
        ecarte.len(),
        1,
        "un tag écarté doit laisser UNE ligne WARN, pas {} :\n{journal}",
        ecarte.len()
    );
    let ligne = ecarte[0];
    assert!(
        ligne.contains("dsf_id3v2_tag_ecarte"),
        "la ligne ne nomme pas l'évènement :\n{ligne}"
    );
    assert!(
        ligne.contains("pas_un_tag_id3v2"),
        "la ligne ne nomme pas le MOTIF du rejet :\n{ligne}"
    );
    assert!(
        ligne.contains("Another Record"),
        "la ligne ne nomme pas le fichier écarté :\n{ligne}"
    );
}
