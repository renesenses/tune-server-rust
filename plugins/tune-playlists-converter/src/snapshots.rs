//! Snapshots — tranche 3 de l'épique #4715 (#4718).
//!
//! Une **copie datée** d'une playlist (son nom, ses pistes, leurs identifiants
//! de service), prise avant toute écriture, et le **retour en arrière**.
//!
//! ## Le retour en arrière ne supprime RIEN
//!
//! L'interface hôte n'a aucune capacité de suppression (règle de Bertrand du
//! 22/09/2026, gardée par `aucune_capacite_hote_ne_supprime_4716`). Un retour
//! en arrière est donc **non destructif** :
//!
//! * mode `completer` — on **rajoute** à la playlist les pistes du snapshot
//!   qui en ont disparu, et on **liste** celles qui y sont en trop
//!   (`a_retirer_par_vous`) : c'est à l'utilisateur de les retirer lui-même,
//!   depuis l'application du service. Tune ne le fait jamais à sa place ;
//! * mode `recreer` — on **crée une nouvelle playlist** avec le nom et les
//!   pistes du snapshot. L'ancienne n'est pas touchée.
//!
//! Comme tout ce qui écrit chez un service, un retour en arrière passe par un
//! **aperçu** (`plan`) puis un **accord** explicite. Et il prend lui-même un
//! snapshot de l'état courant avant d'écrire : un retour en arrière se défait.
//!
//! ## Stockage et rétention
//!
//! Tout vit dans le stockage clé/valeur cloisonné du greffon (#4716). Le
//! stockage ne sait pas SUPPRIMER une clé non plus : la rétention est donc un
//! **anneau** — [`RETENTION_PAR_PLAYLIST`] emplacements par playlist, le plus
//! ancien réécrit par le plus récent. Rien ne grossit sans borne.
//!
//! | Clé | Contenu |
//! |---|---|
//! | `compteur_playlists_snap` | le dernier numéro de playlist attribué |
//! | `snap_pl:<service>/<playlist_id>` | le registre : numéro `k`, nom, nombre de snapshots pris |
//! | `snap:<k>:<emplacement>` | l'en-tête d'un snapshot |
//! | `snap:<k>:<emplacement>:p:<n>` | une page de [`PISTES_PAR_PAGE`] pistes |
//! | `compteur_restaurations` | le dernier numéro de plan de restauration |
//! | `restauration:<emplacement>` | un plan (anneau de [`RETENTION_PLANS`]) |
//!
//! Un snapshot IDENTIQUE au précédent (même nom, mêmes pistes dans le même
//! ordre) n'occupe pas d'emplacement : le précédent est rendu. Sans cela, un
//! lien de synchronisation qui tourne toutes les quinze minutes (#4719)
//! chasserait de l'anneau, en deux heures et demie, tout ce qui compte.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hote::Hote;

/// Nombre de snapshots gardés PAR PLAYLIST. Le onzième réécrit le premier.
pub const RETENTION_PAR_PLAYLIST: u64 = 10;

/// Pistes par page de stockage. L'hôte borne une valeur à 256 Kio ; 400
/// pistes aux titres longs (≈ 400 octets chacune) y tiennent encore.
pub const PISTES_PAR_PAGE: usize = 400;

/// Nombre de plans de restauration gardés (anneau, tous confondus).
pub const RETENTION_PLANS: u64 = 20;

/// Paquet d'ajout, comme pour le transfert (#4717) : le lot d'ajout de TIDAL.
const TAILLE_PAQUET: usize = 100;

const COMPTEUR_PLAYLISTS: &str = "compteur_playlists_snap";
const PREFIXE_REGISTRE: &str = "snap_pl:";
const COMPTEUR_PLANS: &str = "compteur_restaurations";

/// Le service `local` désigne la bibliothèque : identifiants entiers.
pub const LOCAL: &str = "local";

/// Une piste telle qu'elle est gardée : de quoi la reconnaître et la remettre.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PisteSnap {
    /// L'identifiant CHEZ le service de la playlist (`source_id`), ou
    /// l'identifiant entier de la piste pour la bibliothèque locale.
    pub id: String,
    pub titre: String,
    pub artiste: String,
    pub duree_ms: u64,
    #[serde(default)]
    pub isrc: String,
}

/// L'en-tête d'un snapshot — ce que liste l'écran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnTeteSnapshot {
    pub snapshot_id: String,
    pub service: String,
    pub playlist_id: String,
    pub nom: String,
    /// Date de prise, en millisecondes Unix (heure de l'hôte).
    pub pris_le_ms: u64,
    /// Pourquoi il a été pris : `manuel`, `avant_transfert:lot-N`,
    /// `avant_restauration:plan-N`, `avant_synchro:lien-N`.
    pub motif: String,
    pub total: usize,
    pub pages: usize,
    /// Empreinte du nom et des identifiants, dans l'ordre : c'est elle qui
    /// évite de garder deux fois la même copie.
    pub empreinte: String,
}

/// Un snapshot complet : l'en-tête et ses pistes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(flatten)]
    pub entete: EnTeteSnapshot,
    pub pistes: Vec<PisteSnap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registre {
    k: u64,
    service: String,
    playlist_id: String,
    nom: String,
    /// Nombre de snapshots pris depuis toujours ; le dernier est le n°
    /// `compteur`, les [`RETENTION_PAR_PLAYLIST`] derniers sont lisibles.
    compteur: u64,
}

/// Les deux façons de revenir en arrière, toutes deux sans suppression.
pub mod mode {
    /// Rajouter ce qui manque, lister ce qui est en trop.
    pub const COMPLETER: &str = "completer";
    /// Créer une nouvelle playlist depuis le snapshot.
    pub const RECREER: &str = "recreer";
}

/// Un plan de retour en arrière : l'aperçu, puis son exécution.
///
/// Seuls les IDENTIFIANTS sont persistés (une playlist de trois mille titres
/// détaillés ne tiendrait pas dans une valeur) ; le détail lisible est rendu
/// à l'écran avec la réponse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRestauration {
    pub plan_id: String,
    pub snapshot_id: String,
    pub mode: String,
    pub service: String,
    pub playlist_id: String,
    pub nom: String,
    pub calcule_le_ms: u64,
    /// `apercu`, `termine` ou `interrompu`.
    pub etat: String,
    /// Ce qui sera rajouté (mode `completer`) ou versé dans la nouvelle
    /// playlist (mode `recreer`).
    pub a_rajouter_ids: Vec<String>,
    /// Ce qui est dans la playlist et pas dans le snapshot. **Jamais retiré
    /// par Tune** : listé pour que l'utilisateur le retire lui-même.
    pub a_retirer_par_vous_ids: Vec<String>,
    pub deja_presentes: usize,
    #[serde(default)]
    pub rajoutees: Vec<String>,
    #[serde(default)]
    pub playlist_recreee_id: Option<String>,
    #[serde(default)]
    pub snapshot_avant_restauration: Option<String>,
    #[serde(default)]
    pub erreur: Option<String>,
    pub avertissement: String,
}

/// Lire une piste rendue par l'hôte, locale ou de service.
pub fn piste_snap(local: bool, v: &Value) -> PisteSnap {
    let id = if local {
        v.get("track_id")
            .and_then(Value::as_i64)
            .map(|n| n.to_string())
    } else {
        None
    }
    .or_else(|| {
        v.get("source_id")
            .or_else(|| v.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
    .unwrap_or_default();
    PisteSnap {
        id,
        titre: texte(v, "title"),
        artiste: v
            .get("artist_name")
            .or_else(|| v.get("artist"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        duree_ms: v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0),
        isrc: texte(v, "isrc"),
    }
}

fn texte(v: &Value, cle: &str) -> String {
    v.get(cle)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// FNV-1a 64 bits : une empreinte stable, sans dépendance. Ce n'est pas une
/// garde de sécurité, seulement « est-ce la même copie que la dernière ? ».
fn empreinte(nom: &str, pistes: &[PisteSnap]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut avaler = |octets: &[u8]| {
        for b in octets {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    avaler(nom.as_bytes());
    for p in pistes {
        avaler(&[0x1f]);
        avaler(p.id.as_bytes());
    }
    format!("{h:016x}")
}

fn identifiant(k: u64, n: u64) -> String {
    format!("snap-{k}-{n}")
}

/// `snap-<k>-<n>` → `(k, n)`.
fn decoder_identifiant(id: &str) -> Option<(u64, u64)> {
    let reste = id.strip_prefix("snap-")?;
    let (k, n) = reste.split_once('-')?;
    let k = k.parse().ok()?;
    let n: u64 = n.parse().ok()?;
    (n > 0).then_some((k, n))
}

fn emplacement(n: u64) -> u64 {
    (n - 1) % RETENTION_PAR_PLAYLIST
}

/// Lire une playlist, locale ou chez un service : `(nom s'il est connu, pistes)`.
///
/// La réponse d'un service ne porte pas le nom de la playlist ; l'appelant
/// qui le connaît déjà le fournit, sinon [`Snapshots::nom_chez_le_service`].
pub fn lire_playlist<H: Hote + ?Sized>(
    hote: &H,
    service: &str,
    playlist_id: &str,
) -> Result<(Option<String>, Vec<PisteSnap>), String> {
    let (reponse, local) = if service == LOCAL {
        let id: i64 = playlist_id
            .parse()
            .map_err(|_| format!("identifiant de playlist locale non entier : {playlist_id}"))?;
        (hote.playlist_tracks(id)?, true)
    } else {
        (hote.streaming_playlist_tracks(service, playlist_id)?, false)
    };
    let nom = reponse
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let pistes = reponse
        .get("tracks")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| piste_snap(local, v)).collect())
        .unwrap_or_default();
    Ok((nom, pistes))
}

/// AJOUTER des pistes à une playlist, locale ou chez un service, par paquets.
/// Rend les identifiants effectivement envoyés, dans l'ordre, jusqu'à
/// l'erreur s'il y en a une.
pub fn ajouter<H: Hote + ?Sized>(
    hote: &H,
    service: &str,
    playlist_id: &str,
    ids: &[String],
    mut apres_chaque_paquet: impl FnMut(&[String]) -> Result<(), String>,
) -> Result<(), String> {
    for paquet in ids.chunks(TAILLE_PAQUET) {
        if service == LOCAL {
            let pl: i64 = playlist_id.parse().map_err(|_| {
                format!("identifiant de playlist locale non entier : {playlist_id}")
            })?;
            let entiers: Vec<i64> = paquet.iter().filter_map(|i| i.parse().ok()).collect();
            if entiers.is_empty() {
                continue;
            }
            hote.playlist_add_tracks(pl, &entiers)?;
        } else {
            hote.streaming_playlist_add_tracks(service, playlist_id, paquet)?;
        }
        apres_chaque_paquet(paquet)?;
    }
    Ok(())
}

/// Créer une playlist, locale ou chez un service. Rend son identifiant.
pub fn creer<H: Hote + ?Sized>(
    hote: &H,
    service: &str,
    nom: &str,
    description: Option<&str>,
) -> Result<String, String> {
    let reponse = if service == LOCAL {
        hote.playlist_create(nom, description)?
    } else {
        hote.streaming_playlist_create(service, nom, description)?
    };
    let id = reponse.get("playlist_id");
    id.and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| id.and_then(Value::as_i64).map(|n| n.to_string()))
        .ok_or_else(|| "réponse sans playlist_id à la création".to_string())
}

/// Le gestionnaire des snapshots. Comme le convertisseur, il ne tient qu'une
/// référence vers l'hôte : tout son état est dans le stockage clé/valeur.
pub struct Snapshots<'h, H: Hote + ?Sized> {
    hote: &'h H,
}

impl<'h, H: Hote + ?Sized> Snapshots<'h, H> {
    pub fn new(hote: &'h H) -> Self {
        Self { hote }
    }

    // -----------------------------------------------------------------------
    // Prendre
    // -----------------------------------------------------------------------

    /// Lire la playlist et en garder une copie datée. N'écrit RIEN chez le
    /// service : une lecture, puis le stockage du greffon.
    pub fn prendre(
        &self,
        service: &str,
        playlist_id: &str,
        nom_connu: Option<&str>,
        motif: &str,
    ) -> Result<EnTeteSnapshot, String> {
        let (nom_lu, pistes) = lire_playlist(self.hote, service, playlist_id)?;
        let nom = match nom_connu.or(nom_lu.as_deref()) {
            Some(n) => n.to_string(),
            None => self.nom_chez_le_service(service, playlist_id),
        };
        self.prendre_depuis(service, playlist_id, &nom, &pistes, motif)
    }

    /// Garder une copie d'un contenu DÉJÀ lu. C'est ce qu'emploient le
    /// transfert (une playlist qu'il vient de créer est vide, il le sait) et
    /// la synchronisation (qui vient de lire les deux côtés).
    pub fn prendre_depuis(
        &self,
        service: &str,
        playlist_id: &str,
        nom: &str,
        pistes: &[PisteSnap],
        motif: &str,
    ) -> Result<EnTeteSnapshot, String> {
        if service.is_empty() || playlist_id.is_empty() {
            return Err("demande_invalide : service et playlist_id sont obligatoires".into());
        }
        let mut registre = match self.registre(service, playlist_id)? {
            Some(r) => r,
            None => {
                let k = self.compteur(COMPTEUR_PLAYLISTS)? + 1;
                self.hote.kv_set(COMPTEUR_PLAYLISTS, &json!(k))?;
                Registre {
                    k,
                    service: service.to_string(),
                    playlist_id: playlist_id.to_string(),
                    nom: nom.to_string(),
                    compteur: 0,
                }
            }
        };

        let empreinte = empreinte(nom, pistes);
        if registre.compteur > 0
            && let Some(dernier) = self.entete(registre.k, registre.compteur)?
            && dernier.empreinte == empreinte
        {
            // Rien n'a bougé depuis la dernière copie : on la rend, sans
            // consommer d'emplacement.
            return Ok(dernier);
        }

        let n = registre.compteur + 1;
        let place = emplacement(n);
        let pages: Vec<&[PisteSnap]> = pistes.chunks(PISTES_PAR_PAGE).collect();
        // Les pages d'abord, l'en-tête ENSUITE : l'en-tête est le point de
        // validation. Une coupure entre les deux laisse l'ancien en-tête, qui
        // ne désigne pas ce numéro, et le snapshot n'existe simplement pas.
        for (i, page) in pages.iter().enumerate() {
            let v = serde_json::to_value(page).map_err(|e| e.to_string())?;
            self.hote
                .kv_set(&format!("snap:{}:{place}:p:{i}", registre.k), &v)?;
        }
        let entete = EnTeteSnapshot {
            snapshot_id: identifiant(registre.k, n),
            service: service.to_string(),
            playlist_id: playlist_id.to_string(),
            nom: nom.to_string(),
            pris_le_ms: self.hote.maintenant_ms(),
            motif: motif.to_string(),
            total: pistes.len(),
            pages: pages.len(),
            empreinte,
        };
        let v = serde_json::to_value(&entete).map_err(|e| e.to_string())?;
        self.hote
            .kv_set(&format!("snap:{}:{place}", registre.k), &v)?;

        registre.compteur = n;
        registre.nom = nom.to_string();
        let v = serde_json::to_value(&registre).map_err(|e| e.to_string())?;
        self.hote
            .kv_set(&Self::cle_registre(service, playlist_id), &v)?;
        self.hote.journal(
            "info",
            &format!(
                "snapshot {} de {service}/{playlist_id} ({} pistes, {motif})",
                entete.snapshot_id, entete.total
            ),
        );
        Ok(entete)
    }

    // -----------------------------------------------------------------------
    // Lister, lire
    // -----------------------------------------------------------------------

    /// Les playlists qui ont au moins un snapshot.
    pub fn playlists(&self) -> Result<Vec<Value>, String> {
        let liste = self.hote.kv_list(PREFIXE_REGISTRE)?;
        let mut sortie = Vec::new();
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
            let Ok(r) = serde_json::from_value::<Registre>(v) else {
                continue;
            };
            let dernier = self.entete(r.k, r.compteur)?;
            sortie.push(json!({
                "service": r.service,
                "playlist_id": r.playlist_id,
                "nom": r.nom,
                "snapshots": r.compteur.min(RETENTION_PAR_PLAYLIST),
                "dernier_le_ms": dernier.map(|d| d.pris_le_ms),
            }));
        }
        Ok(sortie)
    }

    /// Les snapshots d'une playlist, du plus récent au plus ancien.
    pub fn lister(&self, service: &str, playlist_id: &str) -> Result<Vec<EnTeteSnapshot>, String> {
        let Some(r) = self.registre(service, playlist_id)? else {
            return Ok(Vec::new());
        };
        let premier = r.compteur.saturating_sub(RETENTION_PAR_PLAYLIST) + 1;
        let mut sortie = Vec::new();
        for n in (premier..=r.compteur).rev() {
            if let Some(e) = self.entete(r.k, n)? {
                sortie.push(e);
            }
        }
        Ok(sortie)
    }

    /// Un snapshot complet.
    pub fn lire(&self, snapshot_id: &str) -> Result<Snapshot, String> {
        let (k, n) = decoder_identifiant(snapshot_id)
            .ok_or_else(|| format!("snapshot_inconnu : {snapshot_id}"))?;
        let entete = self.entete(k, n)?.ok_or_else(|| {
            format!(
                "snapshot_expire : {snapshot_id} n'est plus gardé (rétention : \
                 {RETENTION_PAR_PLAYLIST} par playlist) ou n'a jamais existé"
            )
        })?;
        let place = emplacement(n);
        let mut pistes = Vec::with_capacity(entete.total);
        for i in 0..entete.pages {
            let v = self
                .kv_valeur(&format!("snap:{k}:{place}:p:{i}"))?
                .ok_or_else(|| format!("page {i} manquante dans {snapshot_id}"))?;
            let page: Vec<PisteSnap> =
                serde_json::from_value(v).map_err(|e| format!("page {i} illisible : {e}"))?;
            pistes.extend(page);
        }
        pistes.truncate(entete.total);
        Ok(Snapshot { entete, pistes })
    }

    // -----------------------------------------------------------------------
    // Revenir en arrière — sans jamais supprimer
    // -----------------------------------------------------------------------

    /// L'aperçu d'un retour en arrière. **N'écrit rien chez le service.**
    ///
    /// Rend le plan (persisté) et le détail lisible : ce qui sera rajouté, et
    /// ce que l'utilisateur devra retirer LUI-MÊME s'il le veut.
    pub fn apercu_restauration(
        &self,
        snapshot_id: &str,
        mode_demande: &str,
    ) -> Result<(PlanRestauration, Vec<PisteSnap>, Vec<PisteSnap>), String> {
        if mode_demande != mode::COMPLETER && mode_demande != mode::RECREER {
            return Err(format!(
                "demande_invalide : mode « {mode_demande} » inconnu (completer ou recreer)"
            ));
        }
        let snap = self.lire(snapshot_id)?;
        let e = &snap.entete;

        let (a_rajouter, a_retirer, deja) = if mode_demande == mode::COMPLETER {
            let (_, courantes) = lire_playlist(self.hote, &e.service, &e.playlist_id)?;
            comparer(&snap.pistes, &courantes)
        } else {
            (sans_doublon(&snap.pistes), Vec::new(), 0)
        };

        let n = self.compteur(COMPTEUR_PLANS)? + 1;
        self.hote.kv_set(COMPTEUR_PLANS, &json!(n))?;
        let plan = PlanRestauration {
            plan_id: format!("plan-{n}"),
            snapshot_id: snapshot_id.to_string(),
            mode: mode_demande.to_string(),
            service: e.service.clone(),
            playlist_id: e.playlist_id.clone(),
            nom: e.nom.clone(),
            calcule_le_ms: self.hote.maintenant_ms(),
            etat: "apercu".into(),
            a_rajouter_ids: a_rajouter.iter().map(|p| p.id.clone()).collect(),
            a_retirer_par_vous_ids: a_retirer.iter().map(|p| p.id.clone()).collect(),
            deja_presentes: deja,
            rajoutees: Vec::new(),
            playlist_recreee_id: None,
            snapshot_avant_restauration: None,
            erreur: None,
            avertissement: avertissement(mode_demande, a_retirer.len()),
        };
        self.ecrire_plan(&plan)?;
        Ok((plan, a_rajouter, a_retirer))
    }

    /// Exécuter un plan accepté. Refuse sans `accord`, refuse un plan
    /// terminé. **N'appelle jamais que des capacités d'AJOUT et de CRÉATION.**
    pub fn restaurer(
        &self,
        plan_id: &str,
        accord: bool,
    ) -> Result<(PlanRestauration, Vec<PisteSnap>), String> {
        if !accord {
            return Err(
                "accord_requis : rien n'est écrit chez un service sans accord explicite".into(),
            );
        }
        let mut plan = self.lire_plan(plan_id)?;
        if plan.etat == "termine" {
            return Err(format!(
                "plan_deja_engage : le plan {plan_id} est déjà exécuté ; en calculer un nouveau"
            ));
        }
        let snap = self.lire(&plan.snapshot_id)?;
        plan.erreur = None;

        let mut a_retirer = Vec::new();
        let resultat = if plan.mode == mode::COMPLETER {
            let (nom_lu, courantes) = lire_playlist(self.hote, &plan.service, &plan.playlist_id)?;
            // 🔴 Avant d'écrire, une copie de l'état COURANT : un retour en
            // arrière se défait lui aussi.
            if plan.snapshot_avant_restauration.is_none() {
                let avant = self.prendre_depuis(
                    &plan.service,
                    &plan.playlist_id,
                    nom_lu.as_deref().unwrap_or(&plan.nom),
                    &courantes,
                    &format!("avant_restauration:{}", plan.plan_id),
                )?;
                plan.snapshot_avant_restauration = Some(avant.snapshot_id);
                self.ecrire_plan(&plan)?;
            }
            let presentes: HashSet<&str> = courantes.iter().map(|p| p.id.as_str()).collect();
            // Jamais PLUS que l'aperçu : ce qui est apparu depuis n'est pas
            // dans l'accord donné.
            let restant: Vec<String> = plan
                .a_rajouter_ids
                .iter()
                .filter(|id| !presentes.contains(id.as_str()) && !plan.rajoutees.contains(id))
                .cloned()
                .collect();
            let (_, retirer, _) = comparer(&snap.pistes, &courantes);
            a_retirer = retirer;
            plan.a_retirer_par_vous_ids = a_retirer.iter().map(|p| p.id.clone()).collect();
            let service = plan.service.clone();
            let playlist_id = plan.playlist_id.clone();
            ajouter(self.hote, &service, &playlist_id, &restant, |paquet| {
                plan.rajoutees.extend(paquet.iter().cloned());
                self.ecrire_plan(&plan)
            })
        } else {
            let cible = match plan.playlist_recreee_id.clone() {
                Some(id) => id,
                None => {
                    let id = creer(
                        self.hote,
                        &plan.service,
                        &plan.nom,
                        Some("Restauré par Tune depuis un snapshot"),
                    )?;
                    // Persister TOUT DE SUITE, comme le transfert : une reprise
                    // ne doit pas créer une seconde playlist.
                    plan.playlist_recreee_id = Some(id.clone());
                    self.ecrire_plan(&plan)?;
                    id
                }
            };
            let restant: Vec<String> = plan
                .a_rajouter_ids
                .iter()
                .filter(|id| !plan.rajoutees.contains(id))
                .cloned()
                .collect();
            let service = plan.service.clone();
            ajouter(self.hote, &service, &cible, &restant, |paquet| {
                plan.rajoutees.extend(paquet.iter().cloned());
                self.ecrire_plan(&plan)
            })
        };

        match resultat {
            Ok(()) => plan.etat = "termine".into(),
            Err(e) => {
                plan.etat = "interrompu".into();
                plan.erreur = Some(e);
            }
        }
        plan.avertissement = avertissement(&plan.mode, plan.a_retirer_par_vous_ids.len());
        self.ecrire_plan(&plan)?;
        Ok((plan, a_retirer))
    }

    pub fn lire_plan(&self, plan_id: &str) -> Result<PlanRestauration, String> {
        let n: u64 = plan_id
            .strip_prefix("plan-")
            .and_then(|n| n.parse().ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("plan_inconnu : {plan_id}"))?;
        let v = self
            .kv_valeur(&format!("restauration:{}", (n - 1) % RETENTION_PLANS))?
            .ok_or_else(|| format!("plan_inconnu : {plan_id}"))?;
        let plan: PlanRestauration =
            serde_json::from_value(v).map_err(|e| format!("plan illisible : {e}"))?;
        if plan.plan_id != plan_id {
            return Err(format!(
                "plan_inconnu : {plan_id} n'est plus gardé (rétention : {RETENTION_PLANS} plans)"
            ));
        }
        Ok(plan)
    }

    /// Le nom d'une playlist de service, lu dans la liste de ses playlists.
    /// L'identifiant tient lieu de nom si le service ne la liste pas.
    pub fn nom_chez_le_service(&self, service: &str, playlist_id: &str) -> String {
        self.hote
            .streaming_playlists(service)
            .ok()
            .and_then(|r| {
                r.get("playlists")?.as_array()?.iter().find_map(|p| {
                    let id = p
                        .get("source_id")
                        .or_else(|| p.get("id"))
                        .and_then(Value::as_str)?;
                    (id == playlist_id)
                        .then(|| p.get("name").and_then(Value::as_str).map(str::to_string))
                        .flatten()
                })
            })
            .unwrap_or_else(|| playlist_id.to_string())
    }

    // -----------------------------------------------------------------------
    // Internes
    // -----------------------------------------------------------------------

    fn cle_registre(service: &str, playlist_id: &str) -> String {
        format!("{PREFIXE_REGISTRE}{service}/{playlist_id}")
    }

    fn registre(&self, service: &str, playlist_id: &str) -> Result<Option<Registre>, String> {
        Ok(self
            .kv_valeur(&Self::cle_registre(service, playlist_id))?
            .and_then(|v| serde_json::from_value(v).ok()))
    }

    /// L'en-tête du snapshot n°`n` de la playlist `k`, s'il est encore gardé.
    fn entete(&self, k: u64, n: u64) -> Result<Option<EnTeteSnapshot>, String> {
        if n == 0 {
            return Ok(None);
        }
        let Some(v) = self.kv_valeur(&format!("snap:{k}:{}", emplacement(n)))? else {
            return Ok(None);
        };
        let e: EnTeteSnapshot =
            serde_json::from_value(v).map_err(|e| format!("en-tête illisible : {e}"))?;
        // L'emplacement a pu être réécrit par un snapshot plus récent : ce
        // n'est alors plus celui qu'on demande.
        Ok((e.snapshot_id == identifiant(k, n)).then_some(e))
    }

    fn ecrire_plan(&self, plan: &PlanRestauration) -> Result<(), String> {
        let n: u64 = plan
            .plan_id
            .strip_prefix("plan-")
            .and_then(|n| n.parse().ok())
            .unwrap_or(1);
        let v = serde_json::to_value(plan).map_err(|e| e.to_string())?;
        self.hote
            .kv_set(&format!("restauration:{}", (n - 1) % RETENTION_PLANS), &v)?;
        Ok(())
    }

    fn compteur(&self, cle: &str) -> Result<u64, String> {
        Ok(self.kv_valeur(cle)?.and_then(|v| v.as_u64()).unwrap_or(0))
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

/// Ce qui manque et ce qui est en trop, du point de vue du snapshot :
/// `(à rajouter, à retirer par l'utilisateur, déjà présentes)`.
fn comparer(
    snapshot: &[PisteSnap],
    courantes: &[PisteSnap],
) -> (Vec<PisteSnap>, Vec<PisteSnap>, usize) {
    let ids_courants: HashSet<&str> = courantes.iter().map(|p| p.id.as_str()).collect();
    let ids_snapshot: HashSet<&str> = snapshot.iter().map(|p| p.id.as_str()).collect();
    let snapshot = sans_doublon(snapshot);
    let deja = snapshot
        .iter()
        .filter(|p| ids_courants.contains(p.id.as_str()))
        .count();
    let a_rajouter = snapshot
        .into_iter()
        .filter(|p| !ids_courants.contains(p.id.as_str()))
        .collect();
    let a_retirer = sans_doublon(courantes)
        .into_iter()
        .filter(|p| !ids_snapshot.contains(p.id.as_str()))
        .collect();
    (a_rajouter, a_retirer, deja)
}

fn sans_doublon(pistes: &[PisteSnap]) -> Vec<PisteSnap> {
    let mut vus = HashSet::new();
    pistes
        .iter()
        .filter(|p| !p.id.is_empty() && vus.insert(p.id.clone()))
        .cloned()
        .collect()
}

fn avertissement(mode_plan: &str, a_retirer: usize) -> String {
    if mode_plan == mode::RECREER {
        return "Une NOUVELLE playlist sera créée avec le contenu du snapshot. \
                L'ancienne n'est ni modifiée ni supprimée : Tune ne supprime jamais rien \
                chez un service."
            .into();
    }
    if a_retirer == 0 {
        "Les pistes manquantes seront rajoutées en fin de playlist. Rien n'est retiré.".into()
    } else {
        format!(
            "Les pistes manquantes seront rajoutées en fin de playlist. Tune ne supprime \
             jamais rien chez un service : les {a_retirer} piste(s) de « à retirer par vous » \
             sont à retirer vous-même, depuis l'application du service, si vous le souhaitez."
        )
    }
}
