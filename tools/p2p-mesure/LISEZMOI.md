# p2p-mesure — mode d'emploi d'un essai

Banc de mesure pair-à-pair (iroh 1.3.0) pour l'écoute à distance. **Hors
produit**, pour la rc8 (cahier des charges du 19/10/2026). Il n'envoie que des
octets synthétiques : aucun fichier, aucun son.

## Compiler

```sh
cargo build --release --locked --manifest-path tools/p2p-mesure/Cargo.toml
# binaire : tools/p2p-mesure/target/release/p2p-mesure
```

## 1. Préparer la campagne (Bertrand)

Chaque testeur crée sa clé de campagne et envoie **son identifiant** (pas la clé) :

```sh
p2p-mesure client --cle t03.cle --identite
```

Bertrand écrit deux fichiers, qui restent sur la machine du serveur :

```text
# autorises.txt : un identifiant d'endpoint par ligne
5f0c…e1a2   # t03
# jetons.txt : « pseudonyme jeton », 16 caractères au moins
t03 7Hq2…long-et-aleatoire
```

Le pseudonyme (`t01`, `t02`…) est le seul nom qui circule. Les clés et jetons
sont détruits en fin de campagne.

## 2. Lancer le serveur (machine Tune, derrière la box)

```sh
p2p-mesure serveur --campagne c1 --jetons jetons.txt --autorises autorises.txt \
  --relais https://relay.mozaiklabs.fr --cle serveur.cle --site domicile --box livebox
```

- `--relais` : `n0` (relais publics d'n0, **phase 0 seulement, Bertrand seul**),
  l'URL du relais auto-hébergé (voir `RELAIS.md`), ou `aucun`.
- `--box` : `livebox`, `freebox`, `freebox-ipv4-partagee`, `sfrbox`, `bbox`,
  `vps` (témoin), `autre`.
- Le serveur affiche le **ticket** en texte et en QR code. Il contient les
  adresses du serveur : il se transmet à la main, il n'est publié nulle part
  (ni pkarr, ni DNS d'n0).
- Il s'arrête seul au bout de 24 h (`--duree-max-h` peut raccourcir, jamais
  allonger), et refuse toute ALPN autre que `tune-mesure/1`, tout identifiant
  hors liste blanche et tout jeton faux.

## 3. Jouer un essai (testeur)

```sh
export P2P_MESURE_JETON='7Hq2…'          # évite le jeton dans l'historique
p2p-mesure client 'TM1…ticket…' --cle t03.cle --campagne c1 --testeur t03 \
  --reseau 5g --operateur free [--partage] [--long]
```

- `--reseau` : `wifi`, `5g`, `4g`, `filaire`, `wifi-public`, `entreprise`.
- `--operateur` : `orange`, `sfr`, `bouygues`, `free`, `autre`, `aucun`.
- `--partage` : le Mac passe par le partage de connexion du téléphone.
- Essai **court** (défaut) : 60 s de pings. Essai **long** (`--long`) : 20 s en
  vrac, puis 5 min par format (16/44, 24/96, 24/192), soit environ 16 min.

Chaque essai ajoute une ligne à `p2p-mesure-<campagne>-<testeur>-<date>.jsonl`
dans le dossier courant (`--dossier` pour un autre). Le testeur envoie lui-même
ce fichier à Bertrand.

## 4. Agréger (Bertrand)

```sh
p2p-mesure agreger p2p-mesure-c1-*.jsonl --csv agregat.csv --md agregat.md
```

Un tableau par croisement réseau × opérateur × box, plus une ligne « toutes ».

## Ce que contient le journal

Exactement les champs du § 3.2 du cahier. **Aucune adresse IP**, ni en clair ni
hachée, aucune position, aucun nom de Wi-Fi : le client vérifie chaque ligne
avant de l'écrire et masque les adresses des messages d'erreur.

| Champ | Définition |
|---|---|
| `chemin_final` | chemin sélectionné par iroh, suivi chaque seconde et à chaque changement : `direct` (passé en direct, jamais revenu au relais), `relais` (jamais direct), `mixte` (revenu au relais après le direct) ; `null` si la poignée de main a échoué |
| `famille_direct` | `ipv4`, `ipv6` ou `aucune` : famille du dernier chemin direct |
| `t_connecte_ms`, `t_premier_octet_ms`, `t_direct_ms` | horloge monotone du client depuis l'appel `connect` ; premier octet = début de la réponse du serveur sur le flux de contrôle |
| `debit_max_mbps` | flux en vrac (20 s en essai long) |
| `flux[]` | flux cadencé par blocs de 64 Kio après une rafale de 2 s ; `sous_debits` = vidages du tampon simulé de 2 s ; `debit_moyen_mbps` inclut la rafale (+0,7 % sur 5 min) |
| `rtt` | pings de 16 octets toutes les 200 ms en datagrammes QUIC (flux de secours sinon) ; p50, p95, p99 séparés selon le chemin à la réception ; `gigue_ms` = moyenne des écarts absolus entre RTT successifs ; `pertes_ping` = part sans écho |
| `batterie` | `null` en CLI (phase 2 seulement) |

## Agrégation : définitions

- **Taux direct** : essais `direct` / essais retenus (avec un `chemin_final`) ;
  intervalle de **Wilson** à 95 %. Les essais refusés ou en erreur sont comptés
  à part (colonne « Erreurs »).
- **Direct < 5 s** : essais dont `t_direct_ms` < 5000, sur les essais retenus.
- **Taux pondéré par le volume** (indicateur du § 7) : part du volume des
  essais passés en direct. Volume d'un essai = Σ `debit_moyen_mbps × duree_s`
  de ses flux ; un essai court compte pour une minute de 16/44 cadencé
  (60 Mbit). Le journal ne dit pas combien de temps un essai `mixte` a passé
  sur chaque chemin : on donne donc deux bornes, **basse** (mixte compté
  relais) et **haute** (mixte compté direct).
- Centiles par interpolation linéaire entre rangs.

## Tests

```sh
cd tools/p2p-mesure && cargo fmt --check && cargo clippy -- -D clippy::correctness && cargo test
```

Les essais serveur-client tournent sur 127.0.0.1, sans relais ni réseau
extérieur.
