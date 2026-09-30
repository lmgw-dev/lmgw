//! Progress out of `podman build` output (container-builds §5 step 5):
//! podman's own `STEP n/m:` lines (prefixed `[stage/stages] ` in a
//! multi-stage build), cmake's `[ 45%]` and ninja's `[123/456]`.

/// One progress fact a line carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Progress {
    /// podman started step `n` of `m` of the current stage — stage `k` of
    /// `N` when the line carried podman's `[k/N]` prefix.
    Step {
        stage: Option<(u64, u64)>,
        n: u64,
        m: u64,
    },
    /// A compile got this far, in percent — cmake's own, or ninja's count
    /// turned into one.
    Percent(u64),
}

/// The progress `line` reports, if any.
pub(crate) fn parse(line: &str) -> Option<Progress> {
    let t = line.trim_start();
    // `[1/2] STEP 3/12: RUN …` — the stage prefix is podman's, not ninja's.
    let (bracket, rest) = match t.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((inner, rest)) => (Some(inner), rest.trim_start()),
        None => (None, t),
    };
    if let Some(step) = rest.strip_prefix("STEP ") {
        let (frac, _) = step.split_once(':')?;
        let (n, m) = fraction(frac)?;
        let stage = bracket.and_then(fraction);
        return Some(Progress::Step { stage, n, m });
    }
    let inner = bracket?.trim();
    // podman's other stage-prefixed lines (`[2/2] COMMIT …`) are not a count.
    if rest.starts_with("COMMIT") {
        return None;
    }
    if let Some(p) = inner.strip_suffix('%') {
        let p: u64 = p.trim().parse().ok()?;
        return Some(Progress::Percent(p.min(100)));
    }
    let (n, m) = fraction(inner)?;
    Some(Progress::Percent(n.min(m) * 100 / m))
}

/// The whole build's progress in percent, for the job row — what the
/// progress bar shows. podman's stages count equally, the steps of a stage
/// equally within it, and a compile's own percent (`within`) fills the step
/// it runs in. `step` alone restarts in every stage: a bar driven by it read
/// 100 % at the end of the first of six stages and then fell back.
pub(crate) fn overall_percent(
    stage: Option<(u64, u64)>,
    step: Option<(u64, u64)>,
    within: Option<u64>,
) -> Option<u64> {
    let Some((n, m)) = step else {
        return within;
    };
    let (k, stages) = stage.unwrap_or((1, 1));
    let (k, stages) = (k.clamp(1, stages.max(1)), stages.max(1));
    let n = n.clamp(1, m.max(1));
    let in_step = within.unwrap_or(0).min(100) as f64 / 100.0;
    let in_stage = ((n - 1) as f64 + in_step) / m.max(1) as f64;
    let total = ((k - 1) as f64 + in_stage) / stages as f64;
    Some(((total * 100.0).floor() as u64).min(100))
}

/// `n/m` with `m > 0`.
fn fraction(s: &str) -> Option<(u64, u64)> {
    let (n, m) = s.trim().split_once('/')?;
    let (n, m): (u64, u64) = (n.parse().ok()?, m.parse().ok()?);
    (m > 0).then_some((n, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn podman_steps_cmake_percent_and_ninja_counts_are_read() {
        assert_eq!(
            parse("STEP 3/12: RUN apt-get update"),
            Some(Progress::Step {
                stage: None,
                n: 3,
                m: 12
            })
        );
        assert_eq!(
            parse("[1/2] STEP 5/9: RUN cmake --build build"),
            Some(Progress::Step {
                stage: Some((1, 2)),
                n: 5,
                m: 9
            })
        );
        assert_eq!(
            parse("[ 45%] Building CXX object ggml/src/ggml.cpp.o"),
            Some(Progress::Percent(45))
        );
        assert_eq!(
            parse("[100%] Built target llama-server"),
            Some(Progress::Percent(100))
        );
        assert_eq!(
            parse("[123/456] Building CUDA object ggml-cuda/fattn.cu.o"),
            Some(Progress::Percent(26))
        );
        assert_eq!(parse("[2/2] COMMIT localhost/lmgw-llama-server:x"), None);
        assert_eq!(parse("--> 3f1c2a"), None);
        assert_eq!(parse("[0/0] nothing"), None);
        assert_eq!(parse("plain output"), None);
    }

    /// Found in the container-builds e2e: official's `[1/6] STEP 7/7` showed
    /// as 100 % while five stages were still to come.
    #[test]
    fn the_overall_percent_climbs_across_stages_and_never_falls_back() {
        assert_eq!(overall_percent(None, None, None), None);
        assert_eq!(overall_percent(None, None, Some(40)), Some(40));
        assert_eq!(overall_percent(None, Some((1, 4)), None), Some(0));
        assert_eq!(overall_percent(None, Some((3, 4)), Some(50)), Some(62));
        assert_eq!(overall_percent(Some((1, 6)), Some((7, 7)), None), Some(14));
        assert_eq!(
            overall_percent(Some((6, 6)), Some((7, 7)), Some(100)),
            Some(100)
        );

        // Official llama.cpp's line sequence, compile included: the bar only
        // ever grows (stages 4 and 5 are skipped for --target server).
        let lines = [
            "[1/6] STEP 1/7: FROM docker.io/node:24 AS web",
            "[1/6] STEP 7/7: RUN npm run build",
            "[2/6] STEP 1/13: FROM docker.io/nvidia/cuda:13.0.0-devel-ubuntu24.04 AS build",
            "[2/6] STEP 11/13: RUN --mount=type=cache,id=lmgw-llama-cuda,target=/ccache …",
            "[  5%] Building C object ggml/src/CMakeFiles/ggml-base.dir/ggml.c.o",
            "[ 97%] Linking CXX executable ../../bin/llama-server",
            "[2/6] STEP 12/13: RUN mkdir -p /app/lib",
            "[3/6] STEP 1/9: FROM docker.io/nvidia/cuda:13.0.0-runtime-ubuntu24.04 AS base",
            "[6/6] STEP 1/7: FROM 3d7c3e1b AS server",
            "[6/6] STEP 7/7: LABEL dev.lmgw.run=1",
        ];
        let (mut stage, mut step, mut within) = (None, None, None);
        let mut last = 0;
        for l in lines {
            match parse(l) {
                Some(Progress::Step { stage: s, n, m }) => {
                    stage = s;
                    step = Some((n, m));
                    within = None;
                }
                Some(Progress::Percent(p)) => within = Some(p),
                None => {}
            }
            let now = overall_percent(stage, step, within).unwrap();
            assert!(now >= last, "{l}: {now} < {last}");
            last = now;
        }
        assert!(last >= 95, "{last}");
    }
}
