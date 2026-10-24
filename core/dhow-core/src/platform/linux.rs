//! The network half of a Linux adapter (ARCHITECTURE §8), shared by the CLI
//! and the GTK shell. Proxy: the environment first, then the desktop's
//! setting (GNOME `gsettings`, KDE `kioslaverc`). Interfaces from
//! `/sys/class/net`, the default route from `/proc/net/route`.
//!
//! Every input is a field, so tests feed it maps, fake directories and
//! canned output; the GTK shell should set `gsettings` to a `gio::Settings`
//! reader instead of the subprocess. It reads on query only and blocks
//! (up to [`GSETTINGS_DEADLINE`]): async callers use `spawn_blocking`.
//! `watch_network` is unsupported for now (it needs netlink or
//! NetworkManager's D-Bus signals).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::Url;

use super::{AdapterError, InterfaceKind, NetworkInfo, PlatformAdapter, ProxyConfig, ProxyMode};

type Lookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;
type Fetch = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// A wedged dconf/D-Bus must not hang a query.
pub const GSETTINGS_DEADLINE: Duration = Duration::from_secs(1);

/// `XDG_CURRENT_DESKTOP` names whose settings panels write
/// `org.gnome.system.proxy` (GIO's GNOME resolver keys on `GNOME`; Budgie,
/// Pop and Ubuntu list it too).
const GNOME_DESKTOPS: [&str; 6] = [
    "GNOME",
    "Unity",
    "X-Cinnamon",
    "Cinnamon",
    "Pantheon",
    "Budgie",
];

pub struct LinuxAdapter {
    /// An environment variable.
    pub env: Lookup,
    /// `gsettings list-recursively org.gnome.system.proxy`.
    pub gsettings: Option<Fetch>,
    pub kioslaverc: Option<PathBuf>,
    pub sys_class_net: Option<PathBuf>,
    pub proc_net_route: Option<PathBuf>,
}

impl LinuxAdapter {
    pub fn host() -> Self {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")));
        Self {
            gsettings: Some(Box::new(gsettings_subprocess)),
            kioslaverc: config_home.map(|c| c.join("kioslaverc")),
            sys_class_net: Some("/sys/class/net".into()),
            proc_net_route: Some("/proc/net/route".into()),
            ..Self::env_only()
        }
    }

    /// The process environment only: for Unixes whose shell adapter
    /// doesn't exist yet (the CLI on macOS).
    pub fn env_only() -> Self {
        Self {
            env: Box::new(|name| std::env::var(name).ok()),
            gsettings: None,
            kioslaverc: None,
            sys_class_net: None,
            proc_net_route: None,
        }
    }

    /// `None`: direct, or the desktop's setting couldn't be read.
    pub fn proxy(&self) -> Option<ProxyConfig> {
        self.try_proxy().ok().flatten()
    }

    /// `Ok(None)`: direct. `Err`: gsettings failed or timed out, or
    /// `kioslaverc` exists but can't be read; not to be taken as direct.
    pub fn try_proxy(&self) -> Result<Option<ProxyConfig>, AdapterError> {
        if let Some(c) = env_proxy(&self.env) {
            return Ok(Some(c));
        }
        // Other desktops and headless: environment only, so a stale
        // dconf value doesn't count and nothing is forked.
        let desktop = (self.env)("XDG_CURRENT_DESKTOP").unwrap_or_default();
        let is = |names: &[&str]| {
            desktop
                .split(':')
                .any(|d| names.iter().any(|n| d.eq_ignore_ascii_case(n)))
        };
        if is(&["KDE"]) {
            let Some(path) = &self.kioslaverc else {
                return Ok(None);
            };
            match fs::read_to_string(path) {
                Ok(text) => Ok(kde_proxy(&text, &self.env)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(AdapterError::Failed(format!("kioslaverc: {e}"))),
            }
        } else if is(&GNOME_DESKTOPS) {
            let Some(fetch) = &self.gsettings else {
                return Ok(None);
            };
            match fetch() {
                Some(listing) => Ok(gnome_proxy(&listing)),
                None => Err(AdapterError::Failed("gsettings did not answer".into())),
            }
        } else {
            Ok(None)
        }
    }

    fn interfaces(&self) -> (InterfaceKind, bool) {
        let Some(dir) = &self.sys_class_net else {
            return (InterfaceKind::Unknown, false);
        };
        let ifaces = read_interfaces(dir);
        let vpn_active = ifaces.iter().any(|i| i.kind == InterfaceKind::Vpn);
        let route = self
            .proc_net_route
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|t| default_route(&t));
        let by_route = route.and_then(|r| ifaces.iter().find(|i| i.name == r));
        let kind = by_route.map(|i| i.kind).unwrap_or_else(|| {
            [
                InterfaceKind::Ethernet,
                InterfaceKind::Wifi,
                InterfaceKind::Cellular,
            ]
            .into_iter()
            .find(|k| ifaces.iter().any(|i| i.kind == *k))
            .unwrap_or(InterfaceKind::Unknown)
        });
        (kind, vpn_active)
    }
}

impl PlatformAdapter for LinuxAdapter {
    /// The same for every URL: the core doesn't evaluate PAC.
    fn system_proxy(&self, _for_url: &Url) -> Result<ProxyConfig, AdapterError> {
        Ok(self.try_proxy()?.unwrap_or_default())
    }

    fn network_info(&self) -> Result<NetworkInfo, AdapterError> {
        let (interface_kind, vpn_active) = self.interfaces();
        Ok(NetworkInfo {
            interface_kind,
            vpn_active,
            system_proxy: self.proxy(),
            proxy_in_use: ProxyMode::None,
        })
    }
}

/// Killed after [`GSETTINGS_DEADLINE`]. Its output is a few hundred bytes,
/// well under a pipe buffer, so it can't block on a full stdout first.
fn gsettings_subprocess() -> Option<String> {
    let mut child = Command::new("gsettings")
        .args(["list-recursively", "org.gnome.system.proxy"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + GSETTINGS_DEADLINE;
    // Polled only for this query's ≤ 1 s, never while idle.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `http_proxy` and friends, lowercase first (curl's order).
fn env_proxy(env: &Lookup) -> Option<ProxyConfig> {
    let get = |name: &str| {
        env(name)
            .or_else(|| env(&name.to_ascii_uppercase()))
            .filter(|v| !v.trim().is_empty())
    };
    let config = ProxyConfig {
        http: get("http_proxy").and_then(|v| proxy_url(&v, "http")),
        https: get("https_proxy").and_then(|v| proxy_url(&v, "http")),
        all: get("all_proxy").and_then(|v| proxy_url(&v, "http")),
        no_proxy: get("no_proxy").map_or_else(Vec::new, |v| split_hosts(&v)),
        pac: None,
    };
    (!config.is_direct()).then_some(config)
}

/// `host:port` gets `scheme://`; `socks://` means SOCKS5.
fn proxy_url(value: &str, scheme: &str) -> Option<Url> {
    let value = value.trim();
    let url = match value.split_once("://") {
        Some(("socks", rest)) => format!("socks5://{rest}"),
        Some(_) => value.to_owned(),
        None => format!("{scheme}://{value}"),
    };
    Url::parse(&url).ok().filter(|u| u.host_str().is_some())
}

fn split_hosts(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .collect()
}

/// From `gsettings list-recursively org.gnome.system.proxy`: lines of
/// `schema key value`, values in GVariant text form.
fn gnome_proxy(listing: &str) -> Option<ProxyConfig> {
    let values: HashMap<(&str, &str), &str> = listing
        .lines()
        .filter_map(|l| {
            let (schema, rest) = l.trim().split_once(' ')?;
            let (key, value) = rest.split_once(' ')?;
            Some(((schema, key), value.trim()))
        })
        .collect();
    let get = |schema: &str, key: &str| {
        let full = if schema.is_empty() {
            "org.gnome.system.proxy".to_owned()
        } else {
            format!("org.gnome.system.proxy.{schema}")
        };
        values.get(&(full.as_str(), key)).copied()
    };
    let endpoint = |schema: &str, scheme: &str| {
        let host = gvariant_strings(get(schema, "host")?)?.pop()?;
        let port: u16 = get(schema, "port")?.parse().ok().filter(|p| *p != 0)?;
        Url::parse(&format!("{scheme}://{host}:{port}")).ok()
    };
    let config = match gvariant_strings(get("", "mode")?)?.pop()?.as_str() {
        "manual" => ProxyConfig {
            http: endpoint("http", "http"),
            https: endpoint("https", "http"),
            all: endpoint("socks", "socks5"),
            no_proxy: get("", "ignore-hosts")
                .and_then(gvariant_strings)
                .unwrap_or_default(),
            pac: None,
        },
        "auto" => ProxyConfig {
            pac: wpad_or(gvariant_strings(get("", "autoconfig-url")?)?.pop()),
            ..ProxyConfig::default()
        },
        _ => return None,
    };
    (!config.is_direct()).then_some(config)
}

/// `'one'`, `['a', 'b']` or `@as []` as strings.
fn gvariant_strings(value: &str) -> Option<Vec<String>> {
    let value = value.strip_prefix("@as ").unwrap_or(value).trim();
    let inner = match value.strip_prefix('[') {
        Some(rest) => rest.strip_suffix(']')?,
        None => value,
    };
    let mut out = Vec::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        let quote = match c {
            '\'' | '"' => c,
            ',' | ' ' => continue,
            _ => return None,
        };
        let mut s = String::new();
        loop {
            match chars.next()? {
                '\\' => s.push(chars.next()?),
                c if c == quote => break,
                c => s.push(c),
            }
        }
        out.push(s);
    }
    Some(out)
}

/// An empty script URL means WPAD's well-known one.
fn wpad_or(url: Option<String>) -> Option<Url> {
    let url = url.filter(|u| !u.is_empty());
    Url::parse(url.as_deref().unwrap_or("http://wpad/wpad.dat")).ok()
}

/// `[Proxy Settings]` of `kioslaverc`. `ProxyType`: 1 manual, 2 script,
/// 3 WPAD, 4 the named environment variables.
fn kde_proxy(text: &str, env: &Lookup) -> Option<ProxyConfig> {
    let mut section = "";
    let mut values = HashMap::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
        } else if section == "Proxy Settings"
            && let Some((key, value)) = line.split_once('=')
        {
            values.insert(key.trim(), value.trim());
        }
    }
    let get = |key: &str| values.get(key).copied().filter(|v| !v.is_empty());
    let config = match get("ProxyType")? {
        t @ ("1" | "4") => {
            let field = |key: &str, scheme: &str| {
                let v = get(key)?;
                let v = if t == "4" { env(v)? } else { v.to_owned() };
                // KDE writes `http://host 8080`.
                proxy_url(&v.trim().replacen(' ', ":", 1), scheme)
            };
            let no_proxy = get("NoProxyFor").map(|v| {
                if t == "4" {
                    env(v).unwrap_or_default()
                } else {
                    v.to_owned()
                }
            });
            ProxyConfig {
                http: field("httpProxy", "http"),
                https: field("httpsProxy", "http"),
                all: field("socksProxy", "socks5"),
                no_proxy: no_proxy.map_or_else(Vec::new, |v| split_hosts(&v)),
                pac: None,
            }
        }
        "2" => ProxyConfig {
            pac: Url::parse(get("Proxy Config Script")?).ok(),
            ..ProxyConfig::default()
        },
        "3" => ProxyConfig {
            pac: wpad_or(None),
            ..ProxyConfig::default()
        },
        _ => return None,
    };
    (!config.is_direct()).then_some(config)
}

#[derive(Debug)]
struct Interface {
    name: String,
    kind: InterfaceKind,
}

const ARPHRD_ETHER: u32 = 1;
const ARPHRD_PPP: u32 = 512;
const ARPHRD_RAWIP: u32 = 519;
/// tun and WireGuard devices.
const ARPHRD_NONE: u32 = 65534;

/// Interfaces that are up, except loopback and virtual ones (bridges,
/// veths) that aren't VPNs.
fn read_interfaces(dir: &Path) -> Vec<Interface> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Interface> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let read = |f: &str| fs::read_to_string(path.join(f)).ok();
            // tun and WireGuard report `unknown` while up.
            let state = read("operstate")?;
            if !matches!(state.trim(), "up" | "unknown") {
                return None;
            }
            let arp: u32 = read("type")?.trim().parse().ok()?;
            // Hints, not proof: PPP is also DSL, tap also VM networking.
            let vpn = ["tun", "tap", "wg", "ppp"]
                .iter()
                .any(|p| name.starts_with(p))
                || arp == ARPHRD_NONE
                || arp == ARPHRD_PPP;
            let kind = if vpn {
                InterfaceKind::Vpn
            } else if path.join("wireless").exists() || path.join("phy80211").exists() {
                InterfaceKind::Wifi
            } else if name.starts_with("wwan")
                || arp == ARPHRD_RAWIP
                || read("uevent").is_some_and(|u| u.lines().any(|l| l == "DEVTYPE=wwan"))
            {
                InterfaceKind::Cellular
            } else if arp == ARPHRD_ETHER && path.join("device").exists() {
                InterfaceKind::Ethernet
            } else {
                return None;
            };
            Some(Interface { name, kind })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The interface of the IPv4 default route with the lowest metric.
fn default_route(table: &str) -> Option<String> {
    const RTF_UP: u32 = 1;
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let flags = u32::from_str_radix(f.get(3)?, 16).ok()?;
            let metric: u32 = f.get(6)?.parse().ok()?;
            (*f.get(1)? == "00000000" && *f.get(7)? == "00000000" && flags & RTF_UP != 0)
                .then(|| (metric, f[0].to_owned()))
        })
        .min()
        .map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> Lookup {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Box::new(move |k| map.get(k).cloned())
    }

    fn adapter(vars: &[(&str, &str)]) -> LinuxAdapter {
        LinuxAdapter {
            env: env(vars),
            ..LinuxAdapter::env_only()
        }
    }

    fn url(s: &str) -> Option<Url> {
        Some(Url::parse(s).unwrap())
    }

    #[test]
    fn env_lowercase_wins_and_scheme_defaults_to_http() {
        let a = adapter(&[
            ("https_proxy", "proxy.test:3128"),
            ("HTTPS_PROXY", "http://upper.test:1"),
            ("HTTP_PROXY", "http://upper.test:2"),
            ("all_proxy", "socks://s.test:1080"),
            ("NO_PROXY", "localhost, .corp.example,,10.0.0.0/8"),
        ]);
        assert_eq!(
            a.proxy(),
            Some(ProxyConfig {
                http: url("http://upper.test:2"),
                https: url("http://proxy.test:3128"),
                all: url("socks5://s.test:1080"),
                no_proxy: vec![
                    "localhost".into(),
                    ".corp.example".into(),
                    "10.0.0.0/8".into()
                ],
                pac: None,
            })
        );
    }

    #[test]
    fn empty_env_is_direct_and_no_proxy_alone_is_not_a_proxy() {
        assert_eq!(adapter(&[("http_proxy", " ")]).proxy(), None);
        assert_eq!(adapter(&[("no_proxy", "*")]).proxy(), None);
        let info = adapter(&[]).network_info().unwrap();
        assert_eq!(info.interface_kind, InterfaceKind::Unknown);
        assert!(!info.vpn_active);
        assert_eq!(info.system_proxy, None);
    }

    const GNOME_MANUAL: &str = "\
org.gnome.system.proxy autoconfig-url ''
org.gnome.system.proxy ignore-hosts ['localhost', '127.0.0.0/8', 'it\\'s.example']
org.gnome.system.proxy mode 'manual'
org.gnome.system.proxy use-same-proxy true
org.gnome.system.proxy.ftp host ''
org.gnome.system.proxy.ftp port 0
org.gnome.system.proxy.http enabled false
org.gnome.system.proxy.http host 'web.test'
org.gnome.system.proxy.http port 8080
org.gnome.system.proxy.https host 'secure.test'
org.gnome.system.proxy.https port 8443
org.gnome.system.proxy.socks host ''
org.gnome.system.proxy.socks port 0
";

    fn gnome(listing: &'static str) -> LinuxAdapter {
        LinuxAdapter {
            gsettings: Some(Box::new(move || Some(listing.to_owned()))),
            ..adapter(&[("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")])
        }
    }

    #[test]
    fn gnome_manual() {
        assert_eq!(
            gnome(GNOME_MANUAL).proxy(),
            Some(ProxyConfig {
                http: url("http://web.test:8080"),
                https: url("http://secure.test:8443"),
                all: None,
                no_proxy: vec![
                    "localhost".into(),
                    "127.0.0.0/8".into(),
                    "it's.example".into()
                ],
                pac: None,
            })
        );
    }

    #[test]
    fn gnome_auto_none_and_env_first() {
        let auto = "org.gnome.system.proxy autoconfig-url 'http://cfg.test/p.pac'\n\
                    org.gnome.system.proxy ignore-hosts @as []\n\
                    org.gnome.system.proxy mode 'auto'\n";
        assert_eq!(
            gnome(auto).proxy().unwrap().pac,
            url("http://cfg.test/p.pac")
        );
        let wpad = "org.gnome.system.proxy autoconfig-url ''\norg.gnome.system.proxy mode 'auto'\n";
        assert_eq!(
            gnome(wpad).proxy().unwrap().pac,
            url("http://wpad/wpad.dat")
        );
        let none = GNOME_MANUAL.replace("mode 'manual'", "mode 'none'");
        assert_eq!(gnome(none.leak()).proxy(), None);
        let a = LinuxAdapter {
            env: env(&[("http_proxy", "http://env.test:1")]),
            ..gnome(GNOME_MANUAL)
        };
        assert_eq!(a.proxy().unwrap().http, url("http://env.test:1"));
        // gsettings missing or failing.
        let a = LinuxAdapter {
            gsettings: Some(Box::new(|| None)),
            ..adapter(&[("XDG_CURRENT_DESKTOP", "GNOME")])
        };
        assert_eq!(a.proxy(), None);
        // Unknown, not direct: the engine mustn't bypass a proxy for it.
        let any = Url::parse("http://x/").unwrap();
        assert!(matches!(a.system_proxy(&any), Err(AdapterError::Failed(_))));
    }

    #[test]
    fn gsettings_only_on_gnome_desktops() {
        for desktop in ["sway", "XFCE", ""] {
            let a = LinuxAdapter {
                gsettings: Some(Box::new(|| panic!("asked gsettings"))),
                ..adapter(&[("XDG_CURRENT_DESKTOP", desktop)])
            };
            assert_eq!(a.proxy(), None, "{desktop}");
        }
        let a = LinuxAdapter {
            gsettings: Some(Box::new(|| panic!("asked gsettings"))),
            ..adapter(&[])
        };
        assert_eq!(a.proxy(), None);
        for desktop in ["X-Cinnamon", "Budgie:GNOME", "pop:GNOME", "Unity"] {
            let a = LinuxAdapter {
                env: env(&[("XDG_CURRENT_DESKTOP", desktop)]),
                ..gnome(GNOME_MANUAL)
            };
            assert_eq!(
                a.proxy().unwrap().http,
                url("http://web.test:8080"),
                "{desktop}"
            );
        }
    }

    fn kde(rc: &str, vars: &[(&str, &str)]) -> Option<ProxyConfig> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kioslaverc");
        fs::write(&path, rc).unwrap();
        let mut vars = vars.to_vec();
        vars.push(("XDG_CURRENT_DESKTOP", "KDE"));
        let a = LinuxAdapter {
            kioslaverc: Some(path),
            // A KDE desktop doesn't ask GNOME.
            gsettings: Some(Box::new(|| Some(GNOME_MANUAL.to_owned()))),
            ..adapter(&vars)
        };
        a.proxy()
    }

    #[test]
    fn kde_manual_script_and_env() {
        let manual = "[Cache]\nProxyType=9\n\n[Proxy Settings]\nProxyType=1\n\
                      httpProxy=http://web.test 8080\nhttpsProxy=http://secure.test:8443\n\
                      socksProxy=socks://s.test 1080\nftpProxy=\nNoProxyFor=localhost,.lan\n";
        assert_eq!(
            kde(manual, &[]),
            Some(ProxyConfig {
                http: url("http://web.test:8080"),
                https: url("http://secure.test:8443"),
                all: url("socks5://s.test:1080"),
                no_proxy: vec!["localhost".into(), ".lan".into()],
                pac: None,
            })
        );
        let script = "[Proxy Settings]\nProxyType=2\nProxy Config Script=http://cfg.test/p.pac\n";
        assert_eq!(kde(script, &[]).unwrap().pac, url("http://cfg.test/p.pac"));
        let from_env = "[Proxy Settings]\nProxyType=4\nhttpProxy=MY_PROXY\nNoProxyFor=MY_NO\n";
        let c = kde(from_env, &[("MY_PROXY", "kde.test:3128"), ("MY_NO", "a,b")]).unwrap();
        assert_eq!(c.http, url("http://kde.test:3128"));
        assert_eq!(c.no_proxy, ["a", "b"]);
        assert_eq!(kde("[Proxy Settings]\nProxyType=0\n", &[]), None);
    }

    /// A fake `/sys/class/net` entry.
    fn iface(net: &Path, name: &str, state: &str, arp: u32, extra: &[&str]) {
        let dir = net.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("operstate"), format!("{state}\n")).unwrap();
        fs::write(dir.join("type"), format!("{arp}\n")).unwrap();
        for e in extra {
            fs::create_dir(dir.join(e)).unwrap();
        }
    }

    fn sys(net: &Path, route: Option<&str>) -> (InterfaceKind, bool) {
        let route_file = net.parent().unwrap().join("route");
        match route {
            Some(r) => fs::write(&route_file, r).unwrap(),
            None => _ = fs::remove_file(&route_file),
        }
        let a = LinuxAdapter {
            sys_class_net: Some(net.to_owned()),
            proc_net_route: Some(route_file),
            ..adapter(&[])
        };
        let info = a.network_info().unwrap();
        (info.interface_kind, info.vpn_active)
    }

    const ROUTE_HEADER: &str =
        "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n";

    fn route(rows: &[(&str, &str, u32)]) -> String {
        let mut t = ROUTE_HEADER.to_owned();
        for (iface, dest, metric) in rows {
            t += &format!("{iface}\t{dest}\t0101A8C0\t0003\t0\t0\t{metric}\t00000000\t0\t0\t0\n");
        }
        t
    }

    #[test]
    fn interface_kind_follows_the_default_route() {
        let root = tempfile::tempdir().unwrap();
        let net = root.path().join("net");
        iface(&net, "lo", "unknown", 772, &[]);
        iface(&net, "docker0", "up", 1, &[]);
        iface(&net, "enp3s0", "up", 1, &["device"]);
        iface(&net, "wlp2s0", "up", 1, &["device", "wireless"]);
        iface(&net, "wg0", "down", 65534, &[]);
        let wifi_route = route(&[
            ("enp3s0", "0000A8C0", 0),
            ("wlp2s0", "00000000", 600),
            ("enp3s0", "00000000", 700),
        ]);
        assert_eq!(sys(&net, Some(&wifi_route)), (InterfaceKind::Wifi, false));
        // No route table: physical by preference.
        assert_eq!(sys(&net, None), (InterfaceKind::Ethernet, false));

        // A VPN that's up but not the default route (wg-quick's policy routing).
        iface(&net, "proton0", "unknown", 65534, &[]);
        assert_eq!(sys(&net, Some(&wifi_route)), (InterfaceKind::Wifi, true));
        iface(&net, "tun0", "unknown", 65534, &[]);
        let vpn_route = route(&[("tun0", "00000000", 0), ("wlp2s0", "00000000", 600)]);
        assert_eq!(sys(&net, Some(&vpn_route)), (InterfaceKind::Vpn, true));
    }

    #[test]
    fn ppp_and_tap_hint_a_vpn() {
        let root = tempfile::tempdir().unwrap();
        let net = root.path().join("net");
        iface(&net, "wlp2s0", "up", 1, &["device", "wireless"]);
        iface(&net, "vpn-office", "unknown", 512, &[]);
        assert_eq!(sys(&net, None), (InterfaceKind::Wifi, true));
        fs::remove_dir_all(net.join("vpn-office")).unwrap();
        iface(&net, "tap0", "up", 1, &[]);
        let route = route(&[("tap0", "00000000", 0)]);
        assert_eq!(sys(&net, Some(&route)), (InterfaceKind::Vpn, true));
        fs::remove_dir_all(net.join("tap0")).unwrap();
        iface(&net, "ppp0", "unknown", 512, &[]);
        assert_eq!(sys(&net, None), (InterfaceKind::Wifi, true));
    }

    #[test]
    fn cellular_and_nothing_up() {
        let root = tempfile::tempdir().unwrap();
        let net = root.path().join("net");
        iface(&net, "enp3s0", "down", 1, &["device"]);
        iface(&net, "wwan0", "up", 519, &["device"]);
        assert_eq!(sys(&net, None), (InterfaceKind::Cellular, false));
        fs::write(net.join("wwan0/operstate"), "dormant\n").unwrap();
        assert_eq!(
            sys(&net, Some(ROUTE_HEADER)),
            (InterfaceKind::Unknown, false)
        );
        assert_eq!(
            sys(&root.path().join("missing"), None),
            (InterfaceKind::Unknown, false)
        );
    }

    #[test]
    fn gvariant_string_forms() {
        assert_eq!(gvariant_strings("'a'"), Some(vec!["a".into()]));
        assert_eq!(gvariant_strings("@as []"), Some(vec![]));
        assert_eq!(gvariant_strings("[]"), Some(vec![]));
        assert_eq!(
            gvariant_strings(r#"['x', "y'z", 'b\\c']"#),
            Some(vec!["x".into(), "y'z".into(), "b\\c".into()])
        );
        assert_eq!(gvariant_strings("8080"), None);
        assert_eq!(gvariant_strings("'open"), None);
    }
}
