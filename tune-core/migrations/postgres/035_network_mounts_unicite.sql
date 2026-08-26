-- GgB (fil 1562, #2453) : network_mounts sans contrainte d'unicité,
-- create_mount insérait sans regarder — l'écran Emplacements affichait le
-- même partage deux fois, sans retour possible. Purge d'abord (le plus petit
-- id de chaque identité survit), index unique ensuite : posé sur une table
-- non purgée, il échouerait sur les bases qui portent déjà le doublon.
DELETE FROM network_mounts a
 USING network_mounts b
 WHERE a.id > b.id
   AND a.mount_type = b.mount_type
   AND a.server = b.server
   AND a.share = b.share
   AND a.mount_path = b.mount_path;
CREATE UNIQUE INDEX IF NOT EXISTS idx_network_mounts_identite
    ON network_mounts (mount_type, server, share, mount_path);
