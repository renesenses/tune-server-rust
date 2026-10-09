//! Ticket 134 — une lecture ferme la session qu'elle REMPLACE, pas seulement
//! celle qu'elle a notée à son départ.
//!
//! Le terrain (sortie locale) : trois « suivant » rapprochés. Les trois
//! lectures partent toutes avant que la première ait fini ; les
//! deux dernières notent donc la MÊME session à fermer, celle de la piste
//! d'avant. La session de la deuxième lecture, remplacée par la troisième,
//! n'était fermée par personne : jamais lue, puis abandonnée par son
//! décodeur au bout du délai d'envoi seulement, bloqué jusque-là sur un
//! canal plein que plus personne ne lisait.
use crate::orchestrator::transport::sessions_a_fermer_apres_remplacement;
use crate::playback::{NowPlaying, PlaybackManager};

fn lecture(titre: &str, flux: &str) -> NowPlaying {
    NowPlaying {
        title: titre.into(),
        source: "local".into(),
        stream_id: Some(flux.into()),
        ..Default::default()
    }
}

#[test]
fn la_session_remplacee_s_ajoute_a_celle_du_depart() {
    assert_eq!(
        sessions_a_fermer_apres_remplacement(Some("a"), Some("b")),
        vec!["a".to_string(), "b".to_string()]
    );
}

#[test]
fn sans_intercalation_une_seule_session_est_fermee() {
    assert_eq!(
        sessions_a_fermer_apres_remplacement(Some("a"), Some("a")),
        vec!["a".to_string()]
    );
    assert_eq!(
        sessions_a_fermer_apres_remplacement(None, Some("a")),
        vec!["a".to_string()]
    );
    assert_eq!(
        sessions_a_fermer_apres_remplacement(Some("a"), None),
        vec!["a".to_string()]
    );
    assert!(sessions_a_fermer_apres_remplacement(None, None).is_empty());
}

#[tokio::test]
async fn le_remplacement_rend_la_session_remplacee_sous_le_meme_verrou() {
    let pm = PlaybackManager::new();
    assert_eq!(
        pm.play_en_rendant_le_flux_remplace(20, lecture("Piste A", "a"))
            .await,
        None,
        "rien ne jouait : rien à fermer"
    );
    assert_eq!(
        pm.play_en_rendant_le_flux_remplace(20, lecture("Piste B", "b"))
            .await
            .as_deref(),
        Some("a")
    );
    // Recréation du même flux : ne jamais fermer la session qui va jouer.
    assert_eq!(
        pm.play_en_rendant_le_flux_remplace(20, lecture("Piste B", "b"))
            .await,
        None
    );
}

/// Le scénario du ticket, dans l'ordre du journal : B et C partent tous deux
/// pendant que A joue ; B remplace A, puis C remplace B.
#[tokio::test]
async fn trois_suivants_en_rafale_ne_laissent_ouverte_que_la_derniere_session() {
    let pm = PlaybackManager::new();
    pm.play(20, lecture("Piste A", "a")).await;

    // Départs : B et C notent tous deux la session de A.
    let note_b = pm
        .get_state(20)
        .await
        .now_playing
        .and_then(|np| np.stream_id);
    let note_c = pm
        .get_state(20)
        .await
        .now_playing
        .and_then(|np| np.stream_id);

    // B aboutit le premier, C ensuite.
    let remplacee_b = pm
        .play_en_rendant_le_flux_remplace(20, lecture("Piste B", "b"))
        .await;
    let remplacee_c = pm
        .play_en_rendant_le_flux_remplace(20, lecture("Piste C", "c"))
        .await;

    let mut fermees: Vec<String> =
        sessions_a_fermer_apres_remplacement(note_b.as_deref(), remplacee_b.as_deref());
    fermees.extend(sessions_a_fermer_apres_remplacement(
        note_c.as_deref(),
        remplacee_c.as_deref(),
    ));
    fermees.sort();
    fermees.dedup();
    assert_eq!(
        fermees,
        vec!["a".to_string(), "b".to_string()],
        "la session de B, remplacée par C, doit être fermée : sinon son \
         décodeur reste bloqué jusqu'au délai d'envoi"
    );
}

/// Garde de texte : `play_inner` lit la session remplacée au moment du
/// remplacement et ferme la liste entière, sur les deux chemins (réseau avant
/// l'envoi, local après).
#[test]
fn play_inner_ferme_la_session_remplacee_sur_les_deux_chemins() {
    let source = include_str!("../transport.rs");
    let debut = source
        .find("pub(super) async fn play_inner(")
        .expect("play_inner");
    let corps = &source[debut..];
    let fin = corps
        .find("\"orchestrator_play\"")
        .expect("fin de play_inner");
    let corps = &corps[..fin];
    assert!(
        corps.contains(".play_en_rendant_le_flux_remplace(req.zone_id, np)"),
        "play_inner doit lire la session remplacée sous le verrou de l'état"
    );
    assert!(
        !corps.contains("self.playback.play(req.zone_id, np)"),
        "l'ancien appel perd la session remplacée"
    );
    assert_eq!(
        corps.matches("for sid in &sessions_a_fermer").count(),
        2,
        "les chemins réseau et local ferment tous deux la liste"
    );
}
