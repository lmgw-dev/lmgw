use std::sync::Mutex as StdMutex;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::erased::Erased;
use crate::runtime::registry::{CmdOutput, CommandRunner, Registry};

#[test]
fn the_table() {
    use CancelKind::*;
    use JobStatus::*;
    // 0518b34: cancel_queued, no cancel_generating.
    let b = (true, false);
    assert_eq!(kind(Queued, b, true), Native);
    assert_eq!(kind(Submitted, b, false), Native);
    assert_eq!(kind(Generating, b, true), Restart);
    assert_eq!(kind(Generating, b, false), Abandon);
    assert_eq!(kind(Generating, (true, true), false), Native);
    assert_eq!(kind(Queued, (false, false), true), Restart);
}

#[test]
fn a_let_go_error_says_what_became_of_the_job() {
    let timeout = LetGo::Timeout(Duration::from_secs(60));
    let e = let_go_error("image/z", timeout, Some(CancelKind::Abandon), Work::Job).to_string();
    assert!(
        e.contains("image/z did not finish the picture within the route's timeout (60 s"),
        "{e}"
    );
    assert!(
        e.contains("the job runs to its end holding its claim, and its picture is thrown away"),
        "{e}"
    );
    let e = let_go_error("image/z", timeout, None, Work::Job).to_string();
    assert!(e.ends_with("image.request_timeout_seconds)"), "{e}");
    let stall = LetGo::Stalled(Stall {
        quiet: Duration::from_secs(20),
        slowest: Duration::from_secs(1),
    });
    let e = let_go_error("image/z", stall, Some(CancelKind::Restart), Work::Job).to_string();
    assert!(
        e.contains("image/z's job made no progress for 20.0 s"),
        "{e}"
    );
    assert!(e.contains("lmgw stopped image/z's container"), "{e}");
}

/// A `podman` that agrees to everything and records what it was asked.
#[derive(Default)]
struct Podman(StdMutex<Vec<Vec<String>>>);

impl Podman {
    fn stops(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.first().is_some_and(|c| c == "stop"))
            .count()
    }
}

impl CommandRunner for Podman {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        self.0.lock().unwrap().push(args.to_vec());
        Box::pin(async {
            Ok(CmdOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
    }
}

/// A gateway with one image row `z`, its container the wiremock `sd`.
async fn image_row(sd: &MockServer) -> (SharedState, Arc<Podman>) {
    Mock::given(method("GET"))
        .and(path(crate::runtime::descriptor::IMAGE_HEALTH_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(include_str!(
            "../../../../tests/fixtures/sdcpp/capabilities-z-image-turbo-0518b34.json"
        )))
        .mount(sd)
        .await;
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let port = sd.address().port();
    let podman = Arc::new(Podman::default());
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));
    let mut s = crate::config::Settings::default();
    s.vram.load_timeout_seconds = 5;
    let dir = state.data_dir.join("image-models");
    std::fs::create_dir_all(&dir).unwrap();
    s.image.models_dir = dir.display().to_string();
    s.container_prefix = "lmgwtest".into();
    crate::store::save_settings(&state.db, &s).await.unwrap();
    let mut files = serde_json::Map::new();
    files.insert("diffusion_model".into(), "z.gguf".into());
    let row = crate::store::NewImageModel {
        model_id: "z".into(),
        files,
        enabled: true,
        ..Default::default()
    };
    crate::store::insert_image_model(&state.db, &row)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    (state, podman)
}

/// The row's container: its generation and requests in flight.
fn container(state: &SharedState) -> (u64, u32) {
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == Class::Image && v.model_id == "z")
        .map(|v| (v.generation, v.in_flight))
        .expect("the row's container")
}

/// The restart race (module doc): the guard chose to restart the row while
/// its call was the only request, and another request joined before the
/// stop. The stop is refused for it, its generation keeps running, and only
/// the cancelled call's claim goes.
#[tokio::test]
async fn a_request_that_joins_before_the_restart_keeps_the_row() {
    let sd = MockServer::start().await;
    let (state, podman) = image_row(&sd).await;
    let route = state.snapshot().resolve("image/z").unwrap();
    let mine = crate::vram::admit(&state, &route, "image/z")
        .await
        .unwrap()
        .expect("a claim");
    let generation = mine.generation();
    assert!(only_request(&state, "z", generation), "the decision's view");
    // Joins between the decision and the stop.
    let other = crate::vram::admit(&state, &route, "image/z")
        .await
        .unwrap()
        .expect("a second claim");
    assert_eq!(container(&state), (generation, 2));
    let c = follow::Cancelling {
        state: state.clone(),
        hold: Arc::new(mine),
        route: route.clone(),
        model_id: "z".into(),
        job: Job {
            root: format!("http://{}", sd.address()),
            id: "job_1".into(),
            status: JobStatus::Generating,
            pace: Pace::new(Instant::now()),
            task: None,
        },
        why: None,
    };
    c.run(CancelKind::Restart).await;
    assert_eq!(
        podman.stops(),
        0,
        "the other request's generation is not hit"
    );
    assert_eq!(
        container(&state),
        (generation, 1),
        "the cancelled call's claim went, the other's stays"
    );
    drop(other);
}
