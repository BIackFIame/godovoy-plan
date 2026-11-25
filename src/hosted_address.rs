//! Accept only a web origin. Pass the checked text unchanged to the system browser.
use std::net::{Ipv4Addr, Ipv6Addr};

pub fn valid_hosted_address(input: &str) -> bool {
    if input.len() > 300 {
        return false;
    }
    let (tls, authority) = if let Some(rest) = input.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = input.strip_prefix("http://") {
        (false, rest)
    } else {
        return false;
    };
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    let (host, port, loopback) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, suffix)) = rest.split_once(']') else {
            return false;
        };
        let Ok(ip) = host.parse::<Ipv6Addr>() else {
            return false;
        };
        let port = if suffix.is_empty() {
            None
        } else if let Some(port) = suffix.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        };
        (host, port, ip.is_loopback())
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(h, p)| (h, Some(p)));
        if !valid_dns_or_ipv4(host) {
            return false;
        }
        (host, port, host == "127.0.0.1")
    };
    !host.is_empty()
        && (tls || loopback)
        && port.is_none_or(|s| {
            !s.is_empty()
                && s.bytes().all(|c| c.is_ascii_digit())
                && s.parse::<u16>().is_ok_and(|p| p != 0)
        })
}

fn valid_dns_or_ipv4(host: &str) -> bool {
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let last = host.rsplit('.').next().unwrap_or_default();
    // Browsers interpret abbreviated, hexadecimal and octal IPv4 differently
    // from DNS and std::net. Accept IPv4 only in its ordinary four-part form.
    if last.bytes().all(|c| c.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .or_else(|| last.strip_prefix("0X"))
            .is_some_and(|s| s.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return false;
    }
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::valid_hosted_address;
    #[test]
    fn allows_company_tls_and_local_tunnel_origins_only() {
        for s in [
            "https://calendar.example.org",
            "https://OFFICE:8443/",
            "https://10.0.0.4",
            "https://[fd00::2]:443",
            "http://127.0.0.1:9000",
            "http://[::1]:9000/",
        ] {
            assert!(valid_hosted_address(s), "{s}");
        }
        for s in [
            "",
            "http://company.local",
            "http://localhost:9000",
            "http://127.0.0.2",
            "http://127.0.0.1.evil.test",
            "http://2130706433",
            "http://[::ffff:127.0.0.1]",
            "https://127.1",
            "https://0x7f000001",
            "https://0177.0.0.1",
            "https://bad..test",
            "https://user:secret@example.com",
            "https://example.com@127.0.0.1",
            "https://example.org?",
            "https://host/#",
            "https://host/path",
            "https://host/%2f",
            "https://host\\evil",
            "https://host:0",
            "https://host:",
            "https://host:65536",
            "https://host%2eexample",
            "https://host\n",
            "https://host\0evil",
            "javascript:alert(1)",
            "file:///tmp/foo",
        ] {
            assert!(!valid_hosted_address(s), "{s:?}");
        }
    }
}
