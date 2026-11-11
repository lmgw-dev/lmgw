//! The command line (headless server design §1, §5.7): no arguments (or
//! `serve`) serves; `--version` and `--help` answer and never serve; the
//! account commands and `login-link` run once and exit ([`crate::account`]).

use std::ffi::OsString;

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Serve until a quit signal (§1).
    Serve,
    /// Print the version.
    Version,
    /// Print the usage.
    Help,
    /// One of §5.7's commands.
    Account(Account),
}

/// §5.7's commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    /// `account list`.
    List,
    /// `account set-password <login> [--login <new login>]`: `rename` is
    /// account 1's new sign-in name with its first password (§5.3, the
    /// only time it can change).
    SetPassword {
        login: String,
        rename: Option<String>,
    },
    /// `account link <login>`.
    Link(String),
    /// `login-link [<login>]`; `None` is account 1.
    LoginLink(Option<String>),
}

/// The usage text `--help` prints and a wrong command line is answered with.
pub const USAGE: &str = "\
usage: lmgw-server [serve]
       lmgw-server account list
       lmgw-server account set-password <login> [--login <new login>]
       lmgw-server account link <login>
       lmgw-server login-link [<login>]
       lmgw-server --version
       lmgw-server --help

Serves the lmgw gateway without the tray app, for the server image: the
image sets LMGW_DEPLOYMENT=container and LMGW_DATA_DIR=/data. Outside a
container it starts only as a dev instance (LMGW_DEV=1, LMGW_DATA_DIR set to
a scratch dir, and LMGW_BIND_ADDR its address when the stored one will not
do).

The account commands and login-link go through the running gateway
(podman exec -it lmgw lmgw-server …). With the service stopped, account
list, set-password and link run on the data volume itself (podman run --rm
-it -v lmgw-data:/data <image> lmgw-server account …) and apply at the
gateway's next start; login-link needs the running gateway.

set-password reads the new password twice from the terminal without echo,
or as two lines from a pipe. --login gives account 1 ('owner') another
sign-in name with its first password, the only time that name can change.";

/// The command line after the program's name.
pub fn parse<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    let words: Vec<&str> = match args.iter().map(|a| a.to_str()).collect::<Option<Vec<_>>>() {
        Some(words) => words,
        None => return Err(format!("an argument is not valid UTF-8\n\n{USAGE}")),
    };
    match words.as_slice() {
        [] | ["serve"] => Ok(Command::Serve),
        ["--version" | "-V" | "version"] => Ok(Command::Version),
        ["--help" | "-h" | "help"] => Ok(Command::Help),
        ["account", "list"] => Ok(Command::Account(Account::List)),
        ["account", "set-password", login] if !login.starts_with('-') => {
            Ok(Command::Account(Account::SetPassword {
                login: login.to_string(),
                rename: None,
            }))
        }
        ["account", "set-password", login, "--login", rename]
        | ["account", "set-password", "--login", rename, login]
            if !login.starts_with('-') =>
        {
            Ok(Command::Account(Account::SetPassword {
                login: login.to_string(),
                rename: Some(rename.to_string()),
            }))
        }
        ["account", "link", login] => Ok(Command::Account(Account::Link(login.to_string()))),
        ["login-link"] => Ok(Command::Account(Account::LoginLink(None))),
        ["login-link", login] => Ok(Command::Account(Account::LoginLink(Some(
            login.to_string(),
        )))),
        _ => Err(format!(
            "unknown command line: {}\n\n{USAGE}",
            words.join(" ")
        )),
    }
}

/// `lmgw-server <version>`, as `--version` prints it.
pub fn version_line() -> String {
    format!("lmgw-server {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &[&str]) -> Result<Command, String> {
        parse(w.iter().map(OsString::from))
    }

    #[test]
    fn no_arguments_serve() {
        assert_eq!(words(&[]), Ok(Command::Serve));
        assert_eq!(words(&["serve"]), Ok(Command::Serve));
        assert_eq!(words(&["--version"]), Ok(Command::Version));
        assert_eq!(words(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn the_account_commands_parse() {
        assert_eq!(
            words(&["account", "list"]),
            Ok(Command::Account(Account::List))
        );
        assert_eq!(
            words(&["account", "set-password", "owner"]),
            Ok(Command::Account(Account::SetPassword {
                login: "owner".into(),
                rename: None
            }))
        );
        for w in [
            &["account", "set-password", "owner", "--login", "boss"][..],
            &["account", "set-password", "--login", "boss", "owner"][..],
        ] {
            assert_eq!(
                words(w),
                Ok(Command::Account(Account::SetPassword {
                    login: "owner".into(),
                    rename: Some("boss".into())
                })),
                "{w:?}"
            );
        }
        assert!(words(&["account", "set-password", "--login"]).is_err());
        assert!(words(&["account", "set-password", "owner", "--login"]).is_err());
        assert_eq!(
            words(&["account", "link", "sam"]),
            Ok(Command::Account(Account::Link("sam".into())))
        );
        assert_eq!(
            words(&["login-link"]),
            Ok(Command::Account(Account::LoginLink(None)))
        );
        assert_eq!(
            words(&["login-link", "sam"]),
            Ok(Command::Account(Account::LoginLink(Some("sam".into()))))
        );
        assert!(words(&["account", "set-password"]).is_err());
    }

    #[test]
    fn anything_else_is_refused_with_the_usage() {
        let e = words(&["serve", "--port", "1"]).unwrap_err();
        assert!(e.contains("serve --port 1") && e.contains("usage:"), "{e}");
    }
}
