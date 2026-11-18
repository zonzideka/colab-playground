<div align="center">

<img src="assets/icon/icon.svg" alt="Colab Playground icon: a light lightning bolt on a dark tile" width="128" />

# Colab Playground

**A desktop app for the Google Colab CLI: accounts, GPU sessions with cost guards, port forwarding and plugins.**

[Download](https://github.com/zonzideka/colab-playground/releases/latest) | [Usage](docs/USAGE.md) | [Plugins](docs/PLUGINS.md) | [Roadmap](docs/ROADMAP.md)

[![Latest release](https://img.shields.io/github/v/release/zonzideka/colab-playground?logo=github)](https://github.com/zonzideka/colab-playground/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/zonzideka/colab-playground/ci.yml?branch=main&label=CI&logo=githubactions&logoColor=white)](https://github.com/zonzideka/colab-playground/actions/workflows/ci.yml)
[![Built with Rust and GPUI](https://img.shields.io/badge/built%20with-Rust%20%2B%20GPUI-B7410E?logo=rust&logoColor=white)](https://www.gpui.rs/)
[![License](https://img.shields.io/github/license/zonzideka/colab-playground)](LICENSE)

[![Download for Windows x64](https://img.shields.io/badge/Download-Windows%20x64-0078D4?style=for-the-badge&logo=data:image/svg%2bxml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCIgZmlsbD0id2hpdGUiPjxwYXRoIGQ9Ik0wIDBoMTF2MTFIMHpNMTMgMGgxMXYxMUgxM3pNMCAxM2gxMXYxMUgwek0xMyAxM2gxMXYxMUgxM3oiLz48L3N2Zz4=)](https://github.com/zonzideka/colab-playground/releases/latest)
[![Download for macOS on Apple silicon](https://img.shields.io/badge/Download-macOS%20Apple%20silicon-000000?style=for-the-badge&logo=apple&logoColor=white)](https://github.com/zonzideka/colab-playground/releases/latest)
[![Download for Linux x64](https://img.shields.io/badge/Download-Linux%20x64-FCC624?style=for-the-badge&logo=linux&logoColor=white)](https://github.com/zonzideka/colab-playground/releases/latest)

Which file to pick, what the first launch asks and how updates work: [Install a release](docs/USAGE.md#install-a-release).

<img src="docs/images/overview.png" alt="The Overview page with demo data: compute unit balance, burn rate, time left and runtimes running, a warning about a runtime the app did not start, and the Deploy a GGUF model card" width="800" />

</div>

**What it is:** a front end for the Google Colab CLI (`colab`, package `google-colab-cli`). It keeps
several Google accounts, starts and stops Colab runtimes under cost limits, forwards ports from a
runtime to your computer and runs plugins on the runtime. Written in Rust with GPUI and
[Ely GPUI Components](https://github.com/ELY-Open-Source/Ely-GPUI-Components).

**What it is not:** a notebook editor or a replacement for the CLI; everything goes through
`colab`. Model deployment is not part of the core: it is a plugin (the bundled llama.cpp one).

## Features

- **Guided setup.** A checklist finds the Colab CLI or installs it for you, then signs you in.
- **Several Google accounts.** Save them, switch between them, and see every account's balance
  without switching. Before a switch the app lists what it affects.
- **Sessions on the hardware you want.** Pick CPU, a GPU or a TPU, high RAM and a minimum GPU
  memory. Colab hands out an A100 with 40 GB or 80 GB at random; with a minimum set, the app gives
  back a smaller GPU and tries again. Runtimes opened in the browser are listed too, and can be
  stopped.
- **Cost guards.** Auto-stop and idle stop run on the runtime itself, so they apply even when the
  app is closed. A spending cap and a balance floor are checked while the app runs. Balance, burn
  rate and time left are shown live.
- **Port forwarding.** Connect a runtime port to `127.0.0.1` on your computer.
- **Plugins.** A manifest plus one script that runs only on the Colab runtime, never on your
  computer. Bundled: "Deploy a GGUF model" serves a Hugging Face GGUF model with llama.cpp
  (OpenAI-compatible API plus web chat) and picks the best quantisation that fits the GPU it gets.
  Write your own with [docs/PLUGINS.md](docs/PLUGINS.md).
- **Languages and theme.** 简体中文, 繁體中文, 日本語, 한국어 and English; light, dark or follow the
  system.

Every limit and default is editable in Settings and can be overridden for a single session or
deployment.

<p align="center">
  <img src="docs/images/sessions.png" alt="The Sessions page: an A100 and a T4 runtime with burn rate, spend so far and an auto-stop countdown, each with Stop and end billing, Extend by 2 h and Forward a port" width="49%" />
  <img src="docs/images/deploy.png" alt="The Deploy a GGUF model page: a Hugging Face repository search, the files available and the quantisation estimated for a 40 GB and an 80 GB GPU" width="49%" />
</p>

## Get it

Download the file for your computer with the buttons above, or run from source with a Rust
toolchain. On Windows the Colab CLI runs inside WSL.

```sh
cargo run -p colab-playground              # demo data: no Google account, nothing billed
cargo run -p colab-playground --release    # the real Colab CLI
```

Requirements, backend flags, demo scenarios and where settings are stored:
[docs/USAGE.md](docs/USAGE.md).

## Safety and cost

- Colab bills compute units for as long as a runtime **exists**, busy or idle. Closing a forward or
  the app does not stop billing; only a stop, an auto-stop or Colab itself does.
- Stopping wipes the runtime: files, downloaded models, anything not saved elsewhere. Each
  provisioning attempt that is given back is billed for the short time it ran.
- The spending cap and the balance floor only work while the app runs. Auto-stop and idle stop run
  on the runtime, and only if its watchdog started (the app says so when it could not).
- The CLI has one signed-in account for the whole machine. Switching in the app switches it for
  every other use of `colab` too, and runtimes left on the old account keep billing until their
  auto-stop, if they have one.
- Runtimes the app did not start (for example opened in the browser) get no auto-stop from it;
  they are listed so you can stop them.
- A plugin runs its script on your paid runtime. Install only plugins you trust.
- Unknown usage is never shown as zero: it is marked unknown or outdated, and the guards report
  that they cannot check a limit while usage is unknown.

## Documentation

- [docs/USAGE.md](docs/USAGE.md): install a release, build and run, demo data, where things are
  stored.
- [docs/PLUGINS.md](docs/PLUGINS.md): writing a plugin (manifest, environment, status, security).
- [docs/DESIGN.md](docs/DESIGN.md): the product principle, the scope and the rules every change
  follows.
- [docs/TESTING.md](docs/TESTING.md): automated tests and the manual checklist for real mode.
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md): how docs and comments are written, landing a change
  through a pull request, and review bot comments.
- [docs/COLAB_NOTES.md](docs/COLAB_NOTES.md): Colab and CLI behaviour the app is built around.
- [docs/ROADMAP.md](docs/ROADMAP.md): planned but not built.
- [docs/RELEASING.md](docs/RELEASING.md): how a release is made and what each download contains.

## License

MIT, © 2026 ScarletKc. See [LICENSE](LICENSE). Icons are from [Lucide](https://lucide.dev) (ISC
licence, see [assets/icon/LICENSE-lucide.txt](assets/icon/LICENSE-lucide.txt)).
