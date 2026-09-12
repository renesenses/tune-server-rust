# Réinitialiser Tune — où vivent les données, et comment repartir de zéro

Mesuré le 11/09/2026 sur `batch/bugs-11` (`63bb1484`). Issue #3854 (Fuccaro,
fil 1755 : « Comment faire pour repartir sur une installation propre ? »).

Ce document dit **ce qui existe**, **où vivent les données** et **ce qui
n'existe pas**. Il ne propose aucune fonctionnalité : l'arbitrage produit
(« faut-il un bouton *Réinitialiser Tune* ? ») reste ouvert dans #3854.

Une garde CI relit ce fichier et le compare au code
(`tune-server/tests/reinitialisation_documentee_3854.rs`) : si un chemin de
données change dans `config.rs` ou dans le paquet Debian sans que ce document
suive, la porte `Test` rougit. Une documentation de chemins qui dérive en
silence est pire que pas de documentation du tout — elle envoie effacer le
mauvais répertoire.

---

## 1. Ce que Tune sait effacer aujourd'hui

| Geste | Route | Ce qu'il efface RÉELLEMENT |
|---|---|---|
| **Vider la bibliothèque** | `POST /system/library/clear` — `tune-server/src/routes/system/scan.rs` (`library_clear`) | `tracks`, `albums`, `artists`, `track_credits`, et les entrées de file qui pointent vers une piste. **Rien d'autre.** |
| Supprimer toutes les zones | `DELETE /zones` | les zones (suppression logique) |
| Vider l'historique d'écoute | `DELETE /history` | l'historique seul |
| Oublier les sorties détectées | `POST /devices/clear` | la liste des sorties en mémoire et les sorties manuelles |
| Vider le cache hors-ligne | `POST /offline/clear` | le cache hors-ligne |
| Désactiver la licence | `DELETE /system/license` | la licence (retour en Free) |
| Ménage de la base | `POST /system/cleanup` | doublons et orphelins — c'est du ménage, pas une remise à zéro |

Côté interface, **« Vider la bibliothèque »** est le seul de ces gestes présenté
comme tel à l'utilisateur. Il est dans la nouvelle interface depuis **v0.9.143**
(et au niveau *Expert* de l'ancienne).

**Ce qui n'existe nulle part :**

- aucune route, aucune commande, aucun bouton ne remet à zéro **les réglages**
  (`settings`), **les comptes**, **les listes de lecture**, **les collections et
  favoris**, ni **le cache de pochettes sur disque** ;
- `tune-cli` n'a **aucune** sous-commande de réinitialisation ;
- le binaire serveur n'accepte **aucun** drapeau de réinitialisation (son seul
  argument est `--version`).

Repartir vraiment de zéro passe donc, aujourd'hui, par **la suppression manuelle
des répertoires de données, serveur arrêté**. Ils sont listés ci-dessous.

---

## 2. Où vivent les données

`TUNE_DB_PATH`, `TUNE_ARTWORK_DIR`, `TUNE_LOG_FILE` et `TUNE_TOOLS_DIR` passent
avant tout ce qui suit. Sans elles, les chemins par défaut sont :

### Windows

| Quoi | Où |
|---|---|
| Programme | `%LOCALAPPDATA%\Programs\Tune Server\` |
| Base, pochettes, sauvegardes, journal | `%LOCALAPPDATA%\TuneServer\` |
| Fichier de configuration | `%APPDATA%\Tune\tune.toml` |

### macOS

| Quoi | Où |
|---|---|
| Base, pochettes, sauvegardes | `~/Library/Application Support/Tune/` |
| Journal | `~/Library/Logs/tune-server.log` |
| Fichier de configuration | `tune.toml` du répertoire courant, `/etc/tune/tune.toml`, puis `~/.config/tune/tune.toml` |

### Linux (paquet Debian)

| Quoi | Où |
|---|---|
| Programme | `/opt/tune/` |
| Base, pochettes, sauvegardes | `/var/lib/tune/` |
| Réglages du service | `/etc/default/tune-server` |
| Fichier de configuration | `tune.toml` du répertoire courant, puis `/etc/tune/tune.toml` |

Dans les trois cas, les sauvegardes de base vivent dans un sous-répertoire
`backups/` **à côté du fichier de base**.

---

## 3. Repartir de zéro, par plateforme

Arrêter le serveur **avant** toute suppression. Les sauvegardes disparaissent
avec le reste : les recopier ailleurs d'abord si elles comptent.

### Windows

Désinstaller **ne suffit pas** — voir la section 4. Après la désinstallation :

```
rmdir /s /q "%LOCALAPPDATA%\TuneServer"
rmdir /s /q "%APPDATA%\Tune"
```

### macOS

```sh
rm -rf ~/Library/Application\ Support/Tune
rm -f  ~/Library/Logs/tune-server.log
```

### Linux (paquet Debian)

`apt purge` retire `/opt/tune` et `/etc/default/tune-server` mais **conserve
délibérément** `/var/lib/tune` — des années d'indexation ne doivent pas partir
sur un purge mal visé. Le `postrm` l'écrit à l'écran. Pour tout effacer :

```sh
sudo rm -rf /var/lib/tune && sudo deluser tune
```

---

## 4. 🔴 Sur Windows, désinstaller ne donne PAS une installation propre

La section `Uninstall` de l'installeur NSIS
(`.github/workflows/release.yml`) supprime le répertoire d'installation, les
deux raccourcis et la clé de registre. **Elle ne touche ni
`%LOCALAPPDATA%\TuneServer` ni `%APPDATA%\Tune`** — c'est-à-dire ni la base, ni
les pochettes, ni les réglages, ni `tune.toml`.

Un testeur qui désinstalle puis réinstalle **retrouve donc sa bibliothèque, ses
zones et ses réglages tels quels**, et peut légitimement conclure que
« réinitialiser » est impossible. C'est exactement le signalement du fil 1755.

Le dépôt sait pourtant faire : `tune-widget/src-tauri/installer-hooks.nsh` pose
une case « supprimer les données » dans le désinstalleur du mini-lecteur, et
sait ne pas l'appliquer pendant une mise à jour. Le désinstalleur du serveur
n'a pas d'équivalent. **C'est une décision produit, pas un oubli à corriger à
la volée** : elle appartient à #3854.

---

## 5. Ce qui n'est PAS tranché ici

- Faut-il un **« Réinitialiser Tune »** (double confirmation) au-delà de
  « Vider la bibliothèque » ? Rien ne le fait aujourd'hui.
- Le désinstalleur Windows doit-il **proposer** d'effacer les données ?
- `docs/DATA-RELOCATION.md` mentionne un bouton « repartir de zéro sur la clé »
  (double confirmation) pour l'appliance : il n'est **pas** implémenté — les
  routes réellement enregistrées sont `storage`, `storage/mount`, `data/status`,
  `data/relocate`, `install-to-disk`.
