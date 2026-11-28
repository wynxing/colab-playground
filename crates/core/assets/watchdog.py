#!/usr/bin/env python3
"""Colab Playground VM watchdog (stdlib only).

Started detached on the Colab VM right after provisioning. It enforces an
absolute deadline and an optional idle rule, then releases the runtime with
``POST http://$TBE_RUNTIME_ADDR/unassign`` (what
``google.colab.runtime.unassign()`` does).

Everything is driven by a small JSON config file that the app rewrites through
``colab exec`` to extend or cancel the deadline:

    {
      "deadline_epoch": 1767225600.0 | null,   # absolute, VM clock
      "idle_timeout_s": 1800 | null,           # no heartbeat AND idle GPU
      "cancelled": false,                      # true: exit without releasing
      "runtime_addr": "172.28.0.12:9000",      # captured from the kernel env
      "heartbeat": "/root/.colab-playground/heartbeat",
      "log": "/root/.colab-playground/watchdog.log",
      "poll_s": 15,
      "grace_s": 300,                          # wind-down starts this long before the deadline
      "started": 1767218400.0
    }

Wind-down: from ``deadline - grace_s`` the watchdog writes a ``winddown``
marker file next to the config, runs the ordered hook commands it received on
stdin at start (``--hooks-stdin``: a JSON list of ``{"argv": [...], "env":
{...}, "timeout_s": N}``; kept in memory only, never written to disk) and logs
each result. The runtime is still released at the deadline itself. Extending
the deadline before it passes resets the wind-down; cancelling removes it.

Idle means: the heartbeat file (touched by plugins) has not changed for
``idle_timeout_s`` *and* no ``nvidia-smi`` sample in that window showed GPU
utilisation of 5 % or more. On machines without ``nvidia-smi`` only the
heartbeat counts.
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time
import urllib.request

GPU_BUSY_PERCENT = 5
RELEASE_ATTEMPTS = 3


class Watchdog:
    def __init__(self, config_path, hooks=None):
        self.config_path = config_path
        self.hooks = hooks or []
        self.winddown_for = None
        self.marker = os.path.join(os.path.dirname(os.path.abspath(config_path)), "winddown")
        self.config = {}
        self.log_path = None
        self.last_gpu_busy = 0.0
        self.started = time.time()
        self.last_periodic = {}
        self.active_periodic = {}

    def log(self, message):
        line = "%s %s\n" % (time.strftime("%Y-%m-%dT%H:%M:%S%z"), message)
        try:
            with open(self.log_path or "/tmp/colab-playground-watchdog.log", "a") as fh:
                fh.write(line)
        except OSError:
            pass

    def load(self):
        try:
            with open(self.config_path) as fh:
                value = json.load(fh)
            if isinstance(value, dict):
                self.config = value
        except (OSError, ValueError) as exc:
            self.log("config unreadable (%s); keeping previous settings" % type(exc).__name__)
        self.log_path = self.config.get("log") or self.log_path
        started = self.config.get("started")
        if isinstance(started, (int, float)):
            self.started = float(started)

    def gpu_utilisation(self):
        try:
            out = subprocess.run(
                ["nvidia-smi", "--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"],
                capture_output=True, text=True, timeout=20, check=True,
            ).stdout
        except Exception:  # noqa: BLE001
            return None
        values = []
        for line in out.splitlines():
            try:
                values.append(float(line.strip()))
            except ValueError:
                pass
        return max(values) if values else None

    def runtime_addr(self):
        addr = self.config.get("runtime_addr") or os.environ.get("TBE_RUNTIME_ADDR")
        if addr:
            return addr
        # Fall back to any process environment that carries it (the kernel's).
        for pid in os.listdir("/proc"):
            if not pid.isdigit():
                continue
            try:
                with open("/proc/%s/environ" % pid, "rb") as fh:
                    for item in fh.read().split(b"\0"):
                        if item.startswith(b"TBE_RUNTIME_ADDR="):
                            return item.split(b"=", 1)[1].decode()
            except OSError:
                continue
        return None

    def release(self, reason):
        addr = self.runtime_addr()
        if not addr:
            self.log("cannot release (%s): TBE_RUNTIME_ADDR unknown" % reason)
            return False
        for attempt in range(1, RELEASE_ATTEMPTS + 1):
            try:
                request = urllib.request.Request("http://%s/unassign" % addr, data=b"", method="POST")
                with urllib.request.urlopen(request, timeout=30) as response:
                    status = response.status
                if status == 200:
                    self.log("released runtime (%s)" % reason)
                    return True
                self.log("unassign returned HTTP %d (attempt %d)" % (status, attempt))
            except Exception as exc:  # noqa: BLE001
                self.log("unassign failed (%s, attempt %d)" % (type(exc).__name__, attempt))
            time.sleep(10)
        return False

    def idle_for(self, now):
        heartbeat = self.config.get("heartbeat")
        last = self.started
        if heartbeat:
            try:
                last = max(last, os.path.getmtime(heartbeat))
            except OSError:
                pass
        util = self.gpu_utilisation()
        if util is not None and util >= GPU_BUSY_PERCENT:
            self.last_gpu_busy = now
        return now - max(last, self.last_gpu_busy)

    def clear_winddown(self):
        self.winddown_for = None
        try:
            os.remove(self.marker)
        except OSError:
            pass

    def run_hook(self, i, hook, timeout):
        argv = hook.get("argv") if isinstance(hook, dict) else None
        if not argv or not all(isinstance(a, str) for a in argv):
            self.log("hook %d skipped: invalid argv" % i)
            return
        env = dict(os.environ)
        env.update({k: str(v) for k, v in (hook.get("env") or {}).items()})
        try:
            rc = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, timeout=timeout).returncode
            self.log("hook %d (%s) exited %d" % (i, os.path.basename(argv[0]), rc))
        except subprocess.TimeoutExpired:
            self.log("hook %d (%s) timed out after %ds" % (i, os.path.basename(argv[0]), timeout))
        except OSError as exc:
            self.log("hook %d (%s) failed to start (%s)" % (i, os.path.basename(argv[0]), type(exc).__name__))

    def stop_active_periodic(self):
        for i, (proc, _deadline_ts, _timeout, argv0) in list(self.active_periodic.items()):
            rc = proc.poll()
            if rc is None:
                try:
                    proc.terminate()
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
                self.log("periodic hook %d (%s) stopped for wind-down/exit" % (i, argv0))
            else:
                self.log("hook %d (%s) exited %d" % (i, argv0, rc))
            self.last_periodic[i] = time.time()
            del self.active_periodic[i]

    def winddown(self, deadline):
        self.stop_active_periodic()
        self.winddown_for = deadline
        try:
            with open(self.marker, "w") as fh:
                json.dump({"deadline_epoch": deadline, "started": time.time()}, fh)
        except OSError:
            pass
        self.log("wind-down started (%d hook(s))" % len(self.hooks))
        for i, hook in enumerate(self.hooks):
            budget = max(1.0, deadline - time.time())
            timeout = min(budget, float(hook.get("timeout_s") or budget))
            self.run_hook(i, hook, timeout)

    def poll_active_periodic(self, now):
        for i, (proc, deadline_ts, timeout, argv0) in list(self.active_periodic.items()):
            rc = proc.poll()
            if rc is not None:
                self.log("hook %d (%s) exited %d" % (i, argv0, rc))
                self.last_periodic[i] = now
                del self.active_periodic[i]
            elif now >= deadline_ts:
                try:
                    proc.terminate()
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
                self.log("hook %d (%s) timed out after %ds" % (i, argv0, timeout))
                self.last_periodic[i] = now
                del self.active_periodic[i]

    def run_periodic(self, now, deadline):
        self.poll_active_periodic(now)
        for i, hook in enumerate(self.hooks):
            if not isinstance(hook, dict) or i in self.active_periodic:
                continue
            interval = hook.get("interval_s")
            if not isinstance(interval, (int, float)) or interval <= 0:
                continue
            last = self.last_periodic.get(i, self.started)
            if now - last < interval:
                continue
            argv = hook.get("argv")
            if not argv or not all(isinstance(a, str) for a in argv):
                self.log("hook %d skipped: invalid argv" % i)
                self.last_periodic[i] = now
                continue
            timeout = float(hook.get("timeout_s") or 240)
            if isinstance(deadline, (int, float)):
                budget = max(1.0, deadline - time.time())
                timeout = min(timeout, budget)
            env = dict(os.environ)
            env.update({k: str(v) for k, v in (hook.get("env") or {}).items()})
            try:
                proc = subprocess.Popen(
                    argv,
                    env=env,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                self.active_periodic[i] = (proc, now + timeout, timeout, os.path.basename(argv[0]))
            except OSError as exc:
                self.log("hook %d (%s) failed to start (%s)" % (i, os.path.basename(argv[0]), type(exc).__name__))
                self.last_periodic[i] = now

    def run(self):
        self.load()
        self.log("watchdog started (pid %d)" % os.getpid())
        while True:
            self.load()
            if self.config.get("cancelled"):
                self.stop_active_periodic()
                self.clear_winddown()
                self.log("cancelled; exiting without releasing")
                return 0
            now = time.time()
            deadline = self.config.get("deadline_epoch")
            grace = self.config.get("grace_s", 300)
            grace = grace if isinstance(grace, (int, float)) and grace >= 0 else 300
            if not isinstance(deadline, (int, float)):
                if self.winddown_for is not None:
                    self.clear_winddown()
            else:
                if self.winddown_for is not None and deadline != self.winddown_for and now < deadline - grace:
                    self.log("deadline extended; wind-down reset")
                    self.clear_winddown()
                if self.winddown_for is None and now >= deadline - grace and now < deadline:
                    self.winddown(deadline)
                    continue
            if self.winddown_for is None:
                self.run_periodic(now, deadline)
            if isinstance(deadline, (int, float)) and now >= deadline:
                self.stop_active_periodic()
                if self.release("deadline"):
                    return 0
            idle = self.config.get("idle_timeout_s")
            if isinstance(idle, (int, float)) and idle > 0:
                idle_s = self.idle_for(now)
                if idle_s >= idle:
                    if self.hooks and self.winddown_for is None:
                        self.winddown(time.time() + grace)
                    self.stop_active_periodic()
                    if self.release("idle for %ds" % idle_s):
                        return 0
            poll = self.config.get("poll_s", 15)
            time.sleep(max(5, min(60, poll if isinstance(poll, (int, float)) else 15)))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", required=True)
    parser.add_argument("--hooks-stdin", action="store_true", help="read wind-down hooks (JSON) from stdin")
    args = parser.parse_args()
    hooks = []
    if args.hooks_stdin:
        try:
            value = json.loads(sys.stdin.read() or "[]")
            hooks = value if isinstance(value, list) else []
        except ValueError:
            hooks = []
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    pid_path = os.path.join(os.path.dirname(os.path.abspath(args.config)), "watchdog.pid")
    try:
        with open(pid_path, "w") as fh:
            fh.write(str(os.getpid()))
    except OSError:
        pass
    return Watchdog(args.config, hooks).run()


if __name__ == "__main__":
    sys.exit(main())
