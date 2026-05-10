# HermesOS — Hermes Agent desktop (Tauri)

HermesOS is een **desktop-shell** rond de bestaande Hermes Agent-data (`state.db`, `config.yaml`, `.env`, `cron/jobs.json`, profielen onder `~/.hermes/profiles`). Het is **niet** hetzelfde als `hermes dashboard`: er is geen ingesloten web-`/chat`-PTY-tab; dit is een eigen Tauri UI met optionele Python-sidecar.

## Officiële documentatie

- [Hermes Agent documentatie](https://hermes-agent.nousresearch.com/docs/)
- [Windows (native) guide — early beta](https://hermes-agent.nousresearch.com/docs/user-guide/windows-native)

### Windows: data en paden

- **Persistente data** staat standaard onder `%USERPROFILE%\.hermes\` (zelfde layout als Linux). Je kunt dit overschrijven met `HERMES_HOME`.
- **Installer / tooling** kan onder `%LOCALAPPDATA%\hermes\` staan — dat is *niet* hetzelfde als je gebruikersdatahome; verwar ze niet.
- Native Windows wordt door upstream als **early beta** beschreven (subprocess/paden/console-edge cases).

### Pariteit (kort)

| Onderdeel | Desktop (HermesOS) |
|-----------|-------------------|
| Config laden/opslaan (form + ruwe YAML) | Ja; formulier gebruikt web-normalisatie (`model` string + `model_context_length`) |
| Defaults + schema voor formulier | Ja (`scripts/export_config_metadata.py` + `export_env_metadata.py` → embedded JSON) |
| Sessies, zoeken (FTS + fallback) | Ja |
| Cron beheer (`jobs.json`) | Ja (Python `cron_ipc_helper.py`) |
| Profielen (lijst schijf, mutaties) | Ja (Rust lijst + `profiles_ipc_helper.py`) |
| Skills lijst / toggle | Ja (Python helper + YAML-fallback) |
| Runtime providers (`memory.provider`, `context.engine`) naar YAML | Ja — IPC `save_plugin_providers` (web-pariteit) |
| `.env` + metadata (zoals `/api/env`) | Ja — embed uit `export_env_metadata.py`; reveal geblokkeerd |
| OAuth providers | Niet — IPC geeft duidelijke fout |
| Volledige plugin-hub / catalogus-install | Nee — gebruik CLI of web-dashboard; mutatie-API’s **reject** expliciet; `getPluginsHub` zet `desktop_hub_stub` |
| Volledige modelprovider-catalogus in UI | Nee — gebruik YAML / dashboard |
| Actief model + context/capabilities (`get_model_info`) | Ja — Python IPC zoals web; zonder repo/Python valt terug op YAML (`yaml_fallback`) |
| Gateway-status | Alleen ruwe indicator (sidecar); Windows-gateway via docs (schtasks / pythonw) |

### Zoeken in sessies (FTS)

Oude of minimale `state.db`-bestanden kunnen geen `messages_fts` hebben. HermesOS valt dan terug op een **substring-zoekactie** op `messages.content`.

### Config-formulier (web-pariteit)

`get_config` past dezelfde normalisatie toe als het web-dashboard (`model`-dict → string + top-level `model_context_length`; sleutels die met `_` beginnen worden weggefilterd). `save_config` draait het om (`_denormalize_config_from_web`) en behoudt `provider`, `base_url`, enz. vanaf schijf.

### Support: HermesOS vs `hermes dashboard` (decision tree)

Gebruik dit alleen om gebruikers **snel naar het juiste oppervlak** te sturen — geen harde productgaranties.

```text
Start: waar wil je Hermes gebruiken?
│
├─► "Ik wil de officiële browser-dashboard UI (tabs, embedded waar beschikbaar)"
│       → `hermes dashboard` (HTTP op localhost)
│       → Let op: op **native Windows** ontbreekt de `/chat` embedded PTY-tab;
│         de rest van het dashboard werkt nog wel (upstream-documentatie).
│
├─► "Ik wil een desktop-app venster (Tauri), geen browser"
│       → HermesOS-installatie
│       → Data blijft dezelfde (`HERMES_HOME`), maar: geen PTY-dashboard-chat,
│         beperktere plugin-hub/modelcatalogus — zie pariteitstabel hierboven.
│
├─► "Ik wil alleen terminal / scripting"
│       → `hermes` CLI of `hermes --tui`
│
└─► "Ik wil scheduled jobs / gateway op Windows laten draaien"
        → Zie Windows-guide: `hermes gateway install`, schtasks, pythonw, enz.
            (HermesOS beheert alleen `cron/jobs.json`; de scheduler blijft Hermes cron/CLI.)
```

### Build: config-metadata embed

Voor `get_defaults` / `get_schema` moet `src/generated/config-metadata.json` bestaan. Regenereren vanuit de repo (vereist `pip install -e ".[web]"` zodat `hermes_cli.web_server` importeerbaar is):

```bash
# Vanuit repository-root, met geactiveerde venv:
python HermesOS/scripts/export_config_metadata.py
```

### Build: env-metadata embed

Voor `get_env_vars` (zelfde shape als `GET /api/env`) moet `src/generated/env-metadata.json` bestaan:

```bash
python HermesOS/scripts/export_env_metadata.py
```

In **GitHub Actions** (`.github/workflows/release.yml`) worden **beide** exportscripts automatisch vóór `pnpm exec tauri build` gedraaid. Lokaal kun je dezelfde commando's handmatig uitvoeren na upstream-wijzigingen aan `DEFAULT_CONFIG` / `OPTIONAL_ENV_VARS`.

### Updater / releases

- In `src-tauri/tauri.conf.json` staat `bundle.createUpdaterArtifacts` op **`true`**, zodat CI **ondertekende** updater-artifacts (`.sig`) kan produceren. Zonder Minisign-private key faalt `tauri build` lokaal — voor een **snelle lokale installer-test zonder signing** kun je tijdens build overschrijven:
  ```bash
  cd HermesOS
  pnpm exec tauri build -c "{\"bundle\":{\"createUpdaterArtifacts\":false}}"
  ```
- Endpoints staan onder `plugins.updater.endpoints`. **Forks** moeten dit naar hun eigen `latest.json`-URL wijzigen.
- Release-workflow: `.github/workflows/release.yml` — triggert op tags `v*` en op **workflow_dispatch**. Zet in GitHub **Repository secrets**:
  - `TAURI_SIGNING_PRIVATE_KEY` — volledige minisign private key (inclusief comment-header).
  - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — alleen als de key met een wachtwoord is beveiligd (kan leeg blijven).
- Tag-builds kunnen `latest.json` aanvullen via `HermesOS/scripts/write_updater_latest_json.py` en dat bestand als release-asset publiceren (fork-specifieke URL).

**Fout: „Could not fetch a valid release JSON” / updatecheck faalt**

De ingebouwde endpoints proberen o.a. `…/NousResearch/hermes-agent/…/HermesOS/updater/latest.json`. **Zolang die map niet op de default branch van die repo op GitHub staat**, antwoordt de server met **404** — dan kan Tauri geen geldig release‑JSON laden. Dit is normaal voor een lokale checkout waar HermesOS nog niet upstream gepusht is.

**Oplossing (kies één):**

1. **Eigen fork** — push `HermesOS/` naar jouw GitHub-repo en zet `plugins.updater.endpoints` op één URL, bijvoorbeeld:  
   `https://raw.githubusercontent.com/<owner>/<repo>/<branch>/HermesOS/updater/latest.json`
2. **GitHub Release** — bouw met signing, upload het gegenereerde `latest.json` als release-asset en gebruik de `…/releases/latest/download/latest.json`-stijl URL (zoals in `write_updater_latest_json.py` / `build-release.yml`).
3. **Plak‑JSON moet kloppen** — `HermesOS/updater/latest.json` in de repo is een placeholder (`example.com`-download); voor echte updates moeten `url` + `signature` naar jouw ondertekende artefacten wijzen.

### Tests

```bash
cd HermesOS
pnpm run typecheck:web
pnpm test
```

```bash
cd HermesOS/src-tauri
cargo test
```

#### Frontend-testfilosofie (Vitest)

Er is **geen strikte noodzaak** voor aparte React Testing Library-tests op bijvoorbeeld `ConfigPage.tsx` als je vooral **UI-flow** wilt dekken — dat hoort eerder bij integratie- of E2E-tests (Playwright, handmatige QA).

Voor de vraag *„roept de frontend het juiste Tauri-commando aan?”* zijn **contracttests op `src/lib/api.ts`** (`HermesOS/tests/frontend/api.test.ts`) de juiste plek: gemockte `invoke`, assertions op commandonaam en payload. Zo blijft de suite snel en stabiel zonder een tweede renderer-laag te onderhouden.

### Roadmap: CI groen & lokale Windows-pariteit

Onderstaande punten zijn vastgesteld bij audits op Windows en bij vergelijking met `.github/workflows/hermes_os_audit.yml`. Werken in **fasen**; na elke fase opnieuw `pnpm lint`, `pnpm test`, `cargo fmt`, `cargo clippy --all-targets -- -D warnings` (vanuit `HermesOS` / `HermesOS/src-tauri`) draaien.

| Fase | Onderdeel | Bevinding | Gerichte actie |
|------|-----------|-----------|----------------|
| **A** | `cargo fmt --check` + clippy | Diff / Clippy-warnings in `lib.rs`, tests, enz. | **Gedaan:** rustfmt + `cargo clippy --fix --all-targets -- -D warnings`; lint-job gebruikt `clippy --all-targets`. |
| **B** | Pytest `tests/fuzz/` | `ImportError` bij relatieve import `from .ipc_fuzzer`. | **Gedaan in repo:** `test_fuzz_harness.py` voegt de fuzz-map toe aan `sys.path` en importeert `ipc_fuzzer` als top-level module. CI: `cd HermesOS && python -m pytest tests/fuzz/ -v` (let op: root-`pyproject.toml` heeft `addopts = -n auto` — bij conflicten lokaal `-n 0` of vanuit een omgeving zonder xdist). |
| **B** | `ipc_fuzzer.py --mode static` (Windows) | `OSError: [Errno 22]` / pipe-buffer; verkeerde hello-check (`type` i.p.v. JSON-RPC `method`). | **Gedaan:** chunked stdin-write, `PYTHONPATH` via `os.pathsep`, `CREATE_NO_WINDOW`, hello via `_wire_rpc_method`, surrogate-safe `random_unicode_junk`. |
| **C** | `python audit/dep_audit.py` | Lokale audit faalde zonder CI-`--ignore`-lijst. | **Gedaan:** `audit/cargo_audit_ignores.txt` + `cargo_audit_ci.py` (CI en script delen dezelfde lijst); `--ci-parity` voor gedrag als workflow `|| true`. |
| **C** | `dep_audit.py` + `pnpm` | Subproces vindt `pnpm` niet (PATH). | Op Win: audit vanuit shell met Node/pnpm op PATH; geen `shell=True` in script. |
| **D** | Workflow „Sidecar Protocol Tests” | Pad `HermesOS/tests/sidecar_protocol/` ontbrak. | **Gedaan:** `tests/sidecar_protocol/test_sidecar_jsonrpc_smoke.py` (mock sidecar, JSON-RPC hello/ready + `exit`). |
| **D** | Windows footgun-check | Verkeerd pad `../HermesOS/src-tauri/...` bij `working-directory: HermesOS`. | **Gedaan in repo:** open `src-tauri/src/lib.rs` met `encoding='utf-8'`. |

### Institutioneel: bekende verschillen met web-dashboard

HermesOS deelt data (`HERMES_HOME`) met CLI/TUI/dashboard, maar niet elke HTTP-/hub-functie is opnieuw gebouwd in IPC. Overzicht voor support en compliance-review:

| Onderwerp | Toelichting |
|-----------|-------------|
| Plugin-hub UI | Zelfde schermen als dashboard; **`getPluginsHub`** levert een synthetische lege hub met **`desktop_hub_stub: true`**. Lijst leeg ≠ geen plugins op schijf — verifieer met `hermes plugins list`. |
| Plugin-mutaties (`install` / enable / disable / update / remove / rescan / visibility) | **`Promise.reject`** — niet via IPC; gebruik CLI of web-dashboard. |
| `savePluginProviders` / `memory.provider` + `context.engine` | **Ja** — Tauri `save_plugin_providers`, zelfde velden als `PUT /api/dashboard/plugin-providers`. |
| `get_model_info` / modelcatalogus | **Doorgaans ja:** IPC-helper (`model_info_ipc_helper.py`) volgt `GET /api/model/info` (`load_config`, `get_model_context_length`, `get_model_capabilities`). Zonder werkende Python/repo-fallback: YAML alleen (`resolution_source: yaml_fallback`). |
| Structured `/api/env` | **Ja:** embedded `env-metadata.json` + `.env`; zelfde velden als web (`is_set`, `redacted_value`, `description`, `url`, `category`, …). |
| FTS-zoekterm sessies | **Ja:** `MATCH`-sanitization gelijk aan SessionDB (`_sanitize_fts5_query`); substring-fallback blijft voor geen treffers / geen FTS. |
| YAML round-trip | Opslaan via `serde_yaml` kan opmaak/comments herschrijven (zelfde klasse als veel tools). |
| Skills-paden | Zonder Python-sidecar vooral `HERMES_HOME/skills`; geen volledige `_find_all_skills`-pariteit (repo + externe dirs). |
| Updater-URL’s | Standaard NousResearch-endpoints in `tauri.conf.json` — forks moeten eigen `latest.json`-URL zetten. |
| `gateway_running` in status | Gekoppeld aan sidecar-indicator, niet aan aparte Windows schtasks/health-check van Hermes gateway. |

