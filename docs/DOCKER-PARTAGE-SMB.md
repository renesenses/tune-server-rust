# Docker : écouter un partage SMB (NAS, Synology)

Ce guide répond à un cas signalé sur un NAS Synology
([#8069](https://github.com/renesenses/tune-server-rust/issues/8069)) : un
partage Samba monté sur le NAS dans le dossier de musique n'apparaît pas dans
le conteneur Tune, et le bouton de montage d'un partage SMB, dans les
emplacements de la bibliothèque, échoue.

## Pourquoi Tune ne monte pas lui-même un partage dans un conteneur

Pour monter un partage SMB, il faut les privilèges système (root et la capacité
`CAP_SYS_ADMIN`) et l'outil `mount.cifs` (paquet `cifs-utils`). L'image Docker
de Tune n'a ni l'un ni l'autre : le serveur y tourne sous le compte `tune`
(UID 1000), sans `sudo` ni outil de montage. Sur Tune OS (image Raspberry Pi),
un assistant privilégié fait ce travail. Dans un conteneur, cet assistant
n'existe pas.

Le montage depuis Tune ne reste plus muet dans ce cas : il affiche un message
qui explique la marche à suivre. Dans un conteneur, la bonne méthode est de
**monter le partage sur l'hôte**, puis de le **lier au conteneur**.

## 1. Monter le partage sur l'hôte

Sur un Synology, dans DSM : **File Station › Outils › Monter un dossier
distant › Dossier CIFS**. Choisissez comme point de montage un dossier vide
d'un dossier partagé, par exemple `/volume1/music/nas-salon`.

Sur un autre Linux, utilisez une ligne `cifs` dans `/etc/fstab` ou une unité
systemd `.mount`.

## 2. Lier le dossier au conteneur, avec la propagation `rslave`

Par défaut, Docker copie l'arborescence des montages **au démarrage du
conteneur** (propagation `rprivate`). Un partage monté sur l'hôte **après** ce
démarrage n'apparaît donc pas dans le conteneur : le dossier y reste vide.
C'est le cas d'un dossier distant que DSM remonte au démarrage du NAS, parfois
après le lancement des conteneurs.

La propagation `rslave` fait suivre au conteneur les montages ajoutés ensuite
sur l'hôte.

Avec `docker run` :

```bash
docker run -d \
  --name tune-server \
  --network host \
  -v /volume1/music:/music:ro,rslave \
  -v tune-data:/data \
  renesenses/tune:latest
```

Avec Compose (ou un **Projet** de Container Manager sur Synology), il faut la
forme longue du volume :

```yaml
services:
  tune:
    image: renesenses/tune:latest
    volumes:
      - type: bind
        source: /volume1/music
        target: /music
        read_only: true
        bind:
          propagation: rslave
      - tune-data:/data
```

Remplacez `/volume1/music` par le dossier de votre NAS. L'écran **Volume** de
Container Manager ne propose pas toujours le choix de la propagation : dans ce
cas, créez le conteneur à partir d'un projet Compose comme ci-dessus.

Le dossier monté (ici `/music/nas-salon`) se trouve sous `/music`, qui est déjà
un dossier de musique de l'image : relancez une analyse de la bibliothèque. Si
vous liez le partage ailleurs, ajoutez ce dossier aux dossiers de musique dans
les réglages de Tune.

## 3. Recréer le conteneur : un redémarrage ne suffit pas

Docker fixe les volumes d'un conteneur **à sa création**. Ajouter un dossier
lié, changer sa propagation ou passer de lecture seule à lecture-écriture ne
prend effet qu'avec un **nouveau** conteneur : un simple redémarrage relance
l'ancien, avec ses anciens volumes, et le partage n'y apparaît pas.

- Avec `docker run` : `docker rm -f tune-server`, puis relancez la commande
  `docker run` complète avec le nouveau `-v`. Le volume `tune-data` (base,
  réglages, comptes) n'est pas supprimé par `docker rm` : rien n'est perdu.
- Avec Compose : modifiez le fichier, puis `docker compose up -d`. Compose
  voit que la définition a changé et recrée le conteneur.
- Sur Synology (Container Manager) : arrêtez le conteneur et modifiez ses
  réglages de volume, ou dupliquez-le avec le nouveau dossier lié, comme l'a
  fait le testeur qui a signalé le cas (fil
  [Cannot mount shares in NAS version](https://mozaiklabs.fr/forum/threads/cannot-mount-shares-in-nas-version-but-can-do-in-raspberry-version-QI2AhP)).
  Pour un **Projet**, modifiez le fichier Compose et reconstruisez le projet.
  Gardez le même dossier ou volume pour `/data`.

## Si Tune ne trouve aucun fichier alors qu'un autre lecteur les voit

Un autre serveur de musique (Lyrion, par exemple) lit les mêmes dossiers du
NAS, mais Tune n'en trouve aucun. Trois causes sont possibles :

- **Le dossier du conteneur n'est pas un dossier de musique de Tune.** L'image
  analyse `/music` (`TUNE_MUSIC_DIRS='["/music"]'`). Si vous avez repris les
  liaisons d'un autre conteneur, par exemple `/volume1/music:/mnt/music`, Tune
  ne regarde pas `/mnt/music`. Liez le dossier sur `/music`, ou ajoutez le
  chemin **vu depuis le conteneur** (`/mnt/music`, pas `/volume1/music`) aux
  dossiers de musique dans les réglages de Tune, puis relancez une analyse.
- **Le compte de Tune ne peut pas lire les fichiers.** Tune tourne sous
  l'UID/GID `1000:1000`, et un autre conteneur peut tourner sous un autre
  compte. Vérifiez avec
  `docker exec tune-server ls -ln /music`. Si les fichiers ne sont pas
  lisibles par ce compte, donnez-lui le droit de lecture dans DSM, ou lancez
  le conteneur avec `-e PUID=<uid> -e PGID=<gid>` d'un compte qui les lit
  (les identifiants de **votre** NAS, colonnes 3 et 4 de `ls -ln` sur l'hôte).
- **Le partage est monté sur l'hôte après le démarrage du conteneur, sans
  `rslave`.** Voir l'étape 2, puis l'étape 3.

La recherche de partages SMB depuis les réglages de Tune (le bouton qui liste
les partages d'une machine) ne sert pas dans un conteneur : même trouvé, le
partage ne pourrait pas y être monté (voir plus haut). Sur une installation
Linux hors conteneur, la liste des partages passe par l'outil `smbclient` :
sans lui, aucun partage n'est listé. Installez le paquet `smbclient` de votre
distribution.

## Si le conteneur ne démarre plus après l'ajout de `rslave`

Avec `rslave` (ou `rshared`), Docker vérifie que le dossier de l'hôte est
monté en propagation partagée. Sinon, il **refuse de démarrer le conteneur**,
avec un message du type :

```
path /volume1/music is mounted on /volume1 but it is not a shared or slave mount
```

C'est probablement le premier obstacle sur un NAS. Pour le lever :

- `findmnt -o TARGET,PROPAGATION --target /volume1/music` affiche la
  propagation du montage qui porte le dossier ; il faut `shared`.
- Sinon, `mount --make-rshared /volume1` (en root sur l'hôte) le rend partagé,
  jusqu'au prochain redémarrage de l'hôte. Il faut donc le refaire à chaque
  démarrage, par exemple par une tâche planifiée au démarrage. Sur DSM, ce
  comportement n'a pas été vérifié pour chaque version.
- Relancez ensuite le conteneur.

## Si le dossier reste vide

- **Redémarrez le conteneur** une fois le partage monté sur l'hôte. Docker
  reprend au démarrage les montages déjà présents sous le dossier lié, même
  sans `rslave`. Cela vaut pour un dossier **déjà lié** : un dossier que vous
  venez d'ajouter aux volumes demande de recréer le conteneur (étape 3).
- Vérifiez que le volume porte bien `rslave` : sans lui, un partage monté sur
  l'hôte après le démarrage du conteneur n'y apparaît pas.
- Vérifiez depuis le conteneur : `docker exec tune-server ls /music`.

## Et `--cap-add SYS_ADMIN` ?

Donner au conteneur la capacité `SYS_ADMIN` (ou `--privileged`) ne suffit pas
avec l'image officielle : le serveur n'y tourne pas en root et l'outil
`mount.cifs` n'y est pas installé. Cela élargirait en outre beaucoup ce que le
conteneur peut faire sur l'hôte. Nous ne le recommandons pas : montez le
partage sur l'hôte.
