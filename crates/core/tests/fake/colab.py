#!/usr/bin/env python3
"""Fake `colab` CLI for the core's integration tests (never the real one).

Mimics the observable behaviour of google-colab-cli 0.8.0 that the core relies
on: global ``--config`` state files and their pruning, ``sessions`` / ``usage``
/ ``new`` / ``exec`` / ``stop`` / ``version`` / ``whoami`` output formats and
exit codes, the copy-paste OAuth prompt, HTTP 400 for unentitled
accelerators, an allocation request Colab never answers, and ``ssh --proxy-mode`` (including its auto-create of unknown
session names and HTTP 429). Snippets sent with ``exec`` are compiled (syntax
errors surface like kernel tracebacks) and the core's known snippets get
simulated answers. State lives under ``$CPG_FAKE_STATE`` (see fake_common).
"""

import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fake_common as fc  # noqa: E402

MARK = "__CPG_RESULT__ "


def out(line=""):
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


def err(line=""):
    sys.stderr.write(line + "\n")
    sys.stderr.flush()


def log_call(argv):
    with open(os.path.join(fc.STATE, "calls.jsonl"), "a") as fh:
        fh.write(json.dumps({"argv": argv, "account": fc.slot_account()}) + "\n")


def auth_prompt_and_read():
    """The CLI's remote OAuth flow: URL on stderr, code from stdin."""
    err("\nTo authorize colab-cli, visit this URL in any browser:\n")
    err("  https://accounts.google.com/o/oauth2/auth?client_id=fake-client&redirect_uri=https%3A%2F%2Fsdk.cloud.google.com%2Fapplicationdefaultauthcode.html&response_type=code\n")
    err("After approving, Google will display an authorization code.")
    sys.stdout.write("Enter the authorization code: ")
    sys.stdout.flush()
    line = sys.stdin.readline()
    if not line:
        err("Traceback (most recent call last):\nEOFError: EOF when reading a line")
        sys.exit(1)
    return line.strip()


def require_account():
    name = fc.slot_account()
    if name is None:
        auth_prompt_and_read()
        err("oauthlib.oauth2.rfc6749.errors.InvalidGrantError: (invalid_grant) Bad Request")
        sys.exit(1)
    return name


def load_state(path):
    try:
        with open(path) as fh:
            text = fh.read()
        return json.loads(text) if text.strip() else {}
    except FileNotFoundError:
        return {}


def save_state(path, records):
    # Like the real CLI, whose lock file open fails when the state file's
    # directory does not exist: do not create the directory here. The app
    # must create it before invoking the CLI.
    with open(path, "w") as fh:
        json.dump(records, fh, indent=2)


def prune(records, assignments):
    pruned = [n for n, r in records.items() if r.get("endpoint") not in assignments]
    for n in pruned:
        del records[n]
    return len(pruned)


def opt(args, *names):
    for n in names:
        if n in args:
            i = args.index(n)
            if i + 1 < len(args):
                return args[i + 1]
    return None


def label(acc):
    return "CPU" if acc == "NONE" else acc


def cmd_sessions(state_path, args):
    name = require_account()
    s = fc.server()
    mine = fc.account_state(s, name)["assignments"]
    records = load_state(state_path)
    n = prune(records, mine)
    if records or n:
        save_state(state_path, records)
    if n:
        out("[colab] Pruned %d stale local session(s)." % n)
    if not mine:
        out("[colab] No active sessions found on server.")
        return 0
    by_ep = {r["endpoint"]: k for k, r in records.items()}
    for ep, a in mine.items():
        shape = "High-RAM" if a.get("shape") == "HIGH_RAM" else "Standard"
        out("[%s] %s | Hardware: %s | Shape: %s | Variant: %s" % (by_ep.get(ep, "?"), ep, label(a["accelerator"]), shape, a.get("variant", "GPU")))
    return 0


def cmd_usage(state_path, args):
    name = require_account()
    u = fc.usage_of(fc.server(), name)
    out("Current balance: %.2f compute units" % u["currentBalance"])
    out("Usage rate: %.2f/hr" % u["consumptionRateHourly"])
    out("Active assignments: %d" % u["assignmentsCount"])
    return 0


def allocate(s, name, session, acc, high_mem):
    s["counter"] += 1
    ep = "gpu-%s-s-%04d" % (acc.lower(), s["counter"]) if acc != "NONE" else "m-s-%04d" % s["counter"]
    vram = {"T4": 15360, "L4": 23034, "G4": 97887, "H100": 81559}.get(acc)
    if acc == "A100":
        seq = s["knobs"]["a100_sequence"]
        idx = s.setdefault("a100_count", 0)
        vram = seq[min(idx, len(seq) - 1)]
        s["a100_count"] = idx + 1
    variant = "GPU" if acc in ("T4", "L4", "G4", "A100", "H100") else ("TPU" if acc.startswith("V") else "DEFAULT")
    fc.account_state(s, name)["assignments"][ep] = {
        "accelerator": acc, "variant": variant, "shape": "HIGH_RAM" if high_mem else "STANDARD",
        "vram": vram, "created": time.time(), "session": session,
    }
    return ep


def cmd_new(state_path, args):
    name = require_account()
    session = opt(args, "-s", "--session") or "s%06x" % int(time.time() * 1000 % 0xFFFFFF)
    gpu, tpu = opt(args, "--gpu"), opt(args, "--tpu")
    if gpu and gpu.upper() not in ("T4", "L4", "G4", "H100", "A100"):
        err("Error: Invalid value for '--gpu': unsupported value %r." % gpu)
        return 2
    acc = (gpu or tpu or "NONE").upper()
    out("[colab] Creating session '%s'..." % session)
    s = fc.server()
    if acc in s["knobs"]["deny"]:
        err("[colab] Backend rejected accelerator '%s'. You may not have quota or entitlement for this accelerator on your account." % acc)
        return 1
    if acc in s["knobs"]["no_answer"]:
        if s["knobs"]["no_answer_hangs"]:
            # Colab allocates a VM but the reply never arrives, so the CLI records
            # no session (the VM is listed as `[?]`) and keeps waiting.
            allocate(s, name, session, acc, "--high-mem" in args)
            fc.save_server(s)
            time.sleep(600)
            return 1
        # How 0.8.0 (Typer with rich) ends once its retries of the request ran out
        # (the traceback box above these lines is left out).
        err("ReadTimeout: HTTPSConnectionPool(host='colab.research.google.com', port=443): ")
        err("Read timed out. (read timeout=120)")
        return 1
    ep = allocate(s, name, session, acc, "--high-mem" in args)
    fc.save_server(s)
    if not fc.knob("skip_state_write"):
        records = load_state(state_path)
        records[session] = {"name": session, "token": "proxy-token", "url": "https://proxy.invalid", "endpoint": ep,
                            "token_expires_at": "2999-01-01T00:00:00Z", "accelerator": acc, "variant": "GPU",
                            "machine_shape": "STANDARD"}
        save_state(state_path, records)
    out("[colab] Session READY.")
    return 0


def lookup(state_path, name, session):
    records = load_state(state_path)
    rec = records.get(session)
    if rec is None:
        out("[colab] Session '%s' not found." % session)
        sys.exit(1)
    s = fc.server()
    mine = fc.account_state(s, name)["assignments"]
    if rec["endpoint"] not in mine:
        out("[colab] Session '%s' appears to be lost (404/401). Cleaning up." % session)
        del records[session]
        save_state(state_path, records)
        sys.exit(1)
    return s, rec["endpoint"], mine[rec["endpoint"]]


def watchdog_reply(wd, now):
    """What the watchdog snippets print for the simulated watchdog state `wd`."""
    return {"now": now, "deadline_epoch": wd.get("deadline_epoch"), "idle_timeout_s": wd.get("idle_timeout_s"),
            "cancelled": bool(wd.get("cancelled")), "runtime_addr": wd.get("runtime_addr", True),
            "alive": bool(wd.get("running"))}


def simulate(code, env, s, ep, a):
    """Answers for the core's known snippets."""
    now = time.time()
    if '"--query-gpu=name,memory.total"' in code:
        names = {"T4": "Tesla T4", "L4": "NVIDIA L4", "H100": "NVIDIA H100 80GB HBM3", "G4": "NVIDIA RTX PRO 6000"}
        gpu = names.get(a["accelerator"])
        if a["accelerator"] == "A100":
            gpu = "NVIDIA A100-SXM4-80GB" if (a.get("vram") or 0) > 50000 else "NVIDIA A100-SXM4-40GB"
        return {"gpu": gpu, "vram_mib": a.get("vram")}
    if "--hooks-stdin" in code:
        # Start: replaces any watchdog; it keeps running unless the knob makes it exit at once.
        dl, idle = env.get("CPG_WD_DEADLINE_S", "none"), env.get("CPG_WD_IDLE_S", "none")
        a["watchdog_starts"] = a.get("watchdog_starts", 0) + 1
        a["watchdog"] = {"deadline_epoch": None if dl == "none" else now + float(dl),
                         "idle_timeout_s": None if idle == "none" else float(idle), "cancelled": False,
                         "running": not s["knobs"]["watchdog_start_fails"],
                         "grace_s": float(env.get("CPG_WD_GRACE_S", "300")), "hooks": len(json.loads(env.get("CPG_WD_HOOKS", "[]")))}
        return watchdog_reply(a["watchdog"], now)
    if "watchdog.json" in code:
        wd = a.get("watchdog")
        cancel, extend = '_cfg["cancelled"] = True' in code, '_cfg["cancelled"] = False' in code
        if cancel or extend:
            if wd is None:
                return None  # the control file is missing: json.load raises, no result line
            if cancel:
                # Like the real watchdog: still running when the reply is printed, it exits
                # without releasing at its next poll (here: before the next command).
                reply = watchdog_reply(dict(wd, cancelled=True), now)
                wd["cancelled"], wd["running"] = True, False
                return reply
            dl = env.get("CPG_WD_DEADLINE_S", "none")
            wd["deadline_epoch"] = None if dl == "none" else now + float(dl)
            wd["cancelled"] = False
            return watchdog_reply(wd, now)
        # Status: a missing control file reads as cancelled, with nothing running.
        return watchdog_reply(wd if wd is not None else {"cancelled": True, "runtime_addr": False}, now)
    if "PLUGIN_RUN_DIR" in code:
        run_id = env["CPG_RUN_ID"]
        s["runs"][run_id] = {"endpoint": ep, "polls": 0, "killed": False,
                             "env": {k: v for k, v in env.items() if k.startswith("PLUGIN_")}}
        return {"pid": 4242, "run_dir": "/root/.colab-playground/runs/" + run_id}
    if "CPG_LOG_OFFSET" in code:
        run = s["runs"].get(env.get("CPG_RUN_ID", ""))
        if run is None:
            return {"status": None, "alive": False, "log": "", "offset": 0}
        n = run["polls"]
        run["polls"] = n + 1
        stages = s["knobs"]["plugin_stages"]
        fail = s["knobs"]["plugin_fail_stage"]
        secret = run["env"].get("PLUGIN_HF_TOKEN") or run["env"].get("PLUGIN_INPUT_API_KEY") or ""
        log = "poll %d (debug secret=%s)\n" % (n, secret)
        if run["killed"]:
            return {"status": {"state": "failed", "stage": None, "message": "stopped"}, "alive": False, "log": log, "offset": n + 1}
        if n < len(stages):
            st = {"state": "running", "stage": stages[n], "progress": 0.5, "message": "working on " + stages[n]}
            if stages[n] == fail:
                st = {"state": "failed", "stage": stages[n], "message": "simulated failure (token %s)" % secret}
            return {"status": st, "alive": True, "log": log, "offset": n + 1}
        ready = {"state": "ready", "stage": stages[-1], "progress": 1.0, "message": "ready",
                 "outputs": {"model_id": "fake-model", "quant": "Q8_0" if (a.get("vram") or 0) > 70000 else "Q4_K_M"}}
        return {"status": ready, "alive": True, "log": log, "offset": n + 1}
    if "os.killpg" in code:
        run = s["runs"].get(env.get("CPG_RUN_ID", ""))
        if run:
            run["killed"] = True
        return {"signalled": bool(run)}
    return None


def cmd_exec(state_path, args):
    name = require_account()
    session = opt(args, "-s", "--session")
    env = {}
    i = 0
    while i < len(args):
        if args[i] == "--env" and i + 1 < len(args):
            k, _, v = args[i + 1].partition("=")
            if not re.match(r"^[A-Za-z_][A-Za-z0-9_]*$", k) or "=" not in args[i + 1]:
                err("[colab] Invalid --env value %r. Expected KEY=VALUE." % args[i + 1])
                return 2
            env[k] = v
            i += 2
        else:
            i += 1
    s, ep, a = lookup(state_path, name, session)
    code = sys.stdin.read()
    if not code.strip():
        return 0
    try:
        compile(code, "<cell>", "exec")
    except SyntaxError as exc:
        err("Traceback (most recent call last):\nSyntaxError: %s" % exc)
        return 0
    result = simulate(code, env, s, ep, a)
    fc.save_server(s)
    if result is not None:
        out(MARK + json.dumps(result))
    record_history(session, env, code, result)
    return 0


def record_history(session, env, code, result):
    """Like the CLI's HistoryLogger: append the executed cell, including the
    os.environ prelude built from --env, to ~/.config/colab-cli/history."""
    prelude = ""
    if env:
        prelude = "import os\n" + "\n".join("os.environ[%r] = %r" % (k, v) for k, v in env.items()) + "\n"
    outputs = [{"output_type": "stream", "name": "stdout", "text": MARK + json.dumps(result) + "\n"}] if result is not None else []
    event = {"timestamp": time.strftime("%Y-%m-%dT%H:%M:%S+00:00", time.gmtime()), "event_type": "execution",
             "code": prelude + code, "outputs": outputs, "cell_index": None, "cell_id": None}
    directory = os.path.expanduser("~/.config/colab-cli/history")
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, session + ".jsonl")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    with os.fdopen(fd, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(event) + "\n")


def cmd_stop(state_path, args):
    name = require_account()
    session = opt(args, "-s", "--session")
    if fc.knob("stop_noop"):
        # Acknowledged but never carried out: the assignment and the local
        # record both survive, so only a fresh listing reveals the truth.
        out("[colab] Session terminated.")
        return 0
    records = load_state(state_path)
    rec = records.get(session)
    if rec is None:
        out("[colab] Session '%s' not found." % session)
        return 0
    out("[colab] Stopping session '%s'..." % session)
    s = fc.server()
    mine = fc.account_state(s, name)["assignments"]
    # Like the CLI: a record without token expiry is refreshed from the
    # server; an endpoint not assigned to this account is pruned, not stopped.
    if rec["endpoint"] not in mine:
        del records[session]
        save_state(state_path, records)
        out("[colab] Session '%s' not found." % session)
        return 0
    del mine[rec["endpoint"]]
    fc.save_server(s)
    del records[session]
    save_state(state_path, records)
    out("[colab] Session terminated.")
    return 0


def cmd_whoami(state_path, args):
    name = fc.slot_account()
    if name is None:
        code = auth_prompt_and_read()
        if code != fc.knob("good_code"):
            err("Traceback (most recent call last):")
            err("oauthlib.oauth2.rfc6749.errors.InvalidGrantError: (invalid_grant) Malformed auth code.")
            return 1
        name = fc.knob("login_name")
        fc.kr_set("google-colab-cli", "oauth_token", fc.token_for(name))
    out("Auth provider: oauth2")
    out("Email:         %s@example.com" % name)
    return 0


def cmd_ssh(state_path, args):
    if "--proxy-mode" not in args:
        err("fake colab: only --proxy-mode is supported")
        return 2
    name = require_account()
    session = opt(args, "-s", "--session")
    records = load_state(state_path)
    s = fc.server()
    if session not in records:
        # Real CLI behaviour: `--proxy-mode -s NAME` creates a missing session.
        ep = allocate(s, name, session, "NONE", False)
        s.setdefault("autocreated", []).append(session)
        fc.save_server(s)
        records[session] = {"name": session, "token": "t", "url": "u", "endpoint": ep}
        save_state(state_path, records)
    if s["knobs"]["ssh_busy"]:
        err("[colab] Already-active SSH session (HTTP 429): another `colab ssh` is connected to this runtime. Disconnect it and retry.")
        return 1
    # Bridge: stay connected until ssh closes our stdin.
    while sys.stdin.buffer.read(1024):
        pass
    return 0


def main():
    argv = sys.argv[1:]
    log_call(argv)
    state_path = os.path.expanduser("~/.config/colab-cli/sessions.json")
    if argv[:1] == ["--config"]:
        state_path = argv[1]
        argv = argv[2:]
    if not argv:
        err("Usage: colab [OPTIONS] COMMAND [ARGS]...")
        return 2
    cmd, args = argv[0], argv[1:]
    if cmd == "version":
        out("Version: 0.8.0-fake")
        return 0
    handlers = {"sessions": cmd_sessions, "usage": cmd_usage, "new": cmd_new, "exec": cmd_exec,
                "stop": cmd_stop, "whoami": cmd_whoami, "ssh": cmd_ssh}
    if cmd not in handlers:
        err("Error: No such command '%s'." % cmd)
        return 2
    return handlers[cmd](state_path, args)


if __name__ == "__main__":
    sys.exit(main())
