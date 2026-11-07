# Docker on Synology: writable model cache

Tune's acoustic analysis downloads its model on first use. With the default
settings, the image stores `clap-audio-music-2023.onnx` in
`/app/embedding_models`. Text search models and the ONNX runtime also use this
cache. `/data` contains the database and other persistent application data;
mounting `/data` alone does not make `/app/embedding_models` writable.

Prepare a separate persistent folder before starting acoustic analysis. For
example, in DSM File Station, create `docker/tune/embedding_models` on your data
volume. Its full path might be `/volume2/docker/tune/embedding_models`; replace
`volume2` with your actual volume.

In Container Manager, edit the container's **Volume** settings and add:

| Folder on the NAS | Mount path in the container | Permission |
| --- | --- | --- |
| `/volume2/docker/tune/embedding_models` | `/app/embedding_models` | Read/write |

For a Container Manager project using Compose, add this line alongside the
existing `/data` and music mounts:

```yaml
services:
  tune:
    volumes:
      - /volume2/docker/tune/embedding_models:/app/embedding_models
```

Keep your existing image, database volume, music mounts and environment settings.
This is an addition to the service's existing `volumes` list, not a complete
Compose file. The music mount can remain read-only.

Give the account running Tune read/write access to **this cache folder** in DSM.
The image runs Tune as UID/GID `1000:1000` by default. If you configured `PUID`
and `PGID`, use that account's identifiers instead. The entrypoint adjusts
ownership under `/data` when those variables are used; it does not adjust this
separate model mount. An explicit Docker `user:` setting takes precedence over
that mechanism, so use the actual UID/GID in that case. Do not copy an example
UID from another NAS or change permissions on your entire music collection.

Recreate the container with the added mount, then enable or retry acoustic
analysis in Tune. Watch the container log: the model download should finish
without `Permission denied (os error 13)`. Files should appear in the NAS cache
folder and remain there when the container is recreated. If a custom
`audio_embedding_model_path` setting or `TUNE_AUDIO_EMBED_MODEL` environment
variable is configured, mount and grant access to that path's parent directory
instead of the default path above.

This documents the dedicated-volume workaround reported in server issue
[#7028](https://github.com/renesenses/tune-server-rust/issues/7028). A new DSM
installation has not been reproduced as part of this documentation change.
