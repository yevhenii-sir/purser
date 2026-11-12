"""Shared helpers for the server scripts: the spacetime CLI and the SpacetimeDB HTTP API.

Two flavours of every HTTP helper:
  * `request`, `get`, `select_raw`, `call_raw`, `logs` raise `StdbError` (long-running callers such
    as mcp_server.py, which must keep going after a failed call);
  * `post`, `select`, `call` exit the script with the error (seed.py, migrate.py).

The token is `PURSER_TOKEN` when set, otherwise the one of `spacetime login`. It is sent only over
https, or over http to this machine (localhost, an SSH tunnel), and never follows a redirect to
another origin.
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request


LOOPBACK_HOSTS = {"localhost", "127.0.0.1", "::1"}


class StdbError(Exception):
    """A failed CLI or HTTP call. `status` and `body` are the HTTP status and response text when the
    server answered (a reducer's error message is the body of an HTTP 530)."""

    def __init__(self, message, status=None, body=""):
        super().__init__(message)
        self.status = status
        self.body = body


def cli_raw(*args):
    # Run outside the project: inside it the CLI takes the database from spacetime.json.
    try:
        r = subprocess.run(["spacetime", *args], capture_output=True, text=True, cwd=tempfile.gettempdir())
    except FileNotFoundError:
        raise StdbError("the spacetime CLI is not installed (or set PURSER_TOKEN and pass --server as a URL)")
    if r.returncode != 0:
        raise StdbError(f"spacetime {' '.join(args)} failed:\n{r.stdout}{r.stderr}")
    return r.stdout


def cli(*args):
    try:
        return cli_raw(*args)
    except StdbError as e:
        sys.exit(str(e))


def server_url_raw(nickname):
    """Base URL of a CLI server nickname (`maincloud`, `local`, ...) or a URL as is."""
    if nickname.startswith(("http://", "https://")):
        return nickname.rstrip("/")
    for line in cli_raw("server", "list").splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[-1] == nickname and parts[-2] in ("http", "https"):
            return f"{parts[-2]}://{parts[-3]}"
    raise StdbError(f"server '{nickname}' is not in `spacetime server list`")


def server_url(nickname):
    try:
        return server_url_raw(nickname)
    except StdbError as e:
        sys.exit(str(e))


def token_raw():
    env = os.environ.get("PURSER_TOKEN", "").strip()
    if env:
        return env
    m = re.search(r"token \(don't share this!\) is (\S+)", cli_raw("login", "show", "--token"))
    if not m:
        raise StdbError("not logged in: run `spacetime login` or set PURSER_TOKEN")
    return m.group(1)


def token():
    try:
        return token_raw()
    except StdbError as e:
        sys.exit(str(e))


def origin(url):
    u = urllib.parse.urlsplit(url)
    return u.scheme.lower(), (u.hostname or "").lower(), u.port


def check_transport(url):
    """Refuses to send a token in clear text to anything but this machine."""
    scheme, host, _ = origin(url)
    if scheme == "https" or (scheme == "http" and host in LOOPBACK_HOSTS):
        return
    raise StdbError(f"refusing to send the token to {scheme}://{host}: use https, or an SSH tunnel to "
                    "localhost for a self-hosted server (see docs/self-hosting.md)")


class SameOriginAuthRedirect(urllib.request.HTTPRedirectHandler):
    """Follows redirects, but drops the token when one leaves the original origin."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is not None and origin(new.full_url) != origin(req.full_url):
            new.remove_header("Authorization")
        return new


OPENER = urllib.request.build_opener(SameOriginAuthRedirect)


def request(method, url, tok, body=None, content_type="text/plain", timeout=60):
    """One HTTP call; returns the response text or raises `StdbError`."""
    check_transport(url)
    data = body.encode("utf-8") if body is not None else None
    headers = {"Authorization": f"Bearer {tok}"}
    if data is not None:
        headers["Content-Type"] = content_type
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with OPENER.open(req, timeout=timeout) as res:
            return res.read().decode("utf-8")
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        raise StdbError(f"{method} {url}: HTTP {e.code}: {body}", e.code, body)
    except urllib.error.URLError as e:
        raise StdbError(f"{method} {url}: {e.reason} (is the server running / the SSH tunnel open?)")
    except TimeoutError:
        raise StdbError(f"{method} {url}: timed out")


def get(url, tok):
    return request("GET", url, tok)


def post(url, tok, body, content_type="text/plain"):
    try:
        return request("POST", url, tok, body, content_type)
    except StdbError as e:
        sys.exit(str(e))


def unwrap(v):
    # Single-field products (Identity, Timestamp, ...) come as one-element arrays.
    return v[0] if isinstance(v, list) and len(v) == 1 and not isinstance(v[0], (list, dict)) else v


def sql_str(s):
    """A SQL string literal."""
    return "'" + str(s).replace("'", "''") + "'"


def select_raw(base, db, tok, query):
    result = json.loads(request("POST", f"{base}/v1/database/{db}/sql", tok, query))[0]
    cols = [e["name"].get("some", e["name"]) if isinstance(e["name"], dict) else e["name"]
            for e in result["schema"]["elements"]]
    return [dict(zip(cols, map(unwrap, row))) for row in result["rows"]]


def select(base, db, tok, query):
    try:
        return select_raw(base, db, tok, query)
    except StdbError as e:
        sys.exit(str(e))


def call_raw(base, db, tok, name, *args):
    """Calls a reducer or procedure; returns the procedure's result (JSON-decoded twice: our
    procedures return a JSON document as a string), None for a reducer. Raises `StdbError` when the
    call fails (a reducer's error comes back as an HTTP error)."""
    out = request("POST", f"{base}/v1/database/{db}/call/{name}", tok, json.dumps(list(args)), "application/json")
    if not out.strip():
        return None
    value = json.loads(out)
    if isinstance(value, str):
        try:
            return json.loads(value)
        except ValueError:
            return value
    return value


def call(base, db, tok, name, *args):
    try:
        return call_raw(base, db, tok, name, *args)
    except StdbError as e:
        sys.exit(str(e))


def logs(base, db, tok, num_lines=200):
    """The last `num_lines` lines of the module log, oldest first, each parsed from JSON when it
    is JSON (`{"level", "ts", "message", ...}`), else `{"message": line}`."""
    text = get(f"{base}/v1/database/{db}/logs?{urllib.parse.urlencode({'num_lines': int(num_lines)})}", tok)
    out = []
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            v = json.loads(line)
            out.append(v if isinstance(v, dict) else {"message": line})
        except ValueError:
            out.append({"message": line})
    return out
