//! Radio artiste à la demande — #5395 (décisions de Bertrand du 29/09/2026).
//!
//! Le bouton « Radio de l'artiste » ne mélangeait que les titres phares d'UN
//! service. La radio que ce module compose part d'un artiste et de ses voisins,
//! en aléatoire :
//!
//! - **Sources, dans l'ordre** : le service de la fiche d'abord, puis les autres
//!   services actifs ET connectés (`StreamingService::utilisable`), l'API
//!   d'enrichissement, et la bibliothèque locale (artistes du même genre) pour
//!   compléter. Une source suivante n'est interrogée sur les voisins que si les
//!   précédentes n'en ont pas donné assez.
//! - **Part de l'artiste de départ** : [`PART_GRAINE`], environ 20 % des titres.
//! - **Sans fin** : l'auto-lecture de fin de file recharge un lot quand la file
//!   s'épuise sur un titre du dernier lot, grâce au [`ContexteRadioArtiste`]
//!   rangé dans les réglages de la zone (`zone_{id}_radio_artiste`) — il survit
//!   donc au redémarrage, comme les autres réglages `zone_{id}_*`, et part avec
//!   la zone quand on la supprime.
//! - **Pas de doublon** dans une fenêtre de [`FENETRE_ANTI_DOUBLON`] titres, un
//!   titre étant reconnu à son artiste et à son titre normalisés (le même
//!   morceau sur deux services est un doublon), et **pas deux titres du même
//!   artiste d'affilée** quand le lot le permet, y compris d'un lot au suivant.
//! - **Gratuite** : aucune garde Premium (à la différence de `/ai/smart-radio`,
//!   qui reste tel quel).
//!
//! Les sources sont des [`SourceRadio`] : la composition ([`composer_lot`]) se
//! prouve donc sans réseau, avec des sources factices.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::QueueInput;
use crate::streaming::traits::{StreamTrack, StreamingService};

/// Part de l'artiste de départ dans un lot (décision 3 : « environ 20 % »).
pub const PART_GRAINE: f64 = 0.2;
/// Titres par lot : celui de la route, et chaque rechargement.
pub const TAILLE_LOT: usize = 50;
/// Nombre de titres déjà proposés que la radio garde en mémoire pour ne pas
/// les reproposer (environ douze lots).
pub const FENETRE_ANTI_DOUBLON: usize = 300;
/// Voisins retenus au plus par lot.
const MAX_VOISINS: usize = 30;
/// Titres demandés à une source pour un artiste.
const TITRES_PAR_ARTISTE: usize = 20;
/// Albums de l'artiste de départ ouverts au plus par lot. Chaque album coûte
/// un appel au service (`get_album_tracks`) : c'est la borne de ce coût.
pub const MAX_ALBUMS_PAR_LOT: usize = 12;

/// Un titre que la radio peut mettre en file. Un lot en compte 25 : la
/// différence de taille entre les deux variantes ne coûte rien ici.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Candidat {
    Local {
        track_id: i64,
        titre: String,
        artiste: String,
        album: Option<String>,
        duree_ms: i64,
    },
    Service {
        source: String,
        piste: StreamTrack,
    },
}

/// Forme comparable d'un nom : minuscules, sans ce qui est entre parenthèses ou
/// crochets (« (Remastered 2011) », « [Live] »), et sans ponctuation.
pub fn normaliser(s: &str) -> String {
    let mut profondeur = 0usize;
    let mut sortie = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '(' | '[' => profondeur += 1,
            ')' | ']' => profondeur = profondeur.saturating_sub(1),
            _ if profondeur > 0 => {}
            c if c.is_alphanumeric() => sortie.extend(c.to_lowercase()),
            _ => sortie.push(' '),
        }
    }
    sortie.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Candidat {
    pub fn artiste(&self) -> &str {
        match self {
            Candidat::Local { artiste, .. } => artiste,
            Candidat::Service { piste, .. } => &piste.artist,
        }
    }

    pub fn titre(&self) -> &str {
        match self {
            Candidat::Local { titre, .. } => titre,
            Candidat::Service { piste, .. } => &piste.title,
        }
    }

    /// La clé anti-doublon : artiste et titre normalisés. Deux services qui
    /// rendent le même morceau donnent la même clé.
    pub fn cle(&self) -> String {
        format!(
            "{}\u{1f}{}",
            normaliser(self.artiste()),
            normaliser(self.titre())
        )
    }

    /// L'album du titre, pour la diversité par album : son titre normalisé
    /// (le même album sur deux services ou deux éditions se reconnaît), à
    /// défaut son identifiant chez le service. `None` : on ne sait pas.
    pub fn album_cle(&self) -> Option<String> {
        let (titre, id) = match self {
            Candidat::Local { album, .. } => (album.as_deref(), None),
            Candidat::Service { piste, .. } => (piste.album.as_deref(), piste.album_id.as_deref()),
        };
        titre
            .map(normaliser)
            .filter(|t| !t.is_empty())
            .or_else(|| id.filter(|i| !i.is_empty()).map(|i| format!("id:{i}")))
    }

    /// L'identité de la ligne de file : `local:<id>` ou `<service>:<id>`.
    /// C'est elle que la fin de file compare pour savoir si la radio continue.
    pub fn identite_file(&self) -> String {
        match self {
            Candidat::Local { track_id, .. } => identite_locale(*track_id),
            Candidat::Service { source, piste } => identite_service(source, &piste.id),
        }
    }

    pub fn en_entree_de_file(&self) -> QueueInput {
        match self {
            Candidat::Local { track_id, .. } => QueueInput::Local {
                track_id: *track_id,
            },
            Candidat::Service { source, piste } => QueueInput::Streaming {
                source: source.clone(),
                source_id: piste.id.clone(),
                title: piste.title.clone(),
                artist: piste.artist.clone(),
                album: piste.album.clone(),
                cover_url: piste.cover_path.clone(),
                duration_ms: piste.duration_ms as i64,
                track_number: piste.track_number.map(i64::from),
                disc_number: piste.disc_number.map(i64::from),
                album_ref: piste.album_id.clone(),
                artist_ref: piste.artist_id.clone(),
            },
        }
    }

    /// La forme JSON rendue par la route et l'évènement d'ajout.
    pub fn en_json(&self) -> serde_json::Value {
        match self {
            Candidat::Local {
                track_id,
                titre,
                artiste,
                album,
                duree_ms,
            } => serde_json::json!({
                "source": "local",
                "track_id": track_id,
                "title": titre,
                "artist_name": artiste,
                "album_title": album,
                "duration_ms": duree_ms,
            }),
            Candidat::Service { source, piste } => serde_json::json!({
                "source": source,
                "source_id": piste.id,
                "title": piste.title,
                "artist_name": piste.artist,
                "album_title": piste.album,
                "duration_ms": piste.duration_ms,
                "cover_path": piste.cover_path,
            }),
        }
    }
}

pub fn identite_locale(track_id: i64) -> String {
    format!("local:{track_id}")
}

pub fn identite_service(source: &str, source_id: &str) -> String {
    format!("{source}:{source_id}")
}

/// Un album DE l'artiste (pas un album où il n'est qu'invité), tel qu'une
/// source sait le rouvrir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumRadio {
    pub id: String,
    pub titre: String,
}

/// Une source de la radio : elle nomme des voisins, et elle rend des titres
/// d'un artiste. Une source peut ne savoir faire que l'un des deux (l'API
/// d'enrichissement ne rend aucun titre).
///
/// La DISCOGRAPHIE (`albums_de` / `titres_album`, #5395 point 6 du fil 2037)
/// sert la part de l'artiste de départ. Par défaut une source n'en a pas :
/// seules la fiche (son service) et la bibliothèque la fournissent.
#[async_trait::async_trait]
pub trait SourceRadio: Send + Sync {
    fn nom(&self) -> String;
    async fn artistes_similaires(&self, artiste: &str, max: usize) -> Vec<String>;
    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat>;
    /// Les albums DE l'artiste — ceux où il n'est qu'invité sont écartés.
    async fn albums_de(&self, _artiste: &str) -> Vec<AlbumRadio> {
        Vec::new()
    }
    /// Les titres de l'artiste sur l'un de ses albums.
    async fn titres_album(&self, _artiste: &str, _album: &AlbumRadio) -> Vec<Candidat> {
        Vec::new()
    }
}

/// Tirage pseudo-aléatoire sans dépendance (`xorshift64`, comme
/// `generate_shuffle_order`). Graine fixe en test, graine du système sinon.
pub struct Alea(u64);

impl Alea {
    pub fn fixe(graine: u64) -> Self {
        Alea(graine | 1)
    }

    pub fn du_systeme() -> Self {
        let mut octets = [0u8; 8];
        let graine = if getrandom::getrandom(&mut octets).is_ok() {
            u64::from_le_bytes(octets)
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
        };
        Alea::fixe(graine)
    }

    fn suivant(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn melanger<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.suivant() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
}

/// Ce que [`composer_lot`] rend : les titres dans l'ordre de lecture, et de
/// quoi journaliser d'où viennent les voisins.
#[derive(Debug, Default)]
pub struct Lot {
    pub candidats: Vec<Candidat>,
    /// Nombre de voisins retenus, par source, dans l'ordre des sources.
    pub voisins_par_source: Vec<(String, usize)>,
    /// Titres de l'artiste de départ dans le lot.
    pub titres_graine: usize,
}

/// Compose un lot de `taille` titres autour de `graine`.
///
/// `exclure` : clés ([`Candidat::cle`]) à ne pas reproposer — la fenêtre des
/// titres déjà passés et la file en cours. `dernier_artiste` : l'artiste du
/// titre qui précède le lot, pour ne pas le répéter en tête.
/// `commencer_par_la_graine` : le premier lot s'ouvre sur l'artiste demandé.
pub async fn composer_lot(
    graine: &str,
    sources: &[Box<dyn SourceRadio>],
    taille: usize,
    exclure: &HashSet<String>,
    dernier_artiste: Option<&str>,
    commencer_par_la_graine: bool,
    alea: &mut Alea,
) -> Lot {
    let graine = graine.trim();
    if graine.is_empty() || taille == 0 {
        return Lot::default();
    }
    let graine_norm = normaliser(graine);

    // 1. Les voisins, source après source, jusqu'à en avoir assez.
    let mut voisins: Vec<String> = Vec::new();
    let mut vus: HashSet<String> = HashSet::from([graine_norm.clone()]);
    let mut voisins_par_source = Vec::new();
    for source in sources {
        if voisins.len() >= MAX_VOISINS {
            break;
        }
        let mut n = 0usize;
        for nom in source.artistes_similaires(graine, MAX_VOISINS).await {
            let nom = nom.trim().to_string();
            if nom.is_empty() || !vus.insert(normaliser(&nom)) {
                continue;
            }
            voisins.push(nom);
            n += 1;
            if voisins.len() >= MAX_VOISINS {
                break;
            }
        }
        voisins_par_source.push((source.nom(), n));
    }

    let mut deja: HashSet<String> = exclure.clone();
    let prendre = |c: &Candidat, deja: &mut HashSet<String>| deja.insert(c.cle());

    // 2. Les titres des voisins : chaque voisin est cherché dans les sources
    //    dans l'ordre (le service de la fiche d'abord), toutes les recherches
    //    en parallèle — une par voisin, pas une par titre.
    let voulus_graine = if voisins.is_empty() {
        taille
    } else {
        ((taille as f64 * PART_GRAINE).round() as usize).clamp(1, taille)
    };
    let voulus_voisins = taille - voulus_graine;
    alea.melanger(&mut voisins);
    let pools = futures_util::future::join_all(voisins.iter().map(|nom| async move {
        for source in sources {
            let titres = source.titres_de(nom, TITRES_PAR_ARTISTE).await;
            if !titres.is_empty() {
                return titres;
            }
        }
        Vec::new()
    }))
    .await;
    let mut pools: Vec<Vec<Candidat>> = pools
        .into_iter()
        .map(|mut p| {
            alea.melanger(&mut p);
            p
        })
        .collect();
    // Un titre par voisin, puis un deuxième tour s'il en manque : une radio,
    // pas la discographie d'un seul voisin.
    let mut titres_voisins: Vec<Candidat> = Vec::new();
    // Les albums déjà pris chez chaque voisin : son deuxième titre vient d'un
    // AUTRE album quand il en a un.
    let mut albums_pris: Vec<HashSet<String>> = vec![HashSet::new(); pools.len()];
    'tours: loop {
        let mut avance = false;
        for (pool, pris) in pools.iter_mut().zip(albums_pris.iter_mut()) {
            if titres_voisins.len() >= voulus_voisins {
                break 'tours;
            }
            while !pool.is_empty() {
                let i = pool
                    .iter()
                    .rposition(|c| c.album_cle().is_none_or(|a| !pris.contains(&a)))
                    .unwrap_or(pool.len() - 1);
                let c = pool.remove(i);
                if prendre(&c, &mut deja) {
                    if let Some(a) = c.album_cle() {
                        pris.insert(a);
                    }
                    titres_voisins.push(c);
                    avance = true;
                    break;
                }
            }
        }
        if !avance {
            break;
        }
    }

    // 3. Les titres de l'artiste de départ — sa part, élargie de ce que les
    //    voisins n'ont pas pu fournir. Ils puisent dans TOUTE sa discographie
    //    (fil 2037, point 6) : la première source qui a ses albums (la fiche,
    //    à défaut la bibliothèque), au plus un titre par album et par lot tant
    //    qu'il reste des albums. Les titres phares ne servent qu'à compléter.
    let voulus_graine = voulus_graine + voulus_voisins.saturating_sub(titres_voisins.len());
    let mut titres_graine: Vec<Candidat> = Vec::new();
    let est_de_la_graine = |c: &Candidat| normaliser(c.artiste()) == graine_norm;
    for source in sources {
        let mut albums = source.albums_de(graine).await;
        if albums.is_empty() {
            continue;
        }
        alea.melanger(&mut albums);
        // Un album de plus que de titres voulus, pour absorber un album vide
        // ou déjà entendu, et jamais plus que la borne d'appels.
        albums.truncate((voulus_graine + 2).min(MAX_ALBUMS_PAR_LOT));
        let par_album =
            futures_util::future::join_all(albums.iter().map(|al| source.titres_album(graine, al)))
                .await;
        let mut par_album: Vec<Vec<Candidat>> = par_album
            .into_iter()
            .map(|mut p| {
                p.retain(|c| est_de_la_graine(c));
                alea.melanger(&mut p);
                p
            })
            .collect();
        'albums: loop {
            let mut avance = false;
            for pool in par_album.iter_mut() {
                if titres_graine.len() >= voulus_graine {
                    break 'albums;
                }
                while let Some(c) = pool.pop() {
                    if prendre(&c, &mut deja) {
                        titres_graine.push(c);
                        avance = true;
                        break;
                    }
                }
            }
            if !avance {
                break;
            }
        }
        break;
    }
    for source in sources {
        if titres_graine.len() >= voulus_graine {
            break;
        }
        let mut titres = source.titres_de(graine, TITRES_PAR_ARTISTE).await;
        alea.melanger(&mut titres);
        for c in titres {
            if titres_graine.len() >= voulus_graine {
                break;
            }
            // Un titre phare d'un service peut être signé d'un autre artiste
            // (featuring, compilation) : on ne garde que ceux de la graine.
            if est_de_la_graine(&c) && prendre(&c, &mut deja) {
                titres_graine.push(c);
            }
        }
    }
    let n_graine = titres_graine.len();

    // 4. L'ordre : mélangé, puis étalé pour que deux titres du même artiste
    //    ne se suivent pas quand c'est possible.
    let premier = if commencer_par_la_graine && !titres_graine.is_empty() {
        Some(titres_graine.remove(0))
    } else {
        None
    };
    let mut tous: Vec<Candidat> = titres_graine.into_iter().chain(titres_voisins).collect();
    alea.melanger(&mut tous);
    let mut precedent = premier
        .as_ref()
        .map(|c| normaliser(c.artiste()))
        .or_else(|| dernier_artiste.map(normaliser));
    let mut candidats: Vec<Candidat> = premier.into_iter().collect();
    candidats.extend(etaler(tous, &mut precedent));
    candidats.truncate(taille);
    Lot {
        candidats,
        voisins_par_source,
        titres_graine: n_graine,
    }
}

/// Ordonne `restants` (déjà mélangés) pour que ni un artiste ni un album ne
/// se suivent, quand c'est possible. À chaque pas on prend le premier titre
/// d'un artiste ET d'un album différents du précédent — l'ordre reste celui du
/// tirage —, à défaut d'un artiste différent ; sauf quand un artiste a plus de
/// titres que tous les autres réunis : il passe alors en premier, sans quoi la
/// fin du lot serait une série de lui seul.
fn etaler(mut restants: Vec<Candidat>, precedent: &mut Option<String>) -> Vec<Candidat> {
    let mut sortie = Vec::with_capacity(restants.len());
    let mut album_precedent: Option<String> = None;
    while !restants.is_empty() {
        let artistes: Vec<String> = restants.iter().map(|c| normaliser(c.artiste())).collect();
        let albums: Vec<Option<String>> = restants.iter().map(Candidat::album_cle).collect();
        let mut comptes: HashMap<&str, usize> = HashMap::new();
        for a in &artistes {
            *comptes.entry(a.as_str()).or_default() += 1;
        }
        let n = artistes.len();
        let dominant = comptes
            .iter()
            .filter(|(a, c)| **c * 2 > n && precedent.as_deref() != Some(**a))
            .map(|(a, _)| a.to_string())
            .next();
        let autre_album = |i: usize| {
            album_precedent.is_none() || albums[i].is_none() || albums[i] != album_precedent
        };
        // Même règle pour un album qui tiendrait plus de la moitié du reste
        // (une compilation commune à plusieurs voisins) : il passe dès qu'il
        // le peut, sinon il finirait en série.
        let mut comptes_albums: HashMap<&str, usize> = HashMap::new();
        for a in albums.iter().flatten() {
            *comptes_albums.entry(a.as_str()).or_default() += 1;
        }
        let album_dominant = comptes_albums
            .iter()
            .filter(|(a, c)| **c * 2 >= n && album_precedent.as_deref() != Some(**a))
            .map(|(a, _)| a.to_string())
            .next();
        let autre_artiste = |i: usize| precedent.as_deref() != Some(artistes[i].as_str());
        let choix = match dominant {
            Some(d) => (0..n)
                .find(|&i| artistes[i] == d && autre_album(i))
                .or_else(|| artistes.iter().position(|a| *a == d)),
            None => album_dominant
                .as_ref()
                .and_then(|d| (0..n).find(|&i| albums[i].as_ref() == Some(d) && autre_artiste(i)))
                .or_else(|| (0..n).find(|&i| autre_artiste(i) && autre_album(i)))
                .or_else(|| (0..n).find(|&i| autre_artiste(i)))
                .or_else(|| (0..n).find(|&i| autre_album(i))),
        }
        .unwrap_or(0);
        let c = restants.remove(choix);
        *precedent = Some(artistes[choix].clone());
        album_precedent = albums[choix].clone();
        sortie.push(c);
    }
    sortie
}

// ── Le contexte de la radio, rangé dans les réglages de la zone ─────────────

/// Ce que l'auto-lecture doit savoir pour CONTINUER une radio artiste.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContexteRadioArtiste {
    /// L'artiste de départ.
    pub artiste: String,
    /// Le service de la fiche (`None` : fiche de bibliothèque).
    #[serde(default)]
    pub service: Option<String>,
    /// L'identifiant de l'artiste sur ce service, quand la fiche le portait.
    #[serde(default)]
    pub artiste_id: Option<String>,
    /// Clés des titres déjà proposés, les plus récents à la fin.
    #[serde(default)]
    pub deja_proposes: Vec<String>,
    /// Identités de file ([`Candidat::identite_file`]) du dernier lot : la
    /// radio ne continue que si la file s'achève sur l'un d'eux.
    #[serde(default)]
    pub dernier_lot: Vec<String>,
    /// L'artiste du dernier titre du dernier lot.
    #[serde(default)]
    pub dernier_artiste: Option<String>,
    /// Lots composés depuis le départ (1 = celui de la route).
    #[serde(default)]
    pub lots: u32,
}

impl ContexteRadioArtiste {
    pub fn nouveau(artiste: &str, service: Option<&str>, artiste_id: Option<&str>) -> Self {
        ContexteRadioArtiste {
            artiste: artiste.trim().to_string(),
            service: service
                .filter(|s| !s.is_empty() && *s != "local")
                .map(str::to_owned),
            artiste_id: artiste_id.filter(|s| !s.is_empty()).map(str::to_owned),
            ..Default::default()
        }
    }

    /// La file s'achève-t-elle sur un titre de la radio ?
    pub fn continue_sur(&self, identite: &str) -> bool {
        self.dernier_lot.iter().any(|i| i == identite)
    }

    /// Consigne un lot : ses clés entrent dans la fenêtre anti-doublon.
    pub fn consigner(&mut self, lot: &[Candidat]) {
        self.deja_proposes.extend(lot.iter().map(Candidat::cle));
        let trop = self
            .deja_proposes
            .len()
            .saturating_sub(FENETRE_ANTI_DOUBLON);
        self.deja_proposes.drain(..trop);
        self.dernier_lot = lot.iter().map(Candidat::identite_file).collect();
        self.dernier_artiste = lot.last().map(|c| c.artiste().to_string());
        self.lots += 1;
    }
}

pub fn cle_de_reglage(zone_id: i64) -> String {
    format!("zone_{zone_id}_radio_artiste")
}

pub fn lire_contexte(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<ContexteRadioArtiste> {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(&cle_de_reglage(zone_id))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
}

pub fn ecrire_contexte(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    contexte: &ContexteRadioArtiste,
) -> Result<(), String> {
    let json = serde_json::to_string(contexte).map_err(|e| e.to_string())?;
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set(&cle_de_reglage(zone_id), &json)
}

pub fn effacer_contexte(db: &Arc<dyn DbBackend>, zone_id: i64) {
    let _ = crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .delete(&cle_de_reglage(zone_id));
}

/// Les clés des titres déjà dans la file de la zone.
pub fn cles_de_la_file(db: &Arc<dyn DbBackend>, zone_id: i64) -> HashSet<String> {
    crate::db::play_queue_repo::PlayQueueRepo::with_backend(db.clone())
        .get_ordered(zone_id)
        .unwrap_or_default()
        .into_iter()
        .map(|e| {
            format!(
                "{}\u{1f}{}",
                normaliser(e.artist_name.as_deref().unwrap_or("")),
                normaliser(e.title.as_deref().unwrap_or(""))
            )
        })
        .collect()
}

// ── Les sources réelles ─────────────────────────────────────────────────────

type Service = Arc<tokio::sync::RwLock<Box<dyn StreamingService>>>;

/// Un service de streaming comme source : ses voisins par
/// `get_similar_artists`, les titres d'un artiste par ses titres phares.
/// L'artiste est RÉSOLU par une recherche au nom exact
/// (`auto_dj::pick_seed_artist_id`) et jamais deviné : sans correspondance
/// exacte, le service ne rend rien pour lui plutôt qu'un homonyme.
pub struct SourceService {
    nom: String,
    service: Service,
    ids: std::sync::Mutex<HashMap<String, Option<String>>>,
    bannis: HashSet<String>,
    /// Vrai pour le service de la FICHE seulement : c'est lui qui fournit la
    /// discographie de l'artiste de départ (décision du 29/09, fil 2037).
    discographie: bool,
}

impl SourceService {
    pub fn new(
        nom: &str,
        service: Service,
        bannis: HashSet<String>,
        artiste_connu: Option<(&str, &str)>,
    ) -> Self {
        let mut ids = HashMap::new();
        if let Some((artiste, id)) = artiste_connu {
            ids.insert(normaliser(artiste), Some(id.to_string()));
        }
        SourceService {
            nom: nom.to_string(),
            service,
            ids: std::sync::Mutex::new(ids),
            bannis,
            discographie: false,
        }
    }

    /// Ce service est celui de la fiche : il fournit la discographie.
    pub fn avec_discographie(mut self) -> Self {
        self.discographie = true;
        self
    }

    fn en_candidats(&self, pistes: Vec<StreamTrack>, max: usize) -> Vec<Candidat> {
        pistes
            .into_iter()
            .filter(|p| {
                !p.id.is_empty() && p.disponible != Some(false) && !self.bannis.contains(&p.id)
            })
            .take(max)
            .map(|piste| Candidat::Service {
                source: self.nom.clone(),
                piste,
            })
            .collect()
    }

    fn id_en_cache(&self, artiste: &str) -> Option<Option<String>> {
        self.ids
            .lock()
            .ok()
            .and_then(|m| m.get(&normaliser(artiste)).cloned())
    }

    fn retenir(&self, artiste: &str, id: Option<String>) {
        if let Ok(mut m) = self.ids.lock() {
            m.entry(normaliser(artiste)).or_insert(id);
        }
    }

    async fn id_de(&self, artiste: &str) -> Option<String> {
        if let Some(connu) = self.id_en_cache(artiste) {
            return connu;
        }
        let trouves = match self.service.read().await.search(artiste, 10).await {
            Ok(r) => r.artists,
            Err(e) => {
                tracing::debug!(service = %self.nom, artiste, error = %e, "radio_artiste_recherche_echouee");
                Vec::new()
            }
        };
        let id = crate::playback::auto_dj::pick_seed_artist_id(&trouves, artiste);
        self.retenir(artiste, id.clone());
        id
    }
}

#[async_trait::async_trait]
impl SourceRadio for SourceService {
    fn nom(&self) -> String {
        self.nom.clone()
    }

    async fn artistes_similaires(&self, artiste: &str, max: usize) -> Vec<String> {
        let Some(id) = self.id_de(artiste).await else {
            return Vec::new();
        };
        let voisins = match self
            .service
            .read()
            .await
            .get_similar_artists(&id, max)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(service = %self.nom, artiste, error = %e, "radio_artiste_similaires_echoues");
                Vec::new()
            }
        };
        voisins
            .into_iter()
            .filter(|a| !a.name.trim().is_empty() && !a.id.is_empty())
            .map(|a| {
                // L'identifiant du voisin est connu : ses titres se
                // demanderont sans recherche par nom.
                self.retenir(&a.name, Some(a.id.clone()));
                a.name.trim().to_string()
            })
            .collect()
    }

    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat> {
        let Some(id) = self.id_de(artiste).await else {
            return Vec::new();
        };
        let pistes = match self.service.read().await.get_artist_top_tracks(&id).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(service = %self.nom, artiste, error = %e, "radio_artiste_titres_echoues");
                Vec::new()
            }
        };
        self.en_candidats(pistes, max)
    }

    /// Les albums DE l'artiste chez le service : `get_artist_albums` rend aussi
    /// ceux où il n'est qu'invité (compilations, featuring) ; un album signé
    /// d'un AUTRE artiste — identifiant ou nom — est écarté. Un album sans
    /// artiste déclaré est gardé : ses titres sont filtrés un par un sur
    /// l'artiste (`composer_lot`). Un seul appel.
    async fn albums_de(&self, artiste: &str) -> Vec<AlbumRadio> {
        if !self.discographie {
            return Vec::new();
        }
        let Some(id) = self.id_de(artiste).await else {
            return Vec::new();
        };
        let albums = match self.service.read().await.get_artist_albums(&id).await {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(service = %self.nom, artiste, error = %e, "radio_artiste_albums_echoues");
                Vec::new()
            }
        };
        let nom = normaliser(artiste);
        albums
            .into_iter()
            .filter(|al| !al.id.is_empty())
            .filter(
                |al| match al.artist_id.as_deref().filter(|a| !a.is_empty()) {
                    Some(aid) => aid == id,
                    None => al.artist.trim().is_empty() || normaliser(&al.artist) == nom,
                },
            )
            .map(|al| AlbumRadio {
                id: al.id,
                titre: al.title,
            })
            .collect()
    }

    /// Les titres d'un album : un appel par album ouvert, borné par
    /// `MAX_ALBUMS_PAR_LOT`.
    async fn titres_album(&self, artiste: &str, album: &AlbumRadio) -> Vec<Candidat> {
        let pistes = match self.service.read().await.get_album_tracks(&album.id).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(service = %self.nom, artiste, album = %album.id, error = %e, "radio_artiste_album_echoue");
                Vec::new()
            }
        };
        let pistes = pistes
            .into_iter()
            .map(|mut p| {
                // Une piste d'album ne porte pas toujours le titre de son album.
                if p.album.as_deref().is_none_or(|a| a.is_empty()) {
                    p.album = Some(album.titre.clone());
                }
                if p.album_id.is_none() {
                    p.album_id = Some(album.id.clone());
                }
                p
            })
            .collect();
        self.en_candidats(pistes, usize::MAX)
    }
}

/// La bibliothèque locale : les titres d'un artiste qu'elle connaît, et en
/// voisins les artistes du genre dominant de l'artiste de départ.
pub struct SourceBibliotheque {
    db: Arc<dyn DbBackend>,
}

impl SourceBibliotheque {
    pub fn new(db: Arc<dyn DbBackend>) -> Self {
        SourceBibliotheque { db }
    }
}

#[async_trait::async_trait]
impl SourceRadio for SourceBibliotheque {
    fn nom(&self) -> String {
        "local".into()
    }

    async fn artistes_similaires(&self, artiste: &str, max: usize) -> Vec<String> {
        let nom = artiste.trim().to_lowercase();
        let genre = self
            .db
            .query_one(
                "SELECT t.genre FROM tracks t JOIN artists ar ON t.artist_id = ar.id \
                 WHERE LOWER(ar.name) = ?1 AND t.genre IS NOT NULL AND t.genre <> '' \
                 GROUP BY t.genre ORDER BY COUNT(*) DESC, t.genre LIMIT 1",
                &[&nom],
            )
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_string()));
        let Some(genre) = genre else {
            return Vec::new();
        };
        let limite = max as i64;
        self.db
            .query_many(
                "SELECT ar.name FROM tracks t JOIN artists ar ON t.artist_id = ar.id \
                 WHERE t.genre = ?1 AND LOWER(ar.name) <> ?2 \
                 GROUP BY ar.name ORDER BY RANDOM() LIMIT ?3",
                &[&genre, &nom, &limite],
            )
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| r.first().and_then(|v| v.as_string()))
            .collect()
    }

    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat> {
        crate::playback::auto_dj::tracks_for_artist_names(
            &self.db,
            &[artiste.to_string()],
            max,
            max,
        )
        .into_iter()
        .filter_map(|t| {
            Some(Candidat::Local {
                track_id: t["track_id"].as_i64().filter(|id| *id > 0)?,
                titre: t["title"].as_str().unwrap_or_default().to_string(),
                artiste: t["artist"].as_str().unwrap_or_default().to_string(),
                album: t["album"].as_str().map(str::to_owned),
                duree_ms: t["duration_ms"].as_i64().unwrap_or(0),
            })
        })
        .collect()
    }

    /// Les albums DE l'artiste dans la bibliothèque : ceux dont l'artiste
    /// d'album est lui. Une compilation ou l'album d'un autre où il est invité
    /// a un autre artiste d'album : écartée.
    async fn albums_de(&self, artiste: &str) -> Vec<AlbumRadio> {
        let nom = artiste.trim().to_lowercase();
        self.db
            .query_many(
                "SELECT al.id, al.title FROM albums al JOIN artists aa ON al.artist_id = aa.id \
                 WHERE LOWER(aa.name) = ?1 ORDER BY al.id",
                &[&nom],
            )
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                Some(AlbumRadio {
                    id: r.first()?.as_i64()?.to_string(),
                    titre: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                })
            })
            .collect()
    }

    async fn titres_album(&self, artiste: &str, album: &AlbumRadio) -> Vec<Candidat> {
        let Ok(album_id) = album.id.parse::<i64>() else {
            return Vec::new();
        };
        let nom = artiste.trim().to_lowercase();
        let sans_bannis = crate::db::facet_filter::banned_tracks_excluded(
            crate::db::hidden_repo::profil_de_selection_automatique(&self.db),
        );
        let sql = format!(
            "SELECT t.id, t.title, ar.name, t.duration_ms FROM tracks t \
             JOIN artists ar ON t.artist_id = ar.id \
             WHERE t.album_id = ?1 AND LOWER(ar.name) = ?2 AND {sans_bannis}"
        );
        self.db
            .query_many(&sql, &[&album_id, &nom])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                Some(Candidat::Local {
                    track_id: r.first()?.as_i64()?,
                    titre: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    artiste: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    album: Some(album.titre.clone()),
                    duree_ms: r.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
                })
            })
            .collect()
    }
}

/// L'API d'enrichissement (mozaiklabs, par MBID) : des voisins, aucun titre.
pub struct SourceEnrichissement {
    db: Arc<dyn DbBackend>,
}

impl SourceEnrichissement {
    pub fn new(db: Arc<dyn DbBackend>) -> Self {
        SourceEnrichissement { db }
    }
}

#[async_trait::async_trait]
impl SourceRadio for SourceEnrichissement {
    fn nom(&self) -> String {
        "enrichissement".into()
    }

    async fn artistes_similaires(&self, artiste: &str, max: usize) -> Vec<String> {
        crate::playback::auto_dj::similar_artist_names(&self.db, artiste, max).await
    }

    async fn titres_de(&self, _artiste: &str, _max: usize) -> Vec<Candidat> {
        Vec::new()
    }
}

/// Les sources d'une radio, dans l'ordre de la décision 2 : le service de la
/// fiche, les autres services actifs et connectés (par ordre alphabétique,
/// pour qu'un lot ne dépende pas de l'ordre d'une table de hachage),
/// l'enrichissement, la bibliothèque.
pub async fn sources_de_la_radio(
    db: &Arc<dyn DbBackend>,
    services: &Registre,
    contexte: &ContexteRadioArtiste,
) -> Vec<Box<dyn SourceRadio>> {
    let (mut noms, registre): (Vec<String>, Vec<(String, Service)>) = {
        let reg = services.lock().await;
        let noms = reg.list();
        let registre = noms
            .iter()
            .filter_map(|n| reg.get(n).map(|s| (n.clone(), s)))
            .collect();
        (noms, registre)
    };
    noms.sort();
    let par_nom: HashMap<String, Service> = registre.into_iter().collect();
    let fiche = contexte.service.clone();
    let mut ordre: Vec<String> = Vec::new();
    if let Some(ref f) = fiche
        && par_nom.contains_key(f)
    {
        ordre.push(f.clone());
    }
    ordre.extend(noms.into_iter().filter(|n| Some(n) != fiche.as_ref()));

    let mut sources: Vec<Box<dyn SourceRadio>> = Vec::new();
    for nom in ordre {
        let Some(service) = par_nom.get(&nom).cloned() else {
            continue;
        };
        if !service.read().await.utilisable().await {
            continue;
        }
        let mut bannis = HashSet::new();
        crate::db::hidden_repo::exclure_les_titres_de_service_bannis(db, &nom, &mut bannis);
        let connu = (Some(&nom) == fiche.as_ref())
            .then_some(contexte.artiste_id.as_deref())
            .flatten()
            .map(|id| (contexte.artiste.as_str(), id));
        let source = SourceService::new(&nom, service, bannis, connu);
        // La discographie de l'artiste de départ vient de la fiche seule.
        let source = if Some(&nom) == fiche.as_ref() {
            source.avec_discographie()
        } else {
            source
        };
        sources.push(Box::new(source));
    }
    sources.push(Box::new(SourceEnrichissement::new(db.clone())));
    sources.push(Box::new(SourceBibliotheque::new(db.clone())));
    sources
}

/// Le lot suivant d'une radio : compose, et consigne dans `contexte`.
/// `premier` : le lot de la route (il s'ouvre sur l'artiste de départ).
pub async fn lot_suivant(
    db: &Arc<dyn DbBackend>,
    services: &Registre,
    zone_id: i64,
    contexte: &mut ContexteRadioArtiste,
    premier: bool,
) -> Lot {
    let sources = sources_de_la_radio(db, services, contexte).await;
    let mut exclure: HashSet<String> = contexte.deja_proposes.iter().cloned().collect();
    if !premier {
        exclure.extend(cles_de_la_file(db, zone_id));
    }
    let lot = composer_lot(
        &contexte.artiste,
        &sources,
        TAILLE_LOT,
        &exclure,
        contexte.dernier_artiste.as_deref(),
        premier,
        &mut Alea::du_systeme(),
    )
    .await;
    tracing::info!(
        zone_id,
        artiste = %contexte.artiste,
        service = ?contexte.service,
        titres = lot.candidats.len(),
        titres_graine = lot.titres_graine,
        voisins = ?lot.voisins_par_source,
        lot = contexte.lots + 1,
        "radio_artiste_lot"
    );
    contexte.consigner(&lot.candidats);
    lot
}

type Registre = Arc<tokio::sync::Mutex<crate::streaming::registry::ServiceRegistry>>;

/// Départ d'une radio (la route) : le premier lot, et le contexte qui la
/// fera continuer. Le contexte n'est écrit que si le lot n'est pas vide — une
/// radio muette ne doit pas détourner l'auto-lecture de la zone.
pub async fn demarrer(
    db: &Arc<dyn DbBackend>,
    services: &Registre,
    zone_id: i64,
    artiste: &str,
    service: Option<&str>,
    artiste_id: Option<&str>,
) -> Lot {
    let mut contexte = ContexteRadioArtiste::nouveau(artiste, service, artiste_id);
    let lot = lot_suivant(db, services, zone_id, &mut contexte, true).await;
    if !lot.candidats.is_empty()
        && let Err(e) = ecrire_contexte(db, zone_id, &contexte)
    {
        tracing::warn!(zone_id, error = %e, "radio_artiste_contexte_non_ecrit");
    }
    lot
}

/// Fin de file : la radio continue-t-elle ? Oui si la zone porte un contexte
/// de radio artiste ET que la file s'achève sur un titre de son dernier lot
/// (`identites_fin`, voir [`identite_locale`] / [`identite_service`]). Une
/// file qui s'achève sur autre chose — l'auditeur a lancé un album depuis —
/// n'est plus la radio : le contexte est effacé et l'auto-lecture habituelle
/// reprend la main (`None`). `None` aussi quand aucun titre n'a été trouvé.
pub async fn continuer(
    db: &Arc<dyn DbBackend>,
    services: &Registre,
    zone_id: i64,
    identites_fin: &[String],
) -> Option<Lot> {
    let mut contexte = lire_contexte(db, zone_id)?;
    if !identites_fin.iter().any(|i| contexte.continue_sur(i)) {
        tracing::info!(zone_id, artiste = %contexte.artiste, "radio_artiste_quittee");
        effacer_contexte(db, zone_id);
        return None;
    }
    let lot = lot_suivant(db, services, zone_id, &mut contexte, false).await;
    if lot.candidats.is_empty() {
        return None;
    }
    if let Err(e) = ecrire_contexte(db, zone_id, &contexte) {
        tracing::warn!(zone_id, error = %e, "radio_artiste_contexte_non_ecrit");
    }
    Some(lot)
}

#[cfg(test)]
#[path = "radio_artiste_tests.rs"]
mod tests;
