// url_filter.rs

//! Database admission policy; no HTTP requests, redirects, or application DNS cache.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use anyhow::{Context, ensure};
use tokio::{
    net::lookup_host,
    time::{Duration, timeout},
};
use url::{Host, Url};

const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_DNS_ADDRESSES: usize = 64;

#[derive(Clone, Debug, Default)]
pub(crate) struct UrlFilter {
    domains: HashSet<String>,
    addresses: HashSet<IpAddr>,
}

impl UrlFilter {
    pub(crate) fn compile(rules: &[String], scope: &str) -> anyhow::Result<Self> {
        let mut filter = Self::default();
        for (index, rule) in rules.iter().enumerate() {
            filter
                .add(rule)
                .with_context(|| format!("{scope} url_blacklist entry {}", index + 1))?;
        }
        Ok(filter)
    }

    fn add(&mut self, rule: &str) -> anyhow::Result<()> {
        // Accept bare IPv6 addresses as well as the bracketed URL spelling.
        if let Ok(ip) = rule.parse::<IpAddr>() {
            self.addresses.insert(ip);
            return Ok(());
        }
        ensure!(
            !rule.is_empty()
                && !rule.chars().any(|c| c.is_whitespace() || c.is_control())
                && !rule.contains(['/', '?', '#', '@', '%', '*', '\\']),
            "expected a DNS name or IP address, without a scheme, path, or wildcard"
        );
        match Host::parse(rule).context("invalid host")? {
            Host::Domain(domain) => {
                self.domains.insert(dns_name(&domain)?);
            }
            Host::Ipv4(ip) => {
                self.addresses.insert(ip.into());
            }
            Host::Ipv6(ip) => {
                self.addresses.insert(ip.into());
            }
        }
        Ok(())
    }

    pub(crate) fn include(&mut self, other: &Self) {
        self.domains.extend(other.domains.iter().cloned());
        self.addresses.extend(other.addresses.iter().copied());
    }

    fn blocks_domain(&self, name: &str) -> bool {
        let mut suffix = name;
        loop {
            if self.domains.contains(suffix) {
                return true;
            }
            let Some((_, rest)) = suffix.split_once('.') else {
                return false;
            };
            if rest.is_empty() {
                return false;
            }
            suffix = rest;
        }
    }

    pub(crate) async fn check(&self, input: &str) -> anyhow::Result<()> {
        self.check_with_resolver(input, |name| async move {
            // The final dot makes this an absolute lookup, without search suffixes.
            Ok(lookup_host((name.as_str(), 0))
                .await?
                .take(MAX_DNS_ADDRESSES + 1)
                .map(|addr| addr.ip())
                .collect())
        })
        .await
    }

    async fn check_with_resolver<F, R>(&self, input: &str, resolve: F) -> anyhow::Result<()>
    where
        F: FnOnce(String) -> R,
        R: Future<Output = std::io::Result<Vec<IpAddr>>>,
    {
        let url = Url::parse(input).context("invalid URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "unsupported URL scheme"
        );
        let addresses = match url.host().context("URL has no host")? {
            Host::Domain(domain) => {
                let name = dns_name(domain)?;
                ensure!(!self.blocks_domain(&name), "blacklisted domain");
                timeout(DNS_TIMEOUT, resolve(name))
                    .await
                    .context("DNS lookup timed out")?
                    .context("DNS lookup failed")?
            }
            Host::Ipv4(ip) => vec![ip.into()],
            Host::Ipv6(ip) => vec![ip.into()],
        };
        ensure!(!addresses.is_empty(), "DNS lookup returned no addresses");
        ensure!(
            addresses.len() <= MAX_DNS_ADDRESSES,
            "DNS lookup returned too many addresses"
        );
        ensure!(
            addresses.iter().all(|ip| !self.addresses.contains(ip)),
            "blacklisted IP address"
        );
        ensure!(
            addresses.iter().all(|ip| is_public_address(*ip)),
            "non-public IP address"
        );
        Ok(())
    }
}

// Host::parse/Url::parse already perform IDNA conversion. Reject empty labels and
// malformed names rather than silently repairing multiple trailing dots.
fn dns_name(domain: &str) -> anyhow::Result<String> {
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    ensure!(
        !domain.is_empty()
            && domain.len() <= 253
            && domain.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            }),
        "invalid DNS name"
    );
    Ok(format!("{}.", domain.to_ascii_lowercase()))
}

// Keep aligned with sjmb's http_fetch.rs destination policy. This deliberately
// excludes more than RFC1918: special-purpose and IPv6 translation ranges too.
fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let n = u32::from(ip);
    let blocked = [
        (Ipv4Addr::new(0, 0, 0, 0), 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(100, 64, 0, 0), 10),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 88, 99, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(224, 0, 0, 0), 4),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
    ];
    !blocked
        .iter()
        .any(|&(base, bits)| n & (u32::MAX << (32 - bits)) == u32::from(base))
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let n = u128::from(ip);
    let mask = |bits| u128::MAX << (128 - bits);
    if n & mask(3) != u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)) {
        return false;
    }
    let blocked = [
        (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
        (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
        (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    ];
    !blocked
        .iter()
        .any(|&(base, bits)| n & mask(bits) == u128::from(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(rules: &[&str]) -> UrlFilter {
        UrlFilter::compile(
            &rules.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "test",
        )
        .unwrap()
    }

    async fn no_dns(_: String) -> std::io::Result<Vec<IpAddr>> {
        panic!("this URL must not require DNS")
    }

    #[test]
    fn domain_rules_normalize_and_respect_label_boundaries() {
        let filter = filter(&["EXAMPLE.com.", "bücher.example", "internal"]);
        for name in [
            "example.com.",
            "a.example.com.",
            "a.b.example.com.",
            "xn--bcher-kva.example.",
            "internal.",
            "app.internal.",
        ] {
            assert!(filter.blocks_domain(name), "{name}");
        }
        for name in [
            "notexample.com.",
            "example.com.evil.test.",
            "other.example.",
        ] {
            assert!(!filter.blocks_domain(name), "{name}");
        }
        assert_eq!(dns_name("Example.COM").unwrap(), "example.com.");
        assert_eq!(dns_name("example.com.").unwrap(), "example.com.");
    }

    #[test]
    fn malformed_rules_fail_with_scope_and_index() {
        for rule in [
            "",
            " ",
            "*.example.com",
            "https://example.com",
            "example.com/path",
            "example.com:80",
            "example.com..",
            ".example.com",
            "a..example.com",
            "a@example.com",
            "example.com?x",
            "example.com#x",
            "-example.com",
            "example-.com",
            "%65xample.com",
            "example.com\\path",
        ] {
            let error = UrlFilter::compile(&["valid.test".into(), rule.into()], "workspace foo")
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("workspace foo url_blacklist entry 2"),
                "{rule}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn blacklist_precedes_dns_and_checks_only_the_actual_host() {
        let filter = filter(&["example.com", "bücher.example"]);
        for url in [
            "http://EXAMPLE.com./a",
            "https://a.b.example.com:8443/",
            "https://other.test@example.com/",
            "https://bücher.example/",
        ] {
            assert!(
                filter
                    .check_with_resolver(url, no_dns)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("blacklisted")
            );
        }
        for url in [
            "https://notexample.com/",
            "https://example.com.evil.test/",
            "https://example.com@other.test/example.com?host=example.com",
        ] {
            filter
                .check_with_resolver(url, |_| async { Ok(vec!["8.8.8.8".parse().unwrap()]) })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn literal_addresses_are_parsed_without_dns_and_ip_rules_are_exact() {
        let filter = filter(&["8.8.8.8", "[2606:4700:4700::1111]"]);
        for url in [
            "https://8.8.8.8/",
            "http://[2606:4700:4700:0:0:0:0:1111]/",
            "http://192.168.1.1/",
            "http://[::1]/",
            "http://2130706433/",
            "http://0x7f000001/",
        ] {
            assert!(
                filter.check_with_resolver(url, no_dns).await.is_err(),
                "{url}"
            );
        }
        for url in ["https://8.8.8.9/", "http://[2606:4700:4700::1001]/"] {
            filter.check_with_resolver(url, no_dns).await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dns_uses_absolute_names_and_rejects_failures_and_mixed_answers() {
        let filter = UrlFilter::default();
        filter
            .check_with_resolver("https://ExAmPlE.com:8443/path", |name| async move {
                assert_eq!(name, "example.com.");
                Ok(vec![
                    "8.8.8.8".parse().unwrap(),
                    "2606:4700:4700::1111".parse().unwrap(),
                ])
            })
            .await
            .unwrap();
        for addresses in [
            vec![],
            vec!["8.8.8.8", "10.0.0.1"],
            vec!["8.8.8.8", "::1"],
            vec!["8.8.8.8"; 65],
        ] {
            assert!(
                filter
                    .check_with_resolver("https://example.com/", |_| async {
                        Ok(addresses.into_iter().map(|s| s.parse().unwrap()).collect())
                    })
                    .await
                    .is_err()
            );
        }
        let error = filter
            .check_with_resolver("https://example.com/", |_| async {
                Err(std::io::Error::other("resolver failure"))
            })
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("DNS lookup failed: resolver failure"));
        let error = filter
            .check_with_resolver("https://example.com/", |_| std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DNS lookup timed out"));
    }

    #[test]
    fn non_public_address_policy_matches_sjmb() {
        for ip in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.9.8.7",
            "169.254.169.254",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
            "64:ff9b::a00:1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001::1",
            "2001:db8::1",
            "2002:7f00:1::",
            "3fff::1",
        ] {
            assert!(!is_public_address(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.0",
            "172.32.0.1",
            "2606:4700:4700::1111",
        ] {
            assert!(is_public_address(ip.parse().unwrap()), "{ip}");
        }
    }
}

// EOF
