use std::{
    collections::HashMap,
    hash::Hash,
    sync::atomic::{AtomicUsize, Ordering},
};

use tokio::time::{Duration, Instant};

/// Expiry is measured from insertion, not extended by reads. Eviction is bounded
/// by capacity, including for entries whose expiry has not yet been visited.
#[derive(Debug)]
pub(crate) struct ExpiringCache<K, V> {
    entries: HashMap<K, (Instant, V)>,
    capacity: usize,
}

impl<K: Eq + Hash + Clone, V: Clone> ExpiringCache<K, V> {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            entries: HashMap::new(),
            capacity,
        }
    }

    pub(crate) fn get(&mut self, key: &K, now: Instant) -> Option<V> {
        match self.entries.get(key) {
            Some((expires, value)) if *expires > now => Some(value.clone()),
            _ => {
                self.entries.remove(key);
                None
            }
        }
    }

    pub(crate) fn insert(&mut self, key: K, value: V, now: Instant, ttl: Duration) {
        if !self.entries.contains_key(&key) && self.entries.len() == self.capacity {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (expires, _))| expires)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, (now + ttl, value));
    }
}

#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) full: AtomicUsize,
    pub(crate) expired: AtomicUsize,
    pub(crate) oversized_messages: AtomicUsize,
    pub(crate) oversized_urls: AtomicUsize,
    pub(crate) invalid: AtomicUsize,
    pub(crate) duplicates: AtomicUsize,
}

impl Counters {
    pub(crate) fn report(&self) {
        for (reason, count) in [
            ("full message queue", &self.full),
            ("expired message", &self.expired),
            ("oversized message", &self.oversized_messages),
            ("oversized URL", &self.oversized_urls),
            ("invalid input", &self.invalid),
        ] {
            let count = count.swap(0, Ordering::Relaxed);
            if count > 0 {
                tracing::error!("Discarded {count} items: {reason}");
            }
        }
        let duplicates = self.duplicates.swap(0, Ordering::Relaxed);
        if duplicates > 0 {
            tracing::debug!("Ignored {duplicates} duplicate Slack events");
        }
    }
}

/// Keep untrusted console fields on one line and within a UTF-8 byte budget.
pub(crate) fn log_text(text: &str, max_bytes: usize) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        let ch = if ch.is_control() || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if result.len() + ch.len_utf8() > max_bytes {
            break;
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_expires_without_reads_extending_lifetime_and_evicts_oldest() {
        let now = Instant::now();
        let mut cache = ExpiringCache::new(2);
        cache.insert("a", 1, now, Duration::from_secs(10));
        cache.insert("b", 2, now, Duration::from_secs(20));
        assert_eq!(cache.get(&"a", now + Duration::from_secs(9)), Some(1));
        assert_eq!(cache.get(&"a", now + Duration::from_secs(10)), None);
        cache.insert("a", 3, now, Duration::from_secs(10));
        cache.insert("c", 4, now, Duration::from_secs(30));
        assert_eq!(cache.get(&"a", now), None);
        assert_eq!(cache.get(&"b", now), Some(2));
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn logs_remove_controls_and_respect_unicode_byte_boundaries() {
        assert_eq!(log_text("a\r\n\u{1b}b", 20), "a   b");
        assert_eq!(log_text("ééé", 5), "éé");
    }
}
