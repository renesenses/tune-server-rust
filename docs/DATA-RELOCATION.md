# Données Tune sur disque externe (appliance) — Spécification

Origine : Tune OS tourne depuis la clé USB ; la base SQLite et le cache des
pochettes y subissent des I/O lentes (retour Stéphane, 25/07/2026 : « ça lague
très fort pendant le scan »). Objectif : déplacer les données Tune vers un
disque choisi par l'utilisateur (ex. le SSD USB qui contient la musique),
**depuis l'interface web, sans repartitionnement et sans toucher aux fichiers
musicaux**.

## Périmètre

- **Déplacé** : tout le répertoire de données — `tune.db` (+ WAL/SHM),
  `artwork_cache/`, et le contenu de `data_dir`.
- **Pas déplacé** : le binaire, `web/`, `tune.toml` (restent sur la clé).
- **Cible** : un simple dossier `TuneData/` à la racine du volume choisi.
  Aucune partition créée, aucune donnée existante modifiée.
- **Gating** : endpoints sous `/api/v1/appliance/*` (marqueur
  `/etc/tune-appliance`) — hors appliance, 404 comme le reste. Feature
  gratuite (infrastructure, pas premium).

## Règle d'or : jamais de chemin /media/sdXN dans tune.toml

Les noms de périphériques changent d'un boot à l'autre (sda→sdb selon l'ordre
de détection USB). La relocalisation :
1. identifie l'**UUID** du système de fichiers cible (`blkid`) ;
2. crée une unité de montage systemd par UUID vers un chemin stable :
   `/srv/tune-data` (unité `srv-tune\x2ddata.mount`, `nofail`,
   `x-systemd.device-timeout=15s`) ;
3. écrit dans `tune.toml` : `db_path = "/srv/tune-data/TuneData/tune.db"`,
   `artwork_dir = "/srv/tune-data/TuneData/artwork_cache"`.

## API

| Méthode | Route | Rôle |
|---|---|---|
| GET | `/appliance/storage` | Volumes candidats : `{device, uuid, fs, mount_path, size_bytes, free_bytes, label, is_data_target}` (lecture `/proc/mounts` + `statvfs` + `blkid`, exclut la clé système et les montages réseau) |
| POST | `/appliance/data/relocate` | `{uuid}` → lance le déplacement (job async, progression via WebSocket `data_relocation_progress`) |
| GET | `/appliance/data/status` | Emplacement courant, taille, état du job éventuel, présence du volume |

## Procédure de déplacement (section critique)

1. **Préflight** : volume monté en rw ; espace libre ≥ taille données × 1,2 ;
   avertissement non bloquant si FS ≠ ext4 (exFAT/NTFS : SQLite fonctionne,
   fsync/WAL moins robustes — recommander ext4, autoriser quand même).
2. **Quiesce** : lecture stoppée, scan suspendu, `PRAGMA
   wal_checkpoint(TRUNCATE)`, fermeture du pool SQLite.
3. **Copie** : récursive avec progression (octets copiés / total), fsync final.
4. **Vérification** : tailles + `PRAGMA integrity_check` sur la base copiée.
   Échec → on garde l'ancienne config, rien n'a changé (copie purgée).
5. **Bascule** : écriture de l'unité .mount (UUID), réécriture de
   `tune.toml`, restart du service (mécanisme existant #528/#536).
6. **Ancien emplacement** : conservé renommé `TuneData.pre-move/` (filet de
   sécurité) ; bouton « libérer l'espace » dans l'UI pour le purger ensuite.

## Cas limite majeur : disque absent au boot

Si le disque de données est débranché, le serveur ne doit **JAMAIS** retomber
silencieusement sur une base vide (piège fresh-install déjà connu). Au
démarrage, si `db_path` pointe sous `/srv/tune-data` et que le montage est
absent :
- mode **« en attente du disque de données »** : l'UI web sert une page
  d'état claire (« Branchez le disque contenant vos données Tune ») ;
- re-tentative de montage toutes les 10 s ; dès que le volume apparaît, le
  serveur démarre normalement ;
- un bouton « repartir de zéro sur la clé » (double confirmation) restaure
  les chemins par défaut pour qui a réellement perdu son disque.

## UI (web, onglet Système, section visible si `config.appliance`)

- Carte « Emplacement des données » : chemin courant, taille occupée, espace
  libre du volume, badge « clé USB (lent) » vs « disque ».
- Liste des volumes candidats avec taille/libre/FS ; bouton « Déplacer ici ».
- Modale de confirmation (résumé + avertissement FS le cas échéant), barre de
  progression, puis redémarrage automatique du service.
- i18n : 9 locales.

## Tests

- Unitaires : parsing `/proc/mounts`/`blkid`, calcul préflight, génération de
  l'unité .mount (échappement systemd des chemins).
- Intégration : relocalisation d'une petite base vers un tmpdir (stub
  `TUNE_BLKID_BIN`/`TUNE_MOUNT_UNIT_DIR` comme `TUNE_NMCLI_BIN`), échec de
  vérification → rollback, démarrage avec montage absent → mode attente.
- Recette manuelle sur la box de référence : déplacement vers SSD exFAT 2 To
  contenant déjà 2 To de musique (cas Stéphane), reboot avec/sans le disque.

## Phasage

- **MVP (~2-3 j)** : storage list + relocate + mode attente + UI + i18n.
- **V2** : purge ancien emplacement depuis l'UI, stats I/O (badge « lent »
  mesuré), option à l'onboarding appliance (« où stocker les données ? »),
  et priorité basse du scanner (nice/ionice) — chantier frère du lag.
