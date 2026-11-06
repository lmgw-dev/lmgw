//! The benchmark's container gets lmgw's own CDI spec like a model start
//! (machine-check design §3.6.4, the guard's bench part): the plan's command
//! line, the container the run launches and the command line the run stores
//! all put `--cdi-spec-dir` before `run`, and the run makes the spec fresh
//! before it launches.

use super::*;
use crate::support::cdi_machine::CdiMachine;

#[tokio::test]
async fn a_gpu_rows_bench_container_and_its_command_lines_carry_lmgws_spec_dir() {
    let w = World::new(quick()).await;
    let m = CdiMachine::new();
    w.state().set_cdi_for_tests(m.own_spec(&w.state().data_dir));
    let want = format!(
        "podman --cdi-spec-dir {} run -d --replace",
        m.spec_dir(&w.prefix()).display()
    );

    let plan = w.ok("bench_plan", json!({"model_id": "qwen"})).await;
    let cl = plan["command_line"].as_str().unwrap();
    assert!(cl.starts_with(&want), "{cl}");
    assert_eq!(m.generations(), 0, "a plan writes nothing");

    let (id, job) = w.start(json!({"model_id": "qwen", "repetitions": 1})).await;
    w.finished(job).await;
    let d = w.run(id).await;
    assert_eq!(d["run"]["status"], "done", "{}", d["run"]["error"]);
    let stored = d["run"]["command_line"].as_str().unwrap();
    assert!(stored.starts_with(&want), "{stored}");
    let launched: Vec<Vec<String>> = w
        .launcher
        .calls()
        .into_iter()
        .filter(|c| c.iter().any(|a| a == "run"))
        .collect();
    assert_eq!(launched.len(), 1, "{launched:?}");
    assert_eq!(
        launched[0][..3],
        [
            "--cdi-spec-dir".to_string(),
            m.spec_dir(&w.prefix()).display().to_string(),
            "run".to_string()
        ]
    );
    assert_eq!(
        m.generations(),
        1,
        "the run made the spec before it launched"
    );
}
