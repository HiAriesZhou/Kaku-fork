//! HTTP transport: proxy-aware client construction, bounded body and SSE
//! reads, and request retry with cancellation.

use anyhow::{Context, Result};
use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use super::{
    MAX_ERROR_BODY_BYTES, MAX_RESPONSE_BODY_BYTES, MAX_RESPONSE_SSE_LINE_BYTES,
    MAX_RESPONSE_STREAM_BYTES, MAX_RESPONSE_STREAM_EVENTS,
};

/// Build a blocking reqwest client that respects the user's system proxy.
///
/// Reqwest already honors standard proxy env vars; this helper additionally
/// falls back to `scutil --proxy` on macOS so launches from the menu bar or
/// Finder, which inherit launchd's empty environment, still go through the
/// user's configured proxy. Without this fallback such launches silently
/// bypass the proxy, the same hazard already fixed in the curl-based
/// update path.
///
/// `timeout` controls the per-request ceiling; AI chat needs minutes for
/// long streaming completions while web tools should fail fast.
pub(crate) fn build_client_with_proxy(timeout: std::time::Duration) -> reqwest::blocking::Client {
    build_client_with_proxy_redirects(timeout, true)
}

fn build_client_with_proxy_redirects(
    timeout: std::time::Duration,
    follow_redirects: bool,
) -> reqwest::blocking::Client {
    build_client_with_proxy_options(timeout, None, follow_redirects)
}

pub(super) fn build_client_with_proxy_options(
    timeout: std::time::Duration,
    read_timeout: Option<std::time::Duration>,
    follow_redirects: bool,
) -> reqwest::blocking::Client {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(timeout);
    if let Some(read_timeout) = read_timeout {
        builder = builder.timeout(read_timeout);
    }
    if !follow_redirects {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }

    if let Some(proxy_url) = config::proxy::detect_system_proxy() {
        match reqwest::Proxy::all(&proxy_url) {
            Ok(proxy) => {
                // Bypass the proxy for loopback/LAN/CGNAT ranges so a
                // self-hosted model server stays reachable. Critical because
                // reqwest is built without the `socks` feature, so forcing a
                // SOCKS proxy onto an internal `base_url` fails the request
                // outright ("error sending request for url").
                let proxy = proxy.no_proxy(build_no_proxy());
                log::info!(
                    "HTTP client using system proxy: {} (private-range bypass enabled)",
                    proxy_url
                );
                builder = builder.proxy(proxy);
            }
            Err(e) => log::warn!(
                "Failed to apply detected system proxy {}: {}; continuing without proxy",
                proxy_url,
                e
            ),
        }
    }

    builder.build().unwrap_or_else(|e| {
        log::warn!("Failed to build HTTP client: {e}; falling back to default client");
        let mut fallback = reqwest::blocking::Client::builder();
        if let Some(read_timeout) = read_timeout {
            fallback = fallback.timeout(read_timeout);
        }
        let fallback = if follow_redirects {
            fallback
        } else {
            fallback.redirect(reqwest::redirect::Policy::none())
        };
        fallback
            .build()
            .expect("default reqwest client configuration must be valid")
    })
}

/// Hosts and ranges that must bypass any system proxy and connect directly.
///
/// A user with a global SOCKS/HTTP proxy still needs to reach a self-hosted
/// model server on loopback, their LAN, or a CGNAT/Tailscale address. The list
/// combines hard-coded private/loopback ranges, the `NO_PROXY` environment
/// variable, and the macOS `scutil` ExceptionsList.
fn build_no_proxy() -> Option<reqwest::NoProxy> {
    reqwest::NoProxy::from_string(&build_no_proxy_list().join(","))
}

fn build_no_proxy_list() -> Vec<String> {
    let mut entries: Vec<String> = [
        "localhost",
        "127.0.0.0/8",
        "::1",
        "fc00::/7",
        "fe80::/10",
        "169.254.0.0/16",
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "100.64.0.0/10",
        ".local",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    for var in ["NO_PROXY", "no_proxy"] {
        if let Ok(v) = std::env::var(var) {
            entries.extend(
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            );
        }
    }

    entries.extend(config::proxy::system_proxy_exceptions());

    entries
}

/// Process-level HTTP client shared across all overlay sessions.
///
/// TLS stack is initialized once; subsequent `AiClient::new` calls are free.
pub(super) fn shared_http_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| build_client_with_proxy(std::time::Duration::from_secs(600)))
}

pub(super) fn read_response_body_capped(
    response: reqwest::blocking::Response,
    provider_label: &str,
) -> Result<Vec<u8>> {
    read_body_capped(response, MAX_RESPONSE_BODY_BYTES, provider_label)
}

pub(super) fn read_body_capped(
    reader: impl Read,
    max_bytes: usize,
    provider_label: &str,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {provider_label} response body"))?;
    if bytes.len() > max_bytes {
        anyhow::bail!("{provider_label} response exceeded {} bytes", max_bytes);
    }
    Ok(bytes)
}

pub(super) fn read_error_response_preview(
    response: reqwest::blocking::Response,
    max_chars: usize,
) -> String {
    let mut bytes = Vec::new();
    let _ = response
        .take(MAX_ERROR_BODY_BYTES as u64)
        .read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes)
        .chars()
        .take(max_chars)
        .collect()
}

pub(super) fn read_sse_line_capped<R: BufRead>(
    reader: &mut R,
    provider_label: &str,
) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .with_context(|| format!("read {provider_label} SSE line"))?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let new_len = line
            .len()
            .checked_add(take)
            .ok_or_else(|| anyhow::anyhow!("{provider_label} SSE line length overflowed"))?;
        if new_len > MAX_RESPONSE_SSE_LINE_BYTES {
            anyhow::bail!(
                "{provider_label} SSE line exceeded {} bytes",
                MAX_RESPONSE_SSE_LINE_BYTES
            );
        }
        let found_newline = available.get(take.saturating_sub(1)) == Some(&b'\n');
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if found_newline {
            return Ok(Some(line));
        }
    }
}

pub(super) fn add_stream_bytes(
    total: &mut usize,
    amount: usize,
    provider_label: &str,
) -> Result<()> {
    *total = total
        .checked_add(amount)
        .ok_or_else(|| anyhow::anyhow!("{provider_label} stream size overflowed"))?;
    if *total > MAX_RESPONSE_STREAM_BYTES {
        anyhow::bail!(
            "{provider_label} stream exceeded {} bytes",
            MAX_RESPONSE_STREAM_BYTES
        );
    }
    Ok(())
}

pub(super) fn add_stream_event(total: &mut usize, provider_label: &str) -> Result<()> {
    *total += 1;
    if *total > MAX_RESPONSE_STREAM_EVENTS {
        anyhow::bail!(
            "{provider_label} stream exceeded {} events",
            MAX_RESPONSE_STREAM_EVENTS
        );
    }
    Ok(())
}

/// Waits between HTTP attempts without holding up cancellation.
pub(super) fn wait_before_request(
    cancelled: &AtomicBool,
    backoff: std::time::Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + backoff;
    loop {
        // Check even for the first attempt and after the final sleep, so a
        // cancellation at the backoff boundary cannot start another request.
        if cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled before HTTP request");
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(50)));
    }
}

/// Send a request up to `max_attempts` times with exponential backoff on transient
/// failures (network errors, HTTP 429, HTTP 5xx). Non-retryable HTTP errors
/// (4xx other than 429) bail immediately so misconfiguration surfaces fast.
///
/// `provider_label` is folded into log lines and the final error message so a
/// user reading logs can tell which transport failed.
pub(super) fn send_with_retry(
    req: reqwest::blocking::RequestBuilder,
    provider_label: &str,
    cancelled: &AtomicBool,
    max_attempts: u32,
) -> Result<reqwest::blocking::Response> {
    let mut last_err = String::new();
    let max_attempts = max_attempts.max(1);
    for attempt in 0..max_attempts {
        let backoff = if attempt == 0 {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs(1 << attempt)
        };
        wait_before_request(cancelled, backoff)?;
        let r = match req.try_clone().context("clone request")?.send() {
            Ok(r) => r,
            Err(e) => {
                // Provider URLs can contain secret query parameters. Keep the
                // transport error while removing the URL before logs/UI see it.
                last_err = e.without_url().to_string();
                log::warn!(
                    "{} HTTP attempt {}: {}",
                    provider_label,
                    attempt + 1,
                    last_err
                );
                continue;
            }
        };
        let status = r.status();
        if status.is_success() {
            return Ok(r);
        }
        let code = status.as_u16();
        let body = read_error_response_preview(r, MAX_ERROR_BODY_BYTES);
        if code == 429 || code >= 500 {
            let preview: String = body.chars().take(200).collect();
            last_err = format!("{} error {}: {}", provider_label, code, preview);
            log::warn!(
                "{} HTTP attempt {} retryable: {}",
                provider_label,
                attempt + 1,
                last_err
            );
            continue;
        }
        anyhow::bail!("{} error {}: {}", provider_label, code, body);
    }
    Err(anyhow::anyhow!(
        "{} request failed after {} attempts: {}",
        provider_label,
        max_attempts,
        last_err
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        add_stream_bytes, add_stream_event, build_no_proxy_list, read_body_capped,
        read_sse_line_capped,
    };
    use crate::ai_client::{
        MAX_MODELS_BODY_BYTES, MAX_RESPONSE_SSE_LINE_BYTES, MAX_RESPONSE_STREAM_BYTES,
        MAX_RESPONSE_STREAM_EVENTS,
    };
    use std::io::Cursor;
    use std::sync::atomic::AtomicBool;
    #[test]
    fn retry_wait_checks_cancellation_before_first_attempt() {
        let cancelled = AtomicBool::new(true);
        assert!(super::wait_before_request(&cancelled, std::time::Duration::ZERO).is_err());
        assert!(
            super::wait_before_request(&AtomicBool::new(false), std::time::Duration::ZERO).is_ok()
        );
    }

    #[test]
    fn retry_wait_checks_cancellation_after_final_sleep() {
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            // Stay under the 50 ms sleep slice so this is still the final
            // sleep, but leave a loaded CI runner time to schedule the setter.
            assert!(
                super::wait_before_request(&cancelled, std::time::Duration::from_millis(45))
                    .is_err()
            );
        });
    }

    #[test]
    fn retry_wait_interrupts_long_backoff() {
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let start = std::time::Instant::now();
            assert!(
                super::wait_before_request(&cancelled, std::time::Duration::from_secs(2)).is_err()
            );
            assert!(start.elapsed() < std::time::Duration::from_secs(1));
        });
    }

    #[test]
    fn models_body_reader_rejects_oversized_success_payload() {
        let oversized = vec![b'x'; MAX_MODELS_BODY_BYTES + 1];
        let error = read_body_capped(Cursor::new(oversized), MAX_MODELS_BODY_BYTES, "models API")
            .expect_err("oversized model lists must be rejected before JSON parsing");
        assert!(error.to_string().contains("exceeded"));
    }

    #[test]
    fn sse_line_reader_rejects_unterminated_oversized_line() {
        let oversized = vec![b'x'; MAX_RESPONSE_SSE_LINE_BYTES + 1];
        let error = read_sse_line_capped(&mut Cursor::new(oversized), "test")
            .expect_err("oversized line must fail before it is returned");
        assert!(error.to_string().contains("SSE line exceeded"));
    }

    #[test]
    fn stream_budget_rejects_excess_bytes_and_events() {
        let mut bytes = MAX_RESPONSE_STREAM_BYTES;
        assert!(add_stream_bytes(&mut bytes, 1, "test").is_err());
        let mut events = MAX_RESPONSE_STREAM_EVENTS;
        assert!(add_stream_event(&mut events, "test").is_err());
    }

    #[test]
    fn no_proxy_list_includes_private_and_local_model_hosts() {
        let entries = build_no_proxy_list();
        for expected in [
            "localhost",
            "127.0.0.0/8",
            "::1",
            "169.254.0.0/16",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "100.64.0.0/10",
            ".local",
        ] {
            assert!(
                entries.iter().any(|entry| entry == expected),
                "missing no-proxy entry {}",
                expected
            );
        }
    }

    fn proxy_received_connection(url: &str) -> bool {
        let fake = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        fake.set_nonblocking(true).unwrap();
        let proxy = reqwest::Proxy::all(format!("http://{}", fake.local_addr().unwrap()))
            .unwrap()
            .no_proxy(super::build_no_proxy());
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .proxy(proxy)
            .connect_timeout(std::time::Duration::from_millis(300))
            .timeout(std::time::Duration::from_millis(500))
            .build()
            .unwrap();
        let _ = client.get(url).send();
        match fake.accept() {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("fake proxy accept failed: {}", e),
        }
    }

    #[test]
    fn ipv6_unique_local_host_bypasses_proxy() {
        assert!(!proxy_received_connection("http://[fd7a:115c:a1e0::5]:9/"));
    }

    #[test]
    fn ipv6_link_local_host_bypasses_proxy() {
        assert!(!proxy_received_connection("http://[fe80::1]:9/"));
    }

    #[test]
    fn public_ipv6_host_still_uses_proxy() {
        assert!(proxy_received_connection("http://[2001:db8::1]:9/"));
    }
}
