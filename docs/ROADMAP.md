# Roadmap

Colab Playground's core is deliberately small: **Colab accounts, starting and stopping runtimes,
and switching between them**, with the cost guards and port forwarding that make that safe and
useful. Everything else is a plugin. This file records what is planned but not built, so the
ideas and the reasoning are not lost.

## Recorded for later

### Keeping data that must survive the runtime

A stopped runtime is wiped, and the guards stop runtimes on their own. A session could carry a
**keep plan**: VM paths plus one or more targets. Plugins would declare the paths that matter
(`[[keep]] path = "/content/outputs"`); the user picks the targets.

| Target | Runs where | Survives an app-closed stop | Notes |
| --- | --- | --- | --- |
| Local folder | App pulls (tar on the VM, `colab download`) | No | Small/medium files. |
| Hugging Face repo | VM pushes (`hf upload`) | Yes | Fastest for checkpoints and trained models. |
| Google Drive | VM copies into the mount | Yes | Needs browser consent per VM. |

- Triggers: manual "Save now"; before every app-initiated stop; optionally every N minutes; and
  VM-side during the watchdog's wind-down.
- Drive: `colab drivemount` prints a consent URL and waits for Enter, so the app can drive it
  like login. Mounting exposes the whole Drive to code on the VM, third-party plugins included:
  off by default, per session, with a warning that says exactly that.
- Tokens: the CLI already passes values to the VM with `colab exec --env KEY=VALUE` (in memory,
  not on disk); that is how a Hugging Face token reaches a plugin. Colab's notebook "Secrets"
  (`google.colab.userdata`) are not exposed by the CLI.
- v1 ships only the hook points: the watchdog's wind-down phase, `[[keep]]` and `needs` in the
  plugin manifest, secret slots in the keyring vault, and `StopOptions` on the stop path, where a
  pre-stop step will be added.

### Plugins worth writing

- Model training / fine-tuning that pushes checkpoints to a Hugging Face repo.
- Image-generation UIs, notebooks-as-jobs, dataset preparation.
- Other storage targets (S3, GCS) for the keep plan.

### Plugin runs

- Recover a plugin run's live view (stages, log, endpoint cards) after the app restarts. Today a
  script that was already started keeps running on the runtime, and the runtime keeps billing and
  still shows in Sessions (where it can be stopped or a port forwarded by hand), but the run view
  is gone. A reattach
  could read the run directory on the VM (`status.json`, `run.log`).

### The Colab API as a backend

Colab has an official REST API and a hosted MCP endpoint
([announcement](https://medium.com/google-colab/the-google-colab-api-and-mcp-endpoint-for-developers-and-agents-65cfef497056)).
A backend built on the API would send HTTPS requests itself instead of starting `colab`: a third
implementation of the `Backend` trait beside the CLI one and the mock.

- **What the API offers** (from the announcement): create, get, list and delete runtimes; list the
  machine types the subscription allows and read the subscription tier; each runtime returns the
  credentials of its Jupyter server; OAuth 2.0.
- **What it would improve:** no WSL on Windows and no CLI to install. The app would keep one token
  per account, so switching accounts would no longer change the `colab` sign-in of the whole
  machine. Hardware an account is not entitled to would be known before a start, not after a
  failed one.
- **To find out first:** the announcement mentions neither the compute-unit balance and burn rate
  (what the cost guards read) nor SSH (what port forwarding uses). Running code would mean
  speaking the Jupyter protocol to the runtime; `execute_code` exists only on the MCP endpoint.
- **Sign-in cost:** the API must be enabled in a Google Cloud project that has an OAuth client.
  Either every user creates their own, or the app ships a client that Google has verified.

### Platform and distribution

- **Optional installers** next to the portable files, for people who prefer one: an MSI or MSIX for
  Windows, a DMG for macOS, a `.deb`, an AppImage or a Flatpak for Linux. The portable files stay
  ([RELEASING.md](RELEASING.md#the-downloads)). Each format needs its own step in `package.yml`
  (for example WiX for an MSI, `hdiutil` for a DMG) and a decision on what uninstalling does with
  the settings and data.
- **Code signing and notarisation** are deliberately not planned for now, so Windows SmartScreen and
  macOS Gatekeeper ask users to confirm the first launch ([USAGE.md](USAGE.md#install-a-release)).
  Windows would need an Authenticode certificate or a signing service and a `signtool` step; macOS
  needs a paid Apple Developer ID, signing with the hardened runtime, notarisation with `notarytool`
  and stapling. The keys would live in Actions secrets.
- Downloads for more targets: Windows and Linux on Arm64, macOS on Intel. Linux desktop integration
  (a `.desktop` file and icon in the archive).
- A plugin index (discover and update plugins from inside the app).
- Native-Windows CLI support (without WSL), ADC / gcloud authentication.
- Operating sessions on a non-active account without switching (a per-operation account lock,
  as `colab-account-changer`'s pool does).
