//! The profile each LibreOffice run starts with (document-conversion design
//! §5.1): a new, private one per run, seeded with a
//! `registrymodifications.xcu` that turns off what a document could make
//! LibreOffice do beyond reading it. LibreOffice fills in the rest of the
//! profile itself at start (about 0.16 s of a run, measured 2026-11-19 with
//! LibreOffice 26.8.2.2: 0.45 s a `.doc` with a new profile against 0.29 s
//! with one reused).
//!
//! No profile is shared between runs or reused after a run: a profile
//! a run has written is the run's, and a document could leave something in
//! it (a recovery entry, an extension's cache). Each run starts from a
//! copy of a template made by a run without a document (`scratch.rs`).
//!
//! | setting | value | what it prevents |
//! |---|---|---|
//! | `Common/Security/Scripting/DisableMacrosExecution` | true | Basic, Beanshell, JavaScript and Python macros, whatever the level below says |
//! | `Common/Security/Scripting/MacroSecurityLevel` | 3 (very high) | macros from anywhere but a trusted place, of which the profile names none |
//! | `Common/Security/Scripting/DisableActiveContent` | true | embedded OLE objects run as active content, and DDE links |
//! | `Common/Security/Scripting/BlockUntrustedRefererLinks` | true | links followed from a document outside a trusted place |
//! | `Common/Security/Scripting/DisablePythonRuntime` | true | the Python runtime, which macros and extensions would use |
//! | `Writer/Content/Update/Link` | 0 (never) | a Word document's links (to files, to other documents) updated when it is loaded |
//! | `Writer/Content/Update/Field` | false | fields updated when it is loaded |
//! | `Calc/Content/Update/Link` | 1 (never) | a spreadsheet's external links updated when it is loaded |
//!
//! | `Writer/Filter/Import/VBA/Load`, `Impress/…`, `Calc/…` | false | a document's VBA project loaded at all (not even as Basic) |
//!
//! These settings are not the boundary: a document can still make
//! LibreOffice read a file it names (an RTF's `INCLUDEPICTURE` is followed
//! whatever they say). The boundary is the kernel's confinement each run
//! has (`confine.rs`): only its own directory, `/usr` and a few system
//! files are visible to it, and it has no TCP.

/// The seeded `registrymodifications.xcu` (module doc).
pub const REGISTRY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<oor:items xmlns:oor="http://openoffice.org/2001/registry" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
<item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="DisableMacrosExecution" oor:op="fuse"><value>true</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="MacroSecurityLevel" oor:op="fuse"><value>3</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="DisableActiveContent" oor:op="fuse"><value>true</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="BlockUntrustedRefererLinks" oor:op="fuse"><value>true</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="DisablePythonRuntime" oor:op="fuse"><value>true</value></prop></item>
<item oor:path="/org.openoffice.Office.Writer/Content/Update"><prop oor:name="Link" oor:op="fuse"><value>0</value></prop></item>
<item oor:path="/org.openoffice.Office.Writer/Content/Update"><prop oor:name="Field" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Calc/Content/Update"><prop oor:name="Link" oor:op="fuse"><value>1</value></prop></item>
<item oor:path="/org.openoffice.Office.Writer/Filter/Import/VBA"><prop oor:name="Load" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Impress/Filter/Import/VBA"><prop oor:name="Load" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Calc/Filter/Import/VBA"><prop oor:name="Load" oor:op="fuse"><value>false</value></prop></item>
</oor:items>
"#;

/// Where it goes in a profile directory.
pub const REGISTRY_PATH: &str = "user/registrymodifications.xcu";
