//! #2250, point 2 du « À faire » : la MÊME piste 16 bits doit annoncer la même
//! profondeur qu'on la démarre par « Lire » ou par « Lecture aléatoire ».
//!
//! ## Ce que l'issue supposait, et qui est FAUX
//!
//! L'issue posait que `shuffle_all` câble `sample_rate`/`bit_depth` à `None`
//! « là où le chemin "Lire" la transmet ». Les deux sites, mesurés sur la
//! branche courante, passent la MÊME chose :
//!
//! - `shuffle_all` — `tune-server/src/routes/playback.rs`, `sample_rate: None,
//!   bit_depth: None` en dur ;
//! - `play` (le bouton Lecture) — même fichier, `sample_rate:
//!   body.sample_rate, bit_depth: body.bit_depth`, et un client qui démarre une
//!   piste de la bibliothèque n'envoie ni l'un ni l'autre : ces deux champs ne
//!   sont renseignés que par un item de serveur média (`source="upnp"`, les
//!   attributs `res@` du DIDL).
//!
//! Renseigner les deux champs dans `shuffle_all` serait donc un **no-op** : leur
//! unique consommateur est la branche « serveur média / podcast » de
//! `resolve_direct.rs`, atteinte seulement quand `req.source` vaut autre chose
//! que `local` — ce que `shuffle_all` ne fait jamais (`source: None`).
//!
//! ## Ce qui fait réellement l'annonce
//!
//! `composer_le_now_playing` RELIT la ligne `tracks` par `req.track_id`, puis
//! tranche par `resolution_annoncee`. Le seul champ de la demande dont dépend la
//! résolution annoncée d'une piste locale est donc `track_id` — pas
//! `sample_rate`, pas `bit_depth`.
//!
//! Cette garde APPELLE `composer_le_now_playing` (elle ne lit pas son texte)
//! avec les deux formes de demande, sur la même ligne, et compare. Elle tomberait
//! si quelqu'un remettait le repli `.or(resolved.…)` de l'ancien `play_inner`,
//! et elle tomberait aussi si l'annonce cessait de relire la ligne.

use super::transport::Habillage;
use super::{PlayRequest, PlaybackOrchestrator, ResolvedStream};
use crate::db::migrations::run_migrations;
use crate::db::models::Track;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

fn orchestrateur_de_test() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// Le flux tel que `resolve_local_track` le rend pour une piste locale : la
/// résolution y est celle de la SORTIE. Sur une sortie locale le WAV part en
/// 24 bits (`cap_output_bit_depth` borne 16..24) — c'est exactement le chiffre
/// que l'écran ne doit PAS afficher à la place de celui du fichier.
fn flux_local_resolu(sample_rate: Option<u32>, bit_depth: Option<u32>) -> ResolvedStream {
    ResolvedStream {
        url: "http://127.0.0.1:0/stream/2250".into(),
        mime_type: "audio/wav".into(),
        title: "Piste 2250".into(),
        artist: None,
        album: None,
        duration_ms: Some(179_000),
        source: "local".into(),
        cover_url: None,
        stream_id: None,
        file_size: None,
        sample_rate,
        bit_depth,
        channels: Some(2),
        origin_url: None,
        bitrate_kbps: None,
    }
}

/// La demande telle que le bouton « Lire » la construit pour une piste de la
/// bibliothèque : le corps HTTP ne porte ni fréquence ni profondeur.
fn demande_lire(track_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id: 1,
        output_device_id: None,
        track_id: Some(track_id),
        source: None,
        source_id: None,
        title: None,
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    }
}

/// La demande telle que `shuffle_all` la construit pour la piste de tête.
/// Elle ne diffère de la précédente par AUCUN champ qui pèse sur la résolution
/// annoncée — c'est précisément ce que cette garde établit.
fn demande_lecture_aleatoire(track_id: i64) -> PlayRequest {
    PlayRequest {
        sample_rate: None,
        bit_depth: None,
        ..demande_lire(track_id)
    }
}

fn ligne_16_bits(
    orch: &PlaybackOrchestrator,
    sample_rate: Option<i32>,
    bit_depth: Option<i32>,
) -> i64 {
    let pistes = TrackRepo::with_backend(orch.db.clone());
    let mut piste = Track::new("Piste 2250".into());
    piste.file_path = Some("/aucun/chemin/2250/piste.flac".into());
    piste.duration_ms = 179_000;
    piste.sample_rate = sample_rate;
    piste.bit_depth = bit_depth;
    pistes.create(&piste).unwrap()
}

/// Une ligne qui SAIT : 44,1 kHz / 16 bits, sortie transcodée en 24 bits.
/// Les deux chemins doivent annoncer 16, pas 24.
#[tokio::test]
async fn lire_et_lecture_aleatoire_annoncent_la_meme_profondeur() {
    let orch = orchestrateur_de_test();
    let id = ligne_16_bits(&orch, Some(44100), Some(16));
    let resolu = flux_local_resolu(Some(44100), Some(24));
    let habillage = Habillage {
        album: None,
        cover_path: None,
    };

    let par_lire = orch.composer_le_now_playing(&demande_lire(id), &resolu, &habillage);
    let par_aleatoire =
        orch.composer_le_now_playing(&demande_lecture_aleatoire(id), &resolu, &habillage);

    assert_eq!(
        par_lire.bit_depth,
        Some(16),
        "« Lire » doit annoncer la profondeur du FICHIER (16), pas celle de la \
         sortie (24)"
    );
    assert_eq!(
        par_aleatoire.bit_depth, par_lire.bit_depth,
        "la lecture aléatoire annonce une AUTRE profondeur que « Lire » pour la \
         même ligne — c'est le signalement de william, fil 1036 (#2250)"
    );
    assert_eq!(
        par_aleatoire.sample_rate, par_lire.sample_rate,
        "la lecture aléatoire annonce une AUTRE fréquence que « Lire » pour la \
         même ligne (#2250)"
    );
}

/// Une ligne MUETTE : ni fréquence ni profondeur en base. Les deux chemins
/// doivent se taire, et surtout ne pas hériter du 44 100 / 16 que
/// `resolve_local_track` fabrique quand la ligne se tait.
///
/// C'est le cas fréquent de la lecture aléatoire : elle démarre sans cesse une
/// PREMIÈRE piste tirée au hasard, donc une ligne muette bien plus souvent
/// qu'un album qu'on a choisi d'écouter.
#[tokio::test]
async fn une_ligne_muette_se_tait_par_les_deux_chemins() {
    let orch = orchestrateur_de_test();
    let id = ligne_16_bits(&orch, None, None);
    let resolu = flux_local_resolu(Some(44100), Some(16));
    let habillage = Habillage {
        album: None,
        cover_path: None,
    };

    let par_lire = orch.composer_le_now_playing(&demande_lire(id), &resolu, &habillage);
    let par_aleatoire =
        orch.composer_le_now_playing(&demande_lecture_aleatoire(id), &resolu, &habillage);

    assert_eq!(
        par_lire.bit_depth, None,
        "une ligne muette ne doit rien annoncer, pas le 16 fabriqué par la sortie"
    );
    assert_eq!(par_lire.sample_rate, None);
    assert_eq!(par_aleatoire.bit_depth, par_lire.bit_depth);
    assert_eq!(par_aleatoire.sample_rate, par_lire.sample_rate);
}

/// Le champ dont l'annonce dépend RÉELLEMENT.
///
/// La même ligne, la même sortie : avec `track_id` l'écran affiche 16, sans
/// `track_id` il n'affiche rien. C'est donc `track_id`, et lui seul, que le site
/// `shuffle_all` doit continuer de porter ; `sample_rate`/`bit_depth` n'y
/// changent rien — c'est la contre-épreuve du no-op annoncé en tête de module.
#[tokio::test]
async fn track_id_est_le_seul_champ_qui_porte_l_annonce() {
    let orch = orchestrateur_de_test();
    let id = ligne_16_bits(&orch, Some(44100), Some(16));
    let resolu = flux_local_resolu(Some(44100), Some(24));
    let habillage = Habillage {
        album: None,
        cover_path: None,
    };

    let mut sans_ligne = demande_lecture_aleatoire(id);
    sans_ligne.track_id = None;

    let avec_ligne =
        orch.composer_le_now_playing(&demande_lecture_aleatoire(id), &resolu, &habillage);
    let sans = orch.composer_le_now_playing(&sans_ligne, &resolu, &habillage);
    assert_eq!(
        avec_ligne.bit_depth,
        Some(16),
        "avec track_id, la ligne parle"
    );
    assert_eq!(
        sans.bit_depth, None,
        "une demande locale sans track_id n'a aucune ligne à annoncer : elle se \
         tait, elle n'invente pas la profondeur de sortie"
    );
}
