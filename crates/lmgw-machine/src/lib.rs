//! What lmgw reads about the computer it runs on (machine-check design
//! `docs/design/2026-11-03-machine-check-design.md`, WP1).
//!
//! The machine check asks whether this computer can run local models and
//! what is missing; this crate holds the facts it reads and the judgements
//! that do not need lmgw's state. lmgw-core's check reads them with the
//! user's rights; the root helper (`lmgw-machine-helper`, §3.1, WP6) reads
//! the same facts through the same code, so what the person is shown and
//! what runs as root are computed once.
//!
//! - [`facts`]: the one seam ([`Facts`]) every read goes through — files,
//!   links, listings, the environment, the programs run — with the real
//!   machine ([`SystemFacts`]) and, with the `fake` feature, a stated one for
//!   tests (`fake::FakeFacts`);
//! - [`pci`]: the graphics cards on the PCI bus and their drivers;
//! - [`driver`]: the NVIDIA kernel module's and libraries' versions, and
//!   Secure Boot;
//! - [`rpm`]: package owners, header signatures, imported keys, and when the
//!   rpm database last changed;
//! - [`repos`]: dnf's repository sections after dnf5's overrides, and its
//!   metadata cache times;
//! - [`os`]: os-release and image-based systems;
//! - [`cdi`]: CDI spec files, the judgement of lmgw's own spec, and Podman's
//!   resolution of the system's spec dirs;
//! - [`cpu`]: the processor's name, logical processors and instruction
//!   sets (`cpuid`), what the Windows check reports;
//! - [`display`]: the display adapters from the display class's driver keys
//!   in the registry (read on Windows), Windows' counterpart of [`pci`];
//! - [`plan`]: the closed list of root steps that installs the container
//!   toolkit (WP2), its hash, and the instructions for systems without the
//!   button;
//! - [`helper`]: what the root helper and the app say to each other — its
//!   verbs' exit codes, the JSON lines it prints, the system task it starts.
//!
//! **Read-only.** Nothing here writes a file. The one program run with an
//! effect, `nvidia-ctk cdi generate`, is lmgw-core's, into lmgw's own
//! directory; the plan's steps are run by the root helper (WP6), never here.

pub mod cdi;
pub mod cpu;
pub mod display;
pub mod driver;
pub mod facts;
#[cfg(feature = "fake")]
pub mod fake;
pub mod helper;
pub mod os;
pub mod pci;
pub mod plan;
pub mod repos;
pub mod rpm;

pub use facts::{CmdOut, Facts, Kind, Meta, Posix, Space, SystemFacts};
