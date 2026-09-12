//! #2352 — le service du corps d'un flux FICHIER dit enfin sa durée, son
//! débit et le délai de son premier octet.
//!
//! # Ce que le silence a coûté
//!
//! Dominique COMET, fil 1653 (mozaiklabs), 03/09/2026, Tune 0.9.132,
//! `DirettaRendererUPnP` : « la lecture met plus de 30 secondes avant de
//! démarrer après avoir appuyé sur PLAY », sur Qobuz **comme** sur ses
//! fichiers locaux. Son journal a été lu ligne à ligne, et il ferme deux
//! hypothèses au lieu d'en ouvrir une :
//!
//! * `playback_timing` donne `total_ms` = 129, 212, 242 et 1244 sur les quatre
//!   démarrages du journal. Tune envoie son `Play` en **moins d'une seconde et
//!   quart**, pré-tampon compris (`prebuffer_ms=0`, `reached=true`).
//! * `stream_request` du renderer arrive à **±2 ms** de `playback_timing`, et
//!   deux fois AVANT lui. Il n'y a aucune latence de mise en relation, et le
//!   lien est du loopback : serveur et renderer sont sur la même machine
//!   (192.168.1.104 des deux côtés).
//!
//! Les « plus de 30 secondes », s'ils ont eu lieu dans cette fenêtre, se
//! jouent donc **après le premier octet servi**. Et sur ce segment le journal
//! ne portait pas une ligne : `build_file_body` incrémentait `bytes_sent`
//! — un compteur que seul le sondeur lit, jamais journalisé — sans jamais
//! dire en combien de temps les octets sont partis.
//!
//! # Ce que ce témoin fixe
//!
//! Le service du corps d'une session FICHIER émet, au niveau INFO, une ligne
//! `service_fichier_termine` portant `stream_id`, `octets`, `demande`,
//! `premier_octet_ms`, `elapsed_ms`, `debit_kio_s` et `complet`.
//!
//! Ce témoin ne prétend **pas** que le délai a diminué — cette PR ne change
//! aucun comportement. Il tient le fait que la mesure existe, et qu'elle porte
//! ses chiffres : une ligne sans `premier_octet_ms` ni `elapsed_ms` ne mesure
//! rien, et c'est précisément l'état d'avant.
//!
//! # Pourquoi un binaire de test à lui seul
//!
//! Même leçon que #2665, #2890, #3180, #3479 et #3568, déjà payée cinq fois :
//! `tracing` met en cache POUR TOUT LE PROCESSUS la décision « ce point
//! d'appel intéresse-t-il quelqu'un ? ». Un abonné posé au milieu d'une suite
//! qui tourne en parallèle se voit priver d'évènements de façon imprévisible.
//! Ici l'abonné est GLOBAL et ce fichier ne contient QU'UN test.
//!
//! `tune-stream-http` n'a PAS `autotests = false` (vérifié dans son
//! `Cargo.toml`) : ce fichier est donc compilé sans déclaration `[[test]]`, et
//! la caisse est nommée par la porte de test de la CI (`ci.yml`, la ligne
//! `cargo test --no-fail-fast -p tune-core … -p tune-stream-http …`) comme par
//! sa ligne clippy. La garde est exécutée.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use futures_util::StreamExt;
use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qu'on aura
/// entre les mains la prochaine fois qu'un testeur en joindra un.
#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn lire(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
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

/// La ligne du journal qui porte cet évènement.
///
/// ⚠️ L'aiguille est **assemblée à l'exécution**. Écrite en clair, elle
/// figurerait dans ce fichier — et une contre-épreuve qui cherche l'aiguille
/// dans l'arbre se trouverait elle-même.
fn ligne_evenement(journal: &str, evenement: &str) -> String {
    journal
        .lines()
        .find(|l| l.contains(evenement))
        .unwrap_or_else(|| {
            panic!("aucune ligne « {evenement} » dans le journal :\n{journal}");
        })
        .to_string()
}

/// Valeur entière d'un champ `clef=valeur` d'une ligne `tracing`.
fn champ_entier(ligne: &str, clef: &str) -> u64 {
    let aiguille = format!("{clef}=");
    let reste = ligne
        .split(&aiguille)
        .nth(1)
        .unwrap_or_else(|| panic!("champ « {clef} » absent de : {ligne}"));
    reste
        .split_whitespace()
        .next()
        .and_then(|v| {
            v.trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()
        })
        .unwrap_or_else(|| panic!("champ « {clef} » illisible dans : {ligne}"))
}

#[tokio::test]
async fn le_service_du_corps_fichier_dit_sa_duree_son_debit_et_son_premier_octet() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    // Un fichier réel sur le disque : c'est la branche FICHIER de
    // `handle_stream` qu'on veut traverser, celle que le journal de Dominique
    // montre en action (`format=flac`, `range="bytes=0-"`).
    const OCTETS: usize = 300_000;
    // `scratch_file` et pas un chemin composé à la main : le garde
    // `aucune_fuite_de_temporaires` (#3030) refuse le second, et il a raison
    // — un test qui ÉCHOUE laisse sinon son fichier derrière lui. Le nettoyage
    // passe par le `Drop` de `ScratchFile`.
    let fichier = tune_core::test_scratch::scratch_file("i2352-service", ".flac");
    let chemin = fichier.path().to_path_buf();
    std::fs::write(&chemin, vec![0x5Au8; OCTETS]).expect("fichier de test");

    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new("i2352".into(), info, false, 8));
    *session.file_path.lock().await = Some(chemin.to_string_lossy().into_owned());

    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [("i2352".to_string(), session.clone())]
            .into_iter()
            .collect(),
    ));

    let reponse = tune_stream_http::handle_stream(
        Path("i2352.flac".into()),
        State(sessions),
        axum::http::HeaderMap::new(),
    )
    .await;

    // Vider le corps : c'est le service lui-même, celui qu'on chronomètre.
    let mut corps = reponse.into_body().into_data_stream();
    let mut recus = 0usize;
    while let Some(morceau) = corps.next().await {
        recus += morceau.expect("erreur de flux").len();
    }
    assert_eq!(recus, OCTETS, "le corps devait servir tout le fichier");

    // La ligne part à la CHUTE du corps : c'est ce qui la rend fidèle aussi
    // quand le renderer abandonne en cours de route.
    drop(corps);
    tokio::task::yield_now().await;

    let journal = capture.lire();
    // Assemblée à l'exécution — voir `ligne_evenement`.
    let evenement = ["service", "fichier", "termine"].join("_");
    let ligne = ligne_evenement(&journal, &evenement);

    assert!(
        ligne.contains("stream_id=\"i2352\"") || ligne.contains("stream_id=i2352"),
        "la ligne doit porter le `stream_id`, sans quoi elle ne se rattache à \
         aucun `playback_timing` ni à aucun `stream_request` : {ligne}"
    );
    assert_eq!(
        champ_entier(&ligne, "octets"),
        OCTETS as u64,
        "les octets servis doivent être ceux du fichier : {ligne}"
    );
    assert_eq!(
        champ_entier(&ligne, "demande"),
        OCTETS as u64,
        "la longueur demandée doit être annoncée : {ligne}"
    );
    assert!(
        ligne.contains("complet=true"),
        "un service mené à son terme doit s'annoncer complet : {ligne}"
    );
    // Les deux champs qui font toute la valeur de la ligne. Sans eux elle
    // répète `bytes_sent`, qui existait déjà et ne mesurait aucune durée.
    assert!(
        ligne.contains("premier_octet_ms="),
        "sans `premier_octet_ms`, rien ne sépare « Tune a mis du temps à ouvrir \
         la source » de « le renderer tire lentement » : {ligne}"
    );
    assert!(
        ligne.contains("elapsed_ms="),
        "sans `elapsed_ms`, la ligne ne mesure aucune durée : {ligne}"
    );
    // `premier_octet_ms` ne peut pas dépasser la durée totale du service.
    assert!(
        champ_entier(&ligne, "premier_octet_ms") <= champ_entier(&ligne, "elapsed_ms"),
        "le premier octet ne peut pas arriver après la fin du service : {ligne}"
    );

    // Le fichier part avec le `Drop` de `fichier` : rien à retirer ici.
    drop(fichier);
}
