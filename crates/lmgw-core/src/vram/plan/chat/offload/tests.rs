//! The overrides as llama.cpp lists them, what they keep in host memory,
//! and the footprint that follows.

use super::*;
use crate::config::LlamaParams;
use crate::gguf::{HeadTensors, TensorList};
use crate::vram::plan::chat::{footprint, ChatFiles, Shape};
use crate::vram::plan::GgufFiles;

const MIB: u64 = 1024 * 1024;

fn args(s: &[&str]) -> Vec<String> {
    s.iter().map(|s| s.to_string()).collect()
}

/// The bytes of layer `i`'s routed experts in [`moe`]: up, gate and down.
fn exps(i: u64) -> [u64; 3] {
    [(i + 1) * MIB, (i + 1) * MIB, 2 * (i + 1) * MIB]
}

/// A mixture-of-experts header (Gemma 4 26B's shape, 30 layers) with 12
/// layers of tensors listed with an untied head: per layer an
/// attention matrix, the router, a shared expert and the routed experts
/// ([`exps`]); `token_embd` (kept on the host) and `output`.
fn moe() -> ModelSummary {
    let mut t: Vec<(String, u64)> = vec![("token_embd.weight".into(), 40 * MIB)];
    for i in 0..12u64 {
        let [up, gate, down] = exps(i);
        t.extend([
            (format!("blk.{i}.attn_q.weight"), 3 * MIB),
            (format!("blk.{i}.ffn_gate_inp.weight"), 1024),
            (format!("blk.{i}.ffn_up_shexp.weight"), 5 * MIB),
            (format!("blk.{i}.ffn_up_exps.weight"), up),
            (format!("blk.{i}.ffn_gate_exps.weight"), gate),
            (format!("blk.{i}.ffn_down_exps.weight"), down),
        ]);
    }
    t.push(("output.weight".into(), 30 * MIB));
    let mut s = crate::vram::compute::chat::tests::gemma4_26b();
    s.head_tensors = HeadTensors {
        token_embd: Some(40 * MIB),
        per_layer: None,
        output: Some(30 * MIB),
    };
    s.host_input_bytes = 40 * MIB;
    s.tensor_list = TensorList {
        tensors: t,
        parts: 1,
    };
    s
}

/// The routed experts of layers `range`.
fn experts(range: std::ops::Range<u64>) -> u64 {
    range.map(|i| exps(i).iter().sum::<u64>()).sum()
}

#[test]
fn the_overrides_come_in_llama_cpps_order() {
    let o = overrides(
        Some(3),
        &args(&[
            "-ot",
            "a=CPU,b=CUDA0",
            "--n-cpu-moe",
            "7",
            "-cmoe",
            "--n-cpu-ffn=2",
            "--spec-draft-n-cpu-moe",
            "4",
        ]),
        Whose::Model,
    );
    // The field first; its copy in the args is dropped; the draft's flag
    // is the draft's.
    assert_eq!(o.len(), 5, "{o:?}");
    assert!(matches!(
        &o[0],
        Override::FirstLayers {
            family: Family::Experts,
            layers: 3,
            ..
        }
    ));
    assert!(matches!(&o[1], Override::Pattern { pattern, cpu: true, .. } if pattern == "a"));
    assert!(matches!(&o[2], Override::Pattern { pattern, cpu: false, .. } if pattern == "b"));
    assert!(matches!(&o[3], Override::Pattern { pattern, cpu: true, .. } if pattern == EXPERTS));
    assert!(matches!(
        &o[4],
        Override::FirstLayers {
            family: Family::Dense,
            layers: 2,
            ..
        }
    ));

    // Without the field the args' copy counts, in every spelling
    // llama-server accepts (underscores for dashes among them). It refuses
    // `--n-cpu-moe=7`, which is read the same here all the same.
    for a in [
        &["--n-cpu-moe", "7"][..],
        &["-ncmoe", "7"],
        &["--n_cpu_moe", "7"],
        &["--n-cpu-moe=7"],
    ] {
        let o = overrides(None, &args(a), Whose::Model);
        assert!(
            matches!(o.as_slice(), [Override::FirstLayers { layers: 7, .. }]),
            "{a:?}: {o:?}"
        );
    }
    // A field of 0 keeps nothing on the CPU and still drops the copy, in
    // either spelling.
    assert!(overrides(Some(0), &args(&["-ncmoe", "7"]), Whose::Model).is_empty());
    assert!(overrides(Some(0), &args(&["--n_cpu_moe", "7"]), Whose::Model).is_empty());
    // The other flags' underscore spellings.
    let o = overrides(
        None,
        &args(&[
            "--cpu_moe",
            "--override_tensor",
            "x=CPU",
            "--n_cpu_ffn",
            "2",
        ]),
        Whose::Model,
    );
    assert_eq!(o.len(), 3, "{o:?}");
    assert!(matches!(&o[0], Override::Pattern { pattern, cpu: true, .. } if pattern == EXPERTS));
    assert!(matches!(&o[1], Override::Pattern { pattern, cpu: true, .. } if pattern == "x"));
    assert!(matches!(
        &o[2],
        Override::FirstLayers {
            family: Family::Dense,
            layers: 2,
            ..
        }
    ));
    let d = overrides(None, &args(&["--spec_draft_cpu_moe"]), Whose::Draft);
    assert_eq!(d.len(), 1, "{d:?}");

    let d = overrides(
        Some(3),
        &args(&[
            "-ncmoed",
            "4",
            "--cpu-moe-draft",
            "-otd",
            "x=CPU",
            "-ncmoe",
            "9",
        ]),
        Whose::Draft,
    );
    assert_eq!(d.len(), 3, "{d:?}");
    assert!(matches!(&d[0], Override::FirstLayers { layers: 4, .. }));
}

#[test]
fn op_offload_is_on_unless_the_last_word_is_off() {
    assert!(op_offload(&[]));
    assert!(!op_offload(&args(&["--no-op-offload"])));
    assert!(op_offload(&args(&["--no-op-offload", "--op-offload"])));
    assert!(!op_offload(&args(&["--no_op_offload"])));
}

/// `--n-cpu-moe 2` keeps layers 0 and 1's routed experts on the CPU, not
/// layers 10 and 11 (`blk\.1\.` is no `blk.11.`), nor a shared expert or
/// the router.
#[test]
fn n_cpu_moe_keeps_the_first_layers_routed_experts() {
    let s = moe();
    let p = place(&s, &overrides(Some(2), &[], Whose::Model), true, "its").unwrap();
    assert_eq!(p.off_card, experts(0..2));
    assert_eq!(p.onto_card, 0);
    assert_eq!(p.largest_main, 2 * 2 * MIB, "layer 1's down matrix");
    assert!(p.notes[0].contains("by --n-cpu-moe 2"), "{:?}", p.notes);

    let all = place(
        &s,
        &overrides(None, &args(&["--cpu-moe"]), Whose::Model),
        true,
        "its",
    );
    assert_eq!(all.unwrap().off_card, experts(0..12));
}

/// `-ot` patterns are searched for in the name, the first match wins, and
/// one to a device keeps the input embeddings on the card only when it
/// matches them.
#[test]
fn override_tensor_patterns_match_like_llama_cpp() {
    let s = moe();
    let place_args =
        |a: &[&str]| place(&s, &overrides(None, &args(a), Whose::Model), true, "its").unwrap();
    let p = place_args(&["-ot", r"blk\.(1[01])\.ffn_.*_exps.*=CPU"]);
    assert_eq!(p.off_card, experts(10..12));
    assert_eq!(place_args(&["-ot", "exps=CPU"]).off_card, experts(0..12));
    // An earlier override to the card shadows the CPU one for layer 0.
    let p = place_args(&["-ot", r"blk\.0\.=CUDA0", "--cpu-moe"]);
    assert_eq!(p.off_card, experts(1..12));
    // The input embeddings: only an override that matches them moves them.
    assert_eq!(place_args(&["-ot", r"blk\.5\.=CUDA0"]).onto_card, 0);
    let p = place_args(&["-ot", "token_embd=CUDA0"]);
    assert_eq!(p.onto_card, 40 * MIB);
    assert!(
        p.notes.iter().any(|n| n.contains("input embeddings")),
        "{:?}",
        p.notes
    );
    // A dense model's FFN by `--n-cpu-ffn`; nothing of an MoE layer.
    assert_eq!(place_args(&["--n-cpu-ffn", "4"]).off_card, 0);
}

/// A pattern lmgw cannot compile (ECMAScript's `\cJ` control escape, which
/// `fancy-regex` does not read).
const UNREADABLE: &str = r"\cJ";

/// What can't be matched is charged to the card and said: a pattern
/// `fancy-regex` can't compile, a flag that matches nothing, a split set
/// whose later parts weren't read, a header without tensors.
#[test]
fn what_is_not_counted_is_said() {
    let s = moe();
    let p = place(
        &s,
        &overrides(
            None,
            &args(&["-ot", &format!("{UNREADABLE}exps=CPU")]),
            Whose::Model,
        ),
        true,
        "its",
    )
    .unwrap();
    assert_eq!(p.off_card, 0);
    assert!(
        p.notes[0].contains("not a pattern lmgw can match"),
        "{:?}",
        p.notes
    );
    // It may keep experts in host memory, so a cache for them is charged.
    assert!(p.experts_unknown);

    let p = place(
        &s,
        &overrides(None, &args(&["-ot", "nothing=CPU"]), Whose::Model),
        true,
        "its",
    )
    .unwrap();
    assert!(
        p.notes.iter().any(|n| n.contains("matches")),
        "{:?}",
        p.notes
    );

    let mut split = moe();
    split.split_count = Some(3);
    let p = place(&split, &overrides(Some(2), &[], Whose::Model), true, "its").unwrap();
    assert!(
        p.notes.iter().any(|n| n.contains("only 1 of the 3 parts")),
        "{:?}",
        p.notes
    );

    let mut bare = moe();
    bare.tensor_list = TensorList::default();
    let p = place(&bare, &overrides(Some(2), &[], Whose::Model), true, "its").unwrap();
    assert_eq!(p.off_card, 0);
    assert!(p.notes[0].contains("lists no tensors"), "{:?}", p.notes);
    assert!(place(&s, &[], true, "its").is_none());
}

fn files(s: ModelSummary) -> ChatFiles {
    let main: u64 = s.tensor_list.tensors.iter().map(|(_, b)| b).sum();
    ChatFiles {
        files: GgufFiles {
            main_bytes: main,
            companion_bytes: 0,
            summary: Ok(s),
            missing: Vec::new(),
        },
        draft: None,
    }
}

/// The footprint: the experts off the card, the largest of them charged to
/// the compute buffer for op offload, unless op offload is off or the
/// ubatch is below 32.
#[test]
fn the_footprint_counts_n_cpu_moe() {
    let s = moe();
    let main: u64 = s.tensor_list.tensors.iter().map(|(_, b)| b).sum();
    let row = |n: Option<i64>, ubatch: Option<i64>| LlamaParams {
        ctx_size: Some(4096),
        parallel: Some(1),
        n_cpu_moe: n,
        ubatch_size: ubatch,
        ..Default::default()
    };
    let plain = footprint(&files(moe()), "m.gguf", &Shape::of(&row(None, None), &[]));
    assert_eq!(plain.weights_bytes, main - 40 * MIB);
    assert_eq!(plain.note, None);

    let off = footprint(
        &files(moe()),
        "m.gguf",
        &Shape::of(&row(Some(9), None), &[]),
    );
    assert_eq!(off.weights_bytes, main - 40 * MIB - experts(0..9));
    assert_eq!(off.kv_cache_bytes, plain.kv_cache_bytes);
    assert_eq!(
        off.compute_bytes,
        plain.compute_bytes + 2 * 9 * MIB,
        "layer 8's down"
    );
    let note = off.note.unwrap();
    assert!(
        note.contains("not charged to the card") && note.contains("op offload"),
        "{note}"
    );

    let a = args(&["--no-op-offload"]);
    let no_copy = footprint(&files(moe()), "m.gguf", &Shape::of(&row(Some(9), None), &a));
    assert_eq!(no_copy.compute_bytes, plain.compute_bytes);
    let small = footprint(
        &files(moe()),
        "m.gguf",
        &Shape::of(&row(Some(9), Some(16)), &[]),
    );
    assert!(!small.note.unwrap().contains("op offload"));

    // A copy in the args counts like the field.
    let a = args(&["-ncmoe", "9"]);
    let in_args = footprint(&files(moe()), "m.gguf", &Shape::of(&row(None, None), &a));
    assert_eq!(in_args.weights_bytes, off.weights_bytes);
}

/// A large MoE case, its expert sizes as measured from the header of
/// `unsloth/Qwen3.8-Flash-Next-GGUF` UD-IQ4_XS (2026-11-09): 48 layers whose
/// routed experts add up to 56.24 GiB, the last seven 7.886 GiB. At
/// `n_cpu_moe 41` the card holds the weights less 48.35 GiB.
#[test]
fn n_cpu_moe_41_of_48_keeps_all_but_the_last_seven_layers_experts_off_the_card() {
    const GIB: f64 = (1024 * MIB) as f64;
    let total = 56.24 * GIB;
    let tail = 7.886 * GIB;
    let head_layer = ((total - tail) / 41.0) as u64;
    let tail_layer = (tail / 7.0) as u64;
    let mut s = moe();
    s.block_count = Some(48);
    s.tensor_list.tensors = (0..48u64)
        .map(|i| {
            let b = if i < 41 { head_layer } else { tail_layer };
            (format!("blk.{i}.ffn_gate_up_exps.weight"), b)
        })
        .collect();
    let p = place(&s, &overrides(Some(41), &[], Whose::Model), true, "its").unwrap();
    let off = p.off_card as f64 / GIB;
    assert!((off - (56.24 - 7.886)).abs() < 0.01, "{off}");
}

/// `--moe-cache-mib` is charged in full beside CPU-kept experts, from the
/// field or a copy in the args, and not when no experts are kept there.
#[test]
fn the_expert_cache_is_charged_while_experts_are_kept_in_host_memory() {
    let row = |n_cpu_moe: Option<i64>, cache: Option<i64>| LlamaParams {
        ctx_size: Some(4096),
        parallel: Some(1),
        n_cpu_moe,
        moe_cache_mib: cache,
        ..Default::default()
    };
    let fp =
        |p: &LlamaParams, a: &[&str]| footprint(&files(moe()), "m.gguf", &Shape::of(p, &args(a)));
    let off = fp(&row(Some(9), None), &[]);
    let cached = fp(&row(Some(9), Some(64)), &[]);
    assert_eq!(cached.weights_bytes, off.weights_bytes + 64 * MIB);
    assert!(cached.note.unwrap().contains("--moe-cache-mib 64"));
    let in_args = fp(&row(None, None), &["--cpu-moe", "--moe-cache-mib", "64"]);
    let all = fp(&row(None, None), &["--cpu-moe"]);
    assert_eq!(in_args.weights_bytes, all.weights_bytes + 64 * MIB);
    // No experts in host memory, nothing to cache: not charged, and said.
    let plain = fp(&row(None, None), &[]);
    let idle = fp(&row(None, Some(64)), &[]);
    assert_eq!(idle.weights_bytes, plain.weights_bytes);
    assert!(idle.note.unwrap().contains("not charged"));
    // A dense FFN on the CPU is no expert either.
    let dense = fp(&row(None, Some(64)), &["-ot", r"attn_q=CPU"]);
    assert!(dense
        .note
        .unwrap()
        .contains("--moe-cache-mib 64 is not charged"));
}

/// Review finding 1: a pattern to a device decides before the overrides
/// after it. A lookahead is read (`fancy-regex`), so its layers stay on the
/// card and the rest go to the CPU; one lmgw can't read may claim anything,
/// so nothing an override after it keeps in host memory is taken off the
/// card, the input embeddings are charged, and the expert cache is too.
#[test]
fn a_device_pattern_shadows_the_cpu_overrides_after_it() {
    let s = moe();
    let place_args =
        |a: &[&str]| place(&s, &overrides(None, &args(a), Whose::Model), true, "its").unwrap();
    let p = place_args(&[
        "-ot",
        r"blk\.(?!1[01]\.)\d+\.ffn_.*_exps=CUDA0",
        "--cpu-moe",
    ]);
    assert_eq!(p.off_card, experts(10..12), "{:?}", p.notes);
    assert!(!p.experts_unknown);

    let device = format!("{UNREADABLE}=CUDA0");
    let p = place_args(&["-ot", &device, "--cpu-moe"]);
    assert_eq!(p.off_card, 0, "{:?}", p.notes);
    assert_eq!(p.onto_card, 40 * MIB);
    assert!(p.experts_unknown);
    assert!(
        p.notes[0].contains("ahead of the overrides after it (--cpu-moe)"),
        "{:?}",
        p.notes
    );
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains(&fmt_bytes(experts(0..12))) && n.contains("charged to the card")),
        "{:?}",
        p.notes
    );
    // The field renders first, so the same pattern in the args comes after
    // it and shadows nothing of it.
    let p = place(
        &s,
        &overrides(Some(2), &args(&["-ot", &device]), Whose::Model),
        true,
        "its",
    )
    .unwrap();
    assert_eq!(p.off_card, experts(0..2), "{:?}", p.notes);

    // The footprint charges the expert cache beside it in full.
    let row = |cache: Option<i64>| LlamaParams {
        ctx_size: Some(4096),
        parallel: Some(1),
        moe_cache_mib: cache,
        ..Default::default()
    };
    let a = args(&["-ot", &device, "--cpu-moe"]);
    let plain = footprint(&files(moe()), "m.gguf", &Shape::of(&row(None), &a));
    let cached = footprint(&files(moe()), "m.gguf", &Shape::of(&row(Some(64)), &a));
    assert_eq!(cached.weights_bytes, plain.weights_bytes + 64 * MIB);
    let note = cached.note.unwrap();
    assert!(!note.contains("is not charged"), "{note}");
}

/// The MTP layers run in the MTP context, so op offload copies their
/// host-kept experts into its buffer, not the main one's; a weight outside
/// the layers (the output head) is copied for both.
#[test]
fn mtp_layers_experts_are_copied_for_the_mtp_context() {
    // The header of [`moe`] at the 12 layers its tensors list, the last an
    // MTP layer.
    let m = moe();
    let s = ModelSummary {
        expert_used_count: Some(8),
        expert_feed_forward_length: Some(704),
        nextn_predict_layers: Some(1),
        head_tensors: m.head_tensors.clone(),
        host_input_bytes: m.host_input_bytes,
        tensor_list: m.tensor_list.clone(),
        ..crate::vram::compute::chat::tests::gemma4(12, 2816, 2112, 16, (8, 2), 1024)
    };
    let p = place(
        &s,
        &overrides(None, &args(&["--cpu-moe"]), Whose::Model),
        true,
        "its",
    )
    .unwrap();
    assert_eq!(p.off_card, experts(0..12));
    assert_eq!(
        (p.largest_main, p.largest_mtp),
        (2 * 11 * MIB, 2 * 12 * MIB),
        "layer 10's down for the main context, layer 11's for the MTP one"
    );
    let p = place(
        &s,
        &overrides(None, &args(&["-ot", "output=CPU"]), Whose::Model),
        true,
        "its",
    )
    .unwrap();
    assert_eq!((p.largest_main, p.largest_mtp), (30 * MIB, 30 * MIB));

    let row = LlamaParams {
        ctx_size: Some(4096),
        parallel: Some(1),
        spec_type: Some("draft-mtp".into()),
        ..Default::default()
    };
    let fp = |a: &[&str]| footprint(&files(s.clone()), "m.gguf", &Shape::of(&row, &args(a)));
    let plain = fp(&[]);
    let off = fp(&["--cpu-moe"]);
    assert_eq!(off.compute_bytes, plain.compute_bytes + (22 + 24) * MIB);
    let note = off.note.unwrap();
    assert!(
        note.contains("its main context's compute buffer")
            && note.contains("the MTP context's compute buffer"),
        "{note}"
    );
}

/// A draft model's own overrides take its experts off the card from its
/// own file's bytes, and its context is charged the copy of the largest.
#[test]
fn a_draft_models_overrides_count_against_its_file() {
    let d = moe();
    let draft_bytes: u64 = d.tensor_list.tensors.iter().map(|(_, b)| b).sum();
    let mut f = files(moe());
    f.files.companion_bytes = draft_bytes;
    f.draft = Some(Ok(d));
    let row = LlamaParams {
        ctx_size: Some(4096),
        parallel: Some(1),
        draft_gguf_path: Some("d.gguf".into()),
        spec_type: Some("draft-simple".into()),
        ..Default::default()
    };
    let fp = |a: &[&str]| footprint(&f, "m.gguf", &Shape::of(&row, &args(a)));
    let plain = fp(&[]);
    let off = fp(&["--spec-draft-cpu-moe"]);
    assert_eq!(off.weights_bytes, plain.weights_bytes - experts(0..12));
    assert_eq!(off.compute_bytes, plain.compute_bytes + 2 * 12 * MIB);
    let note = off.note.unwrap();
    assert!(
        note.contains("of the draft model's weights stay in host memory")
            && note.contains("the draft context's compute buffer"),
        "{note}"
    );
    // The model's own flag leaves the draft model's experts where they are.
    let main_only = fp(&["--cpu-moe"]);
    assert_eq!(
        main_only.weights_bytes,
        plain.weights_bytes - experts(0..12)
    );
    assert!(!main_only.note.unwrap().contains("draft context's"));
}

/// The bytes of a layer's routed experts (up, gate, down) in Qwen3.8-Flash-Next UD-IQ4_XS, by kind ([`QWEN38_LAYER_KINDS`]): read
/// from its four parts' headers on 2026-11-09.
const QWEN38_EXPERT_KINDS: [[u64; 3]; 6] = [
    [268_697_600, 268_697_600, 235_929_600],
    [268_697_600, 268_697_600, 471_859_200],
    [321_126_400, 321_126_400, 471_859_200],
    [321_126_400, 321_126_400, 576_716_800],
    [445_644_800, 445_644_800, 471_859_200],
    [445_644_800, 445_644_800, 576_716_800],
];

/// Each of its 48 layers' expert kind.
const QWEN38_LAYER_KINDS: [usize; 48] = [
    3, 3, 3, 3, 5, 5, 3, 2, 0, 1, 0, 2, 3, 3, 2, 5, 5, 5, 5, 3, 3, 2, 3, 2, 3, 3, 3, 5, 5, 5, 4, 5,
    4, 5, 4, 4, 3, 5, 3, 3, 3, 3, 2, 2, 2, 3, 3, 5,
];

/// That file as the footprint reads it: its four parts' bytes, its
/// F16 projector's, and the tensors the overrides and the host-kept inputs
/// are matched against (the rest stays on the card in any case).
fn qwen38_files() -> ChatFiles {
    const TOKEN_EMBD: u64 = 337_715_200;
    const PER_LAYER: u64 = 28_800_138_240;
    const OUTPUT: u64 = 521_472_000;
    let mut tensors: Vec<(String, u64)> = vec![
        ("token_embd.weight".into(), TOKEN_EMBD),
        ("per_layer_token_embd.weight".into(), PER_LAYER),
        ("output.weight".into(), OUTPUT),
    ];
    for (i, kind) in QWEN38_LAYER_KINDS.iter().enumerate() {
        let [up, gate, down] = QWEN38_EXPERT_KINDS[*kind];
        tensors.extend([
            (format!("blk.{i}.ffn_up_exps.weight"), up),
            (format!("blk.{i}.ffn_gate_exps.weight"), gate),
            (format!("blk.{i}.ffn_down_exps.weight"), down),
        ]);
    }
    let s = ModelSummary {
        split_count: Some(4),
        head_tensors: HeadTensors {
            token_embd: Some(TOKEN_EMBD),
            per_layer: Some(PER_LAYER),
            output: Some(OUTPUT),
        },
        host_input_bytes: TOKEN_EMBD + PER_LAYER,
        tensor_list: TensorList { tensors, parts: 4 },
        ..crate::vram::compute::qsa::tests::qwen38_flash_next()
    };
    ChatFiles {
        files: GgufFiles {
            main_bytes: 10_946_618 + 49_604_491_008 + 15_256_783_680 + 28_800_138_432,
            companion_bytes: 904_004_000,
            summary: Ok(s),
            missing: Vec::new(),
        },
        draft: None,
    }
}

/// Review item 5: measured rows (2026-11-09, image official-master, direct llama-server probes on a 24 GiB test GPU, 23,028 MiB usable, with about 2,765 MiB held by other programs): ctx 262,144, q8_0, flash
/// attention, all layers on the card, the F16 projector, four auto slots
/// over a unified cache. The footprint plus the 1 GiB headroom covers each
/// process that loaded, and refuses the row that ran out of memory
/// (decision-models design, Appendix C.2, "Estimate against the process").
#[test]
fn measured_qwen38_rows_are_covered() {
    const CARD_FREE: u64 = 23_028 - 2_765;
    let files = qwen38_files();
    let row = |args_: &[&str], ub: i64, batch: Option<i64>| {
        let p = LlamaParams {
            ctx_size: Some(262_144),
            cache_type_k: Some("q8_0".into()),
            cache_type_v: Some("q8_0".into()),
            flash_attn: Some("on".into()),
            n_gpu_layers: Some(999),
            ubatch_size: Some(ub),
            batch_size: batch,
            ..Default::default()
        };
        let fp = footprint(&files, "q.gguf", &Shape::of(&p, &args(args_)));
        fp.total_bytes / MIB + 1024
    };
    // (args, ubatch, batch, the process's MiB as measured).
    for (a, ub, batch, process) in [
        (&["--n-cpu-moe", "43"][..], 1024, None, 18_963),
        (
            &["--cpu-moe", "--moe-cache-mib", "6144"],
            1024,
            None,
            19_082,
        ),
        (&["--n-cpu-moe", "46"], 2048, Some(2048), 17_511),
        (&["--n-cpu-moe", "41"], 512, None, 19_544),
    ] {
        let admitted = row(a, ub, batch);
        assert!(
            admitted >= process && admitted <= CARD_FREE,
            "{a:?} ubatch {ub}: {admitted} MiB charged, the process took {process}"
        );
    }
    // CUDA ran out of memory allocating the projector after the model.
    let oom = row(&["--n-cpu-moe", "43"], 2048, Some(2048));
    assert!(oom > CARD_FREE, "{oom} MiB fits {CARD_FREE}");
}
