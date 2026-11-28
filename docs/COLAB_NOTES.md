# Colab notes: hard-won gotchas

Lessons from deploying GGUF models on Colab GPUs with the `colab` CLI (first
deployment 2026-11-16, google-colab-cli 0.8.0), plus behaviour read from the CLI
source (github.com/googlecolab/google-colab-cli). Colab Playground's core and the
bundled `llama-cpp` plugin are built around every item here; keep them in mind
when changing either.

## Billing

- A runtime bills for as long as it **exists**, whether or not anything runs on
  it. Closing a tunnel or the app does not stop billing; only stopping the
  session (or the VM-side watchdog releasing it) does.
- Rough timings for a 29 GB model on an A100: allocation ~1 min, download
  ~2 min, load + warm-up ~2 min.

## Hardware

- Colab picks the concrete machine. An **A100 is randomly 40 GB or 80 GB** and
  cannot be requested; `--high-mem` only changes system RAM. Hence the
  provisioning loop: allocate, probe `nvidia-smi`, keep or stop and retry.
- `colab new` rejects accelerators the account is not entitled to with
  **HTTP 400** ("Backend rejected accelerator"): retrying cannot help. **HTTP
  412** ("Allocation refused") means too many sessions or a temporary capacity
  limit: retrying later may help.
- `colab new` can also get **no answer at all**. Each `POST /assign` waits 120 s
  (google-auth's default read timeout), then the CLI retries 3 times with a
  10/20/40 s backoff, re-checking the assignment by its notebook hash in
  between. After about 9.5 minutes it exits 1 with a `ReadTimeout` traceback.
  Seen on 2026-11-18 for an A100 on an account its owner reports has no
  subscription, whose `ccu-info` listed A100 as eligible; an L4 had worked
  30 s earlier. The `COLAB_CLI_ASSIGN_RETRIES`, `COLAB_CLI_RETRY_BACKOFF` and
  `COLAB_CLI_REQUEST_TIMEOUT` environment variables tune that loop.
- In 0.8.0 an unknown `--gpu` value is rejected; older releases silently fell
  back to A100. Validate locally either way.

## The CLI's state (`--config`)

- `colab sessions` (and any command that resolves a session) **prunes local
  records whose endpoint is not assigned to the account currently signed in**.
  With several accounts sharing one state file, listing under account B
  deletes A's records. Every account therefore gets its own state file, passed
  with `--config` on every call, including inside the ssh `ProxyCommand`.
- `colab new -s NAME` with a name that already exists silently overwrites the
  local record; the previous VM becomes an unnamed `[?]` orphan. Never reuse a
  name that is still listed.
- `[?]` in `colab sessions` is an assignment without a local record (opened in
  the browser, created under another state file, or a failed `new`). It still
  bills. `colab stop` cannot address it by name; it can be released through
  the CLI's API client (`Client.unassign(endpoint)`), or with a throw-away state
  file holding a placeholder record for that endpoint.
- `colab stop -s NAME` of an unknown name prints "not found" on **stdout** and
  exits **0** in `0.8.0` or **1** in post-`0.8.0`. Post-`0.8.0` also adds
  `colab stop --all` (which exits `0` even when no sessions are active).
  Confirm a stop by listing again.
- The CLI keeps exactly **one** OAuth token, in the OS keyring
  (`google-colab-cli` / `oauth_token`; SecretService over D-Bus in WSL). `0.8.0`
  had no logout; post-`0.8.0` adds `colab logout`, which clears that keyring
  slot, any legacy `token.json`, cached proxy tokens in `sessions.json`, and
  cached API capability flags in `settings.json` without stopping running VMs.
  Colab Playground's `account_helper.py` still swaps the keyring slot directly
  instead of calling `colab logout` so removing or adding an account never
  disturbs another active account or global CLI settings. A missing token makes
  *any* command start the copy-paste OAuth flow: URL on stderr, then `input()` on
  stdin. For `colab exec` that stdin is the code being executed. Watch stderr for
  "To authorize colab-cli" and kill the process. A locked keyring prints "Could
  not read the OAuth token from the OS keyring".
- A legacy `~/.config/colab-cli/token.json` is migrated into the keyring when
  the keyring slot is empty, which matters while adding an account.
- Most subcommands run a daily update check; `update`, `version`, `help`,
  `whoami`, `url`, `log`, `logout`, `pay`, `test-public-api`, `readme`,
  `skill`, and piped `ssh` do not.

## `colab exec`

- It runs **Python** in the runtime's Jupyter kernel (use `subprocess` for shell
  commands). `--timeout` is an *idle* limit (seconds without output; default
  30, 0 = none), enforced in `0.8.0` and newer via streaming polls that also
  raise `"Kernel dead/restarting before the code finished."` if the kernel dies
  mid-execution.
- In `0.8.0`, `colab exec` exited **0 even when the code raised** (traceback on
  stderr). In post-`0.8.0`, uncaught exceptions and `SyntaxError` exit **1**, and
  `SystemExit` follows CPython exit-code semantics (`sys.exit(0)` / `None` → `0`,
  `sys.exit(N)` → `N`, `sys.exit("msg")` → `1` with `msg` on stderr). Snippets
  therefore print one marker line (`__CPG_RESULT__ {json}`) on success, and
  `Cli::exec` treats non-CLI-level non-zero exits as remote code outcomes
  (`Ok` with `stderr` and `result: None`). CLI-level failures exit 1: `"appears
  to be lost (404/401)"`, `"Session '…' not found."`, `"Connection was lost."`,
  `"Kernel dead/restarting before the code finished."`, `"Timeout waiting for …"`.
- `--env K=V` is implemented by prepending `os.environ[K] = V` to the code, so:
  - values **persist in the kernel process** afterwards;
  - the CLI records every executed cell locally, prelude included, in
    `~/.config/colab-cli/history/<session>.jsonl`. That path is fixed and does
    not follow `--config`.

  Colab Playground passes secrets this way: they travel in memory to the VM.
  Immediately after such a call it redacts them from that history file (best
  effort) and removes them from the kernel's environment. The app itself never
  writes them anywhere. See the security notes in `docs/PLUGINS.md`.
- Long or flaky work must not run *inside* `exec`. Running a server with
  `%%bash` through `exec` ended in "RuntimeError: Connection was lost." with
  nothing started. Start work with `subprocess.Popen(..., start_new_session=True)`;
  the `exec` call returns in seconds and the host polls with short `exec` calls.

## `colab ssh`

- In `0.8.0` and newer, a runtime enforces a plan-based concurrent `colab ssh`
  limit (up to 5 sessions). Exceeding it returns **HTTP 429**, reporting either
  `"SSH session limit reached (HTTP 429): N of N SSH sessions in use..."` (when
  the server includes the limit) or `"Already-active SSH session (HTTP 429)"`
  (fallback when no limit is reported). `0.8.0` also supports `colab ssh -- CMD`
  one-shot execution (replacing the removed `colab console` command).
  Post-`0.8.0` adds `--force` (kills existing `sshd` sessions via the kernel
  before connecting) and appends a `--force` hint to both HTTP 429 messages.
  Background status polling over ssh once blocked the tunnel (and vice versa).
  The tunnel owns the ssh slot; everything else uses `colab exec`.
- **Cost trap:** `colab ssh --proxy-mode -s NAME` *creates* a runtime when
  `NAME` is not in its state file. A tunnel reconnecting after its session was
  stopped would silently start a new billed VM. Check that the session is still
  recorded before every (re)connect.
- The server accepts ed25519/ecdsa keys only (RSA is rejected). The CLI sends
  the first of `~/.ssh/id_ed25519.pub`, `id_ecdsa.pub`.
- The tunnel lives with the process that started it; the VM keeps running when
  it ends.

## `colab upload` / `colab download`

- Post-`0.8.0` sets a `(30, 300)` second `(connect, read)` timeout on Contents
  API requests (`upload`, `download`, `ls`, `rm`, `edit`), so a dropped
  connection fails with a `requests` timeout instead of hanging indefinitely.

## llama.cpp on the VM

1. **Do not build from source.** `cmake -DGGML_CUDA=ON` takes 10+ minutes on a
   12-core VM while the A100 bills. The releases ship
   `llama-<tag>-bin-ubuntu-cuda-12.8-x64.tar.gz` plus
   `cudart-llama-<tag>-bin-ubuntu-cuda-12.8-x64.tar.gz`; copy the cudart
   libraries next to `llama-server`. The CUDA 12.8 build works on Colab's 580
   driver (CUDA 13). (If you ever must build: `nvcc` lives in
   `/usr/local/cuda/bin`, not on the ssh `PATH`.)
2. **Silent CPU fallback.** The prebuilt binary needs `libcuda.so.1`, which
   Colab keeps in `/usr/lib64-nvidia`, outside the library path of ssh and
   background processes. llama.cpp then falls back to CPU without an error
   (`--list-devices` shows `(none)`, GPU memory stays at 0). Set
   `LD_LIBRARY_PATH=<bindir>:/usr/lib64-nvidia`, require `CUDA0` in
   `--list-devices` before starting, and check GPU memory use afterwards.
3. **Port 8080 is Colab's** (a node process listens on `*:8080`). Use 8000.
4. **The first request is slow** (CUDA kernel JIT; ~1 tok/s prompt processing
   once, then ~85 tok/s). Send a warm-up request before reporting ready.
5. **GitHub "latest" has no assets** (it points at a `v1.0.0` without
   binaries). List the newest `bNNNN` tags with `git ls-remote --tags` and try a
   few (the newest may still be uploading); fall back to a verified tag
   (`b12430`). Avoid the GitHub API: Colab's shared egress IPs hit anonymous
   rate limits.
6. `hf download` needs a recent `huggingface_hub[hf_xet]` (the upgrade warns
   about Colab's preinstalled `datasets`; harmless). A 29 GB Q8_0 downloads in
   about 2 minutes.
7. Start with `-ngl 999 -fa on --jinja -np 1`. When startup fails (usually out
   of memory), fall back KV cache `f16 → q8_0`, then MTP on → off.

## ComfyUI on the VM

1. **Do not let pip touch PyTorch.** ComfyUI's `requirements.txt` lists `torch` and
   `torchvision` without a version, so installing it replaces the CUDA build of the
   runtime with another one. Drop them from the list, then check
   `torch.cuda.is_available()` before starting the server.
2. **`libcuda.so.1` lives in `/usr/lib64-nvidia`** (see above), so the server needs
   that directory on `LD_LIBRARY_PATH` when it is started from a background process.
3. **The latent decides the node.** `EmptyLatentImage` always makes 4 channels;
   Flux works on 16 and needs `EmptySD3LatentImage`.
4. **One file is a whole checkpoint.** `CheckpointLoaderSimple` wants the model,
   the text encoders and the VAE in one file: the keys are `model.diffusion_model.*`,
   `text_encoders.*` and `vae.*`. The original `flux1-schnell.safetensors` holds the
   transformer alone (776 tensors, no text encoders, no VAE) and cannot be loaded as
   a checkpoint; the all-in-one file is
   `Comfy-Org/Flux-1-Schnell/flux1-schnell-fp8.safetensors` (1438 tensors). A
   diffusers repository (the pieces in `unet/`, `vae/`, `text_encoder/`) is not a
   checkpoint either.
5. **The web editor holds a websocket open**, so the tunnel keeps one TCP connection
   to the server port while a browser tab is open. That is what tells an interactive
   session from an abandoned one when the idle rule has to decide.

## Estimating GPU memory (ScarletKc-2.0 Q8_0 example)

ScarletKc-2.0 (Qwen3.5 architecture) has 64 layers, of which only 16 use full
attention (the rest are linear attention), so its KV cache is small: per token
16 layers × 2 (K, V) × 4 heads × 256 dims × 2 bytes = 64 KiB.

| Item | 256k ctx, f16 KV | 256k ctx, q8_0 KV |
|---|---|---|
| Q8_0 weights | 27.1 GiB | 27.1 GiB |
| mmproj + compute buffers | ~2 GiB | ~2 GiB |
| KV cache | ~16 GiB | ~8.5 GiB |
| MTP draft KV | ~1 GiB | ~0.5 GiB |
| Total | ~46 GiB: fits 80 GB, not 40 GB | ~38 GiB: tight on 40 GB |

Measured on an A100 80 GB (256k, f16 KV, MTP on): 46.8 GiB used, ~64 tok/s
generation, ~50 % MTP acceptance. The plugin's `quant = auto` uses the same
arithmetic (weights + mmproj + KV per token from the GGUF header × context +
1.5 GiB + 5 % of the weights, against 97 % of GPU memory): Q8_0 on 80 GB, Q6_K
with a q8_0 KV cache on 40 GB.

## Releasing a runtime from inside the VM

`google.colab.runtime.unassign()` is a `POST http://$TBE_RUNTIME_ADDR/unassign`.
The variable is in the kernel's environment; a process started from the kernel
inherits it (the watchdog also records it at launch). This lets a detached
watchdog end billing even if the app is gone.

## Host-side quirks

- In Windows Git Bash, `curl -d '{"content":"中文"}'` sends the local code
  page, not UTF-8. Put JSON in a UTF-8 file and use `--data-binary @file`.
- Under WSL2, a listener on `127.0.0.1` inside WSL is reachable from Windows
  `localhost` (localhost forwarding), which is what tunnels rely on.
