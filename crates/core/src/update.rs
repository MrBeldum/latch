//! Finding and fetching Latch releases on GitHub, for both apps' updaters.
//!
//! A public repository needs no login. For a private one the calls carry a
//! token: the caller's setting, `LATCH_GITHUB_TOKEN`, or on macOS whatever
//! git has stored for github.com, which is the login the install script
//! uses too. Without one the requests still go out.
//!
//! Only the Mac talks to GitHub. It replaces its own bundle and sends each
//! PC the new `latch-host.exe` over the control API; see
//! [`crate::api::UPDATE_PATH`].

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const REPO: &str = "MrBeldum/latch";
pub const MAC_ASSET: &str = "latch-macos-arm64.tar.gz";
pub const WINDOWS_ASSET: &str = "latch-windows-x64.zip";
/// The host executable inside [`WINDOWS_ASSET`].
pub const HOST_EXE: &str = "latch-host.exe";

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HOPS: usize = 5;
/// Nothing Latch ships comes near this.
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    /// The API URL; downloading needs `Accept: application/octet-stream`.
    pub url: String,
    pub size: u64,
    /// GitHub's SHA-256 of the file, lowercase hex, when it published one.
    pub sha256: Option<String>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// Worth installing over `current`? A pre-release is offered only to a
    /// machine already on a pre-release.
    pub fn is_newer_than(&self, current: &Version) -> bool {
        self.version > *current && (!self.prerelease || !current.pre.is_empty())
    }
}

/// The version this binary was built as; every crate shares the workspace's.
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

/// Oldest host that will accept a POST to [`crate::api::UPDATE_PATH`].
pub fn first_update() -> Version {
    Version::parse(crate::api::FIRST_UPDATE_VERSION).expect("FIRST_UPDATE_VERSION is semver")
}

/// Whether a running host can take `latch-host.exe` from the Mac.
pub fn host_can_receive_update(running: &Version) -> bool {
    *running >= first_update()
}

/// Whether a peer takes a pushed `latch-host.exe` over `/v1/update`:
/// Windows only. `os` is the peer's `Status.os` (Tailscale's string), or
/// empty from a host older than 4.0, which was always Windows.
pub fn takes_pushed_host(os: &str) -> bool {
    os.is_empty() || os.eq_ignore_ascii_case("windows")
}

/// The GitHub token to use: the configured one, the environment, then what
/// git has stored for github.com.
pub fn token(configured: Option<&str>) -> Option<String> {
    configured
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| {
            crate::legacy::env("LATCH_GITHUB_TOKEN")
                .and_then(|t| t.into_string().ok())
                .filter(|t| !t.trim().is_empty())
        })
        .or_else(git_token)
}

#[cfg(target_os = "macos")]
fn git_token() -> Option<String> {
    // Without the command line tools, /usr/bin/git is a stub that opens an
    // installer dialog; look before calling.
    let have_git = Path::new("/Library/Developer/CommandLineTools/usr/bin/git").exists()
        || Path::new("/Applications/Xcode.app").exists();
    if !have_git {
        return None;
    }
    let mut child = std::process::Command::new("/usr/bin/git")
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let written = child.stdin.take().is_some_and(|mut stdin| {
        stdin
            .write_all(b"protocol=https\nhost=github.com\n\n")
            .is_ok()
    });
    if !written {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("password=").map(str::to_string))
        .filter(|t| !t.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn git_token() -> Option<String> {
    None
}

/// The newest release, as GitHub ranks them (pre-releases excluded).
pub fn latest(token: Option<&str>) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut body = Vec::new();
    let status = fetch(&url, token, "application/vnd.github+json", &mut body)?;
    match status {
        200 => parse_release(&body),
        404 if token.is_none() => {
            bail!("no release found; a private repository needs a GitHub token")
        }
        404 => bail!("no release found"),
        401 | 403 => bail!("GitHub refused the token (HTTP {status})"),
        _ => bail!("GitHub answered HTTP {status}"),
    }
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Deserialize)]
struct AssetJson {
    name: String,
    url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

pub fn parse_release(json: &[u8]) -> Result<Release> {
    let r: ReleaseJson = serde_json::from_slice(json).context("parse the release")?;
    let version = Version::parse(r.tag_name.trim_start_matches('v'))
        .with_context(|| format!("tag {:?} is not a version", r.tag_name))?;
    Ok(Release {
        version,
        tag: r.tag_name,
        prerelease: r.prerelease,
        assets: r
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.url,
                size: a.size,
                sha256: a
                    .digest
                    .as_deref()
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .map(|h| h.to_ascii_lowercase()),
            })
            .collect(),
    })
}

/// Download `asset` to `dest`, checking GitHub's digest and size. The file
/// is written under a temporary name and renamed only when it checks out, so
/// an existing `dest` is always a complete, verified copy.
pub fn download(asset: &Asset, token: Option<&str>, dest: &Path) -> Result<()> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = dest.with_extension("part");
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        let mut sink = Hashing {
            inner: &mut file,
            sha: Sha256::new(),
            written: 0,
        };
        let status = fetch(&asset.url, token, "application/octet-stream", &mut sink)?;
        if status != 200 {
            bail!("GitHub answered HTTP {status} for {}", asset.name);
        }
        let got = hex(&sink.sha.finalize());
        let written = sink.written;
        if let Some(want) = &asset.sha256 {
            if !got.eq_ignore_ascii_case(want) {
                bail!("{} does not match its published SHA-256", asset.name);
            }
        }
        if asset.size > 0 && written != asset.size {
            bail!(
                "{} is {written} bytes, the release says {}",
                asset.name,
                asset.size
            );
        }
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, dest).with_context(|| dest.display().to_string())
}

/// Check that `path` holds exactly the bytes the release describes. A cached
/// download is only as trustworthy as this check: the file may have been
/// truncated or replaced since it was written.
pub fn verify_asset(asset: &Asset, path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    let mut sink = Hashing {
        inner: &mut std::io::sink(),
        sha: Sha256::new(),
        written: 0,
    };
    std::io::copy(&mut file, &mut sink)?;
    if asset.size > 0 && sink.written != asset.size {
        bail!(
            "{} is {} bytes, the release says {}",
            asset.name,
            sink.written,
            asset.size
        );
    }
    if let Some(want) = &asset.sha256 {
        if !hex(&sink.sha.finalize()).eq_ignore_ascii_case(want) {
            bail!("{} does not match its published SHA-256", asset.name);
        }
    }
    Ok(())
}

struct Hashing<'a, W: Write> {
    inner: &'a mut W,
    sha: Sha256,
    written: u64,
}

impl<W: Write> Write for Hashing<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sha.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// GitHub's digest is the only integrity check besides TLS. An asset without
/// one is refused: a same-size cache swap would otherwise become an update.
pub fn require_digest(asset: &Asset) -> Result<()> {
    match &asset.sha256 {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(()),
        _ => bail!(
            "{} has no SHA-256 in the GitHub release; refusing to install it",
            asset.name
        ),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// GET `url` over HTTPS, following redirects, streaming the body into
/// `sink`. Returns the final status; the body is written whatever it is,
/// so callers can read GitHub's error message.
pub fn fetch(url: &str, token: Option<&str>, accept: &str, sink: &mut dyn Write) -> Result<u16> {
    let (first_host, _) = split_url(url)?;
    let mut url = url.to_string();
    for _ in 0..MAX_HOPS {
        let (host, path) = split_url(&url)?;
        // The token is for GitHub's API. A redirect lands on a storage host
        // whose URL carries its own signed grant and refuses a second one.
        let auth = token.filter(|_| host == first_host);
        let mut tls = connect(&host)?;
        let mut head = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: latch/{}\r\nAccept: {accept}\r\nConnection: close\r\n",
            env!("CARGO_PKG_VERSION")
        );
        if let Some(t) = auth {
            let value = format!("Bearer {t}");
            crate::http::validate_header("Authorization", &value)?;
            head.push_str(&format!("Authorization: {value}\r\n"));
        }
        head.push_str("\r\n");
        tls.write_all(head.as_bytes())
            .map_err(crate::http::io_err)?;
        tls.flush().map_err(crate::http::io_err)?;
        let mut reader = BufReader::new(tls);
        let (status, headers) = crate::http::read_response_head(&mut reader)?;
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = crate::http::header(&headers, "location")
                .ok_or_else(|| anyhow!("redirect without a location"))?;
            url = if loc.starts_with("https://") {
                loc.to_string()
            } else if loc.starts_with('/') {
                format!("https://{host}{loc}")
            } else {
                bail!("redirect to {loc:?} is not https");
            };
            continue;
        }
        // TLS peers report a truncated close as an error after the data;
        // the bytes read so far are the reply.
        crate::http::read_response_body(&mut reader, &headers, sink, MAX_DOWNLOAD, true)?;
        return Ok(status);
    }
    bail!("too many redirects")
}

/// `https://host/path?query` into `(host, path-with-query)`.
fn split_url(url: &str) -> Result<(String, String)> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("{url:?} is not an https URL"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if host.is_empty() {
        bail!("{url:?} has no host");
    }
    Ok((host.to_string(), path.to_string()))
}

fn connect(host: &str) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let tcp = crate::http::connect((host, 443u16), TIMEOUT)
        .with_context(|| format!("connect to {host}"))?;
    tcp.set_read_timeout(Some(TIMEOUT))
        .map_err(crate::http::io_err)?;
    tcp.set_write_timeout(Some(TIMEOUT))
        .map_err(crate::http::io_err)?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
    let conn = rustls::ClientConnection::new(tls_config()?, name)?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    if let Some(c) = CONFIG.get() {
        return Ok(c.clone());
    }
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // Pure-Rust crypto provider using s2n-bignum verified assembly via Rust inline
    // assembly (core::arch::asm!), compiled directly by rustc without cc or nasm.
    let provider = rustls_graviola::default_provider();
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let cfg = Arc::new(cfg);
    Ok(CONFIG.get_or_init(|| cfg).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    const RELEASE: &str = r#"{
      "tag_name": "v3.1.0", "prerelease": false,
      "assets": [
        {"name": "latch-macos-arm64.tar.gz", "url": "https://api.github.com/repos/x/y/releases/assets/1",
         "size": 5243666, "digest": "sha256:226CE7321E5185D10F6F83CD38916D25485D1734794119B279A72A9AF3CC4AB7"},
        {"name": "latch-windows-x64.zip", "url": "https://api.github.com/repos/x/y/releases/assets/2", "size": 28514670}
      ]
    }"#;

    #[test]
    fn release_json_is_parsed_and_compared() {
        let r = parse_release(RELEASE.as_bytes()).unwrap();
        assert_eq!(r.version, Version::new(3, 1, 0));
        assert_eq!(r.tag, "v3.1.0");
        let mac = r.asset(MAC_ASSET).unwrap();
        assert_eq!(mac.size, 5_243_666);
        assert_eq!(
            mac.sha256.as_deref(),
            Some("226ce7321e5185d10f6f83cd38916d25485d1734794119b279a72a9af3cc4ab7")
        );
        assert_eq!(r.asset(WINDOWS_ASSET).unwrap().sha256, None);
        require_digest(r.asset(MAC_ASSET).unwrap()).unwrap();
        assert!(require_digest(r.asset(WINDOWS_ASSET).unwrap()).is_err());
        assert!(r.asset("nope").is_none());
        assert!(r.is_newer_than(&Version::new(3, 0, 0)));
        assert!(!r.is_newer_than(&Version::new(3, 1, 0)));
        assert!(!r.is_newer_than(&Version::new(4, 0, 0)));
        // A pre-release goes only to machines already on one.
        let mut pre = r.clone();
        pre.prerelease = true;
        pre.version = Version::parse("3.2.0-beta.1").unwrap();
        assert!(!pre.is_newer_than(&Version::new(3, 1, 0)));
        assert!(pre.is_newer_than(&Version::parse("3.2.0-alpha.1").unwrap()));
        assert!(parse_release(br#"{"tag_name":"latest"}"#).is_err());
    }

    #[test]
    fn is_newer_than_offers_3_2_0_over_3_1_2_rc_5_and_not_equals() {
        let release = Release {
            version: Version::new(3, 2, 0),
            tag: "v3.2.0".into(),
            prerelease: false,
            assets: vec![],
        };
        assert!(release.is_newer_than(&Version::parse("3.1.2-rc.5").unwrap()));
        assert!(!release.is_newer_than(&Version::new(3, 2, 0)));
        assert_eq!(
            current(),
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
        );
    }

    #[test]
    fn urls_split_and_headers_are_case_insensitive() {
        assert_eq!(
            split_url("https://api.github.com/repos/a/b?x=1").unwrap(),
            ("api.github.com".into(), "/repos/a/b?x=1".into())
        );
        assert_eq!(split_url("https://h").unwrap(), ("h".into(), "/".into()));
        assert!(split_url("http://h/").is_err());
        assert!(split_url("https:///x").is_err());
        let h = vec![("location".to_string(), "https://o/x".to_string())];
        assert_eq!(crate::http::header(&h, "Location"), Some("https://o/x"));
    }

    fn read_head<R: BufRead>(r: &mut R) -> Result<(u16, Vec<(String, String)>)> {
        crate::http::read_response_head(r)
    }

    fn read_body<R: BufRead>(
        r: &mut R,
        headers: &[(String, String)],
        sink: &mut dyn Write,
    ) -> Result<()> {
        crate::http::read_response_body(r, headers, sink, MAX_DOWNLOAD, true)
    }

    #[test]
    fn chunked_sized_and_unsized_bodies_are_read() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n7\r\n, world\r\n0\r\nX-Trailer: 1\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let (status, headers) = read_head(&mut r).unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            crate::http::header(&headers, "content-type"),
            Some("text/plain")
        );
        let mut out = Vec::new();
        read_body(&mut r, &headers, &mut out).unwrap();
        assert_eq!(out, b"hello, world");

        let raw =
            b"HTTP/1.1 302 Found\r\nLocation: https://x/y\r\nContent-Length: 3\r\n\r\nabcEXTRA";
        let mut r = BufReader::new(&raw[..]);
        let (status, headers) = read_head(&mut r).unwrap();
        assert_eq!(status, 302);
        let mut out = Vec::new();
        read_body(&mut r, &headers, &mut out).unwrap();
        assert_eq!(out, b"abc");

        let raw = b"HTTP/1.1 200 OK\r\n\r\nto the end";
        let mut r = BufReader::new(&raw[..]);
        let (_, headers) = read_head(&mut r).unwrap();
        let mut out = Vec::new();
        read_body(&mut r, &headers, &mut out).unwrap();
        assert_eq!(out, b"to the end");

        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort";
        let mut r = BufReader::new(&raw[..]);
        let (_, headers) = read_head(&mut r).unwrap();
        assert!(read_body(&mut r, &headers, &mut Vec::new()).is_err());
    }

    #[test]
    fn only_windows_takes_a_pushed_host() {
        assert!(takes_pushed_host("windows"));
        assert!(takes_pushed_host("Windows"));
        assert!(
            takes_pushed_host(""),
            "a pre-4.0 host reports no OS and is Windows"
        );
        assert!(!takes_pushed_host("macOS"));
        assert!(!takes_pushed_host("linux"));
    }

    #[test]
    fn digests_are_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            current(),
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
        );
        assert_eq!(first_update(), Version::new(3, 1, 0));
        assert!(!host_can_receive_update(&Version::new(3, 0, 0)));
        assert!(!host_can_receive_update(&Version::new(3, 0, 1)));
        assert!(host_can_receive_update(&Version::new(3, 1, 0)));
        assert!(host_can_receive_update(&Version::new(3, 2, 0)));
    }

    #[test]
    fn a_cached_asset_is_rechecked_before_use() {
        let dir = std::env::temp_dir().join(format!("latch-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.bin");
        std::fs::write(&path, b"abc").unwrap();
        let mut asset = Asset {
            name: "x.bin".into(),
            url: String::new(),
            size: 3,
            sha256: Some(sha256_hex(b"abc").to_uppercase()),
        };
        assert!(verify_asset(&asset, &path).is_ok());
        assert!(verify_asset(&asset, &dir.join("missing")).is_err());
        std::fs::write(&path, b"abd").unwrap();
        assert!(verify_asset(&asset, &path).is_err());
        asset.sha256 = None;
        assert!(verify_asset(&asset, &path).is_ok());
        std::fs::write(&path, b"abcd").unwrap();
        assert!(verify_asset(&asset, &path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_download_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("latch-dl-{}", std::process::id()));
        let dest = dir.join("x.bin");
        let asset = Asset {
            name: "x.bin".into(),
            url: "https://127.0.0.1/nothing-listens-here".into(),
            size: 1,
            sha256: None,
        };
        assert!(download(&asset, None, &dest).is_err());
        assert!(!dest.exists());
        assert!(!dest.with_extension("part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "talks to real GitHub API over the network"]
    fn real_github_latest_release_fetch() {
        let rel = latest(None).expect("fetching latest release from GitHub API should succeed");
        assert!(!rel.tag.is_empty(), "release tag must not be empty");
        assert!(!rel.assets.is_empty(), "release assets must not be empty");
    }
}
