//! The optional tools of the capability table ([`super`], design §4.1,
//! §5.1, §6): their packages, installers, probes and licences.

use lmgw_api_types::documents::InstallPlatform;

use super::ToolId;

/// How a tool is installed outside Fedora.
#[derive(Debug, Clone, Copy)]
pub enum Install {
    /// A command to run.
    Command(&'static str),
    /// A page with the official installer.
    Link(&'static str),
    /// Nothing to install yet, and why.
    NotYet(&'static str),
    /// It is installed with lmgw itself ([`Packages::bundled`]): there is
    /// no package of its own, lmgw is installed again.
    Bundled,
    /// A distribution lmgw has no command for: the tool's common name, for
    /// a sentence that sends the person to their package manager.
    Manual(&'static str),
}

/// A tool's packages and installers (design §5.1, §6).
#[derive(Debug, Clone, Copy)]
pub struct Packages {
    /// What the tool is commonly called, where a package name would not do
    /// (a distribution other than Fedora's: `poppler`, not `poppler-utils`).
    pub common: &'static str,
    /// The Fedora packages, space-separated, for `dnf install`.
    pub fedora: &'static str,
    /// One more sentence for Fedora, or empty.
    pub fedora_note: &'static str,
    pub windows: Install,
    pub macos: Install,
    /// It is part of lmgw's own package (lmgw's reader): wherever a
    /// package would be named, [`Install::Bundled`] is.
    pub bundled: bool,
}

/// What a probe reads beside the version (design §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extra {
    /// Which of [`Probe::binaries`] exist.
    Binaries,
    /// `--list-langs`: the installed language packs.
    Languages,
    /// `-hide_banner -decoders`: the audio decoders of [`AUDIO_DECODERS`].
    AudioDecoders,
    /// LibreOffice's installed parts, by their launchers beside `soffice`
    /// ([`COMPONENTS`]).
    Components,
}

/// How a tool is found and asked for its version (design §4.3).
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    /// Looked up on `PATH`; the first is the one asked for its version and
    /// the one whose absence makes the tool missing.
    pub binaries: &'static [&'static str],
    /// The version run's arguments.
    pub version_args: &'static [&'static str],
    /// The word the version follows in its output (`pdftotext version
    /// 26.08.0`: `version`).
    pub version_after: &'static str,
    /// Where else to look on Windows and macOS when it is not on `PATH`.
    pub install_dirs: &'static [&'static str],
    pub extras: &'static [Extra],
}

/// One optional tool (design §4.1).
#[derive(Debug)]
pub struct Tool {
    pub id: ToolId,
    /// Its name in plain words.
    pub label: &'static str,
    pub packages: Packages,
    pub probe: Probe,
    pub licence: &'static str,
}

/// The audio decoders that matter for a video's sound (design §4.3, D13):
/// each name, and the ffmpeg decoders that count as it.
pub const AUDIO_DECODERS: &[(&str, &[&str])] = &[
    ("aac", &["aac", "aac_fixed", "libfdk_aac"]),
    ("mp3", &["mp3", "mp3float"]),
    ("opus", &["opus", "libopus"]),
    ("vorbis", &["vorbis", "libvorbis"]),
    ("flac", &["flac"]),
    ("ac3", &["ac3", "ac3_fixed"]),
    ("eac3", &["eac3"]),
];

/// LibreOffice's parts, by the launcher each part's package installs beside
/// `soffice` (checked on Fedora 45: `swriter` is in `libreoffice-writer`,
/// `scalc` in `-calc`, `simpress` in `-impress`; their main libraries are
/// split between those packages and `libreoffice-core`, so the launchers
/// are what tells them apart). On Windows they carry `.exe`.
pub const COMPONENTS: &[(&str, &str)] = &[
    ("Writer", "swriter"),
    ("Calc", "scalc"),
    ("Impress", "simpress"),
];

/// Why pandoc, ffmpeg and LibreOffice read nothing on Windows yet: the
/// runner starts no document tool there until it can bound its memory (a
/// job object, Windows design §4.4).
pub const TOOLS_NOT_RUN_ON_WINDOWS: &str = "not read on Windows yet: lmgw runs document tools \
     under a memory limit, which it cannot set on Windows yet";

/// The tools, in the page's order. pdfium (Windows and macOS PDFs, WP9) is
/// not in it yet.
pub const TOOLS: &[Tool] = &[
    Tool {
        id: ToolId::Reader,
        label: "lmgw's document reader",
        packages: Packages {
            common: "lmgw",
            fedora: "lmgw",
            fedora_note: "",
            windows: Install::NotYet(
                "not read on Windows yet: lmgw's reader runs under a memory limit, which the \
                 runner cannot set on Windows yet",
            ),
            macos: Install::Bundled,
            bundled: true,
        },
        probe: Probe {
            binaries: &[super::super::reader::READER_BIN],
            version_args: &["--version"],
            version_after: super::super::reader::READER_BIN,
            install_dirs: &[],
            extras: &[],
        },
        licence: "MIT",
    },
    Tool {
        id: ToolId::Poppler,
        label: "PDF tools (poppler)",
        packages: Packages {
            common: "poppler",
            fedora: "poppler-utils",
            fedora_note: "",
            windows: Install::NotYet(
                "PDF reading is not available on Windows yet; it comes through pdfium, which \
                 is not built yet",
            ),
            macos: Install::Command("brew install poppler"),
            bundled: false,
        },
        probe: Probe {
            binaries: &["pdftotext", "pdftoppm", "pdftohtml"],
            version_args: &["-v"],
            version_after: "version",
            install_dirs: &[],
            extras: &[Extra::Binaries],
        },
        licence: "GPL-2.0-only OR GPL-3.0-only",
    },
    Tool {
        id: ToolId::Tesseract,
        label: "Text recognition (tesseract)",
        packages: Packages {
            common: "tesseract",
            fedora: "tesseract",
            fedora_note: "Your language's pack is installed with it.",
            windows: Install::Link("https://github.com/UB-Mannheim/tesseract/wiki"),
            macos: Install::Command("brew install tesseract"),
            bundled: false,
        },
        probe: Probe {
            binaries: &["tesseract"],
            version_args: &["--version"],
            version_after: "tesseract",
            install_dirs: &["C:\\Program Files\\Tesseract-OCR"],
            extras: &[Extra::Languages],
        },
        licence: "Apache-2.0",
    },
    Tool {
        id: ToolId::Pandoc,
        label: "Pandoc",
        packages: Packages {
            common: "pandoc",
            fedora: "pandoc-cli",
            fedora_note: "",
            windows: Install::NotYet(TOOLS_NOT_RUN_ON_WINDOWS),
            macos: Install::Link("https://pandoc.org/installing.html"),
            bundled: false,
        },
        probe: Probe {
            binaries: &["pandoc"],
            version_args: &["--version"],
            version_after: "pandoc",
            // Pandoc's per-user and all-users Windows installers; the
            // `%VAR%` is expanded when looked up ([`super::super::probe::lookup`]).
            install_dirs: &["%LOCALAPPDATA%\\Pandoc", "C:\\Program Files\\Pandoc"],
            extras: &[],
        },
        licence: "GPL-2.0-or-later",
    },
    Tool {
        id: ToolId::Ffmpeg,
        label: "ffmpeg",
        packages: Packages {
            common: "ffmpeg",
            fedora: "ffmpeg-free",
            fedora_note: "Or the ffmpeg package from RPM Fusion.",
            windows: Install::NotYet(TOOLS_NOT_RUN_ON_WINDOWS),
            macos: Install::Command("brew install ffmpeg"),
            bundled: false,
        },
        probe: Probe {
            binaries: &["ffmpeg"],
            version_args: &["-hide_banner", "-version"],
            version_after: "version",
            install_dirs: &[],
            extras: &[Extra::AudioDecoders],
        },
        licence: "GPL-3.0-or-later (as Fedora's ffmpeg-free; other builds differ)",
    },
    Tool {
        id: ToolId::LibreOffice,
        label: "LibreOffice",
        packages: Packages {
            common: "LibreOffice",
            fedora: "libreoffice-writer libreoffice-impress",
            fedora_note: "",
            windows: Install::NotYet(TOOLS_NOT_RUN_ON_WINDOWS),
            macos: Install::Link("https://www.libreoffice.org/download/download-libreoffice/"),
            bundled: false,
        },
        probe: Probe {
            binaries: &["soffice"],
            version_args: &["--version"],
            version_after: "LibreOffice",
            install_dirs: &[
                "C:\\Program Files\\LibreOffice\\program",
                "/Applications/LibreOffice.app/Contents/MacOS",
            ],
            extras: &[Extra::Components],
        },
        licence: "MPL-2.0",
    },
];

/// The table's row for `id`.
pub fn tool(id: ToolId) -> &'static Tool {
    // Every ToolId has a row: `the_table_is_whole` proves it.
    TOOLS
        .iter()
        .find(|t| t.id == id)
        .expect("every ToolId has a TOOLS row")
}

/// The command or page that installs `packages` on `platform`, the server
/// image's being a derived image (design §6.2).
pub fn install_hint(packages: &Packages, platform: InstallPlatform) -> (Install, &'static str) {
    if packages.bundled && platform != InstallPlatform::Windows {
        return (Install::Bundled, "");
    }
    match platform {
        InstallPlatform::Fedora | InstallPlatform::ServerImage => {
            (Install::Command(packages.fedora), packages.fedora_note)
        }
        InstallPlatform::OtherLinux => (Install::Manual(packages.common), ""),
        InstallPlatform::Windows => (packages.windows, ""),
        InstallPlatform::Macos => (packages.macos, ""),
    }
}
