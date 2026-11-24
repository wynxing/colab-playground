# Writing a Colab Playground plugin

A plugin turns a script into a one-click action on a Colab runtime: the app
shows a form generated from the manifest, provisions (or reuses) a session,
uploads the script, starts it detached on the VM, renders its progress, and
forwards the ports it declares. The bundled `plugins/llama-cpp/` and
`plugins/comfyui/` are complete examples.

**A plugin never runs anything on the user's machine.** The host only reads the
manifest and uploads the script; the script runs on the user's paid Colab
runtime.

## Layout

```
my-plugin/
  plugin.toml     manifest (at most 64 KiB)
  worker.py       the script named in [run] (.py or .sh, at most 512 KiB)
```

Only `plugin.toml` and the script are installed; other files are ignored.
Symlinks are rejected and the script must be inside the directory.

## Manifest (`api = 1`)

Unknown keys are rejected, and so is an `api` other than `1`.

```toml
api = 1                    # required, must be 1
id = "my-plugin"           # [a-z0-9][a-z0-9-]*, max 48; also the install directory name
version = "0.1.0"          # [A-Za-z0-9.+-], max 32
needs = ["hf_token"]       # optional host capabilities (see below)

[name]                     # required; `en` is mandatory
en = "Run my thing"
zh-CN = "运行我的程序"
zh-TW = "執行我的程式"

[description]              # optional, same shape as [name]
en = "What it does, in the user's words."

[provision]                # defaults for a fresh session (user-editable in the form)
accelerator = "A100"       # NONE | T4 | L4 | G4 | A100 | H100 | V5E1 | V6E1
min_vram_mib = 0           # 0 = any; needs a GPU accelerator
high_mem = false

[[inputs]]
id = "repo"                # [a-z][a-z0-9_-]*, max 32
type = "hf-model"          # text | number | bool | select | secret | hf-model
required = true
default = "…"              # optional; type must match; not allowed for secrets
[inputs.label]
en = "Model repository"
[inputs.help]
en = "Shown under the field."

[[inputs]]
id = "quant"
type = "select"
options = ["auto", "Q8_0", "Q4_K_M"]   # required for select, only for select
default = "auto"                       # must be one of the options

[[inputs]]
id = "port"
type = "number"
default = 8000
min = 1024                 # bounds only for numbers
max = 65535

[run]
script = "worker.py"       # relative, no "..", ends in .py or .sh
keep_alive = false         # true for a server: the app keeps reading its status

[[stages]]                 # progress steps shown in order; ids are what status.json reports
id = "download"
[stages.label]
en = "Download the model"

[[endpoints]]              # ports the host can forward once the run is ready
id = "api"
port_input = "port"        # a number input holding the VM port …
# port = 7860              # … or a fixed port (exactly one of the two)
path = "/v1"               # base path shown to the user (default "/")
health = "/health"         # optional health probe through the tunnel
kind = "openai"            # lower-case word: openai, http, … (affects the endpoint card)
[endpoints.label]
en = "API"

[[keep]]                   # optional: VM paths worth keeping (persistence, coming later)
path = "/content/outputs"  # absolute, no ".."
[keep.label]
en = "Generated files"
```

Labels use locale keys such as `en`, `ja`, `ko`, `zh-CN`, `zh-TW`. Lookup
order: the exact locale, then the same language (`zh-TW` falls back to
`zh-CN`), then `en`. Port 8080 belongs to Colab and is refused.

### Input types

| type | form widget | value in the environment |
|---|---|---|
| `text` | text field | the text |
| `number` | number field (`min`/`max`) | `8000` (integers without a decimal point) |
| `bool` | checkbox | `1` or `0` |
| `select` | dropdown of `options` | the chosen option |
| `secret` | password field | the secret (see the security model) |
| `hf-model` | Hugging Face search + GGUF file list | `owner/name` |

## What the script receives

Inputs arrive as environment variables named `PLUGIN_INPUT_<ID>`: the id is
upper-cased and `-` becomes `_` (`api-key` → `PLUGIN_INPUT_API_KEY`). Ids that
map to the same name are rejected. Missing optional inputs are simply absent.

| Variable | Meaning |
|---|---|
| `PLUGIN_ID`, `PLUGIN_RUN_ID` | plugin id and a unique run id |
| `PLUGIN_RUN_DIR` | the run directory (`/root/.colab-playground/runs/<run id>/`), also the working directory |
| `PLUGIN_STATUS_FILE` | `$PLUGIN_RUN_DIR/status.json` |
| `PLUGIN_HEARTBEAT` | heartbeat file read by the VM watchdog |
| `PLUGIN_HF_TOKEN` | the user's saved Hugging Face token, only with `needs = ["hf_token"]` and only if one is saved |

`.py` scripts run with the VM's Python (`python -u`), `.sh` scripts with
`bash`. stdout and stderr go to `$PLUGIN_RUN_DIR/run.log`, which the app
tails (secrets the host knows are replaced by `***` before display). The app
shows the log up to its end. When a script writes faster than the app reads,
the oldest unread part is left out and a line in the shown log says so: send
heavy output, such as the progress of a download, to a file of its own in the
run directory. The script runs in its own session and process group, detached
from the kernel, so it survives dropped connections.

### Capabilities (`needs`)

| name | effect |
|---|---|
| `hf_token` | `PLUGIN_HF_TOKEN` is set when the user has saved a Hugging Face token on the Plugins page. Treat it as optional. |

Unknown capability names are rejected.

## Status protocol

Write `status.json` **atomically** (write a temporary file, then rename) as
often as useful:

```json
{
  "state": "running",
  "stage": "download",
  "progress": 0.42,
  "message": "12.1 / 29.0 GB",
  "outputs": { "model_id": "qwen3-0.6b" }
}
```

- `state`: `running`, `ready` or `failed`. Before the first write the app shows
  "starting".
- A script with `keep_alive` reports `ready` and stays running: the app keeps reading its
  status, so a server can show what it does later. The run ends when the script stops or the
  user cancels it.
- `stage`: one of the manifest's stage ids.
- `progress`: optional, 0.0 to 1.0, for the current stage.
- `message`: short, plain text (not localised; keep it informative).
- `outputs`: any JSON object; shown on the run view (e.g. the chosen model id).

The host polls about every 10 seconds with `colab exec`. `ready` ends the
wait and offers the declared endpoints as tunnels. The process may (and for
servers should) keep running after `ready`. `failed` ends the run with your
`message`. If the process exits without writing `ready` or `failed`, the run
is reported as failed.

## Heartbeat and idle shutdown

The VM watchdog enforces the session's deadline and, if the user set one, an
idle timeout. **Idle** means the heartbeat file has not changed for that long
**and** no GPU utilisation of 5 % or more was seen. Touch `$PLUGIN_HEARTBEAT`
while doing work that does not use the GPU (downloads, preprocessing) and
while serving requests. The llama.cpp worker touches it every 30 s until ready,
then whenever `/slots` reports a request in progress.

## Cancellation and stopping

- Cancelling a run sends `SIGTERM` to the script's process group. Children you
  start in their own session (such as a server started with
  `start_new_session=True`) are not in that group: install a `SIGTERM` handler
  that stops them.
- A run that provisioned its own session stops that session when cancelled, and
  by default also when it fails. Runs on an existing session leave it running.
- Stopping the session ends everything; there is no separate clean-up hook.

## Security model

- **Nothing local.** The host never executes plugin files. Installing from a Git
  URL uses the local `git` with `clone --depth 1`; the `ext::` and `file://`
  transports are disabled, symlinks are checked out as plain files, and only
  `https://`, `ssh://` and `user@host:path` URLs are accepted.
- **Confirmation.** Installing shows the manifest and states that the script
  will run on the user's paid runtime. Bundled plugins cannot be replaced by a
  user plugin with the same id; they can be disabled.
- **Secrets** (`secret` inputs, `PLUGIN_HF_TOKEN`) travel in memory to the VM,
  through `colab exec --env`. They are never written by the app itself: not to
  its files, not to its logs. The app also replaces them with `***` in the log
  tail, status messages and errors it shows. The launcher removes `PLUGIN_*`
  variables from the kernel's environment right after starting the script.
  - The Colab CLI records executed cells locally, `--env` values included, in
    `~/.config/colab-cli/history/<session>.jsonl` (inside WSL on Windows). The
    app redacts the secrets from that file immediately afterwards. This is best
    effort: the file is rewritten atomically with owner-only permissions, and a
    failure is shown as a warning in the run log without stopping the run.
  - On the VM the value is in your script's environment. Do not print it, and do
    not pass it on the command line of child processes.
- **Validation.** Manifests are validated strictly (types, ids, ports, paths),
  and so are input values (types, options, ranges, `owner/name` repo ids).
- **No privileges.** A plugin gets no access to the user's accounts, tokens or
  files. It only receives the inputs the user typed and the capabilities it
  declared.

## Testing a plugin

- Validate the manifest by installing from the folder: the confirmation dialog
  reports the first rule broken (field and reason).
- Run the script on a cheap CPU or T4 session first; `run.log` and
  `status.json` are in the run directory on the VM.
- In mock mode (`--mock`, the default for debug builds) runs are simulated stage
  by stage from your manifest. This checks the form and stage labels without a
  VM. Install from a folder there; Git URLs are not simulated.
