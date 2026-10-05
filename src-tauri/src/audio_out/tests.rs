//! Sink and stream detection on synthetic `pw-dump` documents (chat-voice
//! WP5). The fixtures mirror what PipeWire 1.6 prints — pipewire-pulse
//! clients whose `pipewire.sec.pid` is the pulse server, native clients with
//! their own, filter nodes, an input stream — with made-up names.

use super::graph::{self, descends_from, ppid_from_stat, stream_pid};
use super::{identity_env, metadata_args, outputs_from, partial_failure, AudioOutput, STREAM_APP};

const DESKTOP: &str = include_str!("fixtures/pw-dump-desktop.json");
const TEXT_VALUES: &str = include_str!("fixtures/pw-dump-text-values.json");

/// The shell is 4000. Its WebKit web process (4242) and GPU process (4243)
/// are its children; everything else lives elsewhere in the session.
fn tree(pid: u32) -> Option<u32> {
    match pid {
        4242 | 4243 => Some(4000),
        4000 => Some(3000),
        5555 | 6666 | 7777 => Some(3000),
        1900 | 1800 => Some(1500),
        3000 | 1500 => Some(1),
        _ => None,
    }
}

#[test]
fn every_audio_sink_is_listed_with_its_serial_and_the_default_marked() {
    let g = graph::parse(DESKTOP).unwrap();
    let outputs = outputs_from(&g);
    let row = |name: &str, description: &str, serial: u64, default: bool| AudioOutput {
        name: name.into(),
        description: description.into(),
        serial,
        default,
    };
    assert_eq!(
        outputs,
        vec![
            row(
                "alsa_output.pci-0000_00_1f.3.analog-stereo",
                "Built-in Audio Analog Stereo",
                50,
                true
            ),
            row(
                "alsa_output.usb-Generic_USB_Headset-00.analog-stereo",
                "USB Headset Analog Stereo",
                1234,
                false
            ),
            // A filter's virtual sink is an output like any other.
            row("echo_cancel_sink", "Echo-Cancel Sink", 52, false),
            // An empty description falls back to the nick, a missing one
            // (and nick) to the name.
            row(
                "alsa_output.pci-0000_01_00.1.hdmi-stereo",
                "Monitor HDMI",
                2048,
                false
            ),
            row("bare_sink", "bare_sink", 54, false),
        ],
        "sources, streams and the configured (not current) default are not outputs"
    );
}

#[test]
fn lmgws_streams_are_the_playback_streams_of_its_process_tree() {
    let g = graph::parse(DESKTOP).unwrap();
    assert_eq!(
        g.own_playback(4000, &tree, Some("lmgw")),
        vec![60, 61, 65],
        "both web-process contexts (pulse, by the reported pid) and the GPU \
         process (native, by the kernel's pid); not the microphone (62), the \
         browser (63), the player (64), the impostor claiming 4242 (66) or the \
         echo canceller's own playback (67)"
    );
    // Another shell sees none of them.
    assert!(g.own_playback(9999, &tree, Some("lmgw")).is_empty());
}

/// Review m2: a browser `xdg-open` started as the shell's child descends
/// from it too. Its streams are not lmgw's: it is not a WebKit process and
/// does not carry lmgw's stream id (xdg-open's environment is scrubbed).
#[test]
fn a_descendant_that_is_not_webkit_is_not_lmgws() {
    let browser_is_a_child = |pid: u32| match pid {
        5555 => Some(4000),
        other => tree(other),
    };
    let g = graph::parse(DESKTOP).unwrap();
    assert_eq!(
        g.own_playback(4000, &browser_is_a_child, Some("lmgw")),
        vec![60, 61, 65],
        "the browser's stream (63) stays out"
    );
    // Without the identity in effect, WebKit's binary alone still counts.
    assert_eq!(
        g.own_playback(4000, &browser_is_a_child, None),
        vec![60, 61, 65]
    );
}

#[test]
fn older_dumps_with_text_values_read_the_same() {
    let g = graph::parse(TEXT_VALUES).unwrap();
    assert_eq!(
        g.default_sink(),
        Some("alsa_output.usb-Generic_USB_Headset-00.analog-stereo"),
        "a metadata value printed as JSON text"
    );
    let outputs = outputs_from(&g);
    assert_eq!(outputs[0].serial, 50, "a serial printed as text");
    assert_eq!(outputs[1].serial, 51, "no serial: the node id stands in");
    assert!(outputs[1].default);
    assert_eq!(g.own_playback(4000, &tree, Some("lmgw")), vec![60]);
}

#[test]
fn a_dump_that_is_not_a_list_of_objects_is_an_error() {
    assert!(graph::parse("").is_err());
    assert!(graph::parse("{\"id\": 1}").is_err());
    let g = graph::parse("[]").unwrap();
    assert!(g.sinks().is_empty());
    assert_eq!(g.default_sink(), None);
}

fn props(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn the_stream_pid_prefers_the_kernels_word_where_it_names_the_app() {
    let pulse_client = props(serde_json::json!({
        "client.api": "pipewire-pulse", "pipewire.sec.pid": 1900, "application.process.id": 4242
    }));
    let native_client = props(serde_json::json!({
        "pipewire.sec.pid": 4243, "application.process.id": 2
    }));
    // On a native node itself, it wins.
    let on_node = props(serde_json::json!({"pipewire.sec.pid": 4243, "application.process.id": 1}));
    assert_eq!(stream_pid(&on_node, None), Some(4243));
    assert_eq!(stream_pid(&on_node, Some(&native_client)), Some(4243));
    // Review n1: a pulse stream's sec.pid is the pulse server's, on the node
    // as on the client (a release that copies it onto the node must not turn
    // every lmgw stream into the server's).
    assert_eq!(
        stream_pid(&on_node, Some(&pulse_client)),
        Some(1),
        "a pulse client: the node's reported pid"
    );
    let pulse_node_with_sec_pid = props(serde_json::json!({
        "client.api": "pipewire-pulse", "pipewire.sec.pid": 1900, "application.process.id": 4242
    }));
    assert_eq!(stream_pid(&pulse_node_with_sec_pid, None), Some(4242));
    // A pulse client's sec.pid is the pulse server: the reported pid is used.
    assert_eq!(
        stream_pid(&props(serde_json::json!({})), Some(&pulse_client)),
        Some(4242)
    );
    let pulse_node =
        props(serde_json::json!({"client.api": "pipewire-pulse", "application.process.id": 4242}));
    assert_eq!(stream_pid(&pulse_node, None), Some(4242));
    // A native client's sec.pid beats what it reports.
    assert_eq!(
        stream_pid(&props(serde_json::json!({})), Some(&native_client)),
        Some(4243)
    );
    // Nothing to go on, or a pid of 0.
    assert_eq!(stream_pid(&props(serde_json::json!({})), None), None);
    assert_eq!(
        stream_pid(
            &props(serde_json::json!({"application.process.id": 0})),
            None
        ),
        None
    );
}

#[test]
fn descent_walks_up_and_stops_at_init_a_gap_or_a_loop() {
    assert!(descends_from(4000, 4000, &tree), "the shell itself");
    assert!(descends_from(4242, 4000, &tree));
    assert!(!descends_from(5555, 4000, &tree));
    assert!(!descends_from(424242, 4000, &tree), "a pid that is gone");
    let looped = |p: u32| Some(if p == 10 { 11 } else { 10 });
    assert!(!descends_from(10, 4000, &looped));
}

#[test]
fn the_parent_pid_is_read_after_the_last_parenthesis() {
    assert_eq!(
        ppid_from_stat("4242 (WebKitWebProces) S 4000 4242 4000 0 -1"),
        Some(4000)
    );
    assert_eq!(ppid_from_stat("77 (a) b (c)) R 12 77 77 0"), Some(12));
    assert_eq!(ppid_from_stat("77 (x y) S 1 77"), Some(1));
    assert_eq!(ppid_from_stat("garbage"), None);
    assert_eq!(
        graph::proc_parent(std::process::id()).is_some(),
        cfg!(target_os = "linux")
    );
}

#[test]
fn the_identity_is_set_unless_the_user_already_chose_one() {
    let keys = |present: &[&str]| -> Vec<&'static str> {
        let present: Vec<String> = present.iter().map(|s| s.to_string()).collect();
        identity_env(&present).into_iter().map(|(k, _)| k).collect()
    };
    assert_eq!(
        keys(&["HOME", "PATH"]),
        vec![
            "PULSE_PROP_OVERRIDE_application.name",
            "PULSE_PROP_OVERRIDE_application.id",
            "PIPEWIRE_PROPS"
        ]
    );
    assert_eq!(
        keys(&["PULSE_PROP_application.name"]),
        vec!["PIPEWIRE_PROPS"]
    );
    assert_eq!(keys(&["PULSE_PROP"]), vec!["PIPEWIRE_PROPS"]);
    assert_eq!(
        keys(&["PULSE_PROP_OVERRIDE_application.id"]),
        vec!["PIPEWIRE_PROPS"]
    );
    // A session-wide property that is not the identity leaves it to lmgw
    // (review n2): GStreamer's application.name would key the stream.
    for other in [
        "PULSE_PROP_media.role",
        "PULSE_PROP_OVERRIDE_media.role",
        "PULSE_PROPS",
    ] {
        assert_eq!(
            keys(&[other]),
            vec![
                "PULSE_PROP_OVERRIDE_application.name",
                "PULSE_PROP_OVERRIDE_application.id",
                "PIPEWIRE_PROPS"
            ],
            "{other}"
        );
    }
    assert_eq!(
        keys(&["PIPEWIRE_PROPS"]),
        vec![
            "PULSE_PROP_OVERRIDE_application.name",
            "PULSE_PROP_OVERRIDE_application.id"
        ]
    );
    let all: Vec<String> = vec![];
    let vars = identity_env(&all);
    assert!(vars.iter().all(|(_, v)| v.contains(STREAM_APP)));
    assert_eq!(
        vars.iter().find(|(k, _)| *k == "PIPEWIRE_PROPS").unwrap().1,
        format!("{{ application.name = \"{STREAM_APP}\" application.id = \"{STREAM_APP}\" }}")
    );
}

/// Review m3: a debug build is keyed apart from the installed app in
/// WirePlumber's memory.
#[test]
fn a_debug_build_streams_as_lmgw_dev() {
    assert_eq!(
        STREAM_APP,
        if cfg!(debug_assertions) {
            "lmgw-dev"
        } else {
            "lmgw"
        }
    );
}

#[test]
fn the_metadata_write_is_wireplumbers_own_form() {
    assert_eq!(
        metadata_args(61, Some(1234)),
        [
            "-n",
            "default",
            "--",
            "61",
            "target.object",
            "1234",
            "Spa:Id"
        ]
    );
    // The default: WirePlumber's "no defined target", which overrides a
    // target in the stream's own properties and clears the remembered one.
    assert_eq!(
        metadata_args(61, None),
        ["-n", "default", "--", "61", "target.object", "-1", "Spa:Id"]
    );
}

/// Review m8: a write that fails after others went through says which moved.
#[test]
fn a_routing_that_fails_halfway_says_what_moved() {
    assert_eq!(
        partial_failure(&[], 60, "pw-metadata exit status: 1: x"),
        "output routing failed on stream 60: pw-metadata exit status: 1: x"
    );
    assert_eq!(
        partial_failure(&[60, 61], 65, "boom"),
        "output routing moved streams [60, 61], then failed on stream 65: boom"
    );
}
