//! Monochrome symbolic icons (docs/design/ui-rebuild/modern-sample.html):
//! 24-unit strokes in the current text colour, drawn at 16 px by `svg.i`
//! (kit.css). One set for the shell, the header bars and the kit's own
//! controls, so an arrow, a close or a menu looks the same everywhere.

use leptos::prelude::*;

/// The icons the kit draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ico {
    /// A window with its sidebar: show or hide the sidebar.
    Panel,
    /// Three lines: a main menu.
    Menu,
    Search,
    Plus,
    Minus,
    /// The close cross, also "clear".
    Close,
    /// A window's minimize.
    WinMin,
    /// A window's maximize (and restore).
    WinMax,
    /// A chevron down: a dropdown's arrow.
    Down,
    /// A chevron right: a row that leads on.
    Right,
    /// Three dots: a row's or page's further actions.
    More,
    Check,
    Play,
    Stop,
    Download,
    /// A sun: the light theme.
    Sun,
    /// A moon: the dark theme.
    Moon,
    /// A chevron left: back to the list a page came from.
    Back,
    /// A speech bubble: chat models.
    Chat,
    /// A fork: embeddings, rerank and decisions.
    Split,
    /// Bars of a waveform: audio models.
    Wave,
    /// A picture: image models.
    Image,
    /// A cloud: models served by an upstream.
    Cloud,
    /// Crossing arrows: a candidate alias.
    Shuffle,
    /// Stacked sheets: a catalog.
    Layers,
    /// A globe: the network.
    Globe,
    /// A clock face: how long things are kept.
    Clock,
    /// Two coins: usage and cost.
    Coins,
    /// A chip with its pins: the graphics card.
    Chip,
    /// A box: containers.
    Box,
    /// A wrench: engines and builds.
    Wrench,
    /// A microphone: spoken conversations.
    Mic,
    /// A robot's head: agents.
    Bot,
    /// A book: docs.
    Book,
    /// A key: credentials.
    Key,
    /// A painter's palette: appearance.
    Palette,
    /// An i in a circle: about.
    Info,
    /// A pencil on a sheet: write something new (a new chat).
    Compose,
    /// A paperclip: attach files.
    Attach,
    /// An arrow up: send.
    Send,
    /// Three sliders: settings of the thing in view.
    Sliders,
    /// A list of lines: a list to show or hide.
    List,
    /// A circling arrow: read again.
    Refresh,
    /// A bookmark: saved presets.
    Bookmark,
    /// A bin: delete.
    Trash,
    /// An arrow out of a tray: upload a file.
    Upload,
    /// A sheet with a folded corner and lines: documents.
    File,
    /// Two overlapping sheets: copy to the clipboard.
    Copy,
    /// Two vertical bars: pause, hold.
    Pause,
    /// Two stacked rounded bars: the Models page.
    Models,
    /// A wand with a four-point spark: fill in for me (values lmgw worked out).
    Wand,
    /// A plug: MCP servers attached.
    Plug,
    /// Three waves: a streamed answer.
    Stream,
    /// A chevron up: move up.
    Up,
}

impl Ico {
    /// Every icon, in the order of the enum: the component sampler shows them
    /// all.
    pub const ALL: &'static [Ico] = &[
        Ico::Panel,
        Ico::Menu,
        Ico::Search,
        Ico::Plus,
        Ico::Minus,
        Ico::Close,
        Ico::WinMin,
        Ico::WinMax,
        Ico::Down,
        Ico::Right,
        Ico::More,
        Ico::Check,
        Ico::Play,
        Ico::Stop,
        Ico::Download,
        Ico::Sun,
        Ico::Moon,
        Ico::Back,
        Ico::Chat,
        Ico::Split,
        Ico::Wave,
        Ico::Image,
        Ico::Cloud,
        Ico::Shuffle,
        Ico::Layers,
        Ico::Globe,
        Ico::Clock,
        Ico::Coins,
        Ico::Chip,
        Ico::Box,
        Ico::Wrench,
        Ico::Mic,
        Ico::Bot,
        Ico::Book,
        Ico::Key,
        Ico::Palette,
        Ico::Info,
        Ico::Compose,
        Ico::Attach,
        Ico::Send,
        Ico::Sliders,
        Ico::List,
        Ico::Refresh,
        Ico::Bookmark,
        Ico::Trash,
        Ico::Upload,
        Ico::File,
        Ico::Copy,
        Ico::Pause,
        Ico::Models,
        Ico::Wand,
        Ico::Plug,
        Ico::Stream,
        Ico::Up,
    ];

    /// The SVG body in a 24×24 box.
    fn body(self) -> &'static str {
        match self {
            Self::Panel => {
                r#"<rect x="3" y="4" width="18" height="16" rx="2.5"/><path d="M9.5 4v16"/>"#
            }
            Self::Menu => r#"<path d="M4 7h16M4 12h16M4 17h16"/>"#,
            Self::Search => r#"<circle cx="11" cy="11" r="7"/><path d="m20.5 20.5-4.5-4.5"/>"#,
            Self::Plus => r#"<path d="M5 12h14M12 5v14"/>"#,
            Self::Minus => r#"<path d="M6 12h12"/>"#,
            Self::Close => r#"<path d="M7 7l10 10M17 7 7 17"/>"#,
            Self::WinMin => r#"<path d="M6 12.5h12"/>"#,
            Self::WinMax => r#"<rect x="6.5" y="6.5" width="11" height="11" rx="1.5"/>"#,
            Self::Down => r#"<path d="m6 9 6 6 6-6"/>"#,
            Self::Right => r#"<path d="m9 6 6 6-6 6"/>"#,
            Self::More => {
                r#"<circle cx="12" cy="5.5" r=".9"/><circle cx="12" cy="12" r=".9"/><circle cx="12" cy="18.5" r=".9"/>"#
            }
            Self::Check => r#"<path d="m5 12.5 4.5 4.5L19 7.5"/>"#,
            Self::Play => r#"<path d="M7 5.5v13l11-6.5z"/>"#,
            Self::Stop => r#"<rect x="6.5" y="6.5" width="11" height="11" rx="1.5"/>"#,
            Self::Download => r#"<path d="M12 4v11M7 10.5l5 5 5-5M4.5 19.5h15"/>"#,
            Self::Sun => {
                r#"<circle cx="12" cy="12" r="4"/><path d="M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.3 5.3l1.4 1.4M17.3 17.3l1.4 1.4M18.7 5.3l-1.4 1.4M6.7 17.3l-1.4 1.4"/>"#
            }
            Self::Moon => r#"<path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5z"/>"#,
            Self::Back => r#"<path d="m15 6-6 6 6 6"/>"#,
            Self::Chat => {
                r#"<path d="M20 14.5a2 2 0 0 1-2 2H8l-4 3.5V6a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2z"/>"#
            }
            Self::Split => r#"<path d="M12 21v-7l-6-6M12 14l6-6M4 4h4v4M20 4h-4v4"/>"#,
            Self::Wave => r#"<path d="M3 12h1.5M7 8v8M10.5 5v14M14 9v6M17.5 7v10M21 12h-1"/>"#,
            Self::Image => {
                r#"<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><circle cx="9" cy="10" r="1.6"/><path d="m4 17 5-4.5 4 3.5 3-2.5 4 3.5"/>"#
            }
            Self::Cloud => {
                r#"<path d="M7 18.5a4.5 4.5 0 0 1-.6-9 6 6 0 0 1 11.4 1.6A3.8 3.8 0 0 1 17.5 18.5z"/>"#
            }
            Self::Shuffle => {
                r#"<path d="M3 7h4l10 10h4M3 17h4l3-3M14 10l3-3h4M18.5 4.5 21 7l-2.5 2.5M18.5 14.5 21 17l-2.5 2.5"/>"#
            }
            Self::Layers => {
                r#"<path d="m12 3 9 4.5-9 4.5-9-4.5z"/><path d="m3 12 9 4.5 9-4.5M3 16.5 12 21l9-4.5"/>"#
            }
            Self::Globe => {
                r#"<circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17M12 3.5c2.4 2.6 3.6 5.4 3.6 8.5s-1.2 5.9-3.6 8.5c-2.4-2.6-3.6-5.4-3.6-8.5s1.2-5.9 3.6-8.5z"/>"#
            }
            Self::Clock => r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>"#,
            Self::Coins => {
                r#"<ellipse cx="9" cy="7" rx="5.5" ry="2.5"/><path d="M3.5 7v4c0 1.4 2.5 2.5 5.5 2.5s5.5-1.1 5.5-2.5V7"/><path d="M9.5 16.3c1 1.3 3.2 2.2 5.5 2.2 3 0 5.5-1.1 5.5-2.5v-4c0-1.2-1.8-2.2-4.2-2.4"/>"#
            }
            Self::Chip => {
                r#"<rect x="6" y="6" width="12" height="12" rx="2"/><rect x="9.5" y="9.5" width="5" height="5" rx="1"/><path d="M9.5 3v3M14.5 3v3M9.5 18v3M14.5 18v3M3 9.5h3M3 14.5h3M18 9.5h3M18 14.5h3"/>"#
            }
            Self::Box => {
                r#"<path d="m12 3 8 4.5v9L12 21l-8-4.5v-9z"/><path d="m4 7.5 8 4.5 8-4.5M12 12v9"/>"#
            }
            Self::Wrench => {
                r#"<path d="M19.6 7.2a4.5 4.5 0 1 1-2.8-2.8L14.5 6.7l.6 2.2 2.2.6z"/><path d="m12.4 11.6-7.4 7.4"/>"#
            }
            Self::Mic => {
                r#"<rect x="9" y="3.5" width="6" height="11" rx="3"/><path d="M5.5 11.5a6.5 6.5 0 0 0 13 0M12 18v2.5"/>"#
            }
            Self::Bot => {
                r#"<rect x="4.5" y="8" width="15" height="11" rx="3"/><path d="M12 8V5M9.5 12.5v1.5M14.5 12.5v1.5M2.5 12.5v2.5M21.5 12.5v2.5"/><circle cx="12" cy="4" r="1"/>"#
            }
            Self::Book => {
                r#"<path d="M5 18.5V5.5a2 2 0 0 1 2-2h12v13H7a2 2 0 0 0-2 2 2 2 0 0 0 2 2h12v-4"/>"#
            }
            Self::Key => {
                r#"<circle cx="8" cy="15" r="4"/><path d="m11 12 8.5-8.5M16 7l2.5 2.5M13.5 9.5l2 2"/>"#
            }
            Self::Palette => {
                r#"<path d="M12 3.5a8.5 8.5 0 1 0 0 17c1.1 0 1.8-.8 1.8-1.8 0-1.2-1-1.6-1-2.7 0-1 .8-1.7 1.8-1.7H17a3.5 3.5 0 0 0 3.5-3.5c0-4.1-3.8-7.3-8.5-7.3z"/><circle cx="7.8" cy="11.5" r="1"/><circle cx="10.2" cy="7.6" r="1"/><circle cx="14.8" cy="7.8" r="1"/>"#
            }
            Self::Info => r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 11v5.5M12 7.6v.4"/>"#,
            Self::Compose => {
                r#"<path d="M11 4.5H6.5a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2V13"/><path d="M17.6 3.9a1.9 1.9 0 0 1 2.7 2.7L12.5 14.4 9 15l.6-3.5z"/>"#
            }
            Self::Attach => {
                r#"<path d="m20 11.5-7.7 7.7a5 5 0 0 1-7.1-7.1l8.1-8.1a3.3 3.3 0 0 1 4.7 4.7l-8.1 8.1a1.7 1.7 0 0 1-2.4-2.4l7.4-7.4"/>"#
            }
            Self::Send => r#"<path d="M12 19.5v-15M6 10.5l6-6 6 6"/>"#,
            Self::Sliders => {
                r#"<path d="M4 7h9M17 7h3M4 17h3M11 17h9"/><circle cx="15" cy="7" r="2"/><circle cx="9" cy="17" r="2"/>"#
            }
            Self::List => r#"<path d="M4 7h16M4 12h16M4 17h10"/>"#,
            Self::Refresh => r#"<path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3M19.5 4.5v4h-4"/>"#,
            Self::Bookmark => r#"<path d="M7 4.5h10v15l-5-3.5-5 3.5z"/>"#,
            Self::Trash => {
                r#"<path d="M4.5 7h15M9.5 7V4.5h5V7M6.5 7l1 12.5h9l1-12.5M10 10.5v6M14 10.5v6"/>"#
            }
            Self::Upload => r#"<path d="M12 15.5V4.5M7 9l5-5 5 5M4.5 19.5h15"/>"#,
            Self::File => {
                r#"<path d="M13.5 3.5H7A1.5 1.5 0 0 0 5.5 5v14A1.5 1.5 0 0 0 7 20.5h10a1.5 1.5 0 0 0 1.5-1.5V8.5z"/><path d="M13.5 3.5v5h5M9 12.5h6M9 16h6"/>"#
            }
            Self::Copy => {
                r#"<rect x="8.5" y="8.5" width="11" height="11" rx="2"/><path d="M15.5 8.5V6A1.5 1.5 0 0 0 14 4.5H6A1.5 1.5 0 0 0 4.5 6v8A1.5 1.5 0 0 0 6 15.5h2.5"/>"#
            }
            Self::Pause => r#"<path d="M9 6v12M15 6v12"/>"#,
            Self::Models => {
                r#"<rect x="3.75" y="3.75" width="16.5" height="6.75" rx="2.25"/><rect x="3.75" y="13.5" width="16.5" height="6.75" rx="2.25"/>"#
            }
            Self::Wand => {
                r#"<path d="m4 20 8.5-8.5"/><path d="M17.5 2.5c.4 2.4 1.5 3.5 4 4-2.5.5-3.6 1.6-4 4-.4-2.4-1.5-3.5-4-4 2.5-.5 3.6-1.6 4-4z" fill="currentColor"/>"#
            }
            Self::Plug => {
                r#"<path d="M9 3.5v4M15 3.5v4M6.5 7.5h11v3a5.5 5.5 0 0 1-11 0zM12 16v4.5"/>"#
            }
            Self::Stream => {
                r#"<path d="M4 8c2.5-2 5.5 2 8 0s5.5-2 8 0M4 12c2.5-2 5.5 2 8 0s5.5-2 8 0M4 16c2.5-2 5.5 2 8 0s5.5-2 8 0"/>"#
            }
            Self::Up => r#"<path d="m6 15 6-6 6 6"/>"#,
        }
    }
}

/// `<svg class="i">` with the icon's strokes; `class` adds to it.
#[component]
pub fn Icon(ico: Ico, #[prop(optional)] class: &'static str) -> impl IntoView {
    let class = if class.is_empty() {
        "i".to_string()
    } else {
        format!("i {class}")
    };
    view! { <svg class=class viewBox="0 0 24 24" aria-hidden="true" inner_html=ico.body()></svg> }
}
