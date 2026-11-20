#!/usr/bin/env python3
"""Colab Playground account helper.

Runs with the Colab CLI's own interpreter (it needs that interpreter's
``keyring`` and ``google-auth``) and is fed on stdin:

    <cli python> - <op> [args...]

It is started through a fixed loader (``python -c <loader> <op> [args...]``)
that reads ``<length>\n<source>`` from stdin; the rest of stdin is available
to operations that take a value (``secret-set``).

It manages the CLI's single OAuth credential slot (keyring service
``google-colab-cli``, user ``oauth_token``) and a vault of saved credentials in
the same keyring (service ``colab-playground``, users ``account:<id>`` plus the
``pending-add`` backup).

Contract: exactly one line ``__CPG_RESULT__ <json>`` on stdout. Google tokens
never leave this process: no token, refresh token or exception text that could
contain one is ever printed. Error details carry only exception type names and
HTTP status codes. App secrets (``secret:<name>``, e.g. a Hugging Face token)
are the one exception: ``secret-get`` returns the value so the app can pass it
to ``colab exec --env``; it is never logged or written to disk.

Mechanism adapted from github.com/mizorewww/colab-account-changer (MIT).
"""

import json
import os
import re
import sys
import threading

COLAB_SERVICE = "google-colab-cli"
COLAB_USER = "oauth_token"
VAULT_SERVICE = "colab-playground"
PENDING_USER = "pending-add"
NO_TOKEN_SENTINEL = '{"colab_playground_no_token": true}'
TOKENINFO_URL = "https://oauth2.googleapis.com/tokeninfo"
CCU_INFO_URL = "https://colab.research.google.com/tun/m/ccu-info"
XSSI_PREFIX = ")]}'\n"
LEGACY_TOKEN_PATH = os.path.expanduser("~/.config/colab-cli/token.json")
READ_TIMEOUT_S = 30.0
MARKER = "__CPG_RESULT__ "
ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,31}$")
ENDPOINT_RE = re.compile(r"^[A-Za-z0-9._:-]{1,256}$")
SECRET_RE = re.compile(r"^[a-z][a-z0-9_]{0,31}$")
MAX_SECRET_BYTES = 16384
# Where the Colab CLI appends every executed cell (its HistoryLogger default;
# independent of --config). Expanded with the same HOME as the CLI.
CLI_HISTORY_DIR = "~/.config/colab-cli/history"
SESSION_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$")
MIN_REDACT_LEN = 4


class Fail(Exception):
    """An expected failure with a machine-readable kind."""

    def __init__(self, kind, detail="", field=None):
        super().__init__(kind)
        self.kind = kind
        self.detail = detail
        self.field = field


def emit(obj):
    sys.stdout.write(MARKER + json.dumps(obj) + "\n")
    sys.stdout.flush()


# ---------------------------------------------------------------- keyring

_daemon_start_attempted = False
_daemon_start_outcome = None


def _try_start_keyring_daemon():
    """Starts the Secret Service daemon once per process, best effort.

    Returns "started" (the binary was found and run), "not_installed"
    (Linux without gnome-keyring-daemon on PATH) or "unavailable" (not
    Linux). Never raises: a failed start must not mask the keyring error
    that prompted it. The daemon keeps running in the user's session
    afterwards, so later helper processes find the service up.
    """
    global _daemon_start_attempted, _daemon_start_outcome
    if _daemon_start_attempted:
        return _daemon_start_outcome
    _daemon_start_attempted = True
    _daemon_start_outcome = "unavailable"
    if not sys.platform.startswith("linux"):
        return _daemon_start_outcome
    try:
        import shutil  # noqa: PLC0415
        import subprocess  # noqa: PLC0415

        path = shutil.which("gnome-keyring-daemon")
        if path is None:
            _daemon_start_outcome = "not_installed"
            return _daemon_start_outcome
        _daemon_start_outcome = "started"
        try:
            subprocess.run(
                [path, "--start", "--components=secrets"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=10,
                check=False,
            )
        except Exception:  # noqa: BLE001
            pass
    except Exception:  # noqa: BLE001
        pass
    return _daemon_start_outcome


def _is_service_unavailable(exc):
    """True when a keyring call failed because the Secret Service is down.

    Matched on exception type names and modules of the secretstorage/dbus
    stack only, so an unrelated failure (a locked keyring, a rejected
    value) never triggers the daemon start or a retry.
    """
    if type(exc).__name__ in ("SecretServiceNotAvailableException", "DBusException"):
        return True
    module = type(exc).__module__ or ""
    return module.startswith("secretstorage") or module.startswith("dbus")


def _unavailable_fail(action, exc, outcome):
    """The Fail for a keyring operation that did not succeed.

    A missing daemon binary is its own kind so the app can show the
    install command instead of unlock guidance that cannot help there.
    """
    if outcome == "not_installed":
        return Fail("keyring_not_installed", "gnome-keyring-daemon not installed")
    return Fail("keyring_unavailable", "keyring %s failed (%s)" % (action, type(exc).__name__))


def _backend_is_fail(keyring):
    backend = keyring.get_keyring()
    module = type(backend).__module__ or ""
    return module.endswith(".fail") or type(backend).__name__ == "FailKeyring"


def _reresolve_backend(keyring):
    # keyring caches the backend it first resolved for the whole process,
    # so without clearing the cache a daemon started now could only ever
    # help the next helper process. The attribute is private; if it is not
    # there, re-resolution returns the cached backend and the start still
    # pays off on the next run.
    try:
        from keyring import core as keyring_core  # noqa: PLC0415

        keyring_core._keyring_backend = None
    except Exception:  # noqa: BLE001
        pass


def _keyring():
    try:
        import keyring  # noqa: PLC0415
    except Exception as exc:  # noqa: BLE001
        raise Fail("keyring_unavailable", "import keyring failed (%s)" % type(exc).__name__)
    try:
        fail = _backend_is_fail(keyring)
    except Exception as exc:  # noqa: BLE001
        raise Fail("keyring_unavailable", "no keyring backend (%s)" % type(exc).__name__)
    if fail:
        # On headless Linux the usual cause is a Secret Service daemon
        # that was never started, which leaves keyring with only its fail
        # backend; starting the daemon can give it a real one.
        outcome = _try_start_keyring_daemon()
        _reresolve_backend(keyring)
        try:
            fail = _backend_is_fail(keyring)
        except Exception:  # noqa: BLE001
            fail = True
        if fail:
            if outcome == "not_installed":
                raise Fail("keyring_not_installed", "gnome-keyring-daemon not installed")
            raise Fail("keyring_unavailable", "no usable keyring backend")
    return keyring


def _delete_error_types(keyring):
    try:
        from keyring.errors import PasswordDeleteError  # noqa: PLC0415

        return (PasswordDeleteError,)
    except Exception:  # noqa: BLE001
        return ()


def kr_get(service, user):
    keyring = _keyring()

    def attempt():
        result = []
        errors = []

        def run():
            try:
                result.append(keyring.get_password(service, user))
            except BaseException as exc:  # noqa: BLE001
                errors.append(exc)

        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        thread.join(READ_TIMEOUT_S)
        if thread.is_alive():
            raise Fail("keyring_unavailable", "keyring read timed out (locked?)")
        return (result[0] if result else None), (errors[0] if errors else None)

    value, error = attempt()
    if error is not None and _is_service_unavailable(error):
        # The read itself is threaded because a locked keyring can block
        # forever; a service that is simply not running fails fast, and one
        # daemon start plus a single retry is enough to outlive it.
        outcome = _try_start_keyring_daemon()
        value, error = attempt()
        if error is not None:
            raise _unavailable_fail("read", error, outcome)
    if error is not None:
        raise Fail("keyring_unavailable", "keyring read failed (%s)" % type(error).__name__)
    return value


def kr_set(service, user, value):
    # Mutations are synchronous: a timed-out write could land after a rollback.
    # Retrying after a raise is safe for the same reason: the first attempt
    # raised before anything landed, so the value is still written at most once.
    keyring = _keyring()
    try:
        keyring.set_password(service, user, value)
    except Exception as exc:  # noqa: BLE001
        if not _is_service_unavailable(exc):
            raise Fail("keyring_unavailable", "keyring write failed (%s)" % type(exc).__name__)
        outcome = _try_start_keyring_daemon()
        try:
            keyring.set_password(service, user, value)
        except Exception as exc2:  # noqa: BLE001
            raise _unavailable_fail("write", exc2, outcome)


def kr_delete(service, user):
    keyring = _keyring()
    ignorable = _delete_error_types(keyring)

    def attempt():
        try:
            keyring.delete_password(service, user)
        except ignorable:
            pass

    try:
        attempt()
    except Exception as exc:  # noqa: BLE001
        if not _is_service_unavailable(exc):
            raise Fail("keyring_unavailable", "keyring delete failed (%s)" % type(exc).__name__)
        outcome = _try_start_keyring_daemon()
        try:
            attempt()
        except Exception as exc2:  # noqa: BLE001
            raise _unavailable_fail("delete", exc2, outcome)


def kr_set_verified(service, user, value):
    kr_set(service, user, value)
    if kr_get(service, user) != value:
        raise Fail("keyring_unavailable", "keyring write could not be verified")


def account_user(account_id):
    if not ID_RE.match(account_id or ""):
        raise Fail("invalid_input", "invalid account id", field="account")
    return "account:" + account_id


def read_slot():
    return kr_get(COLAB_SERVICE, COLAB_USER)


def read_account(account_id):
    return kr_get(VAULT_SERVICE, account_user(account_id))


# ---------------------------------------------------------------- tokens


def parse_token(raw):
    try:
        value = json.loads(raw)
    except Exception:  # noqa: BLE001
        raise Fail("login_verification_failed", "credential is not JSON")
    if not isinstance(value, dict):
        raise Fail("login_verification_failed", "credential is not an object")
    for field in ("refresh_token", "client_id", "client_secret", "token_uri"):
        if not isinstance(value.get(field), str) or not value[field]:
            raise Fail("login_verification_failed", "credential has no %s" % field)
    return value


def same_identity(a_raw, b_raw):
    """True when two credentials share a non-empty refresh token and client."""
    try:
        a = json.loads(a_raw)
        b = json.loads(b_raw)
    except Exception:  # noqa: BLE001
        return False
    if not isinstance(a, dict) or not isinstance(b, dict):
        return False
    rt = a.get("refresh_token")
    return isinstance(rt, str) and bool(rt) and rt == b.get("refresh_token") and a.get("client_id") == b.get("client_id")


def credentials(raw):
    from google.oauth2.credentials import Credentials  # noqa: PLC0415

    return Credentials.from_authorized_user_info(parse_token(raw))


def refreshed(raw):
    """Returns (credentials, token_json) with a valid access token."""
    creds = credentials(raw)
    if not creds.valid:
        from google.auth.transport.requests import Request  # noqa: PLC0415

        try:
            creds.refresh(Request())
        except Exception as exc:  # noqa: BLE001
            raise Fail("login_verification_failed", "token refresh failed (%s)" % type(exc).__name__)
        return creds, creds.to_json()
    return creds, raw


def tokeninfo_email(access_token):
    import requests  # noqa: PLC0415

    try:
        response = requests.get(TOKENINFO_URL, params={"access_token": access_token}, timeout=10)
    except Exception as exc:  # noqa: BLE001
        raise Fail("http", "tokeninfo request failed (%s)" % type(exc).__name__)
    if response.status_code != 200:
        raise Fail("login_verification_failed", "tokeninfo returned HTTP %d" % response.status_code)
    try:
        email = response.json().get("email")
    except Exception:  # noqa: BLE001
        raise Fail("login_verification_failed", "tokeninfo returned invalid JSON")
    if not isinstance(email, str) or not email:
        raise Fail("login_verification_failed", "token has no verified email")
    return email


def inspect(raw):
    """Validates, refreshes if needed and resolves the email of a credential."""
    creds, token_json = refreshed(raw)
    if not creds.token:
        raise Fail("login_verification_failed", "no access token after refresh")
    return token_json, tokeninfo_email(creds.token)


def match_known(raw, known):
    if not raw:
        return None
    for account_id in known:
        saved = read_account(account_id)
        if saved and (saved == raw or same_identity(saved, raw)):
            return account_id
    return None


def pending_backup():
    return kr_get(VAULT_SERVICE, PENDING_USER)


# ---------------------------------------------------------------- operations


def op_status(args):
    raw = read_slot()
    result = {
        "has_token": bool(raw),
        "email": None,
        "matches": match_known(raw, args),
        "pending_add": pending_backup() is not None,
        "legacy_token": os.path.exists(LEGACY_TOKEN_PATH),
    }
    if raw:
        try:
            _, result["email"] = inspect(raw)
        except Fail as exc:
            result["email_error"] = exc.kind
    return result


def op_list(args):
    return {"accounts": [{"id": a, "stored": read_account(a) is not None} for a in args]}


def op_adopt(args):
    if len(args) != 1:
        raise Fail("invalid_input", "adopt <id>", field="account")
    user = account_user(args[0])
    raw = read_slot()
    if not raw:
        raise Fail("auth_required", "the CLI has no credential")
    token_json, email = inspect(raw)
    kr_set_verified(VAULT_SERVICE, user, token_json)
    if token_json != raw:
        kr_set_verified(COLAB_SERVICE, COLAB_USER, token_json)
    return {"email": email}


def op_use(args):
    if not args:
        raise Fail("invalid_input", "use <id> [known...]", field="account")
    target_id, known = args[0], args[1:]
    target_user = account_user(target_id)
    if pending_backup() is not None:
        raise Fail("add_in_progress", "an add-account flow is pending")
    target_raw = read_account(target_id)
    if not target_raw:
        raise Fail("account_not_found", "no saved credential", field="account")
    original = read_slot()
    current_id = match_known(original, known) if original else None
    if original and current_id is None:
        raise Fail("active_account_not_adopted", "save the current credential first")
    if current_id == target_id:
        token_json, email = inspect(original)
        kr_set_verified(VAULT_SERVICE, target_user, token_json)
        return {"email": email}
    token_json, email = inspect(target_raw)
    if token_json != target_raw:
        kr_set_verified(VAULT_SERVICE, target_user, token_json)
    if original and current_id:
        # Keep the outgoing account's (possibly refreshed) credential.
        kr_set_verified(VAULT_SERVICE, account_user(current_id), original)
    try:
        kr_set_verified(COLAB_SERVICE, COLAB_USER, token_json)
    except BaseException:
        if original:
            kr_set(COLAB_SERVICE, COLAB_USER, original)
        raise
    return {"email": email}


def op_remove(args):
    if not args:
        raise Fail("invalid_input", "remove <id> [known...]", field="account")
    user = account_user(args[0])
    raw = read_slot()
    saved = read_account(args[0])
    if raw and saved and (raw == saved or same_identity(raw, saved)):
        # Removing a duplicate of the active credential is fine as long as
        # another saved account still holds it.
        if match_known(raw, [k for k in args[1:] if k != args[0]]) is None:
            raise Fail("invalid_input", "cannot remove the active account", field="account")
    kr_delete(VAULT_SERVICE, user)
    return {}


def usage_of(account_id, slot_raw):
    creds = credential_for(account_id, slot_raw)
    from google.auth.transport.requests import AuthorizedSession  # noqa: PLC0415

    try:
        response = AuthorizedSession(creds).get(
            CCU_INFO_URL,
            headers={"Accept": "application/json", "X-Colab-Client-Agent": "colab-cli"},
            params={"authuser": "0"},
            timeout=15,
        )
    except Exception as exc:  # noqa: BLE001
        raise Fail("http", "usage request failed (%s)" % type(exc).__name__)
    if response.status_code != 200:
        raise Fail("http", "usage request returned HTTP %d" % response.status_code)
    text = response.text
    if text.startswith(XSSI_PREFIX):
        text = text[len(XSSI_PREFIX):]
    try:
        value = json.loads(text)
        balance = float(value.get("currentBalance", value.get("paidComputeUnitsBalance")))
        rate = float(value["consumptionRateHourly"])
        assignments = int(value["assignmentsCount"])
    except Exception:  # noqa: BLE001
        raise Fail("parse", "unexpected usage response")
    return {"balance": balance, "rate": rate, "assignments": assignments}


def op_usage(args):
    """usage <id>...: ccu-info for several saved accounts without switching."""
    if not args:
        raise Fail("invalid_input", "usage <id>...", field="account")
    slot_raw = read_slot()
    results = []
    for account_id in args:
        try:
            item = usage_of(account_id, slot_raw)
            item["ok"] = True
        except Fail as exc:
            item = {"ok": False, "error": {"kind": exc.kind, "detail": exc.detail}}
        except Exception as exc:  # noqa: BLE001
            item = {"ok": False, "error": {"kind": "other", "detail": type(exc).__name__}}
        item["id"] = account_id
        results.append(item)
    return {"accounts": results}


def credential_for(account_id, slot_raw):
    """Fresh credentials for a saved account (the slot's copy if it is active)."""
    user = account_user(account_id)
    use_slot = slot_raw is not None and match_known(slot_raw, [account_id]) == account_id
    source = slot_raw if use_slot else read_account(account_id)
    if not source:
        raise Fail("account_not_found", "no saved credential", field="account")
    creds, token_json = refreshed(source)
    if token_json != source:
        if use_slot:
            kr_set_verified(COLAB_SERVICE, COLAB_USER, token_json)
        kr_set_verified(VAULT_SERVICE, user, token_json)
    return creds


def colab_client(creds):
    """The CLI's own API client. Its internals are not a public API: any
    import or shape mismatch is reported as unsupported_cli_version."""
    try:
        from colab_cli.client import Client, Prod  # noqa: PLC0415
        from google.auth.transport.requests import AuthorizedSession  # noqa: PLC0415

        return Client(Prod(), AuthorizedSession(creds))
    except Exception as exc:  # noqa: BLE001
        raise Fail("unsupported_cli_version", "colab_cli.client unavailable (%s)" % type(exc).__name__)


def _status_of(exc):
    response = getattr(exc, "response", None)
    return getattr(response, "status_code", None) or getattr(exc, "status_code", None)


def _name(value):
    for attr in ("name", "value"):
        v = getattr(value, attr, None)
        if isinstance(v, str):
            return v
    return str(value)


def assignments_of(account_id, slot_raw):
    client = colab_client(credential_for(account_id, slot_raw))
    try:
        listed = client.list_assignments()
    except Exception as exc:  # noqa: BLE001
        status = _status_of(exc)
        if status is not None:
            raise Fail("http", "listing runtimes returned HTTP %s" % status)
        if type(exc).__name__ in ("ValidationError", "AttributeError", "TypeError"):
            raise Fail("unsupported_cli_version", "unexpected assignment shape (%s)" % type(exc).__name__)
        raise Fail("http", "listing runtimes failed (%s)" % type(exc).__name__)
    out = []
    try:
        for a in listed:
            # Never include runtime_proxy_info: it carries a token.
            out.append({
                "endpoint": str(a.endpoint),
                "accelerator": _name(a.accelerator),
                "variant": _name(a.variant),
                "shape": _name(a.machine_shape),
            })
    except Exception as exc:  # noqa: BLE001
        raise Fail("unsupported_cli_version", "unexpected assignment shape (%s)" % type(exc).__name__)
    return out


def op_assignments(args):
    """assignments <id>...: runtimes of several saved accounts, no switching."""
    if not args:
        raise Fail("invalid_input", "assignments <id>...", field="account")
    slot_raw = read_slot()
    results = []
    for account_id in args:
        try:
            item = {"ok": True, "assignments": assignments_of(account_id, slot_raw)}
        except Fail as exc:
            item = {"ok": False, "error": {"kind": exc.kind, "detail": exc.detail}}
        except Exception as exc:  # noqa: BLE001
            item = {"ok": False, "error": {"kind": "other", "detail": type(exc).__name__}}
        item["id"] = account_id
        results.append(item)
    return {"accounts": results}


def op_unassign(args):
    """unassign <id> <endpoint>: release a runtime of any saved account."""
    if len(args) != 2:
        raise Fail("invalid_input", "unassign <id> <endpoint>", field="endpoint")
    account_id, endpoint = args
    if not ENDPOINT_RE.match(endpoint):
        raise Fail("invalid_input", "invalid endpoint", field="endpoint")
    client = colab_client(credential_for(account_id, read_slot()))
    unassign = getattr(client, "unassign", None)
    if unassign is None:
        raise Fail("unsupported_cli_version", "Client.unassign missing")
    try:
        unassign(endpoint)
    except Exception as exc:  # noqa: BLE001
        status = _status_of(exc)
        if status == 404:
            raise Fail("session_gone", "runtime not found")
        if status is not None:
            raise Fail("http", "unassign returned HTTP %s" % status)
        raise Fail("http", "unassign failed (%s)" % type(exc).__name__)
    return {"endpoint": endpoint}


def secret_user(name):
    if not SECRET_RE.match(name or ""):
        raise Fail("invalid_input", "invalid secret name", field="secret")
    return "secret:" + name


def one_secret_name(args, usage):
    if len(args) != 1:
        raise Fail("invalid_input", usage, field="secret")
    return secret_user(args[0])


def op_secret_set(args):
    user = one_secret_name(args, "secret-set <name> (value on stdin)")
    value = sys.stdin.buffer.read(MAX_SECRET_BYTES + 1)
    if len(value) > MAX_SECRET_BYTES:
        raise Fail("invalid_input", "secret too long", field="secret")
    try:
        text = value.decode("utf-8").strip()
    except UnicodeDecodeError:
        raise Fail("invalid_input", "secret is not UTF-8", field="secret")
    if not text:
        raise Fail("invalid_input", "empty secret", field="secret")
    kr_set_verified(VAULT_SERVICE, user, text)
    return {}


def op_secret_get(args):
    return {"value": kr_get(VAULT_SERVICE, one_secret_name(args, "secret-get <name>"))}


def op_secret_has(args):
    return {"present": kr_get(VAULT_SERVICE, one_secret_name(args, "secret-has <name>")) is not None}


def op_secret_delete(args):
    kr_delete(VAULT_SERVICE, one_secret_name(args, "secret-delete <name>"))
    return {}


def _replace_all(text, needles):
    for needle in needles:
        text = text.replace(needle, "***")
    return text


def _redact_json(value, needles):
    if isinstance(value, str):
        return _replace_all(value, needles)
    if isinstance(value, list):
        return [_redact_json(v, needles) for v in value]
    if isinstance(value, dict):
        return {k: _redact_json(v, needles) for k, v in value.items()}
    return value


def op_history_redact(args):
    """history-redact <session>: replace secret values (a JSON list on stdin)
    with *** in the CLI's history file for that session. The CLI records each
    executed cell, including the os.environ prelude it builds from --env."""
    if len(args) != 1 or not SESSION_RE.match(args[0]):
        raise Fail("invalid_input", "history-redact <session>", field="session")
    raw = sys.stdin.buffer.read(1 << 20)
    try:
        secrets = json.loads(raw.decode("utf-8") or "[]")
    except (UnicodeDecodeError, ValueError):
        raise Fail("invalid_input", "secrets must be a JSON list of strings", field="secrets")
    if not isinstance(secrets, list) or not all(isinstance(s, str) for s in secrets):
        raise Fail("invalid_input", "secrets must be a JSON list of strings", field="secrets")
    values = [s for s in secrets if len(s) >= MIN_REDACT_LEN]
    # Forms a value can take inside a decoded history string: as printed, as
    # repr() wrote it into the env prelude, and JSON-escaped inside printed
    # JSON (the __CPG_RESULT__ line of a poll). Undecodable lines may hold any
    # of these JSON-escaped once more.
    decoded = set()
    for s in values:
        r = repr(s)[1:-1]
        decoded.update((s, r, json.dumps(s)[1:-1], json.dumps(r)[1:-1]))
    escaped = set(decoded)
    for s in list(decoded):
        escaped.add(json.dumps(s)[1:-1])
    decoded = sorted(decoded, key=len, reverse=True)
    escaped = sorted(escaped, key=len, reverse=True)
    path = os.path.join(os.path.expanduser(CLI_HISTORY_DIR), args[0] + ".jsonl")
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except FileNotFoundError:
        return {"present": False, "redacted_lines": 0}
    except OSError as exc:
        raise Fail("io", "cannot read the CLI history (%s)" % type(exc).__name__)
    lines = data.split(b"\n")
    changed = 0
    for i, line in enumerate(lines):
        if not line.strip():
            continue
        text = line.decode("utf-8", "surrogateescape")
        try:
            event = json.loads(text)
            redacted = _redact_json(event, decoded)
            new = json.dumps(redacted) if redacted != event else text
        except ValueError:
            new = _replace_all(text, escaped)
        if new != text:
            lines[i] = new.encode("utf-8", "surrogateescape")
            changed += 1
    if changed:
        directory = os.path.dirname(path)
        tmp = os.path.join(directory, ".%s.cpg.%d" % (os.path.basename(path), os.getpid()))
        try:
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "wb") as fh:
                fh.write(b"\n".join(lines))
                fh.flush()
                os.fsync(fh.fileno())
            os.chmod(tmp, 0o600)
            os.replace(tmp, path)
        except OSError as exc:
            try:
                os.remove(tmp)
            except OSError:
                pass
            raise Fail("io", "cannot rewrite the CLI history (%s)" % type(exc).__name__)
    return {"present": True, "redacted_lines": changed}


def op_begin_add(args):
    if pending_backup() is not None:
        raise Fail("add_in_progress", "an add-account flow is already pending")
    if os.path.exists(LEGACY_TOKEN_PATH):
        raise Fail("legacy_token_present", "~/.config/colab-cli/token.json exists")
    original = read_slot()
    if original:
        current_id = match_known(original, args)
        if current_id is None:
            raise Fail("active_account_not_adopted", "save the current credential first")
        kr_set_verified(VAULT_SERVICE, account_user(current_id), original)
        kr_set_verified(VAULT_SERVICE, PENDING_USER, original)
    else:
        kr_set_verified(VAULT_SERVICE, PENDING_USER, NO_TOKEN_SENTINEL)
    kr_delete(COLAB_SERVICE, COLAB_USER)
    if read_slot() is not None:
        # Could not clear the slot: undo the backup so nothing is pending.
        kr_delete(VAULT_SERVICE, PENDING_USER)
        raise Fail("keyring_unavailable", "could not clear the CLI credential")
    return {"had_token": bool(original)}


def op_finish_add(args):
    if len(args) != 1:
        raise Fail("invalid_input", "finish-add <id>", field="account")
    user = account_user(args[0])
    if pending_backup() is None:
        raise Fail("invalid_input", "no add-account flow is pending", field="state")
    raw = read_slot()
    if not raw:
        raise Fail("login_cancelled", "login did not create a credential")
    token_json, email = inspect(raw)
    kr_set_verified(VAULT_SERVICE, user, token_json)
    if token_json != raw:
        kr_set_verified(COLAB_SERVICE, COLAB_USER, token_json)
    kr_delete(VAULT_SERVICE, PENDING_USER)
    return {"email": email}


def op_rollback_add(args):
    del args
    backup = pending_backup()
    if backup is None:
        return {"restored": False}
    if backup == NO_TOKEN_SENTINEL:
        kr_delete(COLAB_SERVICE, COLAB_USER)
        if read_slot() is not None:
            raise Fail("keyring_unavailable", "could not clear the CLI credential")
    else:
        kr_set_verified(COLAB_SERVICE, COLAB_USER, backup)
    kr_delete(VAULT_SERVICE, PENDING_USER)
    return {"restored": True}


OPS = {
    "status": op_status,
    "list": op_list,
    "adopt": op_adopt,
    "use": op_use,
    "remove": op_remove,
    "usage": op_usage,
    "assignments": op_assignments,
    "unassign": op_unassign,
    "secret-set": op_secret_set,
    "secret-get": op_secret_get,
    "secret-has": op_secret_has,
    "secret-delete": op_secret_delete,
    "history-redact": op_history_redact,
    "begin-add": op_begin_add,
    "finish-add": op_finish_add,
    "rollback-add": op_rollback_add,
}


def main(argv):
    op = argv[0] if argv else ""
    handler = OPS.get(op)
    try:
        if handler is None:
            raise Fail("invalid_input", "unknown operation", field="op")
        result = handler(list(argv[1:]))
        result["ok"] = True
        emit(result)
    except Fail as exc:
        error = {"kind": exc.kind, "detail": exc.detail}
        if exc.field:
            error["field"] = exc.field
        emit({"ok": False, "error": error})
    except Exception as exc:  # noqa: BLE001
        # Never str(exc): third-party messages can embed credentials.
        emit({"ok": False, "error": {"kind": "other", "detail": type(exc).__name__}})
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
