use std::sync::atomic::Ordering;

use anyhow::{Context, bail};
use regex::Regex;
use url::Url;

use crate::limits::Counters;

pub(crate) const MAX_URL_BYTES: usize = 2048;
pub(crate) const MAX_URLS_PER_MESSAGE: usize = 16;

pub(crate) fn message_timestamp(ts: &str) -> anyhow::Result<i64> {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    if seconds.is_empty()
        || !seconds.bytes().all(|b| b.is_ascii_digit())
        || (ts.contains('.')
            && (fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit())))
    {
        bail!("invalid Slack message timestamp");
    }
    let seconds = seconds.parse::<i64>().context("Slack timestamp overflow")?;
    chrono::DateTime::from_timestamp(seconds, 0).context("Slack timestamp out of range")?;
    Ok(seconds)
}

pub(crate) fn extract_urls(re: &Regex, text: &str, counters: &Counters) -> Vec<String> {
    re.captures_iter(text)
        .take(MAX_URLS_PER_MESSAGE)
        .filter_map(|capture| {
            let value = capture.get(1)?.as_str();
            // Split Slack's link label before decoding: escaped pipes are URL data.
            let value = value.split_once('|').map_or(value, |(url, _)| url);
            // Decode exactly once, with &amp; last to preserve escaped entity text.
            let value = value
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&");
            if value.len() > MAX_URL_BYTES {
                counters.oversized_urls.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let parsed = Url::parse(&value).ok().filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && !value
                        .chars()
                        .any(|ch| ch.is_control() || ch.is_whitespace())
            });
            let Some(url) = parsed else {
                counters.invalid.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            let url: String = url.into();
            if url.len() > MAX_URL_BYTES {
                counters.oversized_urls.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Some(url)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<String> {
        extract_urls(
            &Regex::new("<(https?://[^>]+)>").unwrap(),
            text,
            &Counters::default(),
        )
    }

    #[test]
    fn slack_links_decode_once_strip_labels_and_normalize_unicode() {
        assert_eq!(
            urls("<https://example.com/?a=1&amp;b=2|Read &amp; enjoy>"),
            ["https://example.com/?a=1&b=2"]
        );
        assert_eq!(
            urls("<https://example.com/?q=&amp;lt;>"),
            ["https://example.com/?q=&lt;"]
        );
        assert_eq!(
            urls("<https://example.com/é>"),
            ["https://example.com/%C3%A9"]
        );
    }

    #[test]
    fn url_limit_checks_stored_bytes_and_discards_without_truncating() {
        let prefix = "https://example.com/";
        let exact = format!("{prefix}{}", "a".repeat(MAX_URL_BYTES - prefix.len()));
        assert_eq!(urls(&format!("<{exact}>")), std::slice::from_ref(&exact));
        assert!(urls(&format!("<{exact}a>")).is_empty());
        // Input fits, but percent-encoding would exceed the storage budget.
        assert!(urls(&format!("<{prefix}{}>", "é".repeat(400))).is_empty());
    }

    #[test]
    fn work_is_bounded_and_optional_empty_invalid_captures_are_safe() {
        assert_eq!(
            urls(&"<https://example.com/> ".repeat(20)).len(),
            MAX_URLS_PER_MESSAGE
        );
        assert!(
            extract_urls(
                &Regex::new("(https)?www").unwrap(),
                "www",
                &Counters::default()
            )
            .is_empty()
        );
        assert!(extract_urls(&Regex::new("()").unwrap(), "x", &Counters::default()).is_empty());
        assert!(urls("<https://example.com/a\nb>").is_empty());
        assert!(urls("<https://?>").is_empty());
    }

    #[test]
    fn timestamps_use_message_seconds_and_reject_invalid_or_overflowing_input() {
        assert_eq!(message_timestamp("1234.567890").unwrap(), 1234);
        for value in [
            "",
            "-1.0",
            "1.",
            "1.2.3",
            "NaN",
            "999999999999999999999",
            "9223372036854775807",
        ] {
            assert!(message_timestamp(value).is_err(), "{value}");
        }
    }
}
