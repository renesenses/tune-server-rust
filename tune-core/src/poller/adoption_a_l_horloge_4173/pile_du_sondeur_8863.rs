//! #8863 — la pile du sondeur.
//!
//! Les bancs du sondeur tournent en `#[tokio::test]`, sur le fil de test et
//! sa pile de 2 Mio. Un sondage qui relance la lecture (fin de piste, repli
//! après une bascule ignorée) descend `tick` → `adopter_l_enchainement_a_l_horloge`
//! → `handle_track_end` → `avancer_avec_reprises` → `play_from_queue…` →
//! `resolve_stream` → services. En debug, chaque niveau recopie sur sa pile
//! la future du niveau suivant avant de l'attendre : tant qu'elles
//! s'emboîtaient en ligne, ces copies se cumulaient (tick = 25 Kio de future,
//! 20 Kio pour l'adoption, 16 Kio pour la fin de piste…) et la pile a débordé
//! sur macOS (rustc 1.95) sous un filtre large.
//!
//! Le sondeur met désormais ces grosses futures en tas (`Box::pin`). Cette
//! épreuve borne ce qui reste en ligne : une nouvelle grosse future attendue
//! sans `Box::pin` dans le sondage ou dans la préparation de la suivante
//! refait passer le plafond, et se voit ici plutôt qu'en débordement aléatoire.

use super::*;

/// La future d'un sondage complet (avant #8863 : 25 584 octets).
const PLAFOND_TICK: usize = 12 * 1024;
/// La fin de piste, qui relance la lecture (avant #8863 : 15 968 octets).
const PLAFOND_FIN_DE_PISTE: usize = 8 * 1024;
/// La préparation de la suivante (avant #8863 : 2 016 octets).
const PLAFOND_PREPARATION: usize = 1024;
/// La résolution de la suivante, déjà en tas depuis #8701.
const PLAFOND_RESOLUTION: usize = 512;

#[tokio::test]
async fn les_futures_du_sondeur_restent_sous_leur_plafond_8863() {
    let mut banc = Banc::monter().await;
    let zone_id = banc.zone_id;
    let maintenant = Instant::now();
    let etat = banc.playback.get_state(zone_id).await;

    // Construites, jamais attendues : seule leur taille compte ici.
    let tick = banc
        .poller
        .tick(&mut banc.poll_states, &mut banc.idle, &maintenant);
    let taille_tick = std::mem::size_of_val(&tick);
    drop(tick);
    let fin = banc.poller.handle_track_end(zone_id, &etat);
    let taille_fin = std::mem::size_of_val(&fin);
    drop(fin);
    let preparation = banc
        .poller
        .prepare_gapless(zone_id, &etat, APPAREIL, Instant::now());
    let taille_preparation = std::mem::size_of_val(&preparation);
    drop(preparation);
    let resolution = banc.orchestrator.resolve_queue_item_url(zone_id, 1);
    let taille_resolution = std::mem::size_of_val(&resolution);
    drop(resolution);

    eprintln!(
        "pile_8863 tick={taille_tick} fin_de_piste={taille_fin} \
         preparation={taille_preparation} resolution={taille_resolution}"
    );
    assert!(
        taille_tick <= PLAFOND_TICK,
        "la future de `tick` fait {taille_tick} octets (plafond {PLAFOND_TICK}) : une \
         grosse future y est attendue en ligne ; la mettre en tas (`Box::pin`)"
    );
    assert!(
        taille_fin <= PLAFOND_FIN_DE_PISTE,
        "la future de `handle_track_end` fait {taille_fin} octets (plafond \
         {PLAFOND_FIN_DE_PISTE})"
    );
    assert!(
        taille_preparation <= PLAFOND_PREPARATION,
        "la future de `prepare_gapless` fait {taille_preparation} octets (plafond \
         {PLAFOND_PREPARATION})"
    );
    assert!(
        taille_resolution <= PLAFOND_RESOLUTION,
        "la future de `resolve_queue_item_url` fait {taille_resolution} octets \
         (plafond {PLAFOND_RESOLUTION})"
    );
}
