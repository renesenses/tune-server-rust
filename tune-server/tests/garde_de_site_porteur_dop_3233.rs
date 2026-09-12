//! #3233 — la garde DoP est-elle encore APPELÉE, et toujours AVANT la
//! conversion ? Le témoin de site qui manquait depuis la v0.9.141.
//!
//! # Ce que ce fichier verrouille, et pourquoi il a fallu l'écrire
//!
//! `refuser_le_porteur_dop` a été livrée par la PR #3566 (v0.9.141), vérifiée
//! dans le dépôt le 08/09 puis **dans le binaire publié** le 09/09
//! (`local_audio_dop_carrier_destroyed_by_resample` : 0 en v0.9.140, 1 en
//! v0.9.144). À chaque ronde, le même verdict a été posé, et il était juste :
//!
//! > « La contre-épreuve est NÉGATIVE. Neutraliser la garde ne fait rougir
//! > aucun test. Les témoins de #3510 couvrent la *fonction pure*
//! > `rupture_du_porteur_dop`, pas ses QUATRE sites d'appel. On peut retirer
//! > les quatre `if refuser_le_porteur_dop(...)` et la suite reste verte. Une
//! > garde sans témoin de site est écrite, pas prouvée. »
//!
//! Mesure au tag publié, qui dit la même chose d'un mot :
//!
//! ```text
//! $ git grep -c refuser_le_porteur_dop v0.9.145
//! v0.9.145:tune-core/src/outputs/local.rs:5
//! ```
//!
//! **Une seule ligne de sortie : le fichier de production.** Aucun fichier de
//! test du dépôt ne nommait la garde. C'est ce trou-ci que ce fichier ferme,
//! et rien d'autre : il ne change aucun comportement audio.
//!
//! # Pourquoi une garde de SITE par `include_str!` et non un test d'exécution
//!
//! `tune-core/src/outputs/local.rs` vit derrière la feature `local-audio`, que
//! le job `Test` de `ci.yml` n'active pas
//! (`--no-default-features --features oaat,cloud-relay,bandcamp`) ; les deux
//! jobs qui l'activent sont conditionnés à `full` et ne sont donc jamais joués
//! sur une PR vers `batch/*`. Un test qui compilerait ce module serait **vert
//! contre rien** (#2816, « témoin endormi »). Lire le *texte* du fichier
//! échappe aux `cfg` : cette garde-ci s'exécute dans le job `Test`.
//!
//! Idiome du dépôt : `refus_de_peripherique_partage_dit_pourquoi.rs`,
//! `la_decision_de_cadence_est_branchee_sur_le_chemin_reel`
//! (`outputs/local/tests.rs`), `terminologie_eq.rs`.
//!
//! # Ce que la garde ne prouve PAS
//!
//! Elle prouve que le code appelle la garde avant la conversion. Elle ne
//! prouve **pas** le cas de Pierre M : WASAPI ne compile pas sur Shrek, la CI
//! compile Windows + ASIO mais **compiler n'est pas exécuter**, et son
//! `dsd_mode` de zone comme la cadence acceptée par son DAC manquent toujours.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

const LOCAL_RS: &str = include_str!("../../tune-core/src/outputs/local.rs");
// REF-8 (#2219) : la décision de cadence (`decide_local_rate_opening`,
// `note_rate_decision`) vit dans `BackendCpal::ouvrir` (`local/backend.rs`) ;
// la fermeture `refuser_le_porteur_dop` et l'étage restent dans `local.rs`.
// La garde lit les deux fichiers concaténés, jamais l'un à la place de l'autre.
const BACKEND_RS: &str = include_str!("../../tune-core/src/outputs/local/backend.rs");
// REF-8 (#2219) : la SECONDE route « décision DoP », celle des bras Windows
// exclusifs sur anneau entier (`local/etage_natif.rs`). Elle ne refuse pas le
// porteur : elle le porte. Voir `la_route_native_porte_le_porteur_dop_et_le_dit`.
const ETAGE_NATIF_RS: &str = include_str!("../../tune-core/src/outputs/local/etage_natif.rs");

// REF-8 (#2219) : le bras ASIO monte l'étage de R1 sur sa route traitée avec
// une fermeture qui refuse TOUT porteur DoP — la seconde route « refuser puis
// convertir », que cette garde doit nommer, et dont elle prouve l'ABSENCE sur
// la route native (le DoP y est porté, jamais refusé).
const BRAS_ASIO_RS: &str = include_str!("../../tune-core/src/outputs/local/bras_asio.rs");

/// La production seule : `local.rs` se termine par `#[cfg(test)] mod tests`,
/// dont le texte citerait nos propres motifs et rendrait la garde complaisante.
fn production() -> String {
    let fin = LOCAL_RS
        .find("#[cfg(test)]\nmod tests")
        .expect("local.rs doit garder son `#[cfg(test)] mod tests` en fin de fichier");
    [&LOCAL_RS[..fin], BACKEND_RS].concat()
}

/// Le texte sans commentaires ni blancs.
///
/// Retirer les blancs fait survivre la garde à un passage de `rustfmt` qui
/// recasserait les lignes — même idiome que
/// `les_quatre_charges_utiles_de_zone_appellent_le_contrat`. Retirer les
/// commentaires AVANT est ce qui permet aux quatre sites de garder la ligne
/// `// #3233 : …` qui les explique : sans ce passage, le commentaire se
/// collerait au code et l'aiguille ne se retrouverait plus.
///
/// `://` est épargné pour ne pas amputer une URL ou un chemin de greffon ALSA.
fn sans_commentaires_ni_blancs(source: &str) -> String {
    let mut assemble = String::with_capacity(source.len());
    for ligne in source.lines() {
        let mut garde = ligne;
        let mut depart = 0usize;
        while let Some(relatif) = ligne[depart..].find("//") {
            let absolu = depart + relatif;
            if absolu > 0 && ligne.as_bytes()[absolu - 1] == b':' {
                depart = absolu + 2;
                continue;
            }
            garde = &ligne[..absolu];
            break;
        }
        assemble.push_str(garde);
        assemble.push('\n');
    }
    assemble.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Le CORPS de la fermeture `refuser_le_porteur_dop`, et rien d'autre.
///
/// Chercher ses lignes dans le fichier entier ne prouverait rien :
/// `force_silent.store(true, …)`, `dop_active.store(false, …)` et le canal
/// `open_failure` vivent à plusieurs autres endroits de `local.rs`. Une
/// fermeture vidée resterait alors verte — c'est exactement ce qu'a montré la
/// contre-épreuve de ce fichier.
fn corps_de_la_fermeture() -> String {
    let production = production();
    // R1 (#2219) : la fermeture est passée à un puits par `&mut dyn FnMut`,
    // ce qui impose de la DÉCLARER `mut`. L'aiguille est donc l'affectation
    // elle-même, sans le mot-clé qui la précède.
    let debut = production
        .find("refuser_le_porteur_dop = |")
        .expect("#3233 — la fermeture `refuser_le_porteur_dop` a disparu de `local.rs`");
    let corps = &production[debut..];
    // La fermeture est déclarée à 12 espaces d'indentation : elle se referme
    // sur `};` à la même colonne, et c'est la première occurrence de ce motif.
    let fin = corps
        .find("\n            };")
        .expect("la fermeture doit se refermer à son indentation de déclaration");
    corps[..fin].to_string()
}

/// LA route, écrite comme un seul motif — et assemblée à l'exécution.
///
/// ⚠️ R1 (#2219) a supprimé les quatre copies que ce motif comptait. Les
/// quatre entrées PCM du chemin partagé existent toujours — piste initiale
/// WAV, piste initiale compressée, et les deux entrées de la piste enchaînée —
/// mais elles ne recopient plus la chaîne : toutes passent par
/// `EtageDeConversion::pousser`, seule route entre des octets décodés et le
/// puits. On ne compte donc plus des copies, on verrouille la route unique.
///
/// C'est strictement plus fort qu'un comptage : une CINQUIÈME entrée ne
/// pourrait pas contourner la garde, alors qu'avant elle n'avait qu'à oublier
/// de recopier les quatre lignes — ce qui est exactement le défaut que #3233
/// décrit.
///
/// ⚠️ R5 (#2219) a changé l'ORTHOGRAPHE de la route, pas la route : le format
/// source de l'étage est devenu un `AudioSpec` et `self.sample_rate` /
/// `self.channels` sont désormais des accesseurs. Le motif est mis à jour
/// mot pour mot — même appel, mêmes arguments, même place AVANT la conversion.
/// C'est le prix d'une garde qui lit du texte, et c'est aussi sa force : elle
/// a rougi dès le premier passage, au lieu de laisser une route se déplacer en
/// silence.
fn motif_de_la_route() -> String {
    [
        "ifrefuser_le_porteur_dop(bloc.dop,self.sample_rate(),self.channels()){",
        "returnPousseeVersLePuits::PorteurDopRefuse;",
        "}",
        // Ce qui SUIT est la moitié qui compte : la garde doit précéder la
        // conversion, jamais la suivre.
        "lettrames_source=bloc.source_frames;",
        "letmots=self.convertir(bloc.samples);",
        "ifpuits.ecrire(&mots){",
    ]
    .concat()
}

/// LE verrou : une route, et la garde placée AVANT la conversion.
///
/// Compter les appels ne suffirait pas. Le défaut que #3233 décrit n'est pas
/// « la garde manque » mais « le porteur DoP est détruit avant qu'on le
/// refuse » : une garde déplacée SOUS la conversion compterait pareil et ne
/// protégerait plus rien. Le motif enferme donc l'ordre.
#[test]
fn la_route_unique_refuse_le_porteur_dop_avant_toute_conversion() {
    let source = sans_commentaires_ni_blancs(&production());
    let routes = source.matches(&motif_de_la_route()).count();
    assert_eq!(
        routes, 1,
        "#3233 — le chemin cpal PARTAGÉ doit refuser le porteur DoP AVANT la \
         conversion, et n'avoir qu'UN endroit où le faire. J'en compte \
         {routes}. Une route retirée, déplacée sous la conversion, ou dont les \
         arguments ont changé, rend le DAC muet sans un mot : le sinc réécrit \
         le marqueur 0x05/0xFA du porteur, le DAC quitte le mode DSD et le \
         temps défile (Pierre M, fil 1043)."
    );

    // La conversion, elle, doit rester DERRIÈRE cette route : `adapt_channels`
    // et `rubato_resample_chunk` ne doivent pas réapparaître en ligne dans une
    // entrée PCM qui se passerait de la garde.
    let convertir = source
        .split("fnconvertir(&mutself,mutmots:Vec<f32>)->Vec<f32>{")
        .nth(1)
        .and_then(|s| s.split("fn").next())
        .expect("#3233 — `EtageDeConversion::convertir` doit rester identifiable");
    assert!(
        convertir.contains("adapt_channels(&mots,self.spec.canaux(),self.sortie.canaux)")
            && convertir.contains("rubato_resample_chunk("),
        "#3233 — la conversion source → sortie doit rester dans l'étage, \
         derrière la garde : l'en sortir rouvrirait la porte à une entrée PCM \
         qui convertit avant de refuser"
    );

    // Et les quatre entrées PCM doivent toutes y mener : deux amorces
    // (piste initiale, piste enchaînée) et la boucle commune qui sert les deux.
    assert!(
        source.matches("etage.pousser(").count() >= 2,
        "#3233 — les amorces des deux pistes doivent passer par la route unique"
    );
    assert!(
        source.contains("etage.pousser(puits,refuser_le_porteur_dop,&mut|bloc|{"),
        "#3233 — la boucle producteur commune doit passer par la route unique, \
         sans quoi la lecture continue court-circuiterait la garde"
    );
}

/// La garde ne doit pas seulement être appelée : quand elle refuse, elle doit
/// DIRE pourquoi et couper le son plutôt que d'envoyer du bruit au DAC.
///
/// C'est la définition de la fermeture, celle que la route partage.
/// Sans ce second verrou, une fermeture vidée de son corps laisserait l'appel
/// en place et le test ci-dessus vert.
///
/// ⚠️ La contre-épreuve de ce test a d'abord été NÉGATIVE, et pour la raison
/// la plus banale qui soit : cherchées dans le fichier ENTIER, les aiguilles
/// se trouvaient ailleurs. `force_silent.store(true, …)` existe à deux autres
/// endroits de `local.rs` — retirer la ligne de la fermeture ne changeait rien
/// au verdict. La recherche est donc bornée au CORPS de la fermeture, et rien
/// qu'à lui.
#[test]
fn la_fermeture_refusante_journalise_force_le_silence_et_retombe_le_dop() {
    let source = sans_commentaires_ni_blancs(&corps_de_la_fermeture());
    for (fragment, pourquoi) in [
        (
            "rupture.journaliser(&device_name);",
            "sans la ligne de journal, un refus est indiscernable d'un DAC muet",
        ),
        (
            "force_silent.store(true,Ordering::SeqCst);",
            "le porteur DoP refusé doit taire la sortie, pas la laisser jouer le \
             flux DSD comme du PCM — c'est le bruit blanc à pleine échelle",
        ),
        (
            "dop_active.store(false,Ordering::SeqCst);",
            "l'écran doit cesser d'annoncer un mode DSD que le DAC ne tient plus",
        ),
        (
            "*slot=Some(rupture.message_utilisateur(&device_name));",
            "sans ce canal, la zone s'arrête sans cause à l'écran (#3618)",
        ),
    ] {
        assert!(
            source.contains(fragment),
            "#3233 — la fermeture `refuser_le_porteur_dop` a perdu `{fragment}` : {pourquoi}"
        );
    }
}

/// Le retour en arrière EXACT : refuser APRÈS avoir converti.
///
/// Ce n'est pas une redite du premier test. Celui-ci rougit sur une forme que
/// le motif laisserait passer — la garde toujours là, mais rangée derrière la
/// conversion, où le porteur est déjà détruit.
///
/// ⚠️ R1 (#2219) : ce test cherchait deux motifs littéraux, qui portaient sur
/// des lignes que la réorganisation a supprimées. Cherchés tels quels, ils ne
/// se trouvaient plus — et le test restait VERT contre rien. Il compare
/// désormais des POSITIONS dans le corps de la route : un ordre ne peut pas
/// devenir vide.
#[test]
fn la_route_ne_refuse_pas_le_porteur_dop_apres_la_conversion() {
    let source = sans_commentaires_ni_blancs(&production());
    let corps = source
        .split("fnpousser(")
        .nth(1)
        .and_then(|s| s.split("fnrendre_la_queue_du_dsp(").next())
        .expect("#3233 — `EtageDeConversion::pousser` doit rester identifiable");

    let refus = corps
        .find("ifrefuser_le_porteur_dop(")
        .expect("#3233 — la route ne refuse plus le porteur DoP du tout");
    let conversion = corps
        .find("self.convertir(")
        .expect("#3233 — la route ne convertit plus : le motif à garder a disparu");
    let ecriture = corps
        .find("puits.ecrire(")
        .expect("#3233 — la route n'écrit plus au puits");

    assert!(
        refus < conversion && conversion < ecriture,
        "#3233 — la garde est passée SOUS la conversion : le sinc a déjà \
         réécrit le porteur DoP, le refus arrive trop tard et le DAC est muet \
         quoi qu'on journalise (refus={refus}, conversion={conversion}, \
         écriture={ecriture})"
    );
}

/// REF-8 (#2219) — la route TRAITÉE d'ASIO passe par la route unique de R1
/// avec une fermeture qui refuse tout porteur (`|dop, _, _| dop`), et le
/// refus est rapporté sous le motif d'avant (`DopUnsupported`, `"ASIO"`).
/// La route NATIVE, elle, ne refuse rien : un `PorteurDopRefuse` n'y existe
/// pas, le porteur traverse intact.
#[test]
fn la_route_traitee_d_asio_refuse_tout_porteur_et_la_native_n_en_refuse_aucun() {
    let source = sans_commentaires_ni_blancs(BRAS_ASIO_RS);
    assert!(
        source.contains("etage.pousser(puits.as_mut(),&mut|dop,_,_|dop,&mut|_|{})"),
        "#3233/REF-8 — la route traitée d'ASIO ne passe plus par `EtageDeConversion::pousser` \
         avec la fermeture qui refuse TOUT porteur DoP : un DoP y traverserait la conversion \
         flottante et sortirait en bruit blanc"
    );
    let flottante = source
        .split("Route::Flottante{etage,puits}=>{")
        .nth(1)
        .and_then(|s| s.split("}}}").next())
        .expect("la route traitée d'ASIO doit rester identifiable dans `Route::pousser`");
    assert!(
        flottante.contains("PousseeVersLePuits::PorteurDopRefuse=>Poussee::PorteurDopRefuse"),
        "#3233/REF-8 — le refus de l'étage de R1 n'est plus relayé par la route traitée"
    );
    assert!(
        source.contains("Poussee::PorteurDopRefuse=>{pcm_refusal=Some(WindowsExclusivePcmError::DopUnsupported);"),
        "#3233/REF-8 — le refus DoP de la route traitée n'est plus rapporté sous \
         `DopUnsupported` : l'écran perd sa cause"
    );
    assert!(
        source.contains("record_windows_exclusive_pcm_refusal(error,\"ASIO\","),
        "#3233/REF-8 — le refus n'est plus rapporté par `record_windows_exclusive_pcm_refusal` \
         avec \"ASIO\""
    );
    // Borné au bras natif de `Route::pousser`, quelle que soit la mise en
    // forme de `rustfmt` (bloc ou expression après `=>`).
    let native = source
        .split("Route::Native{etage,puits}=>")
        .nth(1)
        .and_then(|s| s.split("Route::Flottante{etage,puits}=>{").next())
        .expect("la route native d'ASIO doit rester identifiable dans `Route::pousser`");
    assert!(
        native.contains("etage.decoder_et_pousser("),
        "#3233/REF-8 — la route native d'ASIO ne passe plus par `EtageNatif::decoder_et_pousser`"
    );
    assert!(
        !native.contains("PorteurDopRefuse") && !native.contains("refuser_le_porteur_dop"),
        "#3233/REF-8 — la route native d'ASIO refuse un porteur DoP : elle doit le PORTER \
         (mots entiers, marqueurs intacts), le refus n'appartient qu'à la route flottante"
    );
}

/// Le TITRE de #3233 : « le filtre de cadence est TAUTOLOGIQUE quand
/// l'énumération échoue ». Sa garde de site existe — et elle ne s'exécute
/// jamais sur une PR vers `batch/*`.
///
/// # Ce que ce test ajoute, et pourquoi il n'est pas une redite
///
/// Le défaut du titre a été corrigé par `170fe51f` (« la cadence d'ouverture ne
/// se fonde plus sur des capacités supposées », PR #3252), **première version
/// publiée : v0.9.132**. Le ticket a été ouvert le 02/09 à 20 h 26 UTC ; la PR
/// #3252 a été fusionnée le 03/09 à 01 h 19 UTC, soit **moins de cinq heures
/// plus tard**, et v0.9.132 a été taguée le matin même. Aucun des trois
/// commentaires de vérification (07, 08 et 09/09) ne le relève : ils suivent
/// tous `refuser_le_porteur_dop`, qui traite un AUTRE défaut du même ticket.
///
/// Ce correctif a bien sa garde de site,
/// `la_decision_de_cadence_est_branchee_sur_le_chemin_reel`
/// (`tune-core/src/outputs/local/tests.rs`). Mais elle vit dans un module
/// derrière la feature `local-audio`, et la mesure sur `ci.yml` est sans appel :
///
/// | job | ligne | `local-audio` | joué sur une PR vers `batch/*` |
/// |---|---|---|---|
/// | `Test` | `cargo test … --no-default-features --features oaat,cloud-relay,bandcamp` | **non** | oui |
/// | `Test (jeu de fonctionnalités livré)` | `--features …,local-audio,…` | oui | **non** (`if: … full == 'true'`) |
/// | `cargo test -p tune-core --features audio-embedding` | défauts conservés, donc `local-audio` | oui | **non** (`full`) |
/// | `clippy` | `--all-targets --features …,local-audio,…` | oui | oui, mais clippy **compile sans exécuter** |
///
/// Autrement dit : la seule garde du défaut qui donne son titre à cette P0 est
/// **compilée** à chaque PR et **exécutée** à aucune. C'est la définition du
/// témoin endormi (#2816). Ce test-ci relit le même site depuis
/// `tune-server/tests`, qui tourne, lui, dans le job `Test`.
#[test]
fn la_decision_de_cadence_reste_branchee_et_le_filtre_tautologique_n_est_pas_revenu() {
    let source = sans_commentaires_ni_blancs(&production());

    let appel_reel = [
        "decide_local_rate_opening(sample_rate,default_sr,",
        "enumerated.is_some(),rate_evidence,)",
    ]
    .concat();
    assert!(
        source.contains(&appel_reel),
        "#3233 — le chemin cpal partagé doit passer par `decide_local_rate_opening` \
         en lui donnant la cadence de la source, celle du périphérique, la réponse \
         de l'énumération ET la preuve qui dit ce qu'elle vaut. Sans la preuve, la \
         décision redit oui à une liste fabriquée : sur WASAPI cpal retient les 21 \
         `COMMON_SAMPLE_RATES` sans rien demander à personne, un DSD64 décodé à \
         176 400 Hz est ouvert à 176 400 Hz quoi que sache faire l'endpoint, \
         `needs_resample` reste faux et rubato ne tourne jamais (Pierre M, fil 1043)"
    );

    let court_circuit = [
        "}elseifletSome(cfg)=find_matching_config(&device,channels,sample_rate)",
        ".filter(|c|c.sample_rate==sample_rate)",
    ]
    .concat();
    assert!(
        !source.contains(&court_circuit),
        "#3233 — le filtre TAUTOLOGIQUE est redevenu la CONDITION d'ouverture : \
         `find_matching_config` recopie la cadence demandée dans le `StreamConfig` \
         qu'il rend, donc l'égalité `c.sample_rate == sample_rate` est vraie par \
         CONSTRUCTION et la branche est prise quel que soit le matériel"
    );

    assert!(
        source.contains("note_rate_decision(ObservedRate{"),
        "#3233 — une décision qui change ce qui part au DAC doit atteindre le \
         client : sans `note_rate_decision`, il ne reste que le journal"
    );
}

/// REF-8 (#2219) — la SECONDE route « décision DoP », nommée.
///
/// La route cpal partagée REFUSE le porteur DoP avant toute conversion : le
/// sinc le détruirait. La route native des bras Windows exclusifs (WASAPI,
/// ASIO natif) n'a ni sinc ni adaptation de canaux : le mot part entier,
/// aligné à gauche, jusqu'au DAC. Elle ne refuse donc PAS le porteur — elle
/// le **détecte** (`is_dop_pcm` sur la première fenêtre 24 bits, verrouillé
/// par `dop_latched`), le **porte** tel quel (branche brute de
/// `prepare_windows_native_pcm` : ni volume, ni DSP) et le **dit** (`dop`
/// dans `EcritureNative::Poussee`). Ce test verrouille ces trois gestes, pour
/// qu'une « unification » des deux routes ne fasse pas refuser à WASAPI un
/// DoP qu'il joue aujourd'hui — ni ne fasse taire la décision.
///
/// La preuve d'exécution vit sur Shrek : `empreinte_wasapi_f70496` joue la
/// fixture DoP versionnée à travers l'étage, le puits, l'anneau et
/// `pop_pcm_bytes`, octet pour octet.
#[test]
fn la_route_native_porte_le_porteur_dop_et_le_dit() {
    let etage = sans_commentaires_ni_blancs(
        ETAGE_NATIF_RS
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("etage_natif.rs garde son `mod tests` en fin de fichier"),
    );
    let corps = etage
        .split("fndecoder_et_pousser(")
        .nth(1)
        .and_then(|s| s.split("fnrendre_la_queue(").next())
        .expect("REF-8 — `EtageNatif::decoder_et_pousser` doit rester identifiable");
    assert!(
        corps.contains("prepare_windows_native_pcm(")
            && corps.contains("self.must_classify_24_bit,")
            && corps.contains("self.dop_latched,"),
        "REF-8 — la route native ne décide plus le DoP par `prepare_windows_native_pcm` \
         (sonde 24 bits + verrou) : un porteur DoP serait traité comme du PCM, volume et DSP \
         compris, et le DAC quitterait le mode DSD"
    );
    assert!(
        corps.contains("self.dop_latched=prepared.dop;"),
        "REF-8 — la décision DoP n'est plus verrouillée : un flux mal formé pourrait \
         basculer à une frontière de bloc"
    );
    assert!(
        !corps.contains("refuser_le_porteur_dop") && !corps.contains("PorteurDopRefuse"),
        "REF-8 — la route native REFUSE le porteur DoP : elle le portait tel quel, et c'est \
         ce que les testeurs WASAPI écoutent (Pierre M, fil 1043)"
    );
    assert!(
        corps.contains("dop:prepared.dop,"),
        "REF-8 — la route native ne dit plus si elle porte du DoP : la zone ne peut plus \
         afficher le mode DSD ni caler le volume dessus"
    );

    // La branche brute de la préparation : DoP ⇒ `bit_perfect`, donc aucune
    // arithmétique — c'est `local.rs` qui la tient, et elle doit y rester.
    let preparation = sans_commentaires_ni_blancs(&production());
    let corps = preparation
        .split("fnprepare_windows_native_pcm(")
        .nth(1)
        .and_then(|s| s.split("Some(PreparedNativePcm{").next())
        .expect("#3233 — `prepare_windows_native_pcm` doit rester identifiable");
    assert!(
        corps
            .contains("letdop=dop_latched||(bit_depth==24&&is_dop_pcm(bytes,bit_depth,channels));")
            && la_branche_brute_protege_le_dop(corps),
        "REF-8 — la branche brute de `prepare_windows_native_pcm` ne protège plus le DoP du \
         volume et du DSP"
    );
}

fn la_branche_brute_protege_le_dop(corps: &str) -> bool {
    corps.contains("letbit_perfect=dop||(volume_units==1000&&local_dsp_is_identity(")
}
