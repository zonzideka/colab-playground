//! Update check: is a newer release of this app published on GitHub?
//!
//! One GET to the GitHub API for the latest release of this repository (the generic
//! `colab-playground/<version>` User-Agent of [`crate::http`] is all it sends). Releases are
//! tagged `v<version>`; versions compare by semantic-version rules (a pre-release is older than
//! its release, build metadata is ignored) and only a strictly newer one is an update. The result
//! carries the release page (always under this repository's releases) and the release's assets;
//! [`crate::self_update`] downloads and installs from those assets, and the page stays as the
//! fallback for platforms without an installable asset.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::{CancelToken, Error, ErrorKind, Result, http::HttpClient};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The repository URL from the package metadata (the one home of that fact).
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

pub const API_BASE: &str = "https://api.github.com";

/// `(owner, repo)` of a `https://github.com/<owner>/<repo>` URL.
pub fn repo_slug(repository: &str) -> Option<(String, String)> {
    let rest = repository
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (parts.next().is_none() && ok(owner) && ok(repo)).then(|| (owner.to_owned(), repo.to_owned()))
}

pub fn latest_release_api(owner: &str, repo: &str) -> String {
    format!("{API_BASE}/repos/{owner}/{repo}/releases/latest")
}

/// The releases page every opened URL must sit under.
pub fn releases_prefix(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}/releases/")
}

/// The page of the latest release (the fallback page).
pub fn latest_release_page(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}/releases/latest")
}

/// The page of release `version` (tag `v<version>`).
pub fn release_page(owner: &str, repo: &str, version: &str) -> String {
    format!("https://github.com/{owner}/{repo}/releases/tag/v{version}")
}

/// `url` if it is a page under this repository's releases, else the latest-release page. A URL
/// from a reply is never opened unless it passes this check.
pub fn safe_release_page(url: &str, owner: &str, repo: &str) -> String {
    let prefix = releases_prefix(owner, repo);
    let tail = url.strip_prefix(&prefix);
    let ok = tail.is_some_and(|tail| {
        !tail.is_empty()
            && !tail.contains("..")
            && tail
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+'))
    });
    if ok {
        url.to_owned()
    } else {
        latest_release_page(owner, repo)
    }
}

/// A semantic version: `major.minor.patch[-pre][+build]` (build metadata is dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release identifiers (empty for a release).
    pub pre: Vec<String>,
}

impl Version {
    /// Parses `1.2.3`, `v1.2.3`, `1.2.3-rc.1`, `1.2.3+build.5`. `None` for anything else.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut numbers = core.split('.');
        let mut number = || -> Option<u64> {
            let part = numbers.next()?;
            if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            part.parse().ok()
        };
        let (major, minor, patch) = (number()?, number()?, number()?);
        if numbers.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => {
                let ids: Vec<String> = pre.split('.').map(str::to_owned).collect();
                let valid = ids.iter().all(|id| {
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                });
                if !valid {
                    return None;
                }
                ids
            }
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => compare_pre(&self.pre, &other.pre),
            })
    }
}

fn compare_pre(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let order = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    a.len().cmp(&b.len())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// The file name (`colab-playground-…-….exe`, `SHA256SUMS`, …).
    pub name: String,
    /// Its `browser_download_url`, already checked to sit under this release.
    pub url: String,
    pub size: u64,
}

/// The name of the release's checksum asset.
pub const SUMS_ASSET_NAME: &str = "SHA256SUMS";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Its version (without the `v`).
    pub version: String,
    /// Its tag as published.
    pub tag: String,
    /// The page to open (always under this repository's releases).
    pub page: String,
    /// Its downloadable assets (only the ones that passed the URL and name checks).
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    /// The asset named exactly `name`, if the release carries one.
    pub fn asset_named(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// The platform asset plus `SHA256SUMS`, when both are present. Only then can the app
    /// download, verify and install this release by itself.
    pub fn installable_asset(&self) -> Option<&Asset> {
        let asset = self.asset_named(&platform_asset_name(&self.version)?)?;
        self.asset_named(SUMS_ASSET_NAME)?;
        Some(asset)
    }
}

/// The download asset name for this build's platform and architecture, `None` where the
/// installer does not support it (those platforms keep opening the release page instead).
pub fn platform_asset_name(version: &str) -> Option<String> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some(format!(
            "colab-playground-{version}-aarch64-apple-darwin.zip"
        ));
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Some(format!(
            "colab-playground-{version}-x86_64-pc-windows-msvc.exe"
        ));
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some(format!(
            "colab-playground-{version}-x86_64-unknown-linux-gnu.tar.gz"
        ));
    }
    #[allow(unreachable_code)]
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// No newer release (or none published yet).
    UpToDate,
    Available(Release),
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<LatestAsset>,
}

#[derive(Deserialize)]
struct LatestAsset {
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Asks GitHub whether a release newer than `current` exists for `owner/repo`.
///
/// 404 (nothing published) is up to date. 403 and 429 are [`ErrorKind::RateLimited`]; other
/// failures are [`ErrorKind::Http`]; a reply that cannot be read is [`ErrorKind::Parse`].
pub fn check(
    http: &dyn HttpClient,
    owner: &str,
    repo: &str,
    current: &str,
    cancel: &CancelToken,
) -> Result<UpdateStatus> {
    cancel.check()?;
    let current = Version::parse(current).ok_or_else(|| {
        Error::new(ErrorKind::Parse)
            .with("version", current)
            .detail("the running version is not a semantic version")
    })?;
    let url = latest_release_api(owner, repo);
    let response = http.get(&url, &[("Accept", "application/vnd.github+json")])?;
    cancel.check()?;
    match response.status {
        200..=299 => {}
        404 => return Ok(UpdateStatus::UpToDate),
        403 | 429 => {
            return Err(Error::new(ErrorKind::RateLimited)
                .with("status", response.status)
                .with("url", &url));
        }
        status => {
            return Err(Error::new(ErrorKind::Http)
                .with("status", status)
                .with("url", &url));
        }
    }
    let latest: LatestRelease = serde_json::from_slice(&response.body).map_err(|e| {
        Error::new(ErrorKind::Parse).detail(format!("unexpected release reply: {e}"))
    })?;
    let version = Version::parse(&latest.tag_name).ok_or_else(|| {
        Error::new(ErrorKind::Parse)
            .with("tag", &latest.tag_name)
            .detail("the release tag is not v<version>")
    })?;
    if version <= current {
        return Ok(UpdateStatus::UpToDate);
    }
    // Only assets that really belong to this release's download area and read as plain file
    // names are kept; the rest are dropped without failing the check.
    let download_prefix = format!(
        "https://github.com/{owner}/{repo}/releases/download/{}/",
        latest.tag_name
    );
    let assets = latest
        .assets
        .into_iter()
        .filter(|asset| {
            asset.browser_download_url.starts_with(&download_prefix)
                && !asset.name.is_empty()
                && !asset.name.contains('/')
                && !asset.name.contains("..")
        })
        .map(|asset| Asset {
            name: asset.name,
            url: asset.browser_download_url,
            size: asset.size,
        })
        .collect();
    Ok(UpdateStatus::Available(Release {
        version: version.to_string(),
        tag: latest.tag_name,
        page: safe_release_page(&latest.html_url, owner, repo),
        assets,
    }))
}

pub fn check_this_app(http: &dyn HttpClient, cancel: &CancelToken) -> Result<UpdateStatus> {
    let (owner, repo) = repo_slug(REPOSITORY).ok_or_else(|| {
        Error::new(ErrorKind::Other)
            .with("repository", REPOSITORY)
            .detail("the package repository is not a GitHub URL")
    })?;
    check(http, &owner, &repo, CURRENT_VERSION, cancel)
}

/// The release page of `version` of this app; `None` if the package repository is not a GitHub
/// URL.
pub fn this_app_release_page(version: &str) -> Option<String> {
    let (owner, repo) = repo_slug(REPOSITORY)?;
    Some(release_page(&owner, &repo, version))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::http::HttpResponse;

    struct Fake {
        status: u16,
        body: String,
        asked: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(status: u16, body: &str) -> Fake {
            Fake {
                status,
                body: body.to_owned(),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpClient for Fake {
        fn get(&self, url: &str, _: &[(&str, &str)]) -> Result<HttpResponse> {
            self.asked.lock().unwrap().push(url.to_owned());
            Ok(HttpResponse {
                status: self.status,
                body: self.body.clone().into_bytes(),
            })
        }
    }

    fn reply(tag: &str, url: &str) -> String {
        format!(r#"{{"tag_name":"{tag}","html_url":"{url}","name":"x","draft":false}}"#)
    }

    fn run(fake: &Fake, current: &str) -> Result<UpdateStatus> {
        check(fake, "owner", "repo", current, &CancelToken::new())
    }

    const PAGE: &str = "https://github.com/owner/repo/releases/tag/v0.3.0";

    #[test]
    fn a_newer_release_is_an_update_with_its_page() {
        let fake = Fake::new(200, &reply("v0.3.0", PAGE));
        let status = run(&fake, "0.2.0").unwrap();
        assert_eq!(
            status,
            UpdateStatus::Available(Release {
                version: "0.3.0".into(),
                tag: "v0.3.0".into(),
                page: PAGE.into(),
                assets: vec![],
            })
        );
        assert_eq!(
            fake.asked.lock().unwrap().as_slice(),
            ["https://api.github.com/repos/owner/repo/releases/latest"]
        );
    }

    #[test]
    fn the_same_or_an_older_release_is_up_to_date() {
        for (tag, current) in [
            ("v0.2.0", "0.2.0"),
            ("v0.1.9", "0.2.0"),
            ("v0.2.0", "0.2.1"),
        ] {
            let fake = Fake::new(200, &reply(tag, PAGE));
            assert_eq!(
                run(&fake, current).unwrap(),
                UpdateStatus::UpToDate,
                "{tag} vs {current}"
            );
        }
    }

    #[test]
    fn pre_releases_and_build_metadata_follow_semver() {
        // The release is newer than its pre-release …
        let fake = Fake::new(200, &reply("v0.2.0", PAGE));
        assert!(matches!(
            run(&fake, "0.2.0-rc.1").unwrap(),
            UpdateStatus::Available(_)
        ));
        // … and a pre-release of the running version is not an update.
        let fake = Fake::new(200, &reply("v0.2.0-rc.2", PAGE));
        assert_eq!(run(&fake, "0.2.0").unwrap(), UpdateStatus::UpToDate);
        // Build metadata does not make a version newer.
        let fake = Fake::new(200, &reply("v0.2.0+build.7", PAGE));
        assert_eq!(run(&fake, "0.2.0").unwrap(), UpdateStatus::UpToDate);
        assert!(Version::parse("1.0.0-alpha").unwrap() < Version::parse("1.0.0-alpha.1").unwrap());
        assert!(
            Version::parse("1.0.0-alpha.1").unwrap() < Version::parse("1.0.0-alpha.beta").unwrap()
        );
        assert!(Version::parse("1.0.0-beta.2").unwrap() < Version::parse("1.0.0-beta.11").unwrap());
        assert!(Version::parse("1.0.0-rc.1").unwrap() < Version::parse("1.0.0").unwrap());
    }

    #[test]
    fn tag_forms() {
        for tag in ["v1.2.3", "V1.2.3", "1.2.3"] {
            assert_eq!(Version::parse(tag).unwrap().to_string(), "1.2.3", "{tag}");
        }
        for bad in ["release-1", "v1.2", "v1.2.3.4", "v01.x.3", "", "v1.2.3-"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
        let fake = Fake::new(200, &reply("latest", PAGE));
        assert_eq!(run(&fake, "0.2.0").unwrap_err().kind, ErrorKind::Parse);
    }

    #[test]
    fn no_release_yet_is_up_to_date() {
        let fake = Fake::new(404, r#"{"message":"Not Found"}"#);
        assert_eq!(run(&fake, "0.2.0").unwrap(), UpdateStatus::UpToDate);
    }

    #[test]
    fn rate_limits_and_other_statuses_are_typed_errors() {
        let fake = Fake::new(403, r#"{"message":"API rate limit exceeded"}"#);
        assert_eq!(
            run(&fake, "0.2.0").unwrap_err().kind,
            ErrorKind::RateLimited
        );
        let fake = Fake::new(429, "");
        assert_eq!(
            run(&fake, "0.2.0").unwrap_err().kind,
            ErrorKind::RateLimited
        );
        let error = run(&Fake::new(502, ""), "0.2.0").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Http);
        assert_eq!(error.param("status"), Some("502"));
    }

    #[test]
    fn a_malformed_reply_is_a_parse_error() {
        for body in ["not json", r#"{"html_url":"x"}"#, "[]"] {
            let fake = Fake::new(200, body);
            assert_eq!(
                run(&fake, "0.2.0").unwrap_err().kind,
                ErrorKind::Parse,
                "{body}"
            );
        }
    }

    #[test]
    fn only_pages_under_this_repositorys_releases_are_returned() {
        let fallback = "https://github.com/owner/repo/releases/latest";
        for url in [
            "https://evil.example.com/owner/repo/releases/tag/v0.3.0",
            "https://github.com/other/repo/releases/tag/v0.3.0",
            "https://github.com/owner/repo/releases/",
            "https://github.com/owner/repo/releases/../../evil",
            "https://github.com/owner/repo/releases/tag/v0.3.0?x=javascript:1",
            "http://github.com/owner/repo/releases/tag/v0.3.0",
            "",
        ] {
            assert_eq!(safe_release_page(url, "owner", "repo"), fallback, "{url}");
        }
        let fake = Fake::new(200, &reply("v0.3.0", "https://evil.example.com/x"));
        let UpdateStatus::Available(release) = run(&fake, "0.2.0").unwrap() else {
            panic!("expected an update");
        };
        assert_eq!(release.page, fallback);
    }

    #[test]
    fn the_repository_comes_from_the_package_metadata() {
        let (owner, repo) = repo_slug(REPOSITORY).expect("a GitHub repository URL");
        assert!(!owner.is_empty() && !repo.is_empty());
        assert_eq!(
            repo_slug("https://github.com/a/b.git"),
            Some(("a".to_owned(), "b".to_owned()))
        );
        assert_eq!(repo_slug("https://gitlab.com/a/b"), None);
        assert_eq!(repo_slug("https://github.com/a/b/c"), None);
    }

    #[test]
    fn release_assets_are_filtered_to_this_releases_download_area() {
        let body = r#"{
            "tag_name": "v0.3.0",
            "html_url": "https://github.com/owner/repo/releases/tag/v0.3.0",
            "assets": [
                {"name": "colab-playground-0.3.0-x86_64-pc-windows-msvc.exe",
                 "browser_download_url": "https://github.com/owner/repo/releases/download/v0.3.0/colab-playground-0.3.0-x86_64-pc-windows-msvc.exe",
                 "size": 100},
                {"name": "SHA256SUMS",
                 "browser_download_url": "https://github.com/owner/repo/releases/download/v0.3.0/SHA256SUMS",
                 "size": 10},
                {"name": "evil.exe",
                 "browser_download_url": "https://evil.example.com/releases/download/v0.3.0/evil.exe",
                 "size": 1},
                {"name": "..",
                 "browser_download_url": "https://github.com/owner/repo/releases/download/v0.3.0/..",
                 "size": 1},
                {"name": "a/b",
                 "browser_download_url": "https://github.com/owner/repo/releases/download/v0.3.0/a/b",
                 "size": 1}
            ]
        }"#;
        let fake = Fake::new(200, body);
        let UpdateStatus::Available(release) = run(&fake, "0.2.0").unwrap() else {
            panic!("expected an update");
        };
        let names: Vec<&str> = release
            .assets
            .iter()
            .map(|asset| asset.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "colab-playground-0.3.0-x86_64-pc-windows-msvc.exe",
                "SHA256SUMS"
            ]
        );
    }

    #[test]
    fn installable_asset_needs_both_the_platform_asset_and_the_sums() {
        // On a platform the installer does not support there is no platform asset name, so no
        // release is installable; the assertion below still holds there (both sides are None).
        let platform = platform_asset_name("0.3.0");
        let asset = |name: &str| Asset {
            name: name.to_owned(),
            url: format!("https://github.com/owner/repo/releases/download/v0.3.0/{name}"),
            size: 1,
        };
        let release = |assets: Vec<Asset>| Release {
            version: "0.3.0".into(),
            tag: "v0.3.0".into(),
            page: PAGE.into(),
            assets,
        };
        // Platform asset and sums present: installable when the platform is supported.
        if let Some(name) = &platform {
            let both = release(vec![
                asset(name),
                asset(SUMS_ASSET_NAME),
                asset("colab-playground-0.3.0-x86_64-unknown-linux-gnu.tar.gz"),
            ]);
            assert_eq!(both.installable_asset().map(|a| &a.name), Some(name));
        }
        // Missing the sums, or only the sums, is never installable.
        match &platform {
            Some(name) => {
                let no_sums = release(vec![asset(name)]);
                assert_eq!(no_sums.installable_asset(), None);
                let only_sums = release(vec![asset(SUMS_ASSET_NAME)]);
                assert_eq!(only_sums.installable_asset(), None);
            }
            None => {
                let both = release(vec![
                    asset(SUMS_ASSET_NAME),
                    asset("colab-playground-0.3.0-x86_64-pc-windows-msvc.exe"),
                ]);
                assert_eq!(both.installable_asset(), None);
            }
        }
    }
}
