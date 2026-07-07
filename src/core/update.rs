//! Notify-only update check.
//!
//! On GUI startup (unless `config.check_for_updates` is off) we make one GET to
//! the GitHub releases API, compare the latest published version against the
//! running binary, and — if it's newer — stash an [`UpdateStatus`] global that
//! Settings → About reads to show a "download" prompt linking to the Releases
//! page. That's the whole feature: we never download, replace, or restart
//! anything. The user updates by hand (drag the new `.app`, unzip, …), exactly
//! as the README's Install section describes.
//!
//! Everything here fails soft: no network, a rate-limit, a private/renamed repo,
//! an unparseable tag — all collapse to "no prompt", logged at `debug` and never
//! surfaced. A terminal must open the same whether or not GitHub is reachable.

use anyhow::{Context as _, Result};
use gpui::http_client::{AsyncBody, HttpClient as _, HttpRequestExt as _, RedirectPolicy};
use gpui::{App, Global, http_client};
use reqwest_client::ReqwestClient;
use smol::io::AsyncReadExt as _;

use crate::core::config::Config;

/// `owner/repo` the release check queries — matches the repository the binary is
/// published from (see `Cargo.toml`'s `repository`).
const REPO: &str = "l0ng-ai/tty7";

/// Where the "Download" prompt points. GitHub's `/releases/latest` alias always
/// resolves to the newest published (non-prerelease) build, so it never goes
/// stale as versions roll — no need to embed a specific tag.
pub const RELEASES_URL: &str = "https://github.com/l0ng-ai/tty7/releases/latest";

/// A newer release than the one currently running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    /// The newer version, normalized without a leading `v` (e.g. `"0.3.1"`), for
    /// display in the About panel.
    pub version: String,
}

/// The result of the startup update check, stored as a GPUI global so the
/// Settings view can read it. Absent until the check completes; `available` is
/// `None` when we're already current (or the check failed / was skipped).
#[derive(Clone, Debug, Default)]
pub struct UpdateStatus {
    pub available: Option<AvailableUpdate>,
}

impl Global for UpdateStatus {}

/// Kick off the background update check. Returns immediately; the network work
/// runs on a detached task and, if a newer version exists, writes the
/// [`UpdateStatus`] global and repaints so an already-open About panel updates.
///
/// Honors `config.check_for_updates`: when off, we make no network call at all.
pub fn spawn_check(cx: &mut App) {
    if !cx.global::<Config>().check_for_updates {
        return;
    }

    cx.spawn(async move |cx| {
        let current = env!("CARGO_PKG_VERSION");
        let latest = match fetch_latest_version().await {
            Ok(v) => v,
            Err(e) => {
                // `{e:#}` includes the anyhow context chain; kept at debug so a
                // routine offline start doesn't spam the log.
                log::debug!("update check skipped: {e:#}");
                return;
            }
        };

        if !is_update_available(&latest, current) {
            log::debug!("update check: up to date (latest {latest}, running {current})");
            return;
        }

        let version = latest.trim_start_matches('v').to_string();
        log::info!("update available: {version} (running {current})");
        let _ = cx.update(|cx| {
            cx.set_global(UpdateStatus {
                available: Some(AvailableUpdate { version }),
            });
            // Repaint so a Settings → About that's already open shows the prompt
            // now rather than only on the next interaction.
            cx.refresh_windows();
        });
    })
    .detach();
}

/// The `tag_name` field of GitHub's release payload — the only piece we read.
#[derive(serde::Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// GET the repo's latest release and return its raw tag (e.g. `"v0.3.1"`).
async fn fetch_latest_version() -> Result<String> {
    // GitHub rejects requests without a User-Agent; identify ourselves. The
    // reqwest+rustls stack this rides on is already compiled into the app via
    // `gpui-component-assets`, so constructing a client here is cheap.
    let client = ReqwestClient::user_agent(concat!("tty7/", env!("CARGO_PKG_VERSION")))
        .context("building HTTP client")?;

    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let request = http_client::Request::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::default())
        .context("building request")?;

    let mut response = client
        .send(request)
        .await
        .context("requesting latest release")?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub API returned HTTP {}", response.status().as_u16());
    }

    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .context("reading response body")?;

    let release: LatestRelease =
        serde_json::from_slice(&body).context("parsing release JSON")?;
    Ok(release.tag_name)
}

/// Parse a version string into a `(major, minor, patch)` triple, tolerating a
/// leading `v` and ignoring any pre-release / build suffix (`-rc.1`, `+build`).
/// Missing minor/patch components read as `0`. Returns `None` if the numeric
/// core doesn't parse — the caller treats that as "don't prompt".
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    // A pre-release/build tag (`0.4.0-rc.1`, `0.4.0+ci`) compares by its release
    // core here; we don't ship pre-releases, so finer ordering isn't worth it.
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `latest` names a strictly newer version than `current`. If either
/// side fails to parse we return `false`: an unrecognizable tag should never
/// nag the user to "update" to something we can't even order.
fn is_update_available(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_with_and_without_prefix() {
        assert_eq!(parse_version("v0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version("0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version(" 1.2.0 "), Some((1, 2, 0)));
        // Missing components default to zero.
        assert_eq!(parse_version("v2"), Some((2, 0, 0)));
        assert_eq!(parse_version("v2.5"), Some((2, 5, 0)));
        // Pre-release / build metadata is ignored down to the release core.
        assert_eq!(parse_version("v0.4.0-rc.1"), Some((0, 4, 0)));
        assert_eq!(parse_version("0.4.0+ci.7"), Some((0, 4, 0)));
        // Garbage yields None.
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn detects_newer_versions() {
        assert!(is_update_available("v0.3.1", "0.3.0"));
        assert!(is_update_available("v1.0.0", "0.9.9"));
        assert!(is_update_available("0.4.0", "0.3.99"));
    }

    #[test]
    fn ignores_same_or_older_versions() {
        assert!(!is_update_available("v0.3.0", "0.3.0"));
        assert!(!is_update_available("v0.2.9", "0.3.0"));
        assert!(!is_update_available("0.3.0", "0.3.1"));
    }

    #[test]
    fn unparseable_tag_never_prompts() {
        assert!(!is_update_available("garbage", "0.3.0"));
        assert!(!is_update_available("v0.3.1", "garbage"));
    }
}
