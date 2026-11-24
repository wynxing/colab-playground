#!/usr/bin/env python3
"""ComfyUI deployment worker for Colab Playground (runs on the Colab VM).

Started detached by the host. Steps: detect the GPU -> install ComfyUI
(keeping the CUDA build of PyTorch Colab already has) -> download the
checkpoint chosen by the preset, or a single-file .safetensors from a custom
repository -> start the server on 127.0.0.1 -> warm up with one real txt2img
generation -> report ``ready`` -> keep monitoring the server.

Protocol (see docs/PLUGINS.md):
  * inputs arrive as ``PLUGIN_INPUT_<ID>``; a saved Hugging Face token as
    ``PLUGIN_HF_TOKEN`` (optional);
  * progress is written atomically to ``$PLUGIN_STATUS_FILE``;
  * activity touches ``$PLUGIN_HEARTBEAT`` (read by the VM watchdog).

Lessons kept from docs/COLAB_NOTES.md:
never port 8080; detached ``Popen(start_new_session=True)``; ``libcuda.so.1``
lives in ``/usr/lib64-nvidia``, outside the default library path of a
background process; a real request before reporting ready; a GPU-memory check
against a silent CPU fallback; secrets never printed or put on child command
lines; status.json written atomically; heavy download output to a file of its
own.
"""

import ast
import json
import os
import re
import shlex
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

E = os.environ.get
RUN_DIR = E("PLUGIN_RUN_DIR", "/root/.colab-playground/runs/manual")
STATUS_FILE = E("PLUGIN_STATUS_FILE", os.path.join(RUN_DIR, "status.json"))
HEARTBEAT = E("PLUGIN_HEARTBEAT", "")
COMFY = "/content/ComfyUI"
MODELS_CKPT = os.path.join(COMFY, "models", "checkpoints")
SERVER_LOG = os.path.join(RUN_DIR, "server.log")
NVIDIA_LIB = "/usr/lib64-nvidia"  # Colab keeps libcuda.so.1 here, off the default path

PRESET = (E("PLUGIN_INPUT_PRESET", "sd15") or "sd15").strip()
REPO = E("PLUGIN_INPUT_REPO", "").strip()
PATTERN = E("PLUGIN_INPUT_PATTERN", "").strip()
PORT = int(float(E("PLUGIN_INPUT_PORT", "8188") or 8188))
EXTRA = E("PLUGIN_INPUT_EXTRA_ARGS", "")
STEPS = int(float(E("PLUGIN_INPUT_STEPS", "0") or 0))
SIZE = int(float(E("PLUGIN_INPUT_SIZE", "0") or 0))
HF_TOKEN = E("PLUGIN_HF_TOKEN", "")
SECRETS = [s for s in (HF_TOKEN,) if s]

REPO_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*/[A-Za-z0-9][A-Za-z0-9._-]*$")

# Preset -> (repository, file name, steps, cfg, size). The file name is the
# exact single-file checkpoint in the repository; the presets differ in size
# and in how many steps they need.
PRESETS = {
    "sd15": ("stable-diffusion-v1-5/stable-diffusion-v1-5", "v1-5-pruned-emaonly.safetensors", 20, 7.5, 512),
    "sdxl": ("stabilityai/stable-diffusion-xl-base-1.0", "sd_xl_base_1.0.safetensors", 30, 7.0, 1024),
    # The all-in-one Flux Schnell checkpoint: the model, the text encoders
    # and the VAE in one file, which is what CheckpointLoaderSimple wants.
    # The original black-forest-labs file holds the transformer alone, so it
    # cannot be loaded as a checkpoint.
    "flux-schnell": ("Comfy-Org/Flux-1-Schnell", "flux1-schnell-fp8.safetensors", 4, 1.0, 1024),
}
CUSTOM_DEFAULTS = (None, None, 20, 7.5, 512)  # steps, cfg, size

# What a .safetensors file carries, by the prefixes the formats use. A
# checkpoint needs all three parts; a file with the model alone is a
# diffusion model, not a checkpoint.
MODEL_PREFIXES = ("model.diffusion_model.", "model.", "double_blocks.", "single_blocks.")
CLIP_PREFIXES = ("cond_stage_model.", "conditioner.", "text_encoders.", "clip_l.", "t5xxl.")
VAE_PREFIXES = ("first_stage_model.", "vae.")

# A single-file checkpoint is a .safetensors at the repository root. Files in
# a sub-directory belong to a diffusers layout (unet/, vae/, text_encoder/),
# which ComfyUI cannot load as one checkpoint.
NOT_A_CHECKPOINT = re.compile(
    r"(lora|vae|clip|text[-_]?encoder|unet|controlnet|embedding|tokenizer|safety[-_]?checker|upscale|refiner)",
    re.I,
)

# ------------------------------------------------------------- status

_current_stage = ["detect"]
_failure = [""]
_outputs = {}


def redact(text):
    for s in SECRETS:
        if len(s) >= 4:
            text = text.replace(s, "***")
    return text


def status(state, stage=None, message="", progress=None, outputs=None):
    if stage:
        _current_stage[0] = stage
    doc = {"state": state, "stage": _current_stage[0], "message": redact(message)}
    if progress is not None:
        doc["progress"] = max(0.0, min(1.0, float(progress)))
    if outputs is not None:
        doc["outputs"] = outputs
    print("[%s] %s/%s: %s" % (time.strftime("%H:%M:%S"), state, doc["stage"], doc["message"]), flush=True)
    try:
        os.makedirs(os.path.dirname(STATUS_FILE), exist_ok=True)
    except OSError:
        pass
    tmp = STATUS_FILE + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(doc, fh)
    os.replace(tmp, STATUS_FILE)


def fail(message):
    _failure[0] = message
    status("failed", message=message)
    sys.exit(1)


def touch_heartbeat():
    if not HEARTBEAT:
        return
    try:
        os.makedirs(os.path.dirname(HEARTBEAT), exist_ok=True)
        with open(HEARTBEAT, "a"):
            pass
        os.utime(HEARTBEAT, None)
    except OSError:
        pass


def heartbeat_while(event, every=30):
    def loop():
        while not event.wait(every):
            touch_heartbeat()
    touch_heartbeat()
    threading.Thread(target=loop, daemon=True).start()


# ------------------------------------------------------------- helpers


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, **kw)


def http_json(url, data=None, timeout=30, headers=None):
    h = {"Content-Type": "application/json", **(headers or {})}
    body = json.dumps(data).encode() if data is not None else None
    req = urllib.request.Request(url, data=body, headers=h)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def gpu_info():
    try:
        out = subprocess.run(["nvidia-smi", "--query-gpu=name,memory.total", "--format=csv,noheader,nounits"],
                             capture_output=True, text=True, check=True).stdout.strip().splitlines()[0]
        name, mem = [x.strip() for x in out.split(",")[:2]]
        return name, int(float(mem))
    except Exception:  # noqa: BLE001
        return None, 0


def gpu_used_mib():
    try:
        out = subprocess.run(["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"],
                             capture_output=True, text=True, check=True).stdout.strip().splitlines()[0]
        return int(float(out.strip()))
    except Exception:  # noqa: BLE001
        return 0


def dir_size(path):
    total = 0
    for r, _, fs in os.walk(path):
        for f in fs:
            try:
                total += os.path.getsize(os.path.join(r, f))
            except OSError:
                pass
    return total


def hf_headers():
    return {"Authorization": "Bearer " + HF_TOKEN} if HF_TOKEN else {}


def child_env():
    """The environment for the processes we start: no PLUGIN_* variables, the
    token in the environment only (never on a command line), and the NVIDIA
    library directory on the path so CUDA does not fall back to the CPU."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("PLUGIN_")}
    paths = [NVIDIA_LIB, env.get("LD_LIBRARY_PATH", "")]
    env["LD_LIBRARY_PATH"] = ":".join(p for p in paths if p)
    if HF_TOKEN:
        env["HF_TOKEN"] = HF_TOKEN
    return env


# ------------------------------------------------------------- model


def hf_tree(repo):
    url = "https://huggingface.co/api/models/%s/tree/main?recursive=true" % urllib.parse.quote(repo, safe="/")
    try:
        return http_json(url, headers=hf_headers(), timeout=60)
    except urllib.error.HTTPError as exc:
        hint = " (private or gated: save a Hugging Face token in Settings)" if exc.code in (401, 403) else ""
        fail("cannot read Hugging Face repo %s: HTTP %d%s" % (repo, exc.code, hint))
    except Exception as exc:  # noqa: BLE001
        fail("cannot read Hugging Face repo %s (%s)" % (repo, type(exc).__name__))


def pick_checkpoint():
    """-> (repo, path, size in bytes) of the checkpoint to download."""
    if PRESET == "custom":
        if not REPO:
            fail("the custom preset needs a checkpoint repository (owner/name)")
        if not REPO_ID.match(REPO):
            fail("%r is not a Hugging Face repository id (owner/name)" % REPO)
        repo, pattern = REPO, PATTERN
    else:
        repo, pattern = PRESETS[PRESET][0], PRESETS[PRESET][1]

    files = []
    for f in hf_tree(repo):
        if f.get("type") != "file":
            continue
        path = f.get("path", "")
        if not path.lower().endswith(".safetensors") or path.startswith("."):
            continue
        size = int((f.get("lfs") or {}).get("size") or f.get("size") or 0)
        files.append((path, size))

    if pattern:
        matched = [x for x in files if pattern.lower() in x[0].lower()]
    else:
        matched = [x for x in files if "/" not in x[0] and not NOT_A_CHECKPOINT.search(os.path.basename(x[0]))]
    if not matched:
        root = sorted(x[0] for x in files if "/" not in x[0])[:8]
        fail("%s has no checkpoint matching %r%s" % (
            repo, pattern or "*",
            ("; files at the repository root: %s" % ", ".join(root)) if root else ""))

    # The largest file is the full checkpoint; the smaller ones are usually
    # refiners or a VAE.
    matched.sort(key=lambda x: -x[1])
    path, size = matched[0]
    if "/" in path:
        fail("%s is a diffusers repository: ComfyUI needs one .safetensors file at the "
             "repository root, not the pieces in %s" % (repo, path.split("/")[0]))
    return repo, path, size


def safetensors_keys(path):
    """The tensor names in a .safetensors file, read from its header."""
    try:
        with open(path, "rb") as fh:
            head = fh.read(8)
            if len(head) < 8:
                return None
            length = struct.unpack("<Q", head)[0]
            if length > 64 * 1024 * 1024:
                return None
            data = fh.read(length)
        meta = json.loads(data)
    except Exception:  # noqa: BLE001
        return None
    return [k for k in meta if k != "__metadata__"]


def checkpoint_assets(path):
    """-> (model, text encoders, VAE) that a checkpoint file carries."""
    keys = safetensors_keys(path)
    if not keys:
        return None
    return (any(k.startswith(MODEL_PREFIXES) for k in keys),
            any(k.startswith(CLIP_PREFIXES) for k in keys),
            any(k.startswith(VAE_PREFIXES) for k in keys))


def download(repo, path, total):
    """Uses the Hugging Face library rather than its command line: the errors
    are typed (a gated repository says so) and the file lands in the models
    directory whatever the layout of the repository is."""
    run([sys.executable, "-m", "pip", "install", "-q", "-U", "huggingface_hub"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    os.makedirs(MODELS_CKPT, exist_ok=True)
    dest = os.path.join(MODELS_CKPT, os.path.basename(path))
    if os.path.exists(dest) and (not total or os.path.getsize(dest) == total):
        return dest

    start = dir_size(MODELS_CKPT)
    done = threading.Event()

    def progress():
        while not done.wait(10):
            # Partial downloads carry an ``.incomplete`` suffix, so count the
            # whole directory rather than the final file.
            got = max(0, dir_size(MODELS_CKPT) - start)
            status("running", "download", "%.1f / %.1f GB" % (got / 1e9, total / 1e9),
                   progress=min(1.0, got / total) if total else None)

    threading.Thread(target=progress, daemon=True).start()
    try:
        from huggingface_hub import hf_hub_download
        hf_hub_download(repo_id=repo, filename=path, local_dir=MODELS_CKPT,
                        token=HF_TOKEN or None)
    except Exception as exc:  # noqa: BLE001
        done.set()
        text = "%s" % exc
        if "401" in text or "403" in text or "gated" in text.lower():
            fail("%s is gated: save a Hugging Face token in Settings and run again" % repo)
        fail("checkpoint download failed: %s (see %s)" % (redact(text)[:200], os.path.join(RUN_DIR, "download.log")))
    finally:
        done.set()

    found = [os.path.join(r, f) for r, _, fs in os.walk(MODELS_CKPT) for f in fs
             if f == os.path.basename(path)]
    if not found:
        fail("download finished but %s is not in %s" % (os.path.basename(path), MODELS_CKPT))
    return found[0]


# ------------------------------------------------------------- install


def filter_requirements(lines):
    """Drops torch, torchvision and torchaudio: ComfyUI lists them without a
    version, and a fresh download would be the CPU build, or would fight with
    the CUDA build the runtime already has."""
    kept = []
    for line in lines:
        name = re.split(r"[<>=!~\[]", line.strip())[0].strip().lower()
        if name in ("torch", "torchvision", "torchaudio"):
            continue
        kept.append(line)
    return kept


def install_comfy():
    """Clones ComfyUI and installs its requirements, but keeps the CUDA build
    of PyTorch that Colab already has."""
    if not os.path.exists(os.path.join(COMFY, "main.py")):
        run(["git", "clone", "--depth", "1", "https://github.com/comfyanonymous/ComfyUI", COMFY])

    with open(os.path.join(COMFY, "requirements.txt")) as fh:
        kept = filter_requirements(fh.readlines())
    filtered = os.path.join(RUN_DIR, "requirements.txt")
    with open(filtered, "w") as fh:
        fh.writelines(kept)

    with open(os.path.join(RUN_DIR, "install.log"), "w") as log:
        run([sys.executable, "-m", "pip", "install", "-q", "-r", filtered],
            stdout=log, stderr=subprocess.STDOUT)


def check_torch():
    """Fails early when PyTorch cannot see the GPU: without the driver library
    on the path it loads, but every generation happens on the CPU."""
    probe = ("import torch;"
             "print('ok' if torch.cuda.is_available() else 'no-cuda');"
             "print(torch.cuda.get_device_name(0) if torch.cuda.is_available() else '')")
    try:
        out = subprocess.run([sys.executable, "-c", probe], capture_output=True, text=True,
                             env=child_env(), timeout=300)
    except Exception as exc:  # noqa: BLE001
        fail("could not run PyTorch: %s" % type(exc).__name__)
    lines = [x.strip() for x in out.stdout.splitlines() if x.strip()]
    if not lines or lines[0] != "ok":
        detail = " | ".join(lines)
        if not detail:
            detail = " | ".join((out.stderr or "").strip().splitlines()[-3:]) or "no output"
        fail("PyTorch cannot use the GPU (%s); check that %s holds libcuda.so.1"
             % (redact(detail)[:200], NVIDIA_LIB))
    return lines[1] if len(lines) > 1 else ""


# ------------------------------------------------------------- server

_server = [None]


def port_busy(port):
    with socket.socket() as s:
        s.settimeout(1)
        return s.connect_ex(("127.0.0.1", port)) == 0


def kill_group(pid, sig):
    """Signals the process group of a child that runs in its own session.
    Windows has no process groups, so the process tree is taken down there."""
    if hasattr(os, "killpg"):
        try:
            os.killpg(pid, sig)
        except OSError:
            pass
        return
    subprocess.run(["taskkill", "/T", "/F", "/PID", str(pid)], capture_output=True, check=False)


def stop_server():
    """Stops the server this worker started. It runs in its own session, so
    the signal has to reach its process group. Returns its exit code."""
    p = _server[0]
    if p is None or p.poll() is not None:
        return None
    kill_group(p.pid, signal.SIGTERM)
    for _ in range(20):
        if p.poll() is not None:
            return p.returncode
        time.sleep(0.5)
    kill_group(p.pid, getattr(signal, "SIGKILL", signal.SIGTERM))
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass
    return p.returncode


def start_server():
    cmd = [sys.executable, os.path.join(COMFY, "main.py"), "--listen", "127.0.0.1",
           "--port", str(PORT), "--cuda-malloc", "--dont-print-server"]
    cmd += shlex.split(EXTRA)
    with open(os.path.join(RUN_DIR, "server.cmd"), "w") as fh:
        fh.write(redact(shlex.join(cmd)) + "\n")
    # start_new_session: detached from the worker and the kernel.
    p = subprocess.Popen(cmd, cwd=COMFY, env=child_env(), stdout=open(SERVER_LOG, "w"),
                         stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, start_new_session=True)
    _server[0] = p
    return p


def wait_healthy(p, timeout=600):
    """`/` answers 200 once the HTTP server is up; /system_stats is the real
    readiness probe (the models are loaded lazily, at the first prompt)."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        if p.poll() is not None:
            return False
        try:
            http_json("http://127.0.0.1:%d/system_stats" % PORT, timeout=5)
            return True
        except Exception:  # noqa: BLE001
            pass
        touch_heartbeat()
        time.sleep(3)
    p.kill()
    return False


def log_tail(n=6):
    try:
        with open(SERVER_LOG, errors="replace") as fh:
            return redact(" | ".join(line.strip() for line in fh.readlines()[-n:]))
    except OSError:
        return ""


def on_sigterm(*_):
    stop_server()
    try:
        status("failed", message="stopped")
    except OSError:
        pass
    os._exit(143)


def client_attached(port):
    """True while a browser tab holds the ComfyUI websocket open. The tunnel
    keeps one TCP connection to the VM port for as long as the tab is open, so
    this tells an interactive session from an abandoned one. Returns None when
    the answer is unknown."""
    for cmd in (["ss", "-Htn", "state", "established"], ["netstat", "-tn"]):
        try:
            out = subprocess.run(cmd, capture_output=True, text=True, timeout=15).stdout
        except Exception:  # noqa: BLE001
            continue
        if not out:
            continue
        for line in out.splitlines():
            fields = line.split()
            if len(fields) < 4:
                continue
            # ss: State Recv-Q Send-Q Local:Port Peer:Port; netstat: Proto Recv-Q Send-Q Local Peer
            local = fields[3]
            if local.rsplit(":", 1)[-1] == str(port):
                return True
        return False
    return None


def monitor():
    """After `ready`: touch the heartbeat while a generation runs or while a
    browser is connected, so that the idle rule of the session can still stop
    a session nobody uses any more. A crashed server ends the run. The
    workflow of the newest job is published as it changes."""
    last = ""
    while True:
        time.sleep(30)
        p = _server[0]
        if p is not None and p.poll() is not None:
            fail("ComfyUI exited: %s" % log_tail())
        try:
            q = http_json("http://127.0.0.1:%d/queue" % PORT, timeout=10)
            busy = bool(q.get("queue_running") or q.get("queue_pending"))
        except Exception:  # noqa: BLE001
            busy = False
        attached = client_attached(PORT)
        if busy or attached is not False:
            touch_heartbeat()
        job = newest_job()
        if job and job[0] != last:
            last = job[0]
            _outputs["workflow"] = job[1]
            if job[2]:
                _outputs["images"] = job[2]
            status("ready", message="a new workflow finished", outputs=_outputs)


# ------------------------------------------------------------- warm-up


def newest_job():
    """The newest job ComfyUI ran: its id, the nodes of its workflow and the
    paths of the images it produced. The paths are on the VM; the app turns
    them into links through the forward."""
    try:
        history = http_json("http://127.0.0.1:%d/history" % PORT, timeout=15)
    except Exception:  # noqa: BLE001
        return None
    newest = None
    for prompt_id, entry in history.items():
        completed = ((entry.get("status") or {}).get("completed")) or 0
        if newest is None or completed >= newest[0]:
            newest = (completed, prompt_id, entry)
    if newest is None:
        return None
    _, prompt_id, entry = newest
    graph = entry.get("prompt")
    nodes = {}
    if isinstance(graph, list) and len(graph) > 2 and isinstance(graph[2], dict):
        nodes = {str(k): (v.get("class_type") or "?")
                 for k, v in graph[2].items() if isinstance(v, dict)}
    paths = []
    for out in (entry.get("outputs") or {}).values():
        if not isinstance(out, dict):
            continue
        for image in out.get("images") or []:
            name = image.get("filename")
            if not name:
                continue
            paths.append("/view?" + urllib.parse.urlencode({
                "filename": name,
                "subfolder": image.get("subfolder", ""),
                "type": image.get("type", "output"),
            }))
    return prompt_id, nodes, paths


def warmup_graph(ckpt_name, steps, cfg, size, latent_node):
    """A minimal txt2img workflow: CheckpointLoaderSimple -> two CLIPTextEncode
    -> KSampler -> VAEDecode -> SaveImage. Works for SD1.5, SDXL and the
    single-file Flux checkpoints alike."""
    return {
        "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": ckpt_name}},
        "2": {"class_type": "CLIPTextEncode", "inputs": {"text": "a photo of a red apple on a wooden table",
                                                         "clip": ["1", 1]}},
        "3": {"class_type": "CLIPTextEncode", "inputs": {"text": "", "clip": ["1", 1]}},
        "4": {"class_type": latent_node, "inputs": {"width": size, "height": size, "batch_size": 1}},
        "5": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": steps, "cfg": cfg, "sampler_name": "euler",
                                                   "scheduler": "simple", "denoise": 1.0,
                                                   "model": ["1", 0], "positive": ["2", 0],
                                                   "negative": ["3", 0], "latent_image": ["4", 0]}},
        "6": {"class_type": "VAEDecode", "inputs": {"samples": ["5", 0], "vae": ["1", 2]}},
        "7": {"class_type": "SaveImage", "inputs": {"filename_prefix": "warmup", "images": ["6", 0]}},
    }


def warmup(p, ckpt_name, steps, cfg, size, latent_node):
    try:
        r = http_json("http://127.0.0.1:%d/prompt" % PORT,
                      {"prompt": warmup_graph(ckpt_name, steps, cfg, size, latent_node)}, timeout=1800)
    except urllib.error.HTTPError as exc:
        body = ""
        try:
            body = exc.read().decode("utf-8", "replace")[:300]
        except Exception:  # noqa: BLE001
            pass
        fail("the test prompt was rejected: HTTP %d %s %s" % (exc.code, redact(body), log_tail()))
    except Exception as exc:  # noqa: BLE001
        fail("the test prompt failed: %s %s" % (type(exc).__name__, log_tail()))
    # The prompt API returns as soon as the graph is queued; wait for history.
    prompt_id = r.get("prompt_id")
    if not prompt_id:
        fail("the test prompt was queued without an id: %s %s" % (redact(str(r))[:200], log_tail()))
    for _ in range(600):
        if p.poll() is not None:
            fail("ComfyUI stopped while the test image was running: %s" % log_tail())
        try:
            hist = http_json("http://127.0.0.1:%d/history/%s" % (PORT, prompt_id), timeout=10)
            entry = hist.get(prompt_id)
            if entry:
                if entry.get("status", {}).get("status_str") == "error":
                    fail("the test image failed: %s" % log_tail())
                outputs = entry.get("outputs", {})
                first = next(iter(outputs.values()), {}).get("images", [{}])[0].get("filename")
                return first
        except Exception:  # noqa: BLE001
            pass
        touch_heartbeat()
        time.sleep(3)
    fail("the test image did not finish in time")


# ------------------------------------------------------------- self test


def fake_safetensors(path, keys):
    """A .safetensors file with a header and no weights."""
    body = json.dumps({k: {"dtype": "F16", "shape": [1], "data_offsets": [0, 2]} for k in keys}).encode()
    with open(path, "wb") as fh:
        fh.write(struct.pack("<Q", len(body)))
        fh.write(body)
    return path


def pid_alive(pid):
    if os.name == "nt":
        out = subprocess.run(["tasklist", "/FI", "PID eq %d" % pid, "/NH"],
                             capture_output=True, text=True).stdout
        return str(pid) in out
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def self_test():
    """The parts that can be checked without a Colab runtime:
        python3 worker.py --self-test
    """
    global RUN_DIR, STATUS_FILE, PRESET, REPO, PATTERN, hf_tree
    RUN_DIR = tempfile.mkdtemp(prefix="comfyui-selftest")
    STATUS_FILE = os.path.join(RUN_DIR, "status.json")
    failures = 0

    def check(name, got, want):
        nonlocal failures
        ok = got == want
        failures += 0 if ok else 1
        print("%s %-46s %s" % ("ok  " if ok else "FAIL", name, got if ok else "%r != %r" % (got, want)))

    def expect_failure(name, fn):
        nonlocal failures
        try:
            fn()
        except SystemExit:
            print("ok   %-46s failed as expected" % name)
            return
        failures += 1
        print("FAIL %-46s did not fail" % name)

    def patch(**names):
        """Replaces module level functions and returns a function that puts
        the originals back."""
        old = {k: globals()[k] for k in names}
        globals().update(names)
        return lambda: globals().update(old)

    # ---------------------------------------------------------- picking
    tree = [
        {"type": "file", "path": "v1-5-pruned-emaonly.safetensors", "lfs": {"size": 4270000000}},
        {"type": "file", "path": "unet/diffusion_pytorch_model.safetensors", "lfs": {"size": 10270000000}},
        {"type": "file", "path": "cereal_lora.safetensors", "lfs": {"size": 90000000}},
    ]
    hf_tree = lambda repo: tree
    PRESET, REPO, PATTERN = "sd15", "", ""
    check("the sd15 preset", pick_checkpoint()[:2],
          ("stable-diffusion-v1-5/stable-diffusion-v1-5", "v1-5-pruned-emaonly.safetensors"))
    PRESET, REPO, PATTERN = "custom", "someone/model", ""
    check("a custom repository", pick_checkpoint()[1], "v1-5-pruned-emaonly.safetensors")
    PATTERN = "unet/"
    expect_failure("a diffusers repository", pick_checkpoint)
    PATTERN = "nothing"
    expect_failure("a repository without a match", pick_checkpoint)
    PATTERN = ""
    hf_tree = lambda repo: [{"type": "file", "path": "cereal_lora.safetensors", "lfs": {"size": 90000000}}]
    expect_failure("a repository with only a LoRA", pick_checkpoint)
    REPO = "not-a-repo"
    expect_failure("a repository id that is not owner/name", pick_checkpoint)

    # ---------------------------------------------------------- contents
    ckpt = os.path.join(RUN_DIR, "fake.safetensors")
    check("an SD 1.5 checkpoint", checkpoint_assets(fake_safetensors(ckpt, [
        "model.diffusion_model.input_blocks.0.0.weight", "cond_stage_model.transformer.text_model.emb.weight",
        "first_stage_model.decoder.conv_in.weight"])), (True, True, True))
    check("an SDXL checkpoint", checkpoint_assets(fake_safetensors(ckpt, [
        "model.diffusion_model.input_blocks.0.0.weight", "conditioner.embedders.0.text_model.emb.weight",
        "first_stage_model.decoder.conv_in.weight"])), (True, True, True))
    check("an all-in-one Flux checkpoint", checkpoint_assets(fake_safetensors(ckpt, [
        "model.diffusion_model.img_in.weight", "text_encoders.clip_l.transformer.text_model.emb.weight",
        "text_encoders.t5xxl.transformer.encoder.block.0.weight", "vae.decoder.conv_in.weight"])), (True, True, True))
    check("a Flux transformer alone", checkpoint_assets(fake_safetensors(ckpt, [
        "double_blocks.0.img_attn.qkv.weight", "single_blocks.0.modulation.lin.weight"])), (True, False, False))
    check("a LoRA is no checkpoint", checkpoint_assets(fake_safetensors(ckpt, [
        "lora_unet_down_blocks.0.to_q.lora_A.weight"])), (False, False, False))

    # ---------------------------------------------------------- the newest job
    def job_history(images):
        return {
            "old": {
                "status": {"completed": 1},
                "prompt": [1, "old", {"5": {"class_type": "KSampler"}}],
                "outputs": {},
            },
            "new": {
                "status": {"completed": 9},
                "prompt": [1, "new", {"9": {"class_type": "SaveImage"}}],
                "outputs": {"6": {"images": images}},
            },
        }

    restore = patch(http_json=lambda url, **kw: job_history([
        {"filename": "a.png", "subfolder": "", "type": "output"}]))
    try:
        job = newest_job()
    finally:
        restore()
    check("the newest job wins", job[0], "new")
    check("its workflow is read", job[1], {"9": "SaveImage"})

    restore = patch(http_json=lambda url, **kw: job_history([
        {"filename": "a.png", "subfolder": "", "type": "output"}]))
    try:
        job = newest_job()
    finally:
        restore()
    check("its images are paths on the VM", job[2], ["/view?filename=a.png&subfolder=&type=output"])

    # ---------------------------------------------------------- the install
    # The real install path, with the command helper replaced by a recorder:
    # the shadowing of that helper broke every install once already.
    commands = []

    def fake_run(cmd, **kw):
        commands.append(cmd)
        return subprocess.CompletedProcess(cmd, 0)

    fake_comfy = os.path.join(RUN_DIR, "ComfyUI")
    os.makedirs(fake_comfy, exist_ok=True)
    with open(os.path.join(fake_comfy, "main.py"), "w") as fh:
        fh.write("")
    with open(os.path.join(fake_comfy, "requirements.txt"), "w") as fh:
        fh.write("torch\ntorchvision\ntorchsde\nnumpy>=1.25.0\n")

    restore = patch(COMFY=fake_comfy, run=fake_run)
    try:
        install_comfy()
    finally:
        restore()
    check("the install runs pip", any("pip" in " ".join(c) for c in commands), True)
    check("the install does not clone again", any("clone" in " ".join(c) for c in commands), False)
    installed = [l.strip().lower() for l in open(os.path.join(RUN_DIR, "requirements.txt")).read().splitlines()]
    check("the install leaves PyTorch alone", [l for l in installed if l in ("torch", "torchvision")], [])
    check("the install keeps the rest", "torchsde" in installed and any(l.startswith("numpy") for l in installed), True)

    commands.clear()
    os.remove(os.path.join(fake_comfy, "main.py"))
    restore = patch(COMFY=fake_comfy, run=fake_run)
    try:
        install_comfy()
    finally:
        restore()
    check("the install clones ComfyUI", any("clone" in " ".join(c) for c in commands), True)

    # ---------------------------------------------------------- the model family
    check("the subprocess helper runs a command", run([sys.executable, "-c", "pass"]).returncode, 0)
    # A name defined twice at the top level shadows the first one, and the
    # shadowed one is usually a helper the worker calls.
    tree = ast.parse(open(os.path.abspath(__file__), encoding="utf-8").read())
    names = [n.name for n in tree.body if isinstance(n, ast.FunctionDef)]
    check("no function is defined twice", sorted({n for n in names if names.count(n) > 1}), [])
    PRESET = "flux-schnell"
    check("Flux wants a 16 channel latent", generation_params("Comfy-Org/Flux-1-Schnell", "flux1-schnell-fp8.safetensors"),
          (4, 1.0, 1024, "EmptySD3LatentImage"))
    PRESET = "sd15"
    check("SD 1.5 wants a 4 channel latent", generation_params("r/x", "x.safetensors"),
          (20, 7.5, 512, "EmptyLatentImage"))
    check("PyTorch is left alone", [l for l in filter_requirements(["torch\n", "torchvision\n", "torchsde\n", "numpy>=1.25.0\n"])
                                    if l.strip().lower() in ("torch", "torchvision")], [])

    # ---------------------------------------------------------- the failure paths
    def boom():
        raise RuntimeError("the install failed")

    def must_not_start():
        raise AssertionError("the server must not start")

    restore = patch(install_comfy=boom, check_torch=lambda: "A100", start_server=must_not_start)
    try:
        expect_failure("a failed install", run_worker)
    finally:
        restore()
    check("a failed install is reported", json.load(open(STATUS_FILE))["state"], "failed")

    restore = patch(install_comfy=lambda: None, check_torch=lambda: "A100",
                    pick_checkpoint=lambda: ("r/x", "x.safetensors", 1),
                    download=lambda repo, path, total: path,
                    port_busy=lambda port: True, start_server=must_not_start)
    try:
        expect_failure("a port that is taken", run_worker)
    finally:
        restore()
    check("a taken port is not taken from anyone", "already in use" in json.load(open(STATUS_FILE))["message"], True)

    dead = subprocess.Popen([sys.executable, "-c", "pass"])
    dead.wait()
    check("a server that died is noticed", wait_healthy(dead, timeout=5), False)

    server = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"], start_new_session=True)

    def fake_start():
        _server[0] = server
        return server

    restore = patch(install_comfy=lambda: None, check_torch=lambda: "A100",
                    pick_checkpoint=lambda: ("r/x", "x.safetensors", 1),
                    download=lambda repo, path, total: path,
                    port_busy=lambda port: False,
                    start_server=fake_start, wait_healthy=lambda p: True,
                    warmup=lambda *a: (_ for _ in ()).throw(RuntimeError("the warm-up failed")),
                    gpu_used_mib=lambda: 4096)
    try:
        expect_failure("a failed warm-up", run_worker)
    finally:
        restore()
    check("a failed warm-up stops the server", pid_alive(server.pid), False)

    # a cancelled run stops the server too, and says so
    pidfile = os.path.join(RUN_DIR, "cancel.pid")
    script = ("import subprocess, sys;"
              "sys.path.insert(0, %r);"
              "import worker;"
              "p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'], start_new_session=True);"
              "worker._server[0] = p;"
              "open(%r, 'w').write(str(p.pid));"
              "worker.on_sigterm()") % (os.path.dirname(os.path.abspath(__file__)), pidfile)
    env = dict(os.environ, PLUGIN_STATUS_FILE=os.path.join(RUN_DIR, "cancel-status.json"))
    cancelled = subprocess.run([sys.executable, "-c", script], capture_output=True, text=True, env=env)
    check("a cancelled run exits 143", cancelled.returncode, 143)
    time.sleep(0.5)
    check("a cancelled run stops the server", pid_alive(int(open(pidfile).read())), False)

    print("\n%d failure(s)" % failures)
    return 1 if failures else 0


# ------------------------------------------------------------- main


def generation_params(repo, path):
    """Steps, cfg, size and the latent node for the checkpoint. Flux models
    work on a 16 channel latent (EmptySD3LatentImage) and are distilled, so
    they want a low cfg and few steps."""
    flux = "flux" in os.path.basename(path).lower() or "flux" in repo.lower()
    if PRESET != "custom":
        steps, cfg, size = PRESETS[PRESET][2:]
    elif flux:
        steps, cfg, size = 4, 1.0, 1024
    else:
        steps, cfg, size = CUSTOM_DEFAULTS[2:]
    if STEPS > 0:
        steps = STEPS
    if SIZE > 0:
        size = SIZE
    return steps, cfg, size, ("EmptySD3LatentImage" if flux else "EmptyLatentImage")


def main():
    os.makedirs(RUN_DIR, exist_ok=True)
    signal.signal(signal.SIGTERM, on_sigterm)
    busy = threading.Event()
    heartbeat_while(busy)
    if PRESET not in PRESETS and PRESET != "custom":
        fail("unknown preset %r" % PRESET)
    if PORT == 8080:
        fail("VM port 8080 belongs to Colab; choose another port")

    ready = False
    try:
        status("running", "detect", "checking the GPU")
        gpu, vram = gpu_info()
        if not gpu:
            print("warning: no GPU detected, generation will be very slow", flush=True)
        status("running", "detect", "%s, %d MiB" % (gpu, vram) if gpu else "no GPU (CPU run, very slow)")

        status("running", "install", "installing ComfyUI")
        install_comfy()
        torch_gpu = check_torch()
        status("running", "install", "ComfyUI installed (%s)" % (torch_gpu or gpu or "no GPU"))

        status("running", "download", "looking for a checkpoint")
        repo, ckpt_path, total = pick_checkpoint()
        steps, cfg, size, latent_node = generation_params(repo, ckpt_path)
        if latent_node == "EmptySD3LatentImage" and vram and vram < 20000:
            print("warning: a Flux checkpoint wants about 24 GB of VRAM; this GPU has %d MiB" % vram, flush=True)
        status("running", "download", "%s (%.1f GB)" % (ckpt_path, total / 1e9), progress=0.0)
        ckpt_file = download(repo, ckpt_path, total)
        ckpt_name = os.path.relpath(ckpt_file, MODELS_CKPT)
        assets = checkpoint_assets(ckpt_file)
        if assets:
            missing = []
            if not assets[0]:
                missing.append("the model")
            if not assets[1]:
                missing.append("the text encoders")
            if not assets[2]:
                missing.append("the VAE")
            if missing:
                fail("%s is missing %s: ComfyUI needs one file that holds the model, the text "
                     "encoders and the VAE (the all-in-one Flux file is "
                     "Comfy-Org/Flux-1-Schnell/flux1-schnell-fp8.safetensors)" % (ckpt_name, ", ".join(missing)))
        status("running", "download", "checkpoint ready: %s" % ckpt_name)

        # The server of another run, or one the user started, may hold the
        # port. It is not this worker's to stop.
        if port_busy(PORT):
            fail("port %d is already in use on the VM: another service holds it. "
                 "Stop it, or choose another port" % PORT)

        status("running", "server", "starting ComfyUI on port %d" % PORT)
        p = start_server()
        if not wait_healthy(p):
            fail("ComfyUI did not start: %s" % log_tail())
        status("running", "server", "ComfyUI is up")

        status("running", "warmup", "generating a test image (first run loads the model into VRAM)")
        image = warmup(p, ckpt_name, steps, cfg, size, latent_node)
        used = gpu_used_mib()
        if gpu and used < 512:
            fail("the test image ran on the CPU (the GPU shows %d MiB of memory in use)" % used)
        outputs = {"preset": PRESET, "checkpoint": ckpt_name, "repo": repo, "gpu": gpu,
                   "vram_total_mib": vram, "vram_used_mib": used, "test_image": image,
                   "steps": steps, "cfg": cfg, "size": size, "url_path": "/",
                   "workflows_dir": os.path.join(COMFY, "user", "default", "workflows")}
        job = newest_job()
        if job:
            outputs["workflow"] = job[1]
            if job[2]:
                outputs["images"] = job[2]
        _outputs.update(outputs)
        busy.set()
        status("ready", "warmup", "%s ready: test image %s" % (ckpt_name, image or "done"),
               progress=1.0, outputs=_outputs)
        ready = True
        monitor()
    finally:
        # A run that never became ready leaves no server behind: it would keep
        # the port and the VRAM of a session that stays alive.
        if not ready:
            code = stop_server()
            if code is not None:
                print("the server was stopped (exit %s)" % code, flush=True)
                if _failure[0]:
                    status("failed", message="%s (the server was stopped) %s"
                           % (_failure[0], log_tail(3)))


def run_worker():
    try:
        main()
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001
        fail("worker error: %s: %s" % (type(exc).__name__, redact("%s" % exc)[:200]))


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        sys.exit(self_test())
    run_worker()
