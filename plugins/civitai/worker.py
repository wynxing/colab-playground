#!/usr/bin/env python3
"""Civitai download for Colab Playground. Runs only on the Colab VM.

The token is sent as a header and is never placed in the download address,
the status message, or the log. Without PLUGIN_RUN_DIR it exits before
any request.
"""

import json
import os
import sys
import urllib.request

STAGE = "resolve"
OUT = "/content/civitai"
KINDS = {"checkpoint": ".safetensors", "lora": ".safetensors", "vae": ".vae.pt", "other": ".bin"}


def secrets():
    found = []
    for key, value in os.environ.items():
        if value and (key == "PLUGIN_HF_TOKEN" or key.startswith("PLUGIN_SECRET_")):
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


def status(state, stage, message, progress=None, outputs=None):
    body = {"state": state, "stage": stage, "message": redact(message)}
    if progress is not None:
        body["progress"] = progress
    if outputs:
        body["outputs"] = outputs
    path = os.environ["PLUGIN_STATUS_FILE"]
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as handle:
        json.dump(body, handle)
    os.replace(tmp, path)
    beat()


def fail(message):
    status("failed", STAGE, message)
    raise SystemExit(1)


def safe_name(name, fallback):
    leaf = name.replace("\\", "/").rsplit("/", 1)[-1].strip().strip(".")
    if not leaf or leaf in {".", ".."} or any(c in leaf for c in "/\\\0"):
        return fallback
    cleaned = "".join(c if c.isalnum() or c in "._-" else "_" for c in leaf)
    return cleaned[:120] or fallback


def filename_from(headers, version, kind):
    disposition = headers.get("Content-Disposition", "")
    marker = "filename="
    if marker in disposition.lower():
        raw = disposition[disposition.lower().index(marker) + len(marker):].split(";", 1)[0]
        raw = raw.strip().strip('"')
        return safe_name(raw, version + KINDS.get(kind, ".bin"))
    return version + KINDS.get(kind, ".bin")


def main():
    global STAGE
    if not os.environ.get("PLUGIN_RUN_DIR"):
        sys.stderr.write(
            "This plugin runs only on a Colab runtime started by Colab Playground.\n"
        )
        return 2

    version = os.environ.get("PLUGIN_INPUT_MODEL_VERSION", "").strip()
    kind = os.environ.get("PLUGIN_INPUT_KIND", "checkpoint").strip() or "checkpoint"
    token = os.environ.get("PLUGIN_SECRET_CIVITAI_TOKEN", "")
    STAGE = "resolve"
    if not version.isdigit() or len(version) > 12:
        fail("model version id must be a number")
    if kind not in KINDS:
        fail("file kind is not one of the choices")
    if not token:
        fail("save a Civitai token before starting")
    status("running", STAGE, "version id looks usable", 0.1)

    STAGE = "download"
    status("running", STAGE, "downloading", 0.3)
    url = "https://civitai.com/api/download/models/" + version
    request = urllib.request.Request(
        url,
        headers={
            "Authorization": "Bearer " + token,
            "User-Agent": "colab-playground",
        },
    )
    os.makedirs(OUT, exist_ok=True)
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            name = filename_from(response.headers, version, kind)
            dest = os.path.join(OUT, name)
            tmp = dest + ".partial"
            total = 0
            with open(tmp, "wb") as handle:
                while True:
                    chunk = response.read(1024 * 256)
                    if not chunk:
                        break
                    handle.write(chunk)
                    total += len(chunk)
                    beat()
    except Exception as exc:
        fail(redact(exc))
    if total <= 0:
        fail("the download was empty")

    STAGE = "check"
    os.replace(tmp, dest)
    status("ready", STAGE, "file saved", 1.0, {"path": dest, "bytes": total})
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception as exc:
        if os.environ.get("PLUGIN_STATUS_FILE"):
            status("failed", STAGE, redact(exc))
        raise SystemExit(1)
