# Public plugin catalogue

`python3 scripts/plugin-public-catalog.py --tune-version VERSION` exports schema
version 1. The CI workflow runs the focused contracts and exports an artifact on
pull requests and every `v*` tag. A tag supplies its actual Tune build version;
a pull request uses the workspace package version with `-dev`. No plugin is
instantiated, no playback starts, and no network discovery occurs.

The existing `sdk/plugins.json` inventory determines the available source
providers. A separate narrow public allowlist limits publication. The exporter
reads the actual `TunePlugin` metadata declarations, inherits defaults directly
from `tune-core/src/plugin_sdk.rs`, and respects `catalogued`;
unsupported expressions fail instead of guessing. In-tree versions follow the
provider's implementation (Tune build version or Cargo package version). Native
providers use the version declared by their registry bridge, not the SDK ABI.
WASM versions and minimum Tune versions come from their bundled manifests.
Their initial activation is read from the actual WASM loader; the two bundled
providers are enabled by default, rather than presented as uninstalled.

`default_enabled` describes the initial activation policy, not whether a provider
is compiled into the distribution. A compiled opt-in provider still requires
activation. The current native/in-tree sources do not declare a minimum Tune
version: `min_tune_version` remains null for those entries. It must not be
replaced with an invented historical number. Explicit compatibility declarations
are still needed before claiming that metadata is complete.

The site imports the downloaded artifact using
`php artisan plugins:import-server-catalogue FILE --dry-run`, then the same command
without `--dry-run` after review. The site owns presentation, illustrations,
documentation links and commercial prices; these must not override the exported
software versions. This workflow only uploads a CI artifact. It does not publish
a release, modify a running catalogue, merge code or deploy the site.
