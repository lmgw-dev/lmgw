//! Where the native launcher finds an engine's executable.
//!
//! - **On a platform with pins** (Windows on x86_64): the build of the
//!   class's engine this machine runs ([`engine_assets::Engines::ensure`]:
//!   the variant chosen from the recorded CUDA coverage, downloaded,
//!   verified and unpacked under `<engines>/<engine>/<tag>-<variant>/` on
//!   its first start).
//! - **Elsewhere** (Linux, where the native launcher is only built by the
//!   tests and by hand): the hand-installed layout,
//!   `<engines>/<engine>/<build>/<executable>`, where exactly one build
//!   directory is used; several are an error that lists them (no choice
//!   between hand-installed builds yet).
//!
//! `<engine>` is the class's engine (`llama`, `sdcpp`, `audio`).
//!
//! [`engine_assets::Engines::ensure`]: crate::runtime::engine_assets::Engines::ensure

use std::path::{Path, PathBuf};

pub use crate::runtime::engine_assets::{executable, executable_stem, ENGINES_DIR};
use crate::runtime::engine_assets::{pins::Variant, Engines};
use crate::runtime::launcher::StartWatch;
use crate::runtime::Class;

/// One engine build, found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// The executable.
    pub exe: PathBuf,
    /// Its build directory: the child's working directory, where the
    /// engine's own libraries sit beside it.
    pub dir: PathBuf,
    /// The build directory's name (`b12244-cuda12`, `llama-b12236`).
    pub build: String,
    /// The pinned variant; `None` for a hand-installed build.
    pub variant: Option<Variant>,
    /// Why this build: the variant choice's sentence.
    pub why: Option<String>,
    /// Its first start JIT-compiles the CUDA kernels.
    pub jit: bool,
}

/// The build of `class`'s engine this launcher starts (module doc), for
/// the start `watch` speaks for.
pub async fn resolve(
    engines: &Engines,
    class: Class,
    watch: &dyn StartWatch,
) -> Result<Install, String> {
    match engines.ensure(class, watch).await? {
        Some(i) => {
            let exe = i.dir.join(executable(class));
            Ok(Install {
                exe,
                dir: i.dir,
                build: i.build,
                variant: Some(i.variant),
                why: Some(i.why),
                jit: i.jit,
            })
        }
        None => scan(engines.root(), class),
    }
}

/// The one hand-installed build of `class`'s engine under `root`, or the
/// sentence that says what was looked for.
pub fn scan(root: &Path, class: Class) -> Result<Install, String> {
    let engine_dir = root.join(class.engine());
    let exe_name = executable(class);
    let expected = || format!("{}", engine_dir.join("<build>").join(&exe_name).display());
    let entries = match std::fs::read_dir(&engine_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "no {} build is installed: lmgw starts it as {} (this platform has no pinned \
                 release builds yet, Windows design §4.2, so the build is installed by hand)",
                executable_stem(class),
                expected()
            ))
        }
        Err(e) => return Err(format!("{} could not be read: {e}", engine_dir.display())),
    };
    let mut builds: Vec<String> = entries
        .filter_map(Result::ok)
        // `is_dir` follows a link: `current -> llama-b12236` is a build dir.
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // The installer's own directories (`.staging-…`) are no builds.
        .filter(|n| !n.starts_with('.'))
        .collect();
    builds.sort();
    let build = match builds.as_slice() {
        [] => {
            return Err(format!(
                "no {} build is installed: {} holds no build directory, and lmgw starts it as {}",
                executable_stem(class),
                engine_dir.display(),
                expected()
            ))
        }
        [one] => one.clone(),
        several => {
            return Err(format!(
                "{} holds {} builds ({}), and on this platform lmgw does not choose between \
                 hand-installed builds yet: keep the one to run",
                engine_dir.display(),
                several.len(),
                several.join(", ")
            ))
        }
    };
    let dir = engine_dir.join(&build);
    let exe = dir.join(&exe_name);
    if !exe.is_file() {
        return Err(format!(
            "the {} build {build} has no {exe_name}: lmgw starts it as {}",
            executable_stem(class),
            exe.display()
        ));
    }
    Ok(Install {
        exe,
        dir,
        build,
        variant: None,
        why: None,
        jit: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_one_build_is_found_and_its_absence_names_the_path() {
        let d = tempfile::tempdir().unwrap();
        let err = scan(d.path(), Class::Chat).unwrap_err();
        assert!(err.contains("llama"), "{err}");
        assert!(err.contains(&executable(Class::Chat)), "{err}");

        let build = d.path().join("llama").join("llama-b1");
        std::fs::create_dir_all(&build).unwrap();
        let err = scan(d.path(), Class::Chat).unwrap_err();
        assert!(
            err.contains(&build.join(executable(Class::Chat)).display().to_string()),
            "{err}"
        );

        std::fs::write(build.join(executable(Class::Chat)), b"").unwrap();
        let found = scan(d.path(), Class::Chat).unwrap();
        assert_eq!(found.dir, build);
        assert_eq!(found.build, "llama-b1");
        // The aux class runs the same engine.
        assert_eq!(scan(d.path(), Class::Aux).unwrap(), found);
        // The installer's staging dir is not a build.
        std::fs::create_dir_all(d.path().join("llama").join(".staging-b2-cpu")).unwrap();
        assert_eq!(scan(d.path(), Class::Chat).unwrap(), found);

        std::fs::create_dir_all(d.path().join("llama").join("llama-b2")).unwrap();
        let err = scan(d.path(), Class::Chat).unwrap_err();
        assert!(err.contains("llama-b1, llama-b2"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_build_dir_is_a_build() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("elsewhere").join("llama-b12236");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join(executable(Class::Chat)), b"").unwrap();
        let engine = d.path().join("llama");
        std::fs::create_dir_all(&engine).unwrap();
        std::os::unix::fs::symlink(&real, engine.join("current")).unwrap();
        let found = scan(d.path(), Class::Chat).unwrap();
        assert_eq!(found.build, "current");
        assert!(found.exe.is_file());
    }
}
