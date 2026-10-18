//! The page shell every UI page renders into: left navigation, a header with
//! the daemon's identity and the signed-in principal, and a footer with the
//! build identity. Also the shared empty state and error fragment.

use std::time::Duration;

use maud::{DOCTYPE, Markup, PreEscaped, html};

use super::{DaemonIdentity, Principal, assets};

/// Per-request inputs of the shell.
pub(crate) struct ShellContext<'a> {
    pub(crate) principal: &'a Principal,
    pub(crate) identity: &'a DaemonIdentity,
    /// Entries in the configured surface table.
    pub(crate) surfaces: usize,
}

/// A navigation section of the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Overview,
    Threads,
    Bros,
    Gaps,
    Projects,
    System,
    Configuration,
}

impl Section {
    const ALL: [Section; 7] = [
        Section::Overview,
        Section::Threads,
        Section::Bros,
        Section::Gaps,
        Section::Projects,
        Section::System,
        Section::Configuration,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Section::Overview => "Overview",
            Section::Threads => "Threads",
            Section::Bros => "Bros",
            Section::Gaps => "Gaps",
            Section::Projects => "Projects",
            Section::System => "System",
            Section::Configuration => "Configuration",
        }
    }

    pub(crate) fn href(self) -> &'static str {
        match self {
            Section::Overview => "/",
            Section::Threads => "/threads",
            Section::Bros => "/bros",
            Section::Gaps => "/gaps",
            Section::Projects => "/projects",
            Section::System => "/system",
            Section::Configuration => "/config",
        }
    }

    /// Inline SVG path data for the navigation icon (24x24 viewBox, stroked).
    fn icon(self) -> &'static str {
        match self {
            Section::Overview => {
                r#"<rect x="3" y="3" width="7" height="7"/><rect x="14" y="3" width="7" height="7"/><rect x="3" y="14" width="7" height="7"/><rect x="14" y="14" width="7" height="7"/>"#
            }
            Section::Threads => r#"<path d="M4 5h16v11H9l-5 4z"/>"#,
            Section::Bros => {
                r#"<circle cx="9" cy="8" r="3"/><circle cx="17" cy="9" r="2.5"/><path d="M3 20c0-3.5 2.7-6 6-6s6 2.5 6 6"/><path d="M15 14.5c3 0 6 2 6 5.5"/>"#
            }
            Section::Gaps => r#"<path d="M12 3l9 17H3z"/><path d="M12 10v4M12 17v.5"/>"#,
            Section::Projects => r#"<path d="M3 6h7l2 2h9v11H3z"/>"#,
            Section::System => {
                r#"<rect x="3" y="4" width="18" height="12" rx="1"/><path d="M8 20h8M12 16v4"/>"#
            }
            Section::Configuration => {
                r#"<path d="M4 6h10M18 6h2M4 12h4M12 12h8M4 18h12"/><circle cx="16" cy="6" r="2"/><circle cx="10" cy="12" r="2"/><circle cx="18" cy="18" r="2"/>"#
            }
        }
    }
}

/// Render a full page: the shell around `content`.
pub(crate) fn layout(
    ctx: &ShellContext<'_>,
    active: Option<Section>,
    title: &str,
    content: Markup,
) -> Markup {
    let identity = ctx.identity;
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="htmx-config" content="{\"includeIndicatorStyles\":false}";
                title { (title) " | blackbox " (identity.hostname) }
                link rel="stylesheet" href=(assets::url("style.css"));
                script src=(assets::url("htmx.min.js")) defer {}
                script src=(assets::url("htmx-sse.min.js")) defer {}
            }
            body {
                nav.shell-nav aria-label="Sections" {
                    a.shell-brand href="/" { "blackbox" }
                    ul {
                        @for section in Section::ALL {
                            li {
                                a href=(section.href())
                                    aria-current=[(active == Some(section)).then_some("page")] {
                                    svg viewBox="0 0 24 24" aria-hidden="true" {
                                        (PreEscaped(section.icon()))
                                    }
                                    span { (section.label()) }
                                }
                            }
                        }
                    }
                }
                div.shell-main {
                    header.shell-header {
                        div.shell-identity {
                            span { "host " strong { (identity.hostname) } }
                            span { "version " strong { (identity.version) } }
                            span { "uptime " strong { (format_uptime(identity.started_at.elapsed())) } }
                            span { "surfaces " strong.num { (ctx.surfaces) } }
                        }
                        div.shell-principal {
                            "signed in as " strong { (ctx.principal.0) }
                        }
                    }
                    main.shell-content {
                        h1 { (title) }
                        (content)
                    }
                    footer.shell-footer {
                        "blackboxd " (identity.version) " build " code { (identity.build_id) }
                    }
                }
            }
        }
    }
}

/// What an empty region shows: what is empty and where it comes from.
pub(crate) fn empty_state(what: &str, source: &str) -> Markup {
    html! {
        p.empty-state { (what) " (source: " code { (source) } ")" }
    }
}

/// A failed reader's region: names the reader and keeps the rest of the
/// page alive.
pub(crate) fn error_fragment(reader: &str, error: &str) -> Markup {
    html! {
        div.error-fragment role="alert" {
            p { "The " code { (reader) } " reader failed." }
            p { code { (error) } }
        }
    }
}

/// Uptime as its two most significant units.
pub(crate) fn format_uptime(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let (days, hours, minutes, seconds) = (
        secs / 86_400,
        secs % 86_400 / 3_600,
        secs % 3_600 / 60,
        secs % 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_shows_the_two_most_significant_units() {
        assert_eq!(format_uptime(Duration::from_secs(0)), "0s");
        assert_eq!(format_uptime(Duration::from_secs(59)), "59s");
        assert_eq!(format_uptime(Duration::from_secs(61)), "1m 1s");
        assert_eq!(format_uptime(Duration::from_secs(3_660)), "1h 1m");
        assert_eq!(format_uptime(Duration::from_secs(90_061)), "1d 1h");
    }

    #[test]
    fn fragments_escape_their_inputs() {
        let empty = empty_state("No <threads>", "bbox_thread_list").into_string();
        assert!(empty.contains("No &lt;threads&gt;"));
        let error = error_fragment("bbox_stats", "<script>").into_string();
        assert!(error.contains("&lt;script&gt;"));
        assert!(error.contains("bbox_stats"));
    }
}
