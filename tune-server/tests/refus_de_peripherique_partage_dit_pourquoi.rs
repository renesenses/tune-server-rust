//! Le refus de périphérique du chemin cpal PARTAGÉ dit pourquoi — wav6328.
//!
//! `find_device_with_fallback` (`tune-core/src/outputs/local.rs`) a QUATRE
//! issues, dont une seule n'ouvre rien : le périphérique réglé sur la zone est
//! introuvable ET l'hôte n'expose aucune sortie par défaut sur laquelle se
//! rabattre (`audio_device_not_found_no_default_available`). Ses DEUX
//! consommateurs de production — le flux WAV, c'est-à-dire la bibliothèque
//! locale, et le flux compressé décodé — faisaient `playing.store(false)` puis
//! rendaient la main **sans rien alimenter** : la zone s'arrêtait et l'écran
//! ne savait pas pourquoi.
//!
//! Le même refus sur les chemins EXCLUSIFS, lui, est nommé depuis toujours par
//! `record_exclusive_open_failure`. C'était donc une incohérence, pas un
//! manque — et elle portait sur le chemin le plus emprunté de tous.
//!
//! Le canal est `open_failure: Arc<Mutex<Option<String>>>` →
//! `take_output_failure()` → `poller.rs` → `bus.emit("zone.playback_error",
//! {zone_id, error, fatal: true})` → `routes/ws.rs` → client, qui affiche le
//! texte **verbatim** dans un toast rouge et court-circuite la fenêtre de
//! grâce sur `fatal: true`. Alimenter le canal change donc bien quelque chose
//! de visible.
//!
//! ⚠️ **Pourquoi une garde de SITE par `include_str!` et non un test
//! d'exécution.** `outputs/local.rs` vit derrière la feature `local-audio`,
//! que le job `Test` de `ci.yml` n'active pas
//! (`--no-default-features --features oaat,cloud-relay,bandcamp`) ; les deux
//! jobs qui l'activent sont conditionnés à `full` et ne sont donc jamais joués
//! sur une PR vers `batch/*`. Un test qui compilerait ce module serait vert
//! contre rien. Lire le *texte* du fichier échappe aux `cfg` : cette garde-ci
//! s'exécute dans le job `Test`, sans `local-audio`.
//!
//! Idiome du dépôt : `local.rs` se relit déjà lui-même par `include_str!`
//! (`find_device_with_fallback_passe_bien_l_hote_d_origine`), et
//! `autoeq_import_route.rs` lit déjà `../../tune-core/...` depuis
//! `tune-server/tests/`.
//!
//! REF-8 (#2219) : le consommateur du flux WAV vit désormais dans
//! `BackendCpal::ouvrir` (`local/backend.rs`), qui ne tient pas le créneau
//! d'échec : il rend un `RefusDOuverture::PeripheriqueIntrouvable` typé, et
//! c'est `RefusDOuverture::rapporter` qui appelle
//! `record_shared_device_not_found`. La garde lit donc les DEUX fichiers,
//! concaténés — jamais l'un à la place de l'autre — et suit les deux formes du
//! câblage : en ligne (flux compressé) et par le refus typé (flux WAV).
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

const LOCAL_RS: &str = include_str!("../../tune-core/src/outputs/local.rs");
const BACKEND_RS: &str = include_str!("../../tune-core/src/outputs/local/backend.rs");

/// La production seule : `local.rs` porte un `mod tests` volumineux dont le
/// texte citerait nos propres motifs et rendrait la garde complaisante.
/// `backend.rs` n'a pas de module de test ; il est lu en entier, à la suite.
fn production() -> String {
    let fin = LOCAL_RS
        .find("#[cfg(test)]\nmod tests")
        .expect("local.rs doit garder son `#[cfg(test)] mod tests` en fin de fichier");
    [&LOCAL_RS[..fin], BACKEND_RS].concat()
}

/// LE verrou : **chaque** site de production qui consomme le `None` de
/// `find_device_with_fallback` doit alimenter le canal d'échec.
///
/// La garde ne compte pas des lignes, elle suit les APPELS : pour chaque
/// `= find_device_with_fallback(`, la branche `else` qui suit doit soit appeler
/// `record_shared_device_not_found` avant d'arrêter la zone (flux compressé,
/// en ligne), soit rendre le refus typé `RefusDOuverture::PeripheriqueIntrouvable`
/// (flux WAV, `BackendCpal::ouvrir`) — et alors le rapporteur du refus doit
/// appeler `record_shared_device_not_found`, et `play_url` doit rapporter le
/// refus AVANT d'arrêter la zone. Un troisième consommateur ajouté demain sans
/// câblage tombe ici, un `playing.store(false)` remis à nu à la place du
/// rapporteur aussi, et un refus typé que personne ne rapporte aussi.
#[test]
fn chaque_refus_de_peripherique_partage_alimente_le_canal_d_echec() {
    let src = production();
    let sites: Vec<usize> = src
        .match_indices("= find_device_with_fallback(")
        .map(|(i, _)| i)
        .collect();

    assert_eq!(
        sites.len(),
        2,
        "la production doit garder ses DEUX consommateurs de \
         find_device_with_fallback (flux WAV et flux compressé) ; trouvés : {}",
        sites.len()
    );

    let mut en_ligne = 0usize;
    let mut types = 0usize;
    for (rang, debut) in sites.iter().enumerate() {
        let fin = (debut + 700).min(src.len());
        let bloc = &src[*debut..fin];
        match bloc.find("playing.store(false") {
            Some(arret) => {
                // Forme en ligne : le rapporteur AVANT l'arrêt de la zone.
                let avant_arret = &bloc[..arret];
                assert!(
                    avant_arret.contains("record_shared_device_not_found("),
                    "site {rang} : le refus de périphérique s'arrête sans alimenter \
                     open_failure — la zone s'éteint et l'écran ne sait pas pourquoi. \
                     Le même refus sur les chemins exclusifs passe par \
                     record_exclusive_open_failure. Bloc lu :\n{avant_arret}"
                );
                en_ligne += 1;
            }
            None => {
                // Forme typée (REF-8) : la branche else rend le refus nommé.
                assert!(
                    bloc.contains("return Err(RefusDOuverture::PeripheriqueIntrouvable("),
                    "site {rang} : la branche else n'arrête pas la zone et ne rend pas \
                     non plus `RefusDOuverture::PeripheriqueIntrouvable` : le refus \
                     n'a plus de forme que la garde sache suivre. Bloc lu :\n{bloc}"
                );
                types += 1;
            }
        }
    }
    assert_eq!(
        (en_ligne, types),
        (1, 1),
        "attendu UN site en ligne (flux compressé) et UN site typé (flux WAV, \
         BackendCpal::ouvrir) ; trouvés {en_ligne} en ligne, {types} typé(s)"
    );

    // Le refus typé doit être RAPPORTÉ par le même rapporteur que la forme en
    // ligne — sinon la zone s'éteint sans un mot, exactement le défaut d'origine.
    let rapporteur = src
        .find("fn rapporter(")
        .expect("REF-8 — `RefusDOuverture::rapporter` a disparu de backend.rs");
    let corps = &src[rapporteur..(rapporteur + 2_000).min(src.len())];
    let bras = corps
        .find("PeripheriqueIntrouvable(")
        .expect("`rapporter` ne traite plus `PeripheriqueIntrouvable`");
    assert!(
        corps[bras..(bras + 300).min(corps.len())].contains("record_shared_device_not_found("),
        "`rapporter` reçoit `PeripheriqueIntrouvable` et n'appelle pas \
         `record_shared_device_not_found` : le refus typé n'atteint jamais \
         l'écran. Corps lu :\n{corps}"
    );

    // Et `play_url` doit rapporter le refus AVANT d'arrêter la zone.
    let ouverture = src
        .find("BackendCpal::ouvrir(")
        .expect("REF-8 — play_url n'appelle plus `BackendCpal::ouvrir`");
    let bloc = &src[ouverture..(ouverture + 700).min(src.len())];
    let arret = bloc
        .find("playing.store(false")
        .expect("après `BackendCpal::ouvrir`, la branche Err doit arrêter la zone");
    assert!(
        bloc[..arret].contains(".rapporter(&device_name, &open_failure)"),
        "la branche Err de `BackendCpal::ouvrir` arrête la zone sans appeler \
         `refus.rapporter(&device_name, &open_failure)` : le refus typé est \
         calculé puis jeté. Bloc lu :\n{}",
        &bloc[..arret]
    );
}

/// Le rapporteur doit ÉCRIRE dans le créneau, pas seulement journaliser.
///
/// Un rapporteur qui ne fait qu'un `warn!` laisse l'écran aussi muet qu'avant :
/// c'est exactement le défaut corrigé ici, et il se réintroduirait sans bruit.
#[test]
fn le_rapporteur_partage_ecrit_dans_le_creneau_et_pas_seulement_dans_le_journal() {
    let src = production();
    let debut = src
        .find("fn record_shared_device_not_found(")
        .expect("record_shared_device_not_found doit exister dans la production");
    let corps = &src[debut..(debut + 600).min(src.len())];

    assert!(
        corps.contains("failure_slot: &std::sync::Mutex<Option<String>>"),
        "le rapporteur doit RECEVOIR le créneau d'échec, comme \
         record_exclusive_open_failure et record_feed_stall_failure ; reçu :\n{corps}"
    );
    assert!(
        corps.contains("failure_slot.lock()") && corps.contains("*slot = Some("),
        "le rapporteur doit ÉCRIRE dans le créneau : sans cette écriture, \
         take_output_failure() rend None et zone.playback_error n'est jamais \
         émis ; reçu :\n{corps}"
    );
    assert!(
        corps.contains("user_message("),
        "le message rendu à l'écran doit venir de user_message(), la forme de \
         référence du fichier (WindowsExclusivePcmError) ; reçu :\n{corps}"
    );
}

/// Les deux évènements de journal historiques survivent au refactor.
///
/// Ils ont été récoltés sur le terrain : les perdre casserait la lecture des
/// journaux déjà envoyés par les testeurs.
#[test]
fn les_deux_evenements_de_journal_historiques_sont_conserves() {
    let src = production();
    for evenement in [
        "audio_device_not_found_no_fallback",
        "audio_device_not_found_compressed",
    ] {
        assert!(
            src.contains(evenement),
            "l'évènement de journal `{evenement}` a disparu de la production : \
             les journaux déjà récoltés ne seraient plus lisibles"
        );
    }
}

/// Le canal alimenté doit rester CELUI que le poller draine.
///
/// Ouvrir un second canal ferait un correctif « écrit mais pas branché » :
/// `take_output_failure()` est le seul point de sortie vers
/// `zone.playback_error`.
#[test]
fn le_creneau_alimente_est_bien_celui_que_draine_take_output_failure() {
    let src = production();
    let debut = src
        .find("fn take_output_failure(")
        .expect("take_output_failure doit exister : c'est le seul drain vers l'écran");
    let corps = &src[debut..(debut + 400).min(src.len())];
    assert!(
        corps.contains("open_failure"),
        "take_output_failure doit continuer de drainer `open_failure`, le champ \
         que les rapporteurs alimentent ; reçu :\n{corps}"
    );

    // Une définition (précédée de `fn `) et DEUX appels.
    let occurrences: Vec<usize> = src
        .match_indices("record_shared_device_not_found(")
        .map(|(i, _)| i)
        .collect();
    let appels: Vec<usize> = occurrences
        .iter()
        .copied()
        .filter(|i| !src[..*i].ends_with("fn "))
        .collect();
    assert_eq!(
        occurrences.len() - appels.len(),
        1,
        "record_shared_device_not_found doit être défini exactement une fois"
    );
    assert_eq!(
        appels.len(),
        2,
        "attendu DEUX appels de record_shared_device_not_found (flux WAV et flux \
         compressé) ; trouvés {}",
        appels.len()
    );

    // REF-8 : l'appel du flux WAV vit dans `RefusDOuverture::rapporter`, qui
    // reçoit le créneau en paramètre sous le nom `open_failure`. Il compte
    // comme branché si (1) il est dans `rapporter`, (2) le paramètre est bien
    // le créneau, (3) chaque appel de `.rapporter(` dans `play_url` lui passe
    // `&open_failure`.
    let rapporteur = src
        .find("fn rapporter(")
        .expect("REF-8 — `RefusDOuverture::rapporter` a disparu de backend.rs");
    let fin_du_rapporteur = src[rapporteur..]
        .find("impl std::fmt::Display for RefusDOuverture")
        .map(|i| rapporteur + i)
        .expect("`impl Display for RefusDOuverture` doit suivre `rapporter`");
    assert!(
        src[rapporteur..(rapporteur + 200).min(src.len())]
            .contains("open_failure: &std::sync::Mutex<Option<String>>"),
        "`rapporter` ne reçoit plus le créneau d'échec sous le nom `open_failure`"
    );
    let appels_de_rapporter: Vec<usize> =
        src.match_indices(".rapporter(").map(|(i, _)| i).collect();
    assert!(
        !appels_de_rapporter.is_empty(),
        "personne n'appelle `.rapporter(` : le refus typé n'atteint jamais l'écran"
    );
    for appel in appels_de_rapporter {
        assert!(
            src[appel..(appel + 80).min(src.len())].contains("&open_failure"),
            "un appel de `.rapporter(` passe autre chose que `&open_failure` : \
             le message n'atteindrait jamais l'écran. Bloc lu :\n{}",
            &src[appel..(appel + 80).min(src.len())]
        );
    }

    for appel in appels {
        let bloc = &src[appel..(appel + 300).min(src.len())];
        let dans_le_rapporteur = appel > rapporteur && appel < fin_du_rapporteur;
        assert!(
            bloc.contains("&open_failure")
                || (dans_le_rapporteur && bloc.contains("open_failure)")),
            "un appel passe autre chose que `&open_failure` (ou, dans `rapporter`, \
             que le paramètre `open_failure`) : le message n'atteindrait jamais \
             l'écran. Bloc lu :\n{bloc}"
        );
    }
}
