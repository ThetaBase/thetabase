#!/usr/bin/env python3
"""Act as the ThetaBase GitHub App from the command line.

    python gh-app.py whoami              # prove the private key works
    python gh-app.py installations       # list installations and their ids
    python gh-app.py set-webhook <URL>   # point the App's webhook at <URL>, active

The App JWT proves "I am this app" and can do almost nothing except ask for an
installation token. It is signed RS256 and lives nine minutes rather than the
permitted ten, so it cannot expire mid-flight against a slow request.

RS256 is implemented here rather than pulled from a dependency because this
repo has no Python dependencies and a setup script that needs `pip install`
first is a setup script that gets skipped. It is PKCS#1 v1.5 over SHA-256,
which is a fixed prefix and a modular exponentiation.
"""

import base64
import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

SECRETS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "secrets")
PEM_PATH = os.path.normpath(os.path.join(SECRETS, "github-app.pem"))
JSON_PATH = os.path.normpath(os.path.join(SECRETS, "github-app.json"))

API = "https://api.github.com"

# DigestInfo for SHA-256, RFC 8017 §9.2 step 2.
SHA256_DER_PREFIX = bytes.fromhex("3031300d060960864801650304020105000420")


def _der_len(buf, i):
    """Return (length, next_index) for a DER length at buf[i]."""
    n = buf[i]
    i += 1
    if n < 0x80:
        return n, i
    count = n & 0x7F
    return int.from_bytes(buf[i:i + count], "big"), i + count


def _der_ints(der):
    """Yield the INTEGERs of a top-level DER SEQUENCE."""
    assert der[0] == 0x30, "not a DER SEQUENCE"
    _, i = _der_len(der, 1)
    end = len(der)
    while i < end:
        tag = der[i]
        length, i = _der_len(der, i + 1)
        if tag == 0x02:
            yield int.from_bytes(der[i:i + length], "big")
        i += length


def load_private_key(path=PEM_PATH):
    """Return (n, d) from a PKCS#1 or PKCS#8 RSA private key PEM."""
    text = open(path).read()
    body = "".join(
        line for line in text.splitlines() if not line.startswith("-----")
    )
    der = base64.b64decode(body)

    if "BEGIN PRIVATE KEY" in text:
        # PKCS#8 wraps the PKCS#1 key in an OCTET STRING; find and unwrap it.
        _, i = _der_len(der, 1)
        while i < len(der):
            tag = der[i]
            length, i = _der_len(der, i + 1)
            if tag == 0x04:
                der = der[i:i + length]
                break
            i += length

    ints = list(_der_ints(der))
    # version, n, e, d, ...
    return ints[1], ints[3]


def rs256(message: bytes, n: int, d: int) -> bytes:
    k = (n.bit_length() + 7) // 8
    digest = hashlib.sha256(message).digest()
    t = SHA256_DER_PREFIX + digest
    # EMSA-PKCS1-v1_5: 0x00 0x01 <0xFF padding> 0x00 <DigestInfo>
    if k < len(t) + 11:
        raise ValueError("key too small")
    em = b"\x00\x01" + b"\xff" * (k - len(t) - 3) + b"\x00" + t
    sig = pow(int.from_bytes(em, "big"), d, n)
    return sig.to_bytes(k, "big")


def b64u(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def app_jwt():
    app_id = json.load(open(JSON_PATH))["id"]
    n, d = load_private_key()
    now = int(time.time())
    header = {"alg": "RS256", "typ": "JWT"}
    # iat backdated for clock skew; exp at +540s, inside GitHub's 600s ceiling.
    payload = {"iat": now - 60, "exp": now + 540, "iss": app_id}
    signing_input = (
        b64u(json.dumps(header, separators=(",", ":")).encode()) + "." +
        b64u(json.dumps(payload, separators=(",", ":")).encode())
    ).encode()
    return (signing_input + b"." + b64u(rs256(signing_input, n, d)).encode()).decode()


def call(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        API + path, data=data, method=method,
        headers={
            "authorization": f"Bearer {app_jwt()}",
            "accept": "application/vnd.github+json",
            "x-github-api-version": "2022-11-28",
            "user-agent": "thetabase-gh-app",
            **({"content-type": "application/json"} if data else {}),
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read().decode()
            return r.status, (json.loads(raw) if raw.strip() else None)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    cmd = argv[1]

    if cmd == "whoami":
        status, body = call("GET", "/app")
        if status != 200:
            print(f"FAILED ({status}): {body}")
            return 1
        print(f"authenticated as {body['name']} (id {body['id']}, slug {body['slug']})")
        print(f"  owner        {body['owner']['login']}")
        print(f"  permissions  {body.get('permissions')}")
        print(f"  events       {body.get('events')}")
        print(f"  installs     {body.get('installations_count')}")
        print(f"  webhook      {body.get('hook_attributes', {})}")
        return 0

    if cmd == "installations":
        status, body = call("GET", "/app/installations")
        if status != 200:
            print(f"FAILED ({status}): {body}")
            return 1
        for inst in body:
            print(f"installation {inst['id']} on {inst['account']['login']} "
                  f"({inst['repository_selection']})")
            print(f"  permissions {inst.get('permissions')}")
        return 0

    if cmd == "set-webhook":
        if len(argv) < 3:
            print("usage: gh-app.py set-webhook <base-url-or-full-webhook-url>")
            return 2
        url = argv[2].rstrip("/")
        if not url.endswith("/v1/github/webhook"):
            url += "/v1/github/webhook"
        status, body = call("PATCH", "/app/hook/config", {"url": url})
        if status != 200:
            print(f"FAILED ({status}): {body}")
            return 1
        print(f"webhook url set to {body.get('url')}")
        return 0

    print(f"unknown command: {cmd}")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
