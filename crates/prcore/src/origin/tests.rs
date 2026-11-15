use super::*;

#[test]
fn private_hosts_table() {
    for ok in [
        "localhost",
        "LocalHost",
        "gitlab.localhost",
        "a.b-c.localhost",
        "127.0.0.1",
        "127.255.255.254",
        "10.0.0.1",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.0.1",
        "192.168.1.20",
        "169.254.1.1",
        "::1",
        "[::1]",
        "0:0:0:0:0:0:0:1",
    ] {
        assert!(is_private_host(ok), "{ok}");
    }
    for bad in [
        "",
        "gitlab.com",
        "8.8.8.8",
        "1.1.1.1",
        "172.15.255.255",
        "172.32.0.1",
        "192.169.0.1",
        "169.253.1.1",
        "11.0.0.1",
        "0.0.0.0",
        "255.255.255.255",
        // names that only look private
        "192.168.1.1.evil.com",
        "192.168.1.1.",
        "10.0.0.1.nip.io",
        "localhost.evil.com",
        "localhost.",
        "evillocalhost",
        ".localhost",
        "a..localhost",
        "-a.localhost",
        "a_b.localhost",
        // non-canonical IPv4 forms (a URL parser would turn them into 192.168.1.1)
        "3232235777",
        "0300.0250.1.1",
        "0xc0.0xa8.1.1",
        "0xc0a80101",
        "192.168.1",
        "192.168.001.1",
        "192.168.1.01",
        "192.168.1.256",
        "192.168.1.1:8080",
        "192.168.1.1@evil.com",
        "user@192.168.1.1",
        " 192.168.1.1",
        "192.168.1.1 ",
        "１９２.168.1.1",
        // IPv6 other than loopback, mapped and compatible forms, zones
        "::",
        "::2",
        "fe80::1",
        "fc00::1",
        "fd00::1",
        "::ffff:127.0.0.1",
        "::ffff:192.168.1.1",
        "[::ffff:7f00:1]",
        "::127.0.0.1",
        "::1%eth0",
        "[::1",
        "::1]",
        "[[::1]]",
    ] {
        assert!(!is_private_host(bad), "{bad}");
    }
}

#[test]
fn authority_split() {
    assert_eq!(split_authority("h"), Some(("h", None)));
    assert_eq!(split_authority("h:8080"), Some(("h", Some(8080))));
    assert_eq!(split_authority("[::1]:80"), Some(("[::1]", Some(80))));
    assert_eq!(split_authority("[::1]"), Some(("[::1]", None)));
    for bad in [
        "", "h:", "h:x", "h:99999", "h:+80", "u@h", ":80", "[::1]x", "[::1:80", "h:80:80",
    ] {
        assert_eq!(split_authority(bad), None, "{bad}");
    }
}

#[test]
fn private_http_urls() {
    for ok in [
        "http://192.168.1.20:8080",
        "HTTP://192.168.1.20:8080/gitlab/api/v4",
        "http://localhost",
        "http://[::1]:3000/x",
        "http://git.localhost/?x",
    ] {
        assert!(is_private_http_url(ok), "{ok}");
        assert!(transport_allowed(ok), "{ok}");
    }
    for bad in [
        "http://gitlab.com",
        "http://8.8.8.8",
        "http://192.168.1.1.evil.com",
        "http://192.168.1.1@evil.com",
        "http://evil.com@192.168.1.1",
        "http://3232235777",
        "http://0300.0250.1.1",
        "http://[::ffff:192.168.1.1]",
        "http://192.168.1.1:x",
        "http://",
        "https://192.168.1.1",
        "ftp://192.168.1.1",
        "192.168.1.1",
    ] {
        assert!(!is_private_http_url(bad), "{bad}");
    }
    assert!(transport_allowed("https://gitlab.com/api/v4"));
    assert!(!transport_allowed("http://gitlab.com/api/v4"));
    assert!(!transport_allowed("ftp://x"));
}
