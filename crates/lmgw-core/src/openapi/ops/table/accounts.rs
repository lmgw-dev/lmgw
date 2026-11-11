//! `ops-accounts`: the account ops (headless server design §5.6). None has a
//! self-admin tool (D17): they hand out sign-in links and set passwords.
//! While accounts are off on the desktop every one but `accounts_switch`
//! answers 409 accounts_off.

use lmgw_api_types as dto;

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-accounts";

const fn op(
    name: &'static str,
    summary: &'static str,
    description: &'static str,
    args: OpArgs,
    response: Resp,
    (writes, reveals_secret): (bool, bool),
    confirm_note: Option<&'static str>,
) -> OpDoc {
    OpDoc {
        name,
        tag: TAG,
        summary,
        description: Some(description),
        tool: None,
        args,
        response,
        writes,
        reveals_secret,
        confirm_note,
        deprecated: false,
        example: None,
    }
}

pub(super) const OPS: &[OpDoc] = &[
    op(
        "account_list",
        "List the accounts",
        "Every account with its login, display name, enabled flag, whether it has a password, \
         when it was made and by whom, its last sign-in, its failed sign-ins since then, and \
         the number of its owner keys and sessions; beside them the fixed numbers of sign-in \
         (the shortest password, the body limit, the throttle's delays and entries, the links' \
         lifetimes, how many verifications run at once and the memory they can take).",
        OpArgs::NoArgs,
        Resp::Json(|g| g.root_schema_for::<dto::AccountList>()),
        (false, false),
        None,
    ),
    op(
        "account_create",
        "Create an account",
        "A new account with its own sign-in key (owner:account-<id>), answering a one-time \
         set-password link to hand over (valid 24 h). Every account administers the whole \
         gateway. 409 login_taken for a sign-in name in use.",
        OpArgs::Hand(args::account_create),
        Resp::Json(|g| g.root_schema_for::<dto::AccountLink>()),
        (true, true),
        None,
    ),
    op(
        "account_set",
        "Rename, enable or disable an account",
        "Change an account's display name or enabled flag. Disabling ends its sessions and \
         what they opened, and its owner keys authenticate nothing; the client and device keys \
         it made keep working. Account 1 cannot be disabled, nor can an account disable itself \
         (409).",
        OpArgs::Hand(args::account_set),
        Resp::Json(|g| g.root_schema_for::<dto::AccountDone>()),
        (true, false),
        Some("disabling signs the account out everywhere and pauses its owner keys"),
    ),
    op(
        "account_delete",
        "Delete an account",
        "Deletes the account with its owner keys, sessions and set-password link. Keys of other \
         kinds it made keep working and read \"made by a deleted account (<login>)\". Not \
         account 1, not your own (409).",
        OpArgs::Hand(args::account_id),
        Resp::Json(|g| g.root_schema_for::<dto::AccountDone>()),
        (true, false),
        Some("deletes the account, its owner keys and its sessions"),
    ),
    op(
        "account_password_link",
        "Mint a set-password link for an account",
        "A one-time link to <public origin>/set-password#token=…, valid 24 h, replacing the \
         account's earlier one. Refused for your own account (409 account_self): change your \
         own password with your current one.",
        OpArgs::Hand(args::account_id),
        Resp::Json(|g| g.root_schema_for::<dto::AccountLink>()),
        (true, true),
        None,
    ),
    op(
        "account_password_set",
        "Set a password",
        "Without id, or with your own: your password, with current when you have one; your \
         other sessions end. With another account's id: sets it without current and ends all \
         of that account's sessions. At least 12 characters (Unicode code points after NFKC), \
         no composition rules, no blocklist. login sets account 1's sign-in name with its \
         first password only. 403 login_insecure over http from another machine; 401 \
         password_wrong for a wrong current password, and after repeated wrong ones 429 \
         login_throttled with Retry-After; 503 login_busy with Retry-After: 1 while every \
         verification permit is taken; 403 no_account without id for a credential that \
         belongs to no account.",
        OpArgs::Struct(|g| g.root_schema_for::<crate::accounts::ops::PasswordSet>()),
        Resp::Json(|g| g.root_schema_for::<dto::AccountDone>()),
        (true, false),
        None,
    ),
    op(
        "account_sessions",
        "List an account's sessions",
        "Each session's start, last sight (written at most once an hour), expiry, the address \
         and user agent it signed in from, and whether it is the browser asking.",
        OpArgs::Hand(args::account_id),
        Resp::Json(|g| g.root_schema_for::<dto::SessionList>()),
        (false, false),
        None,
    ),
    op(
        "session_end",
        "End a session, or every session of an account",
        "id ends one session; account ends every session of that account (Sign out \
         everywhere). What a session opened (event streams, sockets) closes with it; ending \
         your own current session signs this browser out.",
        OpArgs::Hand(args::session_end),
        Resp::Json(|g| g.root_schema_for::<dto::AccountDone>()),
        (true, false),
        None,
    ),
    op(
        "account_login_link",
        "Mint a one-time sign-in link for an account",
        "A link to <public origin>/api/session/login?nonce=…, valid 60 s and once, that signs a \
         browser into the account's session: what lmgw-server login-link prints. Refused for \
         a signed-in browser's session (403 login_link_cli_only): it is the command line's, \
         which calls with an owner key.",
        OpArgs::Hand(args::account_id),
        Resp::Json(|g| g.root_schema_for::<dto::AccountLink>()),
        (true, true),
        None,
    ),
    op(
        "accounts_switch",
        "Turn accounts on or off on this desktop",
        "On: everyone who signs in needs an account; the answer carries a session cookie for \
         your account, other browsers sign in again. Off: every session ends, the owner keys of \
         accounts other than account 1 are paused until accounts are on again, and the \
         accounts, their passwords and known browsers are kept; account 1's browser gets the \
         dashboard key's cookie, any other is signed out. Refused in the server image (409 \
         accounts_always_on), where accounts are always on; 403 no_account for a credential \
         that belongs to no account; 409 sign_in_superseded when the caller's account changed \
         while its new session started.",
        OpArgs::Hand(args::accounts_switch),
        Resp::Json(|g| g.root_schema_for::<dto::AccountDone>()),
        (true, false),
        Some("changes who may sign in to this dashboard and ends sessions"),
    ),
    op(
        "audit_list",
        "Read the audit log",
        "One page of the audit log, newest first (headless server design §5.8), everyone's or \
         one account's (account: an id, \"none\" for the entries of no account): each entry's \
         time, account, who (login with session or key), how (dashboard, api, mcp, chat, \
         agent, cli, cli-offline, tray, app), client address, action and the action in words, \
         the row it names with a settings save's fields (a secret's value reads \"changed\", \
         a prompt or a command line its length), the answer's status, and how many times it \
         happened (a failed or throttled sign-in repeated by one client within a minute is one \
         entry); with the whole log's entries, size on disk, oldest entry, the retention \
         (accounts.audit_days) and the audit writes that failed since the start. Readable \
         while accounts are off on the desktop, whose log is kept.",
        OpArgs::Hand(args::audit_list),
        Resp::Json(|g| g.root_schema_for::<dto::AuditPage>()),
        (false, false),
        None,
    ),
];
