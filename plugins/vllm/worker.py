#!/usr/bin/env python3
"""vLLM OpenAI server for Colab Playground. Runs only on the Colab VM.

The host uploads this file and starts it there. It does not run on the
computer where the app is open. Without PLUGIN_RUN_DIR it exits before
any install or download.
"""

import json
import os
import shlex
import signal
import subprocess
import sys
import time
import urllib.request

STAGE = "detect"
CHILD = None
DTYPES = {"auto", "float16", "bfloat16"}


def secrets():
    found = []
    for key, value in os.environ.items():
        if value and (key == "PLUGIN_HF_TOKEN" or key == "PLUGIN_INPUT_API_KEY" or key.startswith("PLUGIN_SECRET_")):
            found.append(value)
    found.sort(key=len, reverse=True)
    return found


def redact(text):
    out = str(text)
    for secret in secrets():
        if len(secret) >= 4:
            out = out.replace(secret, "***")
    return out


def beat():
    path = os.environ.get("PLUGIN_HEARTBEAT", "")
    if path:
        open(path, "a", encoding="utf-8").close()
        os.utime(path, None)


def status(state, stage, message, progress=None):
    body = {"state": state, "stage": stage, "message": redact(message)}
    if progress is not None:
        body["progress"] = progress
    path = os.environ["PLUGIN_STATUS_FILE"]
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as handle:
        json.dump(body, handle)
    os.replace(tmp, path)
    beat()


def fail(message):
    status("failed", STAGE, message)
    raise SystemExit(1)


def run_checked(argv, stage):
    global STAGE
    STAGE = stage
    done = subprocess.run(argv, capture_output=True, text=True)
    if done.returncode != 0:
        detail = (done.stderr or done.stdout or "command failed").strip().splitlines()
        fail(detail[-1] if detail else "command failed")


def number(name, default, low, high):
    raw = os.environ.get(name, "")
    try:
        value = float(raw) if raw else float(default)
    except ValueError:
        fail(name + " is not a number")
    if value < low or value > high:
        fail(name + " is out of range")
    return value


def repo_id(value):
    parts = value.split("/")
    if len(parts) != 2:
        return False

    def ok(part):
        return bool(part) and not part.startswith(("-", ".")) and all(
            c.isalnum() or c in "-_." for c in part
        )

    return ok(parts[0]) and ok(parts[1])


def on_term(_signum, _frame):
    if CHILD is not None and CHILD.poll() is None:
        os.killpg(CHILD.pid, signal.SIGTERM)
    raise SystemExit(0)


def main():
    global STAGE, CHILD
    if not os.environ.get("PLUGIN_RUN_DIR"):
        sys.stderr.write(
            "This plugin runs only on a Colab runtime started by Colab Playground.\n"
        )
        return 2

    signal.signal(signal.SIGTERM, on_term)
    repo = os.environ.get("PLUGIN_INPUT_REPO", "").strip()
    if not repo_id(repo):
        fail("model repository must look like owner/name")
    dtype = os.environ.get("PLUGIN_INPUT_DTYPE", "auto").strip() or "auto"
    if dtype not in DTYPES:
        fail("compute type is not one of the choices")
    max_len = int(number("PLUGIN_INPUT_MAX_LEN", 4096, 128, 131072))
    gpu_util = number("PLUGIN_INPUT_GPU_UTIL", 0.9, 0.5, 0.95)
    port = int(number("PLUGIN_INPUT_PORT", 8001, 1024, 65535))
    if port == 8080:
        fail("port 8080 belongs to Colab")
    extra = os.environ.get("PLUGIN_INPUT_EXTRA_ARGS", "")
    api_key = os.environ.get("PLUGIN_INPUT_API_KEY", "")
    token = os.environ.get("PLUGIN_HF_TOKEN", "")

    STAGE = "detect"
    status("running", STAGE, "checking the GPU", 0.1)
    beat()
    if subprocess.run(["nvidia-smi", "-L"], capture_output=True).returncode != 0:
        fail("no GPU was visible to nvidia-smi")

    STAGE = "install"
    status("running", STAGE, "installing vLLM", 0.2)
    run_checked(
        [sys.executable, "-m", "pip", "install", "--disable-pip-version-check", "vllm"],
        "install",
    )

    STAGE = "download"
    status("running", STAGE, "the server will load the model", 0.4)
    cmd = [
        sys.executable,
        "-m",
        "vllm.entrypoints.openai.api_server",
        "--model",
        repo,
        "--host",
        "127.0.0.1",
        "--port",
        str(port),
        "--max-model-len",
        str(max_len),
        "--gpu-memory-utilization",
        str(gpu_util),
    ]
    if dtype != "auto":
        cmd.extend(["--dtype", dtype])
    for token_arg in shlex.split(extra):
        if any(secret and secret in token_arg for secret in secrets()):
            continue
        cmd.append(token_arg)

    child_env = os.environ.copy()
    if api_key:
        child_env["VLLM_API_KEY"] = api_key
    if token:
        child_env["HF_TOKEN"] = token
        child_env["HUGGING_FACE_HUB_TOKEN"] = token

    STAGE = "server"
    status("running", STAGE, "starting the server", 0.6)
    CHILD = subprocess.Popen(cmd, env=child_env, start_new_session=True)

    STAGE = "warmup"
    health = "http://127.0.0.1:%d/health" % port
    deadline = time.time() + 30 * 60
    while time.time() < deadline:
        beat()
        if CHILD.poll() is not None:
            fail("vLLM exited before it was ready")
        try:
            with urllib.request.urlopen(health, timeout=5) as response:
                if response.status == 200:
                    break
        except Exception:
            time.sleep(5)
    else:
        fail("the health check did not succeed")

    status("ready", STAGE, "OpenAI-compatible API is up", 1.0)
    while True:
        beat()
        if CHILD.poll() is not None:
            fail("vLLM stopped")
        time.sleep(30)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception as exc:
        if os.environ.get("PLUGIN_STATUS_FILE"):
            status("failed", STAGE, redact(exc))
        raise SystemExit(1)
