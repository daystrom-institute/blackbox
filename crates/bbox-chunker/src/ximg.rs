use std::path::Path;

use anyhow::Result;

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

/// Chunk kind for standalone image files (X-IMG,
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
pub const IMAGE_CHUNK_KIND: &str = "image";

fn media_type_for_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Magic-byte confirmation for the extension's claimed media type. Extension
/// alone is spoofable (a renamed file); this is the same "extension +
/// bounded magic scan" idiom `pdf.rs`/`office.rs` use, specialized per image
/// format since each has its own fixed header shape.
fn magic_matches(media_type: &str, bytes: &[u8]) -> bool {
    match media_type {
        "image/png" => bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        // RIFF....WEBP: 4-byte "RIFF", 4-byte little-endian chunk size
        // (ignored), 4-byte "WEBP" form type.
        "image/webp" => bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

/// Standalone image chunker (X-IMG). Claims `.png`/`.jpg`/`.jpeg`/`.gif`/
/// `.webp` by extension + magic bytes, stores the raw bytes in the
/// content-hash-addressed visual payload sidecar (`bbox-visual-store`,
/// outside tantivy), and emits exactly ONE `image` chunk whose text content
/// is the file name stem. VLM caption extraction is explicitly out of
/// scope for this pass (design doc: "VLM caption extraction is optional
/// lexical enrichment, not a gate") - the file stem is the chunk's only
/// lexical content, matching the design's "minimal" allowance.
///
/// The resulting `VisualPayloadRef` rides the chunk two ways: directly on
/// `Chunk::visual_payload` (consumed by the index-time enqueue path, which
/// has the freshly-built `Chunk` in hand) and encoded into `Chunk::symbol`
/// (consumed by the backfill/reembed path, which reconstructs a `Chunk`
/// from stored tantivy fields - see `src/embed_runtime.rs`'s
/// `chunk_from_embedding_doc` - rather than re-chunking the file; `symbol`
/// is an existing plain-text field, so this needs no schema bump).
///
/// `DEPICTS`/`CAPTIONED_AS` edges are out of scope, matching every other
/// chunker in this registry: none currently emit edges (see
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
pub struct XImgChunker;

impl SourceFormatChunker for XImgChunker {
    fn format_id(&self) -> &str {
        "ximg"
    }

    fn claims(&self, path: &Path, sniff: &[u8]) -> bool {
        let Some(media_type) = extension_media_type(path) else {
            return false;
        };
        magic_matches(media_type, sniff)
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        let Some(media_type) = extension_media_type(path) else {
            return Ok((Vec::new(), Vec::new()));
        };
        // Runs inside the same blocking-safe context as every other
        // chunker (IndexWriterActor's dedicated thread or a spawn_blocking
        // closure) - see bbox-visual-store::VisualPayloadStore::put.
        let payload = match bbox_visual_store::global().put(bytes, media_type) {
            Ok(payload) => payload,
            Err(err) => {
                // Chunker `Err` aborts the whole background reindex pass
                // (not just this file), so a payload-store write failure
                // degrades to "no chunk for this image" rather than
                // propagating, matching pdf.rs/office.rs's degradation
                // convention for extraction failures.
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "visual payload store write failed; skipping image chunk"
                );
                return Ok((Vec::new(), Vec::new()));
            }
        };
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("image")
            .to_string();
        let mut chunk =
            placeholder_chunk(path, IMAGE_CHUNK_KIND, None, stem, 0, bytes.len() as u64, 0);
        chunk.symbol = Some(payload.encode());
        chunk.visual_payload = Some(payload);
        Ok((vec![chunk], Vec::new()))
    }
}

fn extension_media_type(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();
    media_type_for_extension(&ext)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Minimal valid 1x1 PNG (67 bytes) - same fixture shape as
    /// `voyage_multimodal.rs`'s test module.
    fn tiny_png() -> Vec<u8> {
        const PNG_1X1: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        PNG_1X1.to_vec()
    }

    fn tiny_jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F']
    }

    fn tiny_gif() -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&[0; 10]);
        bytes
    }

    fn tiny_webp() -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(b"WEBP");
        bytes
    }

    fn with_store<T>(f: impl FnOnce() -> T) -> T {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(bbox_visual_store::VisualPayloadStore::open(
            dir.path().to_path_buf(),
        ));
        let _guard = bbox_visual_store::install_test_global(store);
        f()
    }

    #[test]
    fn claims_png_by_extension_and_magic_bytes() {
        let chunker = XImgChunker;
        assert!(chunker.claims(Path::new("shot.png"), &tiny_png()));
        // Extension without matching magic bytes (renamed/corrupt file) is
        // rejected, not silently treated as an image.
        assert!(!chunker.claims(Path::new("shot.png"), b"not a png"));
        // Right bytes, wrong/no extension: not claimed (extension gate is
        // load-bearing, matching pdf.rs's convention).
        assert!(!chunker.claims(Path::new("shot.bin"), &tiny_png()));
    }

    #[test]
    fn claims_jpeg_gif_webp() {
        let chunker = XImgChunker;
        assert!(chunker.claims(Path::new("a.jpg"), &tiny_jpeg()));
        assert!(chunker.claims(Path::new("a.jpeg"), &tiny_jpeg()));
        assert!(chunker.claims(Path::new("a.gif"), &tiny_gif()));
        assert!(chunker.claims(Path::new("a.webp"), &tiny_webp()));
    }

    #[test]
    fn chunk_emits_one_image_chunk_with_stem_content_and_payload_ref() {
        with_store(|| {
            let chunker = XImgChunker;
            let bytes = tiny_png();
            let (chunks, edges) = chunker
                .chunk(Path::new("diagrams/figure-3.png"), &bytes)
                .unwrap();
            assert!(edges.is_empty());
            assert_eq!(chunks.len(), 1);
            let chunk = &chunks[0];
            assert_eq!(chunk.chunk_kind, IMAGE_CHUNK_KIND);
            assert_eq!(chunk.content, "figure-3");
            assert_eq!(chunk.language, None);
            let payload = chunk.visual_payload.as_ref().unwrap();
            assert_eq!(payload.media_type, "image/png");
            assert_eq!(payload.byte_len, bytes.len() as u64);
            // symbol carries the same ref, round-trippable for backfill.
            let decoded =
                bbox_visual_store::VisualPayloadRef::decode(chunk.symbol.as_deref().unwrap())
                    .unwrap();
            assert_eq!(&decoded, payload);
        });
    }

    #[test]
    fn chunk_writes_bytes_into_the_visual_payload_store() {
        with_store(|| {
            let chunker = XImgChunker;
            let bytes = tiny_png();
            let (chunks, _) = chunker.chunk(Path::new("a.png"), &bytes).unwrap();
            let payload = chunks[0].visual_payload.as_ref().unwrap();
            let stored =
                std::fs::read(bbox_visual_store::global().path_for(&payload.content_hash)).unwrap();
            assert_eq!(stored, bytes);
        });
    }
}
