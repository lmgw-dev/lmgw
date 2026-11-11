//! The account commands and `login-link` (headless server design §5.7,
//! D16, D28, D57).
//!
//! **Through the running gateway**: the gateway holds keys, sessions and
//! nonces in memory, so a direct database write would not reach it, and a
//! nonce exists only in the process that exchanges it. While a gateway holds
//! the data dir's lock, the command reads account 1's door key from the
//! database (read-only, no migration; it runs as root in the gateway's own
//! container, which is the trust it needs) and calls the plain listener's
//! `/api/op/…` on loopback, on the stored `bind_addr`'s port, with it,
//! sending `x-lmgw-client: cli`. Never through a proxy: the door key and a
//! password stay on this machine's loopback whatever `HTTP_PROXY` says.
//!
//! **With the service stopped** the command takes the data dir's lock
//! itself and writes the database (`BEGIN IMMEDIATE`, as every write), and
//! says that the change applies at the gateway's next start. It never
//! migrates: a database this image has not started on yet is refused,
//! since migrating is the gateway start's step. `login-link` has no offline
//! form. A lock held by a gateway the loopback does not reach (a one-off
//! container beside a running one) is refused, naming its holder.
//!
//! A new password is read before the lock is tried, so a person at the
//! prompt never holds the data dir against a gateway that starts meanwhile.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use lmgw_core::state::data_dir_lock::{Busy, DataDirLock};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::cli::Account;

/// The lock file's program name while a command holds the data dir.
pub const PROGRAM: &str = "lmgw-server account";

/// Where a new password comes from: the terminal (or a pipe) in the
/// binary, stated lines in a test.
pub trait Passwords {
    /// One password, after `prompt` where someone types it.
    fn read(&mut self, prompt: &str) -> Result<String, String>;
}

/// The process's stdin: typed without echo on a terminal, one line each
/// from a pipe.
pub struct Stdin;

impl Passwords for Stdin {
    fn read(&mut self, prompt: &str) -> Result<String, String> {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            return Piped(stdin.lock()).read(prompt);
        }
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        // ECHONL echoes the newline that ends the typed line (and with echo
        // left on, the terminal shows it anyway), so the next prompt starts
        // on a line of its own without one of ours.
        let _echo = EchoOff::new();
        Piped(stdin.lock()).read(prompt)
    }
}

/// Lines of a reader: a pipe's, or a test's.
pub struct Piped<R: BufRead>(pub R);

impl<R: BufRead> Passwords for Piped<R> {
    fn read(&mut self, _prompt: &str) -> Result<String, String> {
        let mut line = String::new();
        let n = self
            .0
            .read_line(&mut line)
            .map_err(|e| format!("reading the password: {e}"))?;
        if n == 0 {
            return Err("no password on stdin (it ended); nothing was changed".into());
        }
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

/// Run `cmd` on `data_dir`, a new password read from `passwords`: the
/// text to print, or why it failed.
pub async fn run(
    cmd: Account,
    data_dir: &Path,
    passwords: &mut dyn Passwords,
) -> Result<String, String> {
    let db = data_dir.join(lmgw_core::state::fresh_install::DB_FILE);
    if !db.exists() {
        return Err(format!(
            "no gateway database at {} — has the gateway started on this data dir once?",
            db.display()
        ));
    }
    // The names and the password before the lock (see the module's note).
    let password = match &cmd {
        Account::SetPassword { login, rename } => {
            lmgw_core::accounts::normalize_login(login)?;
            if let Some(r) = rename {
                lmgw_core::accounts::normalize_login(r)?;
            }
            Some(read_new_password(passwords)?)
        }
        Account::Link(login) | Account::LoginLink(Some(login)) => {
            lmgw_core::accounts::normalize_login(login)?;
            None
        }
        Account::List | Account::LoginLink(None) => None,
    };
    match DataDirLock::try_acquire(data_dir, PROGRAM)? {
        Ok(lock) => {
            let out = offline(cmd, password, data_dir, &db).await;
            drop(lock);
            out
        }
        Err(busy) => online(cmd, password, &db, &busy).await,
    }
}

// ---------------------------------------------------------------------------
// Through the running gateway
// ---------------------------------------------------------------------------

struct Gateway {
    base: String,
    key: String,
    http: reqwest::Client,
}

/// The client of the calls through the running gateway: no proxy, whatever
/// the environment names, since every call carries account 1's door key
/// and some a password.
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| format!("building the HTTP client: {e}"))
}

impl Gateway {
    async fn op(&self, name: &str, args: Value) -> Result<Value, String> {
        let resp = self
            .http
            .post(format!("{}/api/op/{name}", self.base))
            .bearer_auth(&self.key)
            .header(
                lmgw_core::accounts::CLIENT_HEADER,
                lmgw_core::accounts::CLIENT_CLI,
            )
            .json(&args)
            .send()
            .await
            .map_err(|e| format!("calling the gateway at {}: {e}", self.base))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        Err(match body.get("message").and_then(Value::as_str) {
            Some(m) => format!("{m} ({status})"),
            None => format!("the gateway answered {status}"),
        })
    }

    async fn account_id(&self, login: &str) -> Result<i64, String> {
        let list = self.op("account_list", json!({})).await?;
        let wanted = lmgw_core::accounts::normalize_login(login)?;
        list["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["login"].as_str() == Some(wanted.as_str()))
            .and_then(|a| a["id"].as_i64())
            .ok_or_else(|| format!("no account '{wanted}'"))
    }
}

/// The plain listener on this machine's loopback (D57): the stored
/// `bind_addr`'s port on `127.0.0.1`, or `[::1]` for an IPv6 address; a
/// loopback address it names is kept as it is. Loopback, not the stored
/// address: a password travels over plain HTTP only from a direct
/// loopback peer (D26).
pub fn loopback_base(bind_addr: &str) -> Result<String, String> {
    let addr: std::net::SocketAddr = bind_addr
        .trim()
        .parse()
        .map_err(|_| format!("the stored bind address '{bind_addr}' does not parse"))?;
    let host = match addr {
        std::net::SocketAddr::V4(a) if a.ip().is_loopback() => a.ip().to_string(),
        std::net::SocketAddr::V4(_) => "127.0.0.1".to_string(),
        std::net::SocketAddr::V6(a) if a.ip().is_loopback() => format!("[{}]", a.ip()),
        std::net::SocketAddr::V6(_) => "[::1]".to_string(),
    };
    Ok(format!("http://{host}:{}", addr.port()))
}

/// Why a held lock with no listener answering on loopback is refused: the
/// holder, and what to do, told apart by the lock file's host line (D57).
pub fn unreachable(busy: &Busy, base: &str) -> String {
    let dir = busy.path.parent().map_or_else(
        || busy.path.display().to_string(),
        |d| d.display().to_string(),
    );
    match busy.same_host {
        Some(true) => format!(
            "{} holds the data dir {dir}, here, but nothing answers at {base} yet: it may still \
             be starting (run the command again in a moment), or its stored bind address \
             names no loopback port",
            busy.holder
        ),
        _ => format!(
            "{} holds the data dir {dir}, and nothing answers at {base} from here: it runs in \
             another container. Run the command inside that one (podman exec -it lmgw \
             lmgw-server …), or stop the service to run it on the data volume",
            busy.holder
        ),
    }
}

async fn online(
    cmd: Account,
    password: Option<String>,
    db: &Path,
    busy: &Busy,
) -> Result<String, String> {
    let pool = store::open_read_only(db)
        .await
        .map_err(|e| format!("reading {}: {e}", db.display()))?;
    let snap = store::load_snapshot(&pool)
        .await
        .map_err(|e| format!("reading {}: {e}", db.display()))?;
    pool.close().await;
    let key = snap
        .owner_key(lmgw_core::agents::token::OWNER_DASHBOARD)
        .ok_or("account 1's sign-in key is missing or disabled in the database")?
        .to_string();
    let gw = Gateway {
        base: loopback_base(&snap.settings.bind_addr)?,
        key,
        http: http_client()?,
    };
    // Is it this container's gateway? A one-off container on a volume a
    // running gateway holds reaches no listener here.
    if gw
        .http
        .get(format!("{}/api/version", gw.base))
        .send()
        .await
        .is_err()
    {
        return Err(unreachable(busy, &gw.base));
    }
    match cmd {
        Account::List => Ok(list_text(&gw.op("account_list", json!({})).await?)),
        Account::SetPassword { login, rename } => {
            let id = gw.account_id(&login).await?;
            let mut args = json!({ "id": id, "password": password.unwrap_or_default() });
            if let Some(r) = rename {
                args["login"] = json!(r);
            }
            let done = gw.op("account_password_set", args).await?;
            Ok(done["message"].as_str().unwrap_or("done").to_string())
        }
        Account::Link(login) => {
            let id = gw.account_id(&login).await?;
            Ok(link_text(
                &gw.op("account_password_link", json!({ "id": id })).await?,
                "set a password",
            ))
        }
        Account::LoginLink(login) => {
            let id = match login {
                Some(l) => gw.account_id(&l).await?,
                None => lmgw_core::accounts::ACCOUNT_ONE,
            };
            Ok(link_text(
                &gw.op("account_login_link", json!({ "id": id })).await?,
                "sign in",
            ))
        }
    }
}

fn list_text(list: &Value) -> String {
    let mut out = String::from(
        "id  login                display name         password  enabled  failed  sessions\n",
    );
    for a in list["accounts"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "{:<3} {:<20} {:<20} {:<9} {:<8} {:<7} {}\n",
            a["id"].as_i64().unwrap_or(0),
            a["login"].as_str().unwrap_or(""),
            a["display_name"].as_str().unwrap_or(""),
            if a["has_password"].as_bool() == Some(true) {
                "set"
            } else {
                "none"
            },
            if a["enabled"].as_bool() == Some(true) {
                "yes"
            } else {
                "no"
            },
            a["failed_logins"].as_i64().unwrap_or(0),
            a["sessions"].as_i64().unwrap_or(0),
        ));
    }
    out.trim_end().to_string()
}

fn link_text(link: &Value, what: &str) -> String {
    format!(
        "{what} for account '{}': {} ({})",
        link["login"].as_str().unwrap_or("?"),
        link["url"].as_str().unwrap_or("?"),
        link["validity"].as_str().unwrap_or("")
    )
}

// ---------------------------------------------------------------------------
// On the data volume, the service stopped
// ---------------------------------------------------------------------------

const NEXT_START: &str = "it applies at the gateway's next start";

/// The offline refusal of a database this image has not started on.
pub const NOT_MIGRATED: &str = "this database is older than this image (a migration of this \
     version has not run on it): start the gateway once with this image first, then run the \
     command again";

async fn offline(
    cmd: Account,
    password: Option<String>,
    data_dir: &Path,
    db: &Path,
) -> Result<String, String> {
    if let Account::LoginLink(_) = cmd {
        return Err(
            "login-link needs the running gateway (a one-time sign-in link lives in its \
             memory); start the service, then run podman exec -it lmgw lmgw-server login-link"
                .into(),
        );
    }
    let pool = store::open_current(db, Some(&store::backups_dir(data_dir)))
        .await
        .map_err(|e| format!("opening {}: {e}", db.display()))?
        .ok_or(NOT_MIGRATED)?;
    let out = offline_on(&pool, cmd, password).await;
    pool.close().await;
    out
}

async fn offline_on(
    pool: &store::SqlitePool,
    cmd: Account,
    password: Option<String>,
) -> Result<String, String> {
    let snap = store::load_snapshot(pool)
        .await
        .map_err(|e| format!("reading the database: {e}"))?;
    let find = |login: &str| -> Result<lmgw_core::accounts::Account, String> {
        let wanted = lmgw_core::accounts::normalize_login(login)?;
        snap.account_by_login(&wanted)
            .cloned()
            .ok_or_else(|| format!("no account '{wanted}'"))
    };
    match cmd {
        Account::List => {
            let sessions = store::list_sessions(pool)
                .await
                .map_err(|e| e.to_string())?;
            let rows: Vec<Value> = snap
                .accounts
                .iter()
                .map(|a| {
                    json!({
                        "id": a.id, "login": a.login, "display_name": a.display_name,
                        "has_password": a.has_password, "enabled": a.enabled,
                        "failed_logins": a.failed_logins,
                        "sessions": sessions.iter().filter(|s| s.account_id == a.id).count(),
                    })
                })
                .collect();
            Ok(list_text(&json!({ "accounts": rows })))
        }
        Account::SetPassword { login, rename } => {
            let account = find(&login)?;
            let rename = lmgw_core::accounts::ops::first_login(&snap, &account, rename.as_deref())
                .map_err(|r| r.message)?;
            let normalized = lmgw_core::accounts::password::checked(&password.unwrap_or_default())?;
            let hash = tokio::task::spawn_blocking(move || {
                lmgw_core::accounts::password::hash(&normalized)
            })
            .await
            .map_err(|e| e.to_string())??;
            store::set_account_password(pool, account.id, &hash, rename.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            let ended = store::delete_sessions(
                pool,
                lmgw_core::accounts::sessions::Which::Account {
                    account_id: account.id,
                    except: None,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            let who = rename.as_deref().unwrap_or(&account.login);
            let mut target = lmgw_core::audit::account_target(&snap, account.id);
            if let Some(new) = &rename {
                target.insert("new_login".into(), new.clone().into());
            }
            let audited = audit(pool, &snap, "account_password_set", target).await;
            Ok(format!(
                "the password of '{who}' is set and its {ended} session(s) ended; {NEXT_START}{audited}"
            ))
        }
        Account::Link(login) => {
            let account = find(&login)?;
            let (url, validity) = offline_link(pool, &snap, account.id).await?;
            let target = lmgw_core::audit::account_target(&snap, account.id);
            let audited = audit(pool, &snap, "account_password_link", target).await;
            Ok(format!(
                "set a password for account '{}': {url} ({validity}); {NEXT_START}{audited}",
                account.login
            ))
        }
        Account::LoginLink(_) => unreachable!("refused by the caller"),
    }
}

/// The audit log's entry for an offline write (§5.8, `via =
/// cli-offline`), named as the op the running gateway would have run: ""
/// once written, else the sentence that says it was not.
async fn audit(
    pool: &store::SqlitePool,
    snap: &lmgw_core::config::Snapshot,
    action: &str,
    target: serde_json::Map<String, Value>,
) -> String {
    let entry = lmgw_core::audit::offline_cli().entry(action, target, 200);
    if let Err(e) = store::insert_audit(pool, &entry, 1).await {
        return format!(" (the audit log could not record it: {e})");
    }
    // The retention, after the entry: the next start prunes too.
    match store::prune_audit(pool, snap.settings.accounts.audit_days).await {
        Ok(_) => String::new(),
        Err(e) => format!(" (the audit log kept its old entries: pruning them failed: {e})"),
    }
}

/// The origin an offline link is built on (§4.3): `public_url`, else the
/// TLS listener the stored settings describe (the last start wrote them,
/// its pinned `LMGW_TLS_*` included), else the plain listener. Nothing
/// serves while the command runs, so the stored TLS listener is the best
/// word for where the next start serves.
pub fn offline_base(s: &lmgw_core::config::Settings) -> String {
    let tls = s.tls.listener().map(|config| lmgw_core::net::ServingTls {
        addr: config.addr,
        config,
    });
    lmgw_core::net::public_base_url(s, tls.as_ref())
}

/// A set-password link written by the command itself: the token in the
/// table, the URL on [`offline_base`].
async fn offline_link(
    pool: &store::SqlitePool,
    snap: &lmgw_core::config::Snapshot,
    account: i64,
) -> Result<(String, String), String> {
    let (token, hash) = lmgw_core::accounts::mint_secret();
    let (expires_at, valid_until) =
        lmgw_core::accounts::links::expiry_after(lmgw_core::accounts::links::PASSWORD_LINK_TTL);
    store::put_link_token(pool, account, &hash, &expires_at)
        .await
        .map_err(|e| e.to_string())?;
    Ok((
        format!(
            "{}{}#token={token}",
            offline_base(&snap.settings),
            lmgw_core::accounts::links::SET_PASSWORD_PATH
        ),
        valid_until,
    ))
}

// ---------------------------------------------------------------------------
// The password from the terminal
// ---------------------------------------------------------------------------

/// The new password, read twice.
fn read_new_password(passwords: &mut dyn Passwords) -> Result<String, String> {
    let first = passwords.read("new password: ")?;
    let second = passwords.read("again: ")?;
    if first != second {
        return Err("the two passwords differ; nothing was changed".into());
    }
    Ok(first)
}

/// The terminal's echo switched off while it lives.
struct EchoOff {
    saved: libc::termios,
}

impl EchoOff {
    fn new() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr on stdin's descriptor with a termios
        // the call fills; nothing else holds it.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return None;
            }
            let saved = t;
            t.c_lflag &= !libc::ECHO;
            t.c_lflag |= libc::ECHONL;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) != 0 {
                return None;
            }
            Some(Self { saved })
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restores what `new` read.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

/// The data dir the commands run on, after the start's own refusals: only
/// in the server image or as a dev instance (D1), and a dev instance never
/// on the installed app's data dir. Without them a dev build on a desktop
/// could migrate and write the tray app's database, which the tray holds
/// no data-dir lock on. `LMGW_DATA_DIR` is required (the image's `/data`).
pub fn data_dir(env: &crate::env::ServerEnv) -> Result<PathBuf, String> {
    if crate::start::refusal(env).is_some() {
        return Err(
            "lmgw-server's account commands run only in the server image \
                    (LMGW_DEPLOYMENT=container) or as a dev instance (LMGW_DEV=1, on a scratch \
                    data dir): on a desktop the database is the tray app's, and the tray holds \
                    no lock that would keep a command from writing it under it"
                .to_string(),
        );
    }
    let dir = env
        .data_dir
        .clone()
        .ok_or_else(|| "LMGW_DATA_DIR is not set (or empty); the image sets /data".to_string())?;
    if env.dev {
        crate::start::refuse_installed_data_dir(&dir).map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loopback_listener_is_on_the_stored_port() {
        assert_eq!(
            loopback_base("0.0.0.0:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            loopback_base("192.0.2.2:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
        assert_eq!(loopback_base("127.0.0.5:9").unwrap(), "http://127.0.0.5:9");
        assert_eq!(loopback_base("[::]:8787").unwrap(), "http://[::1]:8787");
        assert_eq!(
            loopback_base("[2001:db8::2]:8787").unwrap(),
            "http://[::1]:8787"
        );
        assert!(loopback_base("everywhere").is_err());
    }

    #[test]
    fn an_offline_link_follows_the_stored_tls_listener() {
        let mut s = lmgw_core::config::Settings {
            bind_addr: "0.0.0.0:8787".into(),
            ..Default::default()
        };
        assert!(
            offline_base(&s).starts_with("http://"),
            "{}",
            offline_base(&s)
        );
        s.tls.cert_path = "/etc/lmgw/tls/tls.crt".into();
        s.tls.key_path = "/etc/lmgw/tls/tls.key".into();
        s.tls.listen = "127.0.0.1:8443".into();
        assert_eq!(offline_base(&s), "https://127.0.0.1:8443");
        s.public_url = "https://lmgw.example".into();
        assert_eq!(offline_base(&s), "https://lmgw.example");
    }
}
