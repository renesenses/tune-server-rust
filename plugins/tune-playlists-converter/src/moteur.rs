//! Le moteur du convertisseur : aperçu, transfert, reprise.
//!
//! ## La garantie qui tient tout le reste
//!
//! **Un aperçu n'appelle aucune capacité d'écriture.** Pas « il essaie de ne
//! pas », pas « il le fait seulement si… » : [`Convertisseur::apercu`] n'a
//! aucun chemin qui mène à `streaming_playlist_create` ou
//! `streaming_playlist_add_tracks`, et la porte le vérifie avec un hôte de banc
//! qui **échoue** dès qu'on lui demande d'écrire chez un service
//! (`apercu_n_ecrit_rien_chez_le_service`).
//!
//! L'écriture, elle, exige deux choses à la fois : un lot issu d'un aperçu, et
//! un `accord` explicite dans la requête. Un `POST /transfert` sans accord, ou
//! sur un identifiant de lot inconnu, ne crée rien.
//!
//! ## La reprise
//!
//! Le lot est persisté dans le stockage clé/valeur de l'hôte **au fur et à
//! mesure** : après la création de la playlist cible, puis après chaque paquet
//! de titres versé. Une reprise relit cet état et ne refait rien de ce qui est
//! déjà fait — ni la playlist (son identifiant est là), ni les titres (leurs
//! identifiants cible sont là). C'est le sens de
//! [`PlaylistDuLot::restant_a_verser`].
//!
//! Ce n'est pas une élégance : une playlist recréée à chaque reprise est un
//! doublon chez le service, et une capacité de suppression n'existe pas pour
//! aller le rattraper.
//!
//! ## Ce que le greffon NE fait pas, et pourquoi
//!
//! * **L'ETag de TIDAL et la pagination des pistes** sont l'affaire du
//!   connecteur, derrière `add_tracks_to_playlist` / `get_playlist_tracks`. Le
//!   greffon ne parle pas HTTP — il n'a pas la permission `net`. Il verse par
//!   paquets de [`TAILLE_PAQUET`] parce que c'est la taille d'un lot TIDAL, et
//!   parce qu'un paquet versé est un point de reprise.
//! * **La bibliothèque locale comme CIBLE.** Il faudrait apparier un titre
//!   dans la bibliothèque, et l'interface hôte de la tranche 1 n'expose aucune
//!   capacité de recherche locale (`host_search` / un
//!   `host_library_match_track`, permission `library`, n'existent pas). La
//!   demande est refusée explicitement plutôt que silencieusement approximée.

use serde_json::{Value, json};

use crate::appariement::{self, Raison};
use crate::hote::Hote;
use crate::modele::{Appariee, Demande, EnTeteLot, Introuvable, Lot, PlaylistDuLot, etat};
use crate::snapshots::Snapshots;

/// Taille d'un paquet de titres versé chez la cible.
///
/// Cent, comme le lot d'ajout de TIDAL. Chaque paquet versé est enregistré :
/// c'est le grain de la reprise.
pub const TAILLE_PAQUET: usize = 100;

/// Clé du compteur de lots dans le stockage cloisonné.
const CLE_COMPTEUR: &str = "compteur_lots";

/// Préfixe des clés de lot. `kv_list("lot:")` les retrouve toutes.
const PREFIXE_LOT: &str = "lot:";

/// Une piste lue à la source, réduite à ce qui sert à apparier.
#[derive(Debug, Clone)]
struct PisteSource {
    titre: String,
    artiste: String,
    duree_ms: u64,
    isrc: String,
}

/// Le moteur. Ne tient qu'une référence vers l'hôte : tout son état vit dans
/// le stockage clé/valeur, sans quoi une reprise après redémarrage du serveur
/// n'aurait rien à relire.
pub struct Convertisseur<'h, H: Hote + ?Sized> {
    hote: &'h H,
}

impl<'h, H: Hote + ?Sized> Convertisseur<'h, H> {
    pub fn new(hote: &'h H) -> Self {
        Self { hote }
    }

    // -----------------------------------------------------------------------
    // Aperçu — aucune écriture
    // -----------------------------------------------------------------------

    /// Apparier tout le lot et le persister à l'état `apercu`. **Rien n'est
    /// écrit chez un service.**
    pub fn apercu(&self, demande: &Demande) -> Result<Lot, String> {
        self.valider(demande)?;

        let noms = self.noms_des_playlists_source(&demande.source_service)?;
        let lot_id = self.prochain_lot_id()?;

        let mut playlists = Vec::with_capacity(demande.playlists.len());
        for (rang, source_id) in demande.playlists.iter().enumerate() {
            let (source_nom, pistes) = self.lire_la_source(demande, source_id, &noms)?;
            let cible_nom = match demande.suffixe_nom.as_deref() {
                Some(suffixe) if !suffixe.is_empty() => format!("{source_nom}{suffixe}"),
                // Sans suffixe, le nom est repris À L'IDENTIQUE. C'est ce que
                // « transférer à l'identique » veut dire, et le contraire de ce
                // que faisait la route (« … (transferred) »).
                _ => source_nom.clone(),
            };

            let mut appariees = Vec::new();
            let mut introuvables = Vec::new();
            for piste in &pistes {
                match self.apparier(&demande.cible_service, piste) {
                    Ok(c) => appariees.push(Appariee {
                        source_titre: piste.titre.clone(),
                        source_artiste: piste.artiste.clone(),
                        source_duree_ms: piste.duree_ms,
                        cible_id: c.id,
                        cible_titre: c.titre,
                        cible_artiste: c.artiste,
                        cible_duree_ms: c.duree_ms,
                        score: c.score,
                    }),
                    Err(raison) => introuvables.push(Introuvable {
                        source_titre: piste.titre.clone(),
                        source_artiste: piste.artiste.clone(),
                        source_duree_ms: piste.duree_ms,
                        raison,
                    }),
                }
            }

            playlists.push(PlaylistDuLot {
                rang,
                source_playlist_id: source_id.clone(),
                source_nom,
                cible_nom,
                total: pistes.len(),
                appariees,
                introuvables,
                cible_playlist_id: None,
                snapshot_avant: None,
                versees: Vec::new(),
                etat: etat::APERCU.to_string(),
                erreur: None,
            });
        }

        let lot = Lot {
            lot_id,
            source_service: demande.source_service.clone(),
            cible_service: demande.cible_service.clone(),
            etat: etat::APERCU.to_string(),
            playlists,
        };
        self.ecrire_le_lot(&lot)?;
        self.hote.journal(
            "info",
            &format!(
                "apercu du lot {} : {} playlist(s)",
                lot.lot_id,
                lot.playlists.len()
            ),
        );
        Ok(lot)
    }

    // -----------------------------------------------------------------------
    // Transfert et reprise — les seules écritures
    // -----------------------------------------------------------------------

    /// Exécuter un lot précédemment prévisualisé.
    ///
    /// Refuse sans `accord`, et refuse un lot qui n'est plus à l'état `apercu`
    /// (pour reprendre un lot commencé, c'est [`Convertisseur::reprendre`]).
    pub fn transferer(&self, lot_id: &str, accord: bool) -> Result<Lot, String> {
        if !accord {
            return Err(
                "accord_requis : rien n'est écrit chez un service sans accord explicite".into(),
            );
        }
        let lot = self.lire_le_lot(lot_id)?;
        if lot.etat != etat::APERCU {
            return Err(format!(
                "lot_deja_engage : le lot {lot_id} est à l'état « {} » ; utiliser /reprise",
                lot.etat
            ));
        }
        self.verser(lot)
    }

    /// Reprendre un lot interrompu, sans recréer ce qui existe déjà.
    pub fn reprendre(&self, lot_id: &str) -> Result<Lot, String> {
        let lot = self.lire_le_lot(lot_id)?;
        if lot.etat == etat::APERCU {
            return Err(format!(
                "accord_requis : le lot {lot_id} n'a jamais été accepté ; utiliser /transfert"
            ));
        }
        self.verser(lot)
    }

    /// Le corps commun du transfert et de la reprise.
    ///
    /// Une playlist en erreur n'arrête pas le lot : elle est notée, et la
    /// suivante est tentée. Un service qui tombe au milieu d'un lot de trente
    /// playlists ne doit pas coûter les vingt-neuf autres.
    fn verser(&self, mut lot: Lot) -> Result<Lot, String> {
        let cible = lot.cible_service.clone();
        let mut interrompu = false;

        for i in 0..lot.playlists.len() {
            if lot.playlists[i].etat == etat::TERMINE
                || lot.playlists[i].etat == etat::RIEN_A_TRANSFERER
            {
                continue;
            }
            if lot.playlists[i].appariees.is_empty() {
                // On ne crée pas une playlist vide chez un service : elle ne
                // pourrait plus être effacée depuis Tune.
                lot.playlists[i].etat = etat::RIEN_A_TRANSFERER.to_string();
                self.ecrire_la_playlist(&lot.lot_id, &lot.playlists[i])?;
                continue;
            }

            lot.playlists[i].etat = etat::EN_COURS.to_string();
            lot.playlists[i].erreur = None;

            match self.verser_une_playlist(&cible, &lot.lot_id, &mut lot.playlists[i]) {
                Ok(()) => {
                    lot.playlists[i].etat = etat::TERMINE.to_string();
                }
                Err(e) => {
                    interrompu = true;
                    lot.playlists[i].etat = etat::INTERROMPU.to_string();
                    lot.playlists[i].erreur = Some(e.clone());
                    self.hote.journal(
                        "warn",
                        &format!(
                            "lot {} playlist {} interrompue : {e}",
                            lot.lot_id, lot.playlists[i].source_playlist_id
                        ),
                    );
                }
            }
            self.ecrire_la_playlist(&lot.lot_id, &lot.playlists[i])?;
        }

        lot.etat = if interrompu {
            etat::INTERROMPU.to_string()
        } else {
            etat::TERMINE.to_string()
        };
        self.ecrire_l_en_tete(&lot)?;
        Ok(lot)
    }

    /// Créer la playlist si elle n'existe pas encore, puis verser ce qui reste.
    ///
    /// Chaque étape est persistée AVANT la suivante : c'est ce qui rend la
    /// reprise exacte. Si le serveur s'arrête entre la création et le premier
    /// paquet, la reprise retrouve l'identifiant et n'en crée pas un second.
    fn verser_une_playlist(
        &self,
        cible: &str,
        lot_id: &str,
        pl: &mut PlaylistDuLot,
    ) -> Result<(), String> {
        let mut vient_d_etre_creee = false;
        if pl.cible_playlist_id.is_none() {
            let reponse = self.hote.streaming_playlist_create(
                cible,
                &pl.cible_nom,
                Some("Transféré par Tune"),
            )?;
            let id = reponse
                .get("playlist_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "réponse sans playlist_id à la création".to_string())?
                .to_string();
            pl.cible_playlist_id = Some(id);
            // 🔴 Persister TOUT DE SUITE : entre cette ligne et le premier
            // paquet, une coupure laisserait une playlist orpheline que la
            // reprise recréerait — un doublon qu'aucune capacité ne sait
            // effacer.
            self.ecrire_la_playlist(lot_id, pl)?;
            vient_d_etre_creee = true;
        }
        let cible_playlist_id = pl
            .cible_playlist_id
            .clone()
            .expect("identifiant posé juste au-dessus");

        // #4718 — une copie datée de la playlist visée AVANT le premier titre
        // versé. Sans elle, pas d'écriture : c'est ce qui rend le retour en
        // arrière possible. Une playlist que le transfert vient de créer est
        // vide, et on le sait sans la relire (un service peut mettre quelques
        // secondes à la lister) ; une playlist qui existait déjà est lue.
        if pl.snapshot_avant.is_none() {
            let snapshots = Snapshots::new(self.hote);
            let motif = format!("avant_transfert:{lot_id}");
            let entete = if vient_d_etre_creee {
                snapshots.prendre_depuis(cible, &cible_playlist_id, &pl.cible_nom, &[], &motif)?
            } else {
                snapshots.prendre(cible, &cible_playlist_id, Some(&pl.cible_nom), &motif)?
            };
            pl.snapshot_avant = Some(entete.snapshot_id);
            self.ecrire_la_playlist(lot_id, pl)?;
        }

        let restant = pl.restant_a_verser();
        for paquet in restant.chunks(TAILLE_PAQUET) {
            self.hote
                .streaming_playlist_add_tracks(cible, &cible_playlist_id, paquet)?;
            pl.versees.extend(paquet.iter().cloned());
            self.ecrire_la_playlist(lot_id, pl)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Lecture des lots
    // -----------------------------------------------------------------------

    /// Les en-têtes de tous les lots connus, du plus récent au plus ancien.
    pub fn lots(&self) -> Result<Vec<Value>, String> {
        let liste = self.hote.kv_list(PREFIXE_LOT)?;
        let mut entetes = Vec::new();
        for cle in liste
            .get("keys")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(cle) = cle.as_str() else { continue };
            // Les clés de détail (`lot:x:pl:0`) ne sont pas des en-têtes.
            if cle.contains(":pl:") {
                continue;
            }
            if let Some(v) = self.kv_valeur(cle)? {
                entetes.push(v);
            }
        }
        entetes.reverse();
        Ok(entetes)
    }

    /// Un lot complet, en-tête et playlists.
    pub fn lire_le_lot(&self, lot_id: &str) -> Result<Lot, String> {
        let brut = self
            .kv_valeur(&format!("{PREFIXE_LOT}{lot_id}"))?
            .ok_or_else(|| format!("lot_inconnu : {lot_id}"))?;
        let entete: EnTeteLot =
            serde_json::from_value(brut).map_err(|e| format!("en-tête de lot illisible : {e}"))?;
        let mut playlists = Vec::with_capacity(entete.rangs.len());
        for rang in &entete.rangs {
            let brut = self
                .kv_valeur(&Self::cle_playlist(lot_id, *rang))?
                .ok_or_else(|| format!("playlist {rang} manquante dans le lot {lot_id}"))?;
            playlists.push(
                serde_json::from_value(brut)
                    .map_err(|e| format!("playlist {rang} illisible : {e}"))?,
            );
        }
        Ok(Lot {
            lot_id: entete.lot_id,
            source_service: entete.source_service,
            cible_service: entete.cible_service,
            etat: entete.etat,
            playlists,
        })
    }

    // -----------------------------------------------------------------------
    // Internes
    // -----------------------------------------------------------------------

    fn valider(&self, d: &Demande) -> Result<(), String> {
        if d.playlists.is_empty() {
            return Err("aucune playlist demandée".into());
        }
        if d.source_service.is_empty() || d.cible_service.is_empty() {
            return Err("source_service et cible_service sont obligatoires".into());
        }
        if d.cible_service == "local" {
            return Err(
                "cible_locale_non_supportee : l'interface hôte n'expose aucune capacité \
                 d'appariement dans la bibliothèque locale (permission `library`). \
                 Transférer vers la bibliothèque demande cette capacité (#4716)."
                    .into(),
            );
        }
        if d.source_service == d.cible_service {
            return Err("la source et la cible sont le même service".into());
        }
        Ok(())
    }

    /// Les noms des playlists de la source, par identifiant. Un seul appel au
    /// service pour tout le lot ; vide pour une source locale, dont le nom
    /// arrive avec les pistes.
    fn noms_des_playlists_source(&self, source: &str) -> Result<Vec<(String, String)>, String> {
        if source == "local" {
            return Ok(Vec::new());
        }
        let reponse = self.hote.streaming_playlists(source)?;
        Ok(reponse
            .get("playlists")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let id = p
                            .get("source_id")
                            .or_else(|| p.get("id"))
                            .and_then(Value::as_str)?;
                        let nom = p.get("name").and_then(Value::as_str).unwrap_or(id);
                        Some((id.to_string(), nom.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn lire_la_source(
        &self,
        d: &Demande,
        source_id: &str,
        noms: &[(String, String)],
    ) -> Result<(String, Vec<PisteSource>), String> {
        let reponse = if d.source_service == "local" {
            let id: i64 = source_id
                .parse()
                .map_err(|_| format!("identifiant de playlist locale non entier : {source_id}"))?;
            self.hote.playlist_tracks(id)?
        } else {
            self.hote
                .streaming_playlist_tracks(&d.source_service, source_id)?
        };

        let nom = reponse
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                noms.iter()
                    .find(|(id, _)| id == source_id)
                    .map(|(_, n)| n.clone())
            })
            .unwrap_or_else(|| source_id.to_string());

        let pistes = reponse
            .get("tracks")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(Self::piste_source).collect())
            .unwrap_or_default();
        Ok((nom, pistes))
    }

    /// Les pistes locales et les pistes de service partagent leurs clés une
    /// fois sérialisées (`title`, `artist_name`, `duration_ms`, `isrc`) : un
    /// seul lecteur suffit.
    fn piste_source(v: &Value) -> PisteSource {
        PisteSource {
            titre: v
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            artiste: v
                .get("artist_name")
                .or_else(|| v.get("artist"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            duree_ms: v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0),
            isrc: v
                .get("isrc")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// Apparier une piste chez la cible, et appliquer la règle des trois
    /// critères. Une erreur de l'hôte devient une RAISON, pas un arrêt : le
    /// reste de la playlist s'apparie quand même.
    fn apparier(&self, cible: &str, piste: &PisteSource) -> Result<appariement::Candidat, Raison> {
        let reponse = self
            .hote
            .streaming_match_track(
                cible,
                &piste.titre,
                &piste.artiste,
                &piste.isrc,
                piste.duree_ms,
            )
            .map_err(|message| Raison::ServiceEnErreur { message })?;
        appariement::juger(
            piste.duree_ms,
            appariement::candidat_de_la_reponse(&reponse),
        )
    }

    fn prochain_lot_id(&self) -> Result<String, String> {
        let n = self
            .kv_valeur(CLE_COMPTEUR)?
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            + 1;
        self.hote.kv_set(CLE_COMPTEUR, &json!(n))?;
        Ok(format!("lot-{n}"))
    }

    fn cle_playlist(lot_id: &str, rang: usize) -> String {
        format!("{PREFIXE_LOT}{lot_id}:pl:{rang}")
    }

    fn kv_valeur(&self, cle: &str) -> Result<Option<Value>, String> {
        let r = self.hote.kv_get(cle)?;
        if r.get("found").and_then(Value::as_bool) == Some(true) {
            Ok(r.get("value").cloned())
        } else {
            Ok(None)
        }
    }

    fn ecrire_l_en_tete(&self, lot: &Lot) -> Result<(), String> {
        let entete = EnTeteLot {
            lot_id: lot.lot_id.clone(),
            source_service: lot.source_service.clone(),
            cible_service: lot.cible_service.clone(),
            etat: lot.etat.clone(),
            rangs: lot.playlists.iter().map(|p| p.rang).collect(),
        };
        let v = serde_json::to_value(&entete).map_err(|e| e.to_string())?;
        self.hote
            .kv_set(&format!("{PREFIXE_LOT}{}", lot.lot_id), &v)?;
        Ok(())
    }

    fn ecrire_la_playlist(&self, lot_id: &str, pl: &PlaylistDuLot) -> Result<(), String> {
        let v = serde_json::to_value(pl).map_err(|e| e.to_string())?;
        self.hote.kv_set(&Self::cle_playlist(lot_id, pl.rang), &v)?;
        Ok(())
    }

    fn ecrire_le_lot(&self, lot: &Lot) -> Result<(), String> {
        for pl in &lot.playlists {
            self.ecrire_la_playlist(&lot.lot_id, pl)?;
        }
        self.ecrire_l_en_tete(lot)
    }
}
