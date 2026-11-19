//! Small file operations on the host side (inside WSL on Windows).
//!
//! All paths and contents travel as positional arguments or stdin, never in
//! script text.

use std::time::Duration;

use crate::clock::CancelToken;
use crate::error::{Error, ErrorKind, Result};
use crate::host::{Host, ScriptRequest, tail};
use crate::paths::HostPath;

#[derive(Debug, Clone, Copy)]
pub struct HostFs<'a> {
    host: &'a Host,
    env: &'a [(String, String)],
}

// Base64 behind a marker, so noise printed by login-shell profiles cannot
// corrupt the content.
const READ: &str = r#"[ -f "$1" ] || exit 3
printf '__CPG_FILE__ '
base64 < "$1" | tr -d '\n'
printf '\n'"#;

const WRITE: &str = r#"umask 077
d=$(dirname -- "$1")
mkdir -p -- "$d" || exit 1
t="$1.cpg.$$"
cat > "$t" && mv -f -- "$t" "$1""#;

const COPY_IF_EXISTS: &str = r#"[ -f "$1" ] || { echo absent; exit 0; }
umask 077
mkdir -p -- "$(dirname -- "$2")" || exit 1
cp -- "$1" "$2" && echo copied"#;

const REMOVE: &str = r#"rm -f -- "$1""#;

const ENSURE_DIR: &str = r#"umask 077
mkdir -p -- "$1""#;

impl<'a> HostFs<'a> {
    pub fn new(host: &'a Host, env: &'a [(String, String)]) -> Self {
        HostFs { host, env }
    }

    fn req(&self, script: &str) -> ScriptRequest {
        let mut r = ScriptRequest::new(script).timeout(Duration::from_secs(60));
        r.env = self.env.to_vec();
        r
    }

    /// Reads a text file; `None` if it does not exist.
    pub fn read(&self, path: &HostPath, cancel: &CancelToken) -> Result<Option<String>> {
        let out = self
            .host
            .run_quiet(self.req(READ).arg(path.as_str()), cancel)?;
        match out.code {
            Some(0) => {
                use base64::Engine as _;
                let b64 = out
                    .stdout
                    .lines()
                    .find_map(|l| l.strip_prefix("__CPG_FILE__ "))
                    .ok_or_else(|| Error::new(ErrorKind::Parse).detail("missing file marker"))?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|e| Error::new(ErrorKind::Parse).detail(e.to_string()))?;
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
            Some(3) => Ok(None),
            _ => Err(Error::new(ErrorKind::Io).detail(tail(&out.stderr, 300))),
        }
    }

    /// Atomically writes a file (owner-only), creating parent directories.
    pub fn write(&self, path: &HostPath, content: &[u8], cancel: &CancelToken) -> Result<()> {
        let out = self.host.run_quiet(
            self.req(WRITE)
                .arg(path.as_str())
                .stdin_bytes(content.to_vec()),
            cancel,
        )?;
        if out.success() {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::Io).detail(tail(&out.stderr, 300)))
        }
    }

    pub fn copy_if_exists(
        &self,
        src: &HostPath,
        dst: &HostPath,
        cancel: &CancelToken,
    ) -> Result<bool> {
        let out = self.host.run_quiet(
            self.req(COPY_IF_EXISTS).arg(src.as_str()).arg(dst.as_str()),
            cancel,
        )?;
        if !out.success() {
            return Err(Error::new(ErrorKind::Io).detail(tail(&out.stderr, 300)));
        }
        Ok(out.stdout.lines().any(|l| l.trim() == "copied"))
    }

    /// Creates a directory and its parents (owner-only). Existing is fine.
    pub fn ensure_dir(&self, path: &HostPath, cancel: &CancelToken) -> Result<()> {
        let out = self
            .host
            .run_quiet(self.req(ENSURE_DIR).arg(path.as_str()), cancel)?;
        if out.success() {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::Io).detail(tail(&out.stderr, 300)))
        }
    }

    /// Removes a file (missing is fine).
    pub fn remove(&self, path: &HostPath, cancel: &CancelToken) -> Result<()> {
        let out = self
            .host
            .run_quiet(self.req(REMOVE).arg(path.as_str()), cancel)?;
        if out.success() {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::Io).detail(tail(&out.stderr, 300)))
        }
    }
}
