/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Bounded host-name resolution for the guest's network APIs.
//!
//! touchHLE's networking primitives (`getaddrinfo`, `gethostbyname`, CFHost,
//! and by extension every HTTP request) must never block the emulator thread on
//! a name lookup. On a real device that is not a problem: `mDNSResponder`
//! resolves in the background and a request against a dead domain fails within
//! a second. In the emulator, "DNS" means a call into the *host* resolver from
//! the same thread that runs the guest CPU, so a slow or unreachable resolver
//! (a sandbox with no DNS reachable, a captive portal, an ISP that black-holes
//! unknown names) stops the whole emulation — the guest's run loop never runs
//! again, and the app appears frozen on its last frame. That is exactly the
//! "stuck after the Cartoon Network logo" failure mode of games that start an
//! analytics/ad SDK during launch: the SDK's own thread is a guest thread, so
//! its `gethostbyname("api2.playhaven.com")` froze the emulator.
//!
//! Three mechanisms keep that from happening:
//!
//! 1. **Known-defunct services fail immediately.** PlayHaven and Crittercism
//!    shut down years ago, so their hostnames are answered with
//!    [`LookupError::DefunctService`] without asking the host resolver at all.
//!    This is both faster and more faithful than resolving: the real answer
//!    today is NXDOMAIN, and doing the lookup would send a DNS query naming the
//!    user's emulator to whoever owns the domain now.
//! 2. **Every real lookup is bounded.** The blocking call runs on a short-lived
//!    helper thread and the emulator thread waits at most
//!    [`DEFAULT_TIMEOUT_MS`] (overridable with `TOUCHHLE_DNS_TIMEOUT_MS`) for an
//!    answer. On timeout the lookup fails like a resolver failure would.
//! 3. **Results are cached, including failures.** A failing domain is retried
//!    at most once per negative-TTL window instead of on every retry the SDK
//!    makes, so a broken start-up path costs one timeout, not sixty per second.
//!
//! Nothing here touches guest memory or `Environment`, so it is usable from the
//! libc layer, from CFNetwork, and from worker threads, and it is unit-testable
//! without an emulated machine.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long the emulator thread waits for the host resolver by default.
///
/// `mDNSResponder` gives up after roughly one second per configured resolver
/// (plus retries), and apps treat a slower answer as a network failure anyway;
/// a value in the low seconds therefore matches device behaviour while keeping
/// start-up responsive. Override with `TOUCHHLE_DNS_TIMEOUT_MS` (0 disables the
/// bound and restores fully blocking lookups, for debugging).
pub const DEFAULT_TIMEOUT_MS: u64 = 1500;

/// Cap on simultaneously outstanding lookups. Each one owns a helper thread, so
/// a guest that starts hundreds of connections at once must not spawn hundreds
/// of threads; beyond the cap lookups fail fast instead of queueing (which
/// would just move the wait).
const MAX_INFLIGHT_LOOKUPS: usize = 8;

/// How long a successful answer is reused.
const POSITIVE_TTL: Duration = Duration::from_secs(300);

/// How long a failure (including a timeout) is reused.
///
/// The number matters: SDKs retry a failed beacon every few seconds, and
/// without negative caching each retry would burn the full timeout again,
/// producing a permanent stutter. Anything that manages to resolve after a
/// brief outage is retried within half a minute, which is what a real device
/// would effectively do through `mDNSResponder`'s own negative cache.
const NEGATIVE_TTL: Duration = Duration::from_secs(30);

/// Bound on cached host names.
const MAX_CACHED_HOSTS: usize = 128;

/// Domains of third-party services that no longer exist.
///
/// These are *not* "blocked" for privacy reasons; they are dead. Answering them
/// locally is the only way to be both fast and accurate, because at the DNS
/// level they are indistinguishable from an attacker-controlled domain that
/// would now receive the requests (and the device identifiers in them).
///
/// * `playhaven.com` — PlayHaven's monetisation/ad SDK. The company was
///   acquired in 2014 and the service (including `api2.playhaven.com` and
///   `media.playhaven.com`, which SDK < 2.x contacts during `-start`) was
///   retired.
/// * `crittercism.com` — Crittercism (renamed Apteligent) crash/analytics SDK;
///   the service was shut down in 2018. Its NSURLConnection swizzling still
///   fires during the first run loop turn of old builds.
pub const DEFUNCT_LEGACY_DOMAINS: &[&str] = &["playhaven.com", "crittercism.com"];

/// Why a lookup did not produce addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupError {
    /// The hostname belongs to a retired third-party service. Refused without
    /// consulting the host resolver (see [`DEFUNCT_LEGACY_DOMAINS`]).
    DefunctService,
    /// The host resolver did not answer within the bounded timeout.
    TimedOut,
    /// The resolver failed, or answered without any usable address.
    Failed,
}

impl LookupError {
    /// Human-readable description for logs and mock error objects.
    pub fn description(self) -> &'static str {
        match self {
            LookupError::DefunctService => "the service this host belongs to no longer exists",
            LookupError::TimedOut => "the host name lookup timed out",
            LookupError::Failed => "the host name could not be resolved",
        }
    }
}

/// Cached outcome for one host name.
#[derive(Clone)]
struct CacheEntry {
    result: Result<Vec<IpAddr>, LookupError>,
    stored: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

static INFLIGHT_LOOKUPS: AtomicUsize = AtomicUsize::new(0);

/// Decrements [`INFLIGHT_LOOKUPS`] even if the lookup panics.
struct InflightGuard;

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT_LOOKUPS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Normalise a guest-supplied host name for matching and caching:
/// lower-case, without the root label's trailing dot or surrounding space.
fn normalize_host_name(hostname: &str) -> String {
    hostname
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Whether `hostname` belongs to one of the retired services in
/// [`DEFUNCT_LEGACY_DOMAINS`].
///
/// Matching is on label boundaries, so `playhaven.com` and
/// `api2.playhaven.com` match while `notplayhaven.com` and
/// `playhaven.com.example.org` do not.
pub fn is_defunct_legacy_host(hostname: &str) -> bool {
    let hostname = normalize_host_name(hostname);
    if hostname.is_empty() {
        return false;
    }
    DEFUNCT_LEGACY_DOMAINS.iter().any(|domain| {
        hostname == *domain
            || (hostname.len() > domain.len()
                && hostname.ends_with(domain)
                && hostname.as_bytes()[hostname.len() - domain.len() - 1] == b'.')
    })
}

/// The bounded lookup timeout, honouring `TOUCHHLE_DNS_TIMEOUT_MS`.
///
/// `0` means "no bound": the emulator thread then waits for the resolver, which
/// is only useful when debugging the resolver itself.
fn lookup_timeout() -> Option<Duration> {
    let configured = crate::env_var_cached!("TOUCHHLE_DNS_TIMEOUT_MS")
        .and_then(|value| value.trim().parse::<u64>().ok());
    match configured {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(Duration::from_millis(DEFAULT_TIMEOUT_MS)),
    }
}

fn ttl_for(result: &Result<Vec<IpAddr>, LookupError>) -> Duration {
    if result.is_ok() {
        POSITIVE_TTL
    } else {
        NEGATIVE_TTL
    }
}

fn cached_result(hostname: &str, now: Instant) -> Option<Result<Vec<IpAddr>, LookupError>> {
    let cache = cache().lock().unwrap();
    let entry = cache.get(hostname)?;
    if now.duration_since(entry.stored) < ttl_for(&entry.result) {
        Some(entry.result.clone())
    } else {
        None
    }
}

fn store_result(hostname: &str, result: &Result<Vec<IpAddr>, LookupError>, now: Instant) {
    let mut cache = cache().lock().unwrap();
    if cache.len() >= MAX_CACHED_HOSTS && !cache.contains_key(hostname) {
        // Drop the oldest entry; resolution results are cheap to recompute.
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.stored)
            .map(|(name, _)| name.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        hostname.to_string(),
        CacheEntry {
            result: result.clone(),
            stored: now,
        },
    );
}

fn lookup_blocking(hostname: &str) -> Result<Vec<IpAddr>, LookupError> {
    // Port 0 is a placeholder; only the addresses matter.
    let addresses = (hostname, 0u16)
        .to_socket_addrs()
        .map_err(|_| LookupError::Failed)?;
    let mut result: Vec<IpAddr> = Vec::new();
    for address in addresses {
        let ip = address.ip();
        if !result.contains(&ip) {
            result.push(ip);
        }
    }
    if result.is_empty() {
        Err(LookupError::Failed)
    } else {
        Ok(result)
    }
}

/// Resolve `hostname`, bounded by [`lookup_timeout`] and cached.
pub fn resolve_hostname_ips(hostname: &str) -> Result<Vec<IpAddr>, LookupError> {
    let key = normalize_host_name(hostname);
    if key.is_empty() {
        return Err(LookupError::Failed);
    }

    // Dead services never reach the resolver (or the cache TTL rules): the
    // answer can never change, and asking would leak the request to whoever
    // owns the domain now.
    if is_defunct_legacy_host(&key) {
        return Err(LookupError::DefunctService);
    }
    if let Ok(ip) = key.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }

    let now = Instant::now();
    if let Some(cached) = cached_result(&key, now) {
        return cached;
    }

    let result = resolve_uncached(&key);
    if result.is_err() {
        log!(
            "DNS: {:?} failed: {} (answer cached for {}s)",
            key,
            result
                .as_ref()
                .err()
                .map(|error| error.description())
                .unwrap_or_default(),
            NEGATIVE_TTL.as_secs(),
        );
    }
    store_result(&key, &result, now);
    result
}

fn resolve_uncached(hostname: &str) -> Result<Vec<IpAddr>, LookupError> {
    if INFLIGHT_LOOKUPS.fetch_add(1, Ordering::AcqRel) >= MAX_INFLIGHT_LOOKUPS {
        INFLIGHT_LOOKUPS.fetch_sub(1, Ordering::AcqRel);
        log!(
            "DNS: refusing another concurrent lookup for {:?} ({} already in flight)",
            hostname,
            MAX_INFLIGHT_LOOKUPS,
        );
        return Err(LookupError::TimedOut);
    }
    let _guard = InflightGuard;

    let Some(timeout) = lookup_timeout() else {
        // Debugging escape hatch: resolve on the calling thread.
        return lookup_blocking(hostname);
    };

    let (sender, receiver) = mpsc::channel();
    let query = hostname.to_string();
    let spawned = std::thread::Builder::new()
        .name("touchHLE DNS lookup".to_string())
        .spawn(move || {
            // The receiver may be gone (timeout); sending then just fails, and
            // the thread exits either way.
            let _ = sender.send(lookup_blocking(&query));
        });
    if spawned.is_err() {
        return Err(LookupError::Failed);
    }

    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            // The helper thread is left to finish (it cannot be cancelled
            // portably); it only touches its own `String` and a channel, so
            // leaking it briefly is harmless, and [`MAX_INFLIGHT_LOOKUPS`]
            // bounds how many can pile up.
            log!(
                "DNS: lookup for {:?} did not answer within {:?}; reporting a resolver \
                 timeout so the guest's networking code can fail instead of freezing.",
                hostname,
                timeout,
            );
            Err(LookupError::TimedOut)
        }
    }
}

/// IPv4 `SocketAddr`s for `hostname` (port 0), for CFHost/SocketStream callers.
///
/// IPv6 is dropped because the guest-visible `sockaddr` is the 16-byte IPv4
/// layout; this mirrors what the code did before it gained a shared resolver.
pub fn resolve_ipv4_addresses(hostname: &str) -> Result<Vec<SocketAddrV4>, LookupError> {
    let addresses = resolve_hostname_ips(hostname)?;
    let mut result: Vec<SocketAddrV4> = Vec::new();
    for address in addresses {
        if let IpAddr::V4(ipv4) = address {
            let socket_address = SocketAddrV4::new(ipv4, 0);
            if !result.contains(&socket_address) {
                result.push(socket_address);
            }
        }
    }
    if result.is_empty() {
        Err(LookupError::Failed)
    } else {
        Ok(result)
    }
}

/// Resolve `hostname` to any `SocketAddr`, for callers that already gate on
/// `--allow-network-access` themselves (e.g. `getaddrinfo`).
pub fn resolve_socket_addresses(hostname: &str) -> Result<Vec<SocketAddr>, LookupError> {
    resolve_hostname_ips(hostname).map(|addresses| {
        addresses
            .into_iter()
            .map(|ip| SocketAddr::new(ip, 0))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defunct_services_are_recognised_including_subdomains() {
        assert!(is_defunct_legacy_host("playhaven.com"));
        assert!(is_defunct_legacy_host("api2.playhaven.com"));
        assert!(is_defunct_legacy_host("media.playhaven.com"));
        assert!(is_defunct_legacy_host("API2.PlayHaven.COM."));
        assert!(is_defunct_legacy_host("crittercism.com"));
        assert!(is_defunct_legacy_host("app.crittercism.com"));
    }

    #[test]
    fn defunct_matching_respects_label_boundaries() {
        assert!(!is_defunct_legacy_host("notplayhaven.com"));
        assert!(!is_defunct_legacy_host("playhaven.com.example.org"));
        assert!(!is_defunct_legacy_host("playhaven.co"));
        assert!(!is_defunct_legacy_host(""));
    }

    #[test]
    fn defunct_lookup_fails_without_touching_the_resolver() {
        // Would hang (or return NXDOMAIN) if this went to the network.
        assert_eq!(
            resolve_hostname_ips("api2.playhaven.com"),
            Err(LookupError::DefunctService)
        );
        assert_eq!(
            resolve_hostname_ips("media.playhaven.com"),
            Err(LookupError::DefunctService)
        );
    }

    #[test]
    fn numeric_addresses_resolve_without_a_lookup() {
        assert_eq!(
            resolve_hostname_ips("127.0.0.1"),
            Ok(vec![IpAddr::from([127, 0, 0, 1])])
        );
        assert_eq!(
            resolve_hostname_ips("192.0.2.7"),
            Ok(vec![IpAddr::from([192, 0, 2, 7])])
        );
    }

    #[test]
    fn negative_results_are_cached() {
        let now = Instant::now();
        let failure: Result<Vec<IpAddr>, LookupError> = Err(LookupError::TimedOut);
        store_result("negative-cache-test.invalid", &failure, now);
        assert_eq!(
            cached_result("negative-cache-test.invalid", now + Duration::from_secs(1)),
            Some(failure)
        );
        // Expiry restores a fresh attempt.
        assert_eq!(
            cached_result(
                "negative-cache-test.invalid",
                now + NEGATIVE_TTL + Duration::from_secs(1)
            ),
            None
        );
    }

    #[test]
    fn positive_results_outlive_negative_ones() {
        let now = Instant::now();
        let success: Result<Vec<IpAddr>, LookupError> = Ok(vec![IpAddr::from([192, 0, 2, 1])]);
        store_result("positive-cache-test.invalid", &success, now);
        let after_negative_ttl = now + NEGATIVE_TTL + Duration::from_secs(1);
        assert_eq!(cached_result("positive-cache-test.invalid", after_negative_ttl), Some(success));
    }

    #[test]
    fn cache_eviction_keeps_the_table_bounded() {
        let now = Instant::now();
        let failure: Result<Vec<IpAddr>, LookupError> = Err(LookupError::Failed);
        for index in 0..(MAX_CACHED_HOSTS + 8) {
            store_result(&format!("host{index}.invalid"), &failure, now);
        }
        assert!(cache().lock().unwrap().len() <= MAX_CACHED_HOSTS);
    }
}
