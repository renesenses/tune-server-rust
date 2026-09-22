//! Greffon WASM **« Playlists converter »** — transfert de playlists d'un
//! service à l'autre, simple et par lot (#4717, tranche 2 de l'épique #4715).
//!
//! # Ce qu'il fait
//!
//! Copier une ou PLUSIEURS playlists — d'un service, ou de la bibliothèque
//! locale — vers un service ou vers la bibliothèque locale, à l'identique.
//! Trois temps, jamais mélangés :
//!
//! 1. **`POST /apercu`** — lit les sources, apparie chaque titre chez la cible,
//!    et rend le rapport : appariés, approximatifs, introuvables, avec leur
//!    raison. **Il n'écrit rien** chez l'utilisateur ; seul le carnet du
//!    greffon (`kv`) est touché, pour que l'exécution retrouve ce plan.
//! 2. **`POST /executer`** — n'accepte qu'un transfert DÉJÀ prévu, et
//!    seulement avec `confirme: true`. Crée les playlists cibles et y ajoute
//!    les titres appariés. Jamais les approximatifs : trouvé n'est pas
//!    apparié.
//! 3. **La reprise** — le plan est réécrit dans le `kv` après chaque écriture
//!    réussie. Relancer `/executer` sur un lot coupé ne recrée pas la playlist
//!    déjà créée et ne réécrit pas les titres déjà posés.
//!
//! # Ce qu'il ne fait pas, et ne fera pas
//!
//! **Supprimer.** Ni playlist, ni piste, ni chez un service. L'interface hôte
//! de la tranche 1 n'expose aucune capacité de suppression, et le greffon n'en
//! simule aucune : pas de « vider puis remplir », pas de « remplacer ».
//!
//! # Architecture
//!
//! Tout le jugement vit dans [`moteur`], écrit contre le trait [`hote::Hote`]
//! — du Rust ordinaire, testé nativement contre un hôte de banc qui COMPTE ses
//! écritures. La couche wasm ([`abi`]) ne fait que marshaler du JSON : elle ne
//! décide de rien, et c'est la seule partie que l'intégration continue ne peut
//! pas exécuter.

pub mod hote;
pub mod moteur;
pub mod plan;
pub mod routage;

/// L'identifiant de manifeste. Il cloisonne le stockage `kv` côté hôte
/// (`plugin_kv:playlists-converter:…`) : le greffon ne le fournit jamais
/// lui-même, mais le nom doit rester le même des deux côtés.
pub const ID: &str = "playlists-converter";

/// La version d'ABI que ce greffon parle (`HOST_ABI_VERSION` de
/// `tune-plugin-runtime-wasm`).
pub const ABI: u32 = 1;

#[cfg(target_arch = "wasm32")]
pub mod abi;

#[cfg(test)]
mod essais;
