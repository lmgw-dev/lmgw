//! Fixture

use super::*;

pub(super) struct Fixture {
    pub(super) state: SharedState,
    pub(super) gateway: Gw,
    pub(super) world: Arc<Mutex<World>>,
    pub(super) podman: Arc<Podman>,
    /// The container mocks, in the order the port allocator hands them out:
    /// the first model to start gets `first`. All of them answer everything, so
    /// a test only needs this when it scripts extra turns onto a specific
    /// container.
    pub(super) first: MockServer,
    pub(super) _second: MockServer,
    /// The third is for the tests that put an image pipeline on the card
    /// beside the chat and embedding models.
    pub(super) _third: MockServer,
    /// More, handed out after the first three — the ladder tests', where
    /// every climb is a start on a port of its own.
    more: Vec<MockServer>,
    pub(super) _models_dir: tempfile::TempDir,
}

impl Fixture {
    pub(super) fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap()
    }

    /// Kill the first container: its wiremock is shut down, so its published
    /// port stops accepting connections while lmgw's registry still holds a
    /// `ready` entry (and a live claim) pointing at it.
    ///
    /// That is exactly what an OOM-killed llama-server, a `podman stop` from a
    /// shell or a crashed container looks like from here — the one state the
    /// registry lock cannot rule out, because it is a fact about the world and
    /// not about the map. The replacement is started *before* the old server is
    /// dropped, so the freed port cannot be handed straight back.
    pub(super) async fn kill_first_container(&mut self) {
        let dead_port = self.first.address().port();
        let fresh = container(self.world.clone()).await;
        let old = std::mem::replace(&mut self.first, fresh);
        drop(old);
        for _ in 0..500 {
            if reqwest::Client::new()
                .get(format!("http://127.0.0.1:{dead_port}/health"))
                .timeout(Duration::from_millis(50))
                .send()
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the killed container is still answering on {dead_port}");
    }

    pub(super) fn runs(&self) -> Vec<String> {
        self.world().runs.clone()
    }

    pub(super) fn stops(&self) -> Vec<String> {
        self.world().stops.clone()
    }

    /// Switch the fake driver's per-process list on, with `outside` bytes
    /// held by a process that is not lmgw's (§4.7).
    pub(super) fn attribute(&self, outside: u64) {
        let mut w = self.world();
        w.attribution = true;
        w.outside = outside;
    }

    pub(super) fn pid_inspects(&self) -> Vec<Vec<String>> {
        self.world().pid_inspects.clone()
    }

    /// Kill the container on `port`, whichever mock that is — see
    /// [`Self::kill_first_container`].
    pub(super) async fn kill_container_on(&mut self, port: u16) {
        let fresh = container(self.world.clone()).await;
        let slot = [&mut self.first, &mut self._second, &mut self._third]
            .into_iter()
            .chain(self.more.iter_mut())
            .find(|m| m.address().port() == port)
            .expect("a container this fixture made");
        drop(std::mem::replace(slot, fresh));
        for _ in 0..500 {
            if reqwest::Client::new()
                .get(format!("http://127.0.0.1:{port}/health"))
                .timeout(Duration::from_millis(50))
                .send()
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the killed container is still answering on {port}");
    }
}

/// A gateway with one chat model and one embedding model, each backed by a file
/// of the stated size in a real models dir.
///
/// The files are not GGUFs, deliberately: the header parse fails, the estimate
/// falls back to weights-only, and the footprint is therefore *exactly* the
/// number the test set — the arithmetic under assertion is the scheduler's, not
/// llama.cpp's KV formula (which `gguf.rs` tests on its own).
pub(super) async fn fixture(
    total_vram: u64,
    chat_bytes: u64,
    embed_bytes: u64,
    headroom_mb: u64,
) -> Fixture {
    fixture_n(total_vram, chat_bytes, embed_bytes, headroom_mb, 0).await
}

/// [`fixture`] with `extra` more containers after the first three.
pub(super) async fn fixture_n(
    total_vram: u64,
    chat_bytes: u64,
    embed_bytes: u64,
    headroom_mb: u64,
    extra: usize,
) -> Fixture {
    let state = AppState::init_for_tests().await.unwrap();
    let world = Arc::new(Mutex::new(World::default()));
    {
        let mut w = world.lock().unwrap();
        w.size.insert("chat-model".into(), chat_bytes);
        w.size.insert("embed-model".into(), embed_bytes);
        // Weak: the state holds the driver that holds this world.
        let st = Arc::downgrade(&state);
        w.samples = Some(Arc::new(move || {
            st.upgrade().map_or(0, |s| s.vram.residency_samples())
        }));
    }

    let dir = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("chat-model.gguf", chat_bytes),
        ("embed-model.gguf", embed_bytes),
    ] {
        // Sparse: the size is metadata, no disk is spent on it.
        let f = std::fs::File::create(dir.path().join(name)).unwrap();
        f.set_len(bytes).unwrap();
    }

    let first = container(world.clone()).await;
    let second = container(world.clone()).await;
    let third = container(world.clone()).await;
    let mut more = Vec::with_capacity(extra);
    for _ in 0..extra {
        more.push(container(world.clone()).await);
    }
    let podman = Arc::new(Podman::new(world.clone()));
    let ports = Arc::new(Mutex::new(
        [&first, &second, &third]
            .into_iter()
            .chain(&more)
            .map(|c| c.address().port())
            .collect::<VecDeque<u16>>(),
    ));
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || {
            ports.lock().unwrap().pop_front().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "test allocator ran out of ports — an unexpected extra start",
                )
            })
        }),
    )));

    let mut s = Settings::default();
    s.router.models_dir = dir.path().display().to_string();
    s.aux_router.models_dir = dir.path().display().to_string();
    // The fourth class shares the directory: `add_image_model` writes its
    // pipeline file there, and nothing else in these tests reads it.
    s.image.models_dir = dir.path().display().to_string();
    // And the audio class (realtime design §9.4): `audio_residency` puts its
    // model directories there.
    s.audio.models_dir = dir.path().display().to_string();
    s.vram.headroom_mb = headroom_mb;
    s.vram.queue_timeout_seconds = 2;
    store::save_settings(&state.db, &s).await.unwrap();

    // No managed `upstreams` row: since §5 both classes resolve from their own
    // tables onto their class's synthetic upstream.
    store::insert_local_model(
        &state.db,
        &NewLocalModel {
            model_id: "chat-model".into(),
            gguf_path: "chat-model.gguf".into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    store::insert_aux_model(
        &state.db,
        &NewAuxModel {
            model_id: "embed-model".into(),
            gguf_path: "embed-model.gguf".into(),
            kind: AuxKind::Embed,
            pooling: None,
            ctx_size: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    state.vram.set_probe(Arc::new(FakeGpu {
        total: total_vram,
        world: world.clone(),
    }));

    let gateway = serve(state.clone()).await;

    Fixture {
        state,
        gateway,
        world,
        podman,
        first,
        _second: second,
        _third: third,
        more,
        _models_dir: dir,
    }
}

pub(super) async fn vram_status(base: &Gw) -> Value {
    base.client()
        .get(format!("{base}/api/vram"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

pub(super) async fn embed(base: &Gw) -> reqwest::Response {
    base.client()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({"model": "embed/embed-model", "input": "hello"}))
        .send()
        .await
        .unwrap()
}

pub(super) async fn chat(base: &Gw) -> reqwest::Response {
    base.client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "chat-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}
