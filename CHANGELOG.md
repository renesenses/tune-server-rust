# Changelog

All notable changes to Tune are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [0.8.30] — 2026-06-03

### Bug fixes

- Statically link CRT to eliminate VC++ redistributable dependency ([c2e4be7](https://github.com/renesenses/tune-server-rust/commit/c2e4be7b371b288da9d305f15fd3b708c262f4ee))
- OAAT gapless — override set_next_url for prebuffer ([c0ec3ee](https://github.com/renesenses/tune-server-rust/commit/c0ec3eee564d69446f93eb2bbf10961aadb75886))

### CI / build

- Supply base=main to create-pull-request ([cdb8165](https://github.com/renesenses/tune-server-rust/commit/cdb81659a0ae172561a7df4d1ad8d4425e5df718))

### Other

- Disable FLAC passthrough for OAAT (not stable yet) ([ebd8f2b](https://github.com/renesenses/tune-server-rust/commit/ebd8f2bc37af738568c5ba8716029f24bfea00b1))
## [0.8.29] — 2026-06-03

### Bug fixes

- Multi-word FTS queries — proper tokenization per engine ([df8d1a9](https://github.com/renesenses/tune-server-rust/commit/df8d1a9697c3c06f3f139ec79f0ed42fd8f1eb4e))
- Forum job — separate git-cliff output from final body ([e3409b6](https://github.com/renesenses/tune-server-rust/commit/e3409b6af07ee27fb91bb90e95516bee1f5db0ee))
- WAL read lag on ZoneRepo::list — zones disappeared after create ([185f589](https://github.com/renesenses/tune-server-rust/commit/185f5898b0b17d949e62f2bef87c5cb822c5b22d))

### CI / build

- Install git-cliff via cargo (action uses EOL Debian buster) ([420b29b](https://github.com/renesenses/tune-server-rust/commit/420b29bc33a9ab21b55addb92bd0646e658be8eb))

### Documentation

- Add release-operations + postgres-deploy runbooks, refresh path-to-v0.8.50 ([439df26](https://github.com/renesenses/tune-server-rust/commit/439df26aada2259d93bf5bcecf1a58ecd789d0c1))

### Features

- FLAC passthrough for OAAT endpoints ([dbd5f82](https://github.com/renesenses/tune-server-rust/commit/dbd5f82c10bfa200182ef7ec43d256d25a0b6456))
## [0.8.28] — 2026-06-03

### Bug fixes

- Use SO_REUSEADDR to eliminate port conflicts on restart ([4aae04a](https://github.com/renesenses/tune-server-rust/commit/4aae04ab998cb5f845f20c60f37ed5066e513dc9))

### Documentation

- Add path-to-v0.8.50 plan — 3 paliers de stabilisation ([4e6e25b](https://github.com/renesenses/tune-server-rust/commit/4e6e25bb9704a8c68b527c738fac51b8915671f0))
- Recalibrer les dates sur le rythme réel ([591a10d](https://github.com/renesenses/tune-server-rust/commit/591a10dc9645aa44e103884828606c095f7f512a))
- Add release-autonomy plan targeted at v0.9.50 ([5472100](https://github.com/renesenses/tune-server-rust/commit/5472100a0f27e7ae664a85bf691f90e20d8fe7c2))

### Features

- Preflight checks (phase 1 of release autonomy) ([2481656](https://github.com/renesenses/tune-server-rust/commit/248165651ed8cfdb2a2e0dbddb1fdd9a085eb05f))
- Add \`tune release bump\` + find-version-strings.sh (release autonomy phase 2) ([ae340f4](https://github.com/renesenses/tune-server-rust/commit/ae340f4ac1f06f6f86d69d538dfc4bb802db89b6))
- Auto-changelog via git-cliff (release autonomy phase 3) ([8f37633](https://github.com/renesenses/tune-server-rust/commit/8f376333b314f76f164e85f83185d6091d864cfb))
- Add PostgreSQL schema bootstrap (001_initial_schema.sql) ([f668a01](https://github.com/renesenses/tune-server-rust/commit/f668a01a38febf37c365f2c15a3b62ae0d484cd6))
- Enable native bit depth for OAAT (up to 24-bit) ([31bbc2c](https://github.com/renesenses/tune-server-rust/commit/31bbc2c64d965caf7ee2b7cf62efb5b0b6314d06))
- Implement \`tune db migrate-to-postgres\` (PG migration tool) ([5e5d365](https://github.com/renesenses/tune-server-rust/commit/5e5d3653f83f703eb7167156ce523730fd20bb01))
- Homebrew tap + forum auto-publish (autonomy phase 4) ([0fb491b](https://github.com/renesenses/tune-server-rust/commit/0fb491b511562510b02aa3ebf086e82caffedf76))
- Full-text search for PostgreSQL — tsvector + GIN ([9a9e825](https://github.com/renesenses/tune-server-rust/commit/9a9e8256045e410b0ce909dd1f455ad239ed036a))
- Rollback workflow (autonomy phase 5 — final) ([4049cc2](https://github.com/renesenses/tune-server-rust/commit/4049cc26717fa407b6ae9070b6ad61145a730e3e))
- Wire dialect.fts_where into the 3 search() methods ([9b0d473](https://github.com/renesenses/tune-server-rust/commit/9b0d473e5a6a90cafcd144d9bbd62225bfac1f94))
## [0.8.27] — 2026-06-03

### Bug fixes

- GET /zones/{id}/queue returns empty due to WAL read lag ([8af95ec](https://github.com/renesenses/tune-server-rust/commit/8af95ec2bb95ecefd0bf10c61a2f5eec082b57f5))
- Prevent gapless poller from skipping tracks after 1-10s on DMP-A8 ([45f333b](https://github.com/renesenses/tune-server-rust/commit/45f333bf262215bdb8f8384e6581e7fbab942679))
- Read embedded ID3v2 tags from DSF files instead of falling back to filename/directory ([e9d1fba](https://github.com/renesenses/tune-server-rust/commit/e9d1fba7dec7a47a456c77baab52bc2c0057a7a0))
- Align normalize_format tests with 2-arg signature (bit_depth) ([569b030](https://github.com/renesenses/tune-server-rust/commit/569b0303e6a9312ea51ed2b8c4b5c2f59ce9d9de))

### Features

- PostgreSQL groundwork — 13 repos ported to SqlDialect (#1) ([603a7db](https://github.com/renesenses/tune-server-rust/commit/603a7dbbfebb4defd5f9f47a125eca936d1e94fb))
## [0.8.26] — 2026-06-02

### Bug fixes

- Paginate Qobuz favorites to fetch all items, not just first 50 ([fff1a0b](https://github.com/renesenses/tune-server-rust/commit/fff1a0baec092d8cb776a526d1436034a49b2516))
- Force WAV stream format for OAAT outputs on streaming services ([4c5aade](https://github.com/renesenses/tune-server-rust/commit/4c5aadec6ec172f8f3e3bc12aa341eba3d56aa0f))
- Transcode HTTPS streams to WAV for OAAT via FFmpeg ([d668efc](https://github.com/renesenses/tune-server-rust/commit/d668efc6cadb563777f8a869d9a5fc3e9863c162))
- Transcode all non-WAV local files to WAV for OAAT outputs ([cb2e27a](https://github.com/renesenses/tune-server-rust/commit/cb2e27a0e2d496685bb81cb7c4aa409b12dd4a95))
- Transcode ALL formats to WAV for OAAT (including DSD) ([896166e](https://github.com/renesenses/tune-server-rust/commit/896166e8722e4d926e5894af8505a941eafcfe3a))
- Force 24-bit output for OAAT transcoding (DAC requires S32_LE) ([17b0acc](https://github.com/renesenses/tune-server-rust/commit/17b0acc4e7b339f96084aef088f5e6250082746b))
- Force WAV target format for OAAT transcode (was using FLAC→FLAC) ([ffdc31a](https://github.com/renesenses/tune-server-rust/commit/ffdc31a7d9199094dcc030f3a7c4c3d1f4482768))
- Open browser after server is ready to accept connections ([fdfaf04](https://github.com/renesenses/tune-server-rust/commit/fdfaf04395b5070d089f843586868d7da413623d))
- Force WAV target format for OAAT transcode (was using FLAC→FLAC) ([be9a453](https://github.com/renesenses/tune-server-rust/commit/be9a453ff5d9151b927d7321086bf7f448e8b039))
- Resolve merge conflict — native bit depth for OAAT ([9853141](https://github.com/renesenses/tune-server-rust/commit/9853141e6d4f11423a1e85998001cd05fa4a1d77))
- Deduplicate tracks by audio_hash+album_id during scan ([7ddbd77](https://github.com/renesenses/tune-server-rust/commit/7ddbd772bd12eef4704618866c71c85cfe7989ad))
- Use read_connection() for all read-only DB queries, preventing SQLite readonly errors ([cd96d48](https://github.com/renesenses/tune-server-rust/commit/cd96d48dfaebc82a52c3969310dc2b5ebb7709c1))
- Replace total timeout with connect_timeout for OAAT stream ([6b01363](https://github.com/renesenses/tune-server-rust/commit/6b01363fd5a2b24456c3e5daf9afff175f4963ef))

### Documentation

- Add PostgreSQL support as axis 6 of v0.9.0 roadmap ([9c7cc11](https://github.com/renesenses/tune-server-rust/commit/9c7cc11475c67a64dbb92281a8230b9833cefd43))

### Performance

- Faster release builds — opt-level 2, thin LTO, codegen-units 4 ([37cdb74](https://github.com/renesenses/tune-server-rust/commit/37cdb749f36071403abf8907013783fc807146cf))
## [0.8.25] — 2026-06-02

### Bug fixes

- Add /zones/group route alias for /zones/groups (405 fix) ([415f1ee](https://github.com/renesenses/tune-server-rust/commit/415f1ee7e6b145e9aa4c2ae1a45dc2c2092b516d))

### Chore

- Rebuild web client with folder browser ([b793691](https://github.com/renesenses/tune-server-rust/commit/b793691a79244d5dbe8f2b54be9f29ae15120bd6))

### Documentation

- Add 7 language translations + index for getting-started guide ([3e555f0](https://github.com/renesenses/tune-server-rust/commit/3e555f08ab1caa35563521eee3bf40509602a0ed))
- Add v0.9.0 roadmap covering 5 strategic axes ([34f3d90](https://github.com/renesenses/tune-server-rust/commit/34f3d90b952c8420305479efc770b60b4573ee6a))
- Mark v0.8.50 as the trigger to switch on v0.9.0 roadmap ([46de01c](https://github.com/renesenses/tune-server-rust/commit/46de01cdcd1eb431c7bfce9820322af487d75cdf))

### Features

- Browse-dirs endpoint for native folder browser ([c7cfdad](https://github.com/renesenses/tune-server-rust/commit/c7cfdad74b8ee43ccedccc18305223cd30e5d708))
- ISO SACD extraction via sacd_extract subprocess ([e9797f8](https://github.com/renesenses/tune-server-rust/commit/e9797f849e5ca3613b3066a8b8d18b4bad317090))
- Auto-extract ISO SACD during library scan ([dbb50d2](https://github.com/renesenses/tune-server-rust/commit/dbb50d22999fc237ec3301ec90ff58ea44fe080f))
## [0.8.24] — 2026-06-02

### Bug fixes

- WS broadcast closed resubscribes instead of breaking connection ([d180cd2](https://github.com/renesenses/tune-server-rust/commit/d180cd21f786c2749d36da4b2087f9c961c6e41b))
- Add per-file timeout to scanner to prevent hanging on NAS/corrupt files ([382b660](https://github.com/renesenses/tune-server-rust/commit/382b6605514b88ec7ebb0ef78791c7f71894bda4))

### Chore

- Perf baseline script + forum watch GitHub Action ([a9fbcd8](https://github.com/renesenses/tune-server-rust/commit/a9fbcd86fd1a26de9a7688cd7c6fb3790b2e1de9))
- Perf baseline script + forum watch GitHub Action ([49424f2](https://github.com/renesenses/tune-server-rust/commit/49424f25d2221a035a447cc87f8b22c56befae2c))
## [0.8.23] — 2026-06-02

### Bug fixes

- Zone list uses write connection to see freshly created zones ([890e53c](https://github.com/renesenses/tune-server-rust/commit/890e53c9b21f11ed9940f4bc662856eb3ebd8477))

### Documentation

- Add test plan for v0.8.22 ([5580ccf](https://github.com/renesenses/tune-server-rust/commit/5580ccf4f22ccdbb9106492da4af98d2a1ff0b72))
## [0.8.22] — 2026-06-02

### Bug fixes

- Remove duplicate sleep timer route from zones (kept in playback) ([f9c8fc9](https://github.com/renesenses/tune-server-rust/commit/f9c8fc97a2343142f6115c80cdcdb4a09665fee0))
- Remove duplicate POST /{id}/sleep route causing startup panic ([0e18527](https://github.com/renesenses/tune-server-rust/commit/0e1852795e30a3a01c19843178352bc629bf398a))

### Features

- API docs + proactive monitoring insights ([7801962](https://github.com/renesenses/tune-server-rust/commit/7801962d140aedee1892f4d086ac5d8b14fe5699))
## [0.8.21] — 2026-06-02

### Bug fixes

- Ignore flaky OAAT sustained stream test in CI ([691076d](https://github.com/renesenses/tune-server-rust/commit/691076dc53e411e4cb68caa3ffbedae8115bd47a))
- Add DELETE /zones/{id}/queue/{position} endpoint for removing tracks from play queue ([0390a12](https://github.com/renesenses/tune-server-rust/commit/0390a12caf6ce59d5f9459d53462a4677dca54da))
- Zone creation not persisted to DB when scan transaction is open ([c99efe1](https://github.com/renesenses/tune-server-rust/commit/c99efe109f0477e77184e76dff4c3d4456f0efa2))
- Accent-insensitive search via tune_unaccent SQLite function ([82880ed](https://github.com/renesenses/tune-server-rust/commit/82880ed604627d87e37c16d8049ee3067bea17c0))
- Persist playback position across stop/pause/restart ([1c02523](https://github.com/renesenses/tune-server-rust/commit/1c025235817bfb4dd53cbb19c59c2f35cadf4740))
## [0.8.20] — 2026-06-01

### Features

- CLI clap migration + shell completions + collaborative tests ([409b41d](https://github.com/renesenses/tune-server-rust/commit/409b41d386ea51029d53394536174de7e8e7db49))
- Smart album grouping + Wrapped dashboard ([1cd4fb2](https://github.com/renesenses/tune-server-rust/commit/1cd4fb2fa21ef007b2d808c6ebbd52242283a320))
- MDNS service registration — Tune announces itself on the network ([de5acef](https://github.com/renesenses/tune-server-rust/commit/de5acefea584b24df790ea03891124dc8a1d946b))
- DSP per zone + voice search via Whisper ([7c48125](https://github.com/renesenses/tune-server-rust/commit/7c4812546a0484330ce5ca7a5c9b3fbf66b003f6))
- Upgrade oaat 0.1.2 — multiroom on standard tokio runtime ([b7df3e7](https://github.com/renesenses/tune-server-rust/commit/b7df3e7e3c58dc8a3a828de5fe6c5471a173bce9))
- Playlist export (M3U/JSON/CSV/XSPF) + album completeness + sleep timer ([8f7bd92](https://github.com/renesenses/tune-server-rust/commit/8f7bd92383177396754697e781c519009818798e))

### Style

- Cargo fmt ([c05b188](https://github.com/renesenses/tune-server-rust/commit/c05b18875dff49adeafd878c104eb9e8a716f4bf))

### Tests

- OAAT sustained stream load test — 5s continuous playback ([4edc1e9](https://github.com/renesenses/tune-server-rust/commit/4edc1e91159fbd08e2b72eef5657d636652641ae))
## [0.8.19] — 2026-06-01

### Bug fixes

- Add tune-cli to Dockerfile workspace (build failure) ([f47dc21](https://github.com/renesenses/tune-server-rust/commit/f47dc21f5e2dae0ecd700cf574318353f09e6a09))

### Features

- API analytics — per-endpoint latency tracking with ring buffer ([bd66a3c](https://github.com/renesenses/tune-server-rust/commit/bd66a3c0228714702573e422bf18a750bd856d2f))
- Multi-room sync dashboard — GET /zones/sync-status ([61b4fed](https://github.com/renesenses/tune-server-rust/commit/61b4fedc9c706a84f3c902e7e4b22d8aa94b59d1))
- Waveform preview — cached per-track amplitude data ([11355c8](https://github.com/renesenses/tune-server-rust/commit/11355c8faac42b0e96c3b1a6946dc37f86af3008))
- Audio fingerprinting — AcoustID track identification ([61064f7](https://github.com/renesenses/tune-server-rust/commit/61064f795f268e8d114945d8fd42349094cb0942))
- Restore OAAT multiroom groups on server boot ([79c35ac](https://github.com/renesenses/tune-server-rust/commit/79c35acc0e0dbd42cd5ffba07cf2e7e56e945cd2))
- Cross-source track matching — local <-> streaming dedup ([37102aa](https://github.com/renesenses/tune-server-rust/commit/37102aa225d9e9903146432a85754e808e348049))
- Spotify create_playlist + add_tracks_to_playlist ([7fbee12](https://github.com/renesenses/tune-server-rust/commit/7fbee126dd6e7a1d77802ee3779faab6603a82d8))
- Tidal create_playlist + fix Spotify user_id persistence ([dc355a8](https://github.com/renesenses/tune-server-rust/commit/dc355a8e5d8c6cc133acfe39c9995fd114572d0f))
- Qobuz + Deezer create_playlist + playlist write tests + web client deploy ([2b1547e](https://github.com/renesenses/tune-server-rust/commit/2b1547e2e870056bcc74e977f1150d31c6d24c2f))
- Album timeline gaps + career span ([8a5f5d4](https://github.com/renesenses/tune-server-rust/commit/8a5f5d40fe634d41090888c79931b4492e73cf16))
- Network audio analyzer — per-zone bytes/latency tracking ([85c4203](https://github.com/renesenses/tune-server-rust/commit/85c4203e23a28811c7c1dfecea2288b6d73e701b))
- Intelligent crossfade + fingerprint duplicate finder ([8fd7e9b](https://github.com/renesenses/tune-server-rust/commit/8fd7e9b5ebfaff1e7b296d5c397354953698b49c))
- Synced lyrics — LRC parser + sidecar file detection ([fcac902](https://github.com/renesenses/tune-server-rust/commit/fcac902c2490803196277ba24ec98947d2fffafc))
- Radio auto-DJ — seed-based infinite playlist generation ([7d86333](https://github.com/renesenses/tune-server-rust/commit/7d8633397e05b91819730e1724db1630c083cb63))
- JRiver import + encrypted backups ([a4726b5](https://github.com/renesenses/tune-server-rust/commit/a4726b5beac25afabc4c53d36a6116a894a33532))
- Collaborative playlist rooms — WebSocket-ready party mode ([67b242a](https://github.com/renesenses/tune-server-rust/commit/67b242ae3d92ff8aede6566cbda2e538a7a3b7f8))
- Tune-cli — command line companion for Tune server ([211bc2b](https://github.com/renesenses/tune-server-rust/commit/211bc2b5ccc06822db6221f43c51569c67d5b014))

### Style

- Cargo fmt ([b2946ff](https://github.com/renesenses/tune-server-rust/commit/b2946ffd70b614f8d64beba02a8669e8297f0fca))
## [0.8.18] — 2026-06-01

### Bug fixes

- Use write connection for get_or_create during scan (album grouping regression) ([31424ba](https://github.com/renesenses/tune-server-rust/commit/31424ba00b52c3e200669b69661ffdab60886290))
- Albums with multiple genres now appear under all their genres in browse ([99497aa](https://github.com/renesenses/tune-server-rust/commit/99497aa382e3f486fdde0966f3df4c17dddd58c8))
- Gapless playback restart on DMP-A6 — 5s guard period ([b6c2b46](https://github.com/renesenses/tune-server-rust/commit/b6c2b463fff0f2194e1c28406e0ad6902a7a8348))

### Features

- Enriched bug report — FFmpeg version, RSS, OAAT status, markdown endpoint ([050ad97](https://github.com/renesenses/tune-server-rust/commit/050ad97d86481b11e5ec0e5a9fd476665abd6c4b))
- Windows SMB discovery via net view ([08808ee](https://github.com/renesenses/tune-server-rust/commit/08808ee55725dc0c23824bca086dd6a398410918))
- DLNA integration tests with axum-based mock renderer ([ccc514c](https://github.com/renesenses/tune-server-rust/commit/ccc514c4d967ed1d290c63f1bf23b70dd820534d))
- DLNA integration tests with axum-based mock renderer ([a999ab4](https://github.com/renesenses/tune-server-rust/commit/a999ab4ad1bdad9f1b58c9398c005b9e9a2706ba))
- Docker production compose + install script ([c1d8b00](https://github.com/renesenses/tune-server-rust/commit/c1d8b00518459031f731e2343f6a1dbdb7f38dad))
- OAAT multiroom groups API — create, list, delete ([953e26b](https://github.com/renesenses/tune-server-rust/commit/953e26b28ef44f1af18a3c3d46b74533a2410d95))
- Listening history CSV export endpoint ([8bbb1ee](https://github.com/renesenses/tune-server-rust/commit/8bbb1ee61ef6acc2d91d7aed91e8525d955b5258))
- Plugin SDK — on_event callback, config read/write, event bus support ([5dd8ad1](https://github.com/renesenses/tune-server-rust/commit/5dd8ad19ca516b61bd5c8e2477a9ffa42bf4f9bf))

### Style

- Cargo fmt ([a961b5f](https://github.com/renesenses/tune-server-rust/commit/a961b5fe58025a41e6e7697f1ce028a8ce6eb028))
## [0.8.17] — 2026-06-01

### Bug fixes

- Add /zones/group-delays endpoint + reduce WS ping to 15s ([00e79b1](https://github.com/renesenses/tune-server-rust/commit/00e79b1808ebd2f7f4ffa1dcb74ce001b73e065d))
- Enable local-audio feature for Windows release builds ([afbf006](https://github.com/renesenses/tune-server-rust/commit/afbf006533e8b72c11d2d9021e31c44f1cff3fd8))
- Detect Program Files install and auto-migrate data to %LOCALAPPDATA% ([9ca976d](https://github.com/renesenses/tune-server-rust/commit/9ca976d3591bf44578e6bfc9694b05623eb55b24))

### Features

- OAAT diagnostics endpoint + watchdog + unit tests ([e92dbd3](https://github.com/renesenses/tune-server-rust/commit/e92dbd30e38355386f79b63f524a9e9bcd0c1de9))
- OAAT integration test + FLAC pacing fix ([94cd6a4](https://github.com/renesenses/tune-server-rust/commit/94cd6a44b294efb9e21c18ce8a45b59c56a275db))
- DSD passthrough + seek support for OAAT output ([afc6022](https://github.com/renesenses/tune-server-rust/commit/afc60225ff035cea95fc209fd4cc00377240b75c))
- Library health score, streaming compare, demo mode, zone metrics, DLNA GetProtocolInfo ([0438b6a](https://github.com/renesenses/tune-server-rust/commit/0438b6a57d1ad3dc68e4d064a92a9dee579f9938))
## [0.8.16] — 2026-06-01

### Bug fixes

- WAV Content-Length for DLNA renderers + eliminate double-fetch ([c8b5d1f](https://github.com/renesenses/tune-server-rust/commit/c8b5d1fd76549e09c08990d0b7f44b1f6f78ba2c))
- Replace 19 silent filter_map(|r| r.ok()) with proper error propagation ([e1b6363](https://github.com/renesenses/tune-server-rust/commit/e1b63633f0f2f4d2c850d7831cc7cec008930327))
- Poller backoff — resolve undefined `ps` and borrow conflict ([9f5e16d](https://github.com/renesenses/tune-server-rust/commit/9f5e16d9be094986d07a97ffdb7761d13ba7aa63))
- Gate OAAT module behind feature flag for --no-default-features CI ([bd51bdb](https://github.com/renesenses/tune-server-rust/commit/bd51bdbceb978d9cd7b1f152e28be5abd693a1c8))
- Add --features oaat to all CI/Release/Docker builds ([e47dd0f](https://github.com/renesenses/tune-server-rust/commit/e47dd0f64ff5ab1e8af49e2c9dd8b62836d06ea2))

### Chore

- Remove orphan agent worktree refs ([9ca9fef](https://github.com/renesenses/tune-server-rust/commit/9ca9fefa65d9688bd2952a448a4482a8d8fbb9cf))
- Remove dead ZoneManager struct ([cefb137](https://github.com/renesenses/tune-server-rust/commit/cefb1373727dcec86b154ea530b88cc2cf58f321))

### Features

- Now playing credits in zone status + clippy collapsible ifs ([8211175](https://github.com/renesenses/tune-server-rust/commit/8211175ecd917712aa392ff6ec7c58a37ea190d2))
- OAAT controller — full protocol compliance + command relay ([e122ee5](https://github.com/renesenses/tune-server-rust/commit/e122ee5b453c9c7a8e54bec36ba50fcf27ed426a))
- Changelog enriched + telemetry opt-in + health check script ([418fc79](https://github.com/renesenses/tune-server-rust/commit/418fc798e17ce573c54e77e803d6cfa887db5fe9))
- DLNA poll backoff + dev profile + OAAT stream fix ([e6bf9d5](https://github.com/renesenses/tune-server-rust/commit/e6bf9d528b6591423bb96c70f3046728184d6c93))
- OAAT module split + FLAC passthrough + mid-stream reconnection ([723f660](https://github.com/renesenses/tune-server-rust/commit/723f6603f626b29e7e6e9c32410b705cd4794dc4))
- OAAT multi-room output using Zone for synchronized playback ([fb4dc30](https://github.com/renesenses/tune-server-rust/commit/fb4dc303fb9a687785782fad416e621429f6e414))

### Style

- Cargo fmt ([9bd2541](https://github.com/renesenses/tune-server-rust/commit/9bd25417f5c7f158581a2ceb0a4c796b8d97f14e))
## [0.8.15] — 2026-06-01

### Bug fixes

- Gapless transition false positive on DMP-A6/A8 renderers ([711373f](https://github.com/renesenses/tune-server-rust/commit/711373f114735782cdba2834b44b114981f5f7f7))
- DLNA parse_time sub-second precision + Windows crash log ([954842e](https://github.com/renesenses/tune-server-rust/commit/954842edef99f02a1026479fdd1d083084275ed8))
- Gapless cooldown prevents double-play restart + poller/DLNA tests ([64434c3](https://github.com/renesenses/tune-server-rust/commit/64434c3cbca92b63e33b4c9c9936ea9481565ed4))
- OAAT controller — ghost sessions, format negotiation, clock sync timeout ([2e1f0c3](https://github.com/renesenses/tune-server-rust/commit/2e1f0c34d4fc4de319c90f0304baa5938b9dc8c3))

### Style

- Cargo fmt ([1fab734](https://github.com/renesenses/tune-server-rust/commit/1fab734d8b428b3fda252913e74452f85414cddb))

### Tests

- MockOutput + e2e playback tests for gapless non-regression ([c724745](https://github.com/renesenses/tune-server-rust/commit/c724745f7fb71586aa782c258960eacc504913b3))
## [0.8.14] — 2026-06-01

### Bug fixes

- Add WebSocket ping/pong keepalive to prevent connection drops ([86a0db8](https://github.com/renesenses/tune-server-rust/commit/86a0db8a15ca34e178c2dbab6a477c7e8546ae4a))
- Omit DLNA dc:creator tag when artist is None, empty, or "null" ([f46abdf](https://github.com/renesenses/tune-server-rust/commit/f46abdfef55cf30a953148799c4c90db19103a7f))
- WS subscribe format mismatch + scan progress throttle ([e793514](https://github.com/renesenses/tune-server-rust/commit/e79351442d27336af971fc6365eb4f59f52dc90e))
- Zones count = 0 in dashboard + schema divergence + 25 new tests ([5c2dbfe](https://github.com/renesenses/tune-server-rust/commit/5c2dbfe1f5c34ec9206937eaa966ac8e90ff4880))
## [0.8.13] — 2026-06-01

### Bug fixes

- Album grouping — avoid 1:1 track/album ratio ([56378d2](https://github.com/renesenses/tune-server-rust/commit/56378d20c1ea6187a57826404521cddd35ad0d72))

### Other

- V0.8.13 ([1cd5bf2](https://github.com/renesenses/tune-server-rust/commit/1cd5bf2352293341038079a2124daeeb87900dc9))

### Style

- Cargo fmt (69 files) ([b6b2939](https://github.com/renesenses/tune-server-rust/commit/b6b29392ffbcfeffb9cf9df189b58233c8696db2))
## [0.8.12] — 2026-05-31

### Bug fixes

- Gapless transition — advance metadata only, don't re-send track ([8010a0c](https://github.com/renesenses/tune-server-rust/commit/8010a0cf24d2a640ca2e98367ddc4cd86dafe19b))
- Windows crash — resolve db_path to LOCALAPPDATA when relative ([a0b91db](https://github.com/renesenses/tune-server-rust/commit/a0b91dbfb7cd202233fb7f5378bd0019926c7705))

### Features

- IOS/iPadOS/Flutter compatibility with Rust server ([30626f2](https://github.com/renesenses/tune-server-rust/commit/30626f25a6c9019c7694efa856ff2a770b92c69a))
- Integrate SpotifyConnectManager — launch librespot on enable ([3ff9d58](https://github.com/renesenses/tune-server-rust/commit/3ff9d5859db1a3e8388659b9dad2d0f9f5cc4718))
- Multichannel audio support — ITU-R BS.775 downmix, streaming channels ([2c254fa](https://github.com/renesenses/tune-server-rust/commit/2c254fa0d1baaada2754ed4ade2258306a629ec0))

### Other

- V0.8.12 ([9076fd2](https://github.com/renesenses/tune-server-rust/commit/9076fd2507680c8786a110efe4fb9b9e9e35dd18))

### Performance

- Shared HTTP client, Arc caches, batch stats, pagination metadata, SQLite NO_MUTEX ([d759576](https://github.com/renesenses/tune-server-rust/commit/d759576093eeb3df8269039b667aa9c3bb7412db))
- API response cache, database_status batch, scan panic guard, artwork orphan cleanup ([e1c439f](https://github.com/renesenses/tune-server-rust/commit/e1c439f091114f57222f7e4daac28a77f77557ac))

### Refactor

- Clippy cleanup — remove unused imports and variables ([32a2ed0](https://github.com/renesenses/tune-server-rust/commit/32a2ed04368264d5f174e82b27921d66cd2e5445))
- Group tune-core flat modules into library/ directory ([bc463fd](https://github.com/renesenses/tune-server-rust/commit/bc463fd8526fdd542179509ddb8e2665afb0e534))
- DB layer — replace filter_map(ok) with proper error collection ([4e4989d](https://github.com/renesenses/tune-server-rust/commit/4e4989d0fff033ef68bebe5d5b584f1015621ca2))
- Standardize error handling — AppError + Result for all route handlers ([057be61](https://github.com/renesenses/tune-server-rust/commit/057be617beb923ab76bf0b60cd1173386d8b7edb))
- Decompose main.rs — extract auto_scan, background, discovery, startup ([e572cf0](https://github.com/renesenses/tune-server-rust/commit/e572cf0a0ce0cf941b49411a8e46c391abd15f1e))
- Split system.rs and library.rs into directory modules ([bebecf2](https://github.com/renesenses/tune-server-rust/commit/bebecf2332d78ad4ea7156d0a0ff850b877c4e44))
## [0.8.11] — 2026-05-31

### Bug fixes

- Add Windows support for SMB host scan (net view) ([b3cc831](https://github.com/renesenses/tune-server-rust/commit/b3cc8310ee0e4caf415c0e5c45be44632ff32fd9))
- DLNA cover art URL — wrong path /artwork/ instead of /library/artwork/ ([6d5c465](https://github.com/renesenses/tune-server-rust/commit/6d5c46545fcc71493d242bdf6234068cf3afa885))
- DLNA WAV skip + artists without albums ([03e5e0e](https://github.com/renesenses/tune-server-rust/commit/03e5e0e35fc3b3409ebb40f5d0728f89d4905347))
- OAAT controller — add detailed logging for connect/stream lifecycle ([e21a805](https://github.com/renesenses/tune-server-rust/commit/e21a805c64a192de2b8e476468000c90bcfff65d))
- Artist_repo tests — link albums for count/list filter ([8b87452](https://github.com/renesenses/tune-server-rust/commit/8b87452bb7e6c6cacd60797989368abb0818fdc2))

### Other

- V0.8.11 ([a635d89](https://github.com/renesenses/tune-server-rust/commit/a635d89f68a47fc8ca9c07606e7728c89a018903))
## [0.8.10] — 2026-05-30

### Bug fixes

- Implement /logs endpoint — read from file or journalctl ([3b84d46](https://github.com/renesenses/tune-server-rust/commit/3b84d46076527228bf2d4fb77d871c88eff2736b))
- OAAT output_not_found — double 'oaat:' prefix in device_id ([38921f5](https://github.com/renesenses/tune-server-rust/commit/38921f5aeac49a4facd7a4340202fce4798b8878))
- OAAT always use WAV stream, not FLAC ([4647a12](https://github.com/renesenses/tune-server-rust/commit/4647a120849221bd0136207480df997f293b720d))
- Add 'added' field to scan progress WebSocket event ([6d3c42e](https://github.com/renesenses/tune-server-rust/commit/6d3c42e379b11fc787dee7f6c80131342067964e))
- Include AirPlay/OAAT/Squeezebox/Chromecast in device list ([a306db7](https://github.com/renesenses/tune-server-rust/commit/a306db7420fe61de4232c5be766376b94bd9f288))
- SSDP search timeout 3s → 6s, discover slow renderers (Lindemann) ([d79eb0c](https://github.com/renesenses/tune-server-rust/commit/d79eb0c2e2865141a10fca22d4b1c91aead1412c))
- SSDP recv error should continue, not break scan loop ([62eb10b](https://github.com/renesenses/tune-server-rust/commit/62eb10b3f1c8028b125727a596b19ca69901d2f6))
- Implement SMB network scan — replace stub with mDNS discovery ([6d10265](https://github.com/renesenses/tune-server-rust/commit/6d102653f7ff477d0295d7c1f86911aa6114c461))

### Chore

- Update Cargo.lock ([8178004](https://github.com/renesenses/tune-server-rust/commit/817800414eacc1817ed1f2f0e05ead945e0d21ae))

### Other

- SSDP scan diagnostics — count raw packets vs parsed ([3e03955](https://github.com/renesenses/tune-server-rust/commit/3e03955e9b577d51d41ef04b0d6f5d174488cc0c))
- V0.8.10 ([65d0ee7](https://github.com/renesenses/tune-server-rust/commit/65d0ee79d87bb221c70c32765b29dc36c3033570))

### Style

- Cargo fmt ([c1c86ae](https://github.com/renesenses/tune-server-rust/commit/c1c86aec909f4b9c3e448f4a08c87de2a6db99c5))
## [0.8.9] — 2026-05-30

### Bug fixes

- Stop→play resume, orphan artist cleanup, theme persistence ([9f217b5](https://github.com/renesenses/tune-server-rust/commit/9f217b5ef6e7d39ff86501b1bbd215bdcec67454))
- DSF/DFF format detection + queue clear WebSocket event ([19ac5fb](https://github.com/renesenses/tune-server-rust/commit/19ac5fbf29b2c27e265d9e3138bfbd9f5502fb18))

### Other

- V0.8.9 ([8c0f2bb](https://github.com/renesenses/tune-server-rust/commit/8c0f2bb7cdb724ad92b19539766d2208ab6a0b5d))

### Style

- Cargo fmt ([b89ecab](https://github.com/renesenses/tune-server-rust/commit/b89ecab1edc0399ec51e9f8ce1f2f552d7414cdd))
## [0.8.8] — 2026-05-30

### Bug fixes

- Read TUNE_DISCOGS_TOKEN from env + use local time in logs ([b1e22e5](https://github.com/renesenses/tune-server-rust/commit/b1e22e53bde7b299690cd1c276acd422fb0e0bd8))

### Other

- Merge remote-tracking branch 'origin/fix/parallel-scan' into fix/discogs-env-local-time ([8291f5e](https://github.com/renesenses/tune-server-rust/commit/8291f5e18ed2a26ee4612c2565fb33bfadf86ad7))
- V0.8.8 ([1ab997f](https://github.com/renesenses/tune-server-rust/commit/1ab997f9d23e0246773a7e04dc343212fc28a93a))

### Performance

- Optimize scanner — pre-filter unchanged files, transaction batching, in-memory caches ([8e98b1f](https://github.com/renesenses/tune-server-rust/commit/8e98b1fb3de3c8e710843147764da8e07e2ace4b))
- Batched parallel scan with progressive DB availability ([c1f5716](https://github.com/renesenses/tune-server-rust/commit/c1f5716a5a099e9a814007d951e6756ab23f836c))

### Style

- Cargo fmt ([e10b95e](https://github.com/renesenses/tune-server-rust/commit/e10b95e5ba871348259ae688a12e40fe8cbd9edb))
## [0.8.7] — 2026-05-30

### Bug fixes

- Smart playlist persistence race + stop keeps NowPlaying ([a4a05fc](https://github.com/renesenses/tune-server-rust/commit/a4a05fca0a7d831112830f31601228fd8d373684))
- Add pagination to history and doubtful metadata endpoints ([ae6a739](https://github.com/renesenses/tune-server-rust/commit/ae6a7394f607934ed9c0dd915b9105a228942513))

### Features

- Add v0.8.5 + v0.8.6 to /system/changelog ([72d6ede](https://github.com/renesenses/tune-server-rust/commit/72d6edeba90c376e743a51741680d2894a031fb6))
- OAAT via crates.io — CI-compatible, no more path deps ([8d997e0](https://github.com/renesenses/tune-server-rust/commit/8d997e000319c89cb7a3e38a244c49da1e790df0))

### Other

- V0.8.7 ([d79fa40](https://github.com/renesenses/tune-server-rust/commit/d79fa402510681205757453c1b2fd3c94f10b7a5))

### Style

- Cargo fmt ([3fdbc5d](https://github.com/renesenses/tune-server-rust/commit/3fdbc5db81c32bc80000296545603fae0e12d431))
## [0.8.6] — 2026-05-29

### Bug fixes

- Handle actual Qobuz genre API response format ([341cb6a](https://github.com/renesenses/tune-server-rust/commit/341cb6adc0c0d69b0ed320e20e25b48f3a920b0b))
- Expose onboarding_completed in API + populate genres JSON array ([6996b40](https://github.com/renesenses/tune-server-rust/commit/6996b40c5c35e8d95e192eb4555dd08327a3d2ec))
- Add per-zone gapless toggle and resolve cover art URLs for renderers ([550d3df](https://github.com/renesenses/tune-server-rust/commit/550d3dfa6fdc34a3baf935dd70bd6844edd2d359))
- Play queue not populated when launching album or playlist ([a3287a7](https://github.com/renesenses/tune-server-rust/commit/a3287a7e32141ca515479c1b4042739db43cd61c))
- Remove oaat path deps (breaks CI, same as v0.8.4 fix) ([ecefa18](https://github.com/renesenses/tune-server-rust/commit/ecefa18e1f52e5423f3ccfc519e4543668df8bb5))

### Features

- Enable OAAT output feature flag for bit-perfect audio streaming ([8502a98](https://github.com/renesenses/tune-server-rust/commit/8502a98161f2dea71a4e82829ed926ea87ac8fda))
- Add POST /api/v1/system/library/clear endpoint ([9fb80b8](https://github.com/renesenses/tune-server-rust/commit/9fb80b8f7c2abe3d0643d6337bbb69957a361ec9))

### Other

- Merge remote-tracking branch 'origin/fix/onboarding-genres' ([99ae15b](https://github.com/renesenses/tune-server-rust/commit/99ae15b80f49dc02081fa235c03b60af8dd86dbe))
- Merge remote-tracking branch 'origin/fix/play-queue-album' into fix/dlna-gapless-cover ([5309d93](https://github.com/renesenses/tune-server-rust/commit/5309d93c2410c94c9c02efe16cd9f589b1ce6181))
- V0.8.6 ([026c592](https://github.com/renesenses/tune-server-rust/commit/026c5928e7cc0e42476d71fde44bed6f04e19eec))

### Style

- Cargo fmt ([e623685](https://github.com/renesenses/tune-server-rust/commit/e623685d0f5b15633bff8ae0a50f2835377cb38d))
## [0.8.5] — 2026-05-29

### Bug fixes

- Enumerate real network interfaces for SSDP discovery ([c529e56](https://github.com/renesenses/tune-server-rust/commit/c529e5648fce839c7c19f16fe0e91d64d4fb6f45))
- Read TUNE_SPOTIFY_CLIENT_ID env var for OAuth authorization URL ([192417f](https://github.com/renesenses/tune-server-rust/commit/192417f0ff97f627491e87486fc11e8d559af61d))
- Normalize mp4 format to aac + remove recovered source ([3a8326d](https://github.com/renesenses/tune-server-rust/commit/3a8326dc443c7e67a258fbb78f0349f0ef2f9b01))
- Squeezebox discovery JSON-parse error on empty LMS response ([2c18423](https://github.com/renesenses/tune-server-rust/commit/2c1842315d2bb6d7172bf2a4dbab10394c2b5436))
- Library scan returns 0 results on Windows due to path handling bugs ([839b109](https://github.com/renesenses/tune-server-rust/commit/839b1094a0f13d86bf845b8490d247b05d19738a))

### Features

- Populate /system/changelog with v0.8.3 + v0.8.4 release notes ([61b5c03](https://github.com/renesenses/tune-server-rust/commit/61b5c0365ec17aadde1be03f94b15f2851c6a848))
- Implement local USB audio output via cpal with real audio streaming ([3a642ce](https://github.com/renesenses/tune-server-rust/commit/3a642ce9b2c919147ea90055f68be6ef7180bc0d))

### Other

- Merge remote-tracking branch 'origin/fix/spotify-oauth-env-vars' ([78473b7](https://github.com/renesenses/tune-server-rust/commit/78473b773d55a43b5f47a4ee8e0facc248ff6bff))
- Merge remote-tracking branch 'origin/fix/squeezebox-discovery-json-parse' ([4037800](https://github.com/renesenses/tune-server-rust/commit/4037800e448e80a2281524423b1100726b16dee4))
- V0.8.5 ([0b763e0](https://github.com/renesenses/tune-server-rust/commit/0b763e0ddc29936ff037a1c37f73cd763e7a1e79))

### Style

- Cargo fmt ([fce014d](https://github.com/renesenses/tune-server-rust/commit/fce014d211bf759139d6954bf32c2a8994f854bc))
## [0.8.4] — 2026-05-29

### Bug fixes

- Memory leak prevention — session GC, cache eviction, SSDP cleanup ([d806fc3](https://github.com/renesenses/tune-server-rust/commit/d806fc32ecfc047f3b7cc8959f7e40c404236cb9))
- Add explicit -f dsf input format for FFmpeg DSD transcoding ([b5f4017](https://github.com/renesenses/tune-server-rust/commit/b5f4017c986192c4ca53dc596d38a008bf500804))
- Remove oaat path deps from committed Cargo.toml ([1b454d3](https://github.com/renesenses/tune-server-rust/commit/1b454d3c4cef68cc9e4513496c4c965e539f007a))

### CI / build

- Trigger clean CI run ([e105e4c](https://github.com/renesenses/tune-server-rust/commit/e105e4c115558cea91b2c01a4ebb9db3573da2e1))

### Features

- SOAP retry, mute detection, cover art, album in DIDL ([dab312b](https://github.com/renesenses/tune-server-rust/commit/dab312bd4844c7277556bffea9333c5392985db2))
- AirPlay 1 (RAOP) output with native Rust implementation ([98d4202](https://github.com/renesenses/tune-server-rust/commit/98d4202763c89e9e61bc4bb01299acb1b15fa846))
- BluOS + OpenHome outputs with UPnP eventing ([5cbd130](https://github.com/renesenses/tune-server-rust/commit/5cbd1302eaa66601f60d2a8d8bb543c00c12a1d8))
- Deezer decrypt proxy, alarm scheduler, shuffle, ICY metadata, minimal DMR ([d57ec0f](https://github.com/renesenses/tune-server-rust/commit/d57ec0fee9ea76e50528d9fba4d77799a5a2bdf5))
- Port major Python features to Rust — 30+ core modules ([d4eddae](https://github.com/renesenses/tune-server-rust/commit/d4eddae3d433d5cf3639f0d52e49cdf3b1794c4f))
- Batch 4 — DJ player, auto-fix, playlist transfer, credentials vault, fingerprint, remote proxy ([2eb32d3](https://github.com/renesenses/tune-server-rust/commit/2eb32d3b5bdef713bad2e23261f6e1fc4f66f627))
- Batch 5 — audio analyzer, resampler, library watcher, cover fetcher, duplicate detector, scrobble enhancements ([03a7d3d](https://github.com/renesenses/tune-server-rust/commit/03a7d3d2ee5c531f3d978b684d4149cb09062bda))
- Batch 6 — bug report, dashboard, user profiles, party mode, library importer ([ad0d0c9](https://github.com/renesenses/tune-server-rust/commit/ad0d0c93ad855b18c257b852a2bf2ba5c3a004b5))
- Batch 7 — credit enricher, deezer proxy, FTS, event types, batch metadata, audio encoder ([7d1a587](https://github.com/renesenses/tune-server-rust/commit/7d1a587197232e82e971ebf46d9f8832a3530418))
- Batch 8 — config, services manager, M3U parser, health monitor, export CSV, playback history ([97f9c86](https://github.com/renesenses/tune-server-rust/commit/97f9c860bdec4f3c171629e8ee9ba2c7db8c703f))
- Batch 9 — db backup, stream cache, radio favorites, SMB discovery, mount manager ([c15854c](https://github.com/renesenses/tune-server-rust/commit/c15854c6bda080ddd42c79d98c674109fed31c06))
- Batch 10 — artist enrichment, MusicBrainz release lookup, metadata suggestions, Last.fm enrichment ([c7ca3ee](https://github.com/renesenses/tune-server-rust/commit/c7ca3ee091098234b008b310882a5d79c0320837))
- Batch 11 — zone manager, plugin SDK, metadata matcher (final tune-core batch) ([bfa2066](https://github.com/renesenses/tune-server-rust/commit/bfa20660174e0a0e6c405c24fef028896d3560b7))
- Wire tune-core services into tune-server routes ([ab3c68d](https://github.com/renesenses/tune-server-rust/commit/ab3c68db489f9d15ebe4950895c482650e037cd6))

### Other

- Add OAAT output scaffold for tune-core

New output type implementing OutputTarget trait for OAAT endpoints.
Registers as "oaat" type with device_id format "oaat:{endpoint_id}".

Scaffold implements full trait interface (play_media, pause, resume,
stop, seek, volume, mute, status, availability) with state tracking.

Actual OAAT protocol streaming (connect, handshake, UDP audio) is
marked TODO — will be wired once oaat-controller crate is added
as a dependency.

See: https://github.com/renesenses/oaat ([a9b2856](https://github.com/renesenses/tune-server-rust/commit/a9b2856bfd5795016223bf3225c7f7088f2769bb))
- Add OAAT output with real protocol streaming for tune-core

OaatOutput implements OutputTarget trait with full OAAT protocol:
- play_media(): connect to endpoint, handshake, clock sync bootstrap,
  format negotiate (PCM 16/44.1), metadata, play, then fetch WAV from
  Tune's HTTP streamer, skip header, stream PCM via OAAT UDP packets
  with real-time pacing and position tracking
- pause/resume: atomic flags respected by streaming loop
- stop: oneshot signal cleanly terminates streaming task
- is_available: TCP probe to endpoint
- get_status: live position_ms, transport state, metadata

Feature-gated behind "oaat" feature flag:
  cargo build -p tune-core --features oaat

Dependencies: oaat-core + oaat-controller as path deps
(switch to git deps for CI: github.com/renesenses/oaat) ([fb7f864](https://github.com/renesenses/tune-server-rust/commit/fb7f864a5e9b577445c9da596e3cbf8ed88cc60a))
- Wire OAAT auto-discovery via mDNS into Tune

OAAT endpoints now auto-register like Chromecast/AirPlay/BluOS:
- Added OutputType::Oaat with highest priority (8)
- MdnsScanner.with_oaat() browses _oaat._tcp.local.
- Parses OAAT TXT records: name, id, caps, ch, vendor, model, fw
- main.rs creates OaatOutput on discovery, registers in OutputRegistry
- Auto-creates zone for each discovered OAAT endpoint

Feature-gated: oaat feature enabled by default in tune-server. ([5d004c0](https://github.com/renesenses/tune-server-rust/commit/5d004c00578666909f92a172a073ea3d7275709a))
- Smart bit-perfect format: parse WAV header from stream

OaatOutput now reads the WAV header to detect actual sample rate,
bit depth, and channels from the HTTP stream before proposing format:
- 16-bit → PcmS16le, 24-bit → PcmS24le, 32-bit → PcmS32le
- Actual sample rate from WAV (44.1k, 48k, 96k, 192k, etc.)
- Actual channel count from WAV
- Proper "data" chunk search (handles extended WAV headers)

This enables true bit-perfect streaming: a 24/96 FLAC file in Tune
gets decoded to 24/96 WAV, parsed correctly, and proposed as
PcmS24le @ 96000Hz to the OAAT endpoint. ([a8a006a](https://github.com/renesenses/tune-server-rust/commit/a8a006a2156504461d5c3790228c4a28ad0946c3))
- Auto FLAC transport when endpoint supports it ([0d78a05](https://github.com/renesenses/tune-server-rust/commit/0d78a05f63aa4389718a80b13de6e3d8e658688e))
- Add tls: false to OAAT ControllerConfig (matches new TLS field) ([dbaa674](https://github.com/renesenses/tune-server-rust/commit/dbaa674ac5b1b155bfad7d411e2f4e57d3de2520))
- Add Bridge server-side: BridgeOutput + /ws/bridge endpoint

BridgeOutput (tune-core/src/outputs/bridge.rs):
- Implements OutputTarget trait, sends commands via mpsc channel
- Request-response correlation with UUID IDs and oneshot channels
- 10s timeout per command, connected flag for availability

WebSocket bridge endpoint (tune-server/src/routes/bridge.rs):
- GET /ws/bridge?api_key=xxx — API key authenticated
- Handles bridge.hello, bridge.devices, bridge.device_lost, bridge.response
- Auto-creates zones for bridge-discovered devices
- Cleanup on disconnect: removes outputs, sets zones offline

AppState extended with bridge_responses HashMap for response routing.

Zero changes to playback routes, orchestrator, or web UI. ([4856da2](https://github.com/renesenses/tune-server-rust/commit/4856da22091817f1aea23a2c24057e27cf6e1c54))
- V0.8.4 ([e821298](https://github.com/renesenses/tune-server-rust/commit/e82129837a6a4752846b9110b278c142845b87f9))

### Performance

- SQLite optimization for large collections + runtime fixes ([9749733](https://github.com/renesenses/tune-server-rust/commit/97497338ee037a93381e3ca0b86ebddefcb1c390))

### Style

- Cargo fmt (9 files) ([bcbb607](https://github.com/renesenses/tune-server-rust/commit/bcbb6072c6673309642800b6d367ed2011c8ee37))
- Cargo fmt across workspace (81 files) ([c063918](https://github.com/renesenses/tune-server-rust/commit/c063918bdb48c85b649fb08cdc1eb239200160fb))
## [0.8.3] — 2026-05-29

### Bug fixes

- Install NSIS on Windows runner for installer build ([e137d05](https://github.com/renesenses/tune-server-rust/commit/e137d05161f4879a7ecf2de79b4b4e3386e39ea7))
- Squeezebox LMS error handling + config endpoint returns JSON ([6bbffa5](https://github.com/renesenses/tune-server-rust/commit/6bbffa5f401ce01904cab8260392d136491f1860))
- Reduce SSDP scan frequency after device discovery (30s → 120s) ([e28ee77](https://github.com/renesenses/tune-server-rust/commit/e28ee77d4b5ac2313b069a74795509958e269ff7))
- Single ssdp:all M-SEARCH instead of 5 sequential targets ([c02efac](https://github.com/renesenses/tune-server-rust/commit/c02efac0b202df97e1d4b494b7773aa2ac09b657))
- Relaxed MP3 metadata parsing + HTTP stream logging ([dce3896](https://github.com/renesenses/tune-server-rust/commit/dce389665108cd296c9d7d922c66ba8d1656d41e))
- FTS5 contentless tables + populate via INSERT SELECT ([b7a18a3](https://github.com/renesenses/tune-server-rust/commit/b7a18a31620fa67aa21b45600b33600da3cd394c))
- Release.yml secrets syntax + Docker fingerprint cleanup ([ad9f958](https://github.com/renesenses/tune-server-rust/commit/ad9f95880ce9edf618b185c6cff37a29f0597fcc))
- Wrap entire if condition in single \${{ }} expression ([0f14aeb](https://github.com/renesenses/tune-server-rust/commit/0f14aeb57ddea3b34f8437590e24ec6039e10454))
- Use shell-level checks for secrets instead of if conditions ([eb015f3](https://github.com/renesenses/tune-server-rust/commit/eb015f325a5416e7868c746b0f62c57e850df478))

### Other

- V0.8.3 ([bc6f675](https://github.com/renesenses/tune-server-rust/commit/bc6f675cf33957690c157ad445c584ac0361a964))

### Style

- Cargo fmt metadata.rs + squeezebox.rs ([bc3da06](https://github.com/renesenses/tune-server-rust/commit/bc3da066fbb2e7401df5ff6dc3e759a8060c003a))
## [0.8.2] — 2026-05-28

### Bug fixes

- Update zone online status on SSDP device discovery/loss ([8766bb4](https://github.com/renesenses/tune-server-rust/commit/8766bb4aa2bdc7297be750ff31b14702f9acb18e))
- Target tune-core+tune-server only, skip tune-pyo3 (needs ALSA) ([b35e10f](https://github.com/renesenses/tune-server-rust/commit/b35e10f5533a905c7561e61fc54be39e661db9e8))
- Allow unused_mut on search_paths (conditional cfg) ([653c07a](https://github.com/renesenses/tune-server-rust/commit/653c07ac248e2cb5b4f5eaa07a21ce812b0c89a8))
- Resolve clippy items_after_test_module + unused_mut warnings ([bb69dc1](https://github.com/renesenses/tune-server-rust/commit/bb69dc1384cfa105d175c8c73e30ffae1f4916d1))
- Resolve all clippy warnings blocking CI ([5de0886](https://github.com/renesenses/tune-server-rust/commit/5de0886acbb9b94669638e07c0ad042cd6e2a49e))
- Migrate macos-13 to macos-15-intel (deprecated Dec 2025) ([c7e6409](https://github.com/renesenses/tune-server-rust/commit/c7e640962d9d42fd41b71b55cfb5e92e647128df))
- Disable -D warnings in release.yml (same as ci.yml) ([0ac90ca](https://github.com/renesenses/tune-server-rust/commit/0ac90ca573e6ca4665d1747ab4b8811463d7a18a))
- Use TUNE_PORT=8888 (not TUNE_API_PORT which server ignores) ([3d9ff8a](https://github.com/renesenses/tune-server-rust/commit/3d9ff8a4332d09f8e81f87f6352c0b6c59a33696))
- Healthcheck route is /system/stats not /api/system/stats ([e9fd35d](https://github.com/renesenses/tune-server-rust/commit/e9fd35df3c698d6323cd6d3adc0cdad65916c560))
- Accept both TUNE_LOG and TUNE_LOG_LEVEL env vars ([d795d7f](https://github.com/renesenses/tune-server-rust/commit/d795d7f82fc8a8f89c804741944a590e29d189e7))
- Invalidate buildx cache (stale dep layer from dummy sources) ([e3ff99e](https://github.com/renesenses/tune-server-rust/commit/e3ff99e00cb56f892e22de6af796c92a7e32f270))
- Handle SIGTERM in Docker (PID 1) + startup diagnostic log ([d9fba79](https://github.com/renesenses/tune-server-rust/commit/d9fba79afff346aa584e5f24873d7d5bb74d5516))
- DSF double WAV header + Qobuz genre auth on restart ([da8e70e](https://github.com/renesenses/tune-server-rust/commit/da8e70e60ffbe86a940fef287baed41829c6d348))
- Force recompilation after dep cache (dummy binary leak) ([c61db51](https://github.com/renesenses/tune-server-rust/commit/c61db5149e710ba253a197c3ad1655edeaa34868))

### CI / build

- Migrate CI/CD to pure Rust — drop Python/tune-server-linux refs ([5f681da](https://github.com/renesenses/tune-server-rust/commit/5f681da4e7ad53dc6c4de23093807703fb1636db))
- Dockerfile dep cache + healthcheck, fmt check, cross pre-built ([672942e](https://github.com/renesenses/tune-server-rust/commit/672942e1ecbbe1eaa23bfa236f813dd9c6f70a8a))
- Include version in asset filenames ([cff93b7](https://github.com/renesenses/tune-server-rust/commit/cff93b78d58c1869fa4f83ee6316fe40b1fbf74b))
- Add macOS DMG to release assets ([adf9701](https://github.com/renesenses/tune-server-rust/commit/adf97019998cb1e069cd5f8b5a654e36926bde3b))
- Add codesign + notarization for macOS DMG ([ea778cb](https://github.com/renesenses/tune-server-rust/commit/ea778cb5054cc4ff75f7a8f4fe156b2647ac2768))
- Add Windows NSIS installer (setup.exe) ([527d212](https://github.com/renesenses/tune-server-rust/commit/527d2123139ee9e1abf520bf5cf549bcb9c5e476))

### Chore

- Update Cargo.lock for v0.8.2 ([8630f91](https://github.com/renesenses/tune-server-rust/commit/8630f918bb544a1293e19e97cf39c8eaf7441f09))

### Features

- Add browse/discovery methods to Spotify service ([1b9e930](https://github.com/renesenses/tune-server-rust/commit/1b9e930060c3938884a5f6b47e2ea5db014e6ca8))

### Other

- V0.8.2 ([a19b77d](https://github.com/renesenses/tune-server-rust/commit/a19b77d67f698c690e108d0454f3f84ebbdc63e3))

### Style

- Apply cargo fmt across entire workspace ([eea7b81](https://github.com/renesenses/tune-server-rust/commit/eea7b8193ce40d31786697118cf78fb6c0b55f13))
- Cargo fmt config.rs ([0c74682](https://github.com/renesenses/tune-server-rust/commit/0c7468223acbc49e1bae0f1577442bde7ae49054))
## [0.8.1] — 2026-05-28

### Bug fixes

- Default port 8888 + Windows/macOS config paths ([4ff2d66](https://github.com/renesenses/tune-server-rust/commit/4ff2d6671ba93faf034eb91e5d1af97b222f23a6))
- Store stream_id in NowPlaying, cleanup old sessions on play/stop ([35811f9](https://github.com/renesenses/tune-server-rust/commit/35811f9688914de7f6c0c6cc4ddd4cb6b11097d7))
- Album list filters (format, quality) + DSD album discovery ([8371e4f](https://github.com/renesenses/tune-server-rust/commit/8371e4fc5938919dcab1ea5900a1ec819320ea0c))
- DSF/DFF fallback derives album/artist from directory structure ([c6894bd](https://github.com/renesenses/tune-server-rust/commit/c6894bdce59ecc9fcab3958473491d255d35505e))
- Paginate Tidal user playlists (was limited to 50) ([d3d8337](https://github.com/renesenses/tune-server-rust/commit/d3d833717fe56b6eee315768b0bdecd22f4d7407))
- Music-dirs 405 + zone device reconnect event ([d560c82](https://github.com/renesenses/tune-server-rust/commit/d560c82f07bfaa91abe206ee284ec055af05d2dd))
- No-cache on index.html + revert debug Bytes extractor ([0d5bc1c](https://github.com/renesenses/tune-server-rust/commit/0d5bc1c275353c0d874b28b0f26a02181ab94c22))

### Documentation

- Add docker-compose.example.yml for easy deployment ([00e09b4](https://github.com/renesenses/tune-server-rust/commit/00e09b4bd3cb14a273a9be8da6930debad8a5bcd))

### Features

- Add zones, devices, outputs counts to /system/stats ([34e49cd](https://github.com/renesenses/tune-server-rust/commit/34e49cd27f45ef4c81ad619913b1132e6bc5e239))

### Other

- Log play request body on deserialization error ([cb0011d](https://github.com/renesenses/tune-server-rust/commit/cb0011df93efaca42e114f0bc5e293b0bfa762b2))
- Use Bytes extractor for play body logging ([70baac2](https://github.com/renesenses/tune-server-rust/commit/70baac206548e02fd8df200ff3a18ee5c2a7fa3c))
- V0.8.1 ([5a0bf73](https://github.com/renesenses/tune-server-rust/commit/5a0bf73ffc10059d0b10963adeac4295c68d7395))
## [0.8.0] — 2026-05-28

### Bug fixes

- Scan progress WS events + genre search filter ([f4175be](https://github.com/renesenses/tune-server-rust/commit/f4175bec1b8b3aafe1a4826986b692791cb092b1))
- Disable LTO + use 4 codegen-units to prevent OOM on .18 builds ([57c1daa](https://github.com/renesenses/tune-server-rust/commit/57c1daac066c2620b7205e9e94ef5462e85c6133))
- Clean up stream session on stop to allow re-play on Eversolo/DLNA renderers ([45bca9d](https://github.com/renesenses/tune-server-rust/commit/45bca9dcc45b3471e82dc0b6657e28926cf72297))
- SSDP bind to 0.0.0.0 instead of local IP — VPN compatibility ([e5083ee](https://github.com/renesenses/tune-server-rust/commit/e5083ee6a7b064eb52d094a2ef855e0029d9d99e))
- Get_local_ip() prefers LAN gateway over VPN — fixes stream URLs ([5ab491d](https://github.com/renesenses/tune-server-rust/commit/5ab491da5c08d2beff1ab961ca95882f6fb05f68))
- Compilation without albumartist tag — default to "Various Artists" ([77c7700](https://github.com/renesenses/tune-server-rust/commit/77c77001d95e047a2fe8f309085d7892aa49c0a5))
- Remove unused variable ifaces (CI -D warnings) ([67b8493](https://github.com/renesenses/tune-server-rust/commit/67b8493c3a9c99bd2c4986b44f79a19875bdda9f))
- Allow dead_code in playlist_manager (CI -D warnings) ([dad0616](https://github.com/renesenses/tune-server-rust/commit/dad0616f5429edbb0861ed2ad2f1f32eabcfc1ae))
- Allow dead_code on AlbumFilters (CI -D warnings) ([708d712](https://github.com/renesenses/tune-server-rust/commit/708d712ab8c2f17d063cc6f96438d4536a4538d1))

### CI / build

- MacOS Intel cross-compile from macos-latest (macos-13 runners scarce) ([12be038](https://github.com/renesenses/tune-server-rust/commit/12be038616cff734a95ce30756d646ad8c991230))

### Features

- Full dashboard API — totals, top artists/albums/tracks, trend, hourly, zones, sources ([ddec146](https://github.com/renesenses/tune-server-rust/commit/ddec146a2a857ffa3fb7675e8859de5fee2143ee))
- XTune vinyl player as built-in plugin at /xtune/ ([1e0753b](https://github.com/renesenses/tune-server-rust/commit/1e0753bb934819469f90eb16fd33ab3dbb95505c))

### Other

- V0.8.0 — Le serveur Rust est prêt 🦀 ([af69dbc](https://github.com/renesenses/tune-server-rust/commit/af69dbc7e94cf185cad1c3dce16e85e6a9a7f5b8))
## [2.0.0-alpha.5] — 2026-05-27

### Bug fixes

- Metadata completeness NaN% — return albums_without_* fields for web client ([770594a](https://github.com/renesenses/tune-server-rust/commit/770594ab72edb1838a0bef9c6dc7732e84d013c2))
- DSF/DFF fallback — parse header when lofty fails + DSD quality tier ([1f91fb4](https://github.com/renesenses/tune-server-rust/commit/1f91fb4ab7287e266b0ba34f1ea6e1bd01f37a8b))
- Scanner reuses existing album artist to prevent compilation splitting ([bf3920d](https://github.com/renesenses/tune-server-rust/commit/bf3920dfe3d93d662e1c69f1e12e1d47d1d998d4))

### CI / build

- Docker amd64 only (skip ARM64 QEMU for faster builds) ([f38691d](https://github.com/renesenses/tune-server-rust/commit/f38691d0e957c578c558c7f0d89411d9c7017274))

### Features

- Feat: all 52 plugins ported — ListenBrainz, SoundCloud, Bandcamp, Archive.org,
Discogs, Setlist.fm, Home Assistant, Hue, tagger, kiosk, widget, mediasync,
CD-rip, SACD-rip, HQPlayer, room-calibration, visualizer, GraphQL, EQ pro,
Siri, Last.fm social, MQA, Roon Bridge, Qobuz/Tidal Connect, Shazam.

537 routes, 55 modules, 372 tests, 38.8K LOC. ([1dfd2a6](https://github.com/renesenses/tune-server-rust/commit/1dfd2a6e2e64cbe2e130a1b982052a81603daf3e))
- Feat: UPnP MediaServer, Event Bus, ICY radio, JWT auth, offline cache,
smart AI playlists, home dashboard, onboarding wizard, i18n multi-langue

UPnP: ContentDirectory with DIDL-Lite XML, SSDP advertiser, browse by
  artist/album/genre/playlist/radio. Full SOAP + ConnectionManager.
Event Bus: 40+ event types, dual WS subscription (playback + general).
Radio metadata: Radio France/Radio Paradise APIs + ICY fallback parser.
JWT Auth: HMAC-SHA256, Bearer/ApiKey schemes, middleware, login/token.
Offline: download streaming tracks, DB table, background download tasks.
Smart AI: mood/tempo/history/similar/discovery playlists, NLP prompt parser.
Home: personalized dashboard (continue listening, recommendations, mixes).
Onboarding: 6-step wizard (music dirs, streaming, zones, profile).
i18n: 8 locales, 100+ keys each, Accept-Language detection.

574 routes, 61 modules, 387 tests, 43.8K LOC. ([62de7b2](https://github.com/renesenses/tune-server-rust/commit/62de7b23fe18b8015e80eca9b60c3d4c42d69131))
- Radio metadata live polling + scanner/zone/streaming/device event emission ([13fb402](https://github.com/renesenses/tune-server-rust/commit/13fb4022e659c239ca59f0ea29cb1061558a7ce8))
- Tidal featured playlists, new releases, improved user playlists ([a93cde3](https://github.com/renesenses/tune-server-rust/commit/a93cde38ba1faf033ae7090b5a6d8ac791f6006a))

### Other

- Version 0.1.0 → 2.0.0-alpha.4 ([f870988](https://github.com/renesenses/tune-server-rust/commit/f87098811a99dddd1870c112d0b4099d4d3eb61b))
- Version 2.0.0-alpha.5 ([4b6c77a](https://github.com/renesenses/tune-server-rust/commit/4b6c77aa6bcd7e6c967f1ec5bd32605c8b3f340b))
## [2.0.0-alpha.4] — 2026-05-27

### Bug fixes

- Streaming enable/disable takes effect immediately without restart ([8f1e2ca](https://github.com/renesenses/tune-server-rust/commit/8f1e2ca3f71354f72e886f725619d74a1f292913))
- Restore streaming enabled/disabled state from DB on startup ([6c8f5a9](https://github.com/renesenses/tune-server-rust/commit/6c8f5a955bcb613a4f944d14f95083b9a5ea9970))
- Stats include zones/devices count + profile creation returns full object ([6ffefff](https://github.com/renesenses/tune-server-rust/commit/6ffefff7f2bd3ff5ca5d9b9778c454e54e4cb285))
- Position bar resets to 0 — poller recovery called play() on already-playing zones ([5d15aa6](https://github.com/renesenses/tune-server-rust/commit/5d15aa6d0af726235f5b11e44187c45a05d83492))
- Album sort (title/year/artist/added) + format display (DSD, MP3) ([997df41](https://github.com/renesenses/tune-server-rust/commit/997df411de76b05ee5e17049695fe65752848ae8))
- DLNA stop + replay — pre-stop before SetAVTransportURI + 200ms delay ([abf5b00](https://github.com/renesenses/tune-server-rust/commit/abf5b0092f257c864a80b26b2cd9f81931ce039f))
- Now playing cover art — resolve cover_path from DB/streaming API ([9c3b10f](https://github.com/renesenses/tune-server-rust/commit/9c3b10f84942bc4eba0c0e8a9a83de76dd0071ad))

### Features

- Log failed files during scan with path + error reason ([0a74fc0](https://github.com/renesenses/tune-server-rust/commit/0a74fc08501437bd74ee250b1e83ee17b5e7ee99))
- Add playlist-manager (15 routes) + zone-manager (15 routes) + admin health/zones ([7453fa7](https://github.com/renesenses/tune-server-rust/commit/7453fa7e82bf64084830f2f966def2c096c3789e))
- Last.fm scrobbling, FTS5 search, duplicates, Snapcast/Sonos/Squeezebox/Spotify Connect, WS filtering, auto-update, bug report ([c1e6f1e](https://github.com/renesenses/tune-server-rust/commit/c1e6f1efe141f15b20b3c3e549c1c7057cb27add))
- Zero 404 — all 40 missing web client routes implemented ([e6dd7b5](https://github.com/renesenses/tune-server-rust/commit/e6dd7b5ec80915d64aae00a6afee4f14c0ac4287))

### Tests

- 362 tests (+246), MIGRATION.md, README.md, bench.sh ([8aead2c](https://github.com/renesenses/tune-server-rust/commit/8aead2ceeed60987b6cabc87eaa131e0f302296d))
## [2.0.0-alpha.3] — 2026-05-27

### Bug fixes

- Normalize trailing slash in Axum router ([f699879](https://github.com/renesenses/tune-server-rust/commit/f699879c710e4a1ae685fb71d56d9b1f03868ec7))
- Normalize trailing slash in Axum router ([163ae85](https://github.com/renesenses/tune-server-rust/commit/163ae8528343ae5c77d60ce07afb7f8d24bcee93))
- Graceful port retry on startup (no panic on AddrInUse) ([2f4a5d3](https://github.com/renesenses/tune-server-rust/commit/2f4a5d39f0915a8c3c17cd98aaca9574b3214ae3))
- Play/pause/resume/stop return Zone object (web client compat) ([8760c9e](https://github.com/renesenses/tune-server-rust/commit/8760c9e24ef1bb78d448ed69c2a5d7fab1d13a81))
- Qobuz/Tidal album play — expand album tracks before calling getFileUrl ([56b0443](https://github.com/renesenses/tune-server-rust/commit/56b0443a239551d7089eb3b0c47c6db3de25d8c3))
- Tidal auto-refresh expired tokens — retry on 401 ([3912cd7](https://github.com/renesenses/tune-server-rust/commit/3912cd78fc1ac56dda1524d9266a439db779fdbf))
- Compilation albums split per track artist — use ALBUMARTIST for grouping ([689e7cc](https://github.com/renesenses/tune-server-rust/commit/689e7ccac56dce974ae855728d742d68530c83f3))
- Scan imported=0 on second run — proper mtime/size comparison + bulk query ([3a37bfe](https://github.com/renesenses/tune-server-rust/commit/3a37bfeb93f32ed0fc058d66549baae7932e097d))
- DLNA zone creation — wire up device lookup and output factory ([c060c20](https://github.com/renesenses/tune-server-rust/commit/c060c2041ca84c19b9ff6a512ea645d7c206830c))
- Trailing slash redirect uses OriginalUri to preserve /api/v1 prefix ([38f99ed](https://github.com/renesenses/tune-server-rust/commit/38f99ed822c36fe192b2adc0c9e863a972484f5c))
- Serialize streaming album/track/playlist id as source_id ([cccd253](https://github.com/renesenses/tune-server-rust/commit/cccd253b747b817ccf5af6da911ae8201fd6345d))
- Sync DLNA volume to zone state + DB, use playback volume in API response ([98b799c](https://github.com/renesenses/tune-server-rust/commit/98b799cfab2e76e02095d59de7a3b10c40a52e1b))
- Artwork cache immutable + ETag, volume sync DLNA→zone→DB ([d722355](https://github.com/renesenses/tune-server-rust/commit/d72235501e8648bf5a39f38fc4033cb14a6b23da))
- Fetch streaming track metadata when title missing from play request ([4c1cb13](https://github.com/renesenses/tune-server-rust/commit/4c1cb13c95ba85d41c173d290a184c32037f24e5))
- Rename cover_url→cover_path, image_url→image_path in streaming JSON ([7bbdf13](https://github.com/renesenses/tune-server-rust/commit/7bbdf138808429cc4e8452e8c2f83e2e367da6cc))
- Merge duplicate compilation albums + streaming field renames + track metadata fetch ([116be76](https://github.com/renesenses/tune-server-rust/commit/116be761b6323e5ae04a7ad0032d62ca5e6b571e))
- Poller syncs volume from ALL devices, recovers playing state after restart ([269b9df](https://github.com/renesenses/tune-server-rust/commit/269b9dfb8088e5e07ec6675667e2918c49f2f77b))
- Use simple serde rename for cover_path/image_path (rename(serialize=) didn't work) ([25b952f](https://github.com/renesenses/tune-server-rust/commit/25b952fdef6f09717889cc43d2fc0a63df03e455))
- Use rename(serialize=) for cover_path/image_path to not break deserialization ([1f58657](https://github.com/renesenses/tune-server-rust/commit/1f58657d3f7dc7e2279e9f254990c37afc67cbba))
- Rustls CryptoProvider panic on startup — install ring provider early ([cee18d6](https://github.com/renesenses/tune-server-rust/commit/cee18d6beebe99513f581b8d0ab7175f3625ceac))
- Rename cover_url→cover_path, image_url→image_path in struct fields (not just serde) ([7d38a75](https://github.com/renesenses/tune-server-rust/commit/7d38a75c57dcc713d443d8634b7b7fa69b763951))
- Next/previous plays from streaming_queue when local queue is empty ([c7e3bf1](https://github.com/renesenses/tune-server-rust/commit/c7e3bf1f3cf09126b97f4bd2ad822d5f50754334))
- Track Cargo.lock in git (needed for Docker build) ([e4c2791](https://github.com/renesenses/tune-server-rust/commit/e4c27915c8d35f17e37616f3aef70689afe0b197))

### CI / build

- Add workflow_dispatch trigger to release workflow ([ba46512](https://github.com/renesenses/tune-server-rust/commit/ba46512affe1272269e40ea2ff02d2b0a847faee))
- Replace dtolnay/rust-toolchain with actions-rust-lang/setup-rust-toolchain ([74ddf51](https://github.com/renesenses/tune-server-rust/commit/74ddf51726e950d6d3edb23ac0188d98d00d8446))
- Docker build from tune-server-rust repo with web client + correct ports ([36fab9d](https://github.com/renesenses/tune-server-rust/commit/36fab9d6b3e3539ac196a97b3df0ba15c9a9ce74))

### Features

- Qobuz auto-relogin on token expiry ([2badf61](https://github.com/renesenses/tune-server-rust/commit/2badf611e0adc538c6fcd27e1b4bb0e8808b129d))
- Add 25 API routes — streaming browse, profiles, zone PATCH, playlist ops ([835c824](https://github.com/renesenses/tune-server-rust/commit/835c8240aa23e9ac27883a29b55e6ec6fb25cd97))
- Add Qobuz browse, dashboard history, zone groups, M3U import, SMB mounts ([ea61296](https://github.com/renesenses/tune-server-rust/commit/ea61296760928e2bd65784a5447ae2ef36a7a997))
- Add radio favorites, alarms, artist metadata, quick-fav, buffer stats, system routes ([a6d73a8](https://github.com/renesenses/tune-server-rust/commit/a6d73a8d0d2567692e7d470f291419faf5cf33e9))
- Add playlist share/transfer/diff, profile settings/stats, tags CRUD, smart preview, streaming extras ([c658a35](https://github.com/renesenses/tune-server-rust/commit/c658a35b82bbde379197ec9a8087bb2244447f3d))
- Complete API parity — plugins, imports, credits, DJ/party, podcasts, admin ([fb60583](https://github.com/renesenses/tune-server-rust/commit/fb605834ed35d4317107d4036750104a816b0238))
- Complete all stubs — RSS parser, mDNS SMB, Roon/Plex import, DJ audio, party queue ([e39bf2d](https://github.com/renesenses/tune-server-rust/commit/e39bf2de9197e58b3c4335736d6beba01db3fa29))
- Complete Deezer streaming, Spotify token refresh, file watcher ([568f247](https://github.com/renesenses/tune-server-rust/commit/568f24745235fc0f64d2f64376be96bc25ba1902))
- ISO parity — tags complets, compilations, artwork enrichment, metadata editor, remote mode, Docker ARM64 ([f5a850b](https://github.com/renesenses/tune-server-rust/commit/f5a850b1782628758188278ac827cd91b8e14e84))
- Port missing endpoints — genres, folders, settings, history, stats ([6f59332](https://github.com/renesenses/tune-server-rust/commit/6f59332b452238e6d5042875e910b53ed5f012e1))
- Streaming album/playlist play fills queue + streaming queue storage ([8fa63c7](https://github.com/renesenses/tune-server-rust/commit/8fa63c72c1c2e37467f75346f0e92a16bfc73a39))
- AIFF and DSD playback via FFmpeg transcoding ([9c045e4](https://github.com/renesenses/tune-server-rust/commit/9c045e42dc95cf21f4f21b0db44f36cfa20242e9))
- Batch artwork enrichment via MusicBrainz Cover Art Archive ([cb0ff7a](https://github.com/renesenses/tune-server-rust/commit/cb0ff7abad6459f4e0b31ff29d9ae8ebce45b7ef))
- Add /services/tokens CRUD for Last.fm, Discogs, MusicBrainz, Genius ([c91e9c5](https://github.com/renesenses/tune-server-rust/commit/c91e9c5abd18f507529500293766c666e5437f89))
- Service tokens catalog includes streaming services (Tidal, Qobuz, Spotify, Deezer) ([d85aca3](https://github.com/renesenses/tune-server-rust/commit/d85aca3c96ef93bfbd8b9329d7c623f1624668f5))
## [0.8.0-rc1] — 2026-05-21

### Bug fixes

- Tidal user_id extracted from JWT refresh_token ([0fe2965](https://github.com/renesenses/tune-server-rust/commit/0fe2965dabb6f4ebcdf060aa2bdc0edd5220137a))
## [2.0.0-alpha.2] — 2026-05-21

### Bug fixes

- CI — install libasound2-dev for cpal, relax dead_code warnings ([bc851ff](https://github.com/renesenses/tune-server-rust/commit/bc851ff70bb72de8b3daf81a196fd86cd2afbb2c))
- CI — allow clippy style lints, add artifact retention-days ([d98d296](https://github.com/renesenses/tune-server-rust/commit/d98d2960bd63170af0317f4afd7bc705af0a5eda))
- CI — clippy warns instead of errors on style, strict on correctness ([22c18a2](https://github.com/renesenses/tune-server-rust/commit/22c18a237d311d24ed45a0b4a49cd70969731db3))
- CI — use rustls-only reqwest, no openssl dependency ([5b1d163](https://github.com/renesenses/tune-server-rust/commit/5b1d1632ce0c1dc87996bc425a5084f7cba38c8a))
- CI — make cpal optional (local-audio feature) for cross-compilation ([9b18193](https://github.com/renesenses/tune-server-rust/commit/9b18193156721d6bdbb9b7e3341c846c80e907f9))
- CI — forward local-audio feature from tune-server to tune-core ([9163b1a](https://github.com/renesenses/tune-server-rust/commit/9163b1a7e9201ec1ca5cc9b5c6b2ef95f004c3e7))
- WebSocket event format + zone enrichment for transport bar ([ea75434](https://github.com/renesenses/tune-server-rust/commit/ea7543499e990b2d263efe0876baec6bb60333ff))
- Tidal verification URL with https:// prefix ([e7c7602](https://github.com/renesenses/tune-server-rust/commit/e7c7602cfa905615372bf4f4e8bf82d534bad0b4))
- Quality labels lowercase (cd/hires/dsd/lossy) to match web client chips ([194c28a](https://github.com/renesenses/tune-server-rust/commit/194c28af66008fa5f1b05289fcd6419ec811784b))
- Album quality hi-res key with hyphen to match web client chips ([6ea7f57](https://github.com/renesenses/tune-server-rust/commit/6ea7f57c1dac38019c354d6088de2a2219e723fe))
- Tidal auth polling — /status now checks pending device code ([3b6ab43](https://github.com/renesenses/tune-server-rust/commit/3b6ab4315e8e1215d30a7d958c91eccac7680b64))
- Accept empty JSON body for streaming auth (web client compat) ([76a359d](https://github.com/renesenses/tune-server-rust/commit/76a359df0fdb0a887fc116d48bfc7057eefccafb))
- Tidal credentials (tidalapi client_id) + token exchange logging ([1f59844](https://github.com/renesenses/tune-server-rust/commit/1f59844b9b186b8674d9ba4f9059ff2a7357d6a7))
- Streaming deadlock, covers, tests + stability ([f9ad5ca](https://github.com/renesenses/tune-server-rust/commit/f9ad5ca169ebb7cbf252807e97752fa7b2f074e6))
- Volume display stuck at 50% — sync from renderer + restore from DB ([dc6ca66](https://github.com/renesenses/tune-server-rust/commit/dc6ca66c2f8fc8f153cc6a962fe64fa1f8c1fcb0))
- Chromecast compat with rust-cast API changes ([be000e8](https://github.com/renesenses/tune-server-rust/commit/be000e8e0ab7c96c11c5120b1a96a5d522f8b1f3))
- TUNE_MUSIC_DIRS accepte le format JSON array en plus du comma-separated ([c2abce0](https://github.com/renesenses/tune-server-rust/commit/c2abce00feb19fd9045b61d1b8c9801fde5db635))
- Keep mDNS scanner alive for Chromecast discovery ([b2ac2e6](https://github.com/renesenses/tune-server-rust/commit/b2ac2e64d5f946a6d619c85cee016a5bb7312926))
- Persist Qobuz app credentials across restarts ([4e7a24c](https://github.com/renesenses/tune-server-rust/commit/4e7a24c76918ea17b64db4032f45750520f8471b))
- Use latest Rust in Docker (1.87 too old for deps) ([dc7dfcb](https://github.com/renesenses/tune-server-rust/commit/dc7dfcba12764987c7619420afab8116b911852f))

### CI / build

- Retrigger after queue timeout ([769326c](https://github.com/renesenses/tune-server-rust/commit/769326cf490c80febc76d7d09180ea2cabbec61f))
- Add workflow_dispatch trigger for manual runs ([728f5d9](https://github.com/renesenses/tune-server-rust/commit/728f5d998f0398bd4d1cdc8e28d5eabe7a51dd42))
- Fix release artifact upload + allow partial builds ([8f5f7c8](https://github.com/renesenses/tune-server-rust/commit/8f5f7c8f10319aee46c99b674fedc1f8f06ec8dd))

### Features

- Sprint 2 — playback pipeline, web client compat, SSDP discovery, deploy .18 ([58cab07](https://github.com/renesenses/tune-server-rust/commit/58cab072ddeffc5bfeb10ec9e5498090c7c1b597))
- Radio playback via DLNA + auto-zone creation from SSDP ([bc98e6e](https://github.com/renesenses/tune-server-rust/commit/bc98e6e5fabf6b6975ba83938ce253219d6c1e2a))
- Tidal/streaming auth + seek/volume + disconnect route ([9b16176](https://github.com/renesenses/tune-server-rust/commit/9b16176551477bb1013993b8e5b87383185953e8))
- Track cover_path, TV filter, album covers improved ([a663af3](https://github.com/renesenses/tune-server-rust/commit/a663af39acee617691cb646e5f3072bd4d74cdd2))
- Tidal Hi-Res, play delay, volume restore, code cleanup ([766af95](https://github.com/renesenses/tune-server-rust/commit/766af95aa03bad8b6b90b36f5096d91e80284d61))
- Gapless transition detection + CI cache fix ([4275476](https://github.com/renesenses/tune-server-rust/commit/4275476b9546eceb1aba8e14f227e5b724847544))
- Tidal streaming play end-to-end + quality fallback ([5bba7a9](https://github.com/renesenses/tune-server-rust/commit/5bba7a96f8ff3329216953256192f49e58242b02))
- Qobuz streaming end-to-end + remote credential refresh ([18d26f3](https://github.com/renesenses/tune-server-rust/commit/18d26f3de403c275a502bd67719c150911b09ac2))
- Artwork proxy + streaming artist routes for web client ([9979b2a](https://github.com/renesenses/tune-server-rust/commit/9979b2af7922452f513fe5d2c5fd7a732199a884))
- Docker image renesenses/tune + CI publish workflow ([1857277](https://github.com/renesenses/tune-server-rust/commit/1857277018f4c2a7b26934c7d89f7acec1071b37))
## [2.0.0-alpha] — 2026-05-19

### Documentation

- Migration plan Python→Rust v2.0.0 (8 phases, ~20 months) ([5ca938c](https://github.com/renesenses/tune-server-rust/commit/5ca938c3340606b924f1ae1cd9eac143e902b43f))
- Update Phase 5 progress in MIGRATION.md ([74e8df4](https://github.com/renesenses/tune-server-rust/commit/74e8df4cff65c7dd184a786adbe74cb710ddca7b))
- Update Phase 6 progress — core API routes done ([63c4d88](https://github.com/renesenses/tune-server-rust/commit/63c4d88a06c0446e2207594831e02e7f17c7a8df))

### Features

- Complete metadata reader — all 30 TrackMetadata fields + credits ([d29d006](https://github.com/renesenses/tune-server-rust/commit/d29d006efb016d09804272fbd035e89b45ccb276))
- AsyncRingBuffer — tokio mpsc bounded channel for audio chunks ([c78d853](https://github.com/renesenses/tune-server-rust/commit/c78d853e054299c7f4787c9b74eac2e8e50b8b4a))
- Phase 2 audio pipeline — formats, WAV header, FFmpeg subprocess ([489c052](https://github.com/renesenses/tune-server-rust/commit/489c0522bafe95b14822e3bf933722ab0b9481ea))
- PyO3 bindings for audio pipeline — WAV header, FFmpeg, format utils ([e682c7e](https://github.com/renesenses/tune-server-rust/commit/e682c7edef013341b1dc4651772fc22efff86c93))
- RustPipeline PyO3 class — FFmpeg subprocess with chunked read ([fc9e66a](https://github.com/renesenses/tune-server-rust/commit/fc9e66ae43a3b72b50b873b2e5779e21469ad837))
- Phase 3 discovery layer — SSDP + mDNS + XML parser in Rust ([4e33d77](https://github.com/renesenses/tune-server-rust/commit/4e33d773c6353f6f7feed3f575b006c71b922b1b))
- Phase 4 scanner — parallel file walk + hash + watcher in Rust ([1b4463b](https://github.com/renesenses/tune-server-rust/commit/1b4463b95b823026967c7b093951496486218383))
- Phase 5 DB foundation — SQLite wrapper + Artist/Album/Track repos ([573cce4](https://github.com/renesenses/tune-server-rust/commit/573cce4daf4ea91032ac8e28c5a69039345641de))
- Phase 5 — PlaylistRepo, PlayQueueRepo, ZoneRepo + HTTP streamer ([4a11c33](https://github.com/renesenses/tune-server-rust/commit/4a11c33e2bc9a28a84182628ec809a4b1fa76ecc))
- Phase 5 — FTS5 search, Range requests, HTTPS proxy ([a95b7cd](https://github.com/renesenses/tune-server-rust/commit/a95b7cd1797a228b336791e5ef4d769494f994eb))
- Phase 5 — Axum REST API server with full library/zones/playlists endpoints ([f391243](https://github.com/renesenses/tune-server-rust/commit/f39124326d2664e0a2833184e8bf0f75f43f9a04))
- Phase 5 — schema migrations system + /api/v1 prefix alignment ([59f089f](https://github.com/renesenses/tune-server-rust/commit/59f089fe932d0a034fae00062a6cacdc81abaef9))
- Phase 6 — system routes, radios, search, history + new repos ([59701c6](https://github.com/renesenses/tune-server-rust/commit/59701c624826fbb6e9b20a306fc5cf06755dfe76))
- Phase 6 — playback engine, WebSocket, devices, streaming stubs ([f7a0762](https://github.com/renesenses/tune-server-rust/commit/f7a076274d72ed17a5bfab1d273692ed79357bb4))
- Phase 6 — CORS, compression, diagnostics, exports, extended library ([cc7a07b](https://github.com/renesenses/tune-server-rust/commit/cc7a07b88232190f28a18f87b330708a5fad4f38))
- Phase 6 — profiles, favorites, tags, ratings, M3U export ([6e789af](https://github.com/renesenses/tune-server-rust/commit/6e789afc6dc88692b2bed1f37a623c17a900f443))
- Phase 6 — metadata editing, smart playlists, sleep timer ([59d1c19](https://github.com/renesenses/tune-server-rust/commit/59d1c19f022fc48dcbfe276b168e166a772a9718))
- Phase 6 — backup/restore, genre tree, zone groups, M3U import ([fb8f18a](https://github.com/renesenses/tune-server-rust/commit/fb8f18abd4b827c081a190572a332d9b4db68cc8))
- Phase 6 — alarms, EQ/DSP, transfer, network, dashboard, podcasts, plugins ([f502fe9](https://github.com/renesenses/tune-server-rust/commit/f502fe90a42007b0e0586d34ff716f6f620b1124))
- Phase 7 — StreamingService + OutputTarget traits, Tidal, Qobuz, DLNA ([8ca37af](https://github.com/renesenses/tune-server-rust/commit/8ca37af3f4ab69b9bd7089fb600166308268f5ac))
- Phase 7 — wire ServiceRegistry + OutputRegistry, real streaming routes ([74e324b](https://github.com/renesenses/tune-server-rust/commit/74e324b0486c8d9fdc43c000193f24d6705fffc5))
- Phase 7 — Spotify, Deezer, YouTube connector stubs + registration ([0c85db5](https://github.com/renesenses/tune-server-rust/commit/0c85db562a30ab424005e7557803ead1509abe09))
- Phase 7 — wire DLNA outputs via device scan + OutputRegistry ([78c1e30](https://github.com/renesenses/tune-server-rust/commit/78c1e302bb03f247d9c71c1842071d130aaaf355))
- Phase 7 — PlaybackOrchestrator + federated search + DLNA pipeline ([6822c2e](https://github.com/renesenses/tune-server-rust/commit/6822c2eb76dece5da456300c16fe1f1e476be9da))
- Phase 8 prep — release profile (8.3MB binary) + MIGRATION.md update ([4000225](https://github.com/renesenses/tune-server-rust/commit/40002253bff2a7c40d3ffb72007f009024e24d0b))
- A1 — wire PlaybackOrchestrator into all playback routes ([9585df4](https://github.com/renesenses/tune-server-rust/commit/9585df4ebf6528d086db1cf817978501ab9c8923))
- A2-A4 — artwork extraction, web client serving, auto-scan, graceful shutdown ([d99d2b7](https://github.com/renesenses/tune-server-rust/commit/d99d2b7d57fe81a1f2f54cbe2379ef960f6976d8))
- Phase C — CI/CD, Docker, release workflow ([4f3953f](https://github.com/renesenses/tune-server-rust/commit/4f3953ff1f68770fea369a358650bfbe5b1ba2ac))
- Phase D — config file support, structured errors, tune.toml ([eff1a53](https://github.com/renesenses/tune-server-rust/commit/eff1a539a59e099d9f1e6e0d957da2ff5e424cb3))
- Phase B — local audio (cpal), Spotify PKCE OAuth, device enumeration ([067ef0d](https://github.com/renesenses/tune-server-rust/commit/067ef0d39b8b1ed1c4252c5b827a36e68815952a))
- Token persistence + warning cleanup (cargo fix) ([894fda0](https://github.com/renesenses/tune-server-rust/commit/894fda0b8b875cd67c9f949c9de2a039164df28e))
- D1 — 16 HTTP integration tests + lib.rs for testability ([134b078](https://github.com/renesenses/tune-server-rust/commit/134b07840147c587ae3d77135844c362b71e0191))

### Other

- Cargo workspace with tune-core, tune-pyo3, tune-server ([113d39a](https://github.com/renesenses/tune-server-rust/commit/113d39abba55d305c0b9846cd7fa0277378a16d8))
<!-- generated by git-cliff -->
