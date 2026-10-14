/*
 * tuneserver.h — API C de `tune-ffi` (bibliotheque `libtuneserver`).
 *
 * Ecrit a la main : il doit suivre les `#[unsafe(no_mangle)] extern "C"` de
 * `tune-ffi/src/lib.rs`. Le test `l_entete_c_suit_les_exports` le verifie.
 *
 * Consommateurs : l'application iOS qui embarque le serveur
 * (renesenses/tune-server-ios, module Swift `TuneServerFFI` par
 * `module.modulemap`) ; l'hote Android lit les memes symboles par Dart FFI.
 */
#ifndef TUNESERVER_H
#define TUNESERVER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Demarre le serveur dans le processus hote.
 * port            : port HTTP (8888) ;
 * db_path         : chemin ABSOLU de la base SQLite ; le serveur se place
 *                   dans son dossier (chemins relatifs du cache) ;
 * music_dirs_json : tableau JSON de dossiers de musique, ex. "[\"/x/Music\"]" ;
 * web_dir         : dossier du client web, ou NULL.
 * Retour : 0 succes, -1 deja demarre, -2 erreur. */
int32_t tune_server_start(uint16_t port, const char *db_path,
                          const char *music_dirs_json, const char *web_dir);

/* Racine du conteneur de l'application (iOS : NSHomeDirectory()), A APPELER
 * AVANT tune_server_start. Si la base a ete ecrite sous une autre racine qui
 * n'existe plus (iOS a deplace l'application), ses chemins sont reecrits sous
 * la nouvelle au demarrage.
 * Retour : 0 succes, -2 si root est NULL, relatif ou "/". */
int32_t tune_server_set_container_root(const char *root);

/* Scan INCREMENTAL des dossiers de musique, en tache de fond (retour au
 * premier plan : fichiers deposes pendant la suspension). Le surveillant de
 * fichiers, lui, demarre seul avec le serveur. Ne pas appeler depuis un fil
 * du runtime du serveur.
 * Retour : 0 scan lance, 1 un scan tourne deja, -1 serveur pas pret. */
int32_t tune_server_scan(void);

/* Arret propre. Retour : 0 succes, -1 pas demarre. */
int32_t tune_server_stop(void);

/* Etat en JSON, a liberer par tune_free_string. NULL en cas d'echec. */
char *tune_server_status(void);

/* Version du serveur, a liberer par tune_free_string. NULL en cas d'echec. */
char *tune_server_version(void);

/* Libere une chaine rendue par tune_server_status ou tune_server_version. */
void tune_free_string(char *ptr);

#ifdef __cplusplus
}
#endif

#endif /* TUNESERVER_H */
