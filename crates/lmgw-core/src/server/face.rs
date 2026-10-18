//! Which routes a listener serves (device link design §4.6): its face.
//!
//! - **The device listener** serves a route when its `CAPABILITY_TABLE`
//!   row is `Inference` or `Chat` for the request's method, or when it is
//!   one of [`DEVICE_ROUTES`]. Everything else is a 404, as an unknown path
//!   is: no SPA, no `/api`, no labs, no agent proxies.
//! - **The plain listener** serves every route but [`DEVICE_ROUTES`], which
//!   are a 404 there.
//!
//! The rule is derived from the capability the route's own gate declares
//! (`gate`), so a new `/v1` or Chat route reaches devices without a second
//! table; the route walk checks both faces (`tests/it/device_face.rs`).

use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;

use super::Via;
use crate::principal::Cap;

/// The device listener's own routes, `(method, path)`: served on its face
/// whatever their capability, and a 404 on the plain listener's. Enrolment
/// (§3.4); `POST /devices/renew` joins with WP8. None of them can upgrade on
/// a connection that has not authenticated: the serving loop strips the
/// upgrade, which would outlive the connection's place and its lapse.
pub const DEVICE_ROUTES: &[(&str, &str)] = &[("POST", lmgw_api_types::device_link::ENROL_PATH)];

/// Whether `(method, path)` is one of [`DEVICE_ROUTES`].
pub fn is_device_route(method: &Method, path: &str) -> bool {
    DEVICE_ROUTES
        .iter()
        .any(|(m, p)| *p == path && method.as_str() == *m)
}

/// Whether a request on `via` for `(method, path)`, whose route declared
/// `cap`, is on that listener's face.
pub fn serves_on(via: Via, method: &Method, path: &str, cap: Cap) -> bool {
    let own = is_device_route(method, path);
    match via {
        Via::Plain => !own,
        Via::Devices => own || matches!(cap, Cap::Inference | Cap::Chat),
    }
}

/// The answer to a route off the face: the 404 an unknown path gets.
pub(super) fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// The device listener's router's own answer to a method its route does not
/// take: the router's 405 would tell a device that a route off its face
/// exists, so on the device listener it is the 404 an unknown path gets. Only
/// the router's own 405 (`method_not_allowed_fallback`): a 405 a handler
/// sends itself stays one.
pub(super) fn device_face<S>(routes: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    routes.method_not_allowed_fallback(off_face)
}

async fn off_face() -> Response {
    not_found()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the router's own 405 becomes the face's 404; a handler that
    /// answers 405 itself is heard as it answers.
    #[tokio::test]
    async fn only_the_router_s_own_405_is_the_face_s_404() {
        use axum::routing::{get, post};
        use tower::ServiceExt;
        async fn says_405() -> StatusCode {
            StatusCode::METHOD_NOT_ALLOWED
        }
        let app = device_face(
            Router::new()
                .route("/only-get", get(|| async { "ok" }))
                .route("/handler-405", post(says_405)),
        );
        let ask = |method: Method, path: &'static str| {
            let app = app.clone();
            async move {
                let req = axum::http::Request::builder()
                    .method(method)
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap();
                app.oneshot(req).await.unwrap().status()
            }
        };
        assert_eq!(ask(Method::POST, "/only-get").await, StatusCode::NOT_FOUND);
        assert_eq!(ask(Method::GET, "/only-get").await, StatusCode::OK);
        assert_eq!(
            ask(Method::POST, "/handler-405").await,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[test]
    fn the_device_face_serves_inference_chat_and_its_own_routes() {
        let post = Method::POST;
        let get = Method::GET;
        let enrol = lmgw_api_types::device_link::ENROL_PATH;
        assert!(serves_on(
            Via::Devices,
            &post,
            "/v1/chat/completions",
            Cap::Inference
        ));
        assert!(serves_on(Via::Devices, &get, "/chat/api/feed", Cap::Chat));
        assert!(serves_on(Via::Devices, &post, enrol, Cap::Public));
        for cap in [Cap::Public, Cap::Admin, Cap::Ledger, Cap::AgentSelf] {
            assert!(
                !serves_on(Via::Devices, &get, "/api/status", cap),
                "{cap:?}"
            );
        }
        // Another method on the enrolment path is not the device route.
        assert!(!serves_on(Via::Devices, &get, enrol, Cap::Public));
        // The plain listener: everything but the device routes.
        assert!(!serves_on(Via::Plain, &post, enrol, Cap::Public));
        assert!(serves_on(Via::Plain, &get, "/api/status", Cap::Admin));
        assert!(serves_on(Via::Plain, &get, "/", Cap::Public));
    }
}
