//! Who approved the call running now (client-apps design §6.3, L12).
//!
//! A resumed run calls each approved gated call inside [`scope`] with the
//! principal that decided it, and nothing else does: the loop's own turns,
//! a sibling held beside a gated call, every call of `/mcp` run outside
//! it. Two readers take it from there, whatever executor the call went
//! through:
//!
//! - the call's `request_logs` row names the approver
//!   (`request_logs.approved_by`, `mcp::ingress::tool_row`);
//! - a call forwarded to a device-hosted server carries
//!   `_meta["lmgw/approval"] = {decision: "approved", by}`
//!   (`mcp::exec::McpExecutor`).
//!
//! So `lmgw/approval` is stamped exactly when a real approval decision
//! exists for that call, and names who made it. A task-local rather than a
//! parameter of every executor: the wrappers between the loop and the MCP
//! executor (the tool-scope check, a heard turn's gate, the plane split)
//! need not each remember to pass it on, and a call made outside the scope
//! cannot carry one by mistake. It is read at the top of the call, before
//! anything is spawned.

use std::future::Future;

/// The principal that decided a call, as a device reads it.
pub type Approver = lmgw_api_types::mcp_host::Principal;

tokio::task_local! {
    static APPROVED_BY: Option<Approver>;
}

/// Run `call` with `by` as its approver (`None`: a call nobody approved).
pub fn scope<F: Future>(by: Option<Approver>, call: F) -> impl Future<Output = F::Output> {
    APPROVED_BY.scope(by, call)
}

/// Who approved the call running now; `None` outside [`scope`] and for a
/// call nobody approved.
pub fn approved_by() -> Option<Approver> {
    APPROVED_BY.try_with(Clone::clone).ok().flatten()
}

/// How a row names an approver: `<kind>:<name>` (`owner:dashboard`,
/// `device:phone`).
pub fn row_label(by: &Approver) -> String {
    let kind = serde_json::to_value(&by.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    format!("{kind}:{}", by.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::mcp_host::CallerKind;

    fn device(name: &str) -> Approver {
        Approver {
            kind: CallerKind::Device,
            name: name.into(),
        }
    }

    #[tokio::test]
    async fn the_approver_is_there_inside_its_scope_only() {
        assert_eq!(approved_by(), None);
        let seen = scope(Some(device("phone")), async { approved_by() }).await;
        assert_eq!(seen, Some(device("phone")));
        let none = scope(None, async { approved_by() }).await;
        assert_eq!(none, None);
        // Two calls side by side keep their own.
        let (a, b) = futures::future::join(
            scope(Some(device("a")), async {
                tokio::task::yield_now().await;
                approved_by()
            }),
            scope(None, async {
                tokio::task::yield_now().await;
                approved_by()
            }),
        )
        .await;
        assert_eq!((a, b), (Some(device("a")), None));
        assert_eq!(row_label(&device("phone")), "device:phone");
    }
}
