//! The server image's shipped files (headless server design §3, D19, D20, D21, rulings 3 and 11),
//! read from `packaging/server/` as they ship: the Quadlet unit against the quit's bounds and
//! the report's flags, the Containerfile and toolkit repository against the unit and the
//! desktop's pinned NVIDIA key and repository text, and `ci/build-image.sh`'s build context
//! against the Containerfile's `COPY`s.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lmgw_core::config::deployment::{
    quit_constant_seconds, stop_budget, IMAGE_BIND_ADDR, STOP_TIMEOUT_VAR,
};
use lmgw_core::config::Settings;
use lmgw_machine::plan::embedded::{
    KEY_FILE, KEY_URL, NVIDIA_KEY_FINGERPRINT, NVIDIA_STABLE_PREFIX, REPO_FILE, REPO_ID,
    REPO_INCLUDEPKGS, REPO_TEXT,
};

fn packaging_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/server")
}

fn read_path(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn read(name: &str) -> String {
    read_path(&packaging_dir().join(name))
}

/// The Containerfile's instructions of one kind (`COPY`, `EXPOSE`, …), each with its arguments.
fn instructions(containerfile: &str, kind: &str) -> Vec<Vec<String>> {
    let prefix = format!("{kind} ");
    containerfile
        .lines()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .map(|args| args.split_whitespace().map(str::to_string).collect())
        .collect()
}

/// A systemd-style file's `key=value` lines with their section, in order. A `#` or `;` starts a
/// comment only at the start of a line, as systemd reads it, so a note after a value would be
/// part of the value; [`unit`] refuses that.
fn entries(text: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_string();
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("not a key=value line: {line}"));
        out.push((section.clone(), k.trim().to_string(), v.trim().to_string()));
    }
    out
}

struct Unit(Vec<(String, String, String)>);

impl Unit {
    fn all(&self, section: &str, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(s, k, _)| s == section && k == key)
            .map(|(_, _, v)| v.as_str())
            .collect()
    }

    fn one(&self, section: &str, key: &str) -> &str {
        match self.all(section, key).as_slice() {
            [v] => v,
            other => panic!("[{section}] {key}= appears {} times", other.len()),
        }
    }

    fn seconds(&self, section: &str, key: &str) -> u64 {
        let v = self.one(section, key);
        v.parse()
            .unwrap_or_else(|_| panic!("[{section}] {key}={v} is not whole seconds"))
    }

    /// A Podman duration written in seconds (`900s`), as the health keys take it.
    fn duration_seconds(&self, section: &str, key: &str) -> u64 {
        let v = self.one(section, key);
        v.strip_suffix('s')
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("[{section}] {key}={v} is not <whole seconds>s"))
    }
}

fn unit() -> Unit {
    let entries = entries(&read("lmgw.container"));
    for (s, k, v) in &entries {
        assert!(
            !v.contains(" #") && !v.contains("\t#"),
            "[{s}] {k}={v}: systemd reads a note after a value as part of the value"
        );
    }
    Unit(entries)
}

/// The port of the image's plain listener (the bind address a fresh data dir gets).
fn http_port() -> u16 {
    IMAGE_BIND_ADDR
        .parse::<std::net::SocketAddr>()
        .unwrap()
        .port()
}

#[test]
fn the_stop_timeout_covers_a_whole_quit_with_the_default_unload_timeout() {
    let u = unit();
    let unload = Settings::default().vram.unload_timeout_seconds;
    let needed = quit_constant_seconds() + unload;
    let stop = u.seconds("Container", "StopTimeout");
    assert!(
        stop >= needed,
        "StopTimeout={stop} is below a quit's bounds with the default unload timeout ({needed} s)"
    );
    let mirrored = format!("{STOP_TIMEOUT_VAR}={stop}");
    assert!(
        u.all("Container", "Environment")
            .contains(&mirrored.as_str()),
        "the unit mirrors StopTimeout into {mirrored}"
    );
    let systemd = u.seconds("Service", "TimeoutStopSec");
    assert!(
        systemd > stop,
        "TimeoutStopSec={systemd} must be above StopTimeout={stop}: systemd would kill podman's \
         removal"
    );
    // The start's comparison (D20) with the unit's number: the default fits, one second more of
    // unload timeout is said.
    assert!(!stop_budget(Some(stop), unload).over);
    assert!(stop_budget(Some(stop), unload + (stop - needed) + 1).over);
}

#[test]
fn the_unit_stops_with_sigterm() {
    // --systemd=always would switch it to SIGRTMIN+3, which lmgw does not handle.
    assert_eq!(unit().one("Container", "StopSignal"), "SIGTERM");
}

#[test]
fn the_image_is_a_local_tag_that_is_never_pulled() {
    let u = unit();
    assert!(u
        .one("Container", "Image")
        .starts_with("localhost/lmgw-server:"));
    assert_eq!(u.one("Container", "Pull"), "never");
    assert!(
        u.all("Container", "AutoUpdate").is_empty(),
        "no registry to follow yet (ruling 11)"
    );
}

#[test]
fn the_unit_carries_the_reports_flags_for_the_confined_variant() {
    let u = unit();
    // Exactly the report's set: one more device or capability is a reviewed change here.
    let devices: BTreeSet<&str> = u.all("Container", "AddDevice").into_iter().collect();
    assert_eq!(
        devices,
        BTreeSet::from(["nvidia.com/gpu=all", "/dev/net/tun"]),
        "AddDevice="
    );
    let caps: BTreeSet<&str> = u
        .all("Container", "AddCapability")
        .into_iter()
        .flat_map(str::split_whitespace)
        .collect();
    assert_eq!(
        caps,
        BTreeSet::from(["SYS_ADMIN", "NET_ADMIN"]),
        "AddCapability="
    );
    assert_eq!(
        u.one("Container", "SecurityLabelType"),
        "container_engine_t"
    );
    assert!(u.all("Container", "SecurityLabelDisable").is_empty());
    let args = u.all("Container", "PodmanArgs").join(" ");
    assert!(args.contains("--systemd=always"), "{args}");
    assert!(
        !args.contains("label=disable") && !args.contains("--privileged"),
        "{args}"
    );
}

/// The settings a first start of the shipped unit runs with: its `Environment=` lines and the
/// image's `LMGW_DEPLOYMENT=container` read as the start reads them, applied to a fresh data dir.
fn unit_settings(u: &Unit) -> Settings {
    let mut vars: std::collections::HashMap<String, String> = u
        .all("Container", "Environment")
        .into_iter()
        .map(|e| {
            let (k, v) = e
                .split_once('=')
                .unwrap_or_else(|| panic!("Environment={e}"));
            (k.to_string(), v.to_string())
        })
        .collect();
    vars.insert("LMGW_DEPLOYMENT".into(), "container".into());
    let env = lmgw_server::env::read(|k| vars.get(k).cloned())
        .unwrap_or_else(|e| panic!("the unit's environment does not start: {e}"));
    let mut s = Settings::default();
    lmgw_core::config::deployment::apply(&mut s, &env.container);
    s
}

/// `port:port` and `port:port/udp`.
fn tcp_udp(port: u16) -> [String; 2] {
    [format!("{port}:{port}"), format!("{port}:{port}/udp")]
}

#[test]
fn every_listener_and_webrtc_group_is_published_on_tcp_and_udp() {
    let u = unit();
    let published: BTreeSet<String> = u
        .all("Container", "PublishPort")
        .into_iter()
        .map(str::to_string)
        .collect();
    // What the unit's own start binds (§4.4): the TLS listener and its group when it serves
    // TLS, else the plain one and its; the device listener and its group either way. Behind TLS
    // the plain listener is the container's loopback, and publishing it would reach nothing.
    let s = unit_settings(&u);
    let devices = s.devices.effective_port(&s.bind_addr).unwrap();
    let mut wanted: BTreeSet<String> = tcp_udp(devices).into();
    let plain: std::net::SocketAddr = s.bind_addr.parse().unwrap();
    match s.tls.listener() {
        Some(tls) => {
            assert!(plain.ip().is_loopback(), "behind TLS: {plain}");
            let [tcp, _] = tcp_udp(tls.addr.port());
            wanted.insert(tcp);
            let udp = s.realtime.webrtc.tls_port(tls.addr.port());
            wanted.insert(format!("{udp}:{udp}/udp"));
        }
        None => {
            let [tcp, _] = tcp_udp(plain.port());
            wanted.insert(tcp);
            let udp = s.realtime.webrtc.plain_port(plain.port());
            wanted.insert(format!("{udp}:{udp}/udp"));
        }
    }
    assert_eq!(
        published, wanted,
        "PublishPort= vs the listeners the unit's start binds"
    );
    assert!(s.tls.on(), "the shipped unit serves TLS (§3.3)");
    // The image's EXPOSE names every listener the image may serve, each group with it: the plain
    // one (published without TLS), the TLS one, the device listener. What the unit publishes is
    // among them, each under its own number.
    let exposed: BTreeSet<String> = instructions(&read("Containerfile"), "EXPOSE")
        .into_iter()
        .flatten()
        .map(|p| {
            if p.contains('/') {
                p
            } else {
                format!("{p}/tcp")
            }
        })
        .collect();
    let image_ports = [
        http_port(),
        lmgw_core::config::TLS_LISTEN_PROPOSED
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .port(),
        devices,
    ];
    let all: BTreeSet<String> = image_ports
        .iter()
        .flat_map(|p| [format!("{p}/tcp"), format!("{p}/udp")])
        .collect();
    assert_eq!(
        exposed, all,
        "the Containerfile's EXPOSE vs the image's listeners"
    );
    for p in &published {
        let (host, container) = p.split_once(':').unwrap_or_else(|| panic!("{p}"));
        let (cport, proto) = container.split_once('/').unwrap_or((container, "tcp"));
        assert_eq!(host, cport, "{p}: published under its own number");
        assert!(
            exposed.contains(&format!("{cport}/{proto}")),
            "{p} is not EXPOSEd"
        );
    }
}

#[test]
fn the_tls_pair_comes_from_a_read_only_host_directory() {
    let u = unit();
    let s = unit_settings(&u);
    let mount = u
        .all("Container", "Volume")
        .into_iter()
        .find(|v| v.split(':').nth(1) == Some("/etc/lmgw/tls"))
        .expect("a Volume= for /etc/lmgw/tls (D8)");
    let parts: Vec<&str> = mount.split(':').collect();
    assert_eq!(
        parts[0], "%h/lmgw-tls",
        "a host directory of its own, never a broad path"
    );
    let opts: BTreeSet<&str> = parts.get(2).copied().unwrap_or("").split(',').collect();
    assert!(
        opts.contains("ro") && opts.contains("z"),
        "{mount}: read-only and labelled"
    );
    for path in [&s.tls.cert_path, &s.tls.key_path] {
        assert!(
            path.starts_with("/etc/lmgw/tls/"),
            "{path} is in the mounted directory"
        );
    }
    let public = lmgw_core::config::parse_public_url(&s.public_url).expect("LMGW_PUBLIC_URL");
    assert_eq!(public.scheme, lmgw_core::net::Scheme::Https);
    assert_eq!(
        public.port,
        s.tls.listener().unwrap().addr.port(),
        "the published TLS port"
    );
}

/// The device CA's wrapping key (§9, WP8): the Podman secret the core reads, at its default
/// target, readable by lmgw's user (root in the outer container) only, and the unit says how
/// to make it.
#[test]
fn the_unit_hands_lmgw_the_wrap_key_secret() {
    use lmgw_core::devices::tls::{WRAP_KEY_SECRET, WRAP_KEY_SECRET_HOW, WRAP_KEY_SECRET_NAME};
    let u = unit();
    let secret = u.one("Container", "Secret");
    let mut parts = secret.split(',');
    assert_eq!(parts.next(), Some(WRAP_KEY_SECRET_NAME));
    let opts: BTreeSet<&str> = parts.collect();
    assert_eq!(opts, BTreeSet::from(["mode=0400"]), "{secret}");
    assert!(
        !opts
            .iter()
            .any(|o| o.starts_with("target=") || o.starts_with("type=")),
        "the default mount target"
    );
    assert_eq!(
        WRAP_KEY_SECRET,
        format!("/run/secrets/{WRAP_KEY_SECRET_NAME}"),
        "Podman's default target for a mounted secret"
    );
    assert!(
        read("lmgw.container").contains(&format!("#   {WRAP_KEY_SECRET_HOW}\n")),
        "the unit shows how the secret is made, as lmgw's messages say it"
    );

    // A missing secret fails the start before Podman's bare "no such secret": the check runs on
    // the host as the service account, says how to make the secret, with the same command lmgw's
    // messages give, and how to run plain, to the journal, and fails.
    let pre = u.one("Service", "ExecStartPre");
    let script = pre
        .strip_prefix("/bin/sh -c '")
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or_else(|| panic!("one sh -c script: {pre}"));
    assert!(
        !script.contains('\''),
        "one quoted word for systemd: {script}"
    );
    assert!(
        script.starts_with(&format!(
            "podman secret exists {WRAP_KEY_SECRET_NAME} || {{ echo \""
        )),
        "{script}"
    );
    assert!(script.ends_with("\" >&2; exit 1; }"), "{script}");
    assert!(script.contains(WRAP_KEY_SECRET_HOW), "{script}");
    assert!(
        script.contains(&format!("remove the Secret={WRAP_KEY_SECRET_NAME} line")),
        "{script}"
    );
    // systemd would expand a specifier (%) or a variable ($), and read a backslash as an escape.
    assert!(!script.contains(['%', '$', '\\']), "{script}");
}

#[test]
fn the_unit_is_ready_once_lmgw_answers_and_a_long_start_is_not_unhealthy() {
    let u = unit();
    let probe = format!("curl -fsS http://127.0.0.1:{}/api/version", http_port());
    assert_eq!(u.one("Container", "HealthCmd"), probe);
    assert_eq!(
        u.one("Container", "HealthStartupCmd"),
        probe,
        "the startup probe asks the same as the regular check"
    );
    let interval = u.duration_seconds("Container", "HealthStartupInterval");
    assert!(
        (1..=5).contains(&interval),
        "a startup probe every {interval} s: ready should follow the first answer closely"
    );
    assert!(
        u.all("Container", "HealthStartupRetries").is_empty(),
        "the startup probe never restarts the container: TimeoutStartSec decides"
    );
    assert_eq!(u.one("Container", "Notify"), "healthy");
    let start = u.seconds("Service", "TimeoutStartSec");
    assert!(
        start >= 900,
        "TimeoutStartSec={start}: a migrating start's figure (§3.3)"
    );
    assert_eq!(
        u.duration_seconds("Container", "HealthStartPeriod"),
        start,
        "HealthStartPeriod= is TimeoutStartSec's, so a migrating start is never marked unhealthy"
    );
    assert_eq!(u.one("Service", "Restart"), "always");
    // Quadlet waits for the network itself (podman-user-wait-network-online.service).
    assert!(u.all("Unit", "Wants").is_empty() && u.all("Unit", "After").is_empty());
}

#[test]
fn the_unit_mounts_the_images_volumes_from_their_volume_files() {
    let u = unit();
    let containerfile = read("Containerfile");
    let declared = containerfile
        .lines()
        .find_map(|l| l.strip_prefix("VOLUME "))
        .expect("the Containerfile declares its volumes");
    for (target, volume_file) in [
        ("/data", "lmgw-data.volume"),
        ("/var/lib/containers", "lmgw-store.volume"),
        ("/var/tmp", "lmgw-buildcache.volume"),
    ] {
        let mount = u
            .all("Container", "Volume")
            .into_iter()
            .find(|v| v.split(':').nth(1) == Some(target))
            .unwrap_or_else(|| panic!("no Volume= for {target}"));
        let source = mount.split(':').next().unwrap();
        assert_eq!(source, volume_file, "{target}");
        assert!(
            source.ends_with(".volume") && packaging_dir().join(source).is_file(),
            "{target} comes from {source}, which must be a .volume file beside the unit"
        );
        let volume = entries(&read(source));
        assert!(
            volume.iter().all(|(s, _, _)| s == "Volume"),
            "{source}: {volume:?}"
        );
        // Design §3.2's names, not Quadlet's default systemd-<stem>.
        let name: Vec<&str> = volume
            .iter()
            .filter(|(_, k, _)| k == "VolumeName")
            .map(|(_, _, v)| v.as_str())
            .collect();
        assert_eq!(
            name,
            [source.strip_suffix(".volume").unwrap()],
            "{source}: VolumeName="
        );
        assert!(
            declared.contains(&format!("\"{target}\"")),
            "the Containerfile's VOLUME lacks {target}: {declared}"
        );
    }
}

#[test]
fn the_containerfile_starts_lmgw_server_in_container_mode_under_an_init() {
    let c = read("Containerfile");
    for env in [
        "LMGW_DEPLOYMENT=container",
        "LMGW_DATA_DIR=/data",
        "XDG_RUNTIME_DIR=/run/user/0",
        // --systemd=always puts a tmpfs (host RAM) on /tmp; /var/tmp is a volume (D21).
        "TMPDIR=/var/tmp",
    ] {
        assert!(c.contains(env), "the Containerfile's ENV lacks {env}");
    }
    assert!(
        c.contains(r#"ENTRYPOINT ["/usr/libexec/catatonit/catatonit", "--""#),
        "catatonit is PID 1 (D5)"
    );
    let entry = read("entrypoint.sh");
    assert!(entry.contains("exec /usr/local/bin/lmgw-server"), "{entry}");
    assert!(c.contains("COPY lmgw-server /usr/local/bin/lmgw-server"));
}

#[test]
fn the_toolkit_repository_is_the_desktops_with_the_pinned_key() {
    let repo = entries(&read("nvidia-container-toolkit.repo"));
    let desktop = entries(REPO_TEXT);
    // Every line but the name is lmgw-machine's REPO_TEXT (machine check D5, D19): the same
    // section, channel, package list, checks, `enabled` and key location.
    let keyed = |e: &[(String, String, String)]| -> BTreeSet<(String, String, String)> {
        e.iter().filter(|(_, k, _)| k != "name").cloned().collect()
    };
    assert_eq!(keyed(&repo), keyed(&desktop));
    assert_eq!(repo.len(), desktop.len(), "{repo:?}");
    let get = |k: &str| -> Vec<&str> {
        repo.iter()
            .filter(|(_, key, _)| key == k)
            .map(|(_, _, v)| v.as_str())
            .collect()
    };
    let sections: BTreeSet<&str> = repo.iter().map(|(s, _, _)| s.as_str()).collect();
    assert_eq!(sections, BTreeSet::from([REPO_ID]));
    assert_eq!(get("gpgcheck"), ["1"], "D19");
    assert_eq!(get("repo_gpgcheck"), ["1"]);
    assert_eq!(get("enabled"), ["1"]);
    assert_eq!(get("includepkgs"), [REPO_INCLUDEPKGS.join(" ").as_str()]);
    assert!(get("baseurl")[0].starts_with(NVIDIA_STABLE_PREFIX));
    assert_eq!(get("gpgkey"), [KEY_URL], "a local key, never a download");

    let c = read("Containerfile");
    let copies = instructions(&c, "COPY");
    let copied_to = |dest: &str| {
        copies
            .iter()
            .any(|a| a.last().map(String::as_str) == Some(dest))
    };
    assert!(
        copied_to(REPO_FILE),
        "the Containerfile copies the repository to {REPO_FILE}"
    );
    assert!(
        copied_to(KEY_FILE),
        "the Containerfile copies the key to {KEY_FILE}"
    );
    assert!(
        c.contains(&format!("rpmkeys --import {KEY_FILE}")),
        "the Containerfile imports the key it copied"
    );
    // The import's check is an exact comparison of the one key it added with the fingerprint,
    // not a search for it in the whole keyring.
    assert!(
        c.contains(&format!("[ \"$added\" != \"{NVIDIA_KEY_FINGERPRINT}\" ]")),
        "the Containerfile checks the added key against lmgw-machine's pinned fingerprint"
    );
}

#[test]
fn the_build_context_holds_exactly_what_the_containerfile_copies() {
    let script = read_path(&packaging_dir().join("../../ci/build-image.sh"));
    // The packaging/server/ files ci/build-image.sh copies, from its one brace list.
    let list = script
        .lines()
        .find_map(|l| l.trim().strip_prefix("cp packaging/server/{"))
        .and_then(|l| l.split_once('}'))
        .map(|(names, _)| names)
        .expect("ci/build-image.sh copies packaging/server/{…} into the context");
    let from_packaging: BTreeSet<&str> = list.split(',').collect();
    // Every other top-level entry of the context: the first path component after "$CTX/" of
    // each cp or install.
    let mut context: BTreeSet<String> = from_packaging.iter().map(|n| n.to_string()).collect();
    for line in script.lines().map(str::trim) {
        if !(line.starts_with("cp ") || line.starts_with("install ")) {
            continue;
        }
        if let Some((_, rest)) = line.rsplit_once("\"$CTX/") {
            let first = rest.split(['/', '"']).next().unwrap();
            if !first.is_empty() {
                context.insert(first.to_string());
            }
        }
    }
    assert!(
        context.remove("Containerfile"),
        "the context has the Containerfile"
    );
    let copies = instructions(&read("Containerfile"), "COPY");
    let sources: BTreeSet<String> = copies
        .iter()
        .map(|a| {
            assert_eq!(a.len(), 2, "one source per COPY: {a:?}");
            a[0].trim_end_matches('/').to_string()
        })
        .collect();
    assert_eq!(
        sources, context,
        "the Containerfile's COPY sources vs ci/build-image.sh's context"
    );

    // packaging/server/'s files are the image's or the host's (the unit and its volumes): none is
    // left out of both.
    for entry in std::fs::read_dir(packaging_dir()).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let host = name.ends_with(".container") || name.ends_with(".volume");
        assert!(
            host || from_packaging.contains(name.as_str()),
            "packaging/server/{name} is neither copied into the image nor a host unit"
        );
        assert!(!(host && from_packaging.contains(name.as_str())), "{name}");
    }
}
