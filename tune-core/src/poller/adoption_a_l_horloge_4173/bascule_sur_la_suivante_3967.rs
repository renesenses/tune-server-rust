//! #3967 — l'enchaînement VÉRIFIÉ : quand l'appareil a prouvé qu'il tient la
//! suivante, on la lui demande au lieu de tout relancer.
//!
//! ## Ce que le repli d'aujourd'hui coûte (journal Villerio, #4382, 18/09)
//!
//! ```text
//! 20:11:47.198 dlna_set_next  url=".../stream/9f7e6510-….wav"   (le renderer TIRE ce flux)
//! 20:12:20.219 position_past_end_advancing … enchainement=Aucun
//! 20:12:20.219 stream_session_removed stream_id="9f7e6510-…"    <-- 25 Mo déjà servis, jetés
//! 20:12:20.291 dlna_set_uri_ok + dlna_play                      <-- tout est refait
//! ```
//!
//! Le renderer avait le tampon. Tune l'a coupé sous lui, a rouvert une
//! session, reposé l'URI, renvoyé `Play` — et l'appareil a mis 2 à 3 s à
//! redémarrer. Ce module éprouve la seule chose qui évite ces secondes-là :
//! lui demander de passer à ce qu'il tient DÉJÀ.
//!
//! ## Ce qui autorise le geste, et rien d'autre
//!
//! À l'armement, deux lectures que la spécification AVTransport garantit :
//! `GetMediaInfo` → `NextURI` (il NOMME notre URL) et
//! `GetCurrentTransportActions` → `Actions` (il DÉCLARE l'action `Next`).
//! Les deux ensemble valent [`SuivantePreparee::Tenue`]. Tout le reste —
//! `Perdue`, `Inconnue`, un appareil muet, une sortie non-DLNA — retombe sur
//! le repli d'aujourd'hui, mot pour mot ; les témoins du banc parent le
//! prouvent (ils tournent tous avec le défaut `Inconnue`).
//!
//! ## Et si l'appareil ment
//!
//! `Next` acquitté n'est pas `Next` honoré. L'adoption qui suit est
//! surveillée sur [`BASCULE_DELAI_SECS`] — trois sondages, pas huit : le
//! renderer n'a rien à charger. Sans signe de vie, le repli reprend sur la
//! piste ADOPTÉE. Aucune piste sautée, aucun silence définitif.

use super::*;

/// Ce que le renderer avait déjà tiré du flux armé (journal du 18/09).
const OCTETS_TIRES_1845: u64 = 25_231_012;

impl Banc {
    /// L'appareil répondra `verdict` quand on lui demandera, à l'armement,
    /// s'il tient la suivante.
    async fn l_appareil_dit_de_la_suivante(&self, verdict: SuivantePreparee) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.poser_suivante_preparee(verdict);
    }

    /// `Next` sera-t-il HONORÉ (l'appareil bascule vraiment) ou seulement
    /// acquitté ?
    async fn l_appareil_honore_le_next(&self, honore: bool) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.bascule_honoree(honore);
    }

    /// Combien de `Next` sont partis vers l'appareil.
    async fn bascules(&self) -> u64 {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.bascule_call_count()
    }

    /// La signature exacte du DMP-A6 : il a tiré le flux armé, et il nomme
    /// encore le flux de la piste FINIE. Aucun enchaînement n'est attesté.
    async fn la_signature_du_dmp_a6(&mut self, flux: &str) {
        self.le_renderer_tire(flux, OCTETS_TIRES_1845).await;
        let uri_finie = format!("http://192.168.1.196:8888/stream/{}.wav", self.flux_finie);
        self.le_renderer_rapporte(Some(uri_finie)).await;
    }
}

/// **Le témoin.** L'appareil n'a pas enchaîné tout seul — il nomme encore le
/// flux de la piste finie — mais il avait NOMMÉ notre suivante et DÉCLARÉ
/// l'action `Next` à l'armement. Tune la lui demande : la piste suivante
/// démarre sur le tampon déjà rempli, **sans un seul `SetAVTransportURI` +
/// `Play`**, et la session pré-armée n'est pas détruite.
#[tokio::test]
async fn l_appareil_qui_tient_la_suivante_bascule_sans_aucune_relance() {
    let mut banc = Banc::monter().await;
    banc.l_appareil_dit_de_la_suivante(SuivantePreparee::Tenue)
        .await;
    let (flux, _) = banc.armer().await;
    assert_eq!(
        banc.poll_states
            .get(&banc.zone_id)
            .unwrap()
            .suivante_preparee,
        SuivantePreparee::Tenue,
        "l'armement doit avoir relevé ce que l'appareil dit de la suivante"
    );
    banc.la_signature_du_dmp_a6(&flux).await;

    banc.la_fin_a_l_horloge().await;

    assert_eq!(
        banc.bascules().await,
        1,
        "un seul `Next`, au moment où le repli allait partir"
    );
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "AUCUN `SetAVTransportURI` + `Play` : c'est exactement le compteur de \
         relances que #3967 demande à zéro"
    );
    assert!(
        banc.orchestrator.stream_session_alive(&flux).await,
        "la session pré-armée doit VIVRE : c'est son tampon qui joue"
    );
    let (position, titre, stream_id) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
    assert_eq!(
        stream_id.as_deref(),
        Some(flux.as_str()),
        "la zone doit avoir adopté le flux armé, pas une session neuve"
    );
    let surveillance = banc
        .surveillance()
        .expect("une bascule commandée reste surveillée jusqu'au signe de vie");
    assert_eq!(surveillance.preuve, decisions::EnchainementArme::Bascule);
    assert_eq!(
        surveillance.delai_secs, BASCULE_DELAI_SECS,
        "une CONSIGNE se juge en trois sondages, pas en huit"
    );

    // Le renderer repart : la bascule est confirmée, toujours zéro `Play`.
    banc.renderer_a(2_000, 2).await;
    banc.tic().await;
    assert!(
        banc.surveillance().is_none(),
        "la position qui repart confirme la bascule"
    );
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
}

/// **La contre-épreuve, celle qui doit rester verte quoi qu'il arrive.**
/// L'appareil a promis — il nommait la suivante, il déclarait `Next` — et il
/// ACQUITTE le `Next` sans bouger d'un millisecondes. Passé les trois
/// sondages, le repli d'aujourd'hui reprend **sur la piste adoptée** : la
/// file ne saute aucun titre, et le silence n'est pas définitif.
#[tokio::test]
async fn l_appareil_qui_acquitte_le_next_sans_l_honorer_retombe_sur_le_repli() {
    let mut banc = Banc::monter().await;
    banc.l_appareil_dit_de_la_suivante(SuivantePreparee::Tenue)
        .await;
    banc.l_appareil_honore_le_next(false).await;
    let (flux, _) = banc.armer().await;
    banc.la_signature_du_dmp_a6(&flux).await;

    banc.la_fin_a_l_horloge().await;
    assert_eq!(banc.bascules().await, 1);
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "le `Next` vient de partir : on lui laisse ses trois sondages"
    );

    // Un sondage dans le délai : l'appareil est toujours figé, on attend.
    banc.renderer_a(POSITION_GELEE_MS, 1).await;
    banc.tic().await;
    assert!(banc.surveillance().is_some(), "dans le délai, on attend");
    assert_eq!(banc.play_complets().await, Vec::<String>::new());

    // Le délai est écoulé (horloge injectée), l'appareil n'a jamais bougé.
    banc.poll_states
        .get_mut(&banc.zone_id)
        .unwrap()
        .adoption_horloge
        .as_mut()
        .unwrap()
        .depuis = Instant::now() - Duration::from_secs(BASCULE_DELAI_SECS + 1);
    banc.renderer_a(POSITION_GELEE_MS, BASCULE_DELAI_SECS + 1)
        .await;
    banc.tic().await;

    assert_eq!(
        banc.play_complets().await,
        vec![ARMEE.to_string()],
        "sans signe de vie, le repli relance la piste ADOPTÉE — le comportement d'avant"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "AUCUNE piste perdue : l'écran et la file sont sur la piste armée"
    );
}

/// L'appareil a acquitté le `SetNext` mais ne le RETIENT pas
/// (`GetMediaInfo` → `NextURI` vide) : aucun `Next` ne part, le repli
/// d'aujourd'hui s'exécute mot pour mot.
#[tokio::test]
async fn une_suivante_perdue_chez_l_appareil_ne_declenche_aucune_bascule() {
    let mut banc = Banc::monter().await;
    banc.l_appareil_dit_de_la_suivante(SuivantePreparee::Perdue)
        .await;
    let (flux, _) = banc.armer().await;
    banc.la_signature_du_dmp_a6(&flux).await;

    banc.la_fin_a_l_horloge().await;

    assert_eq!(
        banc.bascules().await,
        0,
        "rien à demander à cet appareil-là"
    );
    verifier_le_repli(&banc, &flux).await;
}

/// L'appareil ne répond pas aux deux lectures (pas de `GetMediaInfo`, pas de
/// `GetCurrentTransportActions`) : `Inconnue`, donc le repli. C'est aussi le
/// défaut de toute sortie qui n'implémente rien — d'où les témoins du banc
/// parent, inchangés.
#[tokio::test]
async fn un_appareil_muet_garde_le_repli_d_aujourd_hui() {
    let mut banc = Banc::monter().await;
    // Pas de `l_appareil_dit_de_la_suivante` : le défaut est `Inconnue`.
    let (flux, _) = banc.armer().await;
    assert_eq!(
        banc.poll_states
            .get(&banc.zone_id)
            .unwrap()
            .suivante_preparee,
        SuivantePreparee::Inconnue
    );
    banc.la_signature_du_dmp_a6(&flux).await;

    banc.la_fin_a_l_horloge().await;

    assert_eq!(banc.bascules().await, 0);
    verifier_le_repli(&banc, &flux).await;
}

/// L'appareil a réellement enchaîné TOUT SEUL : on adopte son enchaînement
/// (#4173) et on ne lui commande rien. Demander `Next` à un renderer qui
/// joue déjà la bonne piste ferait sauter un titre.
#[tokio::test]
async fn un_appareil_qui_a_deja_enchaine_ne_recoit_aucun_next() {
    let mut banc = Banc::monter().await;
    banc.l_appareil_dit_de_la_suivante(SuivantePreparee::Tenue)
        .await;
    let (flux, url) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES_1845).await;
    banc.le_renderer_rapporte(Some(url)).await;

    banc.la_fin_a_l_horloge().await;

    assert_eq!(
        banc.bascules().await,
        0,
        "il joue déjà le flux armé : lui demander `Next` sauterait un titre"
    );
    verifier_l_adoption(&banc, &flux, decisions::EnchainementArme::Certain).await;
}

/// La règle pure, ligne à ligne.
#[test]
fn la_porte_de_la_bascule() {
    use decisions::{EnchainementArme as E, bascule_sur_la_suivante_autorisee as porte};
    let flux = Some("9f7e6510");

    // Le seul cas qui ouvre.
    assert!(porte(true, SuivantePreparee::Tenue, flux, E::Aucun));

    // Ce que l'appareil n'a pas prouvé ne l'ouvre pas.
    assert!(!porte(true, SuivantePreparee::Perdue, flux, E::Aucun));
    assert!(!porte(true, SuivantePreparee::Inconnue, flux, E::Aucun));
    // Hors DLNA : aucun autre protocole n'expose `Next`.
    assert!(!porte(false, SuivantePreparee::Tenue, flux, E::Aucun));
    // Rien d'armé : rien vers quoi basculer.
    assert!(!porte(true, SuivantePreparee::Tenue, None, E::Aucun));
    assert!(!porte(true, SuivantePreparee::Tenue, Some(""), E::Aucun));
    // Un enchaînement déjà attesté s'ADOPTE, il ne se commande pas.
    assert!(!porte(true, SuivantePreparee::Tenue, flux, E::Certain));
    assert!(!porte(true, SuivantePreparee::Tenue, flux, E::Probable));
}

// ── #4382 — `track_end_gap` doit NOMMER le verdict d'armement ──────────────
//
// C'est la seule ligne que les rapports de terrain portent (le `diagnostic.md`
// de Villerio du 18/09 n'en contient pas d'autre sur cette transition). Elle
// disait `gapless_sent=true`, c'est-à-dire « le `SetNext` est parti » — et
// rien de plus. Or ce qui décide du geste de #3967, c'est ce que l'appareil a
// RÉPONDU à l'armement. Sans ce champ, le journal du DMP-A6 ne permet pas de
// dire si la branche `Next` s'est armée, et la question ne peut pas être
// tranchée sans avoir l'appareil sous la main.

#[derive(Clone, Default)]
struct JournalDuRepli(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for JournalDuRepli {
    fn write(&mut self, octets: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(octets);
        Ok(octets.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalDuRepli {
    type Writer = JournalDuRepli;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl JournalDuRepli {
    fn texte(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    /// INFO : le niveau d'un export de terrain, pas TRACE.
    fn abonner(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish(),
        )
    }
}

/// La signature exacte du rapport de Villerio : l'appareil n'a rien annoncé
/// de la suivante (`Inconnue`, le défaut), il a tiré le flux armé, et il
/// nomme encore la piste finie. Le repli part — et la ligne qui le dit doit
/// porter le verdict d'armement.
#[tokio::test]
async fn track_end_gap_nomme_le_verdict_d_armement() {
    let mut banc = Banc::monter().await;
    let (flux, _) = banc.armer().await;
    assert_eq!(
        banc.poll_states
            .get(&banc.zone_id)
            .unwrap()
            .suivante_preparee,
        SuivantePreparee::Inconnue,
        "le banc doit partir du verdict par défaut, celui du DMP-A6 non mesuré"
    );
    banc.la_signature_du_dmp_a6(&flux).await;

    let journal = JournalDuRepli::default();
    {
        let _garde = journal.abonner();
        banc.la_fin_a_l_horloge().await;
    }

    let texte = journal.texte();
    assert!(
        texte.contains("track_end_gap"),
        "la ligne d'entonnoir doit être écrite au niveau INFO.\n{texte}"
    );
    assert!(
        texte.contains("suivante_preparee=Inconnue"),
        "`track_end_gap` doit nommer ce que l'appareil a répondu à l'armement : \
         sans ce champ, aucun journal de terrain ne dit si la branche `Next` \
         de #3967 s'est armée.\n{texte}"
    );
}
