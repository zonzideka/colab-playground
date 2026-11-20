//! Downloading, verifying, staging and installing an update of this app.
//!
//! The download and the SHA-256 check are platform-independent; what differs per platform is
//! the archive layout and how the running install is swapped for the new one, so those live in
//! the `cfg`-gated helpers below. Everything here is best-effort where a failure cannot be
//! recovered from (spawning the new instance, cleaning up), and returns a typed [`Error`]
//! otherwise.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::http::HttpClient;
use crate::update::{Asset, Release, SUMS_ASSET_NAME};
use crate::{CancelToken, Error, ErrorKind, Result};

/// The SHA-256 for `asset_name` from a `SHA256SUMS` file, lowercased. Lines are
/// `<hex>  <name>` or `<hex> *<name>`; only an exact name match counts.
pub fn expected_sum(sums: &str, asset_name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, name) = line
            .trim_end_matches('\r')
            .split_once("  ")
            .or_else(|| line.split_once(" *"))?;
        (name == asset_name).then(|| hex.to_ascii_lowercase())
    })
}

/// Streams the file at `path` through SHA-256 and checks it against the entry for
/// `asset_name` in `sums`. A missing entry or a different digest is
/// [`ErrorKind::ChecksumMismatch`].
pub fn verify_download(path: &Path, sums: &str, asset_name: &str) -> Result<()> {
    let expected = expected_sum(sums, asset_name).ok_or_else(|| {
        Error::new(ErrorKind::ChecksumMismatch)
            .with("asset", asset_name)
            .detail("the release's SHA256SUMS has no entry for this asset")
    })?;
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(Error::new(ErrorKind::ChecksumMismatch)
            .with("asset", asset_name)
            .detail(format!("expected {expected}, computed {actual}")));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// A release downloaded into `dir`, ready to swap in as `payload`:
/// the extracted `.app` bundle on macOS, the extracted binary on Linux, the downloaded
/// executable itself on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpdate {
    pub version: String,
    pub dir: PathBuf,
    pub payload: PathBuf,
}

/// Downloads the platform asset and `SHA256SUMS` into `data_dir/updates/<version>`, verifies
/// the download and extracts the payload. On any failure the staging directory is removed.
pub fn stage(
    http: &dyn HttpClient,
    release: &Release,
    data_dir: &Path,
    cancel: &CancelToken,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<StagedUpdate> {
    let asset = release.installable_asset().ok_or_else(|| {
        Error::new(ErrorKind::Unsupported)
            .with("version", &release.version)
            .detail("this platform has no installable update asset")
    })?;
    let sums = release
        .asset_named(SUMS_ASSET_NAME)
        .expect("an installable asset implies SHA256SUMS is present");
    let dir = data_dir.join("updates").join(&release.version);
    let _ = std::fs::remove_dir_all(&dir);
    match stage_into(http, release, asset, sums, &dir, cancel, progress) {
        Ok(staged) => Ok(staged),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(error)
        }
    }
}

fn stage_into(
    http: &dyn HttpClient,
    release: &Release,
    asset: &Asset,
    sums: &Asset,
    dir: &Path,
    cancel: &CancelToken,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<StagedUpdate> {
    std::fs::create_dir_all(dir)?;
    let downloaded = dir.join(&asset.name);
    http.download(&asset.url, &downloaded, cancel, progress)?;
    let sums_response = http.get(&sums.url, &[])?;
    if !(200..=299).contains(&sums_response.status) {
        return Err(Error::new(ErrorKind::Http)
            .with("status", sums_response.status)
            .with("url", &sums.url));
    }
    let sums_text = String::from_utf8_lossy(&sums_response.body).into_owned();
    verify_download(&downloaded, &sums_text, &asset.name)?;
    let payload = extract_payload(dir, &downloaded, &release.version)?;
    Ok(StagedUpdate {
        version: release.version.clone(),
        dir: dir.to_path_buf(),
        payload,
    })
}

/// Runs `command` to completion, folding a spawn failure or a non-zero exit into
/// [`ErrorKind::Other`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_status(command: &mut std::process::Command, what: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|e| Error::new(ErrorKind::Other).detail(format!("cannot run {what}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::Other).detail(format!("{what} failed: {status}")))
    }
}

#[cfg(target_os = "macos")]
fn extract_payload(dir: &Path, downloaded: &Path, version: &str) -> Result<PathBuf> {
    let extracted = dir.join("extracted");
    std::fs::create_dir_all(&extracted)?;
    run_status(
        std::process::Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(downloaded)
            .arg(&extracted),
        "ditto",
    )?;
    let base = format!("colab-playground-{version}-aarch64-apple-darwin");
    let app = extracted.join(base).join("Colab Playground.app");
    if !app.join("Contents/MacOS/colab-playground").is_file() {
        return Err(Error::new(ErrorKind::Other)
            .detail("the extracted bundle has no executable at Contents/MacOS/colab-playground"));
    }
    Ok(app)
}

#[cfg(target_os = "linux")]
fn extract_payload(dir: &Path, downloaded: &Path, version: &str) -> Result<PathBuf> {
    let extracted = dir.join("extracted");
    std::fs::create_dir_all(&extracted)?;
    run_status(
        std::process::Command::new("tar")
            .arg("-xzf")
            .arg(downloaded)
            .arg("-C")
            .arg(&extracted),
        "tar",
    )?;
    let base = format!("colab-playground-{version}-x86_64-unknown-linux-gnu");
    let payload = extracted.join(&base).join("colab-playground");
    if !payload.is_file() {
        return Err(Error::new(ErrorKind::Other).detail(format!(
            "the extracted archive has no {base}/colab-playground"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(payload)
}

#[cfg(target_os = "windows")]
fn extract_payload(dir: &Path, downloaded: &Path, _version: &str) -> Result<PathBuf> {
    let payload = dir.join("colab-playground.exe");
    std::fs::rename(downloaded, &payload)?;
    Ok(payload)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn extract_payload(_dir: &Path, _downloaded: &Path, _version: &str) -> Result<PathBuf> {
    Err(Error::new(ErrorKind::Unsupported).detail("no updater for this platform"))
}

/// The `.app` bundle `path` sits inside, or `None` when it does not.
#[cfg(target_os = "macos")]
fn app_bundle(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".app"))
        })
        .map(Path::to_path_buf)
}

/// Swaps the running install for `staged` and spawns the new instance; the caller exits the
/// process right after `Ok`. The old install is kept as a `.app.old` bundle.
#[cfg(target_os = "macos")]
pub fn apply(staged: &StagedUpdate, current_exe: &Path) -> Result<()> {
    let bundle = app_bundle(current_exe)
        .ok_or_else(|| Error::new(ErrorKind::Other).detail("not running from an app bundle"))?;
    let backup = bundle.with_extension("app.old");
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(&bundle, &backup)?;
    if let Err(error) = std::fs::rename(&staged.payload, &bundle) {
        // A rename across devices fails; copy with ditto instead, then drop the staged copy.
        let copied = run_status(
            std::process::Command::new("/usr/bin/ditto")
                .arg(&staged.payload)
                .arg(&bundle),
            "ditto",
        );
        if let Err(copy_error) = copied {
            let _ = std::fs::rename(&backup, &bundle);
            return Err(copy_error);
        }
        let _ = std::fs::remove_dir_all(&staged.payload);
    }
    let _ = std::process::Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&bundle)
        .status();
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(&bundle)
        .spawn();
    Ok(())
}

/// Swaps the running binary for `staged` and spawns the new instance; the caller exits the
/// process right after `Ok`. The old binary is kept as a `<name>.old` file.
#[cfg(not(target_os = "macos"))]
pub fn apply(staged: &StagedUpdate, current_exe: &Path) -> Result<()> {
    let Some(name) = current_exe.file_name() else {
        return Err(
            Error::new(ErrorKind::Other).detail("the running executable path has no file name")
        );
    };
    let backup = current_exe.with_file_name(format!("{}.old", name.to_string_lossy()));
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(current_exe, &backup)?;
    if let Err(error) = std::fs::copy(&staged.payload, current_exe) {
        let _ = std::fs::rename(&backup, current_exe);
        return Err(error.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The copy already committed the swap; a failed chmod must not report failure now.
        let _ = std::fs::set_permissions(current_exe, std::fs::Permissions::from_mode(0o755));
    }
    let _ = std::process::Command::new(current_exe).spawn();
    Ok(())
}

/// Best-effort removal of a stale backup and the staged-updates directory; never fails.
pub fn cleanup_stale(current_exe: &Path, data_dir: &Path) {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = app_bundle(current_exe) {
        let _ = std::fs::remove_dir_all(bundle.with_extension("app.old"));
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(name) = current_exe.file_name() {
        let _ = std::fs::remove_file(
            current_exe.with_file_name(format!("{}.old", name.to_string_lossy())),
        );
    }
    let _ = std::fs::remove_dir_all(data_dir.join("updates"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::http::HttpResponse;
    use crate::update::Asset;

    fn release(version: &str, asset_name: &str) -> Release {
        let url = format!("https://github.com/o/r/releases/download/v{version}/{asset_name}");
        Release {
            version: version.to_owned(),
            tag: format!("v{version}"),
            page: format!("https://github.com/o/r/releases/tag/v{version}"),
            assets: vec![
                Asset {
                    name: asset_name.to_owned(),
                    url,
                    size: 0,
                },
                Asset {
                    name: SUMS_ASSET_NAME.to_owned(),
                    url: format!(
                        "https://github.com/o/r/releases/download/v{version}/{SUMS_ASSET_NAME}"
                    ),
                    size: 0,
                },
            ],
        }
    }

    #[test]
    fn expected_sum_parses_both_separators_and_matches_exactly() {
        let sums = "abcd  foo\n1234 *foo2\nCAFE  bar\n";
        assert_eq!(expected_sum(sums, "foo"), Some("abcd".into()));
        assert_eq!(expected_sum(sums, "foo2"), Some("1234".into()));
        assert_eq!(expected_sum(sums, "bar"), Some("cafe".into()));
        assert_eq!(expected_sum(sums, "fo"), None);
        assert_eq!(expected_sum(sums, "foo22"), None);
        assert_eq!(expected_sum("", "foo"), None);
    }

    #[test]
    fn verify_download_checks_the_streamed_hash() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("asset.bin");
        std::fs::write(&file, b"payload").unwrap();
        let hex = "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5"; // sha256("payload")
        // The right entry passes, under either separator.
        verify_download(&file, &format!("{hex}  asset.bin\n"), "asset.bin").unwrap();
        verify_download(&file, &format!("{hex} *asset.bin\n"), "asset.bin").unwrap();
        // A wrong digest and a missing entry both fail as a checksum mismatch.
        let wrong = format!("{}  asset.bin\n", "0".repeat(64));
        assert_eq!(
            verify_download(&file, &wrong, "asset.bin")
                .unwrap_err()
                .kind,
            ErrorKind::ChecksumMismatch
        );
        assert_eq!(
            verify_download(&file, "", "asset.bin").unwrap_err().kind,
            ErrorKind::ChecksumMismatch
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn apply_swaps_the_binary_and_keeps_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("colab-playground");
        std::fs::write(&current, b"old").unwrap();
        let staged = StagedUpdate {
            version: "1.0.0".into(),
            dir: dir.path().join("updates").join("1.0.0"),
            payload: dir.path().join("payload"),
        };
        std::fs::write(&staged.payload, b"new bytes").unwrap();
        apply(&staged, &current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new bytes");
        assert!(dir.path().join("colab-playground.old").exists());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn apply_restores_the_backup_when_the_copy_fails() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("colab-playground");
        std::fs::write(&current, b"old").unwrap();
        let staged = StagedUpdate {
            version: "1.0.0".into(),
            dir: dir.path().join("updates").join("1.0.0"),
            // The payload is missing, so the copy after the rename fails.
            payload: dir.path().join("missing"),
        };
        let error = apply(&staged, &current).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Io);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!dir.path().join("colab-playground.old").exists());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn cleanup_stale_removes_the_backup_and_the_updates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("colab-playground");
        std::fs::write(&current, b"old").unwrap();
        let backup = dir.path().join("colab-playground.old");
        std::fs::write(&backup, b"stale").unwrap();
        let data = dir.path().join("data");
        let updates = data.join("updates");
        std::fs::create_dir_all(updates.join("0.9.0")).unwrap();
        cleanup_stale(&current, &data);
        assert!(!backup.exists());
        assert!(!updates.exists());
    }

    /// A minimal `.app` bundle holding `exe_bytes` as its executable, for the macOS swap.
    #[cfg(target_os = "macos")]
    fn fake_bundle(parent: &Path, exe_bytes: &[u8]) -> PathBuf {
        let bundle = parent.join("Colab Playground.app");
        let macos_dir = bundle.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos_dir).unwrap();
        std::fs::write(macos_dir.join("colab-playground"), exe_bytes).unwrap();
        bundle
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_swaps_the_bundle_and_keeps_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        let bundle = fake_bundle(&install, b"old");
        let current = bundle
            .join("Contents")
            .join("MacOS")
            .join("colab-playground");
        let staged = StagedUpdate {
            version: "1.0.0".into(),
            dir: staging.clone(),
            payload: fake_bundle(&staging, b"new bytes"),
        };
        apply(&staged, &current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new bytes");
        let backup_exe = install
            .join("Colab Playground.app.old")
            .join("Contents")
            .join("MacOS")
            .join("colab-playground");
        assert_eq!(std::fs::read(backup_exe).unwrap(), b"old");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_restores_the_bundle_when_the_swap_fails() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        std::fs::create_dir_all(&install).unwrap();
        let bundle = fake_bundle(&install, b"old");
        let current = bundle
            .join("Contents")
            .join("MacOS")
            .join("colab-playground");
        let staged = StagedUpdate {
            version: "1.0.0".into(),
            dir: dir.path().join("updates").join("1.0.0"),
            // The payload bundle is missing, so both the rename and the ditto copy fail.
            payload: dir.path().join("missing.app"),
        };
        let error = apply(&staged, &current).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Other);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!install.join("Colab Playground.app.old").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cleanup_stale_removes_the_bundle_backup_and_the_updates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = fake_bundle(dir.path(), b"new");
        let current = bundle
            .join("Contents")
            .join("MacOS")
            .join("colab-playground");
        let backup = dir.path().join("Colab Playground.app.old");
        std::fs::create_dir_all(backup.join("Contents")).unwrap();
        let data = dir.path().join("data");
        let updates = data.join("updates");
        std::fs::create_dir_all(updates.join("0.9.0")).unwrap();
        cleanup_stale(&current, &data);
        assert!(!backup.exists());
        assert!(!updates.exists());
    }

    /// A fake client that serves fixed asset bytes and a SHA256SUMS body, so `stage` can be
    /// driven end to end without the network.
    struct FakeClient {
        asset: Vec<u8>,
        sums: String,
        asked: Mutex<Vec<String>>,
    }

    impl HttpClient for FakeClient {
        fn get(&self, url: &str, _: &[(&str, &str)]) -> Result<HttpResponse> {
            self.asked.lock().unwrap().push(url.to_owned());
            Ok(HttpResponse {
                status: 200,
                body: self.sums.clone().into_bytes(),
            })
        }

        fn download(
            &self,
            url: &str,
            dest: &Path,
            _cancel: &CancelToken,
            progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
        ) -> Result<u64> {
            self.asked.lock().unwrap().push(url.to_owned());
            std::fs::write(dest, &self.asset)?;
            progress(self.asset.len() as u64, Some(self.asset.len() as u64));
            Ok(self.asset.len() as u64)
        }
    }

    fn sums_for(asset_name: &str, bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}  {asset_name}\n", hasher.finalize())
    }

    /// The Windows path (no extraction): on other platforms only the checksum-mismatch path is
    /// exercised, since a staged tar or zip needs a real archive to extract.
    #[cfg(target_os = "windows")]
    #[test]
    fn stage_downloads_verifies_and_keeps_the_payload() {
        let version = "1.0.0";
        let asset_name = crate::update::platform_asset_name(version).unwrap();
        let body = b"fake executable";
        let sums = sums_for(&asset_name, body);
        let rel = release(version, &asset_name);
        let client = FakeClient {
            asset: body.to_vec(),
            sums,
            asked: Mutex::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut progress: Vec<(u64, Option<u64>)> = Vec::new();
        let staged = stage(
            &client,
            &rel,
            dir.path(),
            &CancelToken::new(),
            &mut |received, total| progress.push((received, total)),
        )
        .unwrap();
        assert_eq!(staged.version, version);
        assert_eq!(std::fs::read(&staged.payload).unwrap(), body);
        assert!(staged.dir.starts_with(dir.path().join("updates")));
    }

    #[test]
    fn stage_removes_the_dir_when_the_checksum_does_not_match() {
        let version = "1.0.0";
        // The fake serves whatever the current platform downloads, but a sums file that does not
        // match, so verification fails before any extraction.
        let Some(asset_name) = crate::update::platform_asset_name(version) else {
            return;
        };
        let body = b"whatever bytes";
        let sums = sums_for(&asset_name, b"different bytes than the asset");
        let rel = release(version, &asset_name);
        let client = FakeClient {
            asset: body.to_vec(),
            sums,
            asked: Mutex::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let error = stage(
            &client,
            &rel,
            dir.path(),
            &CancelToken::new(),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::ChecksumMismatch);
        assert!(
            !dir.path().join("updates").join(version).exists(),
            "the staging directory is removed on failure"
        );
    }
}
