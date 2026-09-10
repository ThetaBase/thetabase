# Setting up the ThetaBase GitHub App

Branch-per-PR (ROADMAP M7) needs a GitHub App. This is how to create one.

**It has to be you.** A GitHub App is registered interactively under an account
or organisation you control, and its private key is generated once and offered
as a single download that GitHub never shows again. There is no API that creates
an App without a browser session and your consent — which is the point, since an
App can be granted write access to repositories.

There are two routes. The manifest is faster and gets the permissions right by
construction; the manual route is there because it is worth knowing what the
manifest is asking for.

---

## Route 1: the manifest (recommended)

GitHub can create an App from a JSON manifest, so you approve a filled-in form
rather than filling one in. It also hands back the private key, webhook secret
and client secret in one response — the only time GitHub ever gives you the key
programmatically.

**In practice, run `create-thetabase-app.py` at the repository root** — it does
everything below, including catching the redirect and exchanging the code
inside the one-hour window. The rest of this section is what it is doing.

Two corrections to the hand-rolled version that follows, both learned by doing
it:

- **`redirect_url` does not need to be public, and must not be
  `/v1/github/setup`** — there is no such route in `api.rs`. The manifest
  redirect is a *browser* redirect, so `http://localhost:<port>/…` works and is
  what the script uses. Only `hook_attributes.url` is fetched by GitHub itself.
- **Create the webhook `active: true`, even pointing at a placeholder.**
  `PATCH /app/hook/config` can change the URL for the life of the App but
  rejects `active` with `"active" is not a permitted key`. Active is settable
  at creation or by hand in the settings UI, and nowhere else — so creating it
  switched off buys a manual step nothing can automate away.

### 1. Save this as `create-thetabase-app.html` and open it in a browser

Replace `YOUR_CONTROL_PLANE_URL` with a URL GitHub can reach. For local
development, use a tunnel (`cloudflared tunnel --url http://localhost:8080`, or
ngrok) — GitHub cannot deliver to `localhost`.

```html
<!doctype html>
<meta charset="utf-8" />
<title>Create the ThetaBase GitHub App</title>
<h1>Create the ThetaBase GitHub App</h1>
<p>This posts a manifest to GitHub. You will see exactly what it asks for
   before anything is created.</p>

<!-- For an organisation, use:
     https://github.com/organizations/YOUR_ORG/settings/apps/new?state=thetabase -->
<form action="https://github.com/settings/apps/new?state=thetabase" method="post">
  <input type="hidden" name="manifest" id="manifest" />
  <button type="submit">Create the app on GitHub</button>
</form>

<script>
  const CONTROL_PLANE = "YOUR_CONTROL_PLANE_URL"; // e.g. https://theta.example.com

  const manifest = {
    name: "ThetaBase",
    url: "https://github.com/FelixKramer/ThetaBase",
    description:
      "Gives every pull request its own database branch, and posts the Safety " +
      "Layer's verdict on any schema change before it lands.",

    hook_attributes: {
      url: `${CONTROL_PLANE}/v1/github/webhook`,
      active: true,
    },

    // Where GitHub sends you after creating it. The one-time code arrives here.
    redirect_url: `${CONTROL_PLANE}/v1/github/setup`,

    // false = installable on any account. true = only yours.
    public: false,

    default_events: ["pull_request"],

    default_permissions: {
      // Read a pull request; that is all the branch lifecycle needs.
      pull_requests: "write", // write is for posting the verdict comment
      // Read the base branch name and the default branch.
      metadata: "read",
    },
  };

  document.getElementById("manifest").value = JSON.stringify(manifest);
</script>
```

### 2. Approve it

GitHub shows you the manifest as a form. Check the permissions — they should be
exactly the two above — then create it.

### 3. Exchange the code

GitHub redirects to your `redirect_url` with `?code=...`. Exchange it **within
one hour**, once:

```sh
curl -X POST -H "Accept: application/vnd.github+json" \
  https://api.github.com/app-manifests/<CODE>/conversions
```

The response carries everything you need:

```json
{
  "id": 123456,
  "client_id": "Iv1.abc123",
  "client_secret": "...",
  "webhook_secret": "...",
  "pem": "-----BEGIN RSA PRIVATE KEY-----\n..."
}
```

**Save the `pem` and `webhook_secret` immediately.** The `pem` is offered once.
Go to [step 5](#5-configure-the-control-plane).

---

## Route 2: by hand

<https://github.com/settings/apps/new> — or, for an organisation,
`https://github.com/organizations/<ORG>/settings/apps/new`.

| Field | Value |
|---|---|
| **GitHub App name** | `ThetaBase` (globally unique; add a suffix if taken) |
| **Homepage URL** | your project or company URL |
| **Webhook** | Active |
| **Webhook URL** | `https://<control-plane>/v1/github/webhook` |
| **Webhook secret** | generate one: `openssl rand -hex 32` — **save it** |

**Repository permissions** — only these two:

| Permission | Access | Why |
|---|---|---|
| **Pull requests** | Read and write | Read the event; write the Safety Layer's comment |
| **Metadata** | Read-only | Mandatory, and grants the base and default branch names |

Leave everything else at *No access*. The App creates branches in **ThetaBase**,
not in git — it needs no `contents` permission, and granting one would be asking
for write access to source code for no reason.

**Subscribe to events:** `Pull request`. Nothing else.

**Where can this be installed:** your account only, until it is ready for
customers.

Create it, then:

1. Note the **App ID** at the top of the settings page.
2. **Generate a private key** at the bottom. The `.pem` downloads once.

---

## 4. Install it

On the App's page: **Install App** → pick the account → **Only select
repositories** → choose them.

The URL after installing ends in the installation id
(`/settings/installations/<ID>`). You will need it.

---

## 5. Configure the Control Plane

```sh
# Verifies every webhook delivery. Without it, deliveries are refused —
# deliberately, since this endpoint creates and destroys branches.
export THETA_GITHUB_WEBHOOK_SECRET='...'

# Authenticate as the App.
export THETA_GITHUB_APP_ID='123456'
export THETA_GITHUB_APP_KEY_PATH='/etc/thetabase/github-app.pem'
# ...or inline, for a secret manager that injects environment variables:
# export THETA_GITHUB_APP_KEY="$(cat github-app.pem)"
```

Then bind each repository to the ThetaBase project its pull requests should act
on. Nothing happens until you do — a repository is never inferred from a name,
because guessing wrong means a stranger's pull request creating branches on
somebody's database:

```sh
curl -X PUT https://<control-plane>/v1/projects/<org>%2F<project>/github \
  -H "authorization: Bearer $THETA_IDENTITY_TOKEN" \
  -H "content-type: application/json" \
  -d '{"repository": "acme/app"}'
```

This requires `CreateProjects` in the owning org — the same capability as making
a project, because that is effectively what the binding grants.

---

## 5b. Or, for local development

`dev-stack.ps1` at the repository root does steps 5 and 6 in one command: it
starts the control plane, opens a Cloudflare quick tunnel, binds this
repository to the seeded dev project, and points the App's webhook at the
tunnel's hostname via `gh-app.py set-webhook`. A quick tunnel gets a new
hostname on every restart, which is why the URL is set from code rather than
by hand.

`gh-app.py` also answers the two questions worth asking of a fresh App:

```sh
python gh-app.py whoami         # does the private key actually work
python gh-app.py installations  # which accounts and repositories
```

---

## 6. Check it

Open a pull request. Within a few seconds:

- a branch named `pr-<number>` exists (`theta branch list --env preview`);
- the pull request carries a **ThetaBase Safety Layer** comment.

If not, the App's **Advanced** tab lists every delivery with its request,
response and a **Redeliver** button. The response body says what happened —
including `"not bound to a project"` if step 5's binding is missing.

---

## What the App can and cannot do

Worth stating plainly, because it is what a customer will ask.

**Can:** read pull-request events on installed repositories; post and edit its
own comments; create, merge and discard branches **inside ThetaBase**.

**Cannot:** read or write repository contents; push commits; merge pull
requests; see any repository it was not installed on. It has no `contents`
permission at all.

**The credential ThetaBase holds** is an installation token, scoped to one
customer's installation and expiring in an hour. It is derived on demand from
the private key and never stored. That is the whole reason for the App: a
personal access token carries one person's permissions across everything they
can reach, dies when they leave the company, and would make a customer hand over
access to their entire account.

---

## Keeping the secrets safe

The private key **is** the App. Anyone holding it can act as ThetaBase on every
installation.

- Never commit it. Mount it as a secret, or inject the PEM as an environment
  variable.
- `ThetaBase` never logs it: `AppCredentials` implements `Debug` with the key
  redacted, because a key in a `Debug` is a key in the first error anyone logs
  and from there in every log aggregator that line reaches.
- Rotate it from the App settings page. Add the new key, deploy, then remove the
  old one — GitHub allows several at once so a rotation needs no downtime.
- If it leaks: delete the key on GitHub first, then investigate. A revoked key
  cannot mint a JWT, and every installation token derived from it dies within
  the hour.

---

## Related code

| What | Where |
|---|---|
| Webhook signature verification | `crates/theta-control/src/github.rs` |
| App JWT and installation tokens | `crates/theta-control/src/github_app.rs` |
| Posting and updating the comment | `crates/theta-control/src/github_api.rs` |
| Rendering the Safety Layer's verdict | `crates/theta-control/src/pr_comment.rs` |
| Running the branch actions | `crates/theta-control/src/preview.rs` |
