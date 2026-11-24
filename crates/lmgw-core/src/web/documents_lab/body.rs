//! The run's request body: the `multipart/form-data` a client would send,
//! streamed from the kept files on the disk in 1 MiB reads instead of built
//! whole in memory (a `Vec` of every file, and a second one of the body, was
//! up to three times the files). The length is known up front from the
//! files' sizes, so the route's own `max_body_mb` check sees the declared
//! length as it does for a client.

use axum::body::{Body, Bytes};
use futures::stream::{self, StreamExt};
use tokio::io::AsyncReadExt;

use super::super::image_lab::multipart::header_safe;

/// How much of a file one read takes.
const READ_BLOCK: usize = 1 << 20;

/// One file part: where its bytes are and what to call it.
pub(super) struct PartFile {
    pub field: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub path: std::path::PathBuf,
    pub size: u64,
}

/// The body and its exact length.
pub(super) struct Streamed {
    pub body: Body,
    pub len: u64,
}

fn file_head(boundary: &str, f: &PartFile) -> Vec<u8> {
    let ct = f
        .content_type
        .as_deref()
        .map(header_safe)
        .unwrap_or_else(|| "application/octet-stream".into());
    format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n\
         Content-Type: {ct}\r\n\r\n",
        f.field,
        header_safe(&f.filename)
    )
    .into_bytes()
}

/// The text fields in the order given, then the files, then the closing
/// boundary.
pub(super) fn multipart_stream(
    boundary: &str,
    fields: &[(String, String)],
    files: Vec<PartFile>,
) -> Streamed {
    let mut head: Vec<u8> = Vec::new();
    for (name, value) in fields {
        head.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    let tail = format!("--{boundary}--\r\n").into_bytes();
    let mut len = (head.len() + tail.len()) as u64;
    let mut pieces: Vec<Piece> = vec![Piece::Bytes(head)];
    for f in files {
        let h = file_head(boundary, &f);
        len += h.len() as u64 + f.size + 2;
        pieces.push(Piece::Bytes(h));
        let size = f.size;
        pieces.push(Piece::File(f.path, size));
        pieces.push(Piece::Bytes(b"\r\n".to_vec()));
    }
    pieces.push(Piece::Bytes(tail));
    let s = stream::iter(pieces).flat_map(|p| match p {
        Piece::Bytes(b) => {
            stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(b)) }).boxed()
        }
        Piece::File(path, size) => file_chunks(path, size).boxed(),
    });
    Streamed {
        body: Body::from_stream(s),
        len,
    }
}

enum Piece {
    Bytes(Vec<u8>),
    File(std::path::PathBuf, u64),
}

/// A file as chunks of [`READ_BLOCK`]: exactly `size` bytes, or an error
/// when the file is shorter or unreadable (never a body that disagrees with
/// its declared length).
fn file_chunks(
    path: std::path::PathBuf,
    size: u64,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> {
    stream::unfold(
        (None::<tokio::fs::File>, path, size, false),
        |(file, path, left, failed)| async move {
            if failed || left == 0 {
                return None;
            }
            let mut file = match file {
                Some(f) => f,
                None => match tokio::fs::File::open(&path).await {
                    Ok(f) => f,
                    Err(e) => return Some((Err(e), (None, path, left, true))),
                },
            };
            let want = left.min(READ_BLOCK as u64) as usize;
            let mut buf = vec![0u8; want];
            match file.read_exact(&mut buf).await {
                Ok(_) => Some((
                    Ok(Bytes::from(buf)),
                    (Some(file), path, left - want as u64, false),
                )),
                Err(e) => Some((Err(e), (None, path, left, true))),
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_stream_is_the_multipart_a_buffer_would_build() {
        let dir = std::env::temp_dir().join(format!("lmgw-body-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big: Vec<u8> = (0..(READ_BLOCK * 2 + 5)).map(|i| (i % 251) as u8).collect();
        let p = dir.join("big");
        std::fs::write(&p, &big).unwrap();
        let fields = vec![("output".to_string(), "text".to_string())];
        let files = vec![PartFile {
            field: "file".into(),
            filename: "a\"b.pdf".into(),
            content_type: Some("application/pdf".into()),
            path: p,
            size: big.len() as u64,
        }];
        let s = multipart_stream("BOUND", &fields, files);
        let got = axum::body::to_bytes(s.body, usize::MAX).await.unwrap();
        assert_eq!(got.len() as u64, s.len, "the declared length is exact");
        let t = String::from_utf8_lossy(&got[..200]).to_string();
        assert!(t.starts_with("--BOUND\r\nContent-Disposition: form-data; name=\"output\""));
        assert!(t.contains("filename=\"ab.pdf\""));
        assert!(got.ends_with(b"--BOUND--\r\n"));
        let at = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        assert!(got.windows(big.len()).any(|w| w == big.as_slice()), "{at}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
