//! Sunshine's control API: `/serverinfo`, pairing, `/applist`, `/launch`,
//! `/resume`, `/cancel`. Plain HTTP on 47989 until paired, then HTTPS on
//! 47984 with our certificate as the client credential and the host's
//! certificate pinned.

use crate::identity::{self, Identity};
use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, bail, Context, Result};
use latch_core::http;
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub const HTTP_PORT: u16 = 47989;
pub const HTTPS_PORT: u16 = 47984;

const TIMEOUT: Duration = Duration::from_secs(6);
/// The host holds the first pairing reply until a PIN is entered.
const PIN_WAIT: Duration = Duration::from_secs(90);
/// How long the host is given to register the first pairing request before
/// anyone is asked to enter the PIN, and how often the wait looks up.
const PIN_ASK_AFTER: Duration = Duration::from_millis(400);
const PIN_POLL: Duration = Duration::from_millis(100);

/// Nobody entered the PIN on the PC before it stopped holding the first
/// pairing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinNotEntered;

impl std::fmt::Display for PinNotEntered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nobody entered the PIN on the PC in time")
    }
}

impl std::error::Error for PinNotEntered {}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerInfo {
    pub hostname: String,
    pub app_version: String,
    pub gfe_version: String,
    pub codec_mode_support: i32,
    pub paired: bool,
    /// App id of the running session, when the host is streaming.
    pub current_game: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: u32,
    pub title: String,
}

/// Host cert is not the pinned DER. Display includes "certificate changed"
/// so the existing session.rs re-pair path still matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinMismatch;

impl std::fmt::Display for PinMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the PC's certificate changed; pair again")
    }
}

impl std::error::Error for PinMismatch {}

/// True when `err` is or wraps [`PinMismatch`], including inside rustls `Other`.
pub fn is_pin_mismatch(err: &anyhow::Error) -> bool {
    err.chain().any(cause_is_pin_mismatch)
}

fn cause_is_pin_mismatch(err: &(dyn std::error::Error + 'static)) -> bool {
    if err.downcast_ref::<PinMismatch>().is_some() {
        return true;
    }
    // rustls::Stream hands its error over as io::Error::new(InvalidData, e),
    // and io::Error::source() skips that inner value (it reports the inner
    // error's own source), so the chain walk never sees the rustls error.
    if let Some(inner) = err
        .downcast_ref::<std::io::Error>()
        .and_then(|io| io.get_ref())
    {
        return cause_is_pin_mismatch(inner);
    }
    let Some(tls) = err.downcast_ref::<rustls::Error>() else {
        return false;
    };
    let other = match tls {
        rustls::Error::InvalidCertificate(rustls::CertificateError::Other(o))
        | rustls::Error::Other(o) => o,
        _ => return false,
    };
    other.0.as_ref().downcast_ref::<PinMismatch>().is_some()
}

fn map_tls(err: anyhow::Error) -> anyhow::Error {
    if is_pin_mismatch(&err) {
        anyhow!(PinMismatch)
    } else {
        err
    }
}

pub struct Client<'a> {
    identity: &'a Identity,
    ip: IpAddr,
    /// Host certificate (DER) learned at pairing; `None` before.
    server_cert: Option<Vec<u8>>,
    tls: Option<Arc<rustls::ClientConfig>>,
    /// [`HTTP_PORT`] and [`HTTPS_PORT`], except against a test's fake host.
    http_port: u16,
    https_port: u16,
}

impl<'a> Client<'a> {
    pub fn new(identity: &'a Identity, ip: IpAddr, server_cert: Option<Vec<u8>>) -> Result<Self> {
        let tls = match &server_cert {
            Some(der) => Some(tls_config(identity, der.clone())?),
            None => None,
        };
        Ok(Self {
            identity,
            ip,
            server_cert,
            tls,
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
        })
    }

    /// The same client against a fake host on other ports. Tests never bind
    /// the real ones: a Mac sharing itself has Sunshine there.
    #[cfg(test)]
    fn with_ports(mut self, http: u16, https: u16) -> Self {
        self.http_port = http;
        self.https_port = https;
        self
    }

    pub fn server_cert(&self) -> Option<&[u8]> {
        self.server_cert.as_deref()
    }

    fn target(&self, path: &str, query: &str) -> String {
        let uuid: u128 = rand::random();
        let mut t = format!(
            "/{path}?uniqueid={}&uuid={uuid:032x}",
            self.identity.unique_id
        );
        if !query.is_empty() {
            t.push('&');
            t.push_str(query);
        }
        t
    }

    fn get_http(&self, path: &str, query: &str, read_timeout: Duration) -> Result<String> {
        let addr = SocketAddr::new(self.ip, self.http_port);
        let mut s = TcpStream::connect_timeout(&addr, TIMEOUT)?;
        s.set_read_timeout(Some(read_timeout))?;
        s.set_write_timeout(Some(TIMEOUT))?;
        let r = http::exchange(
            &mut s,
            "GET",
            &self.target(path, query),
            &self.ip.to_string(),
            "",
        )?;
        check(&r.body)?;
        Ok(r.body)
    }

    /// Sunshine rebuilds its client-certificate store for a moment after a
    /// pairing, and aborts handshakes with an alert meanwhile; those are
    /// retried.
    fn get_https(&self, path: &str, query: &str) -> Result<String> {
        let tls = self
            .tls
            .clone()
            .ok_or_else(|| anyhow!("not paired with this PC yet"))?;
        let target = self.target(path, query);
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.https_once(tls.clone(), &target) {
                Err(e) if attempt < 8 && e.to_string().contains("alert") => {
                    std::thread::sleep(Duration::from_millis(250));
                }
                other => return other,
            }
        }
    }

    fn https_once(&self, tls: Arc<rustls::ClientConfig>, target: &str) -> Result<String> {
        let addr = SocketAddr::new(self.ip, self.https_port);
        let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT)?;
        tcp.set_read_timeout(Some(TIMEOUT))?;
        tcp.set_write_timeout(Some(TIMEOUT))?;
        let name = rustls::pki_types::ServerName::from(self.ip);
        let mut conn = rustls::ClientConnection::new(tls, name)?;
        let mut s = rustls::Stream::new(&mut conn, &mut tcp);
        let r = http::exchange(&mut s, "GET", target, &self.ip.to_string(), "").map_err(map_tls)?;
        check(&r.body)?;
        Ok(r.body)
    }

    /// Over HTTPS when we hold a certificate for this host, which is also the
    /// only way `paired` comes back true.
    pub fn server_info(&self) -> Result<ServerInfo> {
        let xml = match self.tls {
            Some(_) => match self.get_https("serverinfo", "") {
                Ok(x) => x,
                // 401: the host no longer knows our certificate.
                Err(e) if e.to_string().contains("401") => {
                    self.get_http("serverinfo", "", TIMEOUT)?
                }
                Err(e) => return Err(e),
            },
            None => self.get_http("serverinfo", "", TIMEOUT)?,
        };
        Ok(ServerInfo {
            hostname: tag(&xml, "hostname").unwrap_or_default().to_string(),
            app_version: tag(&xml, "appversion").unwrap_or_default().to_string(),
            gfe_version: tag(&xml, "GfeVersion").unwrap_or_default().to_string(),
            codec_mode_support: tag(&xml, "ServerCodecModeSupport")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
            paired: tag(&xml, "PairStatus") == Some("1"),
            current_game: if tag(&xml, "state").is_some_and(|s| s.ends_with("_SERVER_BUSY")) {
                tag(&xml, "currentgame")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0)
            } else {
                0
            },
        })
    }

    /// [`Client::pair_cancellable`] without the cancel, for the live tests:
    /// `pending` runs once, beside the first request, when the host is
    /// waiting for the PIN.
    #[cfg(test)]
    pub fn pair(
        &mut self,
        pin: &str,
        device_name: &str,
        pending: impl FnOnce() + Send,
    ) -> Result<Vec<u8>> {
        let mut pending = Some(pending);
        std::thread::scope(|s| {
            self.pair_cancellable(pin, device_name, |_| {
                if let Some(p) = pending.take() {
                    s.spawn(p);
                }
                false
            })
        })
    }

    /// The Moonlight pairing handshake. The host holds our first request
    /// until someone enters `pin` on its side. Meanwhile `poll` is called
    /// about every 100 ms, from a moment after that request went out, with
    /// whether the host has answered it; the last call says `true`, and the
    /// handshake goes on at once. Entering the PIN is the caller's business,
    /// beside this wait, never in front of it. `poll` returns `true` to
    /// cancel, which aborts the request while it is held. On success the
    /// host certificate is pinned for the later HTTPS calls and returned.
    pub fn pair_cancellable(
        &mut self,
        pin: &str,
        device_name: &str,
        poll: impl FnMut(bool) -> bool,
    ) -> Result<Vec<u8>> {
        let previous_tls = self.tls.clone();
        let previous_cert = self.server_cert.clone();
        let result = self.pair_inner(pin, device_name, poll);
        if result.is_err() {
            self.tls = previous_tls;
            self.server_cert = previous_cert;
            let _ = self.get_http("unpair", "", TIMEOUT);
        }
        result
    }

    fn pair_inner(
        &mut self,
        pin: &str,
        device_name: &str,
        poll: impl FnMut(bool) -> bool,
    ) -> Result<Vec<u8>> {
        let salt: [u8; 16] = rand::random();
        let mut salted = salt.to_vec();
        salted.extend_from_slice(pin.as_bytes());
        let aes_key: [u8; 16] = Sha256::digest(&salted)[..16].try_into().unwrap();
        let cipher = Aes128::new(GenericArray::from_slice(&aes_key));
        let device = urlencode(device_name);

        // Phase 1 blocks on the host until the PIN is entered there.
        let first = {
            let query = format!(
                "devicename={device}&updateState=1&phrase=getservercert&salt={}&clientcert={}",
                hex(&salt),
                hex(self.identity.cert_pem.as_bytes())
            );
            let ip = self.ip;
            let target = self.target("pair", &query);
            let mut s = TcpStream::connect_timeout(&SocketAddr::new(ip, self.http_port), TIMEOUT)?;
            let abort = s.try_clone()?;
            let handle = std::thread::spawn(move || -> Result<String> {
                s.set_read_timeout(Some(PIN_WAIT))?;
                s.set_write_timeout(Some(TIMEOUT))?;
                let r = http::exchange(&mut s, "GET", &target, &ip.to_string(), "")
                    .map_err(held_request_error)?;
                check(&r.body)?;
                Ok(r.body)
            });
            wait_for_first_reply(handle, &abort, poll)?
        };
        if tag(&first, "paired") != Some("1") {
            bail!("the PC refused to start pairing");
        }
        let server_pem = tag(&first, "plaincert")
            .and_then(unhex)
            .ok_or_else(|| anyhow!("the PC is already pairing with someone else"))?;
        let server_der = identity::pem_to_der(&String::from_utf8_lossy(&server_pem))?;

        // Phase 2: our challenge, their response.
        let challenge: [u8; 16] = rand::random();
        let xml = self.get_http(
            "pair",
            &format!(
                "devicename={device}&updateState=1&clientchallenge={}",
                hex(&ecb(&cipher, &challenge, true))
            ),
            TIMEOUT,
        )?;
        if tag(&xml, "paired") != Some("1") {
            bail!("pairing failed at the challenge step");
        }
        let response = tag(&xml, "challengeresponse")
            .and_then(unhex)
            .filter(|c| c.len() == 48)
            .map(|c| ecb(&cipher, &c, false))
            .ok_or_else(|| anyhow!("bad challenge response"))?;
        if response.len() < 48 {
            bail!("bad challenge response");
        }
        let server_response_hash = &response[..32];
        let server_challenge = &response[32..48];

        // Phase 3: prove we hold our key; they prove they hold theirs.
        let secret: [u8; 16] = rand::random();
        let mut to_hash = server_challenge.to_vec();
        to_hash.extend_from_slice(&self.identity.cert_signature()?);
        to_hash.extend_from_slice(&secret);
        let xml = self.get_http(
            "pair",
            &format!(
                "devicename={device}&updateState=1&serverchallengeresp={}",
                hex(&ecb(&cipher, &Sha256::digest(&to_hash), true))
            ),
            TIMEOUT,
        )?;
        if tag(&xml, "paired") != Some("1") {
            bail!("pairing failed at the response step");
        }
        let pairing_secret = tag(&xml, "pairingsecret")
            .and_then(unhex)
            .ok_or_else(|| anyhow!("bad pairing secret"))?;
        if pairing_secret.len() <= 16 {
            bail!("bad pairing secret");
        }
        let server_secret = &pairing_secret[..16];
        identity::verify(&server_der, server_secret, &pairing_secret[16..])
            .context("the PC's signature did not check out")?;
        let mut expected = challenge.to_vec();
        expected.extend_from_slice(&identity::cert_signature(&server_der)?);
        expected.extend_from_slice(server_secret);
        if Sha256::digest(&expected).as_slice() != server_response_hash {
            bail!("wrong PIN");
        }

        // Phase 4: our secret, signed.
        let mut client_pairing_secret = secret.to_vec();
        client_pairing_secret.extend_from_slice(&self.identity.sign(&secret)?);
        let xml = self.get_http(
            "pair",
            &format!(
                "devicename={device}&updateState=1&clientpairingsecret={}",
                hex(&client_pairing_secret)
            ),
            TIMEOUT,
        )?;
        if tag(&xml, "paired") != Some("1") {
            bail!("pairing failed at the secret step");
        }

        // Phase 5: the first HTTPS request, with our certificate.
        self.tls = Some(tls_config(self.identity, server_der.clone())?);
        self.server_cert = Some(server_der.clone());
        let xml = self.get_https(
            "pair",
            &format!("devicename={device}&updateState=1&phrase=pairchallenge"),
        )?;
        if tag(&xml, "paired") != Some("1") {
            bail!("pairing failed at the final step");
        }
        Ok(server_der)
    }

    pub fn app_list(&self) -> Result<Vec<App>> {
        let xml = self.get_https("applist", "")?;
        Ok(parse_apps(&xml))
    }

    /// Start (`resume` false) or rejoin (`resume` true) a session. Returns the
    /// RTSP URL the stream is negotiated on.
    #[allow(clippy::too_many_arguments)]
    pub fn launch(
        &self,
        app_id: u32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        ri_key: &[u8; 16],
        ri_key_id: u32,
        resume: bool,
    ) -> Result<String> {
        let query = format!(
            "appid={app_id}&mode={width}x{height}x{fps}&additionalStates=1&sops=1&rikey={}&rikeyid={ri_key_id}&localAudioPlayMode=0&surroundAudioInfo=196610&remoteControllersBitmap=0&gcmap=0&gcpersist=0&bitrate={}{}",
            hex(ri_key),
            bitrate_kbps,
            crate::ffi::launch_query()
        );
        let xml = self.get_https(if resume { "resume" } else { "launch" }, &query)?;
        tag(&xml, "sessionUrl0")
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the PC did not return a session address"))
    }

    pub fn quit(&self) -> Result<()> {
        self.get_https("cancel", "").map(|_| ())
    }
}

/// The first pairing request's read gives up after [`PIN_WAIT`]: that is
/// nobody entering the PIN, not a network fault.
fn held_request_error(e: anyhow::Error) -> anyhow::Error {
    if e.chain().any(|c| c.to_string() == "timed out") {
        anyhow!(PinNotEntered)
    } else {
        e
    }
}

/// Wait for the host's answer to the first pairing request, which it holds
/// until the PIN is entered. From [`PIN_ASK_AFTER`] on, `poll` hears every
/// [`PIN_POLL`] whether the answer is in (the last call says so) and may
/// cancel the wait, which closes the connection under the held request.
/// The request's own read timeout bounds the whole wait.
fn wait_for_first_reply(
    reply: std::thread::JoinHandle<Result<String>>,
    connection: &TcpStream,
    mut poll: impl FnMut(bool) -> bool,
) -> Result<String> {
    let started = std::time::Instant::now();
    loop {
        let answered = reply.is_finished();
        if answered || started.elapsed() >= PIN_ASK_AFTER {
            let cancel = poll(answered);
            if answered {
                break;
            }
            if cancel {
                let _ = connection.shutdown(std::net::Shutdown::Both);
                let _ = reply.join();
                bail!("pairing cancelled");
            }
        }
        std::thread::sleep(PIN_POLL);
    }
    reply
        .join()
        .map_err(|_| anyhow!("pairing thread panicked"))?
}

/// Every reply carries `status_code`; anything but 200 is an error whose
/// text is in `status_message`.
fn check(xml: &str) -> Result<()> {
    let code = attr(xml, "status_code").unwrap_or("0");
    if code == "200" {
        return Ok(());
    }
    let msg = attr(xml, "status_message").unwrap_or("");
    bail!(
        "HTTP {code}: {}",
        if msg.is_empty() { xml.trim() } else { msg }
    )
}

fn tls_config(identity: &Identity, server_der: Vec<u8>) -> Result<Arc<rustls::ClientConfig>> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = rustls::crypto::ring::default_provider();
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider.clone()))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            der: server_der,
            algs: provider.signature_verification_algorithms,
        }))
        .with_client_auth_cert(
            vec![CertificateDer::from(identity.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone())),
        )?;
    Ok(Arc::new(cfg))
}

/// Accepts exactly the certificate learned at pairing, whatever it says.
#[derive(Debug)]
struct Pinned {
    der: Vec<u8>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.der.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::CertificateError::Other(rustls::OtherError(Arc::new(PinMismatch))).into())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algs.supported_schemes()
    }
}

fn ecb(cipher: &Aes128, data: &[u8], encrypt: bool) -> Vec<u8> {
    let mut out = data.to_vec();
    for block in out.as_chunks_mut::<16>().0 {
        let b = GenericArray::from_mut_slice(block);
        if encrypt {
            cipher.encrypt_block(b);
        } else {
            cipher.decrypt_block(b);
        }
    }
    out
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().as_bytes();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    fn digit(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    s.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Text of the first `<name>…</name>` element.
pub fn tag<'x>(xml: &'x str, name: &str) -> Option<&'x str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some(xml[start..end].trim())
}

/// Value of `name="…"` on the root element.
fn attr<'x>(xml: &'x str, name: &str) -> Option<&'x str> {
    let key = format!("{name}=\"");
    let start = xml.find(&key)? + key.len();
    let end = start + xml[start..].find('"')?;
    Some(&xml[start..end])
}

fn parse_apps(xml: &str) -> Vec<App> {
    let mut apps = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<App>") {
        rest = &rest[i + 5..];
        let Some(j) = rest.find("</App>") else { break };
        let body = &rest[..j];
        if let (Some(title), Some(id)) = (tag(body, "AppTitle"), tag(body, "ID")) {
            if let Ok(id) = id.parse() {
                apps.push(App {
                    id,
                    title: title.to_string(),
                });
            }
        }
        rest = &rest[j + 6..];
    }
    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_hex_from_the_host_is_rejected_without_panicking() {
        for invalid in ["0", "GG", "éé", "a€", "😀", "00\0a"] {
            assert_eq!(unhex(invalid), None, "{invalid:?}");
        }
        assert_eq!(unhex(" 00aF\n"), Some(vec![0, 175]));
    }

    const INFO: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200"><hostname>Gaming-PC</hostname><appversion>7.1.431.-1</appversion><GfeVersion>3.23.0.74</GfeVersion><HttpsPort>47984</HttpsPort><ServerCodecModeSupport>769</ServerCodecModeSupport><PairStatus>1</PairStatus><currentgame>881448767</currentgame><state>SUNSHINE_SERVER_BUSY</state></root>"#;

    #[test]
    fn xml_helpers_read_sunshine_replies() {
        assert_eq!(tag(INFO, "hostname"), Some("Gaming-PC"));
        assert_eq!(tag(INFO, "ServerCodecModeSupport"), Some("769"));
        assert_eq!(tag(INFO, "missing"), None);
        assert_eq!(attr(INFO, "status_code"), Some("200"));
        assert!(check(INFO).is_ok());
        let err = check(r#"<root status_code="400" status_message="Invalid uniqueid"/>"#)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("400") && err.contains("Invalid uniqueid"),
            "{err}"
        );
    }

    #[test]
    fn apps_are_listed_in_order() {
        let xml = r#"<root status_code="200"><App><IsHdrSupported>1</IsHdrSupported><AppTitle>Desktop</AppTitle><ID>881448767</ID></App><App><AppTitle>Steam Big Picture</AppTitle><ID>1234</ID></App></root>"#;
        let apps = parse_apps(xml);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].title, "Desktop");
        assert_eq!(apps[0].id, 881448767);
        assert_eq!(apps[1].id, 1234);
    }

    #[test]
    fn pinned_verifier_rejects_a_different_der_as_pin_mismatch() {
        use rustls::client::danger::ServerCertVerifier;
        let algs = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        let pinned = Pinned {
            der: vec![0x30, 0x82, 0x01],
            algs,
        };
        let name = rustls::pki_types::ServerName::from(IpAddr::from([127, 0, 0, 1]));
        let now = rustls::pki_types::UnixTime::since_unix_epoch(Duration::from_secs(1));
        let presented = rustls::pki_types::CertificateDer::from(vec![0x30, 0x82, 0x99]);
        let err = pinned
            .verify_server_cert(&presented, &[], &name, &[], now)
            .unwrap_err();
        assert!(
            !matches!(err, rustls::Error::General(_)),
            "pin mismatch must not be a generic rustls error: {err:?}"
        );
        let wrapped = anyhow::Error::from(err);
        assert!(is_pin_mismatch(&wrapped), "{wrapped:#}");
        let via_stream = anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            pinned
                .verify_server_cert(&presented, &[], &name, &[], now)
                .unwrap_err(),
        ));
        assert!(
            is_pin_mismatch(&via_stream),
            "the io::Error rustls::Stream produces must still classify: {via_stream:#}"
        );
        assert!(super::map_tls(via_stream)
            .downcast_ref::<PinMismatch>()
            .is_some());
        let surfaced = super::map_tls(wrapped);
        assert!(
            surfaced.to_string().contains("certificate changed"),
            "{surfaced}"
        );
        assert!(surfaced.downcast_ref::<PinMismatch>().is_some());
        let same = rustls::pki_types::CertificateDer::from(vec![0x30, 0x82, 0x01]);
        assert!(pinned
            .verify_server_cert(&same, &[], &name, &[], now)
            .is_ok());
        let generic = anyhow::Error::from(rustls::Error::General("handshake failed".into()));
        assert!(!is_pin_mismatch(&generic), "{generic:#}");
    }

    #[test]
    fn hex_round_trips_and_ecb_inverts() {
        let data = [0u8, 1, 2, 250, 255];
        assert_eq!(hex(&data), "000102FAFF");
        assert_eq!(unhex("000102faff").unwrap(), data);
        assert!(unhex("abc").is_none());
        let cipher = Aes128::new(GenericArray::from_slice(&[9u8; 16]));
        let ct = ecb(&cipher, &[1u8; 32], true);
        assert_ne!(ct, vec![1u8; 32]);
        assert_eq!(ecb(&cipher, &ct, false), vec![1u8; 32]);
        assert_eq!(urlencode("Example Mac"), "Example%20Mac");
    }

    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    /// One identity for every fake pairing; it plays the PC's certificate
    /// too, which is all the first reply has to carry.
    fn identity() -> &'static Identity {
        static ID: std::sync::OnceLock<Identity> = std::sync::OnceLock::new();
        ID.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "latch-fake-pair-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            let id = Identity::load_or_create(&dir).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            id
        })
    }

    type Seen = Arc<Mutex<Vec<(String, Instant)>>>;

    /// A stand-in for Sunshine's pairing port on 127.0.0.1. Each request's
    /// target is recorded with when it arrived; `reply` answers it, taking
    /// its time if it likes (a held first request), or says `None` to hold
    /// the connection until the client closes it.
    fn fake_host(reply: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> (u16, Seen) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Seen = Arc::default();
        let reply = Arc::new(reply);
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let (reply, log) = (reply.clone(), log.clone());
                std::thread::spawn(move || {
                    let Ok(Some(req)) = http::read_request(&mut s) else {
                        return;
                    };
                    log.lock().push((req.path.clone(), Instant::now()));
                    match reply(&req.path) {
                        Some(xml) => {
                            let _ = http::write_bytes(&mut s, 200, "text/xml", xml.as_bytes());
                        }
                        None => {
                            let _ = s.read(&mut [0u8; 1]);
                        }
                    }
                });
            }
        });
        (port, seen)
    }

    fn arrived(seen: &Seen, phrase: &str) -> Option<Instant> {
        seen.lock()
            .iter()
            .find(|(path, _)| path.contains(phrase))
            .map(|(_, at)| *at)
    }

    const NOT_PAIRED: &str = r#"<root status_code="200"><paired>0</paired></root>"#;

    fn client(port: u16) -> Client<'static> {
        Client::new(identity(), IpAddr::from([127, 0, 0, 1]), None)
            .unwrap()
            .with_ports(port, port)
    }

    /// The stuzha deadlock: the PIN is entered on the PC while whatever
    /// hands it over keeps failing. The handshake must go on as soon as
    /// the PC answers the first request, not when the hand-off gives up.
    #[test]
    fn the_handshake_goes_on_as_soon_as_the_pc_answers() {
        let entered = Arc::new(AtomicBool::new(false));
        let cert = hex(identity().cert_pem.as_bytes());
        let (port, seen) = fake_host({
            let entered = entered.clone();
            move |path| {
                if path.contains("phrase=getservercert") {
                    let t = Instant::now();
                    while !entered.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(10) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Some(format!(
                        r#"<root status_code="200"><paired>1</paired><plaincert>{cert}</plaincert></root>"#
                    ))
                } else if path.contains("clientchallenge=") {
                    Some(NOT_PAIRED.into())
                } else {
                    Some(r#"<root status_code="200"/>"#.into())
                }
            }
        });
        let mut polls = Vec::new();
        let mut entered_at = None;
        let err = client(port)
            .pair_cancellable("1234", "test", |answered| {
                polls.push(answered);
                // Someone types the PIN on the PC; the hand-off never
                // reports success and never cancels.
                if entered_at.is_none() {
                    entered_at = Some(Instant::now());
                    entered.store(true, Ordering::SeqCst);
                }
                false
            })
            .unwrap_err();
        assert_eq!(err.to_string(), "pairing failed at the challenge step");
        let challenged = arrived(&seen, "clientchallenge=").expect("phase 2 was attempted");
        let waited = challenged.duration_since(entered_at.unwrap());
        assert!(waited < Duration::from_secs(1), "phase 2 waited {waited:?}");
        assert_eq!(polls.last(), Some(&true), "the last poll hears the answer");
        assert!(polls[..polls.len() - 1].iter().all(|a| !a), "{polls:?}");
        assert!(
            arrived(&seen, "/unpair").is_some(),
            "a failed pairing unpairs"
        );
    }

    #[test]
    fn cancel_ends_a_held_first_request_at_once() {
        let (port, seen) = fake_host(|path| {
            if path.contains("phrase=getservercert") {
                None
            } else {
                Some(r#"<root status_code="200"/>"#.into())
            }
        });
        let start = Instant::now();
        let err = client(port)
            .pair_cancellable("1234", "test", |_| {
                start.elapsed() >= Duration::from_millis(300)
            })
            .unwrap_err();
        assert_eq!(err.to_string(), "pairing cancelled");
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        assert!(
            arrived(&seen, "/unpair").is_some(),
            "a cancelled pairing unpairs"
        );
    }

    #[test]
    fn a_refused_first_request_surfaces_at_once() {
        let (port, _) = fake_host(|_| Some(NOT_PAIRED.into()));
        let start = Instant::now();
        let mut polls = Vec::new();
        let err = client(port)
            .pair_cancellable("1234", "test", |answered| {
                polls.push(answered);
                false
            })
            .unwrap_err();
        assert_eq!(err.to_string(), "the PC refused to start pairing");
        assert!(
            start.elapsed() < Duration::from_millis(600),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(polls, [true], "the caller hears that the wait is over");
    }

    #[test]
    fn a_held_request_that_times_out_means_nobody_entered_the_pin() {
        let e = held_request_error(anyhow!("timed out"));
        assert!(e.downcast_ref::<PinNotEntered>().is_some(), "{e:#}");
        assert_eq!(e.to_string(), "nobody entered the PIN on the PC in time");
        let e = held_request_error(anyhow!("connection closed"));
        assert_eq!(e.to_string(), "connection closed");
    }
}

/// Against the Sunshine on this machine, with Latch Host entering the PIN:
/// `cargo test -p latch-stream pair_real -- --ignored --nocapture`
#[cfg(test)]
mod real {
    use super::*;

    #[test]
    #[ignore = "needs Sunshine and Latch Host on this machine"]
    fn pair_real() {
        let dir = std::env::temp_dir().join("latch-pair-test");
        let identity = Identity::load_or_create(&dir).unwrap();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let mut c = Client::new(&identity, ip, None).unwrap();
        let info = c.server_info().unwrap();
        eprintln!("before: {info:?}");
        let pin = "4321";
        let der = c
            .pair(pin, "latch-test", || {
                for _ in 0..20 {
                    let r: Result<latch_core::api::Ack> = http::post_json(
                        ("127.0.0.1", latch_core::CONTROL_PORT),
                        "/v1/pin",
                        &latch_core::api::PinRequest {
                            pin: pin.into(),
                            name: "latch-test".into(),
                        },
                        Duration::from_secs(10),
                    );
                    match r {
                        Ok(a) if a.ok => return,
                        other => eprintln!("pin submit: {other:?}"),
                    }
                    std::thread::sleep(Duration::from_millis(700));
                }
                panic!("host never accepted the PIN");
            })
            .unwrap();
        eprintln!("server cert: {} bytes", der.len());
        let info = c.server_info().unwrap();
        eprintln!("after: {info:?}");
        assert!(info.paired);
        let apps = c.app_list().unwrap();
        eprintln!("apps: {apps:?}");
        assert!(apps.iter().any(|a| a.title == "Desktop"));
        let again = Client::new(&identity, ip, Some(der)).unwrap();
        assert!(again.server_info().unwrap().paired);
    }
}
