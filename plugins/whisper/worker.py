#!/usr/bin/env python3
"""Whisper transcription for Colab Playground. Runs only on the Colab VM.

Without PLUGIN_RUN_DIR it exits before any install or download.
"""

import json
import os
import subprocess
import sys

STAGE = "install"
MODELS = {"tiny", "base", "small", "medium", "large-v3"}
TASKS = {"transcribe", "translate"}
OUT = "/content/whisper/out"


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


def under_run(path):
    root = os.path.realpath(os.environ["PLUGIN_RUN_DIR"])
    real = os.path.realpath(path)
    return real == root or real.startswith(root + os.sep)


def main():
    global STAGE
    if not os.environ.get("PLUGIN_RUN_DIR"):
        sys.stderr.write(
            "This plugin runs only on a Colab runtime started by Colab Playground.\n"
        )
        return 2

    audio = os.environ.get("PLUGIN_INPUT_AUDIO", "").strip()
    model = os.environ.get("PLUGIN_INPUT_MODEL", "small").strip() or "small"
    language = os.environ.get("PLUGIN_INPUT_LANGUAGE", "").strip()
    task = os.environ.get("PLUGIN_INPUT_TASK", "transcribe").strip() or "transcribe"
    if model not in MODELS:
        fail("Whisper model is not one of the choices")
    if task not in TASKS:
        fail("task is not one of the choices")
    if language and (
        len(language) > 5 or not language.isalpha() or not language.isascii()
    ):
        fail("language must be a short code such as en or zh")
    if not audio or not under_run(audio) or not os.path.isfile(audio):
        fail("the audio file is not in this run")

    STAGE = "install"
    status("running", STAGE, "installing faster-whisper", 0.1)
    done = subprocess.run(
        [sys.executable, "-m", "pip", "install", "--disable-pip-version-check", "faster-whisper"],
        capture_output=True,
        text=True,
    )
    if done.returncode != 0:
        detail = (done.stderr or done.stdout or "pip failed").strip().splitlines()
        fail(detail[-1] if detail else "pip failed")

    token = os.environ.get("PLUGIN_HF_TOKEN", "")
    if token:
        os.environ["HF_TOKEN"] = token
        os.environ["HUGGING_FACE_HUB_TOKEN"] = token

    STAGE = "transcribe"
    status("running", STAGE, "transcribing", 0.4)
    beat()
    from faster_whisper import WhisperModel

    device = "cuda"
    try:
        whisper = WhisperModel(model, device=device, compute_type="float16")
    except Exception:
        device = "cpu"
        status("running", STAGE, "no GPU load; retrying on CPU", 0.5)
        whisper = WhisperModel(model, device="cpu", compute_type="int8")
    segments, _info = whisper.transcribe(
        audio,
        language=language or None,
        task=task,
    )
    text = []
    for segment in segments:
        beat()
        text.append(segment.text)
    transcript = "".join(text).strip()

    STAGE = "export"
    status("running", STAGE, "saving the transcript", 0.9)
    os.makedirs(OUT, exist_ok=True)
    dest = os.path.join(OUT, "transcript.txt")
    with open(dest, "w", encoding="utf-8") as handle:
        handle.write(transcript)
        handle.write("\n")
    status(
        "ready",
        STAGE,
        "transcript saved",
        1.0,
        {"path": dest, "device": device, "chars": len(transcript)},
    )
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
