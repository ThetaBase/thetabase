# Provisioning & Identity Flow Specification

ThetaBase v1

---

## 1. Goal

Zero dashboard visits for day-to-day work. One login, ever. Company/project context resolved from conversational instructions, not manual configuration. Dashboard reserved strictly for billing/upgrade/cancel.

---

## 2. Identity Model

```
User (Google/GitHub identity)
 └─ Organizations (auto-discovered from OAuth scopes + explicit invites)
     └─ Projects (created explicitly or on first "start a db in..." instruction)
         └─ Environments (dev / preview-per-PR / prod)
             └─ thetad instance
```

- **Login**: `theta login` → OAuth flow (Google primary, GitHub secondary) in default browser, once per machine. Produces a long-lived user identity token stored in local secure storage (OS keychain where available).
- **Org discovery**: on first login, the Control Plane resolves known orgs from the OAuth provider (Google Workspace domains, GitHub org memberships) and presents them as available contexts — no manual "add organization" step required for orgs the user is already a verified member of.
- **Project resolution**: projects are created lazily. An instruction like "start a db in Company B / project X" triggers:
  1. Fuzzy-match "Company B" against known orgs for this user.
     - Single confident match → proceed.
     - Ambiguous/no match → single clarifying prompt (not a form, a single question).
  2. Fuzzy-match/lookup "project X" within that org.
     - Exists → resolve to it.
     - Doesn't exist → single confirmation ("Create project 'X' under Company B?") then create.
  3. Environment defaults to `dev` unless the instruction or git branch context implies otherwise (e.g., running inside a PR branch → preview environment automatically, per Journey C/branch-per-PR flow).

---

## 3. Token Issuance Flow

1. Agent/CLI tool calls Control Plane's `resolveContext(orgHint, projectHint, envHint)` using the user's long-lived identity token as auth.
2. Control Plane resolves/creates the org/project/environment as above.
3. Control Plane mints a **short-lived, project+environment-scoped session token** (see Threat Model spec for lifecycle/security details).
4. Token is injected directly into the calling runtime's environment (agent process env, or the app's server environment via the same mechanism Vercel already uses for build-time env injection) — never written to a file the user has to manage, never displayed by default.
5. `thetad` for that project validates the token scope on every request; tokens are useless outside their exact project+environment.

---

## 4. Context Switching Mid-Session

- No logout/login cycle required. A new instruction referencing a different org/project triggers a new `resolveContext` call and a new scoped token for that context — the previous context's token remains valid independently (parallel work across companies is a first-class case, not an edge case).
- CLI/agent tooling maintains a small local cache of "recently resolved contexts" to avoid re-prompting for ambiguous matches repeatedly in one session.

---

## 5. Portal Scope (deliberately minimal)

The web surface exists for exactly these actions and nothing else in v1:

**Billing**
- View current plan/tier and usage against it
- Upgrade/downgrade/cancel subscription
- View billing history/invoices
- (Optional, explicit-action-only) view/rotate a project's current scoped tokens for debugging purposes

**Account administration** — added after the original cut, deliberately:
- View the organisation's members and their roles; set a role; remove a member
- Answer support-access requests from ThetaBase staff: allow, refuse, withdraw

Provisioning, key management, and schema/branch operations are **not present in
the portal UI in v1** — this is an intentional scope cut, not an oversight, to
keep the product honest about where its interface actually lives (CLI/agent/MCP
tooling).

**The test for what belongs here is who does it.** Billing and membership are
things a *person* does, on an account, at a keyboard, perhaps once a month. A
schema change, a branch, a query, a key rotation are things an *agent* does, and
those live in the CLI and the MCP server. The two lists do not overlap and the
boundary is not a matter of taste: a portal that grew a query box would be
competing with the product's actual interface, and a portal that could answer a
gated change would be a second place the Safety Layer's human half lives.

Membership is in scope precisely because it is upstream of that half: roles
decide who can answer a gate, so the list of members is the set of people the
Safety Layer can escalate to. Managing it in a browser is reasonable; using it
is not.

`crates/theta-control/tests/portal_scope.rs` checks this against the file
rather than trusting the paragraph.

---

## 6. Failure & Edge Cases

| Case | Behavior |
|---|---|
| User's OAuth org membership revoked externally (e.g., leaves company) | Existing project session tokens for that org's projects are invalidated on next Control Plane sync; user is prompted to re-auth if they attempt further access |
| Ambiguous org/project name (two orgs with similar names) | Single clarifying question, never a silent guess |
| Agent attempts to resolve a context the user has no access to | Hard denial at the Control Plane, logged, no information leaked about the target project's existence beyond "not found or no access" |
| Token injection target doesn't support env injection (unusual runtime) | Falls back to a documented, explicit `theta token print --project X` command as an escape hatch — logged as an explicit user action, not silent |

---

## 7. Roles

Multi-user orgs need a policy for who can create new projects versus who can only operate within existing ones. Three roles, ordered:

| Role | Can |
|---|---|
| **Member** | Work in the projects that exist: resolve a context, be minted a scoped token, read the audit trail. |
| **Admin** | Everything a member can, plus create projects, write a project's safety policy, and revoke tokens. |
| **Owner** | Everything, plus billing and managing other people's roles. |

Four properties are load-bearing:

- **Roles are not in the identity token.** The token carries the orgs a *provider* vouched for — that is the provider's fact, and it authenticates membership. What a user may *do* is ThetaBase's fact, looked up on every request. A role baked into a token stays in force until that token expires, so an owner revoking an admin's rights would be telling them "you are no longer an admin, from tomorrow".

- **Membership and role are checked together, and neither substitutes for the other.** Leaving the company costs you the first at your next login or immediately via revocation; being demoted costs you the second at once.

- **The first member of an org owns it.** A single-user org gives that user every capability with nothing to configure, which is what keeps this model out of solo v1's way. Discovery never overwrites an existing role, so logging in again cannot silently demote anyone.

- **An org can never be left without an owner.** An org with no owner cannot have its roles fixed, its billing changed, or its projects deleted — by anyone, including whoever just demoted themselves.

Writing a project's safety policy sits with admin rather than owner because it is operational work, but never with member: raising a ceiling is indistinguishable in effect from being handed the right to ignore it (`07-agent-safety-layer.md` §7).

A caller who is not in an org gets the same answer as one naming an org that does not exist, so this cannot be used to enumerate tenants. A caller who *is* in the org but lacks the role is told which boundary they hit — concealing that would make a routine "ask your owner" read as a broken API.

---

## 8. Design-partner terms

The first customers are on negotiated terms. The two ways to handle that are a
column somebody edits by hand and a first-class object; it is an object.

**Four kinds, deliberately not one.** A refund returns money already taken and is
bounded by a specific charge. A credit is money given and is bounded by its own
value. A coupon is a term offered to a prospect who is not a customer yet, and is
*redeemed* rather than applied. A plan override is not an amount at all — it is a
price that keeps applying until it stops. One "adjustment" with a sign would mean
a single validation path trying to be correct about four different bounds, and
the way that fails is that the loosest one wins.

**Each carries who authorised it, what it is worth, what it applies to, and when
it ends.** Expiry is required, not optional, for the reason a data-access grant's
is: an unbounded grant of free usage is a subscription nobody decided to give
away, and it is discovered in a revenue report a year later rather than at
renewal. An over-long term is **refused, not shortened** — quietly capping a
two-year ask produces a customer who believes they have something they do not
(`docs/INVARIANTS.md` invariant 3).

**Refunds are bounded by what remains refundable**, not merely by the charge.
Refunding twice is far likelier than refunding too much once, because two people
working one support ticket is an ordinary Tuesday.

**Issuance and redemption are different systems.** Issuing is an operator act
under `AdministerBilling` — the role separation M9.5 built, which until now had
nothing to guard. Redeeming is reached from signup, by somebody with no
credential at all.

**Redemption cannot create a coupon.** There is no upsert: it finds an existing
code or fails. Collapsing the two would make the anonymous signup form a route
into the billing system, where whatever validates "may this caller create value"
is the same code that runs for a stranger's form post, one missing branch away
from letting them.

**No billing state makes a customer's data unretrievable**, including a written-
off account. Non-payment is a commercial dispute, and a commercial dispute is not
settled by deleting somebody's database. A suspended project is unreachable, not
erased.
