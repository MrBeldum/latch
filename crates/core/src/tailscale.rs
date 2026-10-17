//! Reading Tailscale through its own CLI. The tailnet is Latch's network
//! *and* its list of trusted machines, so both apps lean on it entirely.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The ACL tag Latch uses for a tailnet peer-relay node.
pub const RELAY_TAG: &str = "tag:relay";

/// Short deadline for the optional relay probe. Other CLI calls are bounded too.
const PEER_RELAY_SERVERS_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the CLI lives, first hit wins. The macOS App Store build keeps it
/// inside the bundle; the standalone build symlinks it; Homebrew's is last.
pub fn cli() -> Option<PathBuf> {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    if let Some(p) = PATH.get() {
        return Some(p.clone());
    }
    let found = find_cli()?;
    let _ = PATH.set(found.clone());
    Some(found)
}

fn find_cli() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Program Files\Tailscale\tailscale.exe",
            r"C:\Program Files (x86)\Tailscale\tailscale.exe",
        ]
    } else {
        &[
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
            "/usr/local/bin/tailscale",
            "/opt/homebrew/bin/tailscale",
        ]
    };
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .or_else(|| {
            let mut c = Command::new("tailscale");
            c.arg("--version");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            let out = c.output().ok()?;
            out.status.success().then(|| PathBuf::from("tailscale"))
        })
}

/// `tailscale status --json`, the parts Latch reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Status {
    pub backend_state: String,
    #[serde(rename = "Self")]
    pub self_node: Node,
    pub peer: BTreeMap<String, Node>,
    pub user: BTreeMap<String, User>,
    /// The tailnet this machine is in. Absent before Tailscale 1.26.
    pub current_tailnet: Option<Tailnet>,
    /// Whether tailscaled runs a kernel TUN device. `false` is userspace
    /// networking (a container's tailscaled), where every tailnet peer
    /// reaches a local service from 127.0.0.1; `None` when it does not say.
    #[serde(rename = "TUN")]
    pub tun: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Tailnet {
    /// The owner's login for a personal tailnet ("ada@example.com"), the
    /// domain for an organisation's ("example.com").
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "PascalCase")]
pub struct Node {
    /// Stable node id, the key the client stores wake details under.
    #[serde(rename = "ID")]
    pub id: String,
    pub host_name: String,
    #[serde(rename = "DNSName")]
    pub dns_name: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "UserID")]
    pub user_id: u64,
    #[serde(rename = "TailscaleIPs")]
    pub tailscale_ips: Vec<String>,
    pub online: bool,
    /// The public `ip:port` of the peer's last direct path, if any.
    #[serde(rename = "CurAddr")]
    pub cur_addr: String,
    /// The peer's home DERP region code ("tok", "lax"), which is where its
    /// packets go when there is no direct path.
    pub relay: String,
    /// The peer relay carrying this path, as `ip:port:vni:id`, when
    /// Tailscale found one. Empty for a direct or DERP path, and always
    /// empty before Tailscale 1.86. `Relay` stays set beside it: that is
    /// the peer's *home* DERP, not the path in use.
    #[serde(rename = "PeerRelay")]
    pub peer_relay: String,
    /// ACL tags ("tag:relay"), present only on tagged nodes.
    pub tags: Vec<String>,
    /// Traffic has flowed recently, so `cur_addr` and `relay` describe the
    /// path in use rather than a guess.
    pub active: bool,
    /// When this machine's node key stops working (RFC 3339), unless key
    /// expiry is disabled for it in the admin console; a zero time then.
    pub key_expiry: String,
    pub expired: bool,
}

impl Node {
    pub fn ipv4(&self) -> Option<Ipv4Addr> {
        self.tailscale_ips.iter().find_map(|s| s.parse().ok())
    }
    /// Days until the node key expires; `None` when it never does.
    pub fn key_expiry_days(&self) -> Option<i64> {
        if self.expired {
            return Some(0);
        }
        crate::dates::days_until(&self.key_expiry)
    }
    pub fn is_windows(&self) -> bool {
        self.os.eq_ignore_ascii_case("windows")
    }
    pub fn is_macos(&self) -> bool {
        self.os.eq_ignore_ascii_case("macos") || self.os.eq_ignore_ascii_case("mac os")
    }
    pub fn is_linux(&self) -> bool {
        self.os.eq_ignore_ascii_case("linux")
    }
    /// Short label for the machine list: "Windows", "macOS", "Linux".
    pub fn os_label(&self) -> &'static str {
        if self.is_windows() {
            "Windows"
        } else if self.is_macos() {
            "macOS"
        } else if self.is_linux() {
            "Linux"
        } else if self.os.is_empty() {
            "Unknown"
        } else {
            "Other"
        }
    }
    /// True only for `tag:relay`. Other tags leave a stream PC in the list.
    pub fn is_relay(&self) -> bool {
        self.tags.iter().any(|t| t == RELAY_TAG)
    }
    /// The peer's public IPv4 address, when Tailscale reached it directly.
    pub fn public_ipv4(&self) -> Option<Ipv4Addr> {
        self.cur_addr.rsplit_once(':')?.0.parse().ok()
    }
    /// Packets reach this peer directly rather than through a DERP relay.
    /// Unknown until traffic has flowed; `None` then.
    pub fn direct(&self) -> Option<bool> {
        if !self.cur_addr.is_empty() {
            Some(true)
        } else if self.active || !self.relay.is_empty() {
            Some(false)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct User {
    #[serde(rename = "ID")]
    pub id: u64,
    pub login_name: String,
    pub display_name: String,
}

impl Status {
    pub fn running(&self) -> bool {
        self.backend_state == "Running"
    }
    pub fn self_login(&self) -> Option<&str> {
        self.user
            .get(&self.self_node.user_id.to_string())
            .map(|u| u.login_name.as_str())
    }
    /// Every tailnet peer that can share a desktop: any OS, minus `tag:relay`
    /// nodes that exist only to carry packets. Sorted by hostname.
    pub fn machine_peers(&self) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.peer.values().filter(|n| !n.is_relay()).collect();
        v.sort_by(|a, b| a.host_name.cmp(&b.host_name));
        v
    }
    /// Nodes tagged `tag:relay`, any OS. Sorted by hostname like the PC list.
    pub fn relay_peers(&self) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.peer.values().filter(|n| n.is_relay()).collect();
        v.sort_by(|a, b| a.host_name.cmp(&b.host_name));
        v
    }
}

pub fn parse_status(json: &str) -> Result<Status> {
    serde_json::from_str(json).context("parse tailscale status")
}

/// `tailscale debug peer-relay-servers` stdout. A 1.102.3 run with no
/// servers is `[]`. Valid JSON array of strings → `Some` (empty is known
/// empty, not a failure). Anything else → `None` (unknown, not ACL denial).
pub fn parse_peer_relay_servers(stdout: &str) -> Option<Vec<String>> {
    serde_json::from_str(stdout.trim()).ok()
}

/// Candidate peer-relay servers this node may use. `Err` when the CLI is
/// missing, the command fails or times out, or stdout is not a JSON string
/// array. A successful empty list is `Ok([])`.
pub fn peer_relay_servers() -> Result<Vec<String>> {
    let cli = cli().ok_or_else(|| anyhow::anyhow!("Tailscale is not installed"))?;
    let out = run_limited(
        Command::new(cli).args(["debug", "peer-relay-servers"]),
        PEER_RELAY_SERVERS_TIMEOUT,
    )?;
    parse_peer_relay_servers(&out)
        .ok_or_else(|| anyhow::anyhow!("peer-relay-servers: unexpected output"))
}

/// Run `tailscale status --json`. Errors when the CLI is missing or the
/// daemon is not running (the CLI prints why on stderr).
pub fn status() -> Result<Status> {
    let cli = cli().ok_or_else(|| anyhow::anyhow!("Tailscale is not installed"))?;
    let out = run(Command::new(cli).args(["status", "--json"]))?;
    let st = parse_status(&out)?;
    if !st.running() {
        bail!(
            "Tailscale is {}",
            if st.backend_state.is_empty() {
                "not running".into()
            } else {
                st.backend_state.to_lowercase()
            }
        );
    }
    Ok(st)
}

/// `tailscale whois --json IP`: which account owns the machine at `ip`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct WhoIs {
    pub user_profile: User,
    pub node: WhoIsNode,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct WhoIsNode {
    pub name: String,
    #[serde(rename = "ComputedName")]
    pub computed_name: String,
}

pub fn whois(ip: std::net::IpAddr) -> Result<WhoIs> {
    let cli = cli().ok_or_else(|| anyhow::anyhow!("Tailscale is not installed"))?;
    let out = run_limited(
        Command::new(cli).args(["whois", "--json", &ip.to_string()]),
        Duration::from_secs(2),
    )?;
    serde_json::from_str(&out).context("parse tailscale whois")
}

/// `tailscale whois` for a connection that reached this machine from
/// `addr`, a loopback `ip:port`. Under userspace networking tailscaled
/// relays each tailnet peer's TCP connection from loopback and remembers
/// which peer every local port belongs to. `Ok(None)` when it knows of
/// none: a process on this machine made the connection.
pub fn whois_addr(addr: std::net::SocketAddr) -> Result<Option<WhoIs>> {
    let cli = cli().ok_or_else(|| anyhow::anyhow!("Tailscale is not installed"))?;
    let out = run_limited(
        Command::new(cli).args(["whois", "--json", "--proto=tcp", &addr.to_string()]),
        Duration::from_secs(2),
    );
    match out {
        Ok(out) => serde_json::from_str(&out)
            .map(Some)
            .context("parse tailscale whois"),
        Err(e) if no_such_peer(&e.to_string()) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The CLI's answer for an address no peer is behind: it logs
/// "2026/10/17 16:46:33 peer not found" and exits 1.
fn no_such_peer(err: &str) -> bool {
    err.contains("peer not found") || err.contains("no match for")
}

/// `tailscale netcheck --format=json`, the parts that explain why a peer
/// is relayed: whether UDP works at all, what kind of NAT this machine is
/// behind, and whether the router will map a port for it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "PascalCase")]
pub struct NetCheck {
    #[serde(rename = "UDP")]
    pub udp: bool,
    #[serde(rename = "IPv4")]
    pub ipv4: bool,
    #[serde(rename = "IPv6")]
    pub ipv6: bool,
    /// `true` is a "hard" NAT: the mapped port differs per destination, so
    /// two hard NATs cannot hole-punch each other. `None` when unknown.
    #[serde(rename = "MappingVariesByDestIP")]
    pub mapping_varies_by_dest_ip: Option<bool>,
    #[serde(rename = "UPnP")]
    pub upnp: bool,
    #[serde(rename = "PMP")]
    pub pmp: bool,
    #[serde(rename = "PCP")]
    pub pcp: bool,
    /// DERP region id; see [`derp_code`].
    #[serde(rename = "PreferredDERP")]
    pub preferred_derp: i32,
}

impl NetCheck {
    /// The summary the host puts in its status and the client shows.
    pub fn report(&self) -> crate::api::NatReport {
        crate::api::NatReport {
            udp: self.udp,
            ipv4: self.ipv4,
            ipv6: self.ipv6,
            hard: self.mapping_varies_by_dest_ip,
            portmap: self.upnp || self.pmp || self.pcp,
            derp: derp_code(self.preferred_derp).to_string(),
        }
    }
}

pub fn parse_netcheck(json: &str) -> Result<NetCheck> {
    serde_json::from_str(json).context("parse tailscale netcheck")
}

/// Run `tailscale netcheck --format=json`. Takes a few seconds: it probes
/// every DERP region. Run it off the UI thread and rarely.
pub fn netcheck() -> Result<NetCheck> {
    let cli = cli().ok_or_else(|| anyhow::anyhow!("Tailscale is not installed"))?;
    let out = run(Command::new(cli).args(["netcheck", "--format=json"]))?;
    parse_netcheck(&out)
}

/// An address Tailscale hands out: IPv4 from 100.64.0.0/10.
pub fn is_tailnet(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

/// [`is_tailnet`], or IPv6 unique-local (Tailscale uses fd7a:115c:a1e0::/48).
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_tailnet(v4),
        IpAddr::V6(v6) => v6.octets()[0] & 0xfe == 0xfc,
    }
}

/// The three-letter code of a DERP region, as `tailscale status` prints it.
pub fn derp_code(region: i32) -> &'static str {
    match region {
        1 => "nyc",
        2 => "sfo",
        3 => "sin",
        4 => "fra",
        5 => "syd",
        6 => "blr",
        7 => "tok",
        8 => "lhr",
        9 => "dfw",
        10 => "sea",
        11 => "sao",
        12 => "ord",
        13 => "den",
        14 => "jnb",
        15 => "mia",
        16 => "lax",
        17 => "par",
        18 => "mad",
        19 => "ams",
        20 => "hkg",
        21 => "tor",
        22 => "waw",
        23 => "nai",
        24 => "hnl",
        25 => "nue",
        26 => "hel",
        27 => "dbi",
        _ => "",
    }
}

/// The city behind a DERP region code, for people.
pub fn derp_city(code: &str) -> &str {
    match code {
        "nyc" => "New York",
        "sfo" => "San Francisco",
        "sin" => "Singapore",
        "fra" => "Frankfurt",
        "syd" => "Sydney",
        "blr" => "Bengaluru",
        "tok" => "Tokyo",
        "lhr" => "London",
        "dfw" => "Dallas",
        "sea" => "Seattle",
        "sao" => "São Paulo",
        "ord" => "Chicago",
        "den" => "Denver",
        "jnb" => "Johannesburg",
        "mia" => "Miami",
        "lax" => "Los Angeles",
        "par" => "Paris",
        "mad" => "Madrid",
        "ams" => "Amsterdam",
        "hkg" => "Hong Kong",
        "tor" => "Toronto",
        "waw" => "Warsaw",
        "nai" => "Nairobi",
        "hnl" => "Honolulu",
        "nue" => "Nuremberg",
        "hel" => "Helsinki",
        "dbi" => "Dubai",
        other => other,
    }
}

fn prepare(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // The binary inside Tailscale.app is the GUI and the CLI in one. It
    // takes the CLI path only when SHLVL is set, its sign of "run from a
    // shell"; an app launched from Finder or the Dock has no SHLVL, and
    // then it tries to start the (already running) GUI, prints "The
    // Tailscale GUI failed to start" on stdout and exits 0.
    cmd.env("SHLVL", "1");
}

fn finish(out: std::process::Output) -> Result<String> {
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{}",
            err.trim().lines().next().unwrap_or("tailscale failed")
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    // The same wrapper reports some failures on stdout with exit code 0.
    if stdout.trim_start().starts_with("The Tailscale GUI") {
        bail!(
            "{}",
            stdout.trim().lines().next().unwrap_or("tailscale failed")
        );
    }
    Ok(stdout)
}

fn run(cmd: &mut Command) -> Result<String> {
    run_limited(cmd, Duration::from_secs(10))
}

fn run_limited(cmd: &mut Command, limit: Duration) -> Result<String> {
    prepare(cmd);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().context("run tailscale")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let read = |mut pipe: Box<dyn Read + Send>| {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    };
    let stdout = std::thread::spawn(move || read(Box::new(stdout)));
    let stderr = std::thread::spawn(move || read(Box::new(stderr)));
    let start = Instant::now();
    let status = loop {
        match child.try_wait().context("run tailscale")? {
            Some(status) => break status,
            None if start.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("tailscale timed out");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let stdout = stdout
        .join()
        .map_err(|_| anyhow::anyhow!("tailscale stdout reader failed"))?;
    let stderr = stderr
        .join()
        .map_err(|_| anyhow::anyhow!("tailscale stderr reader failed"))?;
    finish(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows machines in the list, as the tests below look at them.
    fn windows_peers(st: &Status) -> Vec<&Node> {
        st.machine_peers()
            .into_iter()
            .filter(|n| n.is_windows())
            .collect()
    }

    const SAMPLE: &str = r#"{
      "Version": "1.102.3", "BackendState": "Running",
      "Self": {"ID": "nSELF", "HostName": "Gaming-PC", "DNSName": "gaming-pc.example.ts.net.", "OS": "windows",
               "UserID": 42, "TailscaleIPs": ["100.64.0.10", "fd7a::1"], "Online": true},
      "Peer": {
        "nodekey:a": {"ID": "nMAC", "HostName": "Example Mac", "DNSName": "mac.example.ts.net.", "OS": "macOS",
                      "UserID": 42, "TailscaleIPs": ["100.64.0.20"], "Online": false},
        "nodekey:b": {"ID": "nPC2", "HostName": "Office", "DNSName": "office.example.ts.net.", "OS": "windows",
                      "UserID": 42, "TailscaleIPs": ["fd7a::2", "100.64.0.30"], "Online": true, "CurAddr": "203.0.113.5:41641",
                      "Relay": "lax", "Active": true, "KeyExpiry": "2999-03-02T23:03:00Z"},
        "nodekey:c": {"ID": "nPC1", "HostName": "Den", "DNSName": "den.example.ts.net.", "OS": "windows",
                      "UserID": 43, "TailscaleIPs": ["100.64.0.31"], "Online": false},
        "nodekey:d": {"ID": "nPC3", "HostName": "Gaming-PC-2", "OS": "windows", "UserID": 42,
                      "TailscaleIPs": ["100.64.0.11"], "Online": true, "CurAddr": "", "Relay": "tok", "Active": true}
      },
      "User": {"42": {"ID": 42, "LoginName": "user@example.com", "DisplayName": "Example User"}}
    }"#;

    #[test]
    fn status_is_parsed_and_windows_peers_sorted() {
        let st = parse_status(SAMPLE).unwrap();
        assert!(st.running());
        assert_eq!(st.self_node.ipv4(), Some("100.64.0.10".parse().unwrap()));
        assert_eq!(st.self_login(), Some("user@example.com"));
        let pcs = windows_peers(&st);
        assert_eq!(
            pcs.iter().map(|n| n.host_name.as_str()).collect::<Vec<_>>(),
            ["Den", "Gaming-PC-2", "Office"]
        );
        let machines = st.machine_peers();
        assert_eq!(
            machines
                .iter()
                .map(|n| n.host_name.as_str())
                .collect::<Vec<_>>(),
            ["Den", "Example Mac", "Gaming-PC-2", "Office"],
            "machine_peers includes every OS, not only Windows"
        );
        assert_eq!(st.peer["nodekey:a"].os_label(), "macOS");
        assert_eq!(st.peer["nodekey:b"].os_label(), "Windows");
        assert_eq!(
            pcs[2].ipv4(),
            Some("100.64.0.30".parse().unwrap()),
            "v4 is picked whatever the order"
        );
        assert!(pcs[2].online);
        assert_eq!(pcs[2].public_ipv4(), Some("203.0.113.5".parse().unwrap()));
        assert_eq!(pcs[0].public_ipv4(), None);
        // Office's key expires far off; Den has no expiry (a zero time).
        assert!(pcs[2].key_expiry_days().unwrap() > 300_000);
        assert_eq!(pcs[0].key_expiry_days(), None);
        // Office is reached directly; Gaming-PC-2 only through the Tokyo relay;
        // Den has not been talked to, so nobody knows.
        assert_eq!(pcs[2].direct(), Some(true));
        assert_eq!(pcs[1].direct(), Some(false));
        assert_eq!(pcs[1].relay, "tok");
        assert_eq!(pcs[0].direct(), None);
    }

    #[test]
    fn netcheck_is_parsed_and_summarised() {
        // Trimmed from a real `tailscale netcheck --format=json`.
        let json = r#"{"UDP":true,"IPv4":true,"IPv6":false,"MappingVariesByDestIP":false,
            "HairPinning":null,"UPnP":false,"PMP":false,"PCP":false,"PreferredDERP":7,
            "RegionLatency":{"7":185300000},"GlobalV4":"198.51.100.25:60138","GlobalV6":""}"#;
        let n = parse_netcheck(json).unwrap();
        assert!(n.udp && n.ipv4 && !n.ipv6);
        assert_eq!(n.mapping_varies_by_dest_ip, Some(false));
        let r = n.report();
        assert_eq!(r.hard, Some(false));
        assert!(!r.portmap);
        assert_eq!(r.derp, "tok");
        let hard = parse_netcheck(
            r#"{"UDP":true,"MappingVariesByDestIP":true,"UPnP":true,"PreferredDERP":16}"#,
        )
        .unwrap();
        let r = hard.report();
        assert_eq!(r.hard, Some(true));
        assert!(r.portmap, "UPnP counts as a port mapping");
        assert_eq!(r.derp, "lax");
        assert_eq!(derp_city("lax"), "Los Angeles");
        assert_eq!(derp_city("tok"), "Tokyo");
        assert_eq!(derp_city("zzz"), "zzz");
        assert_eq!(derp_code(999), "");
        let unknown = parse_netcheck(r#"{"UDP":false}"#).unwrap();
        assert_eq!(unknown.mapping_varies_by_dest_ip, None);
        assert_eq!(unknown.report().derp, "");
    }

    #[test]
    fn a_stopped_daemon_is_not_running() {
        let st = parse_status(r#"{"BackendState":"Stopped"}"#).unwrap();
        assert!(!st.running());
        assert!(windows_peers(&st).is_empty());
        assert_eq!(st.self_login(), None);
    }

    #[test]
    fn peer_relay_and_tags_are_parsed_without_changing_direct() {
        // Given: status from Tailscale 1.86+, where a peer-relayed node has an
        // empty CurAddr, a home DERP region *and* a PeerRelay.
        let st = parse_status(
            r#"{"BackendState":"Running","Peer":{
              "nodekey:a": {"ID":"nRELAY","HostName":"sj-instance","OS":"linux","Online":true,
                            "TailscaleIPs":["100.64.0.40"],"Tags":["tag:relay"],
                            "CurAddr":"","Relay":"lax","Active":true,"UnknownFutureField":{"x":1}},
              "nodekey:b": {"ID":"nPC","HostName":"Gaming-PC","OS":"windows","Online":true,
                            "TailscaleIPs":["100.64.0.41"],"CurAddr":"","Relay":"tok","Active":true,
                            "PeerRelay":"100.64.0.40:40000:vni:17"}
            }}"#,
        )
        .unwrap();
        let relay = &st.peer["nodekey:a"];
        let pc = &st.peer["nodekey:b"];

        // Then: the new fields land, unknown fields are ignored, and direct()
        // still calls an empty CurAddr not-direct.
        assert_eq!(relay.tags, ["tag:relay"]);
        assert_eq!(relay.peer_relay, "");
        assert_eq!(pc.peer_relay, "100.64.0.40:40000:vni:17");
        assert!(pc.tags.is_empty());
        assert_eq!(pc.direct(), Some(false));
        assert_eq!(pc.relay, "tok", "Relay is the home DERP, not the path");

        // And: status from an older Tailscale, with neither field, still parses.
        let old = parse_status(SAMPLE).unwrap();
        assert!(old.peer["nodekey:b"].peer_relay.is_empty());
        assert!(old.peer["nodekey:b"].tags.is_empty());
        assert_eq!(old.peer["nodekey:b"].direct(), Some(true));
    }

    #[test]
    fn tagged_relays_stay_out_of_the_pc_list() {
        let st = parse_status(
            r#"{"BackendState":"Running","Peer":{
              "nodekey:relay": {"ID":"nRELAY","HostName":"sj-instance","OS":"linux","Online":true,
                                "TailscaleIPs":["100.64.0.40"],"Tags":["tag:relay"]},
              "nodekey:winrelay": {"ID":"nWINREL","HostName":"relay-pc","OS":"windows","Online":true,
                                   "TailscaleIPs":["100.64.0.42"],"Tags":["tag:relay"]},
              "nodekey:pc": {"ID":"nPC","HostName":"Gaming-PC","OS":"windows","Online":true,
                             "TailscaleIPs":["100.64.0.41"]},
              "nodekey:other": {"ID":"nOTHER","HostName":"tagged-pc","OS":"windows","Online":true,
                                "TailscaleIPs":["100.64.0.43"],"Tags":["tag:other"]}
            }}"#,
        )
        .unwrap();

        let pcs = windows_peers(&st);
        assert_eq!(
            pcs.iter().map(|n| n.host_name.as_str()).collect::<Vec<_>>(),
            ["Gaming-PC", "tagged-pc"],
            "a Windows node carrying tag:relay is excluded, but any other tag stays"
        );
        assert_eq!(
            st.machine_peers()
                .iter()
                .map(|n| n.host_name.as_str())
                .collect::<Vec<_>>(),
            ["Gaming-PC", "tagged-pc"],
            "machine_peers is every OS minus tag:relay"
        );

        let relays = st.relay_peers();
        assert_eq!(
            relays
                .iter()
                .map(|n| n.host_name.as_str())
                .collect::<Vec<_>>(),
            ["relay-pc", "sj-instance"],
            "relay discovery sees every tag:relay node, any OS, sorted"
        );
        assert_eq!(relays[0].os, "windows");
        assert_eq!(relays[1].os, "linux");
    }

    #[test]
    fn peer_relay_servers_is_parsed_and_garbage_is_not() {
        assert_eq!(
            parse_peer_relay_servers("[]"),
            Some(vec![]),
            "a successful empty list is known-empty"
        );
        assert_eq!(
            parse_peer_relay_servers(r#"["100.64.0.40","100.64.0.42"]"#),
            Some(vec!["100.64.0.40".into(), "100.64.0.42".into()])
        );
        assert_eq!(parse_peer_relay_servers("not json"), None);
        assert_eq!(parse_peer_relay_servers(r#"{"addr":"100.64.0.40"}"#), None);
        assert_eq!(parse_peer_relay_servers(""), None);
    }

    #[test]
    fn run_limited_kills_a_hung_command() {
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("sh");
            c.args(["-c", "exec sleep 30"]);
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-Command", "Start-Sleep 30"]);
            c
        };
        let err = run_limited(&mut cmd, Duration::from_millis(100)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn cli_output_larger_than_a_pipe_is_drained_before_waiting() {
        let output = run_limited(
            Command::new("sh").args(["-c", "head -c 1048576 /dev/zero"]),
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(output.len(), 1048576);
    }

    #[test]
    fn tailnet_range_is_recognised() {
        assert!(is_tailnet("100.64.0.1".parse().unwrap()));
        assert!(is_tailnet("100.127.255.254".parse().unwrap()));
        assert!(!is_tailnet("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet("192.168.1.2".parse().unwrap()));
        assert!(is_tailnet_ip("100.111.100.57".parse().unwrap()));
        assert!(is_tailnet_ip("fd7a:115c:a1e0::9e2a:381c".parse().unwrap()));
        assert!(!is_tailnet_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_tailnet_ip("2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn the_tailnet_name_is_read_when_present() {
        let st = parse_status(
            r#"{"BackendState":"Running","CurrentTailnet":{"Name":"user@example.com","MagicDNSSuffix":"tail0.ts.net"}}"#,
        )
        .unwrap();
        assert_eq!(st.current_tailnet.unwrap().name, "user@example.com");
        assert!(parse_status(SAMPLE).unwrap().current_tailnet.is_none());
    }

    #[test]
    fn userspace_networking_is_told_from_a_kernel_tun() {
        let st = parse_status(r#"{"BackendState":"Running","TUN":false}"#).unwrap();
        assert_eq!(st.tun, Some(false));
        let st = parse_status(r#"{"BackendState":"Running","TUN":true}"#).unwrap();
        assert_eq!(st.tun, Some(true));
        assert_eq!(parse_status(SAMPLE).unwrap().tun, None, "older versions");
        assert!(no_such_peer("2026/10/17 16:46:33 peer not found"));
        assert!(no_such_peer("no match for IP:port 127.0.0.1:5"));
        assert!(!no_such_peer("tailscale timed out"));
        assert!(!no_such_peer("failed to connect to local Tailscale daemon"));
    }

    #[test]
    fn whois_is_parsed() {
        let w: WhoIs = serde_json::from_str(
            r#"{"Node":{"Name":"mac.example.ts.net.","ComputedName":"mac"},"UserProfile":{"ID":42,"LoginName":"user@example.com","DisplayName":"Example User"}}"#,
        )
        .unwrap();
        assert_eq!(w.user_profile.id, 42);
        assert_eq!(w.node.computed_name, "mac");
    }
}
