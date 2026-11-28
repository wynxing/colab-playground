# Writing a Colab Playground plugin

A plugin turns a script into a one-click action on a Colab runtime: the app
shows a form generated from the manifest, provisions (or reuses) a session,
uploads the script, starts it detached on the VM, renders its progress, and
forwards the ports it declares. The bundled `plugins/llama-cpp/` and
`plugins/comfyui/` are complete examples, and so are the four workload plugins
below.

**A plugin never runs anything on the user's machine.** The host only reads the
manifest and uploads the script; the script runs on the user's paid Colab
runtime.

## Layout

```
my-plugin/
  plugin.toml          manifest (at most 64 KiB)
  worker.py            the script named in [run] (.py or .sh, at most 512 KiB)
  requirements.txt     optional static file listed under [[assets]]
  workflows/demo.json  another static file (relative path, no "..")
```

`plugin.toml`, the script, and every `[[assets]]` file are installed. Anything
else is ignored. Symlinks are rejected. The script and each asset must be a
regular file inside the directory. An asset is at most 512 KiB, and a plugin
has at most 16. Nothing in the directory is executed on this computer: assets
are uploaded to the run directory as `assets/<path>`.

## Manifest (`api = 1` or `api = 2`)

Unknown keys are rejected. `api` must be `1` or `2`. The fields below are the
same for both: `api = 2` only marks a manifest that uses the newer ones. An
`api = 1` manifest may use them too, so older plugins keep working.

```toml
api = 1                    # required, 1 or 2
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
type = "hf-model"          # text | textarea | number | bool | select | secret | hf-model | file | dir | notebook
required = true
default = "…"              # optional; type must match; not allowed for secret, file, dir, or notebook
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
keep_alive = false         # true for a server: the app keeps reading its status. Not with mode = "job"
mode = "service"           # service (default) or job
keep_on_failure = false    # job only: leave a runtime this run opened when the script fails

[[secrets]]                # named keyring slots, optional
id = "civitai_token"       # [a-z][a-z0-9_]*, max 32. Env PLUGIN_SECRET_CIVITAI_TOKEN
required = false           # true: the run refuses to start until a value is saved
[secrets.label]
en = "Civitai token"

[[assets]]
path = "requirements.txt"  # relative, no "..", not the script or plugin.toml

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
| `textarea` | text field that may contain line breaks | the text, unchanged |
| `file` | path of a file on this computer | absolute path on the runtime, under `$PLUGIN_RUN_DIR/inputs/<id>/` |
| `dir` | path of a folder on this computer | absolute path of the uploaded folder, `$PLUGIN_RUN_DIR/inputs/<id>/` |
| `notebook` | path of a `.ipynb` file | absolute path on the runtime, same layout as `file` |

`file`, `dir`, and `notebook` are uploaded with `colab upload` before the script
starts. The local path is only an argument of that upload. It is not written
into the environment, `meta.json`, or the script. The environment receives the
path on the runtime. Directories are expanded to regular files only: symlinks
are rejected, and the upload stops past 256 files, 64 MiB for one file, or
256 MiB in total. A notebook's name must end in `.ipynb`.

### Named secrets (`[[secrets]]`)

Each slot is stored in the keyring as `p<len>_<plugin id>_<slot id>`. `<len>`
is the plugin id's length, two digits (`train` is `05`). `-` in the plugin id
is written as `_`. The length keeps two plugins from sharing one slot:
`train-civitai` / `token` and `train` / `civitai_token` used to both become
`p_train_civitai_token`. The script receives it as
`PLUGIN_SECRET_<SLOT>` (the slot id in upper case) only when a value is saved.
A required slot with no value refuses the run before a runtime is opened. The
value stays in memory for that run and is replaced with `***` in the log and
the UI.

### Job mode (`mode = "job"`)

A job runs the script, then:

- on success, runs the Keep Plan already attached to the session (a no-op when
  there is none) and **stops a runtime this run opened**. A failed save still
  stops that runtime, and the save error is reported;
- on an existing runtime, runs that Keep Plan and leaves the runtime up;
- on failure, stops a runtime this run opened, unless `keep_on_failure` is
  true, in which case it stays up so it can be inspected;
- on cancel, stops a runtime this run opened.

`keep_alive` cannot be combined with `mode = "job"`. A service
(`mode = "service"`, the default) is unchanged: it stays up after `ready`.

### Capabilities that need a yes (`gcp_auth`, `drive`)

`needs = ["gcp_auth"]` or `needs = ["drive"]` does nothing until the run form's
switch is on. The script then receives `PLUGIN_GCP_AUTH=1` or `PLUGIN_DRIVE=1`.
These are not secrets. Saying yes does not run `colab auth` or mount Drive by
itself; mounting Drive stays the separate Keep Plan consent. A missing yes
refuses the run before a runtime is opened.

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
| `gcp_auth` | `PLUGIN_GCP_AUTH=1` only after the user allows it on the run form. |
| `drive` | `PLUGIN_DRIVE=1` only after the user allows it on the run form. |

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
  "outputs": { "model_id": "qwen3-0.6b" },
  "metrics": { "step": 3, "loss": 0.42, "tok_per_s": 12.5 }
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
- `metrics`: optional object. `step`, `loss`, and `tok_per_s` are shown on the
  run page when they are finite numbers. Any other shape is ignored and does
  not fail the poll.

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
- A service that provisioned its own session stops that session when cancelled,
  and when it fails if stop-on-failure is on (the default). A run on an
  existing session leaves it running.
- A job that provisioned its own session stops it after success, after the
  Keep Plan, and also when the script fails, unless `keep_on_failure` is true.
  A job on an existing session never stops it. If saving the Keep Plan fails,
  the runtime this run opened still stops, and the save error is returned.
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

## Bundled workload catalog

`plugins/registry.json` is compiled into the app and shown on the Plugins page.
It is a static index of the short jobs and servers that ship with this build.
The app does not download it, and it does not fetch a Git URL to refresh it.
Each row has the plugin id, the pinned version, `service` or `job`, the default
accelerator and a minimum GPU memory. An installed copy whose version differs
from the pin is marked. A plugin that is not installed is not an update.

This file is not a plugin manifest. The manifest parser does not read it.
Unknown keys in the index are rejected, ids must be unique, and a memory floor
above zero requires a GPU accelerator.

| id | mode | behavior |
| --- | --- | --- |
| `vllm` | service | OpenAI-compatible vLLM server on a GPU. It stays up after it is ready. The port input defaults to 8001. |
| `whisper` | job | Transcribe one uploaded audio file. Then the Keep Plan runs and a runtime this job opened stops. |
| `llm-train` | job | Unsloth or Axolotl, with a short step budget. Same stop rule. Checkpoints are the Keep Plan path. |
| `civitai` | job | Download one model version on a CPU runtime. The token is a named secret slot. It is masked and sent as a header, not in the address. |

A job cannot set `keep_alive`. Secrets stay in memory. Each of these workers
exits before any install or download when `PLUGIN_RUN_DIR` is unset, so the
script does not run on the computer where the app is open.

## Testing a plugin

- Validate the manifest by installing from the folder: the confirmation dialog
  reports the first rule broken (field and reason).
- Run the script on a cheap CPU or T4 session first; `run.log` and
  `status.json` are in the run directory on the VM.
- In mock mode (`--mock`, the default for debug builds) runs are simulated stage
  by stage from your manifest. This checks the form and stage labels without a
  VM. Install from a folder there; Git URLs are not simulated.
