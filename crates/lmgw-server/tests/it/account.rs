//! The account commands (headless server design §5.7, D57; §12's "the CLI
//! through a running gateway, offline under the lock, and refused offline
//! while the lock is held"): `account::run` on a scratch data dir that a
//! container-mode start made, through a test gateway served in this process,
//! and the built binary for what the process adds (the start's refusals, no
//! proxy). Nothing reaches the machine's podman or GPU.

use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use lmgw_core::state::data_dir_lock::DataDirLock;
use lmgw_server::account::{self, Passwords, Piped};
use lmgw_server::cli::Account;
use lmgw_server::start;
use tokio::sync::mpsc;

use crate::support::{container_env, host, recorder, EmptyPodman};

const PASSWORD: &str = "correct horse battery staple";

/// `(action, via, outcome)` of every audit entry in `pool`, oldest first,
/// and the whole log as one text (headless server design §5.8).
async fn audit_of(pool: &lmgw_core::store::SqlitePool) -> (Vec<(String, String, i64)>, String) {
    let rows: Vec<(String, String, i64, String, String)> =
        sqlx::query_as("SELECT action, via, outcome, principal, target FROM audit_log ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    let text = format!("{rows:?}");
    (
        rows.into_iter().map(|(a, v, o, _, _)| (a, v, o)).collect(),
        text,
    )
}

/// A port nothing listens on now, for a start whose stored `bind_addr` must
/// be its real port (the pinned `LMGW_BIND_ADDR` is what it stores).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A container-mode start on `dir`, its listener on `port` (0: the OS's
/// pick, stored as 0): the database made and migrated, the lock held until
/// the answer is dropped.
async fn prepared(dir: &Path, port: u16) -> start::Ready {
    let mut env = container_env(dir, false);
    env.container.bind_addr = Some(format!("127.0.0.1:{port}").parse().unwrap());
    let (exit, _) = recorder();
    let (_signals, mut rx) = mpsc::channel(2);
    start::prepare(
        &env,
        host(&Arc::new(EmptyPodman::default())),
        &mut rx,
        &exit,
    )
    .await
    .unwrap()
    .expect("no signal: ready")
}

/// The new password twice, as a pipe gives it.
fn piped(password: &str) -> Piped<Cursor<Vec<u8>>> {
    Piped(Cursor::new(
        format!("{password}\n{password}\n").into_bytes(),
    ))
}

/// A command that reads no password.
struct NoPassword;

impl Passwords for NoPassword {
    fn read(&mut self, prompt: &str) -> Result<String, String> {
        panic!("asked for a password ({prompt})")
    }
}

/// The password, read while the data dir's lock is free: the command reads
/// it before it tries the lock.
struct ReadWhileFree<'a> {
    dir: &'a Path,
    reads: usize,
}

impl Passwords for ReadWhileFree<'_> {
    fn read(&mut self, _prompt: &str) -> Result<String, String> {
        let probe = DataDirLock::try_acquire(self.dir, "probe").unwrap();
        assert!(probe.is_ok(), "the lock is held while a password is read");
        self.reads += 1;
        Ok(PASSWORD.to_string())
    }
}

#[tokio::test]
async fn offline_on_the_data_volume_under_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    drop(prepared(dir.path(), 0).await);

    let list = account::run(Account::List, dir.path(), &mut NoPassword)
        .await
        .unwrap();
    assert!(list.contains("owner") && list.contains("none"), "{list}");

    let mut typed = ReadWhileFree {
        dir: dir.path(),
        reads: 0,
    };
    let set = Account::SetPassword {
        login: "Owner".into(),
        rename: None,
    };
    let said = account::run(set, dir.path(), &mut typed).await.unwrap();
    assert_eq!(typed.reads, 2);
    assert!(
        said.starts_with("the password of 'owner' is set and its 0 session(s) ended")
            && said.ends_with("it applies at the gateway's next start"),
        "{said}"
    );
    let list = account::run(Account::List, dir.path(), &mut NoPassword)
        .await
        .unwrap();
    assert!(list.contains(" set "), "{list}");

    let link = account::run(Account::Link("owner".into()), dir.path(), &mut NoPassword)
        .await
        .unwrap();
    assert!(
        link.starts_with(
            "set a password for account 'owner': http://127.0.0.1:0/set-password#token="
        ) && link.contains("valid until")
            && link.ends_with("next start"),
        "{link}"
    );

    let e = account::run(Account::LoginLink(None), dir.path(), &mut NoPassword)
        .await
        .unwrap_err();
    assert!(e.contains("login-link needs the running gateway"), "{e}");
    let e = account::run(
        Account::SetPassword {
            login: "owner".into(),
            rename: None,
        },
        dir.path(),
        &mut Piped(Cursor::new(b"one password\nanother one\n".to_vec())),
    )
    .await
    .unwrap_err();
    assert!(e.contains("differ"), "{e}");
    let e = account::run(Account::Link("nobody".into()), dir.path(), &mut NoPassword)
        .await
        .unwrap_err();
    assert_eq!(e, "no account 'nobody'");
    // A short password is refused by the rules before anything is written.
    let e = account::run(
        Account::SetPassword {
            login: "owner".into(),
            rename: None,
        },
        dir.path(),
        &mut piped("short"),
    )
    .await
    .unwrap_err();
    assert!(e.contains("12"), "{e}");

    // The two writes, each an entry of the command line's own (§5.8), and
    // neither the password nor the link's token in any of them.
    let db = dir.path().join(lmgw_core::state::fresh_install::DB_FILE);
    let pool = lmgw_core::store::open_current(&db, None)
        .await
        .unwrap()
        .unwrap();
    let (entries, text) = audit_of(&pool).await;
    assert_eq!(
        entries,
        [
            ("account_password_set".into(), "cli-offline".into(), 200),
            ("account_password_link".into(), "cli-offline".into(), 200),
        ],
        "{text}"
    );
    assert!(text.contains("lmgw-server (gateway stopped)"), "{text}");
    let token = link
        .split_once("#token=")
        .unwrap()
        .1
        .split(' ')
        .next()
        .unwrap();
    assert!(!text.contains(PASSWORD) && !text.contains(token), "{text}");
    pool.close().await;
}

#[tokio::test]
async fn offline_a_database_this_image_has_not_migrated_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ready = prepared(dir.path(), 0).await;
    sqlx::query(
        "DELETE FROM _sqlx_migrations WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)",
    )
    .execute(&ready.state.db)
    .await
    .unwrap();
    drop(ready);
    let e = account::run(Account::List, dir.path(), &mut NoPassword)
        .await
        .unwrap_err();
    assert_eq!(e, account::NOT_MIGRATED);
    assert!(e.contains("start the gateway once with this image first"));
}

#[tokio::test]
async fn a_held_lock_nothing_answers_for_is_refused_naming_the_holder() {
    let dir = tempfile::tempdir().unwrap();
    // Stored bind_addr 127.0.0.1:0: nothing answers there.
    drop(prepared(dir.path(), 0).await);
    let held = DataDirLock::acquire(dir.path(), "lmgw-server").unwrap();

    // Held from this machine: a gateway that may still be starting.
    let e = account::run(Account::List, dir.path(), &mut NoPassword)
        .await
        .unwrap_err();
    assert!(
        e.contains(&format!("lmgw-server (pid {} on ", std::process::id()))
            && e.contains("may still be starting")
            && e.contains("http://127.0.0.1:0"),
        "{e}"
    );

    // Held by another container, as its lock file says.
    std::fs::write(held.path(), "7\n3f2a9c1e04b7\nlmgw-server\n").unwrap();
    let e = account::run(Account::List, dir.path(), &mut NoPassword)
        .await
        .unwrap_err();
    assert!(
        e.contains("lmgw-server (pid 7 on 3f2a9c1e04b7) holds the data dir")
            && e.contains("another container")
            && e.contains("podman exec -it lmgw lmgw-server")
            && e.contains("stop the service"),
        "{e}"
    );
    drop(held);
}

#[tokio::test]
async fn through_the_running_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let ready = prepared(dir.path(), port).await;
    let (signals, rx) = mpsc::channel(2);
    let serving = tokio::spawn(start::serve_until_quit(
        ready.state.clone(),
        ready.listener,
        ready.tls,
        rx,
        |_| panic!("one signal never exits"),
        |_| async {},
    ));

    let list = account::run(Account::List, dir.path(), &mut NoPassword)
        .await
        .unwrap();
    assert!(list.contains("owner") && list.contains("none"), "{list}");

    // Account 1's first password, and with it another sign-in name.
    let said = account::run(
        Account::SetPassword {
            login: "owner".into(),
            rename: Some("boss".into()),
        },
        dir.path(),
        &mut piped(PASSWORD),
    )
    .await
    .unwrap();
    assert!(said.contains("password is set"), "{said}");
    let list = account::run(Account::List, dir.path(), &mut NoPassword)
        .await
        .unwrap();
    assert!(
        list.contains("boss") && list.contains(" set ") && !list.contains("owner"),
        "{list}"
    );
    // Once it has a password the name stays.
    let e = account::run(
        Account::SetPassword {
            login: "boss".into(),
            rename: Some("chief".into()),
        },
        dir.path(),
        &mut piped(PASSWORD),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("login_fixed") || e.contains("first password"),
        "{e}"
    );

    let link = account::run(Account::Link("boss".into()), dir.path(), &mut NoPassword)
        .await
        .unwrap();
    assert!(
        link.starts_with(&format!(
            "set a password for account 'boss': http://127.0.0.1:{port}/set-password#token="
        )),
        "{link}"
    );
    let login = account::run(Account::LoginLink(None), dir.path(), &mut NoPassword)
        .await
        .unwrap();
    assert!(
        login.starts_with(&format!(
            "sign in for account 'boss': http://127.0.0.1:{port}/api/session/login?nonce="
        )),
        "{login}"
    );
    let e = account::run(
        Account::LoginLink(Some("nobody".into())),
        dir.path(),
        &mut NoPassword,
    )
    .await
    .unwrap_err();
    assert_eq!(e, "no account 'nobody'");

    // Through the gateway the gate writes them, `via = cli` from the
    // command's header (§5.7); the list is a read, so no entry.
    let (entries, text) = audit_of(&ready.state.db).await;
    let cli: Vec<_> = entries.iter().filter(|(_, v, _)| v == "cli").collect();
    let actions: Vec<&str> = cli.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        [
            "account_password_set",
            "account_password_set",
            "account_password_link",
            "account_login_link",
        ],
        "{text}"
    );
    assert_eq!(cli[0].2, 200);
    assert!(
        cli[1].2 >= 400,
        "the refused rename keeps its status: {text}"
    );
    assert!(!text.contains(PASSWORD), "{text}");

    signals.send("SIGTERM").await.unwrap();
    serving.await.unwrap().unwrap();
    drop(ready.lock);
}

/// The built binary with only `vars` in its environment, any podman call
/// failing.
fn binary(vars: &[(&str, &str)], args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_lmgw-server"));
    cmd.env_clear()
        .env("CONTAINER_HOST", "unix:///nonexistent-lmgw-wp6")
        .envs(vars.iter().copied())
        .args(args)
        .stdin(std::process::Stdio::null());
    cmd
}

#[tokio::test]
async fn the_commands_refuse_where_a_start_does() {
    // Outside a container and without LMGW_DEV.
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().display().to_string();
    let out = binary(&[("LMGW_DATA_DIR", &data)], &["account", "list"])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("account commands run only in the server image (LMGW_DEPLOYMENT=container)"),
        "{said}"
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());

    // A dev instance on the installed app's data dir, in a scratch home.
    let home = tempfile::tempdir().unwrap();
    let installed = home.path().join(".local/share/lmgw");
    std::fs::create_dir_all(&installed).unwrap();
    let out = binary(
        &[
            ("HOME", &home.path().display().to_string()),
            ("LMGW_DEV", "1"),
            ("LMGW_DATA_DIR", &installed.display().to_string()),
        ],
        &["account", "set-password", "owner"],
    )
    .output()
    .await
    .unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("is the installed app's data dir"), "{said}");
    assert!(std::fs::read_dir(&installed).unwrap().next().is_none());
}

#[tokio::test]
async fn the_calls_never_take_a_proxy() {
    // A proxy the environment names, which hangs up on every connection.
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let taken = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let taken = taken.clone();
        async move {
            while let Ok((conn, _)) = proxy.accept().await {
                taken.fetch_add(1, Ordering::SeqCst);
                drop(conn);
            }
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let ready = prepared(dir.path(), free_port()).await;
    let (signals, rx) = mpsc::channel(2);
    let serving = tokio::spawn(start::serve_until_quit(
        ready.state.clone(),
        ready.listener,
        ready.tls,
        rx,
        |_| panic!("one signal never exits"),
        |_| async {},
    ));
    let data = dir.path().display().to_string();
    let mut vars = vec![("LMGW_DEPLOYMENT", "container"), ("LMGW_DATA_DIR", &*data)];
    for v in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        vars.push((v, &proxy_url));
    }
    let out = binary(&vars, &["account", "list"]).output().await.unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.contains("owner"), "{stdout}");
    assert_eq!(taken.load(Ordering::SeqCst), 0, "a call went to the proxy");

    signals.send("SIGTERM").await.unwrap();
    serving.await.unwrap().unwrap();
    drop(ready.lock);
}
