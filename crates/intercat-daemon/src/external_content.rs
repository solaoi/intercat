//! Explicitly reviewed display-side destinations. No URL is fetched during
//! review, persisted in the journal, or forwarded through a redirect.
use crate::{Result, secret_scan};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Cursor,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore, watch};
use uuid::Uuid;

const IMAGE_BYTES: usize = 4 * 1024 * 1024;
const REVIEW_LIFETIME: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Image,
    Link,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub kind: Kind,
    pub url: String,
}

#[derive(Clone, Serialize)]
pub struct Review {
    pub id: Uuid,
    pub kind: Kind,
    pub url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub data_url: String,
}

struct Pending {
    review: Review,
    created: Instant,
}
#[derive(Clone)]
pub struct Service {
    client: reqwest::Client,
    pending: Arc<Mutex<HashMap<Uuid, Pending>>>,
    scans: Arc<Semaphore>,
}

fn destination(request: &Request) -> Result<reqwest::Url> {
    if request.url.is_empty()
        || request.url.len() > 8192
        || request.url.chars().any(char::is_control)
    {
        return Err("This destination is not a valid URL.".into());
    }
    let url =
        reqwest::Url::parse(&request.url).map_err(|_| "This destination is not a valid URL.")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs containing credentials cannot be opened.".into());
    }
    match (request.kind, url.scheme()) {
        (_, "http" | "https") if url.host_str().is_some() => {}
        (Kind::Link, "mailto") if !url.path().is_empty() => {}
        _ => return Err("This URL scheme cannot be opened.".into()),
    }
    Ok(url)
}

fn scan_variants(raw: &str, url: &reqwest::Url) -> Result<Vec<String>> {
    let mut variants = vec![raw.to_owned(), url.as_str().to_owned()];
    let mut text = url.as_str().to_owned();
    for _ in 0..8 {
        let decoded = percent_encoding::percent_decode_str(&text)
            .decode_utf8()
            .map_err(|_| "This URL has unsupported text encoding.")?
            .into_owned();
        if decoded.chars().any(char::is_control) {
            return Err("URLs containing control characters cannot be opened.".into());
        }
        if decoded == text {
            return Ok(variants);
        }
        text = decoded;
        variants.push(text.clone());
    }
    Err("This URL has too many encoding layers.".into())
}

impl Service {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .referer(false)
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .user_agent("Intercat")
                .build()?,
            pending: Arc::new(Mutex::new(HashMap::new())),
            scans: Arc::new(Semaphore::new(2)),
        })
    }
    pub async fn review(&self, request: Request) -> Result<Review> {
        let url = destination(&request)?;
        let variants = scan_variants(&request.url, &url)?;
        let _permit = self.scans.acquire().await?;
        let scanner =
            tokio::task::spawn_blocking(|| secret_scan::Engine::capture()?.start(&[])).await??;
        if scanner.redact(&variants, watch::channel(false).1).await? != variants {
            return Err("This URL may contain a secret. It was not opened.".into());
        }
        let review = Review {
            id: Uuid::now_v7(),
            kind: request.kind,
            url: url.into(),
        };
        let mut pending = self.pending.lock().await;
        pending.retain(|_, item| item.created.elapsed() < REVIEW_LIFETIME);
        if pending.len() >= 64 {
            return Err("Close an open URL confirmation before opening another.".into());
        }
        pending.insert(
            review.id,
            Pending {
                review: review.clone(),
                created: Instant::now(),
            },
        );
        Ok(review)
    }
    pub async fn cancel(&self, id: Uuid) {
        self.pending.lock().await.remove(&id);
    }
    async fn take(&self, id: Uuid, kind: Kind) -> Result<Review> {
        let item = self
            .pending
            .lock()
            .await
            .remove(&id)
            .ok_or("Review this URL again before opening it.")?;
        if item.review.kind != kind || item.created.elapsed() >= REVIEW_LIFETIME {
            return Err("Review this URL again before opening it.".into());
        }
        Ok(item.review)
    }
    pub async fn link(&self, id: Uuid) -> Result<String> {
        Ok(self.take(id, Kind::Link).await?.url)
    }
    pub async fn image(&self, id: Uuid) -> Result<Image> {
        let review = self.take(id, Kind::Image).await?;
        let mut response = self
            .client
            .get(&review.url)
            .send()
            .await
            .map_err(|_| "The image could not be retrieved.")?;
        if !response.status().is_success() {
            return Err("The image was not returned. Redirects are not followed.".into());
        }
        if response
            .content_length()
            .is_some_and(|size| size > IMAGE_BYTES as u64)
        {
            return Err("The image exceeds the 4 MiB preview limit.".into());
        }
        let svg = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("image/svg+xml"))
            });
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "The image download was interrupted.")?
        {
            if bytes.len().saturating_add(chunk.len()) > IMAGE_BYTES {
                return Err("The image exceeds the 4 MiB preview limit.".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let mime = if svg {
            "image/svg+xml"
        } else {
            let reader = image::ImageReader::new(Cursor::new(&bytes))
                .with_guessed_format()
                .map_err(|_| "The response is not a supported image.")?;
            let format = reader
                .format()
                .ok_or("The response is not a supported image.")?;
            if !matches!(
                format,
                image::ImageFormat::Png
                    | image::ImageFormat::Jpeg
                    | image::ImageFormat::Gif
                    | image::ImageFormat::WebP
            ) {
                return Err("The response is not a supported image.".into());
            }
            let (width, height) = reader
                .into_dimensions()
                .map_err(|_| "The image header is invalid.")?;
            if width == 0
                || height == 0
                || width > 8192
                || height > 8192
                || u64::from(width) * u64::from(height) > 16_000_000
            {
                return Err("The image exceeds the preview dimensions.".into());
            }
            format.to_mime_type()
        };
        Ok(Image {
            data_url: format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn expired_confirmation_is_consumed_without_network_access() -> Result<()> {
        let service = Service::new()?;
        let id = Uuid::now_v7();
        service.pending.lock().await.insert(
            id,
            Pending {
                review: Review {
                    id,
                    kind: Kind::Image,
                    url: "http://127.0.0.1:1/never".into(),
                },
                created: Instant::now() - REVIEW_LIFETIME,
            },
        );
        assert!(
            service
                .image(id)
                .await
                .unwrap_err()
                .to_string()
                .contains("Review this URL again")
        );
        assert!(service.pending.lock().await.is_empty());
        Ok(())
    }
}
