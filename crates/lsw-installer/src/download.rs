//! Windows and driver media acquisition.
//!
//! * **Consumer Windows 11** (Home/Pro multi-edition ISO) through Microsoft's
//!   software-download API, the same flow as the MIT-licensed Mido / quickget
//!   projects. The ISO is verified against the SHA-256 table Microsoft
//!   publishes on the download page.
//! * **Windows 11 Enterprise Evaluation** from the Evaluation Center (used by
//!   automated tests; no product key needed).
//! * **virtio-win** drivers, pinned by version and SHA-256.
//!
//! Every response is untrusted: sizes are bounded, extracted values are
//! filtered, and downloads are only accepted over HTTPS from Microsoft or
//! Fedora hosts.

use futures_util::StreamExt;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Browser user agent expected by Microsoft's download endpoints.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:100.0) Gecko/20100101 Firefox/100.0";
/// Honest user agent for every other host. fedorapeople.org answers browser
/// user agents with an anti-bot challenge page instead of the file.
const LSW_USER_AGENT: &str = concat!("lsw/", env!("CARGO_PKG_VERSION"));
const MAX_REDIRECTS: usize = 10;
const WIN11_PAGE: &str = "https://www.microsoft.com/en-us/software-download/windows11";
const EVAL_PAGE: &str = "https://www.microsoft.com/en-us/evalcenter/download-windows-11-enterprise";
const PROFILE: &str = "606624d44113";
const MAX_PAGE: usize = 2 << 20;

/// Make sure `dest` holds the file with `sha256` (pinned): a cached copy is
/// re-hashed on every use and replaced if it does not match.
pub async fn ensure_pinned(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    sha256: &str,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<(), DownloadError> {
    if dest.exists() {
        if sha256_file(dest).await?.eq_ignore_ascii_case(sha256) {
            return Ok(());
        }
        tracing::warn!(
            "{} does not match its pinned SHA-256; downloading again",
            dest.display()
        );
        tokio::fs::remove_file(dest).await?;
    }
    download(client, url, dest, Some(sha256), progress)
        .await
        .map(|_| ())
}

/// Pinned WinFsp release (GPLv3 with FLOSS exception), installed in the
/// guest for virtio-fs shared folders. Downloaded from upstream, never
/// bundled.
pub const WINFSP_VERSION: &str = "2.1.25156";
pub const WINFSP_URL: &str =
    "https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi";
pub const WINFSP_SHA256: &str = "073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a";

/// Pinned virtio-win release (Fedora, BSD-3-Clause drivers).
pub const VIRTIO_WIN_VERSION: &str = "0.1.302";
pub const VIRTIO_WIN_URL: &str = "https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/archive-virtio/virtio-win-0.1.302-1/virtio-win-0.1.302.iso";
pub const VIRTIO_WIN_SHA256: &str =
    "303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d";

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "Microsoft refused the automated download from this network (common on VPNs and cloud machines). Download the ISO in a browser from {WIN11_PAGE}, then run `lsw create --source iso --iso <file>`"
    )]
    Blocked,
    #[error("unexpected response from {what}: {detail}")]
    Unexpected { what: &'static str, detail: String },
    #[error("refusing to download from untrusted URL {0}")]
    UntrustedUrl(String),
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    Checksum { expected: String, actual: String },
}

/// A resolved download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIso {
    pub url: String,
    pub file_name: String,
    /// Upper-case hex SHA-256 when the publisher provides one.
    pub sha256: Option<String>,
}

/// HTTP client type used by this module.
pub type Client = reqwest::Client;

pub fn client() -> Result<Client, DownloadError> {
    // Every redirect hop must stay on a trusted host, not just the first URL.
    let redirects = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if check_trusted(attempt.url().as_str()).is_ok() {
            attempt.follow()
        } else {
            let url = attempt.url().to_string();
            attempt.error(DownloadError::UntrustedUrl(url))
        }
    });
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(redirects)
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()?)
}

/// Only HTTPS URLs on Microsoft, Fedora or GitHub release hosts are accepted.
fn is_microsoft(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.host_str()
            .is_some_and(|h| h == "microsoft.com" || h.ends_with(".microsoft.com"))
    })
}

pub fn check_trusted(url: &str) -> Result<(), DownloadError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| DownloadError::UntrustedUrl(url.into()))?;
    let host = parsed.host_str().unwrap_or_default();
    let trusted = parsed.scheme() == "https"
        && (host == "microsoft.com"
            || host.ends_with(".microsoft.com")
            || host == "fedorapeople.org"
            || host == "github.com"
            || host == "release-assets.githubusercontent.com");
    if trusted {
        Ok(())
    } else {
        Err(DownloadError::UntrustedUrl(url.into()))
    }
}

async fn get_text(
    client: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
) -> Result<String, DownloadError> {
    let mut req = client.get(url);
    if let Some(r) = referer {
        req = req.header(reqwest::header::REFERER, r);
    }
    let resp = req.send().await?.error_for_status()?;
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > MAX_PAGE {
            return Err(DownloadError::Unexpected {
                what: url_label(url),
                detail: "response too large".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn url_label(url: &str) -> &'static str {
    if url.contains("fedorapeople.org") {
        "virtio-win download"
    } else if url.contains("github") {
        "WinFsp download"
    } else if url.contains("evalcenter") {
        "Evaluation Center"
    } else if url.contains("software-download-connector") {
        "Microsoft download API"
    } else {
        "Microsoft download page"
    }
}

/// Product edition id of the newest multi-edition ISO, from the page HTML.
pub fn parse_product_edition_id(html: &str) -> Option<String> {
    let re = Regex::new(r#"<option value="([0-9]{1,16})">Windows"#).expect("static regex");
    re.captures(html).map(|c| c[1].to_owned())
}

/// `language → SHA-256` table published on the download page.
pub fn parse_hash_table(html: &str) -> Vec<(String, String)> {
    let re = Regex::new(r"<td>([A-Za-z ()]{1,48}) 64-bit</td>\s*<td>([A-Fa-f0-9]{64})</td>")
        .expect("static regex");
    re.captures_iter(html)
        .map(|c| (c[1].trim().to_owned(), c[2].to_ascii_uppercase()))
        .collect()
}

/// SKU id for `language` from `getskuinformationbyproductedition` JSON.
pub fn parse_sku_id(json: &serde_json::Value, language: &str) -> Option<String> {
    json.get("Skus")?.as_array()?.iter().find_map(|sku| {
        let matches = sku.get("Language").and_then(|v| v.as_str()) == Some(language)
            || sku.get("LocalizedLanguage").and_then(|v| v.as_str()) == Some(language);
        if !matches {
            return None;
        }
        let id = sku.get("Id")?.as_str()?;
        id.chars()
            .all(|c| c.is_ascii_digit())
            .then(|| id.to_owned())
    })
}

/// x64 ISO URL from `GetProductDownloadLinksBySku` JSON.
pub fn parse_download_link(json: &serde_json::Value) -> Option<String> {
    json.get("ProductDownloadOptions")?
        .as_array()?
        .iter()
        .filter_map(|o| o.get("Uri")?.as_str())
        .find(|u| u.contains("x64"))
        .map(str::to_owned)
}

/// File name of a download URL (last path segment, query stripped).
pub fn file_name_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let name = parsed.path_segments()?.next_back()?.to_owned();
    let ok = !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    ok.then_some(name)
}

/// Resolve the consumer Windows 11 ISO for `language` (e.g. `English`,
/// `English International`).
pub async fn resolve_consumer(
    client: &reqwest::Client,
    language: &str,
) -> Result<ResolvedIso, DownloadError> {
    let page = get_text(client, WIN11_PAGE, None).await?;
    let edition = parse_product_edition_id(&page).ok_or(DownloadError::Unexpected {
        what: "Microsoft download page",
        detail: "no product edition id".into(),
    })?;
    let sha256 = parse_hash_table(&page)
        .into_iter()
        .find(|(lang, _)| lang == language)
        .map(|(_, h)| h);

    let session = uuid::Uuid::new_v4().to_string();
    client
        .get(format!(
            "https://vlscppe.microsoft.com/tags?org_id=y6jn8c31&session_id={session}"
        ))
        .send()
        .await?
        .error_for_status()?;

    let skus_url = format!(
        "https://www.microsoft.com/software-download-connector/api/getskuinformationbyproductedition?profile={PROFILE}&ProductEditionId={edition}&SKU=undefined&friendlyFileName=undefined&Locale=en-US&sessionID={session}"
    );
    let skus: serde_json::Value = serde_json::from_str(&get_text(client, &skus_url, None).await?)
        .map_err(|e| DownloadError::Unexpected {
        what: "Microsoft download API",
        detail: e.to_string(),
    })?;
    let sku = parse_sku_id(&skus, language).ok_or_else(|| DownloadError::Unexpected {
        what: "Microsoft download API",
        detail: format!("no SKU for language {language:?}"),
    })?;

    let links_url = format!(
        "https://www.microsoft.com/software-download-connector/api/GetProductDownloadLinksBySku?profile={PROFILE}&productEditionId=undefined&SKU={sku}&friendlyFileName=undefined&Locale=en-US&sessionID={session}"
    );
    let text = get_text(client, &links_url, Some(WIN11_PAGE)).await?;
    if text.contains("Sentinel marked this request as rejected") {
        return Err(DownloadError::Blocked);
    }
    let links: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| DownloadError::Unexpected {
            what: "Microsoft download API",
            detail: e.to_string(),
        })?;
    let url = parse_download_link(&links).ok_or(DownloadError::Unexpected {
        what: "Microsoft download API",
        detail: "no x64 download link".into(),
    })?;
    check_trusted(&url)?;
    let file_name = file_name_of(&url).unwrap_or_else(|| "Win11_x64.iso".into());
    Ok(ResolvedIso {
        url,
        file_name,
        sha256,
    })
}

/// Evaluation Center link for `culture` (e.g. `en-US`).
pub fn parse_eval_link(html: &str, culture: &str) -> Option<String> {
    let re = Regex::new(
        r#"data-target="(https://go\.microsoft\.com/fwlink/\?LinkId=[0-9]{1,10}&amp;clcid=0x[0-9a-f]{1,4}&amp;culture=[a-z-]{2,10}&amp;country=[a-z]{2})" aria-label="[^"]{0,80}Download Windows 11 Enterprise ISO 64-bit \(([A-Za-z-]{2,10})\)""#,
    )
    .expect("static regex");
    re.captures_iter(html)
        .find(|c| c[2].eq_ignore_ascii_case(culture))
        .map(|c| c[1].replace("&amp;", "&"))
}

/// Resolve the Windows 11 Enterprise Evaluation ISO.
pub async fn resolve_evaluation(
    client: &reqwest::Client,
    culture: &str,
) -> Result<ResolvedIso, DownloadError> {
    let page = get_text(client, EVAL_PAGE, None).await?;
    let fwlink = parse_eval_link(&page, culture).ok_or_else(|| DownloadError::Unexpected {
        what: "Evaluation Center",
        detail: format!("no Enterprise x64 link for {culture}"),
    })?;
    // Follow the fwlink redirect to learn the real file name.
    let resp = client.head(&fwlink).send().await?.error_for_status()?;
    let url = resp.url().to_string();
    check_trusted(&url)?;
    let file_name = file_name_of(&url).ok_or_else(|| DownloadError::Unexpected {
        what: "Evaluation Center",
        detail: format!("odd file URL {url}"),
    })?;
    Ok(ResolvedIso {
        url,
        file_name,
        sha256: None,
    })
}

/// Download `url` to `dest`, resuming a previous `.part` file, verifying
/// `expected_sha256` when given. Returns the SHA-256 (upper-case hex).
/// `progress(done, total)` is called as data arrives.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_sha256: Option<&str>,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<String, DownloadError> {
    check_trusted(url)?;
    let part: PathBuf = dest.with_extension("part");
    let mut hasher = Sha256::new();
    let mut have: u64 = 0;
    if let Ok(mut f) = tokio::fs::File::open(&part).await {
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            have += n as u64;
        }
    }

    let mut req = client.get(url);
    if !is_microsoft(url) {
        req = req.header(reqwest::header::USER_AGENT, LSW_USER_AGENT);
    }
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let resp = req.send().await?.error_for_status()?;
    // Disk images are never HTML; an HTML 200 is a bot check or error page.
    let is_html = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"));
    if is_html {
        return Err(DownloadError::Unexpected {
            what: url_label(url),
            detail: "the server returned a web page instead of the file".into(),
        });
    }
    let resumed = have > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if have > 0 && !resumed {
        // Server ignored the range: start over.
        hasher = Sha256::new();
        have = 0;
    }
    let total = resp.content_length().map(|l| l + have);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&part)
        .await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        have += chunk.len() as u64;
        progress(have, total);
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    let actual = hex::encode_upper(hasher.finalize());
    if let Some(expected) = expected_sha256
        && !expected.eq_ignore_ascii_case(&actual)
    {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(DownloadError::Checksum {
            expected: expected.to_ascii_uppercase(),
            actual,
        });
    }
    tokio::fs::rename(&part, dest).await?;
    Ok(actual)
}

/// SHA-256 of an existing file (for user-supplied ISOs and cache checks).
pub async fn sha256_file(path: &Path) -> Result<String, DownloadError> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode_upper(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_download_page() {
        let html = r#"<select><option value="" selected>Select</option><option value="3813">Windows 11 (multi-edition ISO for x64 devices)</option></select>
            <tr><td>English International 64-bit</td><td>7E3F373BD3C2321B5D5125DFCF718DFBF1A0ABC50A267118669951890DF98A5D</td></tr>
            <tr><td>English 64-bit</td><td>bd4307df32bc8af33b39ccecb1174aeb345386630f89a2b86c7a4e36b55ea650</td></tr>"#;
        assert_eq!(parse_product_edition_id(html).as_deref(), Some("3813"));
        let table = parse_hash_table(html);
        assert_eq!(table.len(), 2);
        assert_eq!(table[1].0, "English");
        assert_eq!(
            table[1].1,
            "BD4307DF32BC8AF33B39CCECB1174AEB345386630F89A2B86C7A4E36B55EA650"
        );
    }

    #[test]
    fn parses_api_json_defensively() {
        let skus = json!({"Skus": [
            {"Language": "Arabic", "LocalizedLanguage": "Arabic", "Id": "1"},
            {"Language": "English", "LocalizedLanguage": "English", "Id": "18935"},
            {"Language": "Evil", "Id": "1&x=y"}
        ]});
        assert_eq!(parse_sku_id(&skus, "English").as_deref(), Some("18935"));
        assert_eq!(
            parse_sku_id(&skus, "Evil"),
            None,
            "non-numeric ids are rejected"
        );
        let links = json!({"ProductDownloadOptions": [
            {"Uri": "https://software.download.prss.microsoft.com/dbazure/Win11_26H2_English_Arm64.iso?t=1"},
            {"Uri": "https://software.download.prss.microsoft.com/dbazure/Win11_26H2_English_x64.iso?t=2"}
        ]});
        let url = parse_download_link(&links).unwrap();
        assert!(url.contains("x64"));
        assert_eq!(
            file_name_of(&url).as_deref(),
            Some("Win11_26H2_English_x64.iso")
        );
    }

    #[test]
    fn parses_eval_center_links() {
        let html = r##"<a data-target="https://go.microsoft.com/fwlink/?LinkId=2382700&amp;clcid=0x409&amp;culture=en-us&amp;country=us" aria-label="64-bit edition: Download Windows 11 Enterprise ISO 64-bit (en-GB)" href="#">x</a>
            <a data-target="https://go.microsoft.com/fwlink/?LinkId=2382600&amp;clcid=0x409&amp;culture=en-us&amp;country=us" aria-label="64-bit edition: Download Windows 11 Enterprise ISO 64-bit (en-US)" href="#">y</a>"##;
        assert_eq!(
            parse_eval_link(html, "en-US").as_deref(),
            Some(
                "https://go.microsoft.com/fwlink/?LinkId=2382600&clcid=0x409&culture=en-us&country=us"
            )
        );
        assert!(parse_eval_link(html, "de-DE").is_none());
    }

    #[test]
    fn only_trusted_hosts() {
        assert!(check_trusted("https://software.download.prss.microsoft.com/x.iso").is_ok());
        assert!(check_trusted(VIRTIO_WIN_URL).is_ok());
        assert!(check_trusted(WINFSP_URL).is_ok());
        assert!(check_trusted("https://github.com.evil.example/x.msi").is_err());
        assert!(check_trusted("https://evil.githubusercontent.com/x.msi").is_err());
        assert!(check_trusted("http://software.download.prss.microsoft.com/x.iso").is_err());
        assert!(check_trusted("https://microsoft.com.evil.example/x.iso").is_err());
        assert!(is_microsoft(
            "https://software.download.prss.microsoft.com/x.iso"
        ));
        assert!(!is_microsoft(VIRTIO_WIN_URL));
        assert!(!is_microsoft("https://microsoft.com.evil.example/x.iso"));
        assert!(check_trusted("https://evilmicrosoft.com/x.iso").is_err());
        assert!(check_trusted("not a url").is_err());
    }

    #[test]
    fn file_names_are_sanitized() {
        assert_eq!(
            file_name_of("https://x.microsoft.com/a/b/c.iso?q=1").as_deref(),
            Some("c.iso")
        );
        assert_eq!(file_name_of("https://x.microsoft.com/a/..%2Fetc"), None);
        assert_eq!(file_name_of("https://x.microsoft.com/"), None);
    }

    #[tokio::test]
    async fn sha256_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).await.unwrap(),
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
        );
    }
}
