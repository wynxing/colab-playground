#!/usr/bin/env python3
"""Unsloth or Axolotl fine-tune for Colab Playground. Runs only on the Colab VM.

Without PLUGIN_RUN_DIR it exits before any install or download. User values
are read from the environment and passed as data, not written into code.
"""

import json
import os
import subprocess
import sys

STAGE = "install"
OUT = "/content/train/out"
METHODS = {"unsloth", "axolotl"}


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


def status(state, stage, message, progress=None, metrics=None):
    body = {"state": state, "stage": stage, "message": redact(message)}
    if progress is not None:
        body["progress"] = progress
    if metrics:
        body["metrics"] = metrics
    path = os.environ["PLUGIN_STATUS_FILE"]
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as handle:
        json.dump(body, handle)
    os.replace(tmp, path)
    beat()


def fail(message):
    status("failed", STAGE, message)
    raise SystemExit(1)


def ident(value, slash):
    if not value or len(value) > 96 or value.startswith(("-", ".")):
        return False
    allowed = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.")
    if slash:
        allowed.add("/")
    return all(c in allowed for c in value)


def repo_id(value):
    parts = value.split("/")
    return len(parts) == 2 and ident(parts[0], False) and ident(parts[1], False)


def integer(name, default, low, high):
    raw = os.environ.get(name, "")
    try:
        value = int(float(raw)) if raw else default
    except ValueError:
        fail(name + " is not a number")
    if value < low or value > high:
        fail(name + " is out of range")
    return value


def pip_install(package):
    done = subprocess.run(
        [sys.executable, "-m", "pip", "install", "--disable-pip-version-check", package],
        capture_output=True,
        text=True,
    )
    if done.returncode != 0:
        detail = (done.stderr or done.stdout or "pip failed").strip().splitlines()
        fail(detail[-1] if detail else "pip failed")


def load_rows(dataset, data_file):
    from datasets import load_dataset

    if data_file:
        data = load_dataset("json", data_files=data_file, split="train")
    else:
        data = load_dataset(dataset, split="train")
    if "text" in data.column_names:
        return data

    def as_text(row):
        parts = []
        for key in data.column_names:
            value = row.get(key)
            if isinstance(value, str) and value.strip():
                parts.append(value.strip())
        return {"text": "\n".join(parts)}

    return data.map(as_text)


def train_unsloth(base, dataset, data_file, steps):
    from unsloth import FastLanguageModel
    from trl import SFTTrainer
    from transformers import TrainingArguments

    model, tokenizer = FastLanguageModel.from_pretrained(
        model_name=base,
        max_seq_length=512,
        load_in_4bit=True,
    )
    model = FastLanguageModel.get_peft_model(
        model,
        r=16,
        target_modules=["q_proj", "k_proj", "v_proj", "o_proj"],
        lora_alpha=16,
        lora_dropout=0,
        bias="none",
        use_gradient_checkpointing="unsloth",
    )
    data = load_rows(dataset, data_file)
    trainer = SFTTrainer(
        model=model,
        processing_class=tokenizer,
        train_dataset=data,
        dataset_text_field="text",
        args=TrainingArguments(
            output_dir=OUT,
            per_device_train_batch_size=1,
            max_steps=steps,
            logging_steps=1,
            report_to="none",
        ),
    )
    result = trainer.train()
    model.save_pretrained(OUT)
    tokenizer.save_pretrained(OUT)
    loss = None
    if result is not None and getattr(result, "training_loss", None) is not None:
        loss = float(result.training_loss)
    return loss


def train_axolotl(base, dataset, data_file, steps):
    config = {
        "base_model": base,
        "datasets": [
            {
                "path": data_file or dataset,
                "type": "completion" if data_file else "alpaca",
            }
        ],
        "dataset_prepared_path": None,
        "val_set_size": 0,
        "adapter": "lora",
        "lora_r": 16,
        "lora_alpha": 16,
        "sequence_len": 512,
        "micro_batch_size": 1,
        "gradient_accumulation_steps": 1,
        "num_epochs": 1,
        "max_steps": steps,
        "output_dir": OUT,
        "logging_steps": 1,
    }
    path = os.path.join(os.environ["PLUGIN_RUN_DIR"], "axolotl.json")
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(config, handle)
    # Axolotl reads YAML. The JSON above is data; emit one quoted field per line.
    yaml_path = os.path.join(os.environ["PLUGIN_RUN_DIR"], "axolotl.yml")
    lines = [
        "base_model: " + json.dumps(base),
        "datasets:",
        "  - path: " + json.dumps(data_file or dataset),
        "    type: " + json.dumps("completion" if data_file else "alpaca"),
        "adapter: lora",
        "lora_r: 16",
        "lora_alpha: 16",
        "sequence_len: 512",
        "micro_batch_size: 1",
        "num_epochs: 1",
        "max_steps: %d" % steps,
        "output_dir: " + json.dumps(OUT),
        "bf16: auto",
    ]
    with open(yaml_path, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")
    done = subprocess.run(
        [sys.executable, "-m", "axolotl.cli.train", yaml_path],
        capture_output=True,
        text=True,
    )
    if done.returncode != 0:
        detail = (done.stderr or done.stdout or "axolotl failed").strip().splitlines()
        fail(redact(detail[-1] if detail else "axolotl failed"))
    return None


def maybe_push(repo, token):
    if not repo:
        return
    if not repo_id(repo):
        fail("upload repo must look like owner/name")
    if not token:
        fail("uploading needs a saved Hugging Face token")
    from huggingface_hub import HfApi

    HfApi().upload_folder(folder_path=OUT, repo_id=repo, token=token)


def main():
    global STAGE
    if not os.environ.get("PLUGIN_RUN_DIR"):
        sys.stderr.write(
            "This plugin runs only on a Colab runtime started by Colab Playground.\n"
        )
        return 2

    base = os.environ.get("PLUGIN_INPUT_BASE", "").strip()
    dataset = os.environ.get("PLUGIN_INPUT_DATASET", "").strip()
    data_file = os.environ.get("PLUGIN_INPUT_DATA_FILE", "").strip()
    method = os.environ.get("PLUGIN_INPUT_METHOD", "unsloth").strip() or "unsloth"
    steps = integer("PLUGIN_INPUT_MAX_STEPS", 20, 1, 2000)
    integer("PLUGIN_INPUT_EPOCHS", 1, 1, 20)
    push_repo = os.environ.get("PLUGIN_INPUT_PUSH_REPO", "").strip()
    token = os.environ.get("PLUGIN_HF_TOKEN", "")
    if not repo_id(base):
        fail("base model must look like owner/name")
    if method not in METHODS:
        fail("trainer is not one of the choices")
    if data_file:
        root = os.path.realpath(os.environ["PLUGIN_RUN_DIR"])
        real = os.path.realpath(data_file)
        if not (real == root or real.startswith(root + os.sep)) or not os.path.isfile(real):
            fail("the dataset file is not in this run")
        data_file = real
    elif not ident(dataset, True):
        fail("give a dataset id or a dataset file")
    if token:
        os.environ["HF_TOKEN"] = token
        os.environ["HUGGING_FACE_HUB_TOKEN"] = token

    STAGE = "install"
    status("running", STAGE, "installing " + method, 0.1)
    pip_install("unsloth" if method == "unsloth" else "axolotl")
    os.makedirs(OUT, exist_ok=True)

    STAGE = "train"
    status("running", STAGE, "training", 0.4, {"step": 0, "loss": None})
    beat()
    if method == "unsloth":
        loss = train_unsloth(base, dataset, data_file, steps)
    else:
        loss = train_axolotl(base, dataset, data_file, steps)

    STAGE = "save"
    status("running", STAGE, "saving checkpoints", 0.9)
    maybe_push(push_repo, token)
    metrics = {"step": steps}
    if loss is not None:
        metrics["loss"] = loss
    status("ready", STAGE, "checkpoints saved", 1.0, metrics)
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
