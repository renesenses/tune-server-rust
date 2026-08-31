//! Ce que le journal dit — et ne dit pas — quand le scanner insère un lot (#2890).
//!
//! Frère du garde `journal_descriptif_illisible.rs` (#2665), et pour la même
//! raison de fond : **la seule chose qu'on aura entre les mains la prochaine
//! fois, c'est le journal**. Ce fichier-ci verrouille l'autre bord du problème.
//! Là-bas, une trace ne disait pas assez ; ici, une trace en disait trop.
//!
//! ## Ce qui vivait dans le code livré
//!
//! `TrackRepo::create_batch`, en tête de sa boucle sur les pistes, portait
//! depuis le 01/07/2026 (commit `e57c9acc`, message `diag:`) une sonde de
//! débogage : un `tracing::warn!` complet dès que le titre de la piste
//! contenait « personal jesus ».
//!
//! `warn!` est un niveau **livré**. L'export de diagnostic borne chaque module
//! à un quart de la fenêtre (`QUOTA_PAR_MODULE`, #1974) : un émetteur d'une
//! ligne par piste prend donc jusqu'à 250 lignes sur 1000, arrachées à tous
//! les autres modules — et `tune_core::db::track_repo` est précisément celui
//! qu'on lit quand un scan perd des pistes (#2939). Mesuré sur du terrain : un
//! seul émetteur par piste (`replaygain_skipped_oversized`) occupait 32,7 %
//! d'un export de testeur réel avant que le quota n'existe.
//!
//! ## Les deux bords que ce test tient ensemble
//!
//! 1. **Un échec d'insertion réel reste DIT au niveau WARN.** C'est le témoin,
//!    et il passe EN PREMIER : rendre la fonction muette serait pire que le
//!    bruit. Cet avertissement-ci a été ajouté parce que le scanner annonçait
//!    « files=N errors=0 » pendant que les pistes ne rentraient jamais
//!    (JP Borderies, ~205 pistes en base pour ~779 sur le disque). Ses titres
//!    ne mordent aucun prédicat : il est donc VERT DES DEUX CÔTÉS de la
//!    contre-épreuve, ce qui est tout son intérêt.
//! 2. **Le lot nominal est SILENCIEUX au niveau WARN.** C'est le défaut de
//!    #2890 : un scan qui se passe bien n'a rien à dire à ce niveau. Mesuré
//!    avant correctif : **8 lignes WARN pour un lot de 500 pistes**.
//!
//! ## Pourquoi un binaire de test à lui seul
//!
//! Même leçon que #2665, déjà payée : `tracing` met en cache **pour tout le
//! processus** la décision « ce point d'appel intéresse-t-il quelqu'un ? » et
//! le niveau maximal utile. Un abonné posé au milieu d'une suite qui tourne en
//! parallèle se voit priver d'évènements de façon imprévisible. Ici l'abonné
//! est **global** et ce fichier ne contient **qu'un test** : il est installé
//! avant toute autre chose et le résultat ne dépend d'aucun ordonnancement.
//! `autotests = false` dans `tune-core/Cargo.toml` — la cible est déclarée
//! là-bas, sans quoi ce fichier ne serait jamais compilé.

use std::sync::{Arc, Mutex};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Artist, Track};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;

/// Taille réelle d'un lot du scanner (`tune_core::scanner::walker::SCAN_BATCH_SIZE`).
/// La reprendre en dur ici est délibéré : si la constante bouge, ce test doit
/// rester une mesure d'un lot de 500, pas se retailler en silence.
const PISTES_PAR_LOT: usize = 500;

/// Les variantes de « Personal Jesus » qu'une bibliothèque de collectionneur
/// contient réellement : l'original, les remixes du maxi, une réédition, et
/// trois reprises. Le test de la sonde était un `contains` sur le titre **en
/// minuscules** — toutes celles-ci mordaient, y compris celles où le titre
/// n'est même pas de Depeche Mode.
const VARIANTES: &[&str] = &[
    "Personal Jesus",
    "Personal Jesus (Pump Mix)",
    "Personal Jesus (Holier Than Thou Approach)",
    "Personal Jesus (Acoustic)",
    "personal jesus (live)",
    "Personal Jesus 2011 (Alex Metric Remix)",
    "Personal Jesus 2011 (The Stargate Mix)",
    "PERSONAL JESUS",
];

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qu'on aura
/// entre les mains la prochaine fois.
#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    /// Vide la capture et rend ce qu'elle contenait — pour mesurer une phase
    /// sans traîner les lignes de la précédente.
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

/// Compte les lignes de niveau WARN attribuées au dépôt de pistes — c'est
/// l'unité qui compte, puisque le quota d'export se prend PAR MODULE.
fn lignes_warn(journal: &str) -> Vec<&str> {
    journal
        .lines()
        .filter(|l| l.contains("WARN") && l.contains("tune_core::db::track_repo"))
        .collect()
}

#[test]
fn un_lot_de_scan_nominal_n_ecrit_aucune_ligne_warn_par_piste() {
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

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let artist_id = ArtistRepo::new(db.clone())
        .create(&Artist::new("Depeche Mode".into()))
        .unwrap();
    let album_id = AlbumRepo::new(db.clone())
        .get_or_create("Violator", artist_id, Some(1990))
        .unwrap()
        .id
        .unwrap();
    let repo = TrackRepo::new(db.clone());

    // ── 1er temps : le TÉMOIN — un échec réel doit rester DIT ─────────────
    //
    // Il passe EN PREMIER délibérément, et sur des titres qui ne mordent
    // aucun prédicat : c'est ce qui le rend VERT DES DEUX CÔTÉS de la
    // contre-épreuve. Sans lui, ce fichier passerait aussi bien sur une
    // fonction rendue complètement muette — pire que le défaut corrigé :
    // c'est exactement ce silence qui avait fait annoncer « files=N errors=0 »
    // pendant que ~574 pistes ne rentraient jamais en base.
    let mut premiere = Track::new("Enjoy the Silence".into());
    premiere.album_id = Some(album_id);
    premiere.artist_id = Some(artist_id);
    premiere.file_path = Some("/music/temoin/a.flac".into());
    assert_eq!(repo.create_batch(&[premiere]).unwrap(), 1);

    // On rejoue un `file_path` déjà pris : `file_path` est UNIQUE, la ligne
    // échoue, l'autre passe (`execute_many` préserve les erreurs par ligne).
    let mut doublon = Track::new("Enjoy the Silence".into());
    doublon.album_id = Some(album_id);
    doublon.artist_id = Some(artist_id);
    doublon.file_path = Some("/music/temoin/a.flac".into());

    let mut neuve = Track::new("Waiting for the Night".into());
    neuve.album_id = Some(album_id);
    neuve.artist_id = Some(artist_id);
    neuve.file_path = Some("/music/temoin/b.flac".into());

    let rescapees = repo.create_batch(&[doublon, neuve]).unwrap();
    assert_eq!(
        rescapees, 1,
        "une seule des deux pistes pouvait rentrer : sans ça le témoin ne \
         teste rien"
    );

    let journal = capture.vider();
    let dits = lignes_warn(&journal);
    assert_eq!(
        dits.len(),
        1,
        "un échec d'insertion doit produire EXACTEMENT une ligne WARN, ni \
         zéro (la perte de piste redevient invisible) ni plusieurs (le \
         nettoyage n'aurait rien réglé).\njournal capturé :\n{journal}"
    );
    assert!(
        dits[0].contains("track_insert_failed_in_batch"),
        "l'avertissement légitime doit garder son nom d'évènement : c'est ce \
         qu'on grep dans l'export d'un testeur : {}",
        dits[0]
    );
    assert!(
        dits[0].contains("/music/temoin/a.flac"),
        "sans le chemin du fichier, la trace ne permet pas d'agir : {}",
        dits[0]
    );

    // ── 2e temps : un lot NOMINAL, tel que le scanner en pousse ───────────
    //
    // 500 pistes, toutes valides, dont les variantes de « Personal Jesus »
    // qu'un collectionneur possède vraiment. Rien à signaler : aucune ne doit
    // produire la moindre ligne au niveau livré.
    let mut lot: Vec<Track> = Vec::with_capacity(PISTES_PAR_LOT);
    for i in 0..PISTES_PAR_LOT {
        let titre = if i < VARIANTES.len() {
            VARIANTES[i].to_string()
        } else {
            format!("Piste ordinaire {i}")
        };
        let mut piste = Track::new(titre);
        piste.album_id = Some(album_id);
        piste.artist_id = Some(artist_id);
        piste.album_artist = Some("Depeche Mode".into());
        piste.file_path = Some(format!("/music/Violator/{i:03}.flac"));
        lot.push(piste);
    }

    let inserees = repo.create_batch(&lot).unwrap();
    assert_eq!(
        inserees, PISTES_PAR_LOT,
        "le lot nominal doit rentrer en entier, sinon la mesure de bruit \
         ci-dessous porterait sur des échecs et non sur le cas normal"
    );

    let journal = capture.vider();
    let bruit = lignes_warn(&journal);
    assert!(
        bruit.is_empty(),
        "un lot de scan qui se passe BIEN a écrit {} ligne(s) de niveau WARN \
         sur {PISTES_PAR_LOT} pistes ({} variantes de titre sur le chemin \
         nominal). L'export de diagnostic borne ce module à un quart de la \
         fenêtre (QUOTA_PAR_MODULE, #1974) : {} lignes prises sur 1000, \
         arrachées aux modules qu'on cherchait vraiment à lire.\n\
         Lignes émises :\n{}",
        bruit.len(),
        VARIANTES.len(),
        bruit.len().min(250),
        bruit.join("\n")
    );

    // ── 3e temps : un lot qui échoue EN ENTIER reste borné ────────────────
    //
    // Jumelle du défaut de #2890, dans la même fonction : un lot échoue pour
    // UNE cause — FK périmée, base verrouillée, disque plein — répétée 500
    // fois à l'identique. Détailler les 500 mange le quart de fenêtre du
    // module. On rejoue donc le lot précédent en entier : chaque `file_path`
    // est déjà pris, les 500 lignes échouent.
    //
    // Ce qui doit rester vrai malgré le plafond : le TOTAL est dit. Sans lui,
    // le plafond rendrait la perte de pistes moins visible qu'avant — le
    // contraire du but.
    let rejouees = repo.create_batch(&lot).unwrap();
    assert_eq!(
        rejouees, 0,
        "le lot rejoué doit échouer en entier : sinon la mesure ci-dessous ne \
         porte pas sur le pire cas"
    );

    let journal = capture.vider();
    let echecs = lignes_warn(&journal);
    assert!(
        echecs.len() <= 11,
        "un lot de {PISTES_PAR_LOT} pistes qui échoue en entier a écrit {} \
         lignes WARN. Le quota d'export par module est de 250 sur 1000 \
         (#1974) : ce seul lot le remplit et chasse tous les autres modules.\n\
         Premières lignes :\n{}",
        echecs.len(),
        echecs
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    let recap = echecs
        .iter()
        .find(|l| l.contains("track_insert_failures_truncated"))
        .unwrap_or_else(|| {
            panic!(
                "le plafond doit s'annoncer, sinon on ne sait pas qu'on lit un \
                 extrait.\njournal capturé :\n{journal}"
            )
        });
    assert!(
        recap.contains(&format!("echecs={PISTES_PAR_LOT}")),
        "le récapitulatif doit porter le TOTAL des échecs : c'est lui qui \
         empêche le plafond de masquer une perte de pistes : {recap}"
    );
    assert!(
        echecs
            .iter()
            .filter(|l| l.contains("track_insert_failed_in_batch"))
            .count()
            == 10,
        "les dix premiers échecs doivent rester détaillés, avec leurs chemins \
         de fichiers : sans eux, le récapitulatif ne donne rien à rejouer.\n\
         journal capturé :\n{journal}"
    );
}
