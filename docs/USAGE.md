# Building and running Colab Playground

How to build the app, choose its backend, try it with demo data, and find what it stores.
What the app does is in the [README](../README.md); the principles and rules behind it are in
[DESIGN.md](DESIGN.md).

## Install a release

Download the file for your computer from the
[latest release](https://github.com/scarletkc/colab-playground/releases/latest); there is no
installer. Later versions install from inside the app (see [Updates](#updates)); settings and data
stay where they are (see [Where things are stored](#where-things-are-stored)).

- **Windows x64:** `colab-playground-x86_64-pc-windows-msvc.exe` is the whole app; run it. It is
  not code-signed, so SmartScreen may say "Windows protected your PC": click **More info**, then
  **Run anyway**.
- **Linux x64:** unpack `colab-playground-x86_64-unknown-linux-gnu.tar.gz` (`tar -xzf <file>`) and
  run `colab-playground` in the folder it creates. It needs glibc 2.35 or newer (for example
  Ubuntu 22.04 or Debian 12 and later) and libraries a desktop installation already has: libxcb,
  libxkbcommon and libxkbcommon-x11.
- **macOS 11 or later on Apple silicon:** unzip `colab-playground-aarch64-apple-darwin.zip` and
  move `Colab Playground.app` to Applications (opened where it was unzipped, it cannot update
  itself). It is not notarised by Apple, so macOS refuses the first launch: try to open it once,
  then open System Settings → Privacy & Security and, under Security, click **Open Anyway**
  (offered for about an hour after the attempt) and enter your login password. After that it opens
  normally.
- `SHA256SUMS` in the same release lists the SHA-256 of each file; compare it with the output of
  `sha256sum <file>` (Linux), `shasum -a 256 <file>` (macOS) or `Get-FileHash <file>` (PowerShell).
  `SHA256SUMS.sig` is the project's signature of that list, which the app checks before it
  installs an update ([what it covers](RELEASING.md#the-update-signature)).

A download is a release build: it uses the real Colab CLI (see [Build and run](#build-and-run)), so
the real-mode requirements below apply.

## Updates

The app checks GitHub for a newer release, and installs it when you ask.

- **Automatic check:** shortly after start-up, then at most once a day while the app runs. The time
  of the last check that got an answer is kept in the settings file, so restarting does not check
  again within the day. A check that fails is tried again after an hour, or at the next start.
  Turn it off in Settings → About → "Check for updates automatically".
- **By hand:** Settings → About → "Check for updates".
- **When a newer version exists:** a notice appears once for that version, and the Settings entry
  stays marked until you have updated. "Download and restart", on the notice and in Settings →
  About, first says what a restart affects: open port forwards close, runtimes keep running and
  billing, and an auto-stop set on a runtime keeps working. It then downloads the version, checks
  it, puts it in place of the running one and restarts; the steps are shown as they happen, and
  Cancel works until the new version is being put in place. It waits while a runtime is being
  started, a deployment runs, an account is being added or the CLI is being installed. "What's
  new" opens the release page.
- **The check before installing:** the download must match `SHA256SUMS` in the release, and
  `SHA256SUMS` must carry the project's signature for that release (see
  [The update signature](RELEASING.md#the-update-signature)). If anything fails, this version stays
  as it is and keeps working (in the rare case that it cannot be put back where it was, the app
  says where it is now). A download that fails the check is not installed, and the app does not
  offer that version's download page either.
- **When the app cannot update itself** it says why and offers "Open download page" instead, where
  you download the new version as in [Install a release](#install-a-release) and replace the old
  file or app. That happens when the app cannot write to its folder (for example
  `C:\Program Files`), when macOS runs it from where it was unzipped (move it to Applications), for
  a build that is not a release download (such as `cargo run`), and when its files are not laid out
  as the download leaves them.
- **What is sent:** the check is one request for this repository's latest release. Installing
  downloads `SHA256SUMS`, `SHA256SUMS.sig` and the file for your computer from that release. Each
  request carries the app's version and nothing about you or your computer.
- On Windows the replaced program is deleted the next time the app starts.
- A check that fails is reported in Settings → About (and in a message when you started it by hand).
- Demo data never contacts GitHub: it simulates the download, the check and the installation, and
  replaces nothing.

## Feedback

- **Any time:** Settings → About → "Give feedback" opens a new issue on GitHub in your browser,
  with the app's version and operating system filled in.
- **When the app asks:** after a few runtimes or deployments, the app asks whether you enjoy it,
  with "Star on GitHub" (the project page) and "Give feedback". It asks only right after you stop
  a runtime while nothing else runs or starts, at most twice, and never again once you choose
  either link.
- **What is sent:** opening the issue form passes the app's version and operating system to GitHub
  in its link. Nothing is posted until you submit the form, and the app itself sends nothing.

## Requirements

- Rust: the version in `rust-version` under `[workspace.package]` in
  [`Cargo.toml`](../Cargo.toml), or newer.
- Linux build: `libxkbcommon-x11-dev` and `libfontconfig-dev` (Ubuntu package names). Windows and
  macOS need nothing beyond the Rust toolchain.
- Real mode only: on Windows, WSL with a Linux distribution; inside it (or locally on macOS and
  Linux) `bash`, `ssh` for port forwarding, a keyring the CLI can use, and either the Colab CLI
  (`colab`, package `google-colab-cli`) or `uv` / `python3` to install it. Installing plugins from
  a Git URL uses the `git` of the computer running the app (Git for Windows on Windows).

## Build and run

```sh
cargo run -p colab-playground              # debug build: demo data (mock backend)
cargo run -p colab-playground -- --real    # debug build against the real Colab CLI
cargo run -p colab-playground --release    # release build: real Colab CLI by default
```

- Debug builds default to the **mock** backend, release builds to the **real** one.
- `--mock` or `--real` overrides the default; without a flag, `COLAB_PLAYGROUND_BACKEND=mock` or
  `=real` does. A flag wins over the variable. Both flags at once, or any other value of the
  variable, is an error (exit code 2).
- An unknown argument, a flag without its value, or a demo flag without `--mock` is an error too
  (exit code 2, with the usage line on standard error), so a mistyped flag never starts the other
  backend. Mock mode shows a **Demo data** badge, and Settings → About → Backend names the backend
  in use.

## Demo data

The mock backend is a deterministic simulation: it never runs WSL or the CLI, never touches a
Google account, and bills nothing. Its sign-in opens a placeholder Google URL; any code is accepted.

```sh
cargo run -p colab-playground -- --mock-scenario signed-out --mock-fail cli-missing
```

- `--mock-scenario demo` (default): two accounts; on the active one an A100 session, a T4 session
  and a runtime "opened in the browser"; one runtime on the second account.
  `single`: one account, nothing running. `signed-out`: first run, no account.
- `--mock-fail a,b,…` injects failures: `cli-missing`, `install-fails`, `keyring`, `usage-fails`,
  `login-invalid-code`, `login-unverified`, `not-entitled` (A100 and H100), `no-answer` (Colab
  never answers a request for an A100: after a minute the progress says how long it has waited,
  after about ten minutes it fails, and cancelling it shows the cleanup of the runtime the request
  got anyway), `ssh-busy` (the first
  forward gets HTTP 429), `plugin-fails` (at the download stage), `watchdog-restart-fails` (setting
  an auto-stop again after cancelling it cannot put the stop back on the runtime), `runtimes-fail`
  (runtimes cannot be listed), `update-available` and `update-check-fails` (the update check),
  `update-unsupported` (this copy cannot update itself), `update-download-fails`,
  `update-unverified` (the download fails its check) and `update-install-fails` (installing an
  update; add `update-available`).
- Both accept `--flag value` or `--flag=value` and apply only when the mock is selected (add
  `--mock` to a release build). A simulated A100 has 40 GB first and 80 GB on the next attempt:
  set a minimum GPU memory of 70 GB to watch the retry. Git plugin installs are not simulated.

Demo mode still reads and writes the normal settings file; set `COLAB_PLAYGROUND_CONFIG_DIR` to
another folder to keep experiments apart.

## Where things are stored

- **Settings:** `settings.json` in `<OS config dir>/colab-playground`: `%APPDATA%\colab-playground`
  on Windows, `~/Library/Application Support/colab-playground` on macOS,
  `${XDG_CONFIG_HOME:-~/.config}/colab-playground` on Linux. `COLAB_PLAYGROUND_CONFIG_DIR` replaces
  that folder (empty means unset). An unreadable file is moved aside as `settings.json.corrupt`.
  Settings → About shows the file and opens its folder.
- **Real-mode data** (not moved by that variable): `accounts.json` (no secrets), `ledger.json`
  (sessions the app started) and user plugins in `<OS data dir>/colab-playground` (the same folder
  as the settings on Windows and macOS, `${XDG_DATA_HOME:-~/.local/share}/colab-playground` on
  Linux). Per-account CLI state files are under `~/.local/share/colab-playground/accounts/` where
  the CLI runs (inside WSL on Windows; `$XDG_DATA_HOME` is honoured).
- **Sign-ins and the Hugging Face token:** the keyring where the CLI runs (inside WSL on Windows).
  On Windows the app keeps the WSL distribution running while it is open, so a password-protected
  keyring asks to be unlocked once after WSL starts rather than at every background refresh.
- **CLI location:** Settings → "Colab CLI location" picks the WSL distribution and, if detection
  fails, the path of `colab` (its Python interpreter is optional and otherwise found from it).
- **Logs** go to standard error; `RUST_LOG` replaces the default filter
  `warn,colab_playground=info`. Release builds on Windows open no console window.

- **SSH key:** port forwarding leaves `~/.ssh/config` alone and creates `~/.ssh/id_ed25519` only
  when there is neither an ed25519 nor an ECDSA key.

## Working with several accounts

Extending, forwarding and deploying need the runtime's account to be active; stopping works on
any saved account.

## Point an agent at the app

An agent on this computer — Claude Code, an OpenBot agent, anything that speaks MCP — can read what
the app knows about your Colab sessions. It is off by default.

1. Settings → **Agents**: switch **Serve the agent endpoint** on.
2. Press **Show** beside the token, then the copy button on **Agent configuration**.
3. Paste that into the agent's MCP configuration. The address it needs is
   `http://127.0.0.1:<port>/mcp`, and the token goes in an `Authorization: Bearer …` header.
4. **Replace** makes a new token. An agent still holding the old one stops working, which is how a
   machine you no longer want reading the app is cut off.

What it can read: the runtimes and what each costs per hour, compute-unit balances, the plugin run
the app is watching (its stage, the tail of its log, its workflow, and the files it produced), the
installed plugins, the settings, the signed-in accounts and the open forwards.

What it can do: start and stop a runtime, move or cancel a runtime's auto-stop, deploy and run a
plugin, copy a run's files down to this computer, open and close a port forward, enable or install
a plugin, switch the active account, and change the settings. A call takes the same path a click
takes, so the work shows up in the window the way your own click would.

A long job does not have to be asked about again and again: `run_watch` answers as soon as the run
reports something — a new stage, ready, failed, or the runtime gone — and at the latest after about
two minutes with whatever it is doing now. A deploy that takes five minutes is therefore a handful
of short calls, and a client's own timeout is never reached.

**Anything that spends money asks you first.** Starting, stopping, extending and running a plugin
open a dialog naming the action, its target, the rate per hour where the app knows it, and the
client that asked. Allowing it carries the work out. Refusing it, closing the dialog or not
answering within two minutes fails the call, and the agent is told not to ask again — a refusal is
final. One such question stands at a time: a second call that spends money is turned down while one
waits.

The endpoint listens on this computer only, and it refuses a request whose `Origin` is not its own
window, so a page in a browser cannot reach it. The token is the only way in: anyone who has it can
read what is listed above, and it is stored in the operating system's keyring.

