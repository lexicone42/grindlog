//! Resolve a Twitch channel to a raw HLS variant playlist URL — pure Rust,
//! no streamlink needed.
//!
//! Twitch's web player asks their GraphQL endpoint for a signed playback
//! access token, then fetches a master playlist from usher.ttvnw.net using
//! that token. We do the same two steps, pick a variant, and hand the variant
//! URL to ffmpeg, which speaks HLS natively.

use anyhow::{bail, Context, Result};

const GQL_URL: &str = "https://gql.twitch.tv/gql";
/// The public client-id of Twitch's own web player (the same one streamlink
/// sends). Not a secret, but Twitch does rotate it occasionally — if GQL
/// starts returning 400 "Client-ID header is invalid", grab the current one:
/// `curl -s https://www.twitch.tv/<channel> | grep -oE 'clientId="[a-z0-9]+"'`
const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

#[derive(Debug)]
pub enum Resolved {
    Live { variant_url: String, name: String },
    Offline,
}

pub async fn resolve(client: &reqwest::Client, channel: &str, quality: &str) -> Result<Resolved> {
    resolve_inner(client, Target::Live(channel), quality).await
}

/// Resolve a Twitch VOD (twitch.tv/videos/<id>) to a variant playlist URL.
pub async fn resolve_vod(
    client: &reqwest::Client,
    vod_id: &str,
    quality: &str,
) -> Result<Resolved> {
    resolve_inner(client, Target::Vod(vod_id), quality).await
}

enum Target<'a> {
    Live(&'a str),
    Vod(&'a str),
}

async fn resolve_inner(
    client: &reqwest::Client,
    target: Target<'_>,
    quality: &str,
) -> Result<Resolved> {
    let (is_live, login, vod_id) = match target {
        Target::Live(channel) => (true, channel, ""),
        Target::Vod(id) => (false, "", id),
    };
    let body = serde_json::json!({
        "operationName": "PlaybackAccessToken",
        "variables": {
            "isLive": is_live,
            "login": login,
            "isVod": !is_live,
            "vodID": vod_id,
            "playerType": "embed",
            "platform": "site"
        },
        "extensions": { "persistedQuery": {
            "version": 1,
            // Hash of the query streamlink currently uses; update from
            // streamlink's twitch.py if Twitch retires it.
            "sha256Hash": "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9"
        }}
    });
    let resp: serde_json::Value = client
        .post(GQL_URL)
        .header("Client-ID", CLIENT_ID)
        .json(&body)
        .send()
        .await
        .context("twitch gql request failed")?
        .error_for_status()
        .context("twitch gql returned an error status")?
        .json()
        .await
        .context("twitch gql returned non-json")?;

    // GQL reports failures as HTTP 200 with an `errors` array (a retired
    // persisted-query hash, a rotated client-id, rate limiting). Those are
    // NOT "offline" and must be loud, or a live stream is skipped forever.
    if let Some(errs) = resp["errors"].as_array().filter(|a| !a.is_empty()) {
        let msgs: Vec<&str> = errs.iter().filter_map(|e| e["message"].as_str()).collect();
        anyhow::bail!(
            "twitch gql error: {} (see src/twitch_hls.rs on updating the client-id / query hash)",
            msgs.join("; ")
        );
    }
    let token = match target {
        Target::Live(_) => &resp["data"]["streamPlaybackAccessToken"],
        Target::Vod(_) => &resp["data"]["videoPlaybackAccessToken"],
    };
    let (value, sig) = match (token["value"].as_str(), token["signature"].as_str()) {
        (Some(v), Some(s)) => (v, s),
        // No token: channel/vod doesn't exist or Twitch declined; treat as
        // offline and let the poll loop retry.
        _ => return Ok(Resolved::Offline),
    };

    let p = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 999_999)
        .unwrap_or(42);
    let path = match target {
        Target::Live(channel) => format!("api/channel/hls/{channel}.m3u8"),
        Target::Vod(id) => format!("vod/{id}.m3u8"),
    };
    let usher = format!(
        "https://usher.ttvnw.net/{path}\
         ?allow_source=true&allow_audio_only=false&fast_bread=true\
         &p={p}&player_backend=mediaplayer&playlist_include_framerate=true\
         &reassignments_supported=true&supported_codecs=h264\
         &sig={sig}&token={token}",
        token = urlencode(value)
    );

    let resp = client
        .get(&usher)
        .send()
        .await
        .context("usher request failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        // "transcode does not exist" — the channel is offline.
        return Ok(Resolved::Offline);
    }
    let master = resp
        .error_for_status()
        .context("usher returned an error status")?
        .text()
        .await?;
    let variants = parse_master(&master);
    if variants.is_empty() {
        bail!("no variants found in master playlist");
    }
    tracing::debug!(
        "available renditions: {:?}",
        variants
            .iter()
            .map(|v| (&v.name, v.bandwidth))
            .collect::<Vec<_>>()
    );
    let pick = pick_variant(&variants, quality)
        .unwrap_or_else(|| variants.iter().max_by_key(|v| v.bandwidth).unwrap());
    Ok(Resolved::Live {
        variant_url: pick.url.clone(),
        name: if pick.name.is_empty() {
            format!("{} bps", pick.bandwidth)
        } else {
            pick.name.clone()
        },
    })
}

/// Current broadcast title of a live channel (None when offline).
pub async fn stream_title(client: &reqwest::Client, channel: &str) -> Result<Option<String>> {
    let body = serde_json::json!({
        "query": format!("{{user(login:\"{}\"){{stream{{title}}}}}}", channel.trim())
    });
    let resp: serde_json::Value = client
        .post(GQL_URL)
        .header("Client-ID", CLIENT_ID)
        .json(&body)
        .send()
        .await
        .context("twitch gql request failed")?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp["data"]["user"]["stream"]["title"]
        .as_str()
        .map(|s| s.to_string()))
}

/// Fetch a VOD's broadcast start time (unix ms), used to put detected runs
/// on the original timeline.
pub async fn vod_created_at(client: &reqwest::Client, vod_id: &str) -> Result<Option<i64>> {
    let body = serde_json::json!({
        "query": format!("{{video(id:\"{}\"){{createdAt}}}}", vod_id.trim())
    });
    let resp: serde_json::Value = client
        .post(GQL_URL)
        .header("Client-ID", CLIENT_ID)
        .json(&body)
        .send()
        .await
        .context("twitch gql request failed")?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp["data"]["video"]["createdAt"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: String,
    pub bandwidth: u64,
    pub url: String,
}

pub fn parse_master(m3u8: &str) -> Vec<Variant> {
    let mut out = Vec::new();
    let mut pending: Option<(String, u64)> = None;
    for line in m3u8.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let bandwidth = attr(rest, "BANDWIDTH")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let name = attr(rest, "VIDEO").unwrap_or_default();
            pending = Some((name, bandwidth));
        } else if !line.starts_with('#') && !line.is_empty() {
            if let Some((name, bandwidth)) = pending.take() {
                out.push(Variant {
                    name,
                    bandwidth,
                    url: line.to_string(),
                });
            }
        }
    }
    out
}

pub fn pick_variant<'a>(variants: &'a [Variant], quality: &str) -> Option<&'a Variant> {
    match quality {
        "best" | "" => variants.iter().max_by_key(|v| v.bandwidth),
        "worst" => variants.iter().min_by_key(|v| v.bandwidth),
        name => variants
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(name))
            .or_else(|| {
                variants.iter().find(|v| {
                    v.name
                        .to_ascii_lowercase()
                        .contains(&name.to_ascii_lowercase())
                })
            }),
    }
}

/// Attribute lookup inside a `KEY=VALUE,KEY="quoted,value"` attribute list.
fn attr(s: &str, key: &str) -> Option<String> {
    let mut idx = 0;
    while let Some(pos) = s[idx..].find(key) {
        let abs = idx + pos;
        let at_boundary = abs == 0 || matches!(s.as_bytes()[abs - 1], b',' | b':' | b' ');
        let after = &s[abs + key.len()..];
        if at_boundary {
            if let Some(val) = after.strip_prefix('=') {
                return Some(if let Some(q) = val.strip_prefix('"') {
                    q.split('"').next().unwrap_or("").to_string()
                } else {
                    val.split(',').next().unwrap_or("").to_string()
                });
            }
        }
        idx = abs + key.len();
    }
    None
}

/// Minimal percent-encoding (RFC 3986 unreserved set kept literal).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = r#"#EXTM3U
#EXT-X-TWITCH-INFO:NODE="video-edge",MANIFEST-NODE="video-edge"
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="chunked",NAME="1080p60 (source)",AUTOSELECT=YES,DEFAULT=YES
#EXT-X-STREAM-INF:BANDWIDTH=6000000,AVERAGE-BANDWIDTH=5500000,RESOLUTION=1920x1080,CODECS="avc1.64002A,mp4a.40.2",VIDEO="chunked",FRAME-RATE=60.000
https://example.com/chunked/index.m3u8
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="720p60",NAME="720p60",AUTOSELECT=YES,DEFAULT=YES
#EXT-X-STREAM-INF:BANDWIDTH=3400000,RESOLUTION=1280x720,CODECS="avc1.4D401F,mp4a.40.2",VIDEO="720p60",FRAME-RATE=60.000
https://example.com/720p60/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=852x480,CODECS="avc1.4D401F,mp4a.40.2",VIDEO="480p30"
https://example.com/480p30/index.m3u8
"#;

    #[test]
    fn parses_variants() {
        let v = parse_master(MASTER);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].name, "chunked");
        assert_eq!(v[0].bandwidth, 6_000_000);
        assert_eq!(v[0].url, "https://example.com/chunked/index.m3u8");
        assert_eq!(v[2].name, "480p30");
    }

    #[test]
    fn bandwidth_not_confused_with_average_bandwidth() {
        let v = parse_master(MASTER);
        assert_eq!(v[0].bandwidth, 6_000_000); // not 5_500_000
    }

    #[test]
    fn picks_by_quality() {
        let v = parse_master(MASTER);
        assert_eq!(pick_variant(&v, "best").unwrap().name, "chunked");
        assert_eq!(pick_variant(&v, "worst").unwrap().name, "480p30");
        assert_eq!(pick_variant(&v, "720p60").unwrap().name, "720p60");
        assert_eq!(pick_variant(&v, "480p").unwrap().name, "480p30");
        assert!(pick_variant(&v, "4k").is_none());
    }

    #[test]
    fn urlencode_escapes_json_tokens() {
        assert_eq!(urlencode("a{b}"), "a%7Bb%7D");
        assert_eq!(urlencode("safe-._~09AZ"), "safe-._~09AZ");
    }
}
