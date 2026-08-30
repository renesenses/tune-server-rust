use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Plafond de la lecture aléatoire — un RÉGLAGE, plus une constante (#2901)
// ---------------------------------------------------------------------------
//
// Ce plafond a été posé pour fermer un gel d'interface : une file de 30 000
// pistes rendait le client web inutilisable (Jean Valjean, fil 1096, #2228).
// Il a ensuite bloqué le besoin INVERSE : william veut lire une sélection de
// plus de 2 400 pistes et se fait tronquer à 500 (fil 1620, #2901). Les deux
// demandes sont réelles et opposées ; un réglage les réconcilie, chacun
// choisissant pour SA bibliothèque et SON client.
//
// Le défaut ne bouge pas : 500. Qui n'y touche pas garde exactement le
// comportement de #2228 — c'est la garantie donnée à Jean Valjean, et elle ne
// dépend d'aucune action de sa part.
//
// Le plafond du réglage lui-même est MESURÉ, pas choisi. Sur Shrek, sur une
// bibliothèque SQLite de 30 000 pistes, coût serveur d'une lecture aléatoire
// (médiane de 3 tours, profil release) :
//
//     N      tirage   mélange   set_queue   TOTAL     relecture   JSON file
//     500    15,4 ms   12 µs     19,8 ms    35,1 ms     6,5 ms      231 Ko
//     1 000   8,9 ms   13 µs     19,7 ms    28,6 ms     6,7 ms      463 Ko
//     2 000  11,9 ms   26 µs     39,5 ms    51,4 ms    12,1 ms      927 Ko
//     5 000  14,6 ms   46 µs     73,9 ms    88,5 ms    23,7 ms    2 323 Ko
//    10 000  20,3 ms   91 µs    146,9 ms   167,2 ms    38,3 ms    4 646 Ko
//    30 000  28,9 ms  274 µs    461,0 ms   490,2 ms   142,5 ms   13 990 Ko
//
// Le coût serveur est LINÉAIRE : aucun décrochage entre 500 et 30 000. Le
// serveur n'a donc jamais été la cause du gel — le commentaire de #2228 le
// disait déjà (« froze the web UI »), et la mesure le confirme. Ce qui croît
// dangereusement, c'est la charge que le client doit transférer, analyser et
// tenir : la file relue pèse 2,3 Mo à 5 000 contre 14 Mo à 30 000.
//
// D'où le plafond retenu : 5 000, qui laisse un facteur 6 de marge sous le
// SEUL point de rupture jamais observé, tout en couvrant largement les
// ~2 400 pistes de william. Ce qu'on ne peut PAS affirmer : que 5 000 soit
// confortable à l'écran. Le gel est un phénomène du client web (autre dépôt),
// et il ne se mesure pas ici.

/// Réglage : combien de pistes au maximum une lecture aléatoire enfile.
///
/// Vit dans `settings` comme les autres réglages audio (`audio_buffer_kb`,
/// `prebuffer_seconds`…), publié par `GET /config` et écrit par
/// `PATCH /config`. Pas de second système de configuration.
pub const SHUFFLE_MAX_TRACKS_KEY: &str = "shuffle_max_tracks";

/// Défaut : 500. C'est la valeur qu'avait la constante de #2228, et elle
/// s'applique tant que personne ne configure rien.
pub const SHUFFLE_MAX_TRACKS_DEFAULT: i64 = 500;

/// Plancher du réglage. Une file d'UNE piste est étrange mais lisible ; zéro
/// ou négatif ne l'est pas — `shuffle_all` répondrait « no tracks to shuffle »
/// sur une bibliothèque pleine, et le bouton cesserait de fonctionner sans
/// que rien ne l'explique. Un réglage ne doit jamais pouvoir casser la lecture.
pub const SHUFFLE_MAX_TRACKS_FLOOR: i64 = 1;

/// Plafond du réglage — la valeur mesurée ci-dessus, pas un pari.
pub const SHUFFLE_MAX_TRACKS_CEILING: i64 = 5_000;

/// Résout le plafond depuis sa forme PERSISTÉE, quelle qu'elle soit.
///
/// `PATCH /config` écrit tout en texte sans rien valider (`"500"`, `"5000"`,
/// mais aussi bien `"0"`, `"-1"` ou `"beaucoup"`). La validation est donc à la
/// LECTURE, comme pour `replaygain_true_peak_ceiling_db` (#1694) : illisible
/// ⇒ le défaut, hors bornes ⇒ ramené dans les bornes. Jamais une valeur qui
/// empêche de jouer.
///
/// Fonction pure et séparée du stockage, sur le modèle de
/// `resolve_local_audio_backend` : les bornes se testent sans base.
pub fn resolve_shuffle_max_tracks(brut: Option<&str>) -> i64 {
    brut.map(str::trim)
        .filter(|v| !v.is_empty())
        // Une valeur venue de `PATCH /config` peut arriver entourée de
        // guillemets JSON (`"\"800\""`) selon que le client l'envoie en
        // nombre ou en chaîne. Les deux formes désignent le même réglage.
        .map(|v| v.trim_matches('"').trim())
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(SHUFFLE_MAX_TRACKS_DEFAULT)
        .clamp(SHUFFLE_MAX_TRACKS_FLOOR, SHUFFLE_MAX_TRACKS_CEILING)
}

/// Lit le plafond effectif depuis les réglages.
pub fn shuffle_max_tracks(backend: &std::sync::Arc<dyn crate::db::backend::DbBackend>) -> i64 {
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    resolve_shuffle_max_tracks(
        settings
            .get(SHUFFLE_MAX_TRACKS_KEY)
            .ok()
            .flatten()
            .as_deref(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    Off,
    One,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTrack {
    pub id: Option<i64>,
    pub source_id: Option<String>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub album_id: Option<i64>,
    pub duration_ms: u64,
    pub file_path: Option<String>,
    pub cover_path: Option<String>,
    pub source: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub channels: Option<u16>,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
}

pub struct PlayQueue {
    tracks: Vec<QueueTrack>,
    position: i64,
    shuffle: bool,
    repeat: RepeatMode,
    shuffle_order: Vec<usize>,
    shuffle_index: i64,
}

impl PlayQueue {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            position: -1,
            shuffle: false,
            repeat: RepeatMode::Off,
            shuffle_order: Vec::new(),
            shuffle_index: -1,
        }
    }

    pub fn tracks(&self) -> &[QueueTrack] {
        &self.tracks
    }

    pub fn position(&self) -> i64 {
        self.position
    }

    pub fn length(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn shuffle_enabled(&self) -> bool {
        self.shuffle
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        self.repeat
    }

    pub fn current(&self) -> Option<&QueueTrack> {
        if self.shuffle && !self.shuffle_order.is_empty() {
            let idx = self.shuffle_index.max(0) as usize;
            self.shuffle_order
                .get(idx)
                .and_then(|&i| self.tracks.get(i))
        } else if self.position >= 0 {
            self.tracks.get(self.position as usize)
        } else {
            None
        }
    }

    pub fn set_tracks(&mut self, tracks: Vec<QueueTrack>, start_position: usize) {
        self.tracks = tracks;
        self.position = if self.tracks.is_empty() {
            -1
        } else {
            (start_position.min(self.tracks.len().saturating_sub(1))) as i64
        };
        if self.shuffle {
            self.regenerate_shuffle();
        }
    }

    pub fn add_tracks(&mut self, tracks: Vec<QueueTrack>, at_position: Option<usize>) {
        if let Some(pos) = at_position {
            let idx = pos.min(self.tracks.len());
            for (i, track) in tracks.into_iter().enumerate() {
                self.tracks.insert(idx + i, track);
            }
            if (idx as i64) <= self.position {
                self.position += (self.tracks.len() - idx) as i64;
            }
        } else {
            self.tracks.extend(tracks);
        }
        if self.shuffle {
            self.regenerate_shuffle();
        }
    }

    pub fn remove_track(&mut self, pos: usize) -> Option<QueueTrack> {
        if pos >= self.tracks.len() {
            return None;
        }
        let track = self.tracks.remove(pos);
        let pos_i = pos as i64;
        if pos_i < self.position {
            self.position -= 1;
        } else if pos_i == self.position {
            self.position = self
                .position
                .min(self.tracks.len().saturating_sub(1) as i64);
        }
        if self.shuffle {
            self.regenerate_shuffle();
        }
        Some(track)
    }

    pub fn move_track(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.tracks.len() || to >= self.tracks.len() {
            return false;
        }
        let track = self.tracks.remove(from);
        self.tracks.insert(to, track);

        let pos = self.position as usize;
        if pos == from {
            self.position = to as i64;
        } else if from < pos && pos <= to {
            self.position -= 1;
        } else if to <= pos && pos < from {
            self.position += 1;
        }

        if self.shuffle {
            self.regenerate_shuffle();
        }
        true
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.position = -1;
        self.shuffle_order.clear();
        self.shuffle_index = -1;
    }

    pub fn set_shuffle(&mut self, enabled: bool) {
        self.shuffle = enabled;
        if enabled {
            self.regenerate_shuffle();
        } else {
            self.shuffle_order.clear();
            self.shuffle_index = -1;
        }
    }

    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    pub fn next(&mut self) -> Option<&QueueTrack> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One {
            return self.current();
        }

        if self.shuffle {
            self.shuffle_index += 1;
            if self.shuffle_index as usize >= self.shuffle_order.len() {
                if self.repeat == RepeatMode::All {
                    self.regenerate_shuffle();
                    self.shuffle_index = 0;
                } else {
                    return None;
                }
            }
            self.position = self.shuffle_order[self.shuffle_index as usize] as i64;
        } else {
            self.position += 1;
            if self.position as usize >= self.tracks.len() {
                if self.repeat == RepeatMode::All {
                    self.position = 0;
                } else {
                    return None;
                }
            }
        }
        self.current()
    }

    pub fn previous(&mut self) -> Option<&QueueTrack> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.shuffle {
            self.shuffle_index = (self.shuffle_index - 1).max(0);
            self.position = self.shuffle_order[self.shuffle_index as usize] as i64;
        } else {
            self.position = (self.position - 1).max(0);
        }
        self.current()
    }

    pub fn peek_next(&self) -> Option<&QueueTrack> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One {
            return self.current();
        }

        if self.shuffle {
            let next_idx = self.shuffle_index + 1;
            if next_idx as usize >= self.shuffle_order.len() {
                if self.repeat == RepeatMode::All {
                    return self.tracks.first();
                }
                return None;
            }
            self.shuffle_order
                .get(next_idx as usize)
                .and_then(|&i| self.tracks.get(i))
        } else {
            let next_pos = self.position + 1;
            if next_pos as usize >= self.tracks.len() {
                if self.repeat == RepeatMode::All {
                    return self.tracks.first();
                }
                return None;
            }
            self.tracks.get(next_pos as usize)
        }
    }

    pub fn jump_to(&mut self, pos: usize) -> Option<&QueueTrack> {
        if pos >= self.tracks.len() {
            return None;
        }
        self.position = pos as i64;
        if self.shuffle
            && let Some(idx) = self.shuffle_order.iter().position(|&i| i == pos)
        {
            self.shuffle_index = idx as i64;
        }
        self.current()
    }

    fn regenerate_shuffle(&mut self) {
        let len = self.tracks.len();
        if len == 0 {
            self.shuffle_order.clear();
            self.shuffle_index = -1;
            return;
        }

        let mut indices: Vec<usize> = (0..len).collect();
        let current_pos = if self.position >= 0 && (self.position as usize) < len {
            Some(self.position as usize)
        } else {
            None
        };

        if let Some(cur) = current_pos {
            indices.retain(|&i| i != cur);
            fisher_yates_shuffle(&mut indices);
            indices.insert(0, cur);
            self.shuffle_index = 0;
        } else {
            fisher_yates_shuffle(&mut indices);
            self.shuffle_index = 0;
        }

        self.shuffle_order = indices;
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "position": self.position,
            "shuffle": self.shuffle,
            "repeat": self.repeat,
            "tracks": self.tracks,
            "length": self.tracks.len(),
        })
    }
}

impl Default for PlayQueue {
    fn default() -> Self {
        Self::new()
    }
}

fn fisher_yates_shuffle(slice: &mut [usize]) {
    use std::time::SystemTime;
    let mut seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    for i in (1..slice.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed as usize) % (i + 1);
        slice.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_track(id: i64, title: &str) -> QueueTrack {
        QueueTrack {
            id: Some(id),
            source_id: None,
            title: title.to_string(),
            artist_name: None,
            album_title: None,
            album_id: None,
            duration_ms: 180000,
            file_path: None,
            cover_path: None,
            source: None,
            format: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            disc_number: None,
            track_number: None,
        }
    }

    #[test]
    fn empty_queue() {
        let q = PlayQueue::new();
        assert!(q.is_empty());
        assert!(q.current().is_none());
        assert_eq!(q.position(), -1);
    }

    #[test]
    fn set_tracks_and_navigate() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            0,
        );
        assert_eq!(q.length(), 3);
        assert_eq!(q.current().unwrap().title, "A");

        q.next();
        assert_eq!(q.current().unwrap().title, "B");

        q.next();
        assert_eq!(q.current().unwrap().title, "C");

        assert!(q.next().is_none());
    }

    #[test]
    fn previous_bottoms_at_zero() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A"), make_track(2, "B")], 1);
        assert_eq!(q.current().unwrap().title, "B");

        q.previous();
        assert_eq!(q.current().unwrap().title, "A");

        q.previous();
        assert_eq!(q.current().unwrap().title, "A");
    }

    #[test]
    fn repeat_all() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A"), make_track(2, "B")], 0);
        q.set_repeat(RepeatMode::All);
        q.next();
        assert_eq!(q.current().unwrap().title, "B");
        q.next();
        assert_eq!(q.current().unwrap().title, "A");
    }

    #[test]
    fn repeat_one() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A"), make_track(2, "B")], 0);
        q.set_repeat(RepeatMode::One);
        q.next();
        assert_eq!(q.current().unwrap().title, "A");
        q.next();
        assert_eq!(q.current().unwrap().title, "A");
    }

    #[test]
    fn shuffle_mode() {
        let mut q = PlayQueue::new();
        let tracks: Vec<QueueTrack> = (0..10).map(|i| make_track(i, &format!("T{i}"))).collect();
        q.set_tracks(tracks, 0);
        q.set_shuffle(true);
        assert_eq!(q.current().unwrap().title, "T0");
        let mut visited = vec![q.current().unwrap().title.clone()];
        for _ in 0..9 {
            q.next();
            if let Some(t) = q.current() {
                visited.push(t.title.clone());
            }
        }
        assert_eq!(visited.len(), 10);
    }

    #[test]
    fn peek_next_no_side_effects() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            0,
        );
        let peeked = q.peek_next().unwrap().title.clone();
        assert_eq!(peeked, "B");
        assert_eq!(q.current().unwrap().title, "A");
    }

    #[test]
    fn jump_to() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            0,
        );
        q.jump_to(2);
        assert_eq!(q.current().unwrap().title, "C");
    }

    #[test]
    fn remove_track() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            1,
        );
        q.remove_track(0);
        assert_eq!(q.length(), 2);
        assert_eq!(q.position(), 0);
        assert_eq!(q.current().unwrap().title, "B");
    }

    #[test]
    fn move_track() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            0,
        );
        q.move_track(0, 2);
        assert_eq!(q.position(), 2);
        assert_eq!(q.tracks()[0].title, "B");
        assert_eq!(q.tracks()[1].title, "C");
        assert_eq!(q.tracks()[2].title, "A");
    }

    #[test]
    fn add_tracks_at_position() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A"), make_track(3, "C")], 0);
        q.add_tracks(vec![make_track(2, "B")], Some(1));
        assert_eq!(q.length(), 3);
        assert_eq!(q.tracks()[1].title, "B");
    }

    #[test]
    fn clear() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A")], 0);
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.position(), -1);
    }

    #[test]
    fn fisher_yates_produces_permutation() {
        let mut indices: Vec<usize> = (0..20).collect();
        fisher_yates_shuffle(&mut indices);
        let mut sorted = indices.clone();
        sorted.sort();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn to_json() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A")], 0);
        let json = q.to_json();
        assert_eq!(json["position"], 0);
        assert_eq!(json["length"], 1);
    }
}

/// Le plafond de la lecture aléatoire est un RÉGLAGE borné (#2901).
///
/// Deux testeurs veulent l'inverse l'un de l'autre : william veut plus de
/// 2 400 pistes (fil 1620), Jean Valjean a fait poser le plafond parce que
/// 30 000 gelaient son interface (#2228). Ces tests tiennent les deux bouts :
/// le défaut reste 500 pour qui ne touche à rien, et aucune valeur ne peut
/// rendre la lecture aléatoire inopérante.
#[cfg(test)]
mod plafond_aleatoire_reglage_tests {
    use super::{
        SHUFFLE_MAX_TRACKS_CEILING, SHUFFLE_MAX_TRACKS_DEFAULT, SHUFFLE_MAX_TRACKS_FLOOR,
        resolve_shuffle_max_tracks,
    };

    /// LA garantie donnée à Jean Valjean : personne ne configure rien, le
    /// plafond vaut toujours 500. #2228 reste réglée sans qu'il agisse.
    #[test]
    fn sans_reglage_le_plafond_reste_celui_de_2228() {
        assert_eq!(SHUFFLE_MAX_TRACKS_DEFAULT, 500);
        assert_eq!(resolve_shuffle_max_tracks(None), 500);
        assert_eq!(
            resolve_shuffle_max_tracks(Some("")),
            500,
            "vider le champ dans l'interface écrit une chaîne vide, il ne              supprime pas la clé : c'est encore « je n'ai rien choisi »"
        );
        assert_eq!(resolve_shuffle_max_tracks(Some("   ")), 500);
    }

    /// Le besoin de william : une valeur intermédiaire est honorée telle
    /// quelle, y compris au-dessus de ses ~2 400 pistes.
    #[test]
    fn une_valeur_dans_les_bornes_est_honoree_telle_quelle() {
        assert_eq!(resolve_shuffle_max_tracks(Some("1000")), 1000);
        assert_eq!(resolve_shuffle_max_tracks(Some("2500")), 2500);
        assert_eq!(resolve_shuffle_max_tracks(Some("5000")), 5000);
        assert_eq!(
            resolve_shuffle_max_tracks(Some("  3000  ")),
            3000,
            "les espaces d'un champ de saisie ne sont pas une valeur illisible"
        );
        assert_eq!(
            resolve_shuffle_max_tracks(Some("\"2400\"")),
            2400,
            "nombre ou chaîne JSON : PATCH /config persiste les deux formes"
        );
    }

    /// Zéro et négatif : les deux valeurs qui CASSENT la lecture si on les
    /// laisse passer. `shuffle_all` tronquerait à 0, trouverait une sélection
    /// vide et répondrait « no tracks to shuffle » sur une bibliothèque pleine.
    #[test]
    fn zero_et_negatif_ne_peuvent_pas_eteindre_la_lecture_aleatoire() {
        assert_eq!(
            resolve_shuffle_max_tracks(Some("0")),
            SHUFFLE_MAX_TRACKS_FLOOR
        );
        assert_eq!(
            resolve_shuffle_max_tracks(Some("-1")),
            SHUFFLE_MAX_TRACKS_FLOOR
        );
        assert_eq!(
            resolve_shuffle_max_tracks(Some("-30000")),
            SHUFFLE_MAX_TRACKS_FLOOR
        );
        assert!(
            SHUFFLE_MAX_TRACKS_FLOOR >= 1,
            "le plancher doit laisser AU MOINS une piste : c'est ce qui              distingue « une file courte » de « le bouton ne marche plus »"
        );
    }

    /// Au-dessus du maximum mesuré : ramené au maximum, jamais accepté.
    /// 30 000 est précisément la taille qui a gelé l'interface de Jean
    /// Valjean — la borne haute existe pour que ce chiffre reste hors
    /// d'atteinte, même écrit à la main dans la base.
    #[test]
    fn au_dessus_du_maximum_mesure_la_valeur_est_ramenee_au_maximum() {
        assert_eq!(SHUFFLE_MAX_TRACKS_CEILING, 5_000);
        assert_eq!(
            resolve_shuffle_max_tracks(Some("5001")),
            SHUFFLE_MAX_TRACKS_CEILING
        );
        assert_eq!(
            resolve_shuffle_max_tracks(Some("30000")),
            SHUFFLE_MAX_TRACKS_CEILING,
            "la taille du gel de #2228 ne doit jamais être atteignable par le              réglage, quoi qu'on écrive en base"
        );
        assert_eq!(
            resolve_shuffle_max_tracks(Some("999999999")),
            SHUFFLE_MAX_TRACKS_CEILING
        );
    }

    /// Illisible ⇒ le défaut. Une base corrompue ou une valeur d'une version
    /// future ne doit pas décider du comportement.
    #[test]
    fn une_valeur_illisible_retombe_sur_le_defaut() {
        assert_eq!(resolve_shuffle_max_tracks(Some("beaucoup")), 500);
        assert_eq!(resolve_shuffle_max_tracks(Some("500.5")), 500);
        assert_eq!(resolve_shuffle_max_tracks(Some("tout")), 500);
    }

    /// Contre-épreuve des bornes elles-mêmes : un défaut hors bornes serait
    /// silencieusement réécrit, et le « 500 par défaut » deviendrait faux.
    #[test]
    fn le_defaut_est_lui_meme_dans_les_bornes() {
        assert!(SHUFFLE_MAX_TRACKS_FLOOR <= SHUFFLE_MAX_TRACKS_DEFAULT);
        assert!(SHUFFLE_MAX_TRACKS_DEFAULT <= SHUFFLE_MAX_TRACKS_CEILING);
        assert_eq!(
            resolve_shuffle_max_tracks(Some(&SHUFFLE_MAX_TRACKS_DEFAULT.to_string())),
            SHUFFLE_MAX_TRACKS_DEFAULT
        );
    }
}
