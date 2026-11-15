//! Which forge addresses may be reached over plain `http://`: only private and loopback
//! hosts, written literally. Host names are never resolved; only `localhost`, `*.localhost`
//! and literal IPv4 / IPv6 addresses in the private, loopback and link-local ranges qualify.

/// True when `host` (no port, no brackets needed for IPv6) is a private or loopback address
/// that a plain-http forge account may use: `localhost`, `*.localhost`, `127.0.0.0/8`,
/// `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` and `::1`.
///
/// Only the canonical dotted-quad form of an IPv4 address counts (no decimal, octal, hex or
/// shortened forms), and no IPv6 address other than loopback (an IPv4-mapped one neither).
pub fn is_private_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" {
        return true;
    }
    if let Some(sub) = host.strip_suffix(".localhost") {
        return sub.split('.').all(is_dns_label);
    }
    if let Some(v4) = dotted_quad(&host) {
        return matches!(v4, [127, ..] | [10, ..] | [192, 168, ..] | [169, 254, ..])
            || (v4[0] == 172 && (16..=31).contains(&v4[1]));
    }
    let inner = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&host);
    // only hex digits and colons: no zone id, no embedded IPv4 (mapped or compatible)
    inner.contains(':')
        && inner.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
        && inner
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|a| a.is_loopback())
}

/// The host and port of an authority (`host`, `host:8080`, `[::1]:8080`); None when it has
/// user info, an empty host or a port that is not a number.
pub fn split_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    if authority.contains('@') || authority.is_empty() {
        return None;
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let (h, rest) = authority.split_at(end + 1);
        match rest {
            "" => (h, None),
            r => (h, Some(r.strip_prefix(':')?)),
        }
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        None => None,
        Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => {
            Some(p.parse::<u16>().ok()?)
        }
        Some(_) => return None,
    };
    (!host.is_empty()).then_some((host, port))
}

/// True when `url` is `http://<private host>[:port][/...]` (see `is_private_host`): the only
/// plain-http addresses a forge account may use.
pub fn is_private_http_url(url: &str) -> bool {
    let Some(rest) = strip_scheme(url, "http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    split_authority(authority).is_some_and(|(h, _)| is_private_host(h))
}

/// True when a forge API or web address may carry the token: `https://`, or plain
/// `http://` on a private host.
pub fn transport_allowed(url: &str) -> bool {
    strip_scheme(url, "https://").is_some() || is_private_http_url(url)
}

/// The warning shown wherever a plain-http account is used.
pub const CLEARTEXT_WARNING: &str =
    "Unencrypted connection: the token is sent in clear text on your network";

fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let n = scheme.len();
    (url.len() > n && url.is_char_boundary(n) && url[..n].eq_ignore_ascii_case(scheme))
        .then(|| &url[n..])
}

fn is_dns_label(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= 63
        && !l.starts_with('-')
        && !l.ends_with('-')
        && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The four octets of a canonical dotted quad (`192.168.1.1`), None for anything else
/// (`3232235777`, `0300.0250.1.1`, `0xc0.168.1.1`, `192.168.1`, `192.168.001.1`).
fn dotted_quad(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for o in &mut out {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if p.len() > 1 && p.starts_with('0') {
            return None;
        }
        *o = p.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

#[cfg(test)]
mod tests;
