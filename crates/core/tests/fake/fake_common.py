"""Shared state for the fake Colab toolchain used by the core's integration tests.

Everything lives under the directory named by ``CPG_FAKE_STATE`` (a test's
temporary directory). Nothing here ever imports the real ``keyring``,
``google-auth`` or ``colab_cli``: the fake interpreter installs stand-ins for
those modules in ``sys.modules`` before running the real account helper.

Files:
  keyring.json   {"<service>\\u0000<user>": "<value>"}  (fake OS keyring)
  server.json    accounts, their balances and assignments, and test knobs
  calls.jsonl    one line per fake ``colab`` invocation (argv)

Fake identities: account ``alice`` has the token
``{"token": "fake-access-alice", "refresh_token": "rt-alice", ...}`` and the
email ``alice@example.com``.
"""

import json
import os
import sys
import time
import types

# Never resolve third-party packages (a real keyring, google-auth or colab_cli)
# from the system interpreter.
sys.path[:] = [p for p in sys.path if "site-packages" not in p and "dist-packages" not in p]

STATE = os.environ.get("CPG_FAKE_STATE")
if not STATE or not os.path.isdir(STATE):
    sys.stderr.write("fake toolchain: CPG_FAKE_STATE is not set to a directory\n")
    sys.exit(97)

KEYRING = os.path.join(STATE, "keyring.json")
SERVER = os.path.join(STATE, "server.json")

DEFAULT_KNOBS = {
    "deny": [],                     # accelerators that get HTTP 400
    "no_answer": [],                # accelerators whose `new` never gets an answer
    "no_answer_hangs": False,       # ...and waits on (else: exits with the read timeout)
    "a100_sequence": [40960, 81920],  # VRAM of successive A100 allocations
    "login_name": "bob",            # account minted by a successful login
    "good_code": "good-code",       # the only accepted authorisation code
    "plugin_fail_stage": None,      # plugin runs fail at this stage
    "plugin_stages": ["detect", "plan", "download", "server", "warmup"],
    "ssh_busy": False,              # proxy-mode answers HTTP 429
    "watchdog_start_fails": False,  # a started watchdog exits at once
    "stop_noop": False,             # stop claims success but the VM stays assigned
    "keyring_locked": False,        # keyring reads fail
    "keyring_service_down": False,  # keyring ops fail as if the Secret Service were down
    "keyring_no_backend": False,    # keyring resolves only its fail backend
    "cli_api_broken": False,        # colab_cli.client cannot be imported
    "list_assignments_fail_once": False,  # the next list_assignments raises (SSLError-like)
    "skip_state_write": False,      # `colab new` allocates but does not write the state file
}


def _read(path, default):
    try:
        with open(path) as fh:
            text = fh.read()
        return json.loads(text) if text.strip() else default
    except FileNotFoundError:
        return default


def _write(path, value):
    tmp = path + ".tmp.%d" % os.getpid()
    with open(tmp, "w") as fh:
        json.dump(value, fh, indent=1, sort_keys=True)
    os.replace(tmp, path)


def server():
    s = _read(SERVER, {})
    s.setdefault("accounts", {})
    s.setdefault("knobs", {})
    for k, v in DEFAULT_KNOBS.items():
        s["knobs"].setdefault(k, v)
    s.setdefault("counter", 0)
    s.setdefault("runs", {})
    return s


def save_server(s):
    _write(SERVER, s)


def knob(name):
    return server()["knobs"].get(name, DEFAULT_KNOBS.get(name))


def token_for(name):
    return json.dumps({
        "token": "fake-access-" + name,
        "refresh_token": "rt-" + name,
        "client_id": "fake-client",
        "client_secret": "fake-secret",
        "token_uri": "https://oauth2.googleapis.com/token",
    })


def account_of_token(raw):
    try:
        t = json.loads(raw) if isinstance(raw, str) else raw
        tok = t.get("token") or ""
        if tok.startswith("fake-access-"):
            return tok[len("fake-access-"):]
        rt = t.get("refresh_token") or ""
        return rt[3:] if rt.startswith("rt-") else None
    except Exception:  # noqa: BLE001
        return None


def kr_all():
    return _read(KEYRING, {})


def kr_get(service, user):
    return kr_all().get(service + "\0" + user)


def kr_set(service, user, value):
    values = kr_all()
    values[service + "\0" + user] = value
    _write(KEYRING, values)


def kr_delete(service, user):
    values = kr_all()
    key = service + "\0" + user
    if key not in values:
        return False
    del values[key]
    _write(KEYRING, values)
    return True


def slot_account():
    raw = kr_get("google-colab-cli", "oauth_token")
    return account_of_token(raw) if raw else None


def account_state(s, name):
    return s["accounts"].setdefault(name, {"balance": 100.0, "assignments": {}})


RATES = {"NONE": 0.08, "T4": 1.76, "L4": 4.82, "G4": 8.2, "A100": 11.77, "H100": 18.3, "V5E1": 3.9, "V6E1": 6.4}


def usage_of(s, name):
    a = account_state(s, name)
    rate = sum(RATES.get(x["accelerator"], 1.0) for x in a["assignments"].values())
    return {"currentBalance": a["balance"], "consumptionRateHourly": rate, "assignmentsCount": len(a["assignments"])}


# ---------------------------------------------------------------- fake modules


class SSLError(Exception):
    """Stands in for the transport error the real CLI raises on a broken link."""


class SecretServiceNotAvailableException(Exception):
    """Stands in for secretstorage's error when no Secret Service answers.

    The class name is the contract: the account helper recognises the real
    exception by name, so the fake must carry the same one."""


def daemon_started():
    """True once the fake gnome-keyring-daemon (a PATH stub in the tests)
    has run: it leaves this marker in the state directory."""
    return os.path.exists(os.path.join(STATE, "keyring_daemon_started"))


class _FakeKeyringBackend:
    priority = 1


class FailKeyring:
    """Stands in for keyring's fail backend (recognised by class name)."""

    priority = -1


def install_fake_modules():
    """Stand-ins for keyring, google-auth, requests and colab_cli.client."""

    errors = types.ModuleType("keyring.errors")

    class KeyringError(Exception):
        pass

    class PasswordDeleteError(KeyringError):
        pass

    class KeyringLocked(KeyringError):
        pass

    errors.KeyringError = KeyringError
    errors.PasswordDeleteError = PasswordDeleteError
    errors.KeyringLocked = KeyringLocked

    keyring = types.ModuleType("keyring")
    keyring.errors = errors

    def service_down():
        return knob("keyring_service_down") and not daemon_started()

    def get_password(service, user):
        if knob("keyring_locked"):
            raise KeyringLocked("locked")
        if service_down():
            raise SecretServiceNotAvailableException("no Secret Service on the session bus")
        return kr_get(service, user)

    def set_password(service, user, value):
        if knob("keyring_locked"):
            raise KeyringLocked("locked")
        if service_down():
            raise SecretServiceNotAvailableException("no Secret Service on the session bus")
        kr_set(service, user, value)

    def delete_password(service, user):
        if service_down():
            raise SecretServiceNotAvailableException("no Secret Service on the session bus")
        if not kr_delete(service, user):
            raise PasswordDeleteError("missing")

    def get_keyring():
        if knob("keyring_no_backend") and not daemon_started():
            return FailKeyring()
        return _FakeKeyringBackend()

    keyring.get_password = get_password
    keyring.set_password = set_password
    keyring.delete_password = delete_password
    keyring.get_keyring = get_keyring

    google = types.ModuleType("google")
    oauth2 = types.ModuleType("google.oauth2")
    credentials = types.ModuleType("google.oauth2.credentials")
    auth = types.ModuleType("google.auth")
    transport = types.ModuleType("google.auth.transport")
    treq = types.ModuleType("google.auth.transport.requests")

    class Credentials:
        def __init__(self, info):
            self.info = dict(info)
            self.token = info.get("token")
            self.refresh_token = info.get("refresh_token")

        @classmethod
        def from_authorized_user_info(cls, info):
            return cls(info)

        @property
        def valid(self):
            return bool(self.token) and not self.info.get("expired")

        def refresh(self, request):
            if not (self.refresh_token or "").startswith("rt-"):
                raise RuntimeError("RefreshError")
            self.token = "fake-access-" + self.refresh_token[3:]
            self.info["token"] = self.token
            self.info.pop("expired", None)

        def to_json(self):
            return json.dumps(self.info)

    class Request:
        pass

    class Response:
        def __init__(self, status, payload, text=None):
            self.status_code = status
            self._payload = payload
            self.text = text if text is not None else json.dumps(payload)

        def json(self):
            return self._payload

    class AuthorizedSession:
        def __init__(self, creds):
            self.creds = creds

        def get(self, url, headers=None, params=None, timeout=None):
            name = account_of_token({"token": self.creds.token})
            if not url.endswith("/tun/m/ccu-info") or not name:
                return Response(404, {})
            return Response(200, None, ")]}'\n" + json.dumps(usage_of(server(), name)))

    credentials.Credentials = Credentials
    treq.Request = Request
    treq.AuthorizedSession = AuthorizedSession
    google.oauth2 = oauth2
    google.auth = auth
    oauth2.credentials = credentials
    auth.transport = transport
    transport.requests = treq

    requests = types.ModuleType("requests")

    def req_get(url, params=None, timeout=None, **_):
        token = (params or {}).get("access_token", "")
        if url.endswith("/tokeninfo") and token.startswith("fake-access-"):
            name = token[len("fake-access-"):]
            return Response(200, {"email": name + "@example.com", "expires_in": "3599", "scope": "openid"})
        return Response(400, {"error": "invalid_token"})

    requests.get = req_get

    modules = {
        "keyring": keyring,
        "keyring.errors": errors,
        "google": google,
        "google.oauth2": oauth2,
        "google.oauth2.credentials": credentials,
        "google.auth": auth,
        "google.auth.transport": transport,
        "google.auth.transport.requests": treq,
        "requests": requests,
    }

    if knob("cli_api_broken"):
        # `None` in sys.modules makes `import colab_cli` raise ImportError, so
        # a real installation can never be picked up instead.
        modules["colab_cli"] = None
        modules["colab_cli.client"] = None
    else:
        colab_cli = types.ModuleType("colab_cli")
        client = types.ModuleType("colab_cli.client")

        class _Named:
            def __init__(self, name):
                self.name = name
                self.value = name

        class _Assignment:
            def __init__(self, endpoint, a):
                self.endpoint = endpoint
                self.accelerator = _Named(a["accelerator"])
                self.variant = _Named(a.get("variant", "GPU"))
                self.machine_shape = _Named(a.get("shape", "STANDARD"))
                self.runtime_proxy_info = object()

        class ColabRequestError(Exception):
            def __init__(self, status):
                super().__init__("HTTP %d" % status)
                self.response = types.SimpleNamespace(status_code=status)

        class Prod:
            pass

        class Client:
            def __init__(self, env, session):
                self.name = account_of_token({"token": session.creds.token})

            def list_assignments(self):
                s = server()
                if s["knobs"].get("list_assignments_fail_once"):
                    s["knobs"]["list_assignments_fail_once"] = False
                    save_server(s)
                    raise SSLError("simulated connection error")
                a = account_state(s, self.name)
                return [_Assignment(ep, x) for ep, x in a["assignments"].items()]

            def unassign(self, endpoint):
                s = server()
                a = account_state(s, self.name)
                if endpoint not in a["assignments"]:
                    raise ColabRequestError(404)
                del a["assignments"][endpoint]
                save_server(s)

        client.Client = Client
        client.Prod = Prod
        colab_cli.client = client
        modules["colab_cli"] = colab_cli
        modules["colab_cli.client"] = client

    sys.modules.update(modules)
