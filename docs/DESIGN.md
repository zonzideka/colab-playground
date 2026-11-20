# Colab Playground — Design

> A desktop app for the Google Colab CLI: accounts, GPU sessions with cost guards, port
> forwarding and plugins.

That sentence is the project description everywhere (`Cargo.toml`, README, About, GitHub repo);
change it in `[workspace.package]` and copy it, never paraphrase.

This file holds what the code cannot say for itself: the product principle, the scope, and the
rules every change follows. It does not describe how the code works; read the code for that
([Docs and comments](DEVELOPMENT.md#docs-and-comments)). Code that breaks a rule here is a bug in
the code. If a rule turns out to be impossible to follow, stop and report instead of silently
diverging.

## Product principle (the tie-breaker)

**Everything must match how a normal user expects it to work, and be logically sound.** When
this document is silent or two options compete, pick the one a first-time user would predict.
Concretely:

- Speak in the user's terms: "Deploy a model", "Stop and end billing", "Forward port" — never
  "run plugin llama-cpp" or CLI jargon. The plugin system is an implementation detail on the
  quick-deploy path.
- **Anything shown as running can be stopped from where it is shown**, on any saved account,
  including runtimes the app did not create. If an action is impossible, the control says why and
  offers the next best step; it is never silently missing or dead.
- Money is never ambiguous: closing a tunnel or the app does not stop billing, and the UI says
  so at that moment. Auto-stop warns ahead of time and offers "Extend".
- No required input the app can choose itself (session names are generated; forms are pre-filled
  from Defaults). Destructive or costly actions confirm once, stating the consequence.
- Long operations show what is happening now, what comes next, and can be cancelled; cancel
  always leaves no billed VM behind. Errors say what happened and what to do.
- State is honest: stale data is marked stale, estimates are marked `≈`, and mock mode carries a
  visible "Demo data" badge.
- Switching account switches what the app shows and acts on; before switching it lists what is
  affected (open tunnels close; sessions on the old account keep running under their watchdog).

## Goals and non-goals (v1)

Goals

- Windows first (the Colab CLI runs inside WSL); macOS and Linux must build and behave the same.
  Platform-specific code is isolated behind `cfg` and one `Host` abstraction.
- UI built from [Ely GPUI Components](https://github.com/ELY-Open-Source/Ely-GPUI-Components).
  Never hand-roll a widget Ely already ships. Layout glue with `div()` is fine.
- English, 简体中文, 繁體中文, 日本語, 한국어. Light, dark and follow-system appearance.
- Colab core: CLI install, accounts (login / switch / remove), sessions (create with hardware
  targeting + retry, list, stop), port forwarding, cost guards.
- Plugins: everything beyond the Colab core is a plugin. `llama-cpp` is bundled.
- A mock backend drives the whole UI without WSL, the CLI or a Google account.

Non-goals (v1)

- Installers, code signing, plugin marketplace/registry.
- Native-code or WASM plugins. Plugins are declarative (manifest + scripts that run on the VM).
- A browser/WASM build. GPUI draws to a canvas with no DOM or accessibility tree there, the
  process-spawning backend cannot exist in WASM, and it would double the toolchain. UI tests use
  GPUI's headless `TestAppContext` instead ([TESTING.md](TESTING.md)).
- ADC / gcloud authentication. Only the CLI's default OAuth2 credential is managed.

## Hard safety rules

They apply to every contributor and agent.

1. **Never run a real `colab` command that touches the account or a runtime** during development
   or tests: nothing that authenticates, lists, creates, stops, executes or changes credentials
   (`sessions` prunes local state; most subcommands trigger an update check). The only real
   invocations allowed are `colab help [CMD]` and `colab version`, which do neither. The CLI is
   open source (github.com/googlecolab/google-colab-cli, installed version 0.8.0): read it
   instead of experimenting. Do not use the `colab-operator` skill to try things.
2. Debug builds default to the **mock** backend. The real backend needs `--real` (or
   `COLAB_PLAYGROUND_BACKEND=real`). Release builds default to real; `--mock` forces mock.
   The app on the real backend runs real `colab` commands, so an agent starts it there only when
   asked to.
3. No test may spawn `wsl.exe`, except the `#[ignore]`d Windows smoke tests of the WSL bridge
   (`wsl_smoke`), which only run `printf`/`cat` round trips and never name `colab`.
4. The real backend is tested only against the **fake CLI** (`crates/core/tests/fake/`), on unix.
   The fake must never resolve to the system Python's `keyring` or a real `colab`.
5. All external side effects go through one seam: `Host` (process spawning) + `Toolchain`
   (paths of `colab` and its helper interpreter). Nothing else calls `std::process::Command`.
6. Never push to `main` directly. Every change lands through a pull request
   ([DEVELOPMENT.md](DEVELOPMENT.md#landing-a-change)).

## Invariants

Properties the code must keep. A change that breaks one is wrong even if every test passes.

- **Google tokens never leave the account helper's process.** The app sees only the helper's
  non-secret output. The CLI's active token is never cleared without a backup that can be
  restored.
- **The app never writes a secret to its files or logs**, and masks the ones it knows in what it
  shows.
- **Plugins never execute anything on the user's machine.** A plugin is a manifest plus a script
  that runs on the runtime.
- **User-controlled values reach scripts as environment variables or arguments**, never
  interpolated into script text.
- **The user's `~/.ssh/config` is never edited.**
- **Auto-stop is set on the runtime itself whenever it can be**, because a timer in the app dies
  with the app. When a stop only holds while the app is open, the app says so.
- **Unknown usage is never treated as zero.** A failed fetch marks the data stale.
- **The number of sessions the app creates is capped**, so a retry loop cannot fan out into many
  billed VMs.
- **The default of every setting lives in one place**, is editable in Settings and can be
  overridden for one action. Nothing hard-codes it where it is used.
- **No user-visible literal outside the i18n catalog**, and every locale has the same keys.
- **Colours, radii and sizes come from the theme (a size it has no token for, from
  `ui::metrics`), and UI code names every other number but 0 and 1 as a constant.**
