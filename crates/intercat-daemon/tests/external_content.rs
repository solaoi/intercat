use intercat_daemon::{
    Result,
    external_content::{Kind, Request, Service},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn endpoint(
    status: &str,
    headers: &str,
    body: Vec<u8>,
) -> Result<(String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let response = [
        format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes(),
        body,
    ]
    .concat();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            observed.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                let mut byte = [0];
                if stream.read_exact(&mut byte).await.is_err() {
                    break;
                }
                request.push(byte[0]);
            }
            let _ = stream.write_all(&response).await;
        }
    });
    Ok((format!("http://{address}/image"), count, task))
}

fn png() -> Vec<u8> {
    let image = image::DynamicImage::new_rgb8(2, 2);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

#[tokio::test]
async fn review_and_cancel_never_connect_and_confirmation_is_consumed_once() -> Result<()> {
    let (url, count, task) = endpoint("200 OK", "Content-Type: image/png\r\n", png()).await?;
    let service = Service::new()?;
    let cancelled = service
        .review(Request {
            kind: Kind::Image,
            url: url.clone(),
        })
        .await?;
    service.cancel(cancelled.id).await;
    assert!(service.image(cancelled.id).await.is_err());
    let review = service
        .review(Request {
            kind: Kind::Image,
            url,
        })
        .await?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let (first, second) = tokio::join!(service.image(review.id), service.image(review.id));
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let image = first.or(second)?;
    assert!(image.data_url.starts_with("data:image/png;base64,"));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn redirects_and_non_images_do_not_get_a_second_connection_or_retry() -> Result<()> {
    let (target, target_count, target_task) =
        endpoint("200 OK", "Content-Type: image/png\r\n", png()).await?;
    let (url, count, task) =
        endpoint("302 Found", &format!("Location: {target}\r\n"), vec![]).await?;
    let service = Service::new()?;
    let review = service
        .review(Request {
            kind: Kind::Image,
            url,
        })
        .await?;
    assert!(service.image(review.id).await.is_err());
    assert!(service.image(review.id).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(target_count.load(Ordering::SeqCst), 0);
    task.abort();
    target_task.abort();
    let (url, count, task) = endpoint(
        "200 OK",
        "Content-Type: image/png\r\n",
        b"<script>bad()</script>".to_vec(),
    )
    .await?;
    let review = service
        .review(Request {
            kind: Kind::Image,
            url,
        })
        .await?;
    assert!(service.image(review.id).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn secret_urls_and_encoded_secrets_are_rejected_before_connecting() -> Result<()> {
    let (url, count, task) = endpoint("200 OK", "Content-Type: image/png\r\n", png()).await?;
    let service = Service::new()?;
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let encoded: String = token.bytes().map(|byte| format!("%{byte:02X}")).collect();
    for value in [
        token.to_string(),
        encoded.clone(),
        encoded.replace('%', "%25"),
    ] {
        let result = service
            .review(Request {
                kind: Kind::Image,
                url: format!("{url}?token={value}"),
            })
            .await;
        assert!(result.is_err());
    }
    for url in [
        "file:///tmp/a.png",
        "javascript:alert(1)",
        "data:image/png;base64,aGVsbG8=",
        "https://user:password@example.invalid/image",
        "https://example.invalid/%0aheader",
    ] {
        assert!(
            service
                .review(Request {
                    kind: Kind::Image,
                    url: url.into()
                })
                .await
                .is_err()
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn confirmed_links_and_image_reviews_cannot_exchange_authority() -> Result<()> {
    let service = Service::new()?;
    let review = service
        .review(Request {
            kind: Kind::Link,
            url: "mailto:hello@example.invalid?subject=Example".into(),
        })
        .await?;
    assert!(service.image(review.id).await.is_err());
    assert!(service.link(review.id).await.is_err());
    let review = service
        .review(Request {
            kind: Kind::Link,
            url: "https://example.invalid/hello?q=world#note".into(),
        })
        .await?;
    assert_eq!(
        service.link(review.id).await?,
        "https://example.invalid/hello?q=world#note"
    );
    assert!(service.link(review.id).await.is_err());
    Ok(())
}

#[tokio::test]
async fn images_are_bounded_by_download_size_and_dimensions() -> Result<()> {
    let service = Service::new()?;
    let (url, count, task) = endpoint(
        "200 OK",
        "Content-Type: image/png\r\n",
        vec![0; 4 * 1024 * 1024 + 1],
    )
    .await?;
    let review = service
        .review(Request {
            kind: Kind::Image,
            url,
        })
        .await?;
    assert!(
        service
            .image(review.id)
            .await
            .unwrap_err()
            .to_string()
            .contains("4 MiB")
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    task.abort();
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(8193, 1).write_to(&mut bytes, image::ImageFormat::Png)?;
    let (url, count, task) =
        endpoint("200 OK", "Content-Type: image/png\r\n", bytes.into_inner()).await?;
    let review = service
        .review(Request {
            kind: Kind::Image,
            url,
        })
        .await?;
    assert!(
        service
            .image(review.id)
            .await
            .unwrap_err()
            .to_string()
            .contains("dimensions")
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    task.abort();
    Ok(())
}
