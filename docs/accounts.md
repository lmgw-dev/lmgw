# Sign-in accounts on the desktop

By default the desktop app has one owner: the lmgw window signs itself in,
and other browsers sign in with the `owner:dashboard` key or the login link
in the log. Turning on **accounts** lets several people sign in to the same
dashboard, each with their own name and password. The server image always has
accounts ([docs/server.md](server.md)); on the desktop they are a switch, off
by default.

Every account is a full administrator of the same gateway. Everyone sees the
same models, keys, chats and usage, and any account can set another account's
password. Private data per account and other roles are not built yet.

---

## 1. Turning accounts on

**Settings → Network & access → Accounts → Turn accounts on…** asks first:

> Everyone who signs in to this dashboard will need an account. Other
> browsers signed in now sign in again; this one stays signed in.

What happens when you confirm:

- The browser that turned it on gets a session and stays signed in. Every
  other browser signed in with an owner key sees the sign-in card next time.
- The lmgw window and the tray's **Open in Browser** keep working: they always
  sign in account 1. So does the login link in the log.
- Nothing moves. Account 1 already exists on every desktop (its sign-in name
  is `owner`) and already holds every owner key.
- **Require gateway API keys on /v1** is not touched. Accounts are about the
  dashboard; `/v1` and `/mcp` clients keep using keys as before.
- The audit log starts, with the switch as its first entry.

**Set account 1's password first.** While it has none, a banner says so. The
Accounts card sets it without asking for a current one, and in the same form
asks for a sign-in name: leave it empty to keep `owner`. This is the only time
account 1's name can change. Passwords need at least 12 characters.

**More accounts**: **Add account** (a sign-in name and a display name) answers
a set-password link, valid 24 hours, to hand to that person. The card also
disables and deletes accounts, lists and ends their sessions, and sets their
passwords. **Session lifetime** (30 days by default) is how long a browser
stays signed in. **Audit log** lists sign-ins and changes;
**Keep audit entries** sets how long they stay.

A failed sign-in makes the next one for that name wait 1 s, doubling up to 15
minutes, and the refusal says how long. There is no lockout after a number of
failures. Browsers that signed in to an account before are held only by
their own failures; an unknown browser guessing a name can hold that name up
for as long as 15 minutes at a time.

## 2. Signing in from other machines

lmgw accepts a password over plain HTTP only from the same machine. A browser
on another machine needs the TLS listener (section 4) or a reverse proxy
that terminates TLS in front of lmgw (proxy mode). The switch's row says so
while neither is set. With accounts on and TLS set up, people open the public
URL (`https://lmgw.example:8443/`) and sign in with their name and password.

## 3. Turning accounts off

**Turn accounts off…** names the numbers before it acts, for example:

> Turning accounts off signs every browser out and pauses the keys of 2 other
> account(s) (3 key(s)). The 3 account(s), their passwords and the audit log
> (its size is under Audit log, below) are kept and come back when you turn
> accounts on again.

- **Every session ends**, as a "Sign out everywhere" for every account. If you
  are account 1, your browser gets the old owner-key sign-in in the same step;
  any other account's browser is signed out.
- **Other accounts' owner keys are paused**, not deleted. Requests with them
  are refused with a sentence that names the account, the Keys card shows
  them as paused, and they work again when accounts come back on. Account 1's
  keys stay ordinary owner keys. Client, device and agent keys any account
  made keep working.
- **Kept and unused**: the accounts with their passwords, known browsers,
  unused set-password links (refused while accounts are off), who made which
  key or device, and the audit log. Nothing is written to the log while
  accounts are off. The card says what is kept, with the counts.

**Turning it on again** finds all of that: the accounts and passwords, the
paused keys working, the audit log going on. Everyone signs in again, since
every session ended.

**Recovery.** The desktop has no command line for accounts. The lmgw window
always signs in account 1, whose Accounts card can set any account's password.

## 4. TLS on the desktop

The desktop's TLS listener is the same as the server image's, set on
**Settings → Network & access → TLS**. It serves the dashboard and the API
over HTTPS beside the plain listener. It is not tied to accounts: a browser on
another machine also needs a secure context for dictation and clipboard copy.

| Row | What to set |
| --- | --- |
| Certificate | Path to the PEM chain, leaf first, readable by your account |
| Private key | Path to its PEM key, owned by your account, mode 600 |
| TLS address | Where it listens; the row proposes `0.0.0.0:8443` (every address on port 8443) |
| Handshake timeout | How long a client may take for its TLS handshake |
| Public URL | The address other machines open, such as `https://lmgw.example:8443`; links, snippets, pairing proposals and agent origins are built from it |
| Trusted proxies | Only for proxy mode: the address or network of a reverse proxy in front of lmgw |

- Certificate, Private key and TLS address apply at the next start: their
  rows say "restart to apply", and the tray's **Restart gateway** applies
  them. Public URL and Trusted proxies apply at once.
- **Renewal needs no restart.** Put the renewed pair at the same paths and
  lmgw serves it from the next connection on. A pair that does not load keeps
  the old one serving, with the reason on the TLS card.
- The TLS card shows the certificate it serves and warns of an expiry within
  14 days, a key that others can read, and a public URL or agent suffix the
  certificate does not cover.
- The plain listener stays where **Bind address** puts it, because the lmgw
  window loads it. With accounts on, its row says that passwords from other
  machines are accepted on the TLS address only.
- WebRTC calls through the TLS address use **Settings → Realtime → WebRTC
  calls → TLS UDP port**, on the TLS port's number by default.
- Open the port in the firewall yourself; lmgw does not touch it:

  ```sh
  sudo firewall-cmd --permanent --add-port=8443/tcp --add-port=8443/udp
  sudo firewall-cmd --reload
  ```

Agent pages seen from other machines need their own suffix, a wildcard DNS
record and a certificate that covers it, as on a server
([docs/server.md](server.md), "Agent origins on a server").

## 5. Not yet

- Private data per account, roles other than full administrator, per-account
  quotas and model access.
- OIDC sign-in, an idle timeout for sessions, renaming an account after its
  first password.
- A command line for accounts on the desktop.
- Built-in ACME; the certificate comes from you.
