#!/usr/bin/env python3
"""Create the ThetaBase GitHub App via the manifest flow.

Run it, click one button, and it writes the App's credentials to ../secrets/.

    python create-thetabase-app.py                       # placeholder webhook
    python create-thetabase-app.py https://host.example  # live webhook

No public URL is required to run this. The manifest's redirect_url is
localhost because that redirect is a *browser* redirect, not a callback GitHub
makes to a server - only the webhook is delivered to from GitHub's side, and
its URL can be changed for the life of the App via PATCH /app/hook/config
(see gh-app.py set-webhook).

Note that docs/github-app-setup.md's Route 1 points redirect_url at
/v1/github/setup, which is not a route the control plane serves.
"""

import http.server
import json
import os
import secrets
import stat
import sys
import urllib.error
import urllib.request
import webbrowser

PORT = 8931
ORIGIN = f"http://localhost:{PORT}"
# Deliberately outside the repository. A private key one `git add -A` away from
# being committed is a private key that eventually is.
OUT_DIR = os.path.normpath(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "secrets")
)

# Change this if "ThetaBase" is taken — App names are globally unique. GitHub
# will tell you on the confirmation page and let you rename it there.
APP_NAME = "ThetaBase"

# Pass the public base URL as the first argument if you have one:
#   python create-thetabase-app.py https://something.trycloudflare.com
# Without one, the webhook points at a domain that cannot resolve.
#
# The webhook is created ACTIVE either way, and that is deliberate. GitHub lets
# `PATCH /app/hook/config` change the URL for the life of the App, but rejects
# `active` outright ("not a permitted key") — so active is settable at creation
# or by hand in the UI, and nowhere else. Creating it switched off to be tidy
# buys a manual visit to the settings page that nothing can automate away.
# Pointing an active webhook at an unresolvable host costs a few failed
# deliveries, which is the cheaper mistake.
def _base_url(argv):
    """Validate the optional base URL, rather than pasting anything into the
    manifest. An unvalidated argument becomes `--help/v1/github/webhook`, which
    GitHub rejects with `Hook url is missing a scheme` only after opening a
    browser and posting the form."""
    if len(argv) <= 1:
        return None
    arg = argv[1]
    if arg in ("-h", "--help", "/?"):
        print(__doc__)
        sys.exit(0)
    if not arg.startswith(("http://", "https://")):
        sys.exit(
            f"not a URL: {arg!r}\n"
            f"usage: python {os.path.basename(argv[0])} [https://host]"
        )
    return arg.rstrip("/")


BASE_URL = _base_url(sys.argv)
WEBHOOK_URL = (
    f"{BASE_URL}/v1/github/webhook" if BASE_URL
    else "https://placeholder.invalid/v1/github/webhook"
)

STATE = secrets.token_urlsafe(16)

MANIFEST = {
    "name": APP_NAME,
    "url": "https://github.com/FelixKramer/ThetaBase",
    "description": (
        "Gives every pull request its own database branch, and posts the Safety "
        "Layer's verdict on any schema change before it lands."
    ),
    "hook_attributes": {
        "url": WEBHOOK_URL,
        "active": True,  # see above: this cannot be turned on later by API
    },
    "redirect_url": f"{ORIGIN}/setup",
    "public": False,
    "default_events": ["pull_request"],
    "default_permissions": {
        "pull_requests": "write",  # read the event, write the verdict comment
        "metadata": "read",        # mandatory; base and default branch names
    },
}

FORM_PAGE = """<!doctype html>
<meta charset="utf-8" />
<title>Create the ThetaBase GitHub App</title>
<style>
  body {{ font: 16px/1.5 system-ui, sans-serif; max-width: 40rem; margin: 4rem auto; padding: 0 1rem; }}
  pre {{ background: #f6f8fa; padding: 1rem; overflow-x: auto; font-size: 13px; }}
  button {{ font-size: 16px; padding: .6rem 1.2rem; }}
</style>
<h1>Create the ThetaBase GitHub App</h1>
<p>This posts the manifest below to GitHub. GitHub shows it back to you as a
   form — check the permissions there before confirming. Nothing is created
   until you do.</p>
<form action="https://github.com/settings/apps/new?state={state}" method="post">
  <input type="hidden" name="manifest" value='{manifest_attr}' />
  <button type="submit">Create the app on GitHub</button>
</form>
<h2>What it asks for</h2>
<pre>{manifest_pretty}</pre>
<p>Leave this script running. GitHub sends you back here with a one-time code,
   which it exchanges for the private key and writes to <code>secrets/</code>.</p>
"""

DONE_PAGE = """<!doctype html>
<meta charset="utf-8" />
<title>ThetaBase App created</title>
<style>body {{ font: 16px/1.5 system-ui, sans-serif; max-width: 40rem; margin: 4rem auto; padding: 0 1rem; }}</style>
<h1>Done</h1>
<p>App <strong>{name}</strong> (id <code>{app_id}</code>) created. Credentials
   written to <code>secrets/</code>.</p>
<p>Next: <a href="{html_url}/installations/new">install it on FelixKramer/ThetaBase</a>.
   You can close this tab.</p>
"""

ERROR_PAGE = """<!doctype html>
<meta charset="utf-8" />
<title>ThetaBase App setup failed</title>
<style>body {{ font: 16px/1.5 system-ui, sans-serif; max-width: 40rem; margin: 4rem auto; padding: 0 1rem; }}</style>
<h1>That did not work</h1>
<pre>{detail}</pre>
<p>The code is valid for one hour and single-use. If it was already spent, delete
   the half-created App on GitHub and run this script again.</p>
"""


def convert(code):
    """Trade the one-time code for the App's credentials. Only chance at the key."""
    req = urllib.request.Request(
        f"https://api.github.com/app-manifests/{code}/conversions",
        method="POST",
        headers={
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "thetabase-app-setup",
        },
    )
    with urllib.request.urlopen(req) as resp:
        return json.load(resp)


def restrict(path):
    """Make the file readable only by the current user.

    The 0600 passed to os.open is a no-op on Windows, which has no POSIX mode
    bits — so on Windows, replace the file's ACL with a single entry for the
    current user. Best effort: a failure here is reported, not fatal, because
    the key still needs saving.
    """
    if sys.platform != "win32":
        return "0600"
    import subprocess
    user = os.environ.get("USERNAME", "")
    # Modify, not (R,W): on Windows (R,W) excludes DELETE, which locks the
    # owner out of rotating or removing their own key. The security goal is
    # "nobody but this user", not "not even this user".
    rights = "(OI)(CI)(M)" if os.path.isdir(path) else "(M)"
    try:
        subprocess.run(
            ["icacls", path, "/inheritance:r", "/grant:r", f"{user}:{rights}"],
            check=True, capture_output=True,
        )
        return f"ACL: {user} only"
    except (subprocess.CalledProcessError, FileNotFoundError) as exc:
        return f"COULD NOT RESTRICT ({exc}) — tighten it yourself"


def save(app):
    os.makedirs(OUT_DIR, exist_ok=True)
    restrict(OUT_DIR)
    pem_path = os.path.join(OUT_DIR, "github-app.pem")
    json_path = os.path.join(OUT_DIR, "github-app.json")

    # Create with 0600 rather than chmod after the fact — a key that is briefly
    # world-readable is a key that was world-readable.
    mode = stat.S_IRUSR | stat.S_IWUSR
    fd = os.open(pem_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode)
    with os.fdopen(fd, "w", newline="") as f:
        f.write(app["pem"])

    meta = {k: v for k, v in app.items() if k != "pem"}
    fd = os.open(json_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode)
    with os.fdopen(fd, "w") as f:
        json.dump(meta, f, indent=2)

    return pem_path, json_path, restrict(pem_path)


class Handler(http.server.BaseHTTPRequestHandler):
    done = False

    def log_message(self, *args):
        pass  # the script's own output is the log

    def reply(self, body, status=200):
        encoded = body.encode()
        self.send_response(status)
        self.send_header("content-type", "text/html; charset=utf-8")
        self.send_header("content-length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self):
        path, _, query = self.path.partition("?")
        params = dict(
            p.split("=", 1) for p in query.split("&") if "=" in p
        )

        if path == "/":
            manifest_json = json.dumps(MANIFEST)
            self.reply(FORM_PAGE.format(
                state=STATE,
                manifest_attr=manifest_json.replace("'", "&#39;"),
                manifest_pretty=json.dumps(MANIFEST, indent=2),
            ))
            return

        if path != "/setup":
            self.reply("<h1>404</h1>", 404)
            return

        if params.get("state") != STATE:
            self.reply(ERROR_PAGE.format(detail="state mismatch — refusing"), 400)
            return

        code = params.get("code")
        if not code:
            self.reply(ERROR_PAGE.format(detail="no ?code= in the redirect"), 400)
            return

        try:
            app = convert(code)
        except urllib.error.HTTPError as exc:
            detail = f"HTTP {exc.code}\n{exc.read().decode(errors='replace')}"
            self.reply(ERROR_PAGE.format(detail=detail), 502)
            print(f"\n  exchange failed:\n{detail}\n", file=sys.stderr)
            return

        pem_path, json_path, protection = save(app)
        self.reply(DONE_PAGE.format(
            name=app.get("name", APP_NAME),
            app_id=app["id"],
            html_url=app["html_url"],
        ))

        print(f"""
  App created: {app.get('name')}  (id {app['id']}, slug {app.get('slug')})

    private key      {pem_path}   [{protection}]
    everything else  {json_path}

  Set these when the Control Plane runs:

    THETA_GITHUB_APP_ID={app['id']}
    THETA_GITHUB_APP_KEY_PATH={pem_path}
    THETA_GITHUB_WEBHOOK_SECRET={app.get('webhook_secret')}

  Now install it:  {app['html_url']}/installations/new
""")
        Handler.done = True


class Server(http.server.HTTPServer):
    # Windows will happily hand out a port another process is already listening
    # on when SO_REUSEADDR is set, and then deliver the redirect to whichever of
    # us it feels like. Refuse to start instead of racing for the code.
    allow_reuse_address = False


def main():
    try:
        server = Server(("127.0.0.1", PORT), Handler)
    except OSError as exc:
        print(f"  cannot listen on {PORT}: {exc}\n"
              f"  something else is using it — edit PORT at the top and rerun.",
              file=sys.stderr)
        return 1
    print(f"  Open {ORIGIN}/ and click the button. Ctrl-C to give up.")
    webbrowser.open(f"{ORIGIN}/")
    try:
        while not Handler.done:
            server.handle_request()
    except KeyboardInterrupt:
        print("\n  cancelled")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
