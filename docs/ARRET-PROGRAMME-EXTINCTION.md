# Arrêt programmé : éteindre la machine (#8213, #8909)

Réglages › Général › Arrêt programmé. À l'heure dite, les jours cochés, Tune
arrête la lecture. Avec l'action **« Éteindre la machine »**, il éteint
ensuite l'ordinateur.

- L'option est **désactivée par défaut** (`action: "stop_playback"`).
- Seul un **administrateur** peut la poser (`PUT
  /api/v1/system/settings/scheduled-stop`).
- **Hors Tune OS, l'authentification doit être activée** (Réglages ›
  Sécurité) : sans elle, la route refuse l'extinction de l'ordinateur (403
  `auth_required`), et l'échéance la refuse aussi si l'authentification a été
  coupée depuis. Sur Tune OS, sans authentification, l'option reste posable
  depuis la machine elle-même, comme les boutons « Éteindre » et
  « Redémarrer ».
- Le mode validé à la pose (`poweroff_mode` : `tune_os` ou `host`) est rangé
  avec le réglage par le serveur. À l'échéance, rien ne s'éteint si le mode
  courant de la machine diffère (réglage restauré ou importé d'une autre
  installation) : il suffit d'enregistrer de nouveau l'arrêt programmé.
- L'extinction n'a lieu **qu'à l'échéance de l'arrêt programmé**. Aucune route
  HTTP n'éteint un hôte ordinaire (les boutons « Éteindre » / « Redémarrer »
  restent réservés à Tune OS).
- **Docker, Podman, Kubernetes** : l'option est masquée et refusée. Un
  conteneur n'éteint pas son hôte ; programmez l'extinction sur l'hôte.
- **LXC, systemd-nspawn** : ces conteneurs-là ne sont pas toujours détectés.
  L'option peut alors y être proposée ; l'extinction arrête le conteneur
  lui-même, pas la machine qui l'héberge.

`GET /api/v1/system/settings/scheduled-stop` rend `poweroff_available`,
`poweroff_mode` (`"tune_os"`, `"host"` ou `null`) et `last_poweroff_error`
(`{ "at", "code", "error" }` du dernier échec pour l'administrateur, `{ "at",
"code" }` pour les autres ; effacé à la prochaine extinction réussie ou quand
le réglage est reposé).

## Ce que Tune lance

Aucun shell, aucun `sudo`, aucune donnée reçue dans la commande : des chemins
absolus et des arguments fixes.

| Système | Commande |
|---|---|
| Tune OS | `systemctl poweroff` (règle polkit de l'image, `/etc/polkit-1/rules.d/50-tune.rules`) |
| Linux (systemd) | `/usr/bin/systemctl --no-ask-password poweroff` (ou `/bin/systemctl`) |
| Linux (OpenRC + elogind) | `/usr/bin/loginctl --no-ask-password poweroff` (ou `/bin/loginctl`) |
| macOS | `/usr/bin/osascript -e 'tell application "System Events" to shut down'` |
| Windows | `C:\Windows\System32\shutdown.exe /s /t 0` |

Sous Linux, pour un compte non root, `systemctl`/`loginctl` appellent logind :
`org.freedesktop.login1.Manager.PowerOff(interactive=false)`. Polkit répond
oui ou non, sans demander de mot de passe.

## Linux : la règle polkit

Si Tune tourne sous un compte de service (par exemple `tune`) sans session
active, polkit refuse en général `org.freedesktop.login1.power-off`. L'écran
de l'arrêt programmé et le journal (`scheduled_poweroff_failed`) disent alors
« Linux refused to power off the machine (polkit) ».

Posez cette règle en root, en remplaçant `tune` par le compte qui fait tourner
Tune (`systemctl show -p User tune.service`, ou `ps -o user= -C tune-server`) :

```js
// /etc/polkit-1/rules.d/50-tune-poweroff.rules
// Tune (tune-server-rust#8909) : l'arrêt programmé peut éteindre la machine.
polkit.addRule(function (action, subject) {
    if (subject.user === "tune"
        && action.id === "org.freedesktop.login1.power-off") {
        return polkit.Result.YES;
    }
    return polkit.Result.NOT_HANDLED;
});
```

**Ce droit est permanent** : il vaut pour le compte `tune` à toute heure, pas
seulement à l'heure programmée. Tout programme qui tourne sous ce compte peut
éteindre la machine. Réservez ce compte à Tune, et retirez le fichier si vous
renoncez à l'option.

Rien d'autre : ni redémarrage, ni `manage-units`, ni
`power-off-ignore-inhibit` (une mise à jour en cours qui pose un verrou
d'inhibition garde la main). Polkit relit le dossier tout seul ; aucun
redémarrage n'est nécessaire.

**Option : `org.freedesktop.login1.power-off-multiple-sessions`.** Si un
autre utilisateur a une session ouverte (bureau, SSH), logind demande cette
seconde action, et la règle ci-dessus ne suffit pas : l'extinction est
refusée et l'écran le dit. Ajoutez-la seulement si vous acceptez que Tune
éteigne la machine pendant la session de quelqu'un d'autre :

```js
    if (subject.user === "tune"
        && (action.id === "org.freedesktop.login1.power-off"
            || action.id === "org.freedesktop.login1.power-off-multiple-sessions")) {
        return polkit.Result.YES;
    }
```

Debian/Ubuntu avec un polkit antérieur à 0.106 (fichiers `.pkla`) :

```ini
# /etc/polkit-1/localauthority/50-local.d/50-tune-poweroff.pkla
[Tune arret programme]
Identity=unix-user:tune
Action=org.freedesktop.login1.power-off
ResultAny=yes
ResultInactive=yes
ResultActive=yes
```

Vérifier, sous le compte de Tune, sans rien éteindre : `pkcheck --action-id
org.freedesktop.login1.power-off --process $$` doit rendre 0.

## macOS

La première extinction demande d'autoriser Tune à piloter « System Events ».
Si elle a été refusée : Réglages Système › Confidentialité et sécurité ›
Automatisation, cocher « System Events » sous Tune. Les applications ouvertes
peuvent retarder l'extinction (document non enregistré).

## Windows

`shutdown.exe` exige le droit « Arrêter le système » (SeShutdownPrivilege) du
compte qui fait tourner Tune ; les comptes Administrateurs, Utilisateurs (poste
de travail) et LocalSystem l'ont par défaut. Sinon : Stratégie de sécurité
locale › Attribution des droits utilisateur › Arrêter le système.
