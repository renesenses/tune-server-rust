//! Liens auto-sync — tranche 4 de l'épique #4715 (#4719).
//!
//! Un **lien** tient deux playlists à jour l'une par l'autre : deux services
//! différents, ou un service et la bibliothèque locale ; dans un sens
//! (`a_vers_b`) ou dans les deux (`deux_sens`). C'est la tranche la plus
//! risquée : elle écrit chez un service **sans geste** de l'utilisateur. Cinq
//! règles la tiennent, chacune gardée par un essai :
//!
//! 1. **Jamais de suppression.** Il n'y en a pas le moyen (l'hôte n'a aucune
//!    capacité de suppression), et le moteur ne le cherche pas : une piste
//!    disparue d'un côté est **signalée** (`disparues`), jamais retirée de
//!    l'autre — et jamais remise non plus du côté d'où l'utilisateur l'a ôtée.
//! 2. **Une synchronisation n'écrit que des AJOUTS**, qu'elle vienne du
//!    minuteur ou d'une demande.
//! 3. **La première synchronisation d'un lien exige un aperçu accepté**, et
//!    n'écrit pas plus que cet aperçu. Avant elle, le minuteur ne touche pas
//!    au lien.
//! 4. **Un snapshot des deux côtés avant chaque synchronisation** (#4718) —
//!    le retour en arrière reste possible. Pas de snapshot, pas d'écriture de
//!    ce côté.
//! 5. **Un journal** : quand, quel déclencheur, quoi, combien, et ce qui a
//!    échoué. Une écriture chez un service doit toujours pouvoir se retracer.
//!
//! Un lien se met en **pause** et se **supprime** sans toucher aux playlists.
//! Le stockage ne sachant pas effacer une clé, « supprimer » un lien le
//! marque `supprime` : il disparaît des listes et du minuteur, ses playlists
//! restent telles quelles.
//!
//! ## Le minuteur
//!
//! L'hôte réveille le greffon toutes les minutes par un événement `minuteur`
//! (`plugin_on_event`), s'il s'y est abonné dans son manifeste. À chaque
//! réveil, **un seul** lien dû est synchronisé — le plus en retard : une
//! synchronisation est bornée en carburant comme tout appel, et en faire
//! trente d'un coup risquerait de n'en finir aucune.
//!
//! ## Stockage
//!
//! | Clé | Contenu |
//! |---|---|
//! | `compteur_liens` | le dernier numéro de lien attribué |
//! | `lien:<id>` | le lien : extrémités, sens, cadence, état, dates |
//! | `lien_corr:<id>` | les correspondances piste A ↔ piste B |
//! | `lien_vus:<id>` | les pistes déjà vues de chaque côté, les introuvables, les disparitions déjà signalées |
//! | `lien_apercu:<id>` | ce que l'utilisateur a accepté avant la première synchronisation |
//! | `lien_journal:<id>:<emplacement>` | une entrée du journal (anneau de [`JOURNAL_RETENTION`]) |

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::appariement::{self, Candidat, Raison};
use crate::hote::Hote;
use crate::snapshots::{LOCAL, PisteSnap, Snapshots, ajouter, lire_playlist};

/// Cadence minimale d'un lien, en minutes. En deçà, on écrirait chez un
/// service plus souvent qu'aucun humain ne retouche une playlist.
pub const CADENCE_MIN_MINUTES: u64 = 15;
/// Cadence maximale : une semaine.
pub const CADENCE_MAX_MINUTES: u64 = 10_080;
/// Entrées de journal gardées par lien (anneau).
pub const JOURNAL_RETENTION: u64 = 50;
/// Détail gardé par entrée de journal, par liste ; les COMPTES restent exacts.
pub const DETAIL_MAX: usize = 200;

/// Le nom de l'événement que l'hôte envoie toutes les minutes.
pub const EVENEMENT_MINUTEUR: &str = "minuteur";

const COMPTEUR_LIENS: &str = "compteur_liens";
const PREFIXE_LIEN: &str = "lien:";

pub mod sens {
    pub const A_VERS_B: &str = "a_vers_b";
    pub const DEUX_SENS: &str = "deux_sens";
}

pub mod etat_lien {
    /// Créé, jamais synchronisé : il attend un aperçu accepté. Le minuteur
    /// ne le touche pas.
    pub const ATTENTE_APERCU: &str = "attente_premier_apercu";
    pub const ACTIF: &str = "actif";
    pub const EN_PAUSE: &str = "en_pause";
    pub const SUPPRIME: &str = "supprime";
}

pub mod declencheur {
    pub const MINUTEUR: &str = "minuteur";
    pub const DEMANDE: &str = "demande";
    pub const PREMIERE: &str = "premiere";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extremite {
    /// `"local"` pour la bibliothèque, sinon le nom d'un service.
    pub service: String,
    pub playlist_id: String,
    #[serde(default)]
    pub nom: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lien {
    pub lien_id: String,
    pub a: Extremite,
    pub b: Extremite,
    pub sens: String,
    /// `0` = à la demande seulement ; sinon entre [`CADENCE_MIN_MINUTES`] et
    /// [`CADENCE_MAX_MINUTES`].
    pub cadence_minutes: u64,
    pub etat: String,
    pub premiere_synchro_faite: bool,
    pub cree_le_ms: u64,
    #[serde(default)]
    pub derniere_synchro_ms: Option<u64>,
    #[serde(default)]
    pub prochaine_synchro_ms: Option<u64>,
    #[serde(default)]
    pub derniere_erreur: Option<String>,
    #[serde(default)]
    pub journal_compteur: u64,
    #[serde(default)]
    pub supprime_le_ms: Option<u64>,
}

/// Ce que l'écran envoie pour créer un lien.
#[derive(Debug, Clone, Deserialize)]
pub struct DemandeLien {
    pub a: Extremite,
    pub b: Extremite,
    #[serde(default)]
    pub sens: Option<String>,
    #[serde(default)]
    pub cadence_minutes: Option<u64>,
}

/// Une piste à ajouter d'un côté, parce qu'elle est de l'autre.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ajout {
    /// Le côté d'où vient la piste (`a` ou `b`).
    pub de: String,
    /// Le côté où elle sera ajoutée.
    pub vers: String,
    pub source_id: String,
    pub titre: String,
    pub artiste: String,
    pub cible_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntrouvableSync {
    pub de: String,
    pub vers: String,
    pub source_id: String,
    pub titre: String,
    pub artiste: String,
    pub raison: Raison,
}

/// Une piste disparue d'un côté et toujours présente de l'autre. **Signalée,
/// jamais retirée** : si l'utilisateur veut qu'elle parte aussi de l'autre
/// côté, c'est à lui de l'y retirer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Disparue {
    /// Le côté d'où elle a disparu.
    pub disparue_de: String,
    pub id_disparu: String,
    /// Le côté où elle est toujours.
    pub toujours_dans: String,
    pub id_restant: String,
    pub titre: String,
    pub artiste: String,
}

/// L'aperçu (ou le calcul) d'une synchronisation. **Aucune suppression n'y
/// figure, par construction** : il n'y a pas de liste « à retirer ».
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSynchro {
    pub lien_id: String,
    pub calcule_le_ms: u64,
    pub ajouts: Vec<Ajout>,
    pub introuvables: Vec<IntrouvableSync>,
    pub disparues: Vec<Disparue>,
    pub deja_presentes: usize,
    /// Pistes connues introuvables et non redemandées (le minuteur ne les
    /// redemande pas à chaque passage ; une synchronisation à la demande, si).
    pub introuvables_connues: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntreeJournal {
    pub numero: u64,
    pub quand_ms: u64,
    pub declencheur: String,
    /// `ok`, `rien_a_faire`, `partiel` (des écritures ont échoué) ou `echec`
    /// (rien n'a pu être lu ou gardé).
    pub statut: String,
    pub ajoutees: usize,
    pub ajouts: Vec<Ajout>,
    pub introuvables: usize,
    pub introuvables_detail: Vec<IntrouvableSync>,
    /// Les disparitions constatées POUR LA PREMIÈRE FOIS lors de cette
    /// synchronisation. Signalées, jamais retirées.
    pub disparues_signalees: Vec<Disparue>,
    pub echecs: Vec<String>,
    /// Les snapshots pris juste avant d'écrire.
    pub snapshots: Vec<String>,
}

/// Les correspondances piste A ↔ piste B, apprises au fil des synchros.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Correspondances {
    ab: BTreeMap<String, String>,
}

/// Ce que le lien a déjà vu. Les ensembles `vus_*` ne font que CROÎTRE : une
/// piste vue une fois d'un côté puis absente, c'est une piste que
/// l'utilisateur a retirée — on ne la remet pas.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Memoire {
    vus_a: BTreeSet<String>,
    vus_b: BTreeSet<String>,
    introuvables_a: BTreeSet<String>,
    introuvables_b: BTreeSet<String>,
    signalees: BTreeSet<String>,
}

/// Ce que l'utilisateur a accepté avant la première synchronisation : les
/// paires `(côté, identifiant)` qu'il a vu annoncer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ApercuAccepte {
    calcule_le_ms: u64,
    ajouts: Vec<(String, String)>,
}

/// Les lectures d'une synchronisation, gardées pour le snapshot.
struct Lectures {
    a: Vec<PisteSnap>,
    b: Vec<PisteSnap>,
}

pub struct Liens<'h, H: Hote + ?Sized> {
    hote: &'h H,
}

impl<'h, H: Hote + ?Sized> Liens<'h, H> {
    pub fn new(hote: &'h H) -> Self {
        Self { hote }
    }

    // -----------------------------------------------------------------------
    // Créer, lire, régler, mettre en pause, supprimer — rien de tout cela
    // n'écrit chez un service.
    // -----------------------------------------------------------------------

    pub fn creer(&self, d: &DemandeLien) -> Result<Lien, String> {
        let sens = d.sens.clone().unwrap_or_else(|| sens::A_VERS_B.into());
        if sens != sens::A_VERS_B && sens != sens::DEUX_SENS {
            return Err(format!(
                "demande_invalide : sens « {sens} » inconnu (a_vers_b ou deux_sens)"
            ));
        }
        for e in [&d.a, &d.b] {
            if e.service.is_empty() || e.playlist_id.is_empty() {
                return Err(
                    "demande_invalide : chaque extrémité veut un service et une playlist_id".into(),
                );
            }
        }
        if d.a.service == d.b.service {
            return Err(
                "demande_invalide : un lien relie deux services DIFFÉRENTS, ou un service et la \
                 bibliothèque locale"
                    .into(),
            );
        }
        let cadence = normaliser_cadence(d.cadence_minutes.unwrap_or(0))?;
        // Les deux côtés doivent se lire MAINTENANT : un lien vers une
        // playlist introuvable échouerait à chaque réveil du minuteur.
        let a = self.completer_extremite(&d.a)?;
        let b = self.completer_extremite(&d.b)?;

        let n = self.compteur()? + 1;
        self.hote.kv_set(COMPTEUR_LIENS, &json!(n))?;
        let lien = Lien {
            lien_id: format!("lien-{n}"),
            a,
            b,
            sens,
            cadence_minutes: cadence,
            etat: etat_lien::ATTENTE_APERCU.into(),
            premiere_synchro_faite: false,
            cree_le_ms: self.hote.maintenant_ms(),
            derniere_synchro_ms: None,
            prochaine_synchro_ms: None,
            derniere_erreur: None,
            journal_compteur: 0,
            supprime_le_ms: None,
        };
        self.ecrire_lien(&lien)?;
        Ok(lien)
    }

    /// Les liens, supprimés exclus, dans l'ordre de création.
    pub fn lister(&self) -> Result<Vec<Lien>, String> {
        let liste = self.hote.kv_list(PREFIXE_LIEN)?;
        let mut liens = Vec::new();
        for cle in liste
            .get("keys")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(cle) = cle.as_str() else { continue };
            let Some(v) = self.kv_valeur(cle)? else {
                continue;
            };
            if let Ok(l) = serde_json::from_value::<Lien>(v)
                && l.etat != etat_lien::SUPPRIME
            {
                liens.push(l);
            }
        }
        liens.sort_by_key(|l| numero(&l.lien_id));
        Ok(liens)
    }

    pub fn lire(&self, lien_id: &str) -> Result<Lien, String> {
        let lien: Lien = self
            .kv_valeur(&format!("{PREFIXE_LIEN}{lien_id}"))?
            .and_then(|v| serde_json::from_value(v).ok())
            .ok_or_else(|| format!("lien_inconnu : {lien_id}"))?;
        if lien.etat == etat_lien::SUPPRIME {
            return Err(format!("lien_inconnu : {lien_id} a été supprimé"));
        }
        Ok(lien)
    }

    pub fn mettre_en_pause(&self, lien_id: &str, pause: bool) -> Result<Lien, String> {
        let mut lien = self.lire(lien_id)?;
        if pause {
            lien.etat = etat_lien::EN_PAUSE.into();
        } else {
            lien.etat = if lien.premiere_synchro_faite {
                etat_lien::ACTIF.into()
            } else {
                etat_lien::ATTENTE_APERCU.into()
            };
            // Au réveil, on repart d'une cadence pleine : pas de rattrapage en
            // rafale de tout ce que la pause a « manqué ».
            lien.prochaine_synchro_ms = self.prochaine(&lien);
        }
        self.ecrire_lien(&lien)?;
        Ok(lien)
    }

    pub fn regler(&self, lien_id: &str, cadence_minutes: u64) -> Result<Lien, String> {
        let mut lien = self.lire(lien_id)?;
        lien.cadence_minutes = normaliser_cadence(cadence_minutes)?;
        lien.prochaine_synchro_ms = self.prochaine(&lien);
        self.ecrire_lien(&lien)?;
        Ok(lien)
    }

    /// Supprimer le LIEN. Les deux playlists ne sont pas touchées : aucune
    /// capacité hôte n'est appelée ici, sauf l'écriture du stockage du greffon.
    pub fn supprimer(&self, lien_id: &str) -> Result<Value, String> {
        let mut lien = self.lire(lien_id)?;
        lien.etat = etat_lien::SUPPRIME.into();
        lien.supprime_le_ms = Some(self.hote.maintenant_ms());
        lien.prochaine_synchro_ms = None;
        self.ecrire_lien(&lien)?;
        Ok(json!({
            "lien_id": lien_id,
            "supprime": true,
            "playlists_touchees": false,
        }))
    }

    /// Le journal d'un lien, du plus récent au plus ancien.
    pub fn journal(&self, lien_id: &str) -> Result<Vec<EntreeJournal>, String> {
        let lien = self.lire(lien_id)?;
        let premier = lien.journal_compteur.saturating_sub(JOURNAL_RETENTION) + 1;
        let mut entrees = Vec::new();
        for n in (premier..=lien.journal_compteur).rev() {
            if let Some(v) = self.kv_valeur(&cle_journal(lien_id, n))?
                && let Ok(e) = serde_json::from_value::<EntreeJournal>(v)
                && e.numero == n
            {
                entrees.push(e);
            }
        }
        Ok(entrees)
    }

    // -----------------------------------------------------------------------
    // Aperçu — AUCUNE écriture chez un service
    // -----------------------------------------------------------------------

    /// Calculer ce qu'une synchronisation ferait, et le garder comme l'aperçu
    /// que l'utilisateur accepte avant la première synchronisation.
    pub fn apercu(&self, lien_id: &str) -> Result<PlanSynchro, String> {
        let lien = self.lire(lien_id)?;
        let mut corr = self.correspondances(lien_id)?;
        let mut memoire = self.memoire(lien_id)?;
        let (plan, _) = self.calculer(&lien, &mut corr, &mut memoire, true)?;
        let accepte = ApercuAccepte {
            calcule_le_ms: plan.calcule_le_ms,
            ajouts: plan
                .ajouts
                .iter()
                .map(|a| (a.vers.clone(), a.cible_id.clone()))
                .collect(),
        };
        let v = serde_json::to_value(&accepte).map_err(|e| e.to_string())?;
        self.hote.kv_set(&format!("lien_apercu:{lien_id}"), &v)?;
        Ok(plan)
    }

    // -----------------------------------------------------------------------
    // Synchroniser — des AJOUTS, rien d'autre
    // -----------------------------------------------------------------------

    /// Synchroniser un lien. `accord` n'est exigé que pour la PREMIÈRE
    /// synchronisation, qui exige aussi un aperçu et n'écrit pas plus que lui.
    ///
    /// Les refus (lien inconnu, en pause, sans aperçu, sans accord) rendent
    /// une `Err`. Une panne en cours de route (service injoignable…) ne rend
    /// PAS d'`Err` : elle est écrite au journal, qui est fait pour ça.
    pub fn synchroniser(
        &self,
        lien_id: &str,
        accord: bool,
        declencheur_demande: &str,
    ) -> Result<(Lien, EntreeJournal), String> {
        let mut lien = self.lire(lien_id)?;
        if lien.etat == etat_lien::EN_PAUSE {
            return Err(format!(
                "lien_en_pause : le lien {lien_id} est en pause ; le reprendre d'abord"
            ));
        }
        let mut filtre: Option<HashSet<(String, String)>> = None;
        let declencheur = if lien.premiere_synchro_faite {
            declencheur_demande.to_string()
        } else {
            if declencheur_demande == declencheur::MINUTEUR {
                return Err(format!(
                    "apercu_requis : le lien {lien_id} n'a jamais été synchronisé ; le minuteur n'y touche pas"
                ));
            }
            let accepte: ApercuAccepte = self
                .kv_valeur(&format!("lien_apercu:{lien_id}"))?
                .and_then(|v| serde_json::from_value(v).ok())
                .ok_or_else(|| {
                    format!(
                        "apercu_requis : la première synchronisation du lien {lien_id} exige un \
                         aperçu (POST /lien/apercu) puis un accord"
                    )
                })?;
            if !accord {
                return Err(
                    "accord_requis : la première synchronisation d'un lien exige un accord \
                     explicite sur son aperçu"
                        .into(),
                );
            }
            filtre = Some(accepte.ajouts.into_iter().collect());
            declencheur::PREMIERE.to_string()
        };

        let entree = self.executer(&lien, filtre.as_ref(), &declencheur)?;

        let maintenant = self.hote.maintenant_ms();
        lien.derniere_synchro_ms = Some(maintenant);
        lien.derniere_erreur = entree.echecs.first().cloned();
        if !lien.premiere_synchro_faite && entree.statut != "echec" {
            lien.premiere_synchro_faite = true;
            lien.etat = etat_lien::ACTIF.into();
        }
        lien.prochaine_synchro_ms = self.prochaine(&lien);
        lien.journal_compteur = entree.numero;
        self.ecrire_lien(&lien)?;
        Ok((lien, entree))
    }

    /// Le réveil du minuteur : synchroniser **un** lien dû, le plus en retard.
    /// Rend `None` s'il n'y avait rien à faire.
    pub fn tic(&self) -> Result<Option<(Lien, EntreeJournal)>, String> {
        let maintenant = self.hote.maintenant_ms();
        let du = self
            .lister()?
            .into_iter()
            .filter(|l| {
                l.etat == etat_lien::ACTIF
                    && l.premiere_synchro_faite
                    && l.cadence_minutes > 0
                    && l.prochaine_synchro_ms.is_some_and(|p| p <= maintenant)
            })
            .min_by_key(|l| l.prochaine_synchro_ms);
        match du {
            Some(l) => self
                .synchroniser(&l.lien_id, false, declencheur::MINUTEUR)
                .map(Some),
            None => Ok(None),
        }
    }

    // -----------------------------------------------------------------------
    // Internes
    // -----------------------------------------------------------------------

    /// Calculer, écrire, journaliser. Toute panne finit au journal.
    fn executer(
        &self,
        lien: &Lien,
        filtre: Option<&HashSet<(String, String)>>,
        declencheur: &str,
    ) -> Result<EntreeJournal, String> {
        let numero = lien.journal_compteur + 1;
        let mut entree = EntreeJournal {
            numero,
            quand_ms: self.hote.maintenant_ms(),
            declencheur: declencheur.to_string(),
            statut: "ok".into(),
            ajoutees: 0,
            ajouts: Vec::new(),
            introuvables: 0,
            introuvables_detail: Vec::new(),
            disparues_signalees: Vec::new(),
            echecs: Vec::new(),
            snapshots: Vec::new(),
        };

        let mut corr = self.correspondances(&lien.lien_id)?;
        let mut memoire = self.memoire(&lien.lien_id)?;
        // Le minuteur ne redemande pas les introuvables connus ; une demande
        // ou la première synchronisation, si.
        let retenter = declencheur != declencheur::MINUTEUR;
        let (mut plan, lectures) = match self.calculer(lien, &mut corr, &mut memoire, retenter) {
            Ok(r) => r,
            Err(e) => {
                entree.statut = "echec".into();
                entree.echecs.push(format!("lecture : {e}"));
                self.ecrire_journal(&lien.lien_id, &entree)?;
                return Ok(entree);
            }
        };
        if let Some(f) = filtre {
            plan.ajouts
                .retain(|a| f.contains(&(a.vers.clone(), a.cible_id.clone())));
        }

        // 🔴 Un snapshot des DEUX côtés avant d'écrire quoi que ce soit (#4718).
        // Un côté dont la copie échoue ne reçoit rien.
        let snapshots = Snapshots::new(self.hote);
        let motif = format!("avant_synchro:{}", lien.lien_id);
        let mut copie_ok = [false, false];
        for (i, (ext, pistes)) in [(&lien.a, &lectures.a), (&lien.b, &lectures.b)]
            .into_iter()
            .enumerate()
        {
            match snapshots.prendre_depuis(&ext.service, &ext.playlist_id, &ext.nom, pistes, &motif)
            {
                Ok(e) => {
                    entree.snapshots.push(e.snapshot_id);
                    copie_ok[i] = true;
                }
                Err(e) => entree.echecs.push(format!(
                    "snapshot de {}/{} : {e}",
                    ext.service, ext.playlist_id
                )),
            }
        }

        for (cote, ext, ok) in [("a", &lien.a, copie_ok[0]), ("b", &lien.b, copie_ok[1])] {
            let ids: Vec<String> = plan
                .ajouts
                .iter()
                .filter(|a| a.vers == cote)
                .map(|a| a.cible_id.clone())
                .collect();
            if ids.is_empty() {
                continue;
            }
            if !ok {
                entree.echecs.push(format!(
                    "{} ajout(s) vers {cote} non écrits : pas de snapshot préalable",
                    ids.len()
                ));
                continue;
            }
            let mut versees: Vec<String> = Vec::new();
            let r = ajouter(self.hote, &ext.service, &ext.playlist_id, &ids, |paquet| {
                versees.extend(paquet.iter().cloned());
                Ok(())
            });
            if let Err(e) = r {
                entree.echecs.push(format!(
                    "ajout vers {}/{} : {e}",
                    ext.service, ext.playlist_id
                ));
            }
            let vus = if cote == "a" {
                &mut memoire.vus_a
            } else {
                &mut memoire.vus_b
            };
            for id in &versees {
                vus.insert(id.clone());
            }
            entree.ajoutees += versees.len();
            let versees: HashSet<&String> = versees.iter().collect();
            entree.ajouts.extend(
                plan.ajouts
                    .iter()
                    .filter(|a| a.vers == cote && versees.contains(&a.cible_id))
                    .take(DETAIL_MAX.saturating_sub(entree.ajouts.len()))
                    .cloned(),
            );
        }

        entree.introuvables = plan.introuvables.len();
        entree.introuvables_detail = plan.introuvables.into_iter().take(DETAIL_MAX).collect();
        for d in plan.disparues {
            let cle = format!("{}:{}", d.disparue_de, d.id_disparu);
            if memoire.signalees.insert(cle) && entree.disparues_signalees.len() < DETAIL_MAX {
                entree.disparues_signalees.push(d);
            }
        }
        entree.statut = if !entree.echecs.is_empty() {
            "partiel".into()
        } else if entree.ajoutees == 0 {
            "rien_a_faire".into()
        } else {
            "ok".into()
        };

        self.ecrire_etat(&lien.lien_id, &corr, &memoire)?;
        self.ecrire_journal(&lien.lien_id, &entree)?;
        self.hote.journal(
            "info",
            &format!(
                "lien {} ({declencheur}) : {} ajout(s), {} introuvable(s), {} échec(s)",
                lien.lien_id,
                entree.ajoutees,
                entree.introuvables,
                entree.echecs.len()
            ),
        );
        Ok(entree)
    }

    /// Lire les deux côtés et calculer les ajouts. N'ÉCRIT RIEN chez un
    /// service ; met à jour les correspondances et la mémoire EN MÉMOIRE (à
    /// l'appelant de les garder ou non).
    fn calculer(
        &self,
        lien: &Lien,
        corr: &mut Correspondances,
        memoire: &mut Memoire,
        retenter: bool,
    ) -> Result<(PlanSynchro, Lectures), String> {
        let (_, pistes_a) = lire_playlist(self.hote, &lien.a.service, &lien.a.playlist_id)?;
        let (_, pistes_b) = lire_playlist(self.hote, &lien.b.service, &lien.b.playlist_id)?;
        let mut plan = PlanSynchro {
            lien_id: lien.lien_id.clone(),
            calcule_le_ms: self.hote.maintenant_ms(),
            ajouts: Vec::new(),
            introuvables: Vec::new(),
            disparues: Vec::new(),
            deja_presentes: 0,
            introuvables_connues: 0,
        };

        self.passe(
            "a", &pistes_a, &pistes_b, &lien.b, corr, memoire, retenter, &mut plan,
        );
        if lien.sens == sens::DEUX_SENS {
            self.passe(
                "b", &pistes_b, &pistes_a, &lien.a, corr, memoire, retenter, &mut plan,
            );
        } else {
            // Sens unique : une piste retirée de A et toujours dans B est
            // signalée — et laissée dans B.
            let ids_a: HashSet<&str> = pistes_a.iter().map(|p| p.id.as_str()).collect();
            let inverse: BTreeMap<&str, &str> = corr
                .ab
                .iter()
                .map(|(a, b)| (b.as_str(), a.as_str()))
                .collect();
            for p in &pistes_b {
                if let Some(a_id) = inverse.get(p.id.as_str()).map(|a| a.to_string())
                    && !ids_a.contains(a_id.as_str())
                    && memoire.vus_a.contains(&a_id)
                {
                    plan.disparues.push(Disparue {
                        disparue_de: "a".into(),
                        id_disparu: a_id,
                        toujours_dans: "b".into(),
                        id_restant: p.id.clone(),
                        titre: p.titre.clone(),
                        artiste: p.artiste.clone(),
                    });
                }
            }
        }

        // Ce qui est lu aujourd'hui est « vu » : une absence future sera une
        // disparition, pas une piste à remettre.
        memoire.vus_a.extend(pistes_a.iter().map(|p| p.id.clone()));
        memoire.vus_b.extend(pistes_b.iter().map(|p| p.id.clone()));
        Ok((
            plan,
            Lectures {
                a: pistes_a,
                b: pistes_b,
            },
        ))
    }

    /// Une passe X → Y.
    #[allow(clippy::too_many_arguments)]
    fn passe(
        &self,
        de: &str,
        x: &[PisteSnap],
        y: &[PisteSnap],
        vers: &Extremite,
        corr: &mut Correspondances,
        memoire: &mut Memoire,
        retenter: bool,
        plan: &mut PlanSynchro,
    ) {
        let cote_y = if de == "a" { "b" } else { "a" };
        let mut ids_y: HashSet<String> = y.iter().map(|p| p.id.clone()).collect();
        // Dans le sens B → A, les correspondances se lisent à l'envers. On
        // retourne la table UNE fois : une recherche linéaire par piste ferait
        // un parcours quadratique, et le carburant d'un appel est borné.
        let inverse: BTreeMap<String, String> = if de == "b" {
            corr.ab
                .iter()
                .map(|(a, b)| (b.clone(), a.clone()))
                .collect()
        } else {
            BTreeMap::new()
        };
        let mut vus = HashSet::new();
        for p in x {
            if p.id.is_empty() || !vus.insert(p.id.clone()) {
                continue;
            }
            let connu = if de == "a" {
                corr.ab.get(&p.id).cloned()
            } else {
                inverse.get(&p.id).cloned()
            };
            let vus_y = if de == "a" {
                &memoire.vus_b
            } else {
                &memoire.vus_a
            };
            let cible = match connu {
                Some(id) => id,
                None => {
                    let introuvables = if de == "a" {
                        &mut memoire.introuvables_a
                    } else {
                        &mut memoire.introuvables_b
                    };
                    if !retenter && introuvables.contains(&p.id) {
                        plan.introuvables_connues += 1;
                        continue;
                    }
                    match self.apparier(vers, p) {
                        Ok(c) => {
                            introuvables.remove(&p.id);
                            if de == "a" {
                                corr.ab.insert(p.id.clone(), c.id.clone());
                            } else {
                                corr.ab.insert(c.id.clone(), p.id.clone());
                            }
                            c.id
                        }
                        Err(raison) => {
                            introuvables.insert(p.id.clone());
                            plan.introuvables.push(IntrouvableSync {
                                de: de.into(),
                                vers: cote_y.into(),
                                source_id: p.id.clone(),
                                titre: p.titre.clone(),
                                artiste: p.artiste.clone(),
                                raison,
                            });
                            continue;
                        }
                    }
                }
            };
            if ids_y.contains(&cible) {
                plan.deja_presentes += 1;
            } else if vus_y.contains(&cible) {
                // L'utilisateur l'a retirée de Y : on ne la remet PAS.
                plan.disparues.push(Disparue {
                    disparue_de: cote_y.into(),
                    id_disparu: cible,
                    toujours_dans: de.into(),
                    id_restant: p.id.clone(),
                    titre: p.titre.clone(),
                    artiste: p.artiste.clone(),
                });
            } else {
                ids_y.insert(cible.clone());
                plan.ajouts.push(Ajout {
                    de: de.into(),
                    vers: cote_y.into(),
                    source_id: p.id.clone(),
                    titre: p.titre.clone(),
                    artiste: p.artiste.clone(),
                    cible_id: cible,
                });
            }
        }
    }

    /// Apparier une piste CHEZ l'extrémité `vers`, avec la règle des trois
    /// critères (titre + artiste + durée à ±3 s) de la tranche 2.
    fn apparier(&self, vers: &Extremite, p: &PisteSnap) -> Result<Candidat, Raison> {
        let local = vers.service == LOCAL;
        let reponse = if local {
            self.hote
                .library_match_track(&p.titre, &p.artiste, &p.isrc, p.duree_ms)
        } else {
            self.hote.streaming_match_track(
                &vers.service,
                &p.titre,
                &p.artiste,
                &p.isrc,
                p.duree_ms,
            )
        }
        .map_err(|message| Raison::ServiceEnErreur { message })?;
        let mut candidat = appariement::candidat_de_la_reponse(&reponse);
        if local && let Some(c) = candidat.as_mut() {
            // En bibliothèque, l'identifiant qui compte est `track_id` ;
            // `source_id` y désigne, s'il existe, l'origine streaming.
            if let Some(n) = reponse
                .get("matched")
                .and_then(|m| m.get("track_id"))
                .and_then(Value::as_i64)
            {
                c.id = n.to_string();
            }
        }
        appariement::juger(p.duree_ms, candidat)
    }

    fn completer_extremite(&self, e: &Extremite) -> Result<Extremite, String> {
        let (nom, _) = lire_playlist(self.hote, &e.service, &e.playlist_id).map_err(|err| {
            format!(
                "demande_invalide : {}/{} illisible : {err}",
                e.service, e.playlist_id
            )
        })?;
        let nom = match nom {
            Some(n) => n,
            None if !e.nom.is_empty() => e.nom.clone(),
            None => Snapshots::new(self.hote).nom_chez_le_service(&e.service, &e.playlist_id),
        };
        Ok(Extremite {
            service: e.service.clone(),
            playlist_id: e.playlist_id.clone(),
            nom,
        })
    }

    fn prochaine(&self, lien: &Lien) -> Option<u64> {
        (lien.cadence_minutes > 0 && lien.premiere_synchro_faite)
            .then(|| self.hote.maintenant_ms() + lien.cadence_minutes * 60_000)
    }

    fn correspondances(&self, lien_id: &str) -> Result<Correspondances, String> {
        Ok(self
            .kv_valeur(&format!("lien_corr:{lien_id}"))?
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default())
    }

    fn memoire(&self, lien_id: &str) -> Result<Memoire, String> {
        Ok(self
            .kv_valeur(&format!("lien_vus:{lien_id}"))?
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default())
    }

    fn ecrire_etat(
        &self,
        lien_id: &str,
        corr: &Correspondances,
        memoire: &Memoire,
    ) -> Result<(), String> {
        let v = serde_json::to_value(corr).map_err(|e| e.to_string())?;
        self.hote.kv_set(&format!("lien_corr:{lien_id}"), &v)?;
        let v = serde_json::to_value(memoire).map_err(|e| e.to_string())?;
        self.hote.kv_set(&format!("lien_vus:{lien_id}"), &v)?;
        Ok(())
    }

    fn ecrire_journal(&self, lien_id: &str, entree: &EntreeJournal) -> Result<(), String> {
        let v = serde_json::to_value(entree).map_err(|e| e.to_string())?;
        self.hote.kv_set(&cle_journal(lien_id, entree.numero), &v)?;
        Ok(())
    }

    fn ecrire_lien(&self, lien: &Lien) -> Result<(), String> {
        let v = serde_json::to_value(lien).map_err(|e| e.to_string())?;
        self.hote
            .kv_set(&format!("{PREFIXE_LIEN}{}", lien.lien_id), &v)?;
        Ok(())
    }

    fn compteur(&self) -> Result<u64, String> {
        Ok(self
            .kv_valeur(COMPTEUR_LIENS)?
            .and_then(|v| v.as_u64())
            .unwrap_or(0))
    }

    fn kv_valeur(&self, cle: &str) -> Result<Option<Value>, String> {
        let r = self.hote.kv_get(cle)?;
        if r.get("found").and_then(Value::as_bool) == Some(true) {
            Ok(r.get("value").cloned())
        } else {
            Ok(None)
        }
    }
}

fn cle_journal(lien_id: &str, n: u64) -> String {
    format!(
        "lien_journal:{lien_id}:{}",
        (n.max(1) - 1) % JOURNAL_RETENTION
    )
}

fn numero(lien_id: &str) -> u64 {
    lien_id
        .strip_prefix("lien-")
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// `0` = à la demande ; sinon borné. Une cadence hors bornes est REFUSÉE,
/// pas ramenée en silence : l'écran doit savoir ce qu'il a réglé.
fn normaliser_cadence(minutes: u64) -> Result<u64, String> {
    if minutes == 0 || (CADENCE_MIN_MINUTES..=CADENCE_MAX_MINUTES).contains(&minutes) {
        Ok(minutes)
    } else {
        Err(format!(
            "demande_invalide : cadence de {minutes} min hors bornes (0 = à la demande, sinon \
             {CADENCE_MIN_MINUTES} à {CADENCE_MAX_MINUTES})"
        ))
    }
}
