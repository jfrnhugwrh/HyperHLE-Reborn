/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Local responder for retired third-party analytics and advertising services.
//!
//! Games from the 2009–2014 iPhone era commonly bundle an analytics "beacon"
//! (PlayHaven, Crittercism, …) that runs during launch, before the first frame
//! is presented. Those services no longer exist, so the faithful outcome of
//! such a request today is *failure* — but the way it fails matters to the
//! emulator:
//!
//! * If the request really goes out, it can only be answered by whoever owns
//!   the DNS name now, and it carries a device identifier and an app token. That
//!   is both a privacy problem and a security problem, and it also makes
//!   start-up depend on a stranger's server.
//! * If the request hangs (no DNS server reachable, black-holing ISP, sandbox
//!   without network), the SDK's retry logic runs on a guest thread, so the
//!   emulator thread never returns to the run loop: the app freezes, typically
//!   right after the publisher's logo.
//!
//! The fix is to answer these endpoints *locally*, exactly where the HTTP
//! transport is entered, so no name lookup and no socket is involved. The
//! default answer is a synthetic `503 Service Unavailable` with a JSON body:
//! the SDKs in question all treat a 5xx as a terminal "the service is down"
//! condition (they stop retrying and continue launching the game), which is
//! exactly the state those services are in. `--legacy-analytics=fail` instead
//! reports the equivalent of "cannot find host", and
//! `--legacy-analytics=passthrough` disables the interception entirely for
//! anyone who really wants to talk to the network.
//!
//! This lives in the Foundation URL-loading layer on purpose: every HTTP path
//! in touchHLE (`NSURLConnection`, `NSURLSession`, `NSData`, `CFReadStream`)
//! funnels through the transport, so one interception point covers all of them,
//! including requests the guest issues from a secondary thread.

use crate::frameworks::foundation::url_loading::{HttpError, HttpRequest, HttpResult, HttpResponse};
use crate::options::LegacyAnalyticsMode;

/// A retired third-party service whose endpoints are answered locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyEndpoint {
    /// PlayHaven monetisation/ad SDK (`api2.playhaven.com`,
    /// `media.playhaven.com`, …). The service was retired in 2014.
    PlayHaven,
    /// Crittercism/Apteligent crash reporting (`*.crittercism.com`). The
    /// service was shut down in 2018.
    Crittercism,
}

impl LegacyEndpoint {
    /// Host-name suffixes belonging to this service.
    pub fn host_suffixes(self) -> &'static [&'static str] {
        match self {
            LegacyEndpoint::PlayHaven => &["playhaven.com"],
            LegacyEndpoint::Crittercism => &["crittercism.com"],
        }
    }

    /// Short name used in logs and the mock response body.
    pub fn service_name(self) -> &'static str {
        match self {
            LegacyEndpoint::PlayHaven => "PlayHaven",
            LegacyEndpoint::Crittercism => "Crittercism",
        }
    }
}

/// Case-insensitive DNS suffix match on label boundaries.
fn host_matches(host: &str, suffix: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let suffix = suffix.to_ascii_lowercase();
    host == suffix
        || (host.len() > suffix.len()
            && host.ends_with(&suffix)
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.')
}

/// Extract the host from an `http(s)://…` URL without pulling in a URL parser.
///
/// The caller has already validated the URL's scheme and host presence
/// (`url_loading::validate_request`), so this only has to strip the userinfo and
/// port that may legally appear in the authority component.
fn host_of_url(url: &str) -> Option<&str> {
    let (_scheme, rest) = url.split_once("://")?;
    let authority = rest
        .split(|c| c == '/' || c == '?' || c == '#')
        .next()
        .unwrap_or(rest);
    // Drop `user:password@`.
    let host_and_port = match authority.rsplit_once('@') {
        Some((_userinfo, host_and_port)) => host_and_port,
        None => authority,
    };
    // IPv6 literals are not used by these services; treat them as unmatched
    // rather than mis-parsing the colons.
    if host_and_port.starts_with('[') {
        return None;
    }
    let host = match host_and_port.split_once(':') {
        Some((host, _port)) => host,
        None => host_and_port,
    };
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Classify a request URL as belonging to a retired service.
pub fn classify_url(url: &str) -> Option<LegacyEndpoint> {
    let host = host_of_url(url)?;
    for endpoint in [LegacyEndpoint::PlayHaven, LegacyEndpoint::Crittercism] {
        if endpoint
            .host_suffixes()
            .iter()
            .any(|suffix| host_matches(host, suffix))
        {
            return Some(endpoint);
        }
    }
    None
}

/// The synthetic "the service is gone" response.
///
/// `503` plus `Retry-After` mirrors what the (long dead) services' load
/// balancers would return during an outage, and is the status legacy SDKs treat
/// as "give up for this session" rather than "retry immediately".
pub fn mock_service_unavailable_response(
    request: &HttpRequest,
    endpoint: LegacyEndpoint,
) -> HttpResponse {
    let body = format!(
        "{{\"error\":\"service_unavailable\",\"service\":\"{}\",\"reason\":\"the service was \
         shut down; touchHLE answered locally\"}}",
        endpoint.service_name()
    )
    .into_bytes();
    HttpResponse {
        url: request.url.clone(),
        status_code: 503,
        headers: vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            // Discourage immediate retries without looking like a permanent
            // protocol error.
            ("Retry-After".to_string(), "86400".to_string()),
        ],
        body,
    }
}

/// The failure reported in `--legacy-analytics=fail` mode.
///
/// `NSURLErrorCannotFindHost` (-1003) is what a device reports today for these
/// names, since the DNS records no longer exist.
pub fn cannot_find_host_error(request: &HttpRequest, endpoint: LegacyEndpoint) -> HttpError {
    HttpError {
        code: -1003,
        description: format!(
            "The host of {} could not be found: the {} service no longer exists.",
            request.url,
            endpoint.service_name()
        ),
    }
}

/// Apply the configured policy to a request.
///
/// Returns `None` when the request should proceed normally (not a legacy
/// endpoint, or `Passthrough` policy); otherwise the result the caller should
/// deliver to the guest immediately.
pub fn intercept(request: &HttpRequest, mode: LegacyAnalyticsMode) -> Option<HttpResult> {
    let endpoint = classify_url(&request.url)?;
    match mode {
        LegacyAnalyticsMode::Passthrough => None,
        LegacyAnalyticsMode::FailFast => Some(Err(cannot_find_host_error(request, endpoint))),
        LegacyAnalyticsMode::MockServiceUnavailable => {
            log!(
                "Legacy analytics: answered {} request to {:?} locally with HTTP 503 (the {} \
                 service shut down); no DNS lookup or connection was attempted.",
                request.method,
                request.url,
                endpoint.service_name(),
            );
            Some(Ok(mock_service_unavailable_response(request, endpoint)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> HttpRequest {
        HttpRequest::get(url.to_string())
    }

    #[test]
    fn recognises_playhaven_and_crittercism_endpoints() {
        assert_eq!(
            classify_url("http://api2.playhaven.com/v3/publisher/open"),
            Some(LegacyEndpoint::PlayHaven)
        );
        assert_eq!(
            classify_url("https://media.playhaven.com/content/1.png"),
            Some(LegacyEndpoint::PlayHaven)
        );
        assert_eq!(
            classify_url("https://app.crittercism.com/api_beta/crash"),
            Some(LegacyEndpoint::Crittercism)
        );
        // Userinfo and explicit ports must not confuse host extraction.
        assert_eq!(
            classify_url("http://token:secret@api2.playhaven.com:8080/x"),
            Some(LegacyEndpoint::PlayHaven)
        );
    }

    #[test]
    fn does_not_match_unrelated_hosts() {
        assert_eq!(classify_url("http://example.com/playhaven.com"), None);
        assert_eq!(classify_url("http://notplayhaven.com/x"), None);
        assert_eq!(classify_url("http://playhaven.com.evil.example/x"), None);
        assert_eq!(classify_url("http://crittercism.co/x"), None);
        assert_eq!(classify_url("not a url"), None);
        assert_eq!(classify_url("http://[::1]/x"), None);
    }

    #[test]
    fn mock_mode_answers_with_503() {
        let result = intercept(
            &request("http://api2.playhaven.com/v3/publisher/open"),
            LegacyAnalyticsMode::MockServiceUnavailable,
        )
        .expect("legacy endpoint should be intercepted");
        let response = result.expect("mock mode succeeds with a synthetic response");
        assert_eq!(response.status_code, 503);
        assert!(!response.body.is_empty());
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "Retry-After" && value == "86400"));
    }

    #[test]
    fn fail_mode_reports_cannot_find_host() {
        let result = intercept(
            &request("https://app.crittercism.com/api_beta/crash"),
            LegacyAnalyticsMode::FailFast,
        )
        .expect("legacy endpoint should be intercepted");
        let error = result.expect_err("fail mode must not produce a response");
        assert_eq!(error.code, -1003);
    }

    #[test]
    fn passthrough_and_unrelated_hosts_are_left_alone() {
        assert!(intercept(
            &request("http://api2.playhaven.com/x"),
            LegacyAnalyticsMode::Passthrough
        )
        .is_none());
        assert!(intercept(
            &request("http://example.com/x"),
            LegacyAnalyticsMode::MockServiceUnavailable
        )
        .is_none());
    }
}
