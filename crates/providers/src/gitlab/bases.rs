//! Where a GitLab account's API lives, derived from the web address the user types
//! (spec 5.1): `WEB_BASE/api/v4` ([REST](https://docs.gitlab.com/api/rest/)), relative URL
//! roots included (`https://host/gitlab` -> `https://host/gitlab/api/v4`, UNVERIFIED, follows
//! from [relative URL](https://docs.gitlab.com/install/relative_url/)). Only `https://` is
//! accepted, except plain `http://` on a private or loopback address written literally
//! (`prcore::origin::is_private_host`).

use reqwest::Url;

/// Why a plain-http address was refused.
const HTTP_REFUSED: &str =
    "Use an https:// address (plain http only on a private or local address)";

/// The addresses of one GitLab account. Holds no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitLabBases {
    /// `https://host[:port][/prefix]` (or `http://` on a private address), no trailing slash
    /// (opened in the browser only).
    pub web_base: String,
    pub api_base: String,
    /// `host[:port][/prefix]`, lower-case: the `RepoRef` host (clone URLs live under the
    /// prefix). Git credentials are scoped to the host and port only.
    pub git_host: String,
}

/// The web address of gitlab.com.
pub const GITLAB_COM: &str = "https://gitlab.com";

/// The bases for `web_url` (`https://gitlab.com`, `https://git.corp.example`,
/// `https://git.corp.example/gitlab`). `Err` is a short reason for the form.
pub fn bases_for(web_url: &str) -> Result<GitLabBases, String> {
    let text = web_url.trim().trim_end_matches('/');
    if text.is_empty() {
        return Err("Enter the GitLab web address, e.g. https://gitlab.com".into());
    }
    let lower = text.to_ascii_lowercase();
    // plain http only on a private or loopback address, written literally (never resolved)
    let scheme = if lower.starts_with("https://") {
        "https"
    } else if lower.starts_with("http://") {
        if !prcore::origin::is_private_http_url(text) {
            return Err(HTTP_REFUSED.into());
        }
        "http"
    } else {
        return Err("The address must start with https://".into());
    };
    let url = Url::parse(text).map_err(|_| "This is not a valid web address".to_string())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Leave the user name out of the address".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("Enter only the address of the GitLab site, without ? or #".into());
    }
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or("This address has no host")?
        .to_ascii_lowercase();
    if scheme == "http" && !prcore::origin::is_private_host(&host) {
        return Err(HTTP_REFUSED.into());
    }
    let prefix = url.path().trim_end_matches('/').to_ascii_lowercase();
    let segments_ok = prefix.split('/').skip(1).all(|s| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    });
    if !segments_ok {
        return Err("The address has an odd path; enter the GitLab root address".into());
    }
    if prefix.ends_with("/api/v4") || prefix.contains("/-/") {
        return Err("Enter the GitLab web address, not an API or project address".into());
    }
    let hostport = match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    };
    let git_host = format!("{hostport}{prefix}");
    let web_base = format!("{scheme}://{git_host}");
    Ok(GitLabBases {
        api_base: format!("{web_base}/api/v4"),
        web_base,
        git_host,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitlab_com_bases() {
        for typed in ["https://gitlab.com", " https://GitLab.com/ "] {
            let b = bases_for(typed).unwrap();
            assert_eq!(b.web_base, "https://gitlab.com");
            assert_eq!(b.api_base, "https://gitlab.com/api/v4");
            assert_eq!(b.git_host, "gitlab.com");
        }
    }

    #[test]
    fn relative_url_web_base_derives_api_base() {
        let b = bases_for("https://Git.Corp.Example/GitLab/").unwrap();
        assert_eq!(b.web_base, "https://git.corp.example/gitlab");
        assert_eq!(b.api_base, "https://git.corp.example/gitlab/api/v4");
        assert_eq!(b.git_host, "git.corp.example/gitlab");
        let p = bases_for("https://git.corp.example:8443").unwrap();
        assert_eq!(p.api_base, "https://git.corp.example:8443/api/v4");
        assert_eq!(p.git_host, "git.corp.example:8443");
    }

    #[test]
    fn http_on_a_private_address_keeps_its_scheme() {
        let b = bases_for("http://192.168.1.20:8080/").unwrap();
        assert_eq!(b.web_base, "http://192.168.1.20:8080");
        assert_eq!(b.api_base, "http://192.168.1.20:8080/api/v4");
        assert_eq!(b.git_host, "192.168.1.20:8080");
        let p = bases_for("HTTP://GitLab.Localhost/gitlab").unwrap();
        assert_eq!(p.api_base, "http://gitlab.localhost/gitlab/api/v4");
        assert_eq!(p.git_host, "gitlab.localhost/gitlab");
        let v6 = bases_for("http://[::1]:3000").unwrap();
        assert_eq!(v6.api_base, "http://[::1]:3000/api/v4");
    }

    #[test]
    fn http_on_a_public_or_disguised_address_refused() {
        for bad in [
            "http://gitlab.com",
            "http://gitlab.example",
            "http://8.8.8.8",
            "http://192.168.1.1.evil.com",
            "http://192.168.1.1@evil.com",
            "http://user:pw@192.168.1.1",
            "http://3232235777",
            "http://0300.0250.1.1",
            "http://0xc0.0xa8.1.1",
            "http://[::ffff:192.168.1.1]",
            "http://localhost.evil.com",
        ] {
            let e = bases_for(bad).unwrap_err();
            assert!(e.contains("https") || e.contains("user name"), "{bad}: {e}");
        }
    }

    #[test]
    fn http_and_odd_addresses_refused() {
        assert!(bases_for("http://gitlab.example")
            .unwrap_err()
            .contains("https"));
        for bad in [
            "",
            "gitlab.example",
            "ftp://gitlab.example",
            "https://user:pw@gitlab.example",
            "https://gitlab.example/?x=1",
            "https://gitlab.example/#x",
            "https://gitlab.example/api/v4",
            "https://gitlab.example/grp/proj/-/merge_requests",
            "https://gitlab.example/a%20b",
            "https://",
        ] {
            assert!(bases_for(bad).is_err(), "{bad}");
        }
    }
}
