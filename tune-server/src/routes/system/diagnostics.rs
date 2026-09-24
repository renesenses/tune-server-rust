use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

/// Number of recent log lines embedded in a bug report (kept modest so the
/// forum thread stays readable; the "Export logs" button has the full tail).
const BUG_REPORT_LOG_LINES: usize = 200;

/// Fenêtre LUE avant filtrage, pour que les 200 lignes retenues soient 200
/// lignes utiles.
///
/// Mesuré sur un rapport réel (#1884, Bertrand, analyse acoustique figée) :
/// **160 des 200 lignes étaient la même sonde `ssdp_unicast_probe_ok` en
/// DEBUG**, et le rapport ne contenait pas une seule ligne acoustique — la
/// fenêtre couvrait moins de trois minutes. Un rapport arrivé vide de ce qui
/// concerne le défaut oblige à redemander un journal complet, et un
/// signalement sur deux s'éteint en route.
const BUG_REPORT_LOG_SCAN_LINES: usize = 3000;

/// Ne garder d'un journal que ce qui documente un défaut.
///
/// Le DEBUG des modules de découverte est une sonde de bon fonctionnement :
/// sa place est dans le fichier et dans l'export complet, pas dans un rapport
/// de bogue où il chasse tout le reste. On ne garde donc que INFO et au-dessus.
///
/// Une ligne de continuation — celle d'une trace d'erreur, qui ne porte ni
/// horodatage ni niveau — hérite de la décision prise pour la ligne qui la
/// précède : découper une trace en deux vaudrait moins que de la jeter
/// entière.
fn lignes_utiles_pour_un_rapport(journal: &str, garder: usize) -> String {
    let mut retenu: Vec<&str> = Vec::new();
    // Une ligne sans niveau reconnu ouvre le journal : on la garde, faute de
    // quoi un format inattendu viderait le rapport au lieu de l'alléger.
    let mut on_garde = true;
    for ligne in journal.lines() {
        match niveau_de_ligne(ligne) {
            Some(niveau) => {
                on_garde = !matches!(niveau, "DEBUG" | "TRACE");
                if on_garde {
                    retenu.push(ligne);
                }
            }
            None => {
                if on_garde {
                    retenu.push(ligne);
                }
            }
        }
    }
    // Le rapport passe désormais par la MÊME sélection que l'export (#1974) :
    // un module ne peut occuper plus d'un quart de la fenêtre. Il ne l'avait
    // pas, et il tronquait bêtement.
    //
    // Trois journaux de testeurs la même semaine l'exigeaient, et jamais avec
    // le même coupable : chez Bilou, `tune_server::scan_import` et
    // `tune_core::metadata` prenaient les deux tiers de 1 003 lignes — zéro
    // ligne d'enrichissement ne survivait, alors que c'était le sujet de son
    // signalement ; chez Jean Valjean, la boucle de sondage UPnP en prenait
    // 807 sur 1 003. Plafonner le module tient quel que soit le bavard du
    // jour, là où nommer les coupables un à un ne tient jamais longtemps.
    //
    // Écrire ici un SECOND mécanisme aurait été le vrai piège : deux réponses
    // à la même question dérivent, et c'est exactement ce que la doctrine du
    // dépôt interdit.
    let candidates: Vec<String> = retenu.into_iter().map(str::to_owned).collect();
    let (gardees, ecartees) = selectionner_lignes(candidates, garder);
    let mut sortie = gardees.join("\n");
    // Un rapport qui tait ce qu'il a laissé tomber se lit comme s'il avait
    // tout montré — même règle que pour l'export.
    for (module, combien) in ecartees {
        sortie.push_str(&format!(
            "\n… {combien} lignes de « {module} » écartées du rapport (elles sont dans l'export complet)"
        ));
    }
    sortie
}

/// Le niveau d'une ligne de journal, quand elle en porte un.
///
/// Format écrit par `tracing` : `2026-08-17T15:22:15.003+02:00  DEBUG
/// tune_core::discovery::ssdp: …`. On ne cherche le niveau que dans les
/// premiers champs — un `DEBUG` au milieu d'un message ne doit pas faire
/// passer la ligne pour du DEBUG.
fn niveau_de_ligne(ligne: &str) -> Option<&'static str> {
    for mot in ligne.split_whitespace().take(3) {
        match mot {
            "TRACE" => return Some("TRACE"),
            "DEBUG" => return Some("DEBUG"),
            "INFO" => return Some("INFO"),
            "WARN" => return Some("WARN"),
            "ERROR" => return Some("ERROR"),
            _ => {}
        }
    }
    None
}

/// L'horodatage en tête d'une ligne de journal, sous la forme brute écrite par
/// `tracing` (`2026-08-20T09:03:15.059+02:00`), tronqué à la minute.
fn horodatage_de_ligne(ligne: &str) -> Option<&str> {
    let premier = ligne.split_whitespace().next()?;
    // `2026-08-20T09:03` — dix caractères de date, un `T`, cinq d'heure.
    if premier.len() >= 16 && premier.as_bytes()[10] == b'T' && premier.starts_with("20") {
        Some(&premier[..16])
    } else {
        None
    }
}

/// La période réellement couverte par un extrait de journal, `du … au …`.
///
/// Trois mille lignes couvrent des heures sur un serveur au repos et **dix
/// minutes** sur un serveur qui scanne (#2028). L'utilisateur qui décrit un
/// blocage vieux de plusieurs heures nous envoie alors un journal qui ne peut
/// rien en contenir — et rien, ni pour lui ni pour nous, ne distingue ce
/// rapport-là d'un rapport qui couvre la journée. On l'annonce donc.
fn periode_couverte(extrait: &str) -> Option<String> {
    let mut lignes = extrait.lines().filter_map(horodatage_de_ligne);
    let debut = lignes.next()?;
    match lignes.last() {
        Some(fin) if fin != debut => Some(format!("du {debut} au {fin}")),
        _ => Some(format!("à {debut}")),
    }
}

/// Public bug-intake endpoint on the community site. It creates a *moderated*
/// (pending) forum thread server-side with the site's own credentials — the
/// distributed Tune server never holds a forum admin token. Same
/// `/api/v1/community/*` family as the DAC-profile / covers endpoints.
const BUG_REPORT_SUBMIT_URL: &str = "https://mozaiklabs.fr/api/v1/community/bug-report";

/// Racine du service communautaire, `mozaiklabs.fr` sauf réglage contraire.
///
/// Le réglage `mozaik_base_url` existe déjà et sert le même office pour l'API
/// support (`routes/support.rs::base_url`) : on le RÉUTILISE plutôt que d'en
/// inventer un second. Sans lui, le contrat sortant de #4564 — les captures
/// posées sous `images[]` — ne serait éprouvé par personne : un `const` en dur
/// ne se remplace pas dans un banc, et une garde qui n'observe pas les octets
/// qui sortent ne garde rien.
fn bug_report_url(state: &AppState) -> String {
    let base = SettingsRepo::with_backend(state.backend.clone())
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    match base {
        Some(base) => format!("{base}/api/v1/community/bug-report"),
        None => BUG_REPORT_SUBMIT_URL.to_string(),
    }
}

/// The community endpoint caps the thread body at 50k chars; keep headroom.
const BUG_REPORT_MAX_BODY_CHARS: usize = 49_000;

/// Relevé de la famine de l'anneau audio, sortie par sortie (#3205).
///
/// Ce qui est compté : un rappel du pilote à qui l'anneau a rendu MOINS
/// d'échantillons qu'il n'en demandait, le reste étant parti en zéros vers le
/// DAC. C'est un trou audible, et il dit qu'un PRODUCTEUR n'a pas suivi —
/// réseau, décodage, convolution.
///
/// 🔴 Il ne capture PAS l'ordonnancement du noyau, contrairement à ce que ce
/// commentaire affirmait : sur un XRun, cpal saute le rappel de données, donc
/// l'anneau reste plein et ce compteur ne bouge pas. `driver_underruns`, plus
/// bas, est le chiffre qui voit cet incident-là.
///
/// Ce qui n'est PAS compté ici : l'« underrun » ALSA que cpal remonte en
/// `StreamError` et que la sortie locale laisse délibérément passer sans
/// démonter le flux (« ALSA underruns are routine »). Celui-là parle du
/// PILOTE, pas de l'anneau ; il est routinier, et additionné au précédent il
/// rendrait le chiffre inexploitable. Les deux vivent sous deux noms
/// distincts, ici comme dans le contrat de sortie.
///
/// Pourquoi ce chiffre existe : Tune OS paie le Secure Boot et un dépôt COPR
/// non signé pour un noyau `PREEMPT_RT` dont le bénéfice n'a jamais été
/// mesuré. Avec un anneau de deux secondes et une garde de 500 ms, une latence
/// d'ordonnancement de quelques millisecondes est invisible ; ce qui se voit,
/// c'est le nombre de fois où le rappel a manqué de données. S'il reste à zéro
/// une semaine sur un parc réel en noyau standard, le noyau RT est un coût
/// sans gain.
///
/// `try_lock` et non `lock` : un diagnostic ne doit jamais attendre derrière
/// une sortie en train de jouer — même choix que la section OAAT du rapport de
/// bogue.
///
/// 🔴 #3801 — et c'est là que le relevé se retournait contre lui-même. Le
/// `?` de `try_lock().ok()?` faisait DISPARAÎTRE la sortie de la liste. Or une
/// sortie n'est verrouillée que parce que quelqu'un la tient : le sondeur, qui
/// la prend à chaque tick pour lire son statut, ou l'orchestrateur. C'est-à-dire
/// pendant qu'elle JOUE — le seul moment où le chiffre veut dire quelque chose.
///
/// Le dossier #3801 (Didier, décrochages FLAC 16/44 en sortie locale WASAPI)
/// tient tout entier sur une mesure : `ring_starvation` pour sa zone, pendant
/// un morceau qui décroche. Un `events` non nul place la panne en AMONT de la
/// sortie, un zéro la place en AVAL. Une LIGNE ABSENTE, elle, ne se distingue
/// pas d'un zéro : le JSON ne porte plus rien pour cette sortie, et la section
/// du rapport de bogue disparaît entièrement (`if !ring_starvation.is_empty()`).
/// Le testeur envoie un rapport muet, le triage lit « pas de famine », et la
/// moitié du problème qu'une seule mesure devait éliminer est éliminée à tort.
///
/// Depuis #3801, une sortie verrouillée laisse une ligne qui DIT qu'elle n'a pas
/// pu être mesurée. Une sortie sans anneau — tout renderer réseau — reste hors
/// du relevé : elle n'a rien à mesurer, et c'est une propriété permanente de la
/// sortie, pas l'accident d'un instant.
async fn releve_famine_anneau(state: &AppState) -> Vec<Value> {
    let outputs = state.outputs.lock().await;
    outputs
        .list()
        .iter()
        .filter_map(|id| {
            let output = outputs.get(id)?;
            match output.try_lock() {
                Err(_) => ligne_famine(id, EtatDuReleve::Occupee),
                Ok(sortie) => {
                    let etat = match sortie.ring_starvation() {
                        Some(famine) => EtatDuReleve::Mesure {
                            nom: sortie.name(),
                            famine,
                        },
                        None => EtatDuReleve::SansAnneau,
                    };
                    ligne_famine(id, etat)
                }
            }
        })
        .collect()
}

/// Ce qu'un relevé a pu établir pour UNE sortie (#3801).
///
/// Les trois cas sont disjoints et le troisième n'est PAS le deuxième : une
/// sortie sans anneau n'a rien à mesurer, une sortie occupée a quelque chose à
/// mesurer et on n'a pas pu le lire.
enum EtatDuReleve<'a> {
    /// Verrou pris, la sortie tient un anneau : voici son compteur.
    Mesure {
        nom: &'a str,
        famine: tune_core::outputs::traits::OutputRingStarvation,
    },
    /// Verrou pris, aucun anneau : tout renderer réseau. Propriété permanente
    /// de la sortie, elle reste hors du relevé.
    SansAnneau,
    /// Verrou refusé : la sortie était occupée à l'instant du relevé.
    Occupee,
}

/// La ligne de relevé d'une sortie, ou `None` si elle n'a rien à y faire.
///
/// **Pourquoi une ligne pour une sortie occupée** : voir le bloc #3801 sur
/// [`releve_famine_anneau`]. Elle ne porte AUCUNE des clés chiffrées, et c'est
/// délibéré — tous les lecteurs de ce tableau font `as_u64().unwrap_or(0)`, et
/// une clé à zéro serait exactement le mensonge que cette ligne existe pour
/// empêcher. Une clé absente n'est pas un zéro : elle force le lecteur à
/// traiter le cas.
///
/// `output_name` manque aussi, pour la raison qui a produit la ligne : le nom
/// se lit à travers le verrou, celui-là même qui n'a pas pu être pris.
fn ligne_famine(output_id: &str, etat: EtatDuReleve<'_>) -> Option<Value> {
    match etat {
        EtatDuReleve::SansAnneau => None,
        EtatDuReleve::Occupee => Some(json!({
            "output_id": output_id,
            "mesure": "indisponible",
            "raison": "sortie verrouillée au moment du relevé (elle jouait, ou le sondeur la lisait)",
        })),
        EtatDuReleve::Mesure { nom, famine } => Some(json!({
            "output_id": output_id,
            "output_name": nom,
            "mesure": "lue",
            "ring_starvation_events": famine.events,
            "ring_starvation_missing_samples": famine.missing_samples,
            "driver_underruns": famine.driver_underruns,
            "served_samples": famine.served_samples,
            "stream_ms": famine.stream_ms,
        })),
    }
}

/// La section « famine de l'anneau » du rapport de bogue, à partir du relevé.
///
/// Extraite du corps de `bug_report_markdown` par #3801 pour qu'un témoin
/// puisse la lire : c'est ce texte-là que le testeur colle sur le forum, et
/// c'est donc lui, et pas le JSON, qui doit rendre impossible de confondre
/// « mesuré à zéro » et « pas mesuré ».
fn section_famine_anneau(releve: &[Value]) -> String {
    if releve.is_empty() {
        return String::new();
    }
    let mut md = String::from("## Ring starvation (famine de l'anneau audio)\n");
    for s in releve {
        if s["mesure"] == "indisponible" {
            md.push_str(&format!(
                "- {} : ⚠ MESURE INDISPONIBLE — {}. Ce n'est PAS un compteur à zéro : \
                 l'anneau de cette sortie n'a pas pu être lu.\n",
                s["output_id"].as_str().unwrap_or("?"),
                s["raison"].as_str().unwrap_or("sortie verrouillée"),
            ));
            continue;
        }
        md.push_str(&format!(
            "- {} : {} événement(s), {} échantillon(s) manquant(s) sur {} servis ({} ms de flux) ; {} sous-alimentation(s) du pilote\n",
            s["output_name"].as_str().unwrap_or("?"),
            s["ring_starvation_events"].as_u64().unwrap_or(0),
            s["ring_starvation_missing_samples"].as_u64().unwrap_or(0),
            s["served_samples"].as_u64().unwrap_or(0),
            s["stream_ms"].as_u64().unwrap_or(0),
            s["driver_underruns"].as_u64().unwrap_or(0),
        ));
    }
    md.push_str(
        "  (un événement = un rappel audio comblé par des zéros, donc un \
         PRODUCTEUR en retard ; la sous-alimentation du pilote est l'autre \
         panne — le processus pas ordonnancé à temps — et c'est elle qui \
         décide du noyau RT de Tune OS)\n\n",
    );
    md
}

/// #3801 — Didier, décrochages FLAC 16/44 en sortie locale WASAPI (fil 1741).
///
/// Le dossier tient sur UNE mesure, et elle est déjà livrée chez lui :
/// `ring_starvation` pour sa zone, pendant un morceau qui décroche. Non nul, la
/// panne est en amont de la sortie ; nul, elle est en aval. Une seule lecture
/// élimine la moitié du problème.
///
/// Ces témoins gardent la seule chose qui pouvait rendre cette mesure
/// trompeuse : qu'elle soit ABSENTE et se lise comme un zéro.
///
/// **Ce qu'ils couvrent** : la construction du relevé et le texte que le
/// testeur colle sur le forum. Tout est indépendant de la plate-forme — pas un
/// `cfg(windows)`, pas la feature `local-audio` — et tourne dans la cible `lib`
/// de `tune-server`, donc à chaque `cargo test --workspace`.
///
/// **Ce qu'ils NE couvrent PAS** : le décrochage lui-même. Rien ici ne joue de
/// son, n'ouvre WASAPI, ni ne prouve où la panne de Didier se trouve. Le fil
/// de rendu WASAPI (`outputs/wasapi_exclusive.rs`) et le bras
/// `outputs/local/bras_wasapi.rs` sont `cfg(target_os = "windows")` : ni Shrek
/// ni le Mac ne les compilent, et aucun job ne les EXÉCUTE.
#[cfg(test)]
mod releve_famine_visible_3801 {
    use super::*;
    use tune_core::outputs::traits::OutputRingStarvation;

    fn compteur_a_zero() -> OutputRingStarvation {
        OutputRingStarvation::default()
    }

    /// LE témoin du dossier. Une sortie dont le verrou n'a pas pu être pris —
    /// c'est-à-dire une sortie qui JOUE, la seule qui vaille d'être mesurée —
    /// doit laisser une ligne. Avant #3801, le `?` de `try_lock().ok()?` la
    /// faisait purement disparaître du tableau.
    #[test]
    fn une_sortie_occupee_laisse_une_ligne_au_lieu_de_disparaitre() {
        let ligne = ligne_famine("local:SMSL SU-8", EtatDuReleve::Occupee).expect(
            "une sortie occupée doit laisser une ligne : son ABSENCE se lit comme un \
             compteur à zéro, et c'est la lecture qui a fermé #3801 à tort",
        );
        assert_eq!(ligne["output_id"], "local:SMSL SU-8");
        assert_eq!(ligne["mesure"], "indisponible");
        assert!(
            ligne["raison"].as_str().is_some_and(|r| !r.is_empty()),
            "la ligne doit dire POURQUOI la mesure manque"
        );
    }

    /// Et elle ne doit porter aucun chiffre : tous les lecteurs du tableau font
    /// `as_u64().unwrap_or(0)`, donc une clé posée à zéro serait le mensonge
    /// même que la ligne existe pour empêcher.
    #[test]
    fn la_ligne_indisponible_ne_porte_aucun_compteur() {
        let ligne = ligne_famine("local:SMSL SU-8", EtatDuReleve::Occupee).unwrap();
        for cle in [
            "ring_starvation_events",
            "ring_starvation_missing_samples",
            "driver_underruns",
            "served_samples",
            "stream_ms",
        ] {
            assert!(
                ligne.get(cle).is_none(),
                "la ligne d'une mesure indisponible ne doit porter aucun compteur, \
                 or elle porte « {cle} » : un lecteur en `unwrap_or(0)` y lirait un zéro \
                 fabriqué"
            );
        }
    }

    /// La contre-épreuve de la contre-épreuve : une sortie SANS anneau — tout
    /// renderer réseau — reste hors du relevé. Elle n'a rien à mesurer, et
    /// c'est une propriété permanente, pas l'accident d'un instant. Sans cette
    /// garde, le correctif noierait le relevé sous une ligne par renderer.
    #[test]
    fn une_sortie_sans_anneau_reste_hors_du_releve() {
        assert!(
            ligne_famine("dlna:Cabasse", EtatDuReleve::SansAnneau).is_none(),
            "un renderer réseau n'a pas d'anneau : il n'a rien à faire dans le relevé"
        );
    }

    /// Une mesure réellement lue garde EXACTEMENT ses clés d'avant #3801 : le
    /// correctif ne doit rien changer à ce que lisent les rapports déjà
    /// déposés.
    #[test]
    fn une_mesure_lue_garde_ses_cles() {
        let famine = OutputRingStarvation {
            events: 41,
            missing_samples: 17_640,
            served_samples: 3_528_000,
            driver_underruns: 0,
            stream_ms: 40_000,
        };
        let ligne = ligne_famine(
            "local:SMSL SU-8",
            EtatDuReleve::Mesure {
                nom: "Sortie SMSL SU-8",
                famine,
            },
        )
        .unwrap();
        assert_eq!(ligne["output_name"], "Sortie SMSL SU-8");
        assert_eq!(ligne["ring_starvation_events"], 41);
        assert_eq!(ligne["ring_starvation_missing_samples"], 17_640);
        assert_eq!(ligne["served_samples"], 3_528_000);
        assert_eq!(ligne["stream_ms"], 40_000);
        assert_eq!(ligne["driver_underruns"], 0);
    }

    /// Le texte que le testeur colle sur le forum. Une mesure indisponible ne
    /// doit PAS s'y écrire comme la phrase d'un compteur à zéro — c'est la
    /// forme sous laquelle la confusion arrive jusqu'au triage.
    #[test]
    fn le_rapport_ne_lit_pas_une_mesure_absente_comme_un_zero() {
        let indisponible = ligne_famine("local:SMSL SU-8", EtatDuReleve::Occupee).unwrap();
        let section = section_famine_anneau(std::slice::from_ref(&indisponible));

        assert!(
            section.contains("local:SMSL SU-8"),
            "la sortie doit être NOMMÉE dans le rapport, sans quoi le testeur envoie un \
             rapport muet : {section}"
        );
        assert!(
            section.contains("MESURE INDISPONIBLE"),
            "le rapport doit dire que la mesure manque : {section}"
        );
        assert!(
            !section.contains("0 événement(s)"),
            "le rapport écrit « 0 événement(s) » pour une mesure qui n'a jamais été \
             prise : c'est le zéro fabriqué de #3801, et il envoie chercher la panne en \
             aval de l'anneau : {section}"
        );
    }

    /// Et un zéro RÉEL, lui, doit continuer de se lire comme un zéro : c'est
    /// une mesure, et elle vaut autant que l'autre moitié du diagnostic.
    #[test]
    fn un_zero_reellement_mesure_reste_un_zero_dans_le_rapport() {
        let mesuree = ligne_famine(
            "local:SMSL SU-8",
            EtatDuReleve::Mesure {
                nom: "Sortie SMSL SU-8",
                famine: compteur_a_zero(),
            },
        )
        .unwrap();
        let section = section_famine_anneau(std::slice::from_ref(&mesuree));
        assert!(section.contains("Sortie SMSL SU-8"));
        assert!(
            section.contains("0 événement(s)"),
            "un zéro mesuré doit rester lisible comme tel : {section}"
        );
        assert!(!section.contains("MESURE INDISPONIBLE"));
    }

    /// Un relevé vide n'écrit toujours aucune section : le comportement
    /// d'avant l'extraction de `section_famine_anneau`.
    #[test]
    fn un_releve_vide_n_ecrit_aucune_section() {
        assert_eq!(section_famine_anneau(&[]), "");
    }
}

/// #3479 — ce que l'étage d'égalisation PRODUIT, et pas seulement ce qu'il
/// annonce.
///
/// `eq_change_journal` (v0.9.141) et `duree_ms` / `amortissement` (v0.9.145)
/// mesurent l'INSTALLATION de l'étage : famille de sortie, format avant et
/// après, pré-gain, premier échec. Reivax66 en a déposé 25 lignes, toutes
/// concordantes — `premier_echec="-"`, `format_avant == format_apres`, aucune
/// famine d'anneau — pendant que son symptôme était « l'égaliseur coupe le son
/// mais n'interrompt pas la lecture ».
///
/// Ces deux faits ne se contredisent pas : ils portent sur deux choses
/// différentes. Un étage qui s'installe sans erreur peut rendre du **silence**
/// échantillon par échantillon, et `EqProcessor` sait exactement quand cela
/// arrive — `process_interleaved` compte `non_finite_samples` et remet le
/// sample à zéro (`audio/eq.rs`). Une cascade de biquads devenue instable
/// (coefficients extrêmes, Q élevé à cadence basse) produit des `NaN` en
/// chaîne : l'anneau reste alimenté, servi à l'heure, et le DAC reçoit des
/// zéros. C'est le seul mécanisme INTERNE à l'étage qui rende exactement le
/// symptôme décrit.
///
/// Ce compteur existe depuis longtemps et atteint déjà
/// `/zones/{id}/signal-path`. Mais le rapport de diagnostic — **ce que le
/// testeur dépose** — ne le portait pas, et aucune ligne de journal ne le dit
/// non plus. Il était donc mesuré et illisible, exactement comme la famine de
/// l'anneau avant #3205.
///
/// ⚠️ Un `0` ici n'innocente pas l'égaliseur : il écarte le repliement sur
/// zéro, pas un pré-gain mal calculé ni un étage en aval. Il retire une
/// hypothèse de la liste, ce qui est tout ce qu'on lui demande.
///
/// ⚠️ **Ce que ce chiffre compte, exactement.** `process_stats` compte depuis
/// la construction de l'`EqProcessor`. Sur le chemin `local_a_chaud`, un
/// processeur neuf est bâti à chaque cran de curseur — sept en 1,5 s dans
/// l'export de Reivax66 — et `inherit_state_from` lui transmet désormais les
/// compteurs avec l'historique des filtres, faute de quoi le nombre repartait
/// de zéro au moment même que le ticket décrit. Il reste remis à zéro quand la
/// **forme** de la cascade change (une bande qui sort par `is_neutral()`,
/// un changement de nombre de canaux) et à chaque nouvelle piste : ce n'est
/// alors plus le même étage.
///
/// `try_lock` et non `lock`, même raison que [`releve_famine_anneau`] : un
/// diagnostic n'attend jamais derrière une sortie en train de jouer.
async fn releve_dsp_egaliseur(state: &AppState) -> Vec<Value> {
    let outputs = state.outputs.lock().await;
    outputs
        .list()
        .iter()
        .filter_map(|id| {
            let output = outputs.get(id)?;
            let output = output.try_lock().ok()?;
            let metriques = output.dsp_metrics()?;
            Some(json!({
                "output_id": id,
                "output_name": output.name(),
                "eq_overs": metriques.eq_overs,
                "eq_non_finite_samples": metriques.eq_non_finite_samples,
            }))
        })
        .collect()
}

/// CLD-3 — les reports 429 du cloud, lisibles dans le rapport.
///
/// Quand mozaiklabs.fr répond 429, chaque portée (`CloudScope`) retient ses
/// appels jusqu'à l'échéance ; jusqu'ici seul `/cloud/telemetry/status` le
/// disait, et personne ne le lisait. Le rapport de diagnostic est ce que le
/// testeur colle sur le forum : une bio qui n'arrive pas, une proposition de
/// métadonnées qui n'est pas envoyée, doivent pouvoir se lire comme « portée
/// retenue encore N secondes », pas comme une panne muette. `remaining_seconds`
/// est borné à zéro : une échéance passée n'est pas une dette négative.
fn rapport_des_reports_cloud(
    actifs: &[tune_core::cloud::rate_limit::ActiveCloudBackoff],
    maintenant_epoch: u64,
) -> Value {
    let portees: Vec<Value> = actifs
        .iter()
        .map(|a| {
            json!({
                "scope": a.scope,
                "until_epoch": a.until_epoch,
                "remaining_seconds": a.until_epoch.saturating_sub(maintenant_epoch),
                "retry_after_seconds": a.retry_after_seconds,
            })
        })
        .collect();
    json!({
        "count": portees.len(),
        "scopes": portees,
    })
}

fn maintenant_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── DUP-1, phase 0 — doublons présumés de zones, en LECTURE SEULE ───────────

/// Ce que le diagnostic a besoin de savoir d'une zone. Projection de `Zone`
/// pour que la règle se teste sans base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZoneVue {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) output_type: String,
    pub(crate) output_device_id: String,
    pub(crate) online: bool,
}

pub(crate) fn zone_vue(z: &tune_core::db::zone_repo::Zone) -> Option<ZoneVue> {
    Some(ZoneVue {
        id: z.id?,
        name: z.name.clone(),
        output_type: z.output_type.clone().unwrap_or_default(),
        output_device_id: z.output_device_id.clone()?,
        online: z.online,
    })
}

/// L'adresse IPv4 d'un identifiant AirPlay historique `airplay-<ip>-<port>`.
fn ip_d_identifiant_airplay(reste: &str) -> Option<&str> {
    let (hote, _port) = reste.rsplit_once('-')?;
    let quatre =
        hote.split('.').count() == 4 && hote.chars().all(|c| c.is_ascii_digit() || c == '.');
    quatre.then_some(hote)
}

/// La clé d'APPAREIL d'une zone : ce qui reste quand on retire ce qui
/// n'identifie rien (mesure du 05/09 sur .18) :
/// - UPnP : `uuid:` retiré, suffixe `_MR` ou `_MS` retiré, minuscules. Un
///   Sonos annonce TROIS UDN pour un seul appareil : la racine ZonePlayer,
///   `…_MR` (sous-appareil MediaRenderer) et `…_MS` (MediaServer). Les trois
///   ont fait trois zones sur le serveur de test — relevé du 09/09 sur .18 :
///   « Chambre » en 8 (racine), 6 (`_MR`) et 9 (`_MS`), « Cuisine » en 7, 11
///   et 12. Ne retirer que `_MR` laissait la troisième hors du rapport ET
///   inéligible à la fusion, alors que c'est le même haut-parleur ;
/// - AirPlay historique `airplay-<ip>-<port>` : l'adresse ne dit rien de
///   stable (l'Apple TV du 13/08 est devenue un Sonos) ; si un appareil
///   découvert porte cette adresse ET une adresse matérielle, c'est elle la clé,
///   sinon l'adresse IP, faute de mieux ;
/// - AirPlay `airplay-<mac>` : l'adresse matérielle.
///
/// `None` pour les sorties sans identité réseau (locale, navigateur, OAAT).
pub(crate) fn cle_appareil(
    zone: &ZoneVue,
    appareils: &[tune_core::discovery::device::DiscoveredDevice],
) -> Option<String> {
    let id = zone.output_device_id.trim();
    if let Some(reste) = id.strip_prefix("airplay-") {
        if let Some(ip) = ip_d_identifiant_airplay(reste) {
            let mac = appareils
                .iter()
                .find(|d| d.host == ip)
                .and_then(|d| d.mac_address.as_deref())
                .map(|m| m.to_ascii_lowercase());
            return Some(match mac {
                Some(m) => format!("mac:{m}"),
                None => format!("ip:{ip}"),
            });
        }
        return Some(format!("mac:{}", reste.to_ascii_lowercase()));
    }
    if let Some(reste) = id.strip_prefix("uuid:") {
        // `strip_suffix` et non `trim_end_matches` : on retire UN suffixe de
        // sous-appareil, pas une répétition. Les deux suffixes sont ceux que
        // Sonos annonce et rien d'autre n'est deviné — une identité qu'on ne
        // sait pas prouver ne doit pas devenir une fusion (13/08).
        let socle = reste
            .strip_suffix("_MR")
            .or_else(|| reste.strip_suffix("_MS"))
            .unwrap_or(reste);
        return Some(format!("udn:{}", socle.to_ascii_lowercase()));
    }
    None
}

/// L'hôte réseau d'une zone, pour rapprocher deux PROTOCOLES d'un même
/// appareil (Eversolo en DLNA et en AirPlay) : l'adresse de l'appareil
/// découvert qui porte l'identifiant, ou l'adresse contenue dans un
/// identifiant AirPlay historique.
fn hote_de_zone(
    zone: &ZoneVue,
    appareils: &[tune_core::discovery::device::DiscoveredDevice],
) -> Option<String> {
    if let Some(d) = appareils.iter().find(|d| d.id == zone.output_device_id) {
        return Some(d.host.clone());
    }
    zone.output_device_id
        .strip_prefix("airplay-")
        .and_then(ip_d_identifiant_airplay)
        .map(str::to_string)
}

/// DUP-1 (phase 2) : dans un groupe, une zone hors ligne dont une jumelle est
/// en ligne est PROBABLEMENT remplacee — l'appareil a change d'identifiant et
/// l'ancienne ligne ne reviendra pas. C'est une proposition de fusion
/// (`POST /zones/{doublon}/fusionner-dans/{cible}`), jamais une action, et
/// jamais deduite de l'age seul : une zone seule, si vieille soit-elle, est
/// eteinte, pas remplacee.
fn groupe_json(motif: &str, cle: &str, zones: &[&ZoneVue]) -> Value {
    let en_ligne = zones.iter().filter(|z| z.online).count();
    // #3747 — un groupe NOMMÉ n'est pas un groupe FUSIONNABLE.
    //
    // `POST /zones/{doublon}/fusionner-dans/{cible}` exige que les deux zones
    // rendent la MÊME clé `cle_appareil`, non nulle ; elle refuse tout le
    // reste par `409 zones_distinctes`. Or la SECONDE règle de ce rapport
    // groupe par HÔTE, et par construction aucune de ses zones ne partage de
    // clé d'appareil avec une autre : celles qui en partagent une sont déjà
    // sorties par la première règle, et sont dans `deja`.
    //
    // Un groupe « même hôte, deux protocoles » sortait donc avec exactement la
    // forme d'une famille fusionnable, alors qu'AUCUNE action ne peut le
    // suivre : un Eversolo vu en SSDP/DLNA et en mDNS/AirPlay est deux espaces
    // d'identifiants disjoints. Le refus de la route est correct ; c'est le
    // rapport qui promettait ce qu'il ne pouvait pas tenir. Il le dit
    // maintenant lui-même, et dit POURQUOI.
    let fusionnable = !cle.starts_with("hote:");
    json!({
        "motif": motif,
        "cle": cle,
        "en_ligne": en_ligne,
        "fusionnable": fusionnable,
        "fusion_refusee_motif": (!fusionnable).then_some(
            "deux protocoles de découverte différents sur le même hôte : \
             SSDP/DLNA et mDNS/AirPlay n'ont aucun identifiant commun, et la \
             fusion serait refusée (409 zones_distinctes). Supprimez la zone \
             dont vous ne voulez pas ; ses réglages ne sont pas reportés.",
        ),
        "zones": zones.iter().map(|z| json!({
            "id": z.id,
            "name": z.name,
            "output_type": z.output_type,
            "output_device_id": z.output_device_id,
            "online": z.online,
            "remplacee_probable": !z.online && en_ligne > 0,
        })).collect::<Vec<_>>(),
    })
}

/// DUP-1, phase 0 : les groupes de zones qui désignent PROBABLEMENT le même
/// appareil. Deux règles, dans cet ordre : même clé d'appareil (UDN ou adresse
/// matérielle), puis même hôte sous deux protocoles. Rien n'est fusionné —
/// les homonymes existent, une adresse se réattribue — le rapport NOMME, et
/// l'utilisateur ou un chantier suivant tranche.
pub(crate) fn doublons_de_zones(
    zones: &[ZoneVue],
    appareils: &[tune_core::discovery::device::DiscoveredDevice],
) -> Vec<Value> {
    use std::collections::BTreeMap;
    let mut par_cle: BTreeMap<String, Vec<&ZoneVue>> = BTreeMap::new();
    for z in zones {
        if let Some(cle) = cle_appareil(z, appareils) {
            par_cle.entry(cle).or_default().push(z);
        }
    }
    let mut groupes = Vec::new();
    let mut deja: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    for (cle, zs) in &par_cle {
        if zs.len() < 2 {
            continue;
        }
        let motif = if cle.starts_with("udn:") {
            "même appareil UPnP (UDN, suffixe _MR ou _MS retiré)"
        } else if cle.starts_with("mac:") {
            "même appareil AirPlay (adresse matérielle)"
        } else {
            "même adresse IP AirPlay (identifiant historique)"
        };
        deja.extend(zs.iter().map(|z| z.id));
        groupes.push(groupe_json(motif, cle, zs));
    }
    let mut par_hote: BTreeMap<String, Vec<&ZoneVue>> = BTreeMap::new();
    for z in zones {
        if deja.contains(&z.id) {
            continue;
        }
        if let Some(h) = hote_de_zone(z, appareils) {
            par_hote.entry(h).or_default().push(z);
        }
    }
    for (hote, zs) in &par_hote {
        let types: std::collections::BTreeSet<&str> =
            zs.iter().map(|z| z.output_type.as_str()).collect();
        if zs.len() >= 2 && types.len() >= 2 {
            groupes.push(groupe_json(
                "même hôte, deux protocoles",
                &format!("hote:{hote}"),
                zs,
            ));
        }
    }
    groupes
}

pub(super) async fn diagnostics(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    // #3182 : lue sur le moteur ACTIF, et `null` quand elle n'est pas lisible.
    // Le `else { 0 }` d'avant faisait dire à toute base PostgreSQL qu'elle
    // n'avait jamais été migrée. Voir `super::version_de_schema`.
    let db_version = super::version_de_schema(&state);
    let music_dirs = super::get_music_dirs_list(&state.backend);
    let uptime_secs = state.started_at.elapsed().as_secs();

    // Zone count
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone_count = zone_repo.count().unwrap_or(0);
    // DUP-1 (phase 0) : les zones telles qu'elles sont, pour nommer les doublons.
    let zones_vues: Vec<ZoneVue> = zone_repo
        .list()
        .unwrap_or_default()
        .iter()
        .filter_map(zone_vue)
        .collect();

    // Discovered devices grouped by type
    let scanner = &state.scanner;
    let devices = scanner.devices().await;
    let mut devices_by_type: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for d in &devices {
        devices_by_type
            .entry(d.device_type.to_string())
            .or_default()
            .push(d.name.clone());
    }

    // Connectors (streaming services)
    let registry = state.services.lock().await;
    let connectors: Vec<String> = registry.list();
    drop(registry);

    // Audio outputs
    let audio_backend_pref = &state.display_audio_backend();
    let (audio_outputs, audio_backend_name, asio_avail, audio_backend_status) = {
        #[cfg(feature = "local-audio")]
        {
            let devs: Vec<String> =
                tune_core::outputs::local::list_audio_devices_with_backend(audio_backend_pref)
                    .iter()
                    .map(|d| d.name.clone())
                    .collect();
            let name = tune_core::outputs::local::active_backend_name(audio_backend_pref);
            let asio = tune_core::outputs::local::asio_available();
            // #1395 — le rapport de diagnostic est ce que le testeur colle sur
            // le forum. Il portait le backend ACTIF sans jamais dire lequel
            // avait été DEMANDÉ ni pourquoi il n'avait pas été honoré : c'est
            // une capture de journal qu'il a fallu réclamer à Bilou pour
            // apprendre que son pilote ASIO n'exposait aucune sortie.
            let status = serde_json::to_value(tune_core::outputs::local::active_backend_status(
                audio_backend_pref,
            ))
            .unwrap_or(serde_json::Value::Null);
            (devs, name, asio, status)
        }
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = audio_backend_pref;
            (Vec::<String>::new(), "none", false, serde_json::Value::Null)
        }
    };

    // Scan status
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let scan_result: Option<serde_json::Value> = settings
        .get("scan_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());

    // Memory RSS
    let rss_mb = get_rss_mb();

    // #3205 — le seul chiffre qui dise si l'audio a réellement sauté.
    let ring_starvation = releve_famine_anneau(&state).await;
    // #3479 — ce que l'étage d'égalisation a réellement produit.
    let dsp_egaliseur = releve_dsp_egaliseur(&state).await;

    // DB backend — #3182.
    //
    // Il se lisait dans un réglage `settings.db_engine` que RIEN n'écrit :
    // aucun `set("db_engine", …)` n'existe dans l'arbre (le seul autre point
    // qui porte ce nom, `routes/system/config.rs`, le CALCULE déjà depuis le
    // backend). La seule branche jamais empruntée était donc le
    // `unwrap_or("sqlite")`, et `db_backend` — recopié dans `db.engine` plus
    // bas — annonçait « sqlite » sur toute installation PostgreSQL.
    let db_backend = state.backend.engine().as_str();
    // Snapshot only: idle failures do not become a new alarm or stop policy.
    let zone_poller_metrics = state.poller_metrics.lock().await.clone();

    Json(json!({
        "server_version": tune_core::version(),
        "rust_version": tune_core::rustc_version(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        // #2117 : `uptime_seconds` mesure BIEN ce processus — il naît d'un
        // `Instant` posé au démarrage — mais un compteur relatif ne permet pas
        // de VÉRIFIER que le processus interrogé est le même qu'à l'appel
        // précédent : il faut le déduire, et la déduction a déjà fait écarter
        // à tort l'hypothèse d'un redémarrage pendant un diagnostic. L'ancrage
        // absolu ci-dessous répond sans déduction : il change au redémarrage.
        "uptime_seconds": uptime_secs,
        "process_started_at": state.process_started_at_rfc3339(),
        "memory_rss_mb": rss_mb,
        "db_backend": db_backend,
        "active_zones": zone_count,
        "zone_poller_metrics": zone_poller_metrics,
        // DUP-1 (phase 0) : les zones qui désignent probablement le même appareil,
        // nommées avec leur raison. Le rapport ne fusionne rien : sur .18 le 05/09,
        // un Sonos, un Mac et un Eversolo avaient chacun deux zones.
        "zones_doublons": doublons_de_zones(&zones_vues, &devices),
        // #2154 — une base incomplète ne doit plus pouvoir ignorer des
        // réglages pendant des mois sans laisser de trace dans le rapport.
        "zone_settings_ignored": tune_core::db::zone_repo::zone_settings_ignored(),
        "discovered_devices": devices_by_type,
        "connectors": connectors,
        "audio_outputs_available": audio_outputs,
        "audio_backend": audio_backend_name,
        // #1395 — `audio_backend` dit ce qui TOURNE ; il ne disait pas ce qui
        // avait été DEMANDÉ, ni pourquoi les deux diffèrent. `null` sans
        // sortie locale compilée.
        "audio_backend_status": audio_backend_status,
        "asio_available": asio_avail,
        // #3205 — famine de l'anneau par sortie : `ring_starvation_events`
        // compte les rappels comblés par des zéros, `..._missing_samples`
        // dit combien d'échantillons ont manqué (un micro-trou et une
        // coupure d'une seconde ne se ressemblent pas), et `served_samples`
        // / `stream_ms` donnent le dénominateur qui rend le taux calculable.
        // À NE PAS confondre avec l'underrun ALSA : voir `releve_famine_anneau`.
        "ring_starvation": ring_starvation,
        // #3479 — `eq_non_finite_samples` > 0 dit que l'étage d'égalisation a
        // remis des échantillons à ZÉRO : c'est du silence produit par l'EQ
        // lui-même, sur un anneau qui n'a pas eu faim. `eq_overs` dit
        // l'inverse, la saturation. Les deux étaient mesurés et invisibles.
        "dsp_egaliseur": dsp_egaliseur,
        // #2218 (T9 suite) — ce que chaque étage a ÉCRÊTÉ depuis le démarrage,
        // compté là où le clamp a lieu (ReplayGain, égaliseur, mixeur), sans
        // le changer. Par processus, pas par zone : ces étages ne connaissent
        // pas leur zone. Le journal porte `dsp_ecretage` au premier écrêtage
        // d'une piste et à sa fin.
        "dsp_ecretage": tune_core::audio::ecretage::releve(),
        // #2201 — le garde anti-crash ASIO ne doit plus vivre uniquement dans
        // une ligne WARN que l'utilisateur ne verra jamais.
        "asio_warm_scan": crate::startup::asio_warm_status(),
        // #2392 : pourquoi un fournisseur de sortie hors-arbre est inerte.
        // Absent de la liste = non compilé ; présent avec un `refusal` = droit
        // manquant, et le refus dit lequel et quoi faire ; présent sans refus
        // et `devices: 0` = il cherche et ne trouve rien. Ces trois cas
        // donnaient jusqu'ici le même écran vide.
        "output_providers": crate::discovery_setup::provider_status_snapshot(),
        "scan_status": {
            "status": scan_status,
            "tracks": tracks,
            "albums": albums,
            "last_result": scan_result,
        },
        // CLD-3 — les portées cloud retenues par un 429, avec leur échéance :
        // sans cela un enrichissement qui n'arrive pas ressemble à une panne.
        "cloud_rate_limits": rapport_des_reports_cloud(
            &tune_core::cloud::rate_limit::active_all(&settings),
            maintenant_epoch(),
        ),
        "features": tune_core::enabled_features(),
        // Legacy fields kept for backward compatibility
        "engine": "rust",
        "platform": std::env::consts::OS,
        "pid": std::process::id(),
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "db": {
            "engine": db_backend,
            "migration_version": db_version,
        },
        "music_dirs": music_dirs,
        "tracks_count": tracks,
        "albums_count": albums,
        "artists_count": artists,
        "rust_engines": {
            "available": true,
            "version": tune_core::version(),
            "metadata_engine": "lofty",
            "discovery_engine": "mdns-sd + socket2",
            "scanner_engine": "walkdir + rayon",
            // #3182 : le PILOTE, pas une constante. `rusqlite` n'est même pas
            // lié au processus quand le serveur tourne sur PostgreSQL.
            "db_engine": match state.backend.engine() {
                tune_core::db::engine::Engine::Sqlite => "rusqlite",
                tune_core::db::engine::Engine::Postgres => "sqlx",
            },
        },
    }))
}

/// Read process RSS in megabytes. Returns None on unsupported platforms.
fn get_rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * 4096 / 1024 / 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id();
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8(o.stdout)
                    .ok()?
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .map(|kb| kb / 1024)
            })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None::<u64>
    }
}

/// La section « fournisseurs de sortie » d'un rapport de bogue.
///
/// Vide quand le binaire n'embarque aucun fournisseur hors-arbre : il n'y a
/// alors rien à dire, et une section vide dans chaque rapport serait du bruit.
fn section_fournisseurs_de_sortie(instantane: &Value) -> String {
    let Some(fournisseurs) = instantane["providers"].as_array().filter(|l| !l.is_empty()) else {
        return String::new();
    };

    let mut md = String::from("## Output Providers\n");
    if !instantane["account_linked"].as_bool().unwrap_or(true) {
        md.push_str(
            "- ⚠ **No linked Mozaiklabs account** — paid module entitlements travel with the \
             account, never with the license key, so no paid output module can be active.\n",
        );
    }
    let modules = instantane["licensed_modules"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    md.push_str(&format!(
        "- Licensed modules: {}\n",
        if modules.is_empty() { "none" } else { &modules }
    ));

    for f in fournisseurs {
        let nom = f["provider"].as_str().unwrap_or("?");
        let appareils = f["devices"].as_u64().unwrap_or(0);
        match f["refusal"]["code"].as_str() {
            Some(code) => md.push_str(&format!(
                "- {nom}: **idle — {code}** ({})\n",
                f["refusal"]["message"].as_str().unwrap_or("")
            )),
            None => md.push_str(&format!("- {nom}: active, {appareils} device(s)\n")),
        }
    }
    md.push('\n');
    md
}

pub(super) async fn diagnostics_bundle(State(state): State<AppState>) -> Json<Value> {
    diagnostics(State(state)).await
}

pub(super) async fn diagnostics_network(State(state): State<AppState>) -> Json<Value> {
    let scanner = &state.scanner;
    let devices = scanner.devices().await;
    let outputs = state.outputs.lock().await;
    let output_count = outputs.list().len();
    Json(json!({
        "discovered_devices": devices.len(),
        "registered_outputs": output_count,
        // L'etat du canal TCP de SlimProto (port 3483). Sans ce champ, un bind
        // refuse ne vivait que dans une ligne de journal, dans une tache
        // detachee : le testeur n'avait AUCUN moyen de savoir que ses platines
        // Squeezebox ne pourraient jamais se connecter (#2938). `null` tant
        // qu'aucune tentative d'ecoute n'a eu lieu.
        "slimproto": tune_core::slimproto::etat_ecoute(),
        "lms_cli": tune_core::slimproto::cli_server::etat_ecoute(),
        "slimproto_udp": tune_core::slimproto::discovery::etat_ecoute(),
        // L'etat de l'ecouteur SSDP (port 1900) et le nombre de reponses
        // M-SEARCH emises. Sans ce champ, « Tune repond-il aux M-SEARCH ? » ne
        // se mesurait qu'au tcpdump, chez le testeur — c'est exactement ce
        // qu'a du faire celui de #3687. `null` tant qu'aucune liaison n'a ete
        // tentee.
        "ssdp": tune_core::discovery::ssdp::etat_ecoute_ssdp(),
        "devices": devices.iter().map(|d| json!({
            "id": d.id,
            "name": d.name,
            "host": d.host,
            "type": format!("{:?}", d.device_type),
        })).collect::<Vec<_>>(),
    }))
}

pub(super) async fn diagnostics_oaat(State(state): State<AppState>) -> Json<Value> {
    let outputs = state.outputs.lock().await;
    let mut endpoints = Vec::new();
    for id in outputs.list() {
        if let Some(output) = outputs.get(&id) {
            let output = output.lock().await;
            if let Some(diag) = output.diagnostics_json() {
                endpoints.push(diag);
            }
        }
    }
    Json(json!({
        "oaat_endpoints": endpoints,
        "count": endpoints.len(),
    }))
}

pub(super) async fn health_monitor(State(state): State<AppState>) -> Json<Value> {
    let report = state.health_monitor.run_checks().await;
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    Json(json!({
        "status": report.status,
        "uptime_seconds": report.uptime_seconds,
        "tracks": tracks,
        "scan_status": scan_status,
        "engine": "rust",
        "checks": report.checks,
        "alerts": report.alerts,
    }))
}

pub(super) async fn health_alerts(State(state): State<AppState>) -> Json<Value> {
    let alerts = state.health_monitor.alerts().await;
    Json(json!(alerts))
}

#[derive(Deserialize)]
pub(super) struct LogsQuery {
    lines: Option<usize>,
}

/// Bounded tail window for `/system/logs`, kept bounded regardless of how
/// large the append-only log has grown (rotation only runs at startup, so a
/// long-running server's file can reach hundreds of MB).
///
/// 8 MiB et non 2 : depuis #1974 on lit `CANDIDATE_FACTOR` fois plus de lignes
/// que demandé pour pouvoir SÉLECTIONNER au lieu de tronquer. 8 000 lignes de
/// journal pèsent environ 1,6 Mo — 2 Mo passait tout juste, et « tout juste »
/// se transforme en fenêtre amputée le jour où les messages s'allongent, sans
/// que rien ne le dise.
const LOG_TAIL_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug)]
enum LogTailError {
    /// No file at the path — fall through to journalctl/syslog fallbacks.
    Missing,
    /// The file exists but reading it failed — surfaced as such instead of
    /// the misleading "No log file found".
    Unreadable(String),
}

fn read_log_tail(
    log_path: &str,
    max_lines: usize,
    tail_bytes: u64,
) -> Result<Vec<String>, LogTailError> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LogTailError::Missing),
        Err(e) => return Err(LogTailError::Unreadable(e.to_string())),
    };
    let unreadable = |e: std::io::Error| LogTailError::Unreadable(e.to_string());
    let len = f.metadata().map_err(unreadable)?.len();
    let start = len.saturating_sub(tail_bytes);
    f.seek(SeekFrom::Start(start)).map_err(unreadable)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(unreadable)?;

    let text = String::from_utf8_lossy(&buf);
    // If we started mid-file the first line is likely truncated — drop it.
    let body = if start > 0 {
        text.find('\n').map(|nl| &text[nl + 1..]).unwrap_or("")
    } else {
        &text
    };

    let lines: Vec<&str> = body.lines().rev().take(max_lines).collect();
    Ok(lines.into_iter().rev().map(str::to_string).collect())
}

/// Combien de lignes brutes lire avant d'en selectionner `max_lines`.
///
/// Rebalancer suppose d'avoir le choix : prendre exactement les 1000
/// dernieres lignes, c'est deja avoir subi la troncature qu'on veut corriger.
/// On lit donc large, puis on selectionne.
const CANDIDATE_FACTOR: usize = 8;

/// Part maximale d'une fenetre d'export qu'un seul sous-systeme peut occuper.
///
/// Les sous-systemes n'ecrivent pas au meme rythme : `discovery::ssdp` ecrit
/// toutes les quelques secondes (annonces reseau, re-enregistrements),
/// `audio::embedding` une ligne par lot — une toutes les quinze minutes. Sur
/// une fenetre a plafond simple, le premier chasse mecaniquement le second.
///
/// Autrement dit : **plus un traitement est lent, donc plus il est suspect,
/// moins il a de chances d'apparaitre dans l'export.** L'outil de diagnostic
/// est aveugle exactement la ou on en a besoin.
///
/// Mesure sur deux exports de Bilou (#1974) : 529 et 562 lignes de SSDP sur
/// 1003. Le second ne contenait AUCUNE ligne d'embedding, alors que l'analyse
/// acoustique etait l'objet du signalement. Il avait fourni le bon fichier, au
/// bon moment, et il etait inexploitable.
const QUOTA_PAR_MODULE: f64 = 0.25;

/// Part maximale du quota d'un module qu'un SEUL evenement peut occuper en
/// premiere main.
///
/// [`QUOTA_PAR_MODULE`] a resolu le probleme d'un module qui chasse les
/// autres. Il reste ENTIER un cran plus bas : a l'interieur d'un module, le
/// quota se depense sur les lignes les plus RECENTES, donc sur la derniere
/// rafale — et une rafale, par definition, repete le meme evenement.
///
/// Mesure sur le rapport de Reivax66 (ticket support 78, #3580, fenetre de
/// 200 lignes couvrant 10:21 -> 10:42) : `tune_core::orchestrator` a exactement
/// atteint son quota (50 lignes retenues, **60 ecartees**), et sur ces 50,
/// **39 sont trois evenements repetes** — 13 `radio_local_decode_stream_connected`
/// et 13 `radio_local_decode_started` tires d'une rafale de 200 ms, plus 13
/// `orchestrator_play_retap_deduped_same_inflight_track` d'une autre. Les 60
/// ecartees sont les plus ANCIENNES : celles des trois cycles de lecture qui
/// ont echoue. Le rapport a donc jete la chaine de decision (`output_play_failed`,
/// `initial_prebuffer_done`, `radio_proxy_transcode_for_dlna`) pour garder une
/// rafale, et le dossier est reste inexploitable.
///
/// **Un huitieme** : sur une fenetre de 200, un evenement ne prend plus que 6
/// des 50 lignes de son module en premiere main. Les trois rafales rendent 21
/// places, et le budget du module couvre l'incident au lieu d'une seconde.
///
/// Ce n'est PAS un second mecanisme : c'est le meme, un cran plus bas, avec la
/// meme reprise juste apres — voir `selectionner_lignes`, ou une ligne differee
/// par ce quota-ci reprend la place que son module n'a pas depensee. Un module
/// qui n'atteint pas son quota ne perd donc AUCUNE ligne, rafale comprise.
const QUOTA_PAR_EVENEMENT: f64 = 0.125;

/// Le module (`target` de tracing) d'une ligne de log, si elle en porte un.
///
/// Format du writer (`fmt::layer()` par defaut, `bootstrap.rs`) :
/// `<horodatage>  INFO tune_core::discovery::ssdp: message`.
///
/// Rend `None` pour tout ce qui ne suit pas cette forme — continuation d'un
/// message multiligne, trace de panique, sortie d'un tiers. Ces lignes-la ne
/// sont JAMAIS ecartees : une ligne qu'on ne sait pas classer est une ligne
/// dont on ne sait pas si elle compte.
fn module_de_la_ligne(ligne: &str) -> Option<&str> {
    const NIVEAUX: [&str; 5] = [" ERROR ", " WARN ", " INFO ", " DEBUG ", " TRACE "];
    let (_, apres) = NIVEAUX
        .iter()
        .find_map(|n| ligne.split_once(n).map(|p| (n, p)))?;
    let cible = apres.1.split_whitespace().next()?;
    let cible = cible.strip_suffix(':')?;
    // `tune_core::discovery::ssdp` — un module, pas un mot isole comme le
    // debut d'une phrase. Sans cette exigence, un message qui commence par
    // « erreur: » se ferait compter comme un module a lui tout seul.
    if cible.is_empty() || !cible.contains("::") {
        return None;
    }
    Some(cible)
}

/// L'EVENEMENT d'une ligne : le premier mot du message, celui que `tracing`
/// ecrit juste apres le module.
///
/// `… INFO tune_core::orchestrator: initial_prebuffer_done zone_id=5 …`
/// rend `Some("initial_prebuffer_done")`.
///
/// Un nom d'evenement de ce depot est un identifiant Rust en minuscules, et il
/// porte au moins un `_`. L'exigence n'est pas cosmetique : sans elle, un
/// message redige en phrase — « Le renderer a acquitte Play… » — se ferait
/// compter comme un evenement a lui tout seul et rationner a ce titre. Une
/// ligne dont on ne sait pas nommer l'evenement rend `None` et ne repond alors
/// que du quota de son module, exactement comme avant.
fn evenement_de_la_ligne(ligne: &str) -> Option<&str> {
    const NIVEAUX: [&str; 5] = [" ERROR ", " WARN ", " INFO ", " DEBUG ", " TRACE "];
    let (_, apres) = NIVEAUX.iter().find_map(|n| ligne.split_once(n))?;
    let mut mots = apres.split_whitespace();
    // Le module, deja valide par `module_de_la_ligne` : on le saute.
    let _cible = mots.next()?.strip_suffix(':')?;
    let evenement = mots.next()?;
    let bien_forme = evenement.len() >= 3
        && evenement.contains('_')
        && evenement
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    bien_forme.then_some(evenement)
}

/// Choisit `max_lines` lignes parmi `candidates`, en empechant un seul module
/// d'occuper plus de [`QUOTA_PAR_MODULE`] de la fenetre, ni un seul evenement
/// plus de [`QUOTA_PAR_EVENEMENT`] du quota de son module.
///
/// Deux passes, et la seconde est ce qui rend la premiere sans risque :
///
/// 1. du plus recent au plus ancien, on garde chaque ligne dont le module n'a
///    pas epuise son quota ;
/// 2. si la fenetre n'est pas pleine — parce que les quotas ont beaucoup
///    ecarte — on la complete avec les lignes mises de cote, toujours du plus
///    recent au plus ancien.
///
/// La seconde passe garantit qu'on ne rend JAMAIS moins de lignes que la
/// troncature simple : a taille egale, l'export dit strictement plus. Sur une
/// machine ou seul SSDP parle, il reste donc integralement.
///
/// Rend les lignes dans l'ordre chronologique, et le decompte par module de ce
/// qui a ete ecarte — un export qui tait ce qu'il a laisse tomber se lit comme
/// s'il avait tout montre.
fn selectionner_lignes(
    candidates: Vec<String>,
    max_lines: usize,
) -> (Vec<String>, std::collections::BTreeMap<String, usize>) {
    use std::collections::BTreeMap;

    if max_lines == 0 {
        return (Vec::new(), BTreeMap::new());
    }
    if candidates.len() <= max_lines {
        return (candidates, BTreeMap::new());
    }

    let quota = ((max_lines as f64 * QUOTA_PAR_MODULE).floor() as usize).max(1);
    let quota_evenement = ((quota as f64 * QUOTA_PAR_EVENEMENT).floor() as usize).max(1);
    let mut comptes: BTreeMap<String, usize> = BTreeMap::new();
    let mut comptes_evenement: BTreeMap<String, usize> = BTreeMap::new();
    // `Option<String>` et non l'indice : on garde la ligne retenue et, pour
    // celles mises de cote, de quoi les reprendre en seconde passe.
    let mut retenues: Vec<usize> = Vec::with_capacity(max_lines);
    // Repoussees par le quota d'EVENEMENT (#3580). Elles ne sont pas perdues :
    // elles repassent juste apres, sur le budget non depense de leur module.
    let mut differees: Vec<usize> = Vec::new();
    let mut ecartees: Vec<usize> = Vec::new();

    for (i, ligne) in candidates.iter().enumerate().rev() {
        if retenues.len() >= max_lines {
            break;
        }
        match module_de_la_ligne(ligne) {
            Some(m) => {
                let n = comptes.entry(m.to_string()).or_insert(0);
                if *n >= quota {
                    ecartees.push(i);
                    continue;
                }
                // #3580 — le quota d'un module se depensait sur sa derniere
                // RAFALE, qui repete le meme evenement. Une premiere main
                // plafonnee par evenement fait couvrir l'incident au meme
                // budget ; ce qui deborde repasse juste apres.
                if let Some(e) = evenement_de_la_ligne(ligne) {
                    let ne = comptes_evenement.entry(format!("{m}::{e}")).or_insert(0);
                    if *ne >= quota_evenement {
                        differees.push(i);
                        continue;
                    }
                    *ne += 1;
                }
                *n += 1;
                retenues.push(i);
            }
            // Non classable : jamais ecartee.
            None => retenues.push(i),
        }
    }

    // Reprise des differees. Le quota par evenement ne RETIRE rien a un
    // module : il choisit seulement lesquelles de ses lignes il garde en
    // premier. Un module qui n'a pas epuise son quota reprend donc ici toute
    // sa rafale — c'est ce qui rend ce cran supplementaire sans risque, et
    // c'est ce que verifie `aucun_module_ne_perd_de_ligne_par_le_quota_evenement`.
    for i in std::mem::take(&mut differees) {
        if retenues.len() >= max_lines {
            break;
        }
        match module_de_la_ligne(&candidates[i]) {
            Some(m) => {
                let n = comptes.entry(m.to_string()).or_insert(0);
                if *n < quota {
                    *n += 1;
                    retenues.push(i);
                } else {
                    ecartees.push(i);
                }
            }
            None => retenues.push(i),
        }
    }

    // Seconde passe : completer avec ce qu'on avait mis de cote.
    for i in ecartees.iter().copied() {
        if retenues.len() >= max_lines {
            break;
        }
        retenues.push(i);
    }

    retenues.sort_unstable();

    // Ce qu'on RAPPORTE comme mis de cote, et le calcul n'est pas celui qu'on
    // ecrit d'abord.
    //
    // Compter toutes les lignes non retenues serait faux, et faussement
    // alarmant : sur 3000 lignes lues pour une fenetre de 1000, la troncature
    // simple en jetait deja 2000 sans jamais le dire. Les annoncer ici ferait
    // passer pour une perte ce qui est le fonctionnement normal d'une fenetre.
    //
    // Le seul chiffre honnete est le DEPLACEMENT : les lignes qui auraient
    // figure dans la fenetre d'avant — les `max_lines` dernieres — et qu'on a
    // ecartees au profit d'autres. C'est exactement ce que le quota a coute, ni
    // plus ni moins. Un module bavard seul en scene n'y apparait donc pas : il
    // n'a rien cede a personne.
    //
    // Ce calcul est le second : le premier comptait tout, et le test de
    // non-regression `un_seul_module_bavard_reste_entier` l'a refuse.
    let seuil_ancienne_fenetre = candidates.len().saturating_sub(max_lines);
    let retenu: std::collections::BTreeSet<usize> = retenues.iter().copied().collect();
    let mut vraiment_ecartees: BTreeMap<String, usize> = BTreeMap::new();
    for i in seuil_ancienne_fenetre..candidates.len() {
        if retenu.contains(&i) {
            continue;
        }
        if let Some(m) = module_de_la_ligne(&candidates[i]) {
            *vraiment_ecartees.entry(m.to_string()).or_insert(0) += 1;
        }
    }

    let lignes = retenues
        .into_iter()
        .map(|i| candidates[i].clone())
        .collect::<Vec<_>>();
    (lignes, vraiment_ecartees)
}

pub(super) async fn logs(Query(q): Query<LogsQuery>) -> Json<Value> {
    collect_recent_logs(q.lines.unwrap_or(1000)).await
}

#[derive(Deserialize)]
pub(super) struct RegistreQuery {
    /// Filtrer sur une passe. Sans ce parametre, tout le registre.
    task: Option<String>,
    /// Nombre maximum de lignes rendues. Borne a 500 par le registre.
    limit: Option<i64>,
}

/// `GET /system/task-runs` — le registre des executions automatisees (#2080).
///
/// Ce que cette route repond, et que rien ne repondait avant : « la passe
/// a-t-elle tourne, quand, combien de temps, et avec quel resultat ». Le
/// journal defile et se perd ; `/system/background-tasks` ne connait que le
/// PRESENT (les taches en cours, en memoire, perdues au redemarrage). Ici,
/// c'est le PASSE, et il survit au redemarrage.
///
/// `boot_id` distingue les incarnations du processus : deux executions de boots
/// differents ne se confondent pas, et c'est ce qui rend lisible « la passe a
/// ete interrompue par un redemarrage ».
///
/// La reponse ne contient ni chemin, ni cle, ni jeton — des compteurs et des
/// verdicts. Elle peut donc etre collee telle quelle dans un ticket.
pub(super) async fn task_runs(
    State(state): State<AppState>,
    Query(q): Query<RegistreQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let registre = tune_core::db::task_run_repo::TaskRunRepo::with_backend(state.backend.clone());
    let limite = q.limit.unwrap_or(100);

    let runs = registre.lister(q.task.as_deref(), limite).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
    })?;
    let dernieres = registre.resume().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
    })?;

    Ok(Json(json!({
        // L'incarnation COURANTE. Une ligne qui ne porte pas ce boot_id vient
        // d'un demarrage anterieur — c'est la lecture qui evite de prendre une
        // vieille execution pour l'actuelle.
        "boot_id": tune_core::db::task_run_repo::boot_id(),
        // Les passes que le registre sait ecrire aujourd'hui. Une passe de
        // cette liste ABSENTE de `latest` n'a jamais tourne sur cette
        // installation ; sans la liste, on ne saurait pas la distinguer d'une
        // passe qu'on aurait oublie de cabler.
        "wired_tasks": tune_core::db::task_run_repo::TACHES_CABLEES,
        "retention": {
            "runs_per_task": tune_core::db::task_run_repo::RETENTION_EXECUTIONS_PAR_PASSE,
            "days": tune_core::db::task_run_repo::RETENTION_JOURS,
        },
        "latest": dernieres,
        "runs": runs,
    })))
}

/// Collect the most recent server logs (tail): log file first, then
/// journalctl/syslog (Linux) or stderr files / unified log (macOS). Returns a
/// `Json<Value>` with `logs`/`lines`/`source`. Shared by the `/logs` endpoint
/// and the bug report so both surface identical output. Async because the tail
/// read runs on a blocking pool (spawn_blocking) to keep off the Tokio runtime.
pub(super) async fn collect_recent_logs(max_lines: usize) -> Json<Value> {
    // Try the server's own log file first — same path the writer uses (main),
    // resolved via the shared helper so reader and writer always agree. This is
    // what makes "Export logs" work on Linux under Docker / a bare terminal,
    // where journalctl doesn't apply and no file existed before.
    let log_path = crate::config::default_log_file_path()
        .to_string_lossy()
        .into_owned();

    // Read only a bounded tail, off the async runtime. Reading the whole file
    // with read_to_string both blocked a Tokio worker (same trap as
    // admin_errors, #1096) and could fail outright on a low-RAM box once the
    // file had grown large — and that failure fell through to the misleading
    // "No log file found" fallback, exporting an empty log (Yacine, DS418j
    // 1 GB RAM).
    {
        let path = log_path.clone();
        // On lit CANDIDATE_FACTOR fois plus de lignes que demandé, puis on
        // sélectionne : rebalancer suppose d'avoir le choix, et prendre
        // exactement les N dernières lignes c'est déjà avoir subi la troncature
        // qu'on veut corriger (#1974).
        let a_lire = max_lines.saturating_mul(CANDIDATE_FACTOR).max(max_lines);
        let tail =
            tokio::task::spawn_blocking(move || read_log_tail(&path, a_lire, LOG_TAIL_BYTES)).await;
        match tail {
            Ok(Ok(brutes)) => {
                let lues = brutes.len();
                let (lines, ecartees) = selectionner_lignes(brutes, max_lines);
                if !ecartees.is_empty() {
                    tracing::info!(
                        lues,
                        rendues = lines.len(),
                        ecartees = ?ecartees,
                        "log_export_rebalanced"
                    );
                }
                return Json(json!({
                    "logs": lines.join("\n"),
                    "lines": lines.len(),
                    "source": "file",
                    "path": log_path,
                    // Ce qui a été mis de côté, par module. Un export qui tait
                    // ce qu'il a laissé tomber se lit comme s'il avait tout
                    // montré — et c'est exactement ce qui a coûté deux
                    // allers-retours à Bilou.
                    "scanned_lines": lues,
                    "set_aside": ecartees,
                }));
            }
            Ok(Err(LogTailError::Unreadable(e))) => {
                return Json(json!({
                    "logs": format!("Log file exists but could not be read: {e}\nPath: {log_path}"),
                    "lines": 0,
                    "source": "file_unreadable",
                    "path": log_path,
                }));
            }
            // Missing file or a cancelled blocking task: try the fallbacks.
            Ok(Err(LogTailError::Missing)) | Err(_) => {}
        }
    }

    // Try journalctl on Linux (multiple service names)
    #[cfg(target_os = "linux")]
    {
        // `tune` D'ABORD : c'est le nom de l'unité sur Tune OS
        // (`/etc/systemd/system/tune.service`, posé par l'image), et il
        // manquait à cette liste. Conséquence : sur l'appliance que nous
        // distribuons, l'export de journaux ne trouvait JAMAIS rien — ni
        // fichier (le serveur y écrit sur la sortie standard, captée par
        // systemd), ni journalctl (mauvais nom d'unité), ni syslog. Le
        // testeur recevait « No log file found. Launch Tune from a terminal »,
        // conseil absurde sur un boîtier sans écran, et nous joignait un
        // fichier de quatre lignes (Stéphane Villerio, 19/08).
        for service in &["tune", "tune-server", "tune-rust"] {
            if let Ok(output) = std::process::Command::new("journalctl")
                .args([
                    "-u",
                    service,
                    "-n",
                    &max_lines.to_string(),
                    "--no-pager",
                    "-o",
                    "short-iso",
                ])
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    let count = text.lines().count();
                    if count > 1 {
                        return Json(json!({
                            "logs": text,
                            "lines": count,
                            "source": "journalctl",
                            "service": service,
                        }));
                    }
                }
            }
        }
        // Fallback: read from /var/log/syslog
        if let Ok(content) = std::fs::read_to_string("/var/log/syslog") {
            let lines: Vec<&str> = content
                .lines()
                .filter(|l| l.contains("tune-server") || l.contains("tune_"))
                .rev()
                .take(max_lines)
                .collect();
            if !lines.is_empty() {
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                return Json(json!({
                    "logs": lines.join("\n"),
                    "lines": lines.len(),
                    "source": "syslog",
                }));
            }
        }
    }

    // macOS: try stderr log files FIRST (Homebrew launchd captures tracing
    // output here), then fall back to `log show`.  The tracing logs contain
    // the actual application events (auto_next, track_ended, etc.) while
    // `log show` only captures CoreAudio/system noise.
    #[cfg(target_os = "macos")]
    {
        let stderr_paths = [
            format!(
                "{}/Library/Logs/tune-server.log",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
            ),
            "/usr/local/var/log/tune-server.log".into(),
            "/opt/homebrew/var/log/tune-server.log".into(),
        ];
        for p in &stderr_paths {
            if let Ok(content) = std::fs::read_to_string(p) {
                let lines: Vec<&str> = content.lines().rev().take(max_lines).collect();
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                if !lines.is_empty() {
                    return Json(json!({
                        "logs": lines.join("\n"),
                        "lines": lines.len(),
                        "source": "file",
                        "path": p,
                    }));
                }
            }
        }

        // Fallback: macOS unified log — filter to Tune tracing lines only
        if let Ok(output) = std::process::Command::new("log")
            .args([
                "show",
                "--predicate",
                "process == \"tune-server\"",
                "--last",
                "5m",
                "--style",
                "compact",
            ])
            .output()
        {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let lines: Vec<&str> = text
                    .lines()
                    .filter(|l| {
                        l.contains("tune_")
                            || l.contains("INFO")
                            || l.contains("WARN")
                            || l.contains("ERROR")
                    })
                    .collect();
                let lines: Vec<&str> = lines.into_iter().rev().take(max_lines).collect();
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                if !lines.is_empty() {
                    return Json(json!({
                        "logs": lines.join("\n"),
                        "lines": lines.len(),
                        "source": "macos_log",
                    }));
                }
            }
        }
    }

    // Fallback: check stderr capture file (Linux / non-macOS)
    #[cfg(not(target_os = "macos"))]
    {
        let stderr_paths: [String; 3] = [
            format!(
                "{}/Library/Logs/tune-server.log",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
            ),
            "/usr/local/var/log/tune-server.log".into(),
            "/opt/homebrew/var/log/tune-server.log".into(),
        ];
        for p in &stderr_paths {
            if let Ok(content) = std::fs::read_to_string(p) {
                let lines: Vec<&str> = content.lines().rev().take(max_lines).collect();
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                if !lines.is_empty() {
                    return Json(json!({
                        "logs": lines.join("\n"),
                        "lines": lines.len(),
                        "source": "file",
                        "path": p,
                    }));
                }
            }
        }
    }

    // Dire ce qui a été tenté, pas seulement ce qui a échoué.
    //
    // « No log file found » avec un seul chemin laissait croire à un problème
    // de fichier, alors que trois mécanismes distincts ont été essayés. Sans
    // cette liste, ni le testeur ni nous ne pouvons dire lequel a manqué — et
    // c'est nous qui redemandons un journal que sa machine ne sait pas
    // produire.
    #[cfg(target_os = "linux")]
    let tentatives = format!(
        "Chemins et sources essayés :\n  - fichier : {log_path}\n           - journalctl -u tune / tune-server / tune-rust\n  - /var/log/syslog"
    );
    #[cfg(not(target_os = "linux"))]
    let tentatives = format!("Chemins et sources essayés :\n  - fichier : {log_path}");

    Json(json!({
        "logs": format!(
            "Aucun journal accessible. Si Tune tourne en service, la commande \
             ci-dessous le donne en direct :\n  journalctl -u tune -n 2000 --no-pager\n\n{tentatives}"
        ),
        "lines": 0,
        "source": "none",
    }))
}

// --- Log level management ---

#[derive(Deserialize)]
pub(super) struct LogLevelBody {
    level: String,
}

pub(super) async fn get_log_level(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let level = settings
        .get("log_level")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_LOG").ok())
        .unwrap_or_else(|| "info".into());
    Json(json!({
        "level": level,
        "available": ["error", "warn", "info", "debug", "trace"],
    }))
}

pub(super) async fn set_log_level(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<LogLevelBody>,
) -> Json<Value> {
    let valid = ["error", "warn", "info", "debug", "trace"];
    let level = body.level.to_lowercase();
    if !valid.contains(&level.as_str()) {
        return Json(json!({ "error": format!("Invalid level: {}. Use: {:?}", level, valid) }));
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let _ = settings.set("log_level", &level);

    // Also update the TUNE_LOG env var for the current process
    // SAFETY: single-threaded env access at this point
    unsafe {
        std::env::set_var("TUNE_LOG", &level);
    }

    Json(json!({
        "status": "ok",
        "level": level,
        "note": "Log level saved. Full effect after server restart.",
    }))
}

/// Ce qu'un rapport écrit à la place d'une version de schéma illisible.
///
/// Surtout pas `0` : le rapport est lu par un humain qui instruit un ticket, et
/// `0` s'y lit « base jamais migrée ». « Inconnue » et « jamais migrée » sont
/// deux états différents, et #3182 est né de les avoir confondus.
const VERSION_DE_SCHEMA_INCONNUE: &str = "unknown";

/// Rend une version de schéma telle qu'elle sera lue dans le markdown.
///
/// Fonction NUE — elle ne prend pas d'`AppState` — pour qu'une épreuve puisse
/// la sonder sans base ; c'est le rendu qui est éprouvé, pas la condition.
/// Ce que la ligne « Interface (web) » dit quand `web/version.json` n'existe
/// pas. Surtout pas la version du serveur : deux numeros identiques feraient
/// disparaitre l'ecart que #3380 existe pour rendre visible.
const SANS_VERSION_INTERFACE: &str =
    "inconnue (web/version.json absent : build web anterieur a #3380)";

fn version_de_schema_affichee(version: Option<i32>) -> String {
    version.map_or_else(|| VERSION_DE_SCHEMA_INCONNUE.to_string(), |v| v.to_string())
}

/// Une valeur de réglage telle qu'elle doit se LIRE dans le markdown (#2856).
///
/// Une chaîne perd ses guillemets JSON — `resample_policy: none`, pas
/// `resample_policy: "none"` —, tout le reste s'écrit tel quel. Fonction NUE,
/// éprouvable sans base ni `AppState`.
fn valeur_lisible(valeur: &Value) -> String {
    match valeur.as_str() {
        Some(texte) => texte.to_string(),
        None => valeur.to_string(),
    }
}

/// Ce que l'index de recherche couvre RÉELLEMENT, table par table.
///
/// #4319 — Tades, fil 1841 : « quand je regarde répertoire j'ai bien 2 albums
/// Mahler par Mehta, la 2 et la 3 ; quand je fais une recherche je ne trouve
/// que la 2 ». Un album présent en base et absent de la recherche a deux
/// explications très différentes — le mot cherché ne correspond pas, ou la
/// ligne manque à l'index — et le rapport ne permettait de trancher ni l'une
/// ni l'autre : il donnait le nombre d'albums, jamais le nombre d'albums
/// INDEXÉS.
///
/// Rendu en `(table, lignes indexées, lignes en base)`. Chaque compte vaut
/// `None` quand il n'a pas pu être lu — une table FTS absente sur un moteur qui
/// n'en a pas (PostgreSQL indexe par `tsvector`, pas par table miroir) ne doit
/// pas faire mentir le rapport avec un zéro.
///
/// 🔴 #4565 — **les deux chiffres se lisent ici, et nulle part ailleurs.**
///
/// Le dénominateur venait des compteurs de la bibliothèque (`ArtistRepo::count`
/// & co) affichés juste au-dessus dans le rapport. Or `ArtistRepo::count()` ne
/// compte PAS la table `artists` : il compte les artistes **porteurs d'au moins
/// un album** (`WHERE id IN (SELECT DISTINCT artist_id FROM albums …)`), tandis
/// que le déclencheur `artists_fts_insert` indexe **chaque** insertion dans
/// `artists`, sans condition. Les deux ensembles n'étaient pas comparables, et
/// tout serveur portant un seul artiste sans album (piste seule, compilation,
/// featuring — l'ordinaire) affichait un ⚠ permanent sur une ligne saine :
/// `artists 1826/1824 ⚠` chez Jean Valjean en 0.9.158, premier retour de
/// terrain de l'instrumentation posée par #4319 — qui mentait donc dès son
/// premier usage. `albums` et `tracks` tombaient juste parce que LEURS
/// `count()` sont, eux, non filtrés.
///
/// Le dénominateur est désormais `SELECT COUNT(*) FROM {table}` : la ligne
/// mesure l'INDEX — combien de lignes de la table source lui manquent — ce qui
/// est exactement la question que #4319 sert à trancher (« le mot cherché ne
/// correspond pas » vs « la ligne manque à l'index »). Le titre de la ligne dit
/// ce qui est comparé, pour qu'on ne la confonde plus avec le compteur
/// `Artists:` du dessus, qui répond à une autre question.
fn couverture_de_l_index(state: &AppState) -> Vec<(&'static str, Option<i64>, Option<i64>)> {
    if state.backend.engine() != tune_core::db::engine::Engine::Sqlite {
        // PostgreSQL : l'index vit dans une COLONNE `search_tsv`, il n'y a pas
        // de table miroir à compter. On ne rend rien plutôt qu'un chiffre qui
        // ne voudrait rien dire.
        return Vec::new();
    }
    let compte = |sql: String| -> Option<i64> {
        state
            .backend
            .query_one(&sql, &[])
            .ok()
            .flatten()
            .and_then(|c| c.first().and_then(|v| v.as_i64()))
    };
    ["albums", "tracks", "artists"]
        .into_iter()
        .map(|table| {
            let indexees = compte(format!("SELECT COUNT(*) FROM {table}_fts"));
            let en_base = compte(format!("SELECT COUNT(*) FROM {table}"));
            (table, indexees, en_base)
        })
        .collect()
}

/// Ce que la grille Bibliothèque › Artistes peut afficher comme PORTRAITS
/// (#4845, fil 1903 — « toutes les vignettes en initiales »).
///
/// La grille rend un portrait SEULEMENT si `artists.image_path` est posé ET
/// que `/library/artwork/{condensat}` trouve le fichier en cache ; sinon les
/// initiales (`ArtistesV2.svelte` → `AlbumArt`). Aucun repli sur une
/// pochette d'album, aucune condition de licence. Trois causes donnent donc
/// le même écran et ne se distinguaient pas dans un rapport :
///
/// * `sans_image` — jamais enrichi (enrichissement coupé, `enrich_on_scan`
///   faux) ou ligne recréée à vide (bibliothèque vidée, artiste orphelin
///   purgé puis recréé) ;
/// * `cache_perdu` — la base annonce une image que le cache n'a plus
///   (cache effacé, `%LOCALAPPDATA%` d'un autre compte) ;
/// * `distantes` — une URL servie par le mandataire, qui peut échouer.
///
/// Le périmètre est celui de la grille : les artistes porteurs d'un album.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PortraitsDArtistes {
    pub total: i64,
    pub affichables: i64,
    pub sans_image: i64,
    pub cache_perdu: i64,
    pub distantes: i64,
    /// `image_source` des images posées (`community`, `auto`, `upload`…).
    pub sources: std::collections::BTreeMap<String, i64>,
}

pub(crate) fn releve_portraits_d_artistes(
    backend: &dyn tune_core::db::backend::DbBackend,
    cache_dir: &std::path::Path,
) -> Option<PortraitsDArtistes> {
    let lignes = backend
        .query_many(
            "SELECT image_path, image_source FROM artists \
             WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)",
            &[],
        )
        .ok()?;
    let mut r = PortraitsDArtistes::default();
    for cols in lignes {
        r.total += 1;
        let chemin = cols
            .first()
            .and_then(|v| v.as_string())
            .filter(|p| !p.trim().is_empty());
        let Some(chemin) = chemin else {
            r.sans_image += 1;
            continue;
        };
        let source = cols
            .get(1)
            .and_then(|v| v.as_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "?".into());
        *r.sources.entry(source).or_insert(0) += 1;
        if chemin.starts_with("http") {
            r.distantes += 1;
            continue;
        }
        // Même résolution que `artist_image` et que la grille : un condensat
        // hexadécimal tel quel, un chemin par son `artwork_hash`.
        let hex = (chemin.len() == 32 || chemin.len() == 64)
            && chemin.chars().all(|c| c.is_ascii_hexdigit());
        let adresse = if hex {
            chemin
        } else {
            tune_core::library::artwork::artwork_hash(&chemin)
        };
        if tune_core::library::artwork::find_cached(cache_dir, &adresse).is_some() {
            r.affichables += 1;
        } else {
            r.cache_perdu += 1;
        }
    }
    Some(r)
}

/// La ligne du rapport de bogue — celle que le testeur colle sur le forum.
pub(crate) fn ligne_portraits_d_artistes(p: &PortraitsDArtistes) -> String {
    let sources = if p.sources.is_empty() {
        String::new()
    } else {
        let s: Vec<String> = p.sources.iter().map(|(k, n)| format!("{k} {n}")).collect();
        format!(" ; sources : {}", s.join(", "))
    };
    format!(
        "- Portraits d'artistes (grille Artistes) : {}/{} affichables — sans image {}, cache perdu {}, URL distante {}{}\n",
        p.affichables, p.total, p.sans_image, p.cache_perdu, p.distantes, sources
    )
}

/// Combien d'albums la bibliothèque MASQUE — la seule cause de #4319 qu'un
/// rapport tranche SANS retour du testeur.
///
/// #4319 — Tades voit deux albums Mahler/Mehta dans **Répertoires**, la
/// recherche n'en rend qu'un. L'instruction du ticket a réduit le champ à
/// quatre mécanismes, tous côté données. L'un d'eux, l'album **masqué**,
/// produit exactement cette signature :
///
/// - [`hidden_albums_excluded`] retire l'album de la bibliothèque **et** de la
///   recherche — c'est un `NOT EXISTS` sur `hidden_items`, appliqué dans
///   `AlbumRepo` à la liste comme à `search_page` ;
/// - `browse_directory` (`routes/library/browse.rs`) liste les sous-dossiers
///   depuis le **disque** (`std::fs::read_dir`) et n'applique aucun de ces
///   filtres : le dossier reste visible dans Répertoires.
///
/// « Je le vois dans Répertoires, la recherche ne le trouve pas » est donc la
/// description littérale d'un album masqué — et rien, dans le rapport que le
/// testeur colle sur le forum, ne le disait. La ligne posée par #4428 mesure
/// l'index ; elle ne voit pas ce filtre, qui s'applique **après** lui.
///
/// Contrairement à [`couverture_de_l_index`], cette mesure vaut sur les **deux**
/// moteurs : `hidden_items` est créée en SQLite (`init_schema`, migration 89)
/// comme en PostgreSQL (`pg_migrate`). Il n'y a donc pas de garde par moteur.
///
/// `None` quand le compte n'a pas pu être lu — une base trop ancienne pour
/// porter la table ne doit pas afficher un `0` qui affirmerait, à tort, que
/// rien n'est masqué. Même règle que pour la couverture de l'index : on ne
/// remplace jamais une absence de mesure par un zéro mesuré.
fn albums_masques(state: &AppState) -> Option<i64> {
    state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM hidden_items WHERE item_type = 'album'",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
}

/// La ligne du rapport, telle qu'elle se LIT — fonction NUE, éprouvable sans
/// base ni `AppState` (même parti que [`valeur_lisible`]).
///
/// Le libellé nomme la conséquence, pas seulement le nombre : un testeur qui
/// colle son rapport doit pouvoir faire le rapprochement avec ce qu'il voit à
/// l'écran, sans connaître le schéma.
fn ligne_albums_masques(compte: Option<i64>) -> String {
    match compte {
        Some(n) => format!(
            "- Albums masqués : {n} (exclus de la bibliothèque ET de la recherche, \
             toujours visibles dans Répertoires)\n"
        ),
        None => "- Albums masqués : illisible\n".to_string(),
    }
}

/// Generate a bug report with comprehensive diagnostic data.
/// Returns JSON that can also be rendered as markdown by the client.
pub(super) async fn generate_bug_report(State(state): State<AppState>) -> Json<Value> {
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let uptime_secs = state.started_at.elapsed().as_secs();
    // #3182 — voir `super::version_de_schema`. Ce rapport est ce que le
    // testeur COLLE sur le forum : « Migration version: 0 » y était lu comme
    // une base jamais migrée.
    let db_version = super::version_de_schema(&state);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // #3380 — la version de l'INTERFACE. `web/` est deploye separement du
    // binaire : sans elle, un bogue d'ecran s'instruit sans savoir quel ecran
    // tournait. `None` quand `web/version.json` n'existe pas — JAMAIS un repli
    // sur la version du serveur, qui rendrait l'ecart invisible.
    let version_interface =
        tune_core::interface_web::version_interface(&crate::config::resolve_web_dir());
    let music_dirs = super::get_music_dirs_list(&state.backend);
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());

    // Zones
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone_count = zone_repo.count().unwrap_or(0);
    let zone_settings_ignored = tune_core::db::zone_repo::zone_settings_ignored();
    let asio_warm_scan = crate::startup::asio_warm_status();
    let zones: Vec<Value> = zone_repo
        .list()
        .unwrap_or_default()
        .iter()
        .map(|z| json!({ "id": z.id, "name": z.name, "output_type": z.output_type }))
        .collect();

    // Streaming services status
    let registry = state.services.lock().await;
    let service_status = registry.status_all().await;
    drop(registry);

    // Discovered devices
    let scanner = &state.scanner;
    let devices = scanner.devices().await;
    let outputs = state.outputs.lock().await;
    let output_count = outputs.list().len();
    drop(outputs);
    // Le registre des serveurs multimedia, lu ici et rendu dans la section
    // « Network » plus bas. Trie par nom : deux rapports du meme testeur
    // doivent se comparer ligne a ligne.
    let mut serveurs_multimedia: Vec<(String, String, u16, bool, u64)> = {
        let registre = state.media_servers.lock().await;
        registre
            .values()
            .map(|ms| {
                (
                    ms.name.clone(),
                    ms.host.clone(),
                    ms.port,
                    ms.is_reachable(),
                    ms.age().as_secs(),
                )
            })
            .collect()
    };
    serveurs_multimedia.sort();

    let uptime_str = format!(
        "{}d {}h {}m {}s",
        uptime_secs / 86400,
        (uptime_secs % 86400) / 3600,
        (uptime_secs % 3600) / 60,
        uptime_secs % 60,
    );

    // Memory RSS
    let rss_mb = {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map(|pages| pages * 4096 / 1024 / 1024)
        }
        #[cfg(target_os = "macos")]
        {
            let pid = std::process::id();
            std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()?
                        .trim()
                        .parse::<u64>()
                        .ok()
                        .map(|kb| kb / 1024)
                })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None::<u64>
        }
    };

    // OAAT diagnostics
    let oaat_endpoints: Vec<Value> = {
        let outputs = state.outputs.lock().await;
        outputs
            .list()
            .iter()
            .filter_map(|id| {
                let output = outputs.get(id)?;
                let output = output.try_lock().ok()?;
                output.diagnostics_json()
            })
            .collect()
    };

    // Build markdown text
    let ring_starvation = releve_famine_anneau(&state).await;
    let dsp_egaliseur = releve_dsp_egaliseur(&state).await;
    let mut md = String::new();
    md.push_str("# Tune Bug Report\n\n");
    md.push_str(&format!(
        "**Version**: {} (engine: rust)\n",
        tune_core::version()
    ));
    // #3380 : juste sous la version du serveur, parce que c'est la paire qui
    // se lit — deux numeros qui divergent expliquent a eux seuls un ticket.
    md.push_str(&format!(
        "**Interface (web)**: {}\n",
        version_interface
            .as_deref()
            .unwrap_or(SANS_VERSION_INTERFACE)
    ));
    md.push_str(&format!(
        "**Platform**: {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    md.push_str(&format!("**Uptime**: {uptime_str}\n"));
    // #2117 : un rapport de bogue est lu bien après avoir été produit, souvent
    // à côté d'un journal horodaté. « 1h19 » ne se recoupe avec rien ; une date
    // de démarrage se recoupe avec tout.
    md.push_str(&format!(
        "**Process started**: {}\n",
        state.process_started_at_rfc3339()
    ));
    md.push_str(&format!("**PID**: {}\n", std::process::id()));
    if let Some(rss) = rss_mb {
        md.push_str(&format!("**Memory**: {rss} MB RSS\n"));
    }
    md.push_str(&format!(
        "**ASIO warm scan**: {} — {}\n",
        asio_warm_scan.state, asio_warm_scan.message
    ));
    md.push('\n');

    md.push_str("## Library\n");
    md.push_str(&format!("- Tracks: {tracks}\n"));
    md.push_str(&format!("- Albums: {albums}\n"));
    md.push_str(&format!("- Artists: {artists}\n"));
    // #4845 — ce que la grille Artistes peut afficher, et pourquoi pas.
    let portraits = releve_portraits_d_artistes(
        state.backend.as_ref(),
        &crate::routes::library::artwork_cache_dir(),
    );
    if let Some(p) = &portraits {
        md.push_str(&ligne_portraits_d_artistes(p));
    }
    md.push_str(&format!("- Music dirs: {}\n", music_dirs.join(", ")));
    md.push_str(&format!("- Scan status: {scan_status}\n"));
    // #4319 — « je le vois dans Répertoires, la recherche ne le trouve pas ».
    // Le nombre d'entrées INDEXÉES tranche entre « le mot ne correspond pas »
    // et « la ligne manque à l'index ». Muet sur PostgreSQL, qui indexe par
    // colonne et n'a pas de table miroir à compter.
    //
    // #4565 — le dénominateur est le nombre de lignes de la table SOURCE, lu
    // par `couverture_de_l_index` elle-même : deux ensembles comparables. Il ne
    // vient plus des compteurs de bibliothèque ci-dessus, dont celui des
    // artistes est FILTRÉ (artistes porteurs d'un album) et levait un ⚠
    // permanent sur une ligne saine. Le titre dit ce qui est comparé.
    let couverture = couverture_de_l_index(&state);
    if !couverture.is_empty() {
        md.push_str("- Index de recherche : ");
        let lignes: Vec<String> = couverture
            .iter()
            .map(|(table, indexees, en_base)| match (indexees, en_base) {
                (Some(n), Some(total)) => {
                    let ecart = if n == total { "" } else { " ⚠" };
                    format!("{table} {n}/{total}{ecart}")
                }
                // Une moitié illisible n'est pas un écart : on ne compare pas
                // un chiffre à une absence, et surtout on ne lève pas de ⚠.
                (Some(n), None) => format!("{table} {n}/? (base illisible)"),
                (None, _) => format!("{table} illisible"),
            })
            .collect();
        md.push_str(&lignes.join(", "));
        // Ce que les deux chiffres SONT, écrit sur la ligne. Sans cela on la
        // compare aux compteurs `Albums:`/`Artists:` du dessus, qui ne
        // répondent pas à la même question — c'est précisément ce que #4565
        // a coûté.
        md.push_str(" (indexées/en base)");
        md.push('\n');
    }
    // #4319 — le filtre qui s'applique APRÈS l'index. Un album masqué sort de
    // la bibliothèque et de la recherche, mais reste visible dans Répertoires,
    // qui lit les sous-dossiers depuis le disque. La ligne d'index ci-dessus ne
    // peut pas le voir : elle compte des lignes indexées, pas des lignes
    // filtrées. Voir `albums_masques`.
    let masques = albums_masques(&state);
    md.push_str(&ligne_albums_masques(masques));
    md.push('\n');

    md.push_str(&format!("## Zones ({zone_count})\n"));
    for z in &zones {
        md.push_str(&format!(
            "- {} ({})\n",
            z["name"].as_str().unwrap_or("?"),
            z["output_type"].as_str().unwrap_or("?")
        ));
    }
    md.push_str(&format!(
        "- Zone settings not persisted: {zone_settings_ignored}\n"
    ));
    md.push('\n');

    md.push_str("## Streaming Services\n");
    for s in &service_status {
        let auth = if s["authenticated"].as_bool().unwrap_or(false) {
            "authenticated"
        } else {
            "not authenticated"
        };
        let enabled = if s["enabled"].as_bool().unwrap_or(false) {
            "enabled"
        } else {
            "disabled"
        };
        md.push_str(&format!(
            "- {}: {}, {}\n",
            s["name"].as_str().unwrap_or("?"),
            enabled,
            auth
        ));
    }
    md.push('\n');

    md.push_str("## Network\n");
    md.push_str(&format!("- Discovered devices: {}\n", devices.len()));
    // #2718 et tickets support 61, 87, 97, 98 — « plus de serveurs
    // multimedia ». « Discovered devices » ne compte QUE les renderers ; le
    // registre des serveurs multimedia est un autre registre, et ce rapport
    // — celui que le testeur JOINT a son ticket — n'en disait pas un mot.
    // Quatre rapports de suite ont donc ete lus sans que la liste dont le
    // testeur signalait la disparition y figure une seule fois.
    md.push_str(&format!(
        "- Serveurs multimedia: {}\n",
        serveurs_multimedia.len()
    ));
    for (nom, hote, port, joignable, age) in &serveurs_multimedia {
        md.push_str(&format!(
            "  - {nom} — {hote}:{port} — {} — vu il y a {age} s\n",
            if *joignable {
                "joignable"
            } else {
                "INJOIGNABLE"
            }
        ));
    }
    md.push_str(&format!("- Registered outputs: {output_count}\n"));
    // #2938 : cinq testeurs ont joint un journal ou le bind TCP 3483 echoue.
    // La ligne existait, noyee dans le journal et en anglais ; personne ne l'a
    // reliee a « ma platine n'apparait pas ». Ici elle est en haut du rapport,
    // avec sa cause sondee.
    match tune_core::slimproto::etat_ecoute() {
        Some(etat) if !etat.ecoute => {
            md.push_str(&format!(
                "- **⚠ SlimProto (Squeezebox) HORS SERVICE** — port {} : {}\n",
                etat.port,
                etat.message.as_deref().unwrap_or("cause inconnue"),
            ));
            if let Some(err) = etat.erreur_systeme.as_deref() {
                md.push_str(&format!("  - erreur systeme : {err}\n"));
            }
        }
        Some(etat) => {
            md.push_str(&format!(
                "- SlimProto (Squeezebox): en ecoute sur {}\n",
                etat.port
            ));
        }
        None => {
            md.push_str("- SlimProto (Squeezebox): aucune tentative d'ecoute\n");
        }
    }

    for (nom, etat) in [
        (
            "Pont CLI LMS",
            tune_core::slimproto::cli_server::etat_ecoute(),
        ),
        (
            "Découverte SlimProto UDP",
            tune_core::slimproto::discovery::etat_ecoute(),
        ),
    ] {
        match etat {
            // #4361 — une écoute PORTÉE AILLEURS que sur le port demandé est un
            // service rendu, pas une panne : elle ne mérite pas le « HORS
            // SERVICE ». Mais la taire ferait de ce rapport le complice du
            // silence qu'on corrige — la télécommande de l'utilisateur vise
            // encore l'ancien numéro. Le repli se reconnaît à son message.
            Some(etat) if etat.ecoute && etat.message.is_some() => md.push_str(&format!(
                "- **⚠ {nom} REPLIÉ** — en écoute sur {} {} : {}\n",
                etat.protocole,
                etat.port,
                etat.message.as_deref().unwrap_or_default(),
            )),
            Some(etat) if etat.ecoute => md.push_str(&format!(
                "- {nom} : en écoute sur {} {}\n",
                etat.protocole, etat.port
            )),
            Some(etat) => md.push_str(&format!(
                "- **⚠ {nom} HORS SERVICE** : {}\n  - erreur système : {}\n",
                etat.message.as_deref().unwrap_or("cause inconnue"),
                etat.erreur_systeme.as_deref().unwrap_or("inconnue"),
            )),
            None => md.push_str(&format!("- {nom} : aucune écoute active ni échec retenu\n")),
        }
    }

    // #3687 : un testeur a passe une soiree au tcpdump et au M-SEARCH Python
    // pour savoir si Tune repond aux recherches SSDP. La reponse tient en une
    // ligne, et elle est desormais ici — avec, en cas de panne, la cause.
    match tune_core::discovery::ssdp::etat_ecoute_ssdp() {
        Some(etat) if !etat.ecoute => {
            md.push_str(&format!(
                "- **⚠ Decouverte SSDP HORS SERVICE** — port {} : {}\n",
                etat.port,
                etat.message.as_deref().unwrap_or("cause inconnue"),
            ));
            if let Some(err) = etat.erreur_systeme.as_deref() {
                md.push_str(&format!("  - erreur systeme : {err}\n"));
            }
        }
        Some(etat) => {
            md.push_str(&format!(
                "- Decouverte SSDP: en ecoute sur {}, {} reponse(s) M-SEARCH emise(s)\n",
                etat.port, etat.reponses_msearch
            ));
            if etat.echecs > 0 {
                md.push_str(&format!(
                    "  - {} liaison(s) refusee(s) avant reprise\n",
                    etat.echecs
                ));
            }
        }
        None => {
            md.push_str("- Decouverte SSDP: aucune tentative d'ecoute\n");
        }
    }
    md.push('\n');

    // #2392 : c'est CE bloc qui aurait épargné au bêta-testeur du module
    // Diretta une réinstallation complète de Fedora. Un rapport de bogue qui
    // dit « fournisseur diretta, 0 appareil, aucun compte lié » se lit en dix
    // secondes ; un rapport muet oblige à tout redemander.
    md.push_str(&section_fournisseurs_de_sortie(
        &crate::discovery_setup::provider_status_snapshot(),
    ));

    if !oaat_endpoints.is_empty() {
        md.push_str(&format!("## OAAT Endpoints ({})\n", oaat_endpoints.len()));
        for ep in &oaat_endpoints {
            md.push_str(&format!(
                "- {} ({}): connected={}, packets={}, format={}\n",
                ep["name"].as_str().unwrap_or("?"),
                ep["host"].as_str().unwrap_or("?"),
                ep["connected"].as_bool().unwrap_or(false),
                ep["packets_sent"].as_u64().unwrap_or(0),
                ep["format"].as_str().unwrap_or("?"),
            ));
            if ep["stall_detected"].as_bool().unwrap_or(false) {
                md.push_str("  **⚠ STALL DETECTED**\n");
            }
        }
        md.push('\n');
    }

    // #3205 : sans cette section, une famine ne laissait AUCUNE trace dans ce
    // que le testeur colle sur le forum — et c'est ce rapport, sur un parc
    // réel, qui doit décider si le noyau RT de Tune OS sert à quelque chose.
    md.push_str(&section_famine_anneau(&ring_starvation));
    // #3479 : sans cette section, un etage d'egalisation qui rend du SILENCE
    // ne laissait aucune trace dans ce que le testeur depose — ni ici, ni dans
    // le journal. Reivax66 a fourni 25 lignes `eq_change_journal` toutes
    // saines pendant que son son disparaissait : elles disent que l'etage
    // s'installe, jamais ce qu'il produit.
    if !dsp_egaliseur.is_empty() {
        md.push_str("## DSP — egaliseur (ce que l'etage PRODUIT)\n");
        for d in &dsp_egaliseur {
            md.push_str(&format!(
                "- {} : {} echantillon(s) remis a ZERO (non finis), {} saturation(s)\n",
                d["output_name"].as_str().unwrap_or("?"),
                d["eq_non_finite_samples"].as_u64().unwrap_or(0),
                d["eq_overs"].as_u64().unwrap_or(0),
            ));
        }
        md.push_str(
            "  (un echantillon « remis a zero » = une cascade de biquads devenue \
instable ; l'anneau reste alimente et le DAC recoit du silence. A ne pas \
confondre avec la famine de l'anneau, comptee au-dessus)\n\n",
        );
    }
    // #2218 (T9 suite) : ReplayGain sans pic tague ecretait 66 % d'un sinus a
    // −0,1 dBFS sans compteur ni journal ; l'egaliseur comptait ses overs sans
    // les dire. Chaque etage compte desormais la ou son clamp a lieu, sans le
    // changer. Par processus depuis le demarrage, pas par zone.
    let dsp_ecretage = tune_core::audio::ecretage::releve();
    md.push_str("## DSP — ecretage (compte la ou le clamp a lieu, #2218)\n");
    for (nom, e) in dsp_ecretage.etages() {
        md.push_str(&format!(
            "- {nom} : {} echantillon(s) ecrete(s) sur {} ({} %), exces max {} LSB, {} bloc(s) ecretant(s), {} piste(s) close(s) avec ecretage, {} ligne(s) dsp_ecretage\n",
            e.echantillons_ecretes,
            e.echantillons_vus,
            e.pourcentage,
            e.exces_max_lsb,
            e.appels_ecretants,
            e.pistes_ecretees,
            e.lignes_journal,
        ));
    }
    md.push_str(
        "  (compte depuis le demarrage du processus, tous flux confondus ; le \
journal porte `dsp_ecretage` au premier ecretage d'une piste et a sa fin, \
jamais par bloc. Les echantillons ne sont pas modifies par le comptage)\n\n",
    );
    md.push_str("## Database\n");
    // #3182 : c'était `format!("- Engine: sqlite\n")` — un `format!` sans
    // argument, donc une chaîne littérale, et toute installation PostgreSQL
    // se déclarait SQLite dans son propre rapport. Sur le ticket 71 de
    // jfpaquet cette ligne a failli faire écarter #3181, qui n'existe que
    // parce que le moteur est PostgreSQL.
    md.push_str(&format!("- Engine: {}\n", state.backend.engine()));
    md.push_str(&format!(
        "- Migration version: {}\n",
        version_de_schema_affichee(db_version)
    ));

    // #2856 — le rapport ne portait AUCUNE section de réglages. Ni l'état de
    // l'enrichissement au scan, ni le moteur audio : deux faits qu'il fallait
    // redemander au testeur à chaque ticket de métadonnées ou de son, alors
    // que le serveur les a sous la main. La fiche système (`/system/profile`)
    // en portait déjà une partie ; le rapport, lui, est ce que le testeur
    // COLLE sur le forum, et c'est là qu'on lit un ticket.
    //
    // La liste des réglages publiables est celle de la fiche, PARTAGÉE et non
    // recopiée : deux listes auraient divergé, et la seconde n'aurait pas
    // hérité de la garde qui interdit d'y faire entrer une clé secrète.
    let reglages = super::profile::support_settings(|k| settings.get(k).ok().flatten());
    let moteur_audio = super::profile::moteur_audio(&state);
    md.push_str("\n## Settings\n");
    md.push_str(&format!(
        "- Audio backend: requested={}, active={}\n",
        valeur_lisible(&moteur_audio["backend_requested"]),
        valeur_lisible(&moteur_audio["backend_active"]),
    ));
    md.push_str(&format!(
        "- Exclusive mode: requested={}, effective={}, forced={}{}\n",
        valeur_lisible(&moteur_audio["exclusive_mode"]["requested"]),
        valeur_lisible(&moteur_audio["exclusive_mode"]["effective"]),
        valeur_lisible(&moteur_audio["exclusive_mode"]["forced"]),
        match moteur_audio["exclusive_mode"]["detail"].as_str() {
            Some(raison) => format!(" — {raison}"),
            None => String::new(),
        },
    ));
    for (cle, valeur) in &reglages {
        md.push_str(&format!("- {cle}: {}\n", valeur_lisible(valeur)));
    }

    // Recent logs (tail) — the single most useful part of a bug report. Reuses
    // the same collector as the /logs endpoint so the report matches what the
    // "Export logs" button shows.
    // On lit large et on filtre, plutôt que de lire 200 lignes et d'espérer
    // qu'elles parlent du défaut (#1884). L'export complet, lui, reste verbatim.
    let Json(logs_json) = collect_recent_logs(BUG_REPORT_LOG_SCAN_LINES).await;
    let brut = logs_json["logs"].as_str().unwrap_or("");
    let filtre = lignes_utiles_pour_un_rapport(brut, BUG_REPORT_LOG_LINES);
    let log_text = filtre.trim();
    let log_source = logs_json["source"].as_str().unwrap_or("none");
    let periode = periode_couverte(log_text)
        .map(|p| format!(", {p}"))
        .unwrap_or_default();
    md.push_str(&format!(
        "\n## Recent Logs ({BUG_REPORT_LOG_LINES} dernières lignes INFO et au-dessus{periode}, source: {log_source} — le DEBUG est dans l'export complet)\n"
    ));
    if log_text.is_empty() {
        md.push_str("_No logs available._\n");
    } else {
        md.push_str("```\n");
        md.push_str(log_text);
        md.push_str("\n```\n");
    }

    Json(json!({
        "version": tune_core::version(),
        // #3380 — le champ que la telemetrie reprend et que l'admin mozaiklabs
        // affichera a cote de `version`. `null` = interface non identifiable.
        "ui_version": version_interface,
        "engine": "rust",
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "uptime_seconds": uptime_secs,
        "uptime": uptime_str,
        "process_started_at": state.process_started_at_rfc3339(),
        "pid": std::process::id(),
        "rss_mb": rss_mb,
        "library": {
            "tracks": tracks,
            "albums": albums,
            "artists": artists,
            // #4845 — portraits que la grille Artistes peut afficher.
            "artist_portraits": portraits.as_ref().map(|p| json!({
                "total": p.total,
                "displayable": p.affichables,
                "without_image": p.sans_image,
                "cache_missing": p.cache_perdu,
                "remote_url": p.distantes,
                "sources": p.sources,
            })),
            "music_dirs": music_dirs,
            "scan_status": scan_status,
            // #4319 — ce que la RECHERCHE voit, à côté de ce que la
            // bibliothèque contient. Absent sur PostgreSQL.
            //
            // #4565 — `search_index_total` est le SEUL dénominateur juste :
            // le nombre de lignes de la table source. Les compteurs
            // `library.artists` & co ci-dessus répondent à une autre question
            // (`ArtistRepo::count()` ne compte que les artistes porteurs d'un
            // album) ; les comparer à l'index levait un ⚠ permanent.
            "search_index": couverture
                .iter()
                .map(|(t, indexees, _)| ((*t).to_string(), json!(indexees)))
                .collect::<serde_json::Map<_, _>>(),
            "search_index_total": couverture
                .iter()
                .map(|(t, _, en_base)| ((*t).to_string(), json!(en_base)))
                .collect::<serde_json::Map<_, _>>(),
            // #4319 — le filtre d'APRÈS l'index. `null` quand la table n'a pas
            // pu être lue : une base trop ancienne ne doit pas dire « 0 ».
            "hidden_albums": masques,
        },
        "zones": {
            "count": zone_count,
            "items": zones,
        },
        "zone_settings_ignored": zone_settings_ignored,
        "asio_warm_scan": asio_warm_scan,
        "streaming_services": service_status,
        "network": {
            "discovered_devices": devices.len(),
            "registered_outputs": output_count,
            "slimproto": tune_core::slimproto::etat_ecoute(),
            "lms_cli": tune_core::slimproto::cli_server::etat_ecoute(),
            "slimproto_udp": tune_core::slimproto::discovery::etat_ecoute(),
            // Le pendant de la ligne markdown ci-dessus (#3687).
            "ssdp": tune_core::discovery::ssdp::etat_ecoute_ssdp(),
        },
        "oaat_endpoints": oaat_endpoints,
        "ring_starvation": ring_starvation,
        // Le pendant JSON de la section markdown ci-dessus (#3479).
        "dsp_egaliseur": dsp_egaliseur,
        // Le pendant JSON de la section « DSP — ecretage » (#2218).
        "dsp_ecretage": dsp_ecretage,
        "database": {
            // #3182 : même mensonge que la ligne markdown ci-dessus, dans le
            // corps JSON que le client lit.
            "engine": state.backend.engine().as_str(),
            "migration_version": db_version,
        },
        // #2856 — les mêmes réglages que la section markdown, pour le client
        // qui lit le JSON. Même source, donc jamais deux vérités.
        "settings": reglages,
        "audio": moteur_audio,
        "markdown": md,
    }))
}

/// Returns the bug report as raw markdown (text/markdown) for direct forum paste.
pub(super) async fn bug_report_markdown(
    State(state): State<AppState>,
) -> (
    axum::http::StatusCode,
    [(axum::http::header::HeaderName, &'static str); 1],
    String,
) {
    let Json(report) = generate_bug_report(State(state)).await;
    let md = report["markdown"].as_str().unwrap_or("").to_string();
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        md,
    )
}

#[derive(Deserialize)]
pub(super) struct BugReportSubmitBody {
    #[serde(default)]
    description: String,
}

// ---------------------------------------------------------------------------
// #4564 — les captures du signalement forum
// ---------------------------------------------------------------------------

/// Au plus trois captures. Le point d'entrée communautaire est OUVERT (aucun
/// jeton, `throttle:5,60` côté site) : la borne n'est pas cosmétique.
const BUG_REPORT_MAX_IMAGES: usize = 3;

/// 4 Mio par capture — exactement le plafond de l'éditeur du forum
/// (`ThreadController::uploadImage`, `image|max:4096`), lui qui exige pourtant
/// une session authentifiée.
const BUG_REPORT_MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// Des IMAGES, et rien d'autre.
///
/// Ce n'est pas une prudence de principe : les captures sont posées **en ligne**
/// dans le corps du fil, en `<img>`. Un `.log` ou un `.zip` n'y a aucune forme
/// d'existence — la table `forum_attachments` a été SUPPRIMÉE côté site
/// (migration `2026_03_07_100001`). Accepter un journal ici reviendrait à le
/// ranger sur le disque sans que rien ne le montre jamais.
const BUG_REPORT_IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Une capture reçue du navigateur, déjà bornée.
struct CaptureJointe {
    file_name: String,
    content_type: String,
    bytes: Vec<u8>,
}

fn extension_de(nom: &str) -> String {
    nom.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn mime_image(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
}

/// 400 avec un code machine ET une phrase lisible : c'est elle que l'écran
/// montre au testeur. Un refus muet le renverrait ouvrir un second fil à la
/// main — précisément ce que #4564 supprime.
fn refus_de_capture(code: &str, message: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "message": message })),
    )
        .into_response()
}

/// Lit le `multipart/form-data` du signalement : la `description`, et les
/// captures sous `images[]`.
///
/// Nombre, taille et type sont vérifiés **ici**, avant tout relais, pour que le
/// refus soit une phrase et non un 422 amont. Le nombre et le type sont jugés
/// AVANT de bufferiser l'octet : un envoi hors bornes ne coûte pas sa taille en
/// mémoire.
async fn lire_captures(
    mut multipart: axum::extract::Multipart,
) -> Result<(String, Vec<CaptureJointe>), axum::response::Response> {
    let mut description = String::new();
    let mut images: Vec<CaptureJointe> = Vec::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(refus_de_capture("invalid_multipart", &e.to_string())),
        };

        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(str::to_string);
        let declared_ct = field.content_type().map(str::to_string);

        match file_name {
            Some(fname) if !fname.is_empty() => {
                if images.len() >= BUG_REPORT_MAX_IMAGES {
                    return Err(refus_de_capture(
                        "too_many_images",
                        &format!("Trop de captures : {BUG_REPORT_MAX_IMAGES} images au maximum."),
                    ));
                }
                let ext = extension_de(&fname);
                if !BUG_REPORT_IMAGE_EXT.contains(&ext.as_str()) {
                    return Err(refus_de_capture(
                        "image_type",
                        &format!(
                            "« {fname} » n'est pas une image. Formats acceptés : {}.",
                            BUG_REPORT_IMAGE_EXT.join(", ")
                        ),
                    ));
                }
                let bytes = match field.bytes().await {
                    Ok(b) => b,
                    Err(e) => return Err(refus_de_capture("image_read", &e.to_string())),
                };
                if bytes.len() > BUG_REPORT_MAX_IMAGE_BYTES {
                    return Err(refus_de_capture(
                        "image_too_large",
                        &format!(
                            "« {fname} » dépasse {} Mo.",
                            BUG_REPORT_MAX_IMAGE_BYTES / (1024 * 1024)
                        ),
                    ));
                }
                images.push(CaptureJointe {
                    content_type: declared_ct.unwrap_or_else(|| mime_image(&ext).to_string()),
                    file_name: fname,
                    bytes: bytes.to_vec(),
                });
            }
            _ => {
                let value = match field.text().await {
                    Ok(v) => v,
                    Err(e) => return Err(refus_de_capture("invalid_field", &e.to_string())),
                };
                if name == "description" {
                    description = value;
                }
            }
        }
    }

    Ok((description, images))
}

/// POST /system/bug-report/submit — build the local bug report (diagnostics +
/// recent logs), prepend the user's free-text description, and forward it to the
/// mozaiklabs.fr community bug endpoint, which creates a *moderated* (pending)
/// `bug` forum thread with its own credentials and returns the public URL. Done
/// server-to-server (this Rust process, not the browser) so it dodges the cloud's
/// CORS origin allow-list and can attach the instance id / version / OS the
/// browser doesn't have. The distributed server never holds a forum admin token.
///
/// # #4564 — les captures
///
/// Un seul point d'entrée, deux formats, choisis d'après le `Content-Type`
/// entrant — même patron que `routes/support.rs` et que `/import/roon` :
/// `application/json` (chemin historique, sans capture) ou
/// `multipart/form-data` avec `images[]`.
///
/// **Ce que le forum accepte réellement — vérifié dans `site-mozaiklabs` avant
/// d'écrire ce contrat, et non supposé.** `BugReportController::store` ne
/// validait que `{title?, body, os?, version?, instance_id?}` : une clé inconnue
/// est jetée en silence par `validate()`. La voie voisine du même écran, le
/// ticket de support, accepte bien `attachments[]` — mais c'est une AUTRE chaîne
/// (`SupportTicketController`, authentifiée premium, avec un modèle
/// `SupportAttachment`). Un fil de forum, lui, n'a plus de pièces jointes du
/// tout : la table `forum_attachments` a été supprimée (migration
/// `2026_03_07_100001`) et un fil porte ses images EN LIGNE, déposées par
/// `ThreadController::uploadImage` — qui exige une session authentifiée, donc
/// inatteignable depuis ce relais serveur-à-serveur sans jeton.
///
/// Le champ s'appelle donc `images[]` et non `attachments[]`, et le point
/// d'entrée communautaire a été étendu pour l'accepter (PR site-mozaiklabs
/// jumelle). Sans elle, ce code enverrait des fichiers que le site jetterait
/// sans le dire.
pub(super) async fn submit_bug_report(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::RequestExt;
    use axum::response::IntoResponse;

    let est_multipart = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("multipart/form-data"));

    let (description, images) = if est_multipart {
        let multipart = match req.extract::<axum::extract::Multipart, _>().await {
            Ok(m) => m,
            Err(rej) => return rej.into_response(),
        };
        match lire_captures(multipart).await {
            Ok(v) => v,
            Err(resp) => return resp,
        }
    } else {
        match req.extract::<Json<BugReportSubmitBody>, _>().await {
            Ok(Json(b)) => (b.description, Vec::new()),
            Err(rej) => return rej.into_response(),
        }
    };

    let (code, corps) = envoyer_le_rapport(state, description, images).await;
    (code, Json(corps)).into_response()
}

/// Le corps du signalement, une fois le format d'entrée résolu.
///
/// Séparé du handler pour une raison : c'est ici que vit tout ce qui était déjà
/// éprouvé — composition du fil, titre, troncature à 50 000 caractères,
/// identifiant d'instance —, et les captures ne devaient RIEN y changer d'autre
/// que le transport.
async fn envoyer_le_rapport(
    state: AppState,
    description: String,
    images: Vec<CaptureJointe>,
) -> (axum::http::StatusCode, Value) {
    use axum::http::StatusCode;

    let description = description.trim().to_string();

    // Build the diagnostics + logs report (same content as the preview/markdown).
    let backend = state.backend.clone();
    let url = bug_report_url(&state);
    let Json(report) = generate_bug_report(State(state)).await;
    let report_md = report["markdown"].as_str().unwrap_or("").to_string();
    if report_md.trim().is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "empty bug report" }),
        );
    }

    // Compose the thread body: the user's own words first, then diagnostics.
    let full_markdown = if description.is_empty() {
        report_md
    } else {
        format!("{description}\n\n---\n\n{report_md}")
    };

    let version = tune_core::version();
    let platform = std::env::consts::OS;

    // Title: first non-empty line of the description, else a generic one.
    let title = description
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| format!("Bug: {}", l.chars().take(80).collect::<String>()))
        .unwrap_or_else(|| format!("Bug report — Tune {version} ({platform})"));

    // The site caps the body at 50k chars — truncate the tail (oldest logs) if
    // the report runs long rather than getting rejected wholesale.
    let body_md = if full_markdown.chars().count() > BUG_REPORT_MAX_BODY_CHARS {
        let kept: String = full_markdown
            .chars()
            .take(BUG_REPORT_MAX_BODY_CHARS)
            .collect();
        format!("{kept}\n\n_…report truncated…_")
    } else {
        full_markdown
    };

    let instance_id = tune_core::db::settings_repo::SettingsRepo::with_backend(backend)
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    // Contract of the community bug-report endpoint: { title?, body, os?,
    // version?, instance_id? } — plus `images[]` depuis #4564.
    let champs: [(&str, String); 5] = [
        ("title", title),
        ("body", body_md),
        ("os", platform.to_string()),
        ("version", version.to_string()),
        ("instance_id", instance_id),
    ];

    let client = match tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("http client: {e}") }),
            );
        }
    };

    // Sans capture, le fil part EXACTEMENT comme avant : un corps JSON. #4564
    // n'ajoute un multipart que lorsqu'il y a quelque chose à transporter — un
    // serveur qui ne joint rien ne change donc rien à ce qu'il émettait.
    let nb_images = images.len();
    // `Accept: application/json`, et ce n'est pas décoratif : sans lui, Laravel
    // répond à un refus de validation par une REDIRECTION 302 vers la page
    // précédente au lieu d'un 422. `reqwest` la suit, tombe sur une page HTML
    // en 200, et le refus se lirait « envoyé » — le testeur croirait sa capture
    // partie. Mesuré sur le banc Pest de la PR jumelle `site-mozaiklabs`.
    let requete = if images.is_empty() {
        let payload: serde_json::Map<String, Value> = champs
            .into_iter()
            .map(|(k, v)| (k.to_string(), Value::String(v)))
            .collect();
        client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&payload)
    } else {
        let mut form = reqwest::multipart::Form::new();
        for (k, v) in champs {
            form = form.text(k, v);
        }
        for image in images {
            let part = match reqwest::multipart::Part::bytes(image.bytes)
                .file_name(image.file_name)
                .mime_str(&image.content_type)
            {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "image_invalid_mime", "message": e.to_string() }),
                    );
                }
            };
            // `images[]` — le nom exact qu'attend la règle Laravel `images.*`.
            // Sous un autre nom, le site accepterait la requête et jetterait
            // les fichiers EN SILENCE : le testeur croirait sa capture partie.
            form = form.part("images[]", part);
        }
        client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .multipart(form)
    };

    match requete.send().await {
        Ok(resp) if resp.status().is_success() => {
            // Site responds { status, images, thread: { id, slug, url } }.
            let data: Value = resp.json().await.unwrap_or_else(|_| json!({}));
            let thread = &data["thread"];
            // #4564 — le nombre que le SITE dit avoir rangé, pas celui qu'on a
            // envoyé : l'écran annonce ce qui est arrivé, pas ce qu'on espérait.
            // Un site antérieur à la PR jumelle ne rend pas la clé — on ne
            // fabrique alors aucun chiffre.
            let images_rangees = data.get("images").and_then(Value::as_u64);
            if images_rangees.is_some_and(|n| n as usize != nb_images) {
                tracing::warn!(
                    envoyees = nb_images,
                    rangees = images_rangees,
                    "bug_report_captures_partielles"
                );
            }
            (
                StatusCode::OK,
                json!({
                    "status": "ok",
                    "url": thread.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                    "slug": thread.get("slug").and_then(|v| v.as_str()).unwrap_or(""),
                    "images": images_rangees,
                }),
            )
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            tracing::warn!(status, images = nb_images, "bug_report_submit_rejected");
            (
                StatusCode::BAD_GATEWAY,
                json!({ "error": "cloud rejected the report", "status": status }),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "bug_report_submit_failed");
            (
                StatusCode::BAD_GATEWAY,
                json!({ "error": format!("could not reach the bug service: {e}") }),
            )
        }
    }
}

pub(super) async fn audio_check() -> Json<Value> {
    let formats = vec![
        "flac", "wav", "aiff", "mp3", "aac", "ogg", "opus", "alac", "dsd", "wavpack", "ape",
    ];

    Json(json!({
        "native_engine": true,
        "supported_formats": formats,
        "lofty_available": true,
        "engine": "rust",
    }))
}

pub(super) async fn asio_warm_scan_status() -> Json<Value> {
    Json(json!(crate::startup::asio_warm_status()))
}

/// Retire uniquement le témoin qui interdit le prochain préchauffage.
///
/// La tentative attend le redémarrage : énumérer les pilotes ASIO à chaud peut
/// faire planter le processus ou heurter une sortie qui possède déjà le DAC.
pub(super) async fn rearm_asio_warm_scan(
    _admin: crate::auth::RequireAdmin,
) -> (StatusCode, Json<Value>) {
    use crate::startup::AsioWarmRearm;

    match crate::startup::rearm_asio_warm_scan() {
        Ok(AsioWarmRearm::Rearmed) => (
            StatusCode::OK,
            Json(json!({
                "status": "rearmed",
                "retry": "next_restart",
                "message": "Le balayage ASIO sera retenté une fois au prochain démarrage de Tune.",
                "asio_warm_scan": crate::startup::asio_warm_status(),
            })),
        ),
        Ok(AsioWarmRearm::AlreadyReady) => (
            StatusCode::OK,
            Json(json!({
                "status": "already_ready",
                "retry": "next_restart",
                "message": "Le balayage ASIO est déjà autorisé au prochain démarrage.",
                "asio_warm_scan": crate::startup::asio_warm_status(),
            })),
        ),
        Ok(AsioWarmRearm::DisabledByEnv) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "asio_warm_scan_disabled_by_env",
                "message": "Retirez TUNE_DISABLE_ASIO_SCAN puis redémarrez Tune ; le réarmement ne contourne pas ce coupe-circuit.",
                "asio_warm_scan": crate::startup::asio_warm_status(),
            })),
        ),
        Ok(AsioWarmRearm::Unsupported) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": "asio_not_supported",
                "message": "Le préchauffage ASIO ne concerne que Windows.",
                "asio_warm_scan": crate::startup::asio_warm_status(),
            })),
        ),
        Err(error) => {
            tracing::warn!(%error, "asio_warm_scan_rearm_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "asio_warm_scan_rearm_failed",
                    "message": error,
                    "asio_warm_scan": crate::startup::asio_warm_status(),
                })),
            )
        }
    }
}

/// Anonymous telemetry snapshot — returns what would be sent if telemetry
/// is enabled. No data leaves the server unless the user explicitly opts in.
///
/// #3383 : `enabled` disait autrefois « le reglage vaut exactement `"true"` »,
/// ce qui annonçait un opt-out sur une installation neuve qui n'avait rien
/// decoche — et ignorait `TUNE_TELEMETRY`. Il dit maintenant l'etat EFFECTIF,
/// le meme que les gardes d'envoi consultent, par le meme appel.
pub(super) async fn telemetry_snapshot(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = tune_core::cloud::telemetry::TelemetryReporter::is_enabled_for(&settings);
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let zone_count = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let uptime = state.started_at.elapsed().as_secs();

    Json(json!({
        "enabled": enabled,
        "payload": {
            "version": tune_core::version(),
            "engine": "rust",
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "uptime_seconds": uptime,
            "tracks": tracks,
            "albums": albums,
            "artists": artists,
            "zones": zone_count,
        }
    }))
}

/// #3383 — cette route ecrivait deja la bonne cle, mais personne ne la lisait :
/// un aller-retour ferme sur lui-meme. Elle est desormais BRANCHEE, parce que
/// `TelemetryReporter::is_enabled_for` consulte cette meme cle. Ce n'est donc
/// plus un troisieme interrupteur mort a cote de deux autres, c'est le meme.
///
/// La reponse renvoie l'etat EFFECTIF et non ce qui vient d'etre demande :
/// `TUNE_TELEMETRY=false` reste souverain, et un appelant qui rallume alors
/// que l'exploitant a coupe doit le voir.
pub(super) async fn telemetry_toggle(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let _ = settings.set(
        tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY,
        if enabled { "true" } else { "false" },
    );
    Json(json!({
        "enabled": tune_core::cloud::telemetry::TelemetryReporter::is_enabled_for(&settings),
    }))
}

pub(super) async fn api_stats(State(state): State<AppState>) -> Json<Value> {
    let stats = state.api_analytics.stats();
    Json(serde_json::to_value(stats).unwrap_or_default())
}

pub(super) async fn api_insights(State(state): State<AppState>) -> Json<Value> {
    let stats = state.api_analytics.stats();
    let mut issues: Vec<Value> = Vec::new();

    // High error rate
    if stats.error_rate_pct > 5.0 {
        issues.push(json!({
            "severity": "warning",
            "type": "high_error_rate",
            "message": format!("API error rate is {:.1}% (threshold: 5%)", stats.error_rate_pct),
        }));
    }

    // Slow endpoints (P95 > 500ms)
    for ep in &stats.slowest_endpoints {
        if ep.p95_latency_ms > 500 {
            issues.push(json!({
                "severity": "warning",
                "type": "slow_endpoint",
                "endpoint": ep.endpoint,
                "p95_ms": ep.p95_latency_ms,
                "message": format!("{} P95 latency {}ms (threshold: 500ms)", ep.endpoint, ep.p95_latency_ms),
            }));
        }
    }

    // Zone poller issues
    let metrics = state.poller_metrics.lock().await;
    for (zone_id, m) in metrics.iter() {
        if m.total_polls > 10 && m.total_errors > 0 {
            let err_pct = m.total_errors as f64 / m.total_polls as f64 * 100.0;
            if err_pct > 10.0 {
                issues.push(json!({
                    "severity": "error",
                    "type": "zone_poll_failures",
                    "zone_id": zone_id,
                    "error_rate_pct": (err_pct * 10.0).round() / 10.0,
                    "message": format!("Zone {} has {:.0}% poll error rate", zone_id, err_pct),
                }));
            }
        }
        if m.max_latency_ms > 2000 {
            issues.push(json!({
                "severity": "warning",
                "type": "zone_high_latency",
                "zone_id": zone_id,
                "max_latency_ms": m.max_latency_ms,
                "message": format!("Zone {} max latency {}ms", zone_id, m.max_latency_ms),
            }));
        }
        // #2493 : l'appareil annonce toujours jouer alors que la position a
        // atteint — ou depasse — la duree de la piste depuis une minute. Le
        // sondeur ne coupe rien (une duree fausse produit la meme forme qu'une
        // lecture bloquee), mais il refuse de laisser le diagnostic annoncer
        // une lecture saine.
        if m.lecture_au_dela_de_la_duree {
            issues.push(json!({
                "severity": "warning",
                "type": "zone_playback_beyond_duration",
                "zone_id": zone_id,
                "message": format!(
                    "Zone {zone_id} : l'appareil annonce toujours la lecture alors que la \
                     position a atteint la fin de la piste. Soit la lecture est bloquee, soit \
                     la duree connue est fausse — voir lecture_annoncee_au_dela_de_la_duree \
                     dans le journal."
                ),
            }));
        }
    }
    drop(metrics);

    let status = if issues.iter().any(|i| i["severity"] == "error") {
        "degraded"
    } else if issues.is_empty() {
        "healthy"
    } else {
        "warning"
    };

    Json(json!({
        "status": status,
        "issues": issues,
        "total_issues": issues.len(),
        "api_requests_analyzed": stats.total_requests,
    }))
}

pub(super) async fn api_docs() -> Json<Value> {
    let routes = vec![
        // System
        ("GET", "/system/version", "Server version and engine"),
        ("GET", "/system/health", "Health check"),
        (
            "GET",
            "/system/stats",
            "Library statistics (tracks, albums, artists, zones)",
        ),
        ("GET", "/system/diagnostics", "Full diagnostic report"),
        ("GET", "/system/changelog", "Version changelog"),
        (
            "GET",
            "/system/api-stats",
            "Per-endpoint latency and error analytics",
        ),
        (
            "GET",
            "/system/api-docs",
            "This endpoint — API documentation",
        ),
        (
            "GET",
            "/system/audio/asio-warm-scan",
            "ASIO startup scan fail-safe status",
        ),
        (
            "POST",
            "/system/audio/asio-warm-scan/rearm",
            "Allow one ASIO startup scan on the next restart (admin)",
        ),
        ("GET", "/system/telemetry", "Telemetry snapshot (opt-in)"),
        ("POST", "/system/scan", "Trigger library scan"),
        ("GET", "/system/scan/status", "Scan progress"),
        ("GET", "/system/logs", "Server logs"),
        ("GET", "/system/backups", "List backups"),
        ("POST", "/system/backups", "Create backup"),
        ("POST", "/system/backups/encrypt", "Create encrypted backup"),
        ("POST", "/system/import/roon", "Import from Roon"),
        ("POST", "/system/import/jriver", "Import from JRiver XML"),
        ("POST", "/system/import/plex", "Import from Plex"),
        // Library
        (
            "GET",
            "/library/albums",
            "List albums (paginated, filterable)",
        ),
        (
            "GET",
            "/library/albums/grouped",
            "Albums grouped by release (deluxe/remastered)",
        ),
        ("GET", "/library/albums/{id}", "Album details"),
        ("GET", "/library/albums/{id}/tracks", "Album tracks"),
        (
            "GET",
            "/library/albums/{id}/completeness",
            "Album track completeness check",
        ),
        ("GET", "/library/artists", "List artists"),
        (
            "GET",
            "/library/artists/{id}/timeline",
            "Artist discography with gaps",
        ),
        ("GET", "/library/tracks", "List tracks (paginated)"),
        (
            "GET",
            "/library/tracks/{id}/waveform",
            "Track waveform (200-point amplitude)",
        ),
        (
            "GET",
            "/library/tracks/{id}/synced-lyrics",
            "Synchronized lyrics (.lrc)",
        ),
        (
            "GET",
            "/library/tracks/{id}/source-links",
            "Cross-service matches",
        ),
        (
            "POST",
            "/library/identify",
            "Identify track via AcoustID fingerprint",
        ),
        (
            "GET",
            "/library/duplicates",
            "Duplicate tracks (hash + fingerprint + metadata)",
        ),
        (
            "GET",
            "/library/stats/completeness",
            "Library health score (A-F grade)",
        ),
        ("GET", "/library/genre-tree", "Hierarchical genre tree"),
        ("GET", "/search", "Federated search (local + streaming)"),
        // Zones & Playback
        ("GET", "/zones", "List zones"),
        ("POST", "/zones", "Create zone"),
        (
            "GET",
            "/zones/{id}/status",
            "Zone playback status + credits",
        ),
        (
            "GET",
            "/zones/{id}/network-health",
            "Zone network quality metrics",
        ),
        ("GET", "/zones/sync-status", "All zones with poller metrics"),
        ("POST", "/zones/{id}/play", "Play track/album/playlist"),
        ("POST", "/zones/{id}/pause", "Pause"),
        ("POST", "/zones/{id}/next", "Next track"),
        ("POST", "/zones/{id}/sleep", "Sleep timer with fade"),
        ("GET", "/zones/{id}/dsp", "Zone DSP/EQ config"),
        // Streaming
        (
            "GET",
            "/streaming/services",
            "List streaming services status",
        ),
        (
            "GET",
            "/streaming/compare",
            "Compare search across services",
        ),
        (
            "GET",
            "/streaming/{service}/search",
            "Search a streaming service",
        ),
        // Playlists
        ("GET", "/playlists", "List playlists"),
        ("POST", "/playlists", "Create playlist"),
        (
            "GET",
            "/playlists/{id}/export",
            "Export (format=m3u|json|csv|xspf)",
        ),
        // Radio & DJ
        ("GET", "/radio/auto", "Auto-DJ playlist from seed track"),
        ("GET", "/radios", "List radio stations"),
        // Dashboard
        ("GET", "/dashboard/stats", "Listening dashboard"),
        ("GET", "/dashboard/wrapped", "Year-in-review Wrapped stats"),
        ("GET", "/dashboard/top-artists", "Top artists"),
        ("GET", "/dashboard/genre-breakdown", "Genre distribution"),
        // Party
        ("POST", "/party/rooms", "Create collaborative room"),
        ("GET", "/party/rooms", "List rooms"),
        // Other
        (
            "POST",
            "/voice-search",
            "Voice search via Whisper transcription",
        ),
        (
            "GET",
            "/demo/library",
            "Read-only library browse (demo mode)",
        ),
    ];

    let endpoints: Vec<Value> = routes.iter().map(|(method, path, desc)| {
        json!({"method": method, "path": format!("/api/v1{path}"), "description": desc})
    }).collect();

    Json(json!({
        "version": tune_core::version(),
        "total_endpoints": endpoints.len(),
        "endpoints": endpoints,
    }))
}

/// List ASIO audio devices (Windows-only, requires `asio` feature).
pub(super) async fn asio_devices(State(_state): State<AppState>) -> Json<Value> {
    #[cfg(feature = "local-audio")]
    {
        let devices = tokio::task::spawn_blocking(tune_core::outputs::local::list_asio_devices)
            .await
            .unwrap_or_default();
        let count = devices.len();
        Json(json!({
            "devices": devices,
            "asio_available": tune_core::outputs::local::asio_available(),
            "count": count,
        }))
    }
    #[cfg(not(feature = "local-audio"))]
    {
        Json(json!({
            "devices": [],
            "asio_available": false,
            "count": 0,
        }))
    }
}

#[cfg(test)]
mod log_tail_tests {
    use super::*;

    #[test]
    fn missing_file_is_missing_not_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.log");
        match read_log_tail(path.to_str().unwrap(), 10, 1024) {
            Err(LogTailError::Missing) => {}
            _ => panic!("expected Missing"),
        }
    }

    #[test]
    fn tail_window_drops_truncated_first_line_and_caps_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let content: String = (0..100).map(|i| format!("line-{i:03}\n")).collect();
        std::fs::write(&path, &content).unwrap();

        // Window smaller than the file: starts mid-file, first partial line dropped.
        let lines = read_log_tail(path.to_str().unwrap(), 1000, 95).unwrap();
        assert!(lines.len() < 100);
        assert_eq!(lines.last().unwrap(), "line-099");
        // Every returned line is complete.
        assert!(lines.iter().all(|l| l.starts_with("line-")));

        // max_lines caps the result at the newest lines.
        let lines = read_log_tail(path.to_str().unwrap(), 3, u64::MAX).unwrap();
        assert_eq!(lines, ["line-097", "line-098", "line-099"]);
    }

    #[test]
    fn whole_file_when_window_is_larger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.log");
        std::fs::write(&path, "a\nb\n").unwrap();
        let lines = read_log_tail(path.to_str().unwrap(), 1000, 1024).unwrap();
        assert_eq!(lines, ["a", "b"]);
    }
}

#[cfg(test)]
mod tests_journal_rapport {
    use super::{
        horodatage_de_ligne, lignes_utiles_pour_un_rapport, niveau_de_ligne, periode_couverte,
    };

    /// Le cas mesuré : 160 sondes SSDP en DEBUG chassaient tout le reste.
    #[test]
    fn le_debug_bavard_ne_chasse_plus_ce_qui_compte() {
        let mut journal = String::new();
        for i in 0..160 {
            journal.push_str(&format!(
                "2026-08-17T15:22:15.003+02:00 DEBUG tune_core::discovery::ssdp: ssdp_unicast_probe_ok id=uuid:{i}\n"
            ));
        }
        journal.push_str(
            "2026-08-17T15:25:00.000+02:00  INFO tune_core::audio::embedding: audio_embedding_batch embedded=10\n",
        );
        journal.push_str(
            "2026-08-17T15:25:01.000+02:00  WARN tune_core::audio::embedding: audio_embed_decode_failed track_id=42\n",
        );

        let garde = lignes_utiles_pour_un_rapport(&journal, 200);

        assert!(
            !garde.contains("ssdp_unicast_probe_ok"),
            "le DEBUG bavard sort"
        );
        assert!(garde.contains("audio_embedding_batch"), "l'INFO reste");
        assert!(garde.contains("audio_embed_decode_failed"), "le WARN reste");
        assert_eq!(garde.lines().count(), 2);
    }

    /// La coupe se fait APRÈS le filtrage : on garde N lignes utiles, pas les
    /// N dernières lignes du fichier.
    #[test]
    fn on_garde_les_dernieres_lignes_utiles() {
        let mut journal = String::new();
        for i in 0..10 {
            journal.push_str(&format!("2026-08-17T10:00:0{i}Z  INFO m: utile-{i}\n"));
            journal.push_str(&format!("2026-08-17T10:00:0{i}Z DEBUG m: bruit-{i}\n"));
        }
        let garde = lignes_utiles_pour_un_rapport(&journal, 3);
        assert_eq!(garde.lines().count(), 3);
        assert!(garde.contains("utile-9") && garde.contains("utile-7"));
        assert!(!garde.contains("utile-6"), "seules les trois dernières");
        assert!(!garde.contains("bruit"));
    }

    /// Une trace d'erreur suit sa ligne d'en-tête : la découper en deux
    /// vaudrait moins que de la jeter entière.
    #[test]
    fn une_trace_suit_la_ligne_qui_la_porte() {
        let journal = "2026-08-17T10:00:00Z ERROR m: panic\n    at src/lib.rs:12\n    at src/main.rs:3\n\
                       2026-08-17T10:00:01Z DEBUG m: sonde\n    detail de la sonde\n";
        let garde = lignes_utiles_pour_un_rapport(journal, 200);
        assert!(
            garde.contains("at src/lib.rs:12"),
            "la trace de l'ERROR reste"
        );
        assert!(garde.contains("at src/main.rs:3"));
        assert!(!garde.contains("detail de la sonde"), "celle du DEBUG part");
    }

    /// Un format inattendu ne doit pas vider le rapport : sans niveau
    /// reconnu, on garde.
    #[test]
    fn un_journal_sans_niveau_reconnu_est_conserve() {
        let journal = "ligne sans niveau\nune autre\n";
        let garde = lignes_utiles_pour_un_rapport(journal, 200);
        assert_eq!(garde.lines().count(), 2);
    }

    /// Le niveau se lit dans les premiers champs — pas au milieu du message,
    /// sans quoi une ligne parlant de « DEBUG » serait jetée.
    #[test]
    fn le_mot_debug_dans_un_message_ne_compte_pas() {
        assert_eq!(
            niveau_de_ligne("2026-08-17T10:00:00Z  INFO m: log_level=DEBUG applique"),
            Some("INFO")
        );
        assert_eq!(
            niveau_de_ligne("2026-08-17T10:00:00Z DEBUG m: coucou"),
            Some("DEBUG")
        );
        assert_eq!(niveau_de_ligne("    at src/lib.rs:12"), None);

        let journal = "2026-08-17T10:00:00Z  INFO m: log_level=DEBUG applique\n";
        assert!(lignes_utiles_pour_un_rapport(journal, 10).contains("log_level=DEBUG"));
    }

    #[test]
    fn la_periode_couverte_est_annoncee() {
        let j = "2026-08-20T09:03:15.059+02:00  INFO a: debut\n\
                 2026-08-20T09:08:00.000+02:00  WARN a: milieu\n\
                 2026-08-20T09:13:13.491+02:00  INFO a: fin\n";
        assert_eq!(
            periode_couverte(j).unwrap(),
            "du 2026-08-20T09:03 au 2026-08-20T09:13"
        );
    }

    #[test]
    fn une_seule_minute_ne_sannonce_pas_comme_un_intervalle() {
        let j = "2026-08-20T09:03:15.059+02:00  INFO a: seule\n";
        assert_eq!(periode_couverte(j).unwrap(), "à 2026-08-20T09:03");
        let deux = "2026-08-20T09:03:15.059+02:00  INFO a: une\n\
                    2026-08-20T09:03:59.000+02:00  INFO a: deux\n";
        assert_eq!(periode_couverte(deux).unwrap(), "à 2026-08-20T09:03");
    }

    #[test]
    fn un_journal_sans_horodatage_ne_promet_aucune_periode() {
        // Un format inattendu ne doit pas produire une période inventée : mieux
        // vaut ne rien annoncer que d'annoncer faux.
        assert!(periode_couverte("Tune Server 0.9.90 | windows\n=====\n").is_none());
        assert!(periode_couverte("").is_none());
        assert!(horodatage_de_ligne("    at src/lib.rs:12").is_none());
        assert!(horodatage_de_ligne("2026-08-20 09:03:15 INFO a: espace au lieu de T").is_none());
    }

    /// Le cas qui a motivé #2028 : trois mille lignes qui ne couvrent que dix
    /// minutes. Rien ne le disait, et l'en-tête laissait croire à un journal
    /// représentatif.
    #[test]
    fn dix_minutes_de_scan_sannoncent_comme_dix_minutes() {
        let mut j = String::new();
        for i in 0..600 {
            j.push_str(&format!(
                "2026-08-20T09:{:02}:{:02}.000+02:00  INFO tune_server::scan_import: DIAG\n",
                3 + i / 60,
                i % 60
            ));
        }
        assert_eq!(
            periode_couverte(&j).unwrap(),
            "du 2026-08-20T09:03 au 2026-08-20T09:12"
        );
    }
}

/// #1974 — l'export de journaux était noyé par la découverte SSDP.
///
/// Deux exports successifs de Bilou (fil forum 1479, 0.9.88 · Windows) :
/// 529 puis **562 lignes de SSDP sur 1003**. Le second ne contenait AUCUNE
/// ligne d'embedding, alors que l'analyse acoustique était l'objet même du
/// signalement. Il avait fourni le bon fichier, au bon moment, et il était
/// inexploitable.
#[cfg(test)]
mod selection_de_lignes {
    use super::*;

    fn ligne(module: &str, n: usize) -> String {
        format!("2026-08-20T10:00:00.000+02:00  INFO {module}: message {n}")
    }

    /// Reproduit la proportion mesurée : 562 lignes de SSDP, une poignée du
    /// sujet, et le reste réparti. Avant, la fenêtre de 1000 les avalait.
    fn journal_de_bilou() -> Vec<String> {
        let mut v = Vec::new();
        // L'embedding écrit une ligne par lot — une toutes les quinze minutes.
        // Elles sont donc ANCIENNES, et c'est précisément ce qui les
        // condamnait : la troncature garde la fin.
        for i in 0..4 {
            v.push(ligne("tune_core::audio::embedding", i));
        }
        for i in 0..151 {
            v.push(ligne("tune_core::metadata::matcher", i));
        }
        for i in 0..1400 {
            v.push(ligne("tune_core::discovery::ssdp", i));
        }
        v
    }

    #[test]
    fn le_module_est_lu_dans_la_ligne() {
        assert_eq!(
            module_de_la_ligne(&ligne("tune_core::discovery::ssdp", 1)),
            Some("tune_core::discovery::ssdp")
        );
        // Une continuation de message multiligne, une trace de panique : pas de
        // module, donc jamais écartée.
        assert_eq!(module_de_la_ligne("    at src/main.rs:42"), None);
        assert_eq!(module_de_la_ligne(""), None);
        // Un mot isolé suivi de « : » n'est pas un module — sans l'exigence du
        // `::`, un message commençant par « erreur: » compterait pour un module
        // à lui tout seul et se ferait rationner.
        assert_eq!(
            module_de_la_ligne("2026-08-20T10:00:00.000+02:00  WARN erreur: ceci"),
            None
        );
    }

    #[test]
    fn le_signalement_de_bilou_ne_disparait_plus() {
        let (retenues, ecartees) = selectionner_lignes(journal_de_bilou(), 1000);

        assert_eq!(retenues.len(), 1000, "la fenêtre doit rester pleine");

        let compte = |m: &str| retenues.iter().filter(|l| l.contains(m)).count();
        // LE point du ticket : les quatre lignes d'embedding survivent.
        assert_eq!(
            compte("audio::embedding"),
            4,
            "les lignes du sujet signalé ont de nouveau disparu"
        );
        // Et SSDP ne peut plus prendre plus du quart... en première passe.
        // Il en reprend ensuite, faute d'autre chose à montrer — c'est voulu.
        assert!(
            compte("discovery::ssdp") < 1400,
            "SSDP occupe encore toute la fenêtre"
        );
        assert!(!ecartees.is_empty(), "rien n'a été mis de côté ?");
    }

    /// La troncature simple est le point de comparaison : avec la même fenêtre,
    /// elle perdait tout du sujet. Ce test échouerait sur l'ancien code.
    #[test]
    fn la_troncature_simple_perdait_tout() {
        let journal = journal_de_bilou();
        let ancienne: Vec<&String> = journal.iter().rev().take(1000).collect();
        assert_eq!(
            ancienne
                .iter()
                .filter(|l| l.contains("audio::embedding"))
                .count(),
            0,
            "le journal d'essai ne reproduit pas le défaut : revoir les proportions"
        );
    }

    /// Garde-fou de non-régression, et le plus important des trois : on ne rend
    /// JAMAIS moins de lignes qu'avant. Sur une machine où seul SSDP parle, le
    /// quota ne doit rien retirer — il n'y a rien d'autre à montrer.
    #[test]
    fn un_seul_module_bavard_reste_entier() {
        let journal: Vec<String> = (0..3000)
            .map(|i| ligne("tune_core::discovery::ssdp", i))
            .collect();
        let (retenues, ecartees) = selectionner_lignes(journal, 1000);
        assert_eq!(retenues.len(), 1000);
        assert!(
            ecartees.is_empty(),
            "des lignes ont été perdues alors qu'il n'y avait rien à leur préférer"
        );
    }

    #[test]
    fn l_ordre_chronologique_est_conserve() {
        let mut journal = Vec::new();
        for i in 0..50 {
            journal.push(ligne("a::b", i));
            journal.push(ligne("c::d", i));
        }
        let (retenues, _) = selectionner_lignes(journal.clone(), 40);
        // Les retenues doivent apparaître dans le même ordre relatif que dans
        // le journal : un export dont les lignes sont mélangées ne se lit pas.
        let positions: Vec<usize> = retenues
            .iter()
            .map(|l| journal.iter().position(|j| j == l).unwrap())
            .collect();
        let mut triees = positions.clone();
        triees.sort_unstable();
        assert_eq!(positions, triees);
    }

    #[test]
    fn moins_de_candidats_que_demande_rend_tout() {
        let journal: Vec<String> = (0..10).map(|i| ligne("a::b", i)).collect();
        let (retenues, ecartees) = selectionner_lignes(journal.clone(), 1000);
        assert_eq!(retenues, journal);
        assert!(ecartees.is_empty());
    }

    #[test]
    fn une_fenetre_nulle_ne_panique_pas() {
        let (retenues, _) = selectionner_lignes(journal_de_bilou(), 0);
        assert!(retenues.is_empty());
    }

    // --- #2028, dernier volet : le rapport hérite du quota par module ---

    fn ligne_de(module: &str, n: usize) -> String {
        format!(
            "2026-08-20T09:03:{:02}.000+02:00  INFO {module}: evenement n={n}",
            n % 60
        )
    }

    /// Le cœur du défaut : chez Bilou, 311 lignes de `scan_import` et 322 de
    /// `metadata` ne laissaient AUCUNE ligne d'enrichissement dans le rapport
    /// — alors que l'enrichissement était l'objet de son signalement.
    #[test]
    fn le_bavard_ne_chasse_plus_la_ligne_qui_compte_du_rapport() {
        let mut journal = String::new();
        for i in 0..311 {
            journal.push_str(&ligne_de("tune_server::scan_import", i));
            journal.push('\n');
        }
        for i in 0..322 {
            journal.push_str(&ligne_de("tune_core::metadata", i));
            journal.push('\n');
        }
        journal.push_str(
            "2026-08-20T09:13:00.000+02:00  INFO tune_core::enrichment: batch_artist_mbid_match_started count=7837\n",
        );

        let rapport = lignes_utiles_pour_un_rapport(&journal, 200);
        assert!(
            rapport.contains("batch_artist_mbid_match_started"),
            "la ligne rare doit survivre au vacarme"
        );
    }

    /// Le décompte ne rapporte QUE le déplacement — ce que le quota a coûté à
    /// d'autres — et pas le débordement de fenêtre, qui est son fonctionnement
    /// normal. Il faut donc un vrai cas de sauvetage : un module ancien que le
    /// bavard aurait entièrement chassé, et que le quota ramène.
    #[test]
    fn le_rapport_dit_ce_que_le_quota_a_deplace() {
        let mut journal = String::new();
        // Anciennes, et hors de la fenêtre simple : elles n'y seraient jamais.
        for i in 0..50 {
            journal.push_str(&ligne_de("tune_core::orchestrator", i));
            journal.push('\n');
        }
        // Récentes, assez nombreuses pour remplir la fenêtre à elles seules.
        for i in 0..300 {
            journal.push_str(&ligne_de("tune_server::scan_import", i));
            journal.push('\n');
        }

        let rapport = lignes_utiles_pour_un_rapport(&journal, 200);
        assert!(
            rapport.contains("tune_core::orchestrator"),
            "le module ancien doit être sauvé par son quota"
        );
        assert!(rapport.contains("écartées du rapport"), "{rapport}");
        assert!(
            rapport.contains("tune_server::scan_import"),
            "et c'est le bavard qui a cédé la place : {rapport}"
        );
        assert!(rapport.contains("export complet"), "{rapport}");
    }

    #[test]
    fn un_journal_calme_traverse_le_rapport_sans_rien_perdre() {
        // Le quota ne doit pas s'inviter là où personne ne monopolise rien :
        // à taille égale, le rapport ne dit jamais moins qu'avant.
        let mut journal = String::new();
        for i in 0..20 {
            journal.push_str(&ligne_de("tune_core::orchestrator", i));
            journal.push('\n');
        }
        let rapport = lignes_utiles_pour_un_rapport(&journal, 200);
        assert_eq!(rapport.lines().count(), 20);
        assert!(!rapport.contains("écartées"), "rien à annoncer : {rapport}");
    }

    // --- #3580 : le quota d'un module se depensait sur sa derniere rafale ---

    /// Une ligne telle que `tracing` l'ecrit : module, puis EVENEMENT, puis
    /// les champs. C'est la forme reelle des journaux de terrain — celle que
    /// `ligne_de` ci-dessus ne reproduit pas (son message est une phrase).
    fn ligne_evt(module: &str, evenement: &str, n: usize) -> String {
        format!(
            "2026-09-04T10:{:02}:{:02}.000+02:00  INFO {module}: {evenement} zone_id=5 n={n}",
            21 + n / 60,
            n % 60
        )
    }

    #[test]
    fn l_evenement_est_lu_dans_la_ligne() {
        assert_eq!(
            evenement_de_la_ligne(&ligne_evt(
                "tune_core::orchestrator",
                "initial_prebuffer_done",
                1
            )),
            Some("initial_prebuffer_done")
        );
        // Un message redige en phrase n'est PAS un evenement : le compter
        // comme tel le ferait rationner sous un nom qui n'existe pas.
        assert_eq!(
            evenement_de_la_ligne(
                "2026-09-04T10:35:00.000+02:00  WARN tune_core::outputs::dlna: Le renderer a acquitte"
            ),
            None
        );
        // Ni un mot unique sans `_` : trop de messages commencent ainsi.
        assert_eq!(
            evenement_de_la_ligne(
                "2026-09-04T10:35:00.000+02:00  INFO tune_core::poller: playing zone_id=5"
            ),
            None
        );
        // Une ligne sans module n'a pas d'evenement non plus.
        assert_eq!(evenement_de_la_ligne("    at src/main.rs:42"), None);
    }

    /// Le journal de Reivax66 (ticket support 78, #3580), dans ses proportions
    /// MESUREES sur le `diagnostic.md` recu : fenetre de 200 lignes couvrant
    /// 10:21 -> 10:42, `tune_core::orchestrator` retenu a exactement 50 lignes
    /// et **60 ecartees** — et sur les 50 retenues, **39 etaient trois
    /// evenements repetes** tires de deux rafales de quelques centaines de ms.
    ///
    /// Ce que le rapport a donc jete : la chaine de decision des TROIS cycles
    /// de lecture qui ont echoue (`radio_proxy_transcode_for_dlna`,
    /// `initial_prebuffer_done`, `output_play_failed`). Sans elle, on ne peut
    /// pas savoir ce que contenait l'URI envoyee au Denon ni a quel moment —
    /// c'est-a-dire exactement la question du ticket.
    fn journal_de_reivax66() -> Vec<String> {
        let mut v = Vec::new();

        // --- Les 60 plus ANCIENNES lignes du module : c'est ce que le
        // rapport a jete. Trois cycles de lecture qui echouent, noyes dans le
        // bavardage de la meme periode.
        for cycle in 0..3usize {
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "radio_proxy_transcode_for_dlna",
                cycle,
            ));
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "initial_prebuffer_done",
                cycle,
            ));
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "output_play_failed",
                cycle,
            ));
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "orchestrator_play",
                cycle,
            ));
        }
        for i in 0..48 {
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "radio_local_decode_started",
                i,
            ));
        }

        // --- Les 50 plus RECENTES : celles que le quota gardait, et dont 39
        // sont trois evenements repetes, tires de deux rafales.
        for i in 0..13 {
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "radio_local_decode_stream_connected",
                i,
            ));
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "radio_local_decode_started",
                100 + i,
            ));
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "orchestrator_play_retap_deduped_same_inflight_track",
                i,
            ));
        }
        for i in 0..2 {
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "initial_prebuffer_done",
                50 + i,
            ));
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "orchestrator_play",
                50 + i,
            ));
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "output_play_sent",
                50 + i,
            ));
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "playback_timing",
                50 + i,
            ));
        }
        for i in 0..3 {
            v.push(ligne_evt(
                "tune_core::orchestrator",
                "radio_proxy_transcode_for_dlna",
                50 + i,
            ));
        }

        // --- Le reste de la fenetre : deux modules qui, eux, la remplissent
        // largement. Sans eux le quota ne mordrait pas, et le defaut ne se
        // reproduirait pas.
        for i in 0..100 {
            v.push(ligne_evt(
                "tune_core::outputs::local",
                "local_audio_device_found",
                i,
            ));
        }
        for i in 0..200 {
            v.push(ligne_evt(
                "tune_core::http::streamer",
                "radio_stream_session_created",
                i,
            ));
        }
        v
    }

    /// Compte les lignes du rapport dont l'EVENEMENT est exactement `e`.
    ///
    /// `contains` ne suffit pas : `orchestrator_play` est un prefixe de
    /// `orchestrator_play_retap_deduped_same_inflight_track`, et un temoin qui
    /// confond les deux ne mesure rien.
    fn compte_evenement(rapport: &str, e: &str) -> usize {
        rapport
            .lines()
            .filter(|l| evenement_de_la_ligne(l) == Some(e))
            .count()
    }

    #[test]
    fn le_rapport_de_reivax66_garde_la_chaine_de_decision() {
        let rapport = lignes_utiles_pour_un_rapport(&journal_de_reivax66().join("\n"), 200);

        // Le budget du module est INCHANGE : 50 lignes, son quota. Ce temoin
        // n'achete rien avec des lignes en plus — il depense les memes
        // autrement.
        assert_eq!(
            rapport
                .lines()
                .filter(|l| module_de_la_ligne(l) == Some("tune_core::orchestrator"))
                .count(),
            50,
            "le quota du module a bouge, ce n'est plus la meme mesure :\n{rapport}"
        );

        // LE point du ticket : sans ces trois evenements, on ne peut pas dire
        // ce que Tune a envoye au renderer, ni a quel moment.
        assert_eq!(
            compte_evenement(&rapport, "output_play_failed"),
            3,
            "les trois echecs de lecture sont de nouveau absents du rapport :\n{rapport}"
        );
        assert_eq!(
            compte_evenement(&rapport, "initial_prebuffer_done"),
            5,
            "le prebuffer des cycles en echec manque :\n{rapport}"
        );
        assert_eq!(
            compte_evenement(&rapport, "radio_proxy_transcode_for_dlna"),
            6,
            "l'origine des flux servis au renderer manque :\n{rapport}"
        );
    }

    /// Le garde-fou du cran supplementaire, et le plus important des deux :
    /// un module qui n'atteint PAS son quota ne doit perdre aucune ligne, meme
    /// quand toutes ses lignes sont le meme evenement.
    ///
    /// Le cas est reel : dans le meme rapport, `tune_core::outputs::dlna` tient
    /// 21 lignes pour un quota de 50, dont **8 `dlna_play_acquitte_mais_pas_
    /// applique_relance`** — les lignes qui NOMMENT le defaut. Un plafond par
    /// evenement applique sans reprise en aurait supprime deux.
    #[test]
    fn aucun_module_ne_perd_de_ligne_par_le_quota_evenement() {
        let mut v = Vec::new();
        for i in 0..8 {
            v.push(ligne_evt(
                "tune_core::outputs::dlna",
                "dlna_play_acquitte_mais_pas_applique_relance",
                i,
            ));
        }
        for i in 0..13 {
            v.push(ligne_evt("tune_core::outputs::dlna", "dlna_set_uri_ok", i));
        }
        // Un bavard a cote, pour que la fenetre soit effectivement disputee.
        for i in 0..400 {
            v.push(ligne_evt(
                "tune_core::http::streamer",
                "radio_stream_session_created",
                i,
            ));
        }
        let rapport = lignes_utiles_pour_un_rapport(&v.join("\n"), 200);
        assert_eq!(
            rapport
                .lines()
                .filter(|l| l.contains("dlna_play_acquitte_mais_pas_applique_relance"))
                .count(),
            8,
            "le quota par evenement a mange des lignes d'un module qui n'avait \
             pas epuise le sien :\n{rapport}"
        );
    }

    #[test]
    fn le_niveau_filtre_toujours_avant_le_quota() {
        // L'ordre compte : plafonner d'abord laisserait du DEBUG occuper un
        // quota au détriment d'un WARN.
        let journal = "2026-08-20T09:03:00.000+02:00 DEBUG tune_core::discovery::ssdp: sonde a=1\n\
                       2026-08-20T09:03:01.000+02:00  WARN tune_core::outputs::bluos: add_rejected b=2\n";
        let rapport = lignes_utiles_pour_un_rapport(journal, 200);
        assert!(rapport.contains("add_rejected"));
        assert!(!rapport.contains("sonde a=1"));
    }
}

/// #2392 — la section « fournisseurs de sortie » du rapport de bogue.
#[cfg(test)]
mod fournisseurs_de_sortie {
    use super::*;

    /// #2392 : le rapport de bogue doit dire pourquoi un fournisseur payant est
    /// inerte. C'est le canal qui aurait épargné au bêta-testeur du module
    /// Diretta une réinstallation complète de son système d'exploitation.
    #[test]
    fn le_rapport_dit_quand_un_module_paye_est_inerte_faute_de_compte_lie() {
        let instantane = serde_json::json!({
            "account_linked": false,
            "licensed_modules": [],
            "providers": [{
                "provider": "diretta",
                "required_module": "diretta",
                "devices": 0,
                "refusal": {
                    "code": "module_account_not_linked",
                    "message": "link your Mozaiklabs account",
                },
            }],
        });
        let md = section_fournisseurs_de_sortie(&instantane);
        assert!(md.contains("No linked Mozaiklabs account"), "{md}");
        assert!(md.contains("Licensed modules: none"), "{md}");
        assert!(
            md.contains("diretta: **idle — module_account_not_linked**"),
            "{md}"
        );
    }

    /// Droit présent mais rien sur le réseau : l'autre cas, et il doit se lire
    /// différemment — sinon on n'a fait que déplacer l'ambiguïté.
    #[test]
    fn le_rapport_distingue_un_module_actif_qui_ne_trouve_rien() {
        let instantane = serde_json::json!({
            "account_linked": true,
            "licensed_modules": ["diretta"],
            "providers": [{
                "provider": "diretta",
                "required_module": "diretta",
                "devices": 0,
                "refusal": null,
            }],
        });
        let md = section_fournisseurs_de_sortie(&instantane);
        assert!(!md.contains("No linked Mozaiklabs account"), "{md}");
        assert!(md.contains("Licensed modules: diretta"), "{md}");
        assert!(md.contains("diretta: active, 0 device(s)"), "{md}");
    }

    /// Aucun fournisseur hors-arbre (le cas du binaire public, et l'état avant
    /// la première passe) : pas de section du tout, pas de bruit.
    #[test]
    fn aucun_fournisseur_hors_arbre_najoute_aucune_section() {
        assert_eq!(section_fournisseurs_de_sortie(&Value::Null), "");
        assert_eq!(
            section_fournisseurs_de_sortie(&serde_json::json!({ "providers": [] })),
            ""
        );
    }
}

/// #3182 — « inconnue » n'est pas `0`.
#[cfg(test)]
mod version_de_schema_rendue {
    use super::*;

    /// Le distinguo qui fait tout le défaut : une version illisible s'écrit en
    /// toutes lettres, jamais en `0`. `0` est une version PLAUSIBLE — celle
    /// d'une base neuve jamais migrée — et c'est exactement ainsi que le
    /// rapport de jfpaquet a été lu sur sa base de 77 291 pistes.
    #[test]
    fn une_version_illisible_ne_se_rend_pas_en_zero() {
        let rendu = version_de_schema_affichee(None);
        assert_ne!(rendu, "0");
        assert_eq!(rendu, VERSION_DE_SCHEMA_INCONNUE);
        // Et le rendu ne doit pas être un nombre : un lecteur qui compare
        // « la version annoncée » à un numéro attendu doit buter dessus.
        assert!(
            rendu.parse::<i64>().is_err(),
            "« {rendu} » se lit comme un numéro de migration"
        );
    }

    /// La contre-épreuve : une version connue se rend telle quelle, `0`
    /// compris. Une base SQLite neuve EST à la version 0, et le rapport doit
    /// pouvoir le dire — c'est la lecture, pas le chiffre, qui était fausse.
    #[test]
    fn une_version_connue_se_rend_telle_quelle() {
        assert_eq!(version_de_schema_affichee(Some(0)), "0");
        assert_eq!(version_de_schema_affichee(Some(49)), "49");
    }
}

#[cfg(test)]
mod tests_reports_cloud {
    use super::rapport_des_reports_cloud;
    use tune_core::cloud::rate_limit::ActiveCloudBackoff;

    /// CLD-3 : chaque portée retenue est nommée avec son échéance et le temps
    /// restant ; une échéance passée rend zéro, jamais un négatif ; sans
    /// report, `count` vaut zéro et la liste existe (le client n'a pas à
    /// deviner l'absence).
    #[test]
    fn le_rapport_nomme_les_portees_retenues_et_borne_le_restant() {
        let maintenant = 1_700_000_000u64;
        let actifs = [
            ActiveCloudBackoff {
                scope: "bios_artists_read",
                until_epoch: maintenant + 90,
                retry_after_seconds: 120,
            },
            ActiveCloudBackoff {
                scope: "telemetry",
                until_epoch: maintenant - 5,
                retry_after_seconds: 60,
            },
        ];
        let r = rapport_des_reports_cloud(&actifs, maintenant);
        assert_eq!(r["count"], 2);
        assert_eq!(r["scopes"][0]["scope"], "bios_artists_read");
        assert_eq!(r["scopes"][0]["remaining_seconds"], 90);
        assert_eq!(r["scopes"][0]["retry_after_seconds"], 120);
        assert_eq!(
            r["scopes"][1]["remaining_seconds"], 0,
            "une échéance passée n'est pas une dette"
        );

        let vide = rapport_des_reports_cloud(&[], maintenant);
        assert_eq!(vide["count"], 0);
        assert!(vide["scopes"].as_array().is_some_and(|v| v.is_empty()));
    }
}

#[cfg(test)]
mod tests_doublons_de_zones {
    use super::{ZoneVue, cle_appareil, doublons_de_zones};
    use tune_core::discovery::device::{DiscoveredDevice, OutputType};

    fn zone(id: i64, name: &str, t: &str, dev: &str, online: bool) -> ZoneVue {
        ZoneVue {
            id,
            name: name.into(),
            output_type: t.into(),
            output_device_id: dev.into(),
            online,
        }
    }

    /// DUP-1 (phase 2) : « remplacée » ne vient que d'une jumelle en ligne.
    /// Le Sonos hors ligne dont l'UDN racine est en ligne est probablement
    /// remplacé ; deux zones d'un même appareil toutes deux hors ligne ne le
    /// sont pas ; une zone seule n'apparaît même pas.
    #[test]
    fn remplacee_probable_ne_vient_que_d_une_jumelle_en_ligne() {
        let zones = vec![
            zone(
                6,
                "Chambre",
                "dlna",
                "uuid:RINCON_B8E937B44D0801400_MR",
                false,
            ),
            zone(
                8,
                "Chambre - Sonos",
                "dlna",
                "uuid:RINCON_B8E937B44D0801400",
                true,
            ),
            zone(30, "Bureau", "dlna", "uuid:BUREAU_MR", false),
            zone(31, "Bureau - Node", "dlna", "uuid:BUREAU", false),
            zone(12, "Lindemann", "dlna", "uuid:LINDEMANN", false),
        ];
        let groupes = doublons_de_zones(&zones, &[]);
        let drapeau = |id: i64| {
            groupes
                .iter()
                .flat_map(|g| g["zones"].as_array().cloned().unwrap_or_default())
                .find(|z| z["id"].as_i64() == Some(id))
                .map(|z| z["remplacee_probable"].as_bool().unwrap_or(false))
        };
        assert_eq!(
            drapeau(6),
            Some(true),
            "hors ligne, jumelle en ligne : remplacée probable"
        );
        assert_eq!(drapeau(8), Some(false), "la jumelle en ligne ne l'est pas");
        assert_eq!(
            drapeau(30),
            Some(false),
            "deux zones hors ligne : éteintes, pas remplacées"
        );
        assert_eq!(drapeau(31), Some(false));
        assert_eq!(drapeau(12), None, "une zone seule n'est pas un doublon");
    }

    /// La mesure du 05/09 sur .18, rejouée : le Sonos (UDN et UDN `_MR`), le
    /// Mac (identifiant IP historique et adresse matérielle), l'Eversolo en
    /// DLNA et en AirPlay ; le Lindemann et le décodeur restent seuls.
    #[test]
    fn les_doublons_de_dix_huit_sont_nommes_et_les_zones_seules_laissees() {
        let zones = vec![
            zone(
                6,
                "Chambre",
                "dlna",
                "uuid:RINCON_B8E937B44D0801400_MR",
                false,
            ),
            zone(
                8,
                "Chambre - Sonos Play:1",
                "dlna",
                "uuid:RINCON_B8E937B44D0801400",
                true,
            ),
            zone(
                20,
                "Mac Studio",
                "airplay",
                "airplay-76:4D:00:C0:BD:51",
                false,
            ),
            zone(4, "Mac13,1", "airplay", "airplay-192.168.1.41-7000", true),
            zone(
                10,
                "Eversolo DMP-A8",
                "dlna",
                "uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE",
                true,
            ),
            zone(
                2,
                "eversolo,1",
                "airplay",
                "airplay-192.168.1.17-5500",
                true,
            ),
            zone(
                13,
                "Lindemann",
                "dlna",
                "uuid:e92cc83b-3083-4239-9b17-1026d9344dcc",
                false,
            ),
            zone(
                17,
                "Décodeur TV UHD",
                "dlna",
                "uuid:00ababad-7947-1048-8a00-5cb13ebb9dd4",
                true,
            ),
            zone(15, "Cet ordinateur", "browser", "", true),
        ];
        let mut mac = DiscoveredDevice::new(
            "airplay-76:4D:00:C0:BD:51".into(),
            "Mac Studio".into(),
            OutputType::Airplay,
            "192.168.1.41".into(),
            7000,
        );
        mac.mac_address = Some("76:4D:00:C0:BD:51".into());
        let eversolo = DiscoveredDevice::new(
            "uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE".into(),
            "Eversolo".into(),
            OutputType::Dlna,
            "192.168.1.17".into(),
            49152,
        );
        let appareils = vec![mac, eversolo];

        let groupes = doublons_de_zones(&zones, &appareils);
        let ids = |g: &serde_json::Value| -> Vec<i64> {
            g["zones"]
                .as_array()
                .unwrap()
                .iter()
                .map(|z| z["id"].as_i64().unwrap())
                .collect()
        };
        assert_eq!(groupes.len(), 3, "{groupes:#?}");
        assert!(
            groupes[0]["motif"]
                .as_str()
                .unwrap()
                .contains("adresse matérielle")
        );
        assert_eq!(ids(&groupes[0]), [20, 4]);
        assert_eq!(groupes[0]["en_ligne"], 1);
        assert!(groupes[1]["motif"].as_str().unwrap().contains("UDN"));
        assert_eq!(ids(&groupes[1]), [6, 8]);
        assert_eq!(groupes[2]["motif"], "même hôte, deux protocoles");
        assert_eq!(ids(&groupes[2]), [10, 2]);
        let tous: Vec<i64> = groupes.iter().flat_map(ids).collect();
        for seul in [13, 17, 15] {
            assert!(!tous.contains(&seul), "la zone {seul} est seule");
        }
    }

    /// La clé d'appareil : `_MR` et `uuid:` s'effacent, l'adresse IP se résout
    /// en adresse matérielle quand un appareil découvert la porte, sinon reste
    /// une adresse ; une sortie locale n'a pas de clé.
    #[test]
    fn la_cle_d_appareil_retire_ce_qui_n_identifie_rien() {
        let z = |dev: &str| zone(1, "z", "dlna", dev, true);
        assert_eq!(
            cle_appareil(&z("uuid:RINCON_ABC_MR"), &[]).as_deref(),
            Some("udn:rincon_abc")
        );
        assert_eq!(
            cle_appareil(&z("uuid:RINCON_ABC"), &[]).as_deref(),
            Some("udn:rincon_abc")
        );
        assert_eq!(
            cle_appareil(&z("airplay-AA:BB:CC:DD:EE:FF"), &[]).as_deref(),
            Some("mac:aa:bb:cc:dd:ee:ff")
        );
        assert_eq!(
            cle_appareil(&z("airplay-192.168.1.37-7000"), &[]).as_deref(),
            Some("ip:192.168.1.37")
        );
        assert_eq!(cle_appareil(&z("local:hw:0,0"), &[]), None);
        assert_eq!(cle_appareil(&z("oaat:1081bb7a"), &[]), None);
    }
    /// Les TROIS UDN d'un Sonos ne font qu'un appareil, donc un seul groupe.
    ///
    /// Relevé du 09/09 sur .18 (`tune_v2.db`) : « Chambre » existe en racine
    /// (id 8), en `_MR` (id 6) et en `_MS` (id 9) ; « Cuisine » en 7, 11 et
    /// 12. Tant que seul `_MR` était retiré, la ligne `_MS` n'était ni nommée
    /// par le rapport ni fusionnable par la route : elle restait à l'écran
    /// sans aucun moyen de la faire disparaître sans perdre ses réglages.
    #[test]
    fn les_trois_udn_d_un_sonos_ne_font_qu_un_seul_groupe() {
        let z = |dev: &str| zone(1, "z", "dlna", dev, true);
        for suffixe in ["", "_MR", "_MS"] {
            assert_eq!(
                cle_appareil(&z(&format!("uuid:RINCON_B8E937B44D0801400{suffixe}")), &[])
                    .as_deref(),
                Some("udn:rincon_b8e937b44d0801400"),
                "suffixe {suffixe:?}"
            );
        }
        // Un suffixe qui n'est pas un sous-appareil Sonos ne s'efface pas :
        // deux appareils différents ne doivent pas se retrouver dans le même
        // groupe parce que leur UDN finit pareil.
        assert_eq!(
            cle_appareil(&z("uuid:ABC_MZ"), &[]).as_deref(),
            Some("udn:abc_mz")
        );
        let zones = vec![
            zone(8, "Chambre", "dlna", "uuid:RINCON_B8E937B44D0801400", true),
            zone(
                6,
                "Chambre",
                "dlna",
                "uuid:RINCON_B8E937B44D0801400_MR",
                false,
            ),
            zone(
                9,
                "Chambre - Sonos Play:1 Media Renderer",
                "dlna",
                "uuid:RINCON_B8E937B44D0801400_MS",
                false,
            ),
        ];
        let groupes = doublons_de_zones(&zones, &[]);
        assert_eq!(groupes.len(), 1, "{groupes:#?}");
        let mut ids: Vec<i64> = groupes[0]["zones"]
            .as_array()
            .unwrap()
            .iter()
            .map(|z| z["id"].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, [6, 8, 9], "les trois lignes sont un seul appareil");
    }

    /// La garde de `fusionner_zones`, recalculée ici : les deux zones doivent
    /// rendre la MÊME clé d'appareil, non nulle. Tout le reste est
    /// `409 zones_distinctes`.
    ///
    /// Le témoin ne relit donc pas le drapeau qu'il vient de poser : il
    /// compare `fusionnable` à ce que la ROUTE ferait.
    fn la_route_accepterait(groupe: &serde_json::Value, appareils: &[DiscoveredDevice]) -> bool {
        let zones: Vec<ZoneVue> = groupe["zones"]
            .as_array()
            .unwrap()
            .iter()
            .map(|z| {
                zone(
                    z["id"].as_i64().unwrap(),
                    z["name"].as_str().unwrap(),
                    z["output_type"].as_str().unwrap(),
                    z["output_device_id"].as_str().unwrap(),
                    z["online"].as_bool().unwrap(),
                )
            })
            .collect();
        let cles: Vec<Option<String>> = zones.iter().map(|z| cle_appareil(z, appareils)).collect();
        cles[0].is_some() && cles.iter().all(|c| *c == cles[0])
    }

    /// #3747 — le rapport ne propose plus une fusion que la route refusera.
    ///
    /// Mesuré le 09/09 : un Eversolo vu en DLNA (SSDP, `uuid:…`) et en AirPlay
    /// (mDNS, `airplay-<MAC>`) sort dans un groupe « même hôte, deux
    /// protocoles ». Les deux zones ne partagent AUCUN identifiant, et la
    /// route répond `409 zones_distinctes` — correctement. Ce qui manquait,
    /// c'est que le rapport le dise AVANT.
    #[test]
    fn un_groupe_de_meme_hote_est_nomme_mais_pas_fusionnable() {
        let zones = vec![
            zone(
                10,
                "Eversolo",
                "dlna",
                "uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE",
                true,
            ),
            zone(
                11,
                "Eversolo",
                "airplay2",
                "airplay-AA:BB:CC:DD:EE:01",
                true,
            ),
        ];
        let dlna = DiscoveredDevice::new(
            "uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE".into(),
            "Eversolo".into(),
            OutputType::Dlna,
            "192.168.1.17".into(),
            49152,
        );
        let airplay = DiscoveredDevice::new(
            "airplay-AA:BB:CC:DD:EE:01".into(),
            "Eversolo".into(),
            OutputType::Airplay,
            "192.168.1.17".into(),
            7000,
        );
        let appareils = vec![dlna, airplay];
        let groupes = doublons_de_zones(&zones, &appareils);
        assert_eq!(groupes.len(), 1, "{groupes:#?}");
        let g = &groupes[0];
        assert!(
            g["cle"].as_str().unwrap().starts_with("hote:"),
            "le groupe attendu est celui de la règle par hôte : {g:#?}"
        );
        assert!(
            !la_route_accepterait(g, &appareils),
            "prémisse du témoin : la route DOIT refuser ce groupe"
        );
        assert_eq!(
            g["fusionnable"],
            serde_json::json!(false),
            "un groupe que la route refuse ne doit pas être annoncé fusionnable : {g:#?}"
        );
        assert!(
            g["fusion_refusee_motif"]
                .as_str()
                .unwrap_or_default()
                .contains("zones_distinctes"),
            "le refus doit être nommé, pas laissé à deviner : {g:#?}"
        );
    }

    /// L'autre sens, et il est indispensable : un groupe RÉELLEMENT
    /// fusionnable reste annoncé fusionnable, et sans motif de refus. Sans ce
    /// témoin, poser `fusionnable: false` partout resterait vert.
    #[test]
    fn un_groupe_de_meme_appareil_reste_fusionnable() {
        let zones = vec![
            zone(8, "Chambre", "dlna", "uuid:RINCON_ABC", true),
            zone(6, "Chambre", "dlna", "uuid:RINCON_ABC_MR", false),
        ];
        let groupes = doublons_de_zones(&zones, &[]);
        assert_eq!(groupes.len(), 1, "{groupes:#?}");
        let g = &groupes[0];
        assert!(
            la_route_accepterait(g, &[]),
            "prémisse du témoin : la route DOIT accepter ce groupe"
        );
        assert_eq!(g["fusionnable"], serde_json::json!(true), "{g:#?}");
        assert_eq!(
            g["fusion_refusee_motif"],
            serde_json::Value::Null,
            "rien à refuser, donc aucun motif : {g:#?}"
        );
    }
}

/// #4845 (JPierre, fil 1903) — toutes les vignettes de la grille Artistes en
/// initiales. La cause n'est pas établie : le rapport de bogue ne disait rien
/// des portraits. Il dit désormais, pour les artistes de la grille, combien
/// sont affichables et, sinon, POURQUOI — les trois causes du même écran.
#[cfg(test)]
mod portraits_d_artistes_4845 {
    use super::{PortraitsDArtistes, ligne_portraits_d_artistes, releve_portraits_d_artistes};
    use tune_core::db::artist_repo::ArtistRepo;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::models::Artist;

    #[test]
    fn le_rapport_distingue_sans_image_cache_perdu_et_affichable_4845() {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let db: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
        let cache = tempfile::tempdir().unwrap();
        let artistes = ArtistRepo::with_backend(db.clone());
        let mut ids = Vec::new();
        for nom in [
            "Annie Lennox",
            "Deep Purple",
            "Diana Krall",
            "Dire Straits",
            "Sans album",
        ] {
            ids.push(artistes.create(&Artist::new(nom.into())).unwrap());
        }
        // Les quatre premiers portent un album (ils sont dans la grille).
        for id in &ids[..4] {
            db.execute_batch(&format!(
                "INSERT INTO albums (title, artist_id) VALUES ('Album {id}', {id});"
            ))
            .unwrap();
        }
        // Annie Lennox : portrait en cache — affichable.
        let octets: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let condensat =
            tune_core::library::artwork::cache_fetched_image(&octets, cache.path(), "jpg").unwrap();
        artistes
            .update_image(ids[0], &condensat, "community")
            .unwrap();
        // Deep Purple : la base annonce une image que le cache n'a plus.
        artistes
            .update_image(ids[1], &"ab".repeat(32), "auto")
            .unwrap();
        // Diana Krall : jamais enrichie. Dire Straits : URL distante.
        artistes
            .update_image(ids[3], "https://exemple.invalid/p.jpg", "auto")
            .unwrap();

        let releve =
            releve_portraits_d_artistes(db.as_ref(), cache.path()).expect("le relevé se lit");
        let mut sources = std::collections::BTreeMap::new();
        sources.insert("auto".to_string(), 2);
        sources.insert("community".to_string(), 1);
        assert_eq!(
            releve,
            PortraitsDArtistes {
                total: 4,
                affichables: 1,
                sans_image: 1,
                cache_perdu: 1,
                distantes: 1,
                sources,
            },
            "#4845 — le rapport doit séparer les trois causes des initiales"
        );
        assert_eq!(
            ligne_portraits_d_artistes(&releve),
            "- Portraits d'artistes (grille Artistes) : 1/4 affichables — sans image 1, \
             cache perdu 1, URL distante 1 ; sources : auto 2, community 1\n"
        );
    }
}
