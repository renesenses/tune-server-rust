//! Le chemin de LECTURE doit dire qu'il a jeté un morceau de la piste (#2218).
//!
//! # Ce qui est mesuré, et pourquoi ce témoin existe
//!
//! La tranche T4 (`e6abd7c0`) a branché le contrôle de longueur du conteneur
//! sur [`decode_to_pcm`] : `STREAMINFO.total samples` face au nombre de trames
//! réellement rendues, et un `warn!` quand la seconde est inférieure. C'est le
//! chemin d'ANALYSE — ReplayGain, empreintes, conversion.
//!
//! Le chemin qui joue la musique est l'autre : `decode_to_pcm_streaming_inner`,
//! qui a sa propre boucle symphonia et n'a JAMAIS lu `track.num_frames`.
//! Mesuré le 12/09/2026 sur `tests/fixtures/flac/ref_16_44100_stereo.flac`,
//! un octet inversé au milieu des trames :
//!
//! ```text
//! sain   : 17 640 trames servies
//! abîmé  : 13 544 trames servies   (−4 096 trames, 92,9 ms)
//! retour : Ok((16, 44100))
//! journal: 0 ligne
//! ```
//!
//! # Ce que la perte N'EST PAS
//!
//! ⚠️ Ce n'est **pas** une troncature, contrairement à ce que la note de la
//! tranche T1 et les notes de la v0.9.147 annoncent. Mesuré : le préfixe commun
//! avec la référence vaut 8 192 trames, le suffixe commun 5 352, et
//! `préfixe + suffixe` fait exactement la sortie abîmée. La queue est donc
//! **présente et juste** : ce qui manque est **un seul bloc FLAC de 4 096
//! trames**, prélevé au milieu. Le décodeur se resynchronise tout seul —
//! `PacketBuilder::try_build` de `symphonia-bundle-flac` vide sa file de
//! fragments dès que le fragment suivant porte un CRC-16 juste.
//!
//! Le « 23,2 % » n'est donc pas une propriété du défaut, c'est une propriété de
//! la fixture : elle dure 0,4 s, et **un bloc EST** 23,2 % de 0,4 s. Sur une
//! piste de quatre minutes, le même octet abîmé coûte 92,9 ms, soit 0,04 %.
//!
//! # Ce que ce témoin ne voit pas
//!
//! * Il ne dit rien du REFUS. Servir ou refuser un fichier abîmé est un
//!   arbitrage ouvert ; ce témoin exige seulement que la perte laisse une
//!   trace, jamais qu'elle interrompe la lecture.
//! * Il ne couvre que les conteneurs qui annoncent une longueur. Un MP3 sans
//!   tag Xing ne déclenchera jamais cette alerte.
//! * Il ne mesure pas le CRC des trames rendues : un décodeur qui rendrait le
//!   bon NOMBRE de trames fausses passerait ici. C'est le travail des
//!   empreintes de `flac_empreintes_reference.rs` (T1).
use std::sync::{Arc, Mutex};

fn chemin_fixture(nom: &str) -> String {
    format!("{}/tests/fixtures/flac/{nom}", env!("CARGO_MANIFEST_DIR"))
}

/// Un puits de journal partagé par TOUS les fils.
///
/// ⚠️ Un abonné `tracing` posé avec `set_default` est **local au fil**. Le
/// décodage tourne sur un fil dédié (il lui faut un consommateur en face), donc
/// un abonné local n'aurait rien capté — et le témoin aurait été **vert à
/// vide** : zéro ligne lue, conclusion « rien n'est journalisé », quelle que
/// soit la réalité. D'où l'abonné GLOBAL, et le témoin positif plus bas.
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<String>>>);

impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("journal")
            .push(String::from_utf8_lossy(buf).into_owned());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Journal {
        self.clone()
    }
}

/// Sert la piste par le chemin de LECTURE et rend le nombre de trames servies.
fn trames_servies(chemin: &str) -> usize {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1024);
    let chemin = chemin.to_owned();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let poignee = rt.handle().clone();
    let decodeur = std::thread::spawn(move || {
        let _entree = poignee.enter();
        tune_core::audio::decode::decode_to_pcm_streaming(&chemin, None, None, tx, 32768)
    });
    let octets = rt.block_on(async move {
        let mut n = 0usize;
        while let Some(bloc) = rx.recv().await {
            n += bloc.len();
        }
        n
    });
    let sortie = decodeur.join().expect("fil de décodage");
    assert!(
        sortie.is_ok(),
        "le chemin de lecture a refusé la piste : {sortie:?} — ce témoin porte \
         sur un décodage qui RÉUSSIT en perdant du signal, pas sur un refus"
    );
    // 16 bits stéréo : 4 octets par trame.
    octets / 4
}

/// Copie la fixture en inversant UN octet pris au milieu des trames audio.
///
/// Le milieu du fichier : ni `fLaC`, ni `STREAMINFO`, ni la table de recherche.
/// Les octets viennent de la fixture versionnée — ce témoin ne fabrique pas son
/// flux, il perturbe un vrai fichier d'un seul bit à huit positions.
fn copie_avec_un_octet_abime(dossier: &std::path::Path, nom: &str) -> String {
    let mut octets = std::fs::read(chemin_fixture(nom)).expect("lire la fixture");
    let milieu = octets.len() / 2;
    assert!(milieu > 64, "{nom} : fixture trop courte");
    octets[milieu] ^= 0xFF;
    let copie = dossier.join(nom);
    std::fs::write(&copie, &octets).expect("écrire la copie abîmée");
    copie.to_str().expect("chemin utf-8").to_owned()
}

#[test]
fn le_chemin_de_lecture_journalise_la_perte_dun_bloc_flac() {
    let journal = Journal::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(journal.clone())
            .finish(),
    )
    .expect("abonné global");

    let nom = "ref_16_44100_stereo.flac";
    let dossier = tune_core::test_scratch::scratch_dir("journal-perte-flac-2218");
    let abime = copie_avec_un_octet_abime(&dossier, nom);

    // ── 1. La piste SAINE : aucune alerte, et le journal capte bien ─────────
    let trames_saines = trames_servies(&chemin_fixture(nom));
    let apres_sain: Vec<String> = journal.0.lock().expect("journal").clone();

    // Témoin POSITIF. Sans lui, un puits de journal muet (abonné local au fil,
    // niveau trop haut, écrivain non branché) rendrait toutes les assertions
    // « aucune alerte » vraies par construction : le témoin serait vert à vide.
    assert!(
        apres_sain
            .iter()
            .any(|l| l.contains("decoded_symphonia_streaming")),
        "le puits de journal n'a capté AUCUNE ligne du décodage sain \
         ({} lignes en tout) — la capture est aveugle, et tout ce que ce \
         témoin conclut ensuite sur l'absence d'alerte ne vaut rien",
        apres_sain.len()
    );
    assert!(
        !apres_sain
            .iter()
            .any(|l| l.contains("decodage_incomplet_le_conteneur_annoncait_plus")),
        "une piste SAINE a déclenché l'alerte de perte — l'alerte crierait à \
         chaque lecture et ne voudrait plus rien dire"
    );

    // ── 2. La piste ABÎMÉE : la perte est réelle, et elle est dite ──────────
    let trames_abimees = trames_servies(&abime);
    let lignes: Vec<String> = journal.0.lock().expect("journal").clone();
    let apres_abime: Vec<&String> = lignes.iter().skip(apres_sain.len()).collect();

    assert!(
        trames_abimees < trames_saines,
        "{nom} : la copie abîmée a servi AUTANT de trames que la saine \
         ({trames_abimees}) — il n'y a plus de perte à journaliser, et ce \
         témoin ne mesure plus rien : le retourner"
    );

    let alerte = apres_abime
        .iter()
        .find(|l| l.contains("decodage_incomplet_le_conteneur_annoncait_plus"));
    assert!(
        alerte.is_some(),
        "{nom} : le chemin de LECTURE a servi {trames_abimees} trames au lieu \
         de {trames_saines} — {} trames jetées, soit {:.1} % de la piste — et \
         n'a écrit AUCUNE ligne `decodage_incomplet_le_conteneur_annoncait_plus`. \
         Il rend Ok(_), la zone joue un trou de {:.0} ms, et rien dans le \
         journal exporté par « Diagnostics » ne permet de le savoir. \
         {} lignes lues pendant ce décodage : {:?}",
        trames_saines - trames_abimees,
        100.0 * (1.0 - trames_abimees as f64 / trames_saines as f64),
        1000.0 * (trames_saines - trames_abimees) as f64 / 44100.0,
        apres_abime.len(),
        apres_abime
    );
    let alerte = alerte.expect("alerte présente");

    assert!(
        alerte.contains("WARN"),
        "l'alerte existe mais pas au niveau WARN : {alerte:?} — le journal \
         exporté par « Diagnostics » est filtré à `info`, une ligne `debug` n'y \
         figurerait pas et la panne resterait muette pour le seul lecteur qui \
         compte, le testeur qui la remonte"
    );
    assert!(
        alerte.contains(&format!("trames_rendues={trames_abimees}")),
        "l'alerte ne porte pas le compte réellement servi \
         (`trames_rendues={trames_abimees}` attendu) : {alerte:?} — un compteur \
         qui ne chiffre pas la perte ne permet ni de la mesurer ni de la suivre"
    );
    assert!(
        alerte.contains(&format!("trames_annoncees={trames_saines}"))
            || alerte.contains(&format!("Some({trames_saines})")),
        "l'alerte ne porte pas la longueur ANNONCÉE par le conteneur \
         ({trames_saines}) : {alerte:?} — sans les deux nombres, la ligne dit \
         qu'il manque quelque chose sans dire combien"
    );
}
