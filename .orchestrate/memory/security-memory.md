# Security memory

## Map

- [Contributor rules](../../AGENTS.md): protected data, credentials, registry keys and file-safety invariants.
- [Reconciliation](../../ARCHITECTURE.md#deciding-the-reconcile-table) and [file changes](../../ARCHITECTURE.md#changing-the-skills-folder).
- [Private repositories](../../docs/GUIDE.md#private-repositories): authentication behavior.

## Commands

- `git --no-pager diff -- <assigned-paths>` for the assigned trust-boundary changes.
- `scripts\dev.ps1 test -Quick -Filter TEXT` for relevant existing safety tests.
- No external scanner is configured in the profile; do not install one during a feature run.

## Learned

- Tray relaunch after an app update (reviewed 2026-11-20, uncommitted tree): `replacement::verified_tray` holds
  `tiny-skill-sync.exe` through `win::install::open(path, false)` (GENERIC_READ, `FILE_SHARE_READ` only, no link, one
  hard link) and every ancestor folder through `Directory::pin` (`FILE_SHARE_READ | FILE_SHARE_WRITE`, never DELETE).
  `win::proc::start` passes the absolute path as `lpApplicationName` (no path search), no handle inheritance, `None`
  environment (inherits), no current-folder override. `cloud()` accepts Cloud Files reparse tags, so a placeholder
  is accepted by `plain()`.
- Debug seam: `TEST_RESTART`, `CMD_TEST_RESTART*` and the `on_command` arm are all `#[cfg(debug_assertions)]`; the
  release profile and the `size-*` profiles do not turn debug assertions on, `.cargo\config.toml` only sets crt-static,
  and `release.yml` builds with `scripts\build.ps1` (`cargo build --release --bins`). A UTF-8/UTF-16 byte search of
  `target\release\*.exe` for "Debug test" and "TEST_RESTART" finds nothing.
- `--restarted` and `--restarted-open` give a caller no privilege: they wait at most 120 s for the single-instance
  mutex, set a status line, and reopen the window (a caller can already post `CMD_SETTINGS`).
- PowerShell 5.1 has no `[Text.Encoding]::Latin1`; use `GetEncoding(28591)` for byte searches.
- Copilot plugins (issue #7 F, reviewed 2026-11-20, uncommitted tree on 893cebe), `src\win\copilot.rs`:
  - `launch` → `proc::capture`: job via `PROC_THREAD_ATTRIBUTE_JOB_LIST` with KILL_ON_JOB_CLOSE, a 3-handle
    inherit list, 64 KiB per stream, and 30/180/60 s timeouts with cancel.
  - `proc::create` passes `lpCurrentDirectory = null`, so the Copilot child inherits the caller's current folder.
  - Its environment is inherited plus `NO_COLOR=1`. The app sets no token environment variable (`src` has no
    `set_var`).
  - `find_exe` order: `TSS_COPILOT_CLI` (an absolute `.exe`, no fallback), then
    `%LOCALAPPDATA%\GitHubCopilotCLI\copilot.exe`, then absolute PATH entries that pass `check_root` and do not
    name the current folder.
  - `isolated` runs only when a `TSS_HOME`/`TSS_SKILLS_DIR`/`TSS_TEST_ROOT`/`TSS_AGENTS_DIR`/`TSS_INSTRUCTIONS_DIR`
    variable is set. It compares names only against the environment's `USERPROFILE\.copilot` and
    `LOCALAPPDATA\copilot`.
  - It misses UNC admin shares, 8.3 names, junctions and a redirected `USERPROFILE`. The e2e `Assert-LiveCopilotEnv`
    adds an inside-scenario check and a reparse-point check.
  - `first_error` masks only `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`/`github_pat_` lines. state.json stores no error text,
    but stores self-declared publisher and version strings with no length bound.
- Plugin names: `normal_name` is `[a-z0-9]` runs joined by single `-`, at most 64 bytes.
  - `change()` re-checks both halves of `name@market`.
  - `add_market` requires a `RepoSpec` round trip and `check_branch`, and refuses `#` and `HEAD`.
  - `proc::command_line` refuses `"`. Branches may still contain `& | < > % ! ( ) ; $` and the backtick.
- Origin proof `NativeMarket::confirms` requires all of: a non-default market, the exact name, a listing that
  shows `GitHub: repo`, and settings.json `extraKnownMarketplaces.<name>.source` {github, repo, `ref` == branch}.
  Unknown origin fields are ignored.
- Plugin preview: `printable` escapes control and bidi characters but not zero-width/format characters. The
  publisher shown is self-declared: `author.name`, else the marketplace `owner.name`.
- Plugin tests use `FakePluginManager` (`World::plugin_engine`, the cli `TestHost`). `scripts\test.ps1` sets
  `TSS_TEST_ROOT`, so a real-manager launch in tests is refused unless COPILOT_HOME and COPILOT_CACHE_HOME are set.

- Issue #8 lanes (reviewed 2026-11-21, uncommitted tree; static only):
  - One branch-root listing per source (`engine\root.rs`). Truncation and a missing folder fail the source before any lane runs.
  - `tree::lane_discovery` treats filtered/bundled items as `protected` and colliding/rejected items as `rejected`; both are evidence.
  - Skills match those names with `reconcile::listed_as` (fold or `same_win_name`); leaves use `same_win_name`.
  - Type-off lanes are absent from `cfg.repos()` but listed in `known_lanes`, so they are never orphans.
  - Plugin capabilities = manifest keys + convention files (`catalog::HOOK/MCP/LSP_PATHS`) + default
    agents/skills/commands folders (`inventory.rs` ~452). `catalog::Capabilities` is now read only by tests.
  - Manifests: `root::check_budget` (size total + count <= MAX_SKILL_FILES) before downloads, and
    `read_manifests` checks cancel + budget per file; the preview uses `DEFAULT_MAX_DOWNLOAD`.
  - A bare source with plugins resolves the default branch and lists it by name. A poll is a conditional request at HEAD.
  - `state.json` format 3 (`needs_format_3`: `lane_polls` or plugin roots). `lane_polls` is scheduling only:
    filtered by `matches(lane)` and `!has_source_id`, pruned at load and sync, and never read for ownership.
  - Lane IDs are derived from config only; `fresh_lane_ids` renames a lane that is turned on when its ID has foreign records.
  - `seen::update` re-reads the file before merging and never overwrites a newer version. The file is display-only.
  - File lanes cache confirmation in `RootListing::confirmed` (repo/branch/path/pin/tree ID), set only after success.
  - `settings_write_lock_within`: CREATE_NEW + OPEN_REPARSE_POINT + delete-on-close; retry only waits.

- App-update token fallback (reviewed 2026-11-22, uncommitted tree):
  - `release::metadata_at` always sends anonymously first. It calls `TokenProvider::token()` only when
    `response_error(..).limited` holds (429, or 403 with remaining 0, retry-after, or reset without remaining) and
    more than `TOKEN_LOOKUP_BUDGET` (GH_TIMEOUT + 2 s) remains. It retries the same `MetadataUrl` once. Any retry
    failure returns the original anonymous `ReleaseError`.
  - `MetadataUrl` and `DownloadUrl` have private fields, so only `release` builds them.
    `ReleaseRequest::Download` carries no bearer.
  - `release_hash::request_headers` refuses a bearer unless the request is secure, the host is `api.github.com`,
    the port is 443 and the token is non-empty `is_ascii_graphic`. It builds the UTF-16 header at exact capacity and
    wipes it after a synchronous `WinHttpSendRequest` (`WinHttpOpen` flags 0).
  - The release transport sets NO_PROXY, DISABLE_REDIRECTS|AUTHENTICATION|COOKIES, AUTOLOGON HIGH, a null client
    cert and no global pooling, and never captures error bodies. `http_error` maps every error to fixed text.
  - `Updates::system()` hard-codes `Official::new(SystemTokens)`, not the host's `tokens()`.
  - `TSS_NO_APP_UPDATES` stops `Updates::run` before any network. test.ps1, the common.ps1 sandbox and e2e set it.
  - The skill engine was already anonymous-first with a token retry (`engine.rs` `request_tree`). The skill
    `http.rs` checks only CR/LF in a token and allows https or loopback.
  - Token copies that were never wiped (pre-existing): `cred::decode_blob` intermediates, the `proc::read_some`
    buffer and stdout Vec, and WinHTTP's internal copy of the header.

## Baselines

- 2026-11-20 relaunch review: `cargo test --lib only_a_tray_proven` and `cargo test --lib restarted` pass (debug);
  `cargo build --release --bins` was up to date. No scanner is in the profile; none was run. The e2e, soak and
  `relaunch-smoke.ps1` were not run by security. Source inspection is not scanner coverage.
- 2026-11-20 plugins review (issue #7 F): each of these exited 0.
  - `scripts\dev.ps1 test -Quick -Filter plugin`: lib 23, app 1, cli 4, engine_plugins 16 passed.
  - `-Filter isolat`: 4 passed.
  - `-Filter proc`: 12 passed, including the grandchild kill test.
  - The `win::copilot` lookup and `first_error` tests match neither filter (use `-Filter copilot`).
  - No live e2e, real Copilot CLI run or advisory scan.
- 2026-11-22 token fallback review: `scripts\dev.ps1 test -Quick -Filter release_hash` passed 2 lib tests (exit 0).
  The `release::tests` retry tests were inspected but not run by security.

## Open proposals

None.
