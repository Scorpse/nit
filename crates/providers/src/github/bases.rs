//! Where a GitHub account's APIs live, derived from the web address the user types
//! (spec 5.1): github.com, GitHub Enterprise Server (`https://HOST/api/v3`,
//! `https://HOST/api/graphql`) or GHE.com data residency (`https://api.SUB.ghe.com`,
//! UNVERIFIED). Only `https://` is accepted, except plain `http://` on a private or
//! loopback address written literally (`prcore::origin::is_private_host`).

use reqwest::Url;

/// Why a plain-http address was refused.
const HTTP_REFUSED: &str =
    "Use an https:// address (plain http only on a private or local address)";

/// The addresses of one GitHub account. Holds no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubBases {
    /// `https://host[:port]` (or `http://` on a private address), no trailing slash (opened in
    /// the browser only).
    pub web_base: String,
    pub api_base: String,
    pub graphql_url: String,
    /// `host[:port]`, lower-case: the `RepoRef` host and the only host git credentials go to.
    pub git_host: String,
}

/// The bases for the web address `web_url` (`https://github.com`, `https://ghe.corp.example`,
/// `https://octo.ghe.com`). `Err` is a short reason for the form.
pub fn bases_for(web_url: &str) -> Result<GitHubBases, String> {
    let text = web_url.trim().trim_end_matches('/');
    if text.is_empty() {
        return Err("Enter the GitHub web address, e.g. https://github.com".into());
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
        return Err("Enter only the address of the GitHub site, without ? or #".into());
    }
    if !matches!(url.path(), "" | "/") {
        return Err(
            "Enter only the address of the GitHub site (no path), e.g. https://github.example.com"
                .into(),
        );
    }
    let host = url
        .host_str()
        .ok_or("This address has no host")?
        .to_ascii_lowercase();
    if scheme == "http" && !prcore::origin::is_private_host(&host) {
        return Err(HTTP_REFUSED.into());
    }
    let git_host = match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.clone(),
    };
    if scheme == "https"
        && matches!(
            host.as_str(),
            "github.com" | "www.github.com" | "api.github.com"
        )
        && url.port().is_none()
    {
        return Ok(GitHubBases {
            web_base: "https://github.com".into(),
            api_base: "https://api.github.com".into(),
            graphql_url: "https://api.github.com/graphql".into(),
            git_host: "github.com".into(),
        });
    }
    let web_base = format!("{scheme}://{git_host}");
    if scheme == "https" && host.ends_with(".ghe.com") && url.port().is_none() {
        let sub = host.trim_start_matches("api.");
        return Ok(GitHubBases {
            web_base: format!("https://{sub}"),
            api_base: format!("https://api.{sub}"),
            graphql_url: format!("https://api.{sub}/graphql"),
            git_host: sub.to_string(),
        });
    }
    Ok(GitHubBases {
        api_base: format!("{web_base}/api/v3"),
        graphql_url: format!("{web_base}/api/graphql"),
        web_base,
        git_host,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_com_bases() {
        for typed in [
            "https://github.com",
            " https://GitHub.com/ ",
            "https://api.github.com",
        ] {
            let b = bases_for(typed).unwrap();
            assert_eq!(b.api_base, "https://api.github.com");
            assert_eq!(b.graphql_url, "https://api.github.com/graphql");
            assert_eq!(b.git_host, "github.com");
            assert_eq!(b.web_base, "https://github.com");
        }
    }

    #[test]
    fn ghes_base_derived_from_web_url() {
        let b = bases_for("https://GHE.corp.example/").unwrap();
        assert_eq!(b.web_base, "https://ghe.corp.example");
        assert_eq!(b.api_base, "https://ghe.corp.example/api/v3");
        assert_eq!(b.graphql_url, "https://ghe.corp.example/api/graphql");
        assert_eq!(b.git_host, "ghe.corp.example");
        let p = bases_for("https://ghe.corp.example:8443").unwrap();
        assert_eq!(p.api_base, "https://ghe.corp.example:8443/api/v3");
        assert_eq!(p.git_host, "ghe.corp.example:8443");
    }

    #[test]
    fn ghe_com_bases() {
        let b = bases_for("https://octo.ghe.com").unwrap();
        assert_eq!(b.api_base, "https://api.octo.ghe.com");
        assert_eq!(b.graphql_url, "https://api.octo.ghe.com/graphql");
        assert_eq!(b.git_host, "octo.ghe.com");
    }

    #[test]
    fn http_on_a_private_address_keeps_its_scheme() {
        let b = bases_for("http://192.168.1.20:8080/").unwrap();
        assert_eq!(b.web_base, "http://192.168.1.20:8080");
        assert_eq!(b.api_base, "http://192.168.1.20:8080/api/v3");
        assert_eq!(b.graphql_url, "http://192.168.1.20:8080/api/graphql");
        assert_eq!(b.git_host, "192.168.1.20:8080");
        let l = bases_for("http://ghe.localhost").unwrap();
        assert_eq!(l.api_base, "http://ghe.localhost/api/v3");
    }

    #[test]
    fn http_on_a_public_or_disguised_address_refused() {
        for bad in [
            "http://github.com",
            "http://ghe.corp.example",
            "http://octo.ghe.com",
            "http://8.8.8.8",
            "http://192.168.1.1.evil.com",
            "http://192.168.1.1@evil.com",
            "http://3232235777",
            "http://0300.0250.1.1",
            "http://[::ffff:192.168.1.1]",
        ] {
            let e = bases_for(bad).unwrap_err();
            assert!(e.contains("https") || e.contains("user name"), "{bad}: {e}");
        }
    }

    #[test]
    fn http_url_refused() {
        assert!(bases_for("http://ghe.corp.example")
            .unwrap_err()
            .contains("https"));
        for bad in [
            "",
            "ghe.corp.example",
            "ftp://ghe.corp.example",
            "https://user:pw@ghe.corp.example",
            "https://ghe.corp.example/some/path",
            "https://ghe.corp.example/?x=1",
            "https://",
        ] {
            assert!(bases_for(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn graphql_is_on_the_api_origin() {
        for typed in [
            "https://github.com",
            "https://ghe.corp.example",
            "https://o.ghe.com",
        ] {
            let b = bases_for(typed).unwrap();
            let api = Url::parse(&b.api_base).unwrap();
            let gql = Url::parse(&b.graphql_url).unwrap();
            assert_eq!(api.origin(), gql.origin(), "{typed}");
        }
    }
}
