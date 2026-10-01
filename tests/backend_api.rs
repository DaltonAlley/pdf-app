use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    fs,
    io::{Cursor, Read},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body, HttpBody},
    http::{header, Method, Request, StatusCode},
    response::Response,
};
use bytes::Bytes;
use http_body::{Frame, SizeHint};
use image::{GenericImageView, ImageBuffer, Rgba};
use lopdf::{dictionary, Dictionary, Document as LoDocument, Object, Stream};
use pdf_tools_server::{app, AppState};
use pdfium_render::prelude::{
    PdfPageObjectsCommon, PdfPagePaperSize, PdfPoints, PdfRenderConfig, Pdfium,
};
use serial_test::serial;
use tokio::time::{sleep, Instant};
use tower::ServiceExt;
use zip::ZipArchive;

#[path = "support/finished_size.rs"]
mod finished_size;

struct MultipartPart {
    name: String,
    filename: Option<String>,
    bytes: Vec<u8>,
}

fn text_part(name: &str, value: &str) -> MultipartPart {
    MultipartPart {
        name: name.to_string(),
        filename: None,
        bytes: value.as_bytes().to_vec(),
    }
}

fn upload_part(name: &str, filename: &str, bytes: Vec<u8>) -> MultipartPart {
    MultipartPart {
        name: name.to_string(),
        filename: Some(filename.to_string()),
        bytes,
    }
}

fn multipart_request(path: &str, parts: Vec<MultipartPart>) -> Request<Body> {
    let boundary = "pdf-tools-test-boundary";
    let mut body = Vec::new();

    for part in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match part.filename {
            Some(filename) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n\r\n",
                    part.name, filename
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                    part.name
                )
                .as_bytes(),
            ),
        }
        body.extend_from_slice(&part.bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const TEST_IMPOSE_UPLOAD_LIMIT_BYTES: usize = 1024 * 1024;
const OVERSIZED_PDF_PADDING_BYTES: usize = TEST_IMPOSE_UPLOAD_LIMIT_BYTES + STREAM_CHUNK_BYTES;

struct RepeatedChunkBody {
    head: Option<Bytes>,
    chunk: Bytes,
    remaining: usize,
    tail: Option<Bytes>,
}

impl HttpBody for RepeatedChunkBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(head) = self.head.take() {
            return Poll::Ready(Some(Ok(Frame::data(head))));
        }
        if self.remaining > 0 {
            let length = self.remaining.min(self.chunk.len());
            self.remaining -= length;
            return Poll::Ready(Some(Ok(Frame::data(self.chunk.slice(..length)))));
        }
        Poll::Ready(self.tail.take().map(|tail| Ok(Frame::data(tail))))
    }

    fn size_hint(&self) -> SizeHint {
        let length = self
            .head
            .as_ref()
            .map_or(0, Bytes::len)
            .saturating_add(self.remaining)
            .saturating_add(self.tail.as_ref().map_or(0, Bytes::len));
        SizeHint::with_exact(length as u64)
    }
}

fn oversized_streaming_pdf_request(path: &str) -> Request<Body> {
    let boundary = "pdf-tools-streaming-boundary";
    let (pdf_head, pdf_tail) = minimal_pdf_parts(OVERSIZED_PDF_PADDING_BYTES);
    let mut head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"oversized.pdf\"\r\n\r\n"
    )
    .into_bytes();
    head.extend_from_slice(&pdf_head);
    // Whitespace before the xref table is valid PDF syntax and does not create a
    // huge decoded artwork object in either structural parser.
    let chunk = Bytes::from(vec![b' '; STREAM_CHUNK_BYTES]);
    let mut tail = pdf_tail;
    tail.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let body = RepeatedChunkBody {
        head: Some(Bytes::from(head)),
        chunk,
        remaining: OVERSIZED_PDF_PADDING_BYTES,
        tail: Some(Bytes::from(tail)),
    };
    let content_length = body.size_hint().exact().unwrap();

    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::CONTENT_LENGTH, content_length)
        .body(Body::new(body))
        .unwrap()
}

fn minimal_pdf_parts(padding_bytes: usize) -> (Vec<u8>, Vec<u8>) {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for object in [
        b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".as_slice(),
        b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n".as_slice(),
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 252 144] /Resources <<>> /Contents 4 0 R >>\nendobj\n".as_slice(),
        b"4 0 obj\n<< /Length 0 >>\nstream\n\nendstream\nendobj\n".as_slice(),
    ] {
        offsets.push(pdf.len());
        pdf.extend_from_slice(object);
    }
    let xref = pdf.len() + padding_bytes;
    let mut tail = b"xref\n0 5\n0000000000 65535 f \n".to_vec();
    for offset in offsets {
        tail.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    tail.extend_from_slice(
        format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    (pdf, tail)
}

fn impose_staging_paths(state: &AppState) -> BTreeSet<std::path::PathBuf> {
    fs::read_dir(state.impose_staging_path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("pdf-tools-impose-") && name.ends_with(".upload")
                })
        })
        .collect()
}

fn get_request(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn json_request(path: &str, value: serde_json::Value) -> Request<Body> {
    method_json_request(Method::POST, path, value)
}

fn method_json_request(method: Method, path: &str, value: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

fn delete_request(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::DELETE)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn put_request(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::PUT)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn test_pdfium() -> Option<Arc<Pdfium>> {
    use std::sync::OnceLock;

    static PDFIUM: OnceLock<Option<Arc<Pdfium>>> = OnceLock::new();
    PDFIUM
        .get_or_init(|| {
            let bindings = match std::env::var("PDF_TOOLS_PDFIUM_PATH") {
                Ok(path) if !path.trim().is_empty() => Pdfium::bind_to_library(path.trim()),
                _ => Pdfium::bind_to_system_library(),
            };
            Some(Arc::new(Pdfium::new(bindings.unwrap_or_else(|error| {
                panic!(
                    "backend API tests require PDFium; set PDF_TOOLS_PDFIUM_PATH or install a system library: {error}"
                )
            }))))
        })
        .clone()
}

fn state_or_skip() -> Option<Arc<AppState>> {
    let Some(pdfium) = test_pdfium() else {
        eprintln!("skipping backend API test: PDFium is unavailable");
        return None;
    };
    Some(Arc::new(AppState::for_tests(pdfium).unwrap()))
}

fn state_with_download_limit_or_skip(max_download_bytes: usize) -> Option<Arc<AppState>> {
    let Some(pdfium) = test_pdfium() else {
        eprintln!("skipping backend API test: PDFium is unavailable");
        return None;
    };
    Some(Arc::new(
        AppState::for_tests_with_download_limit(pdfium, Some(max_download_bytes)).unwrap(),
    ))
}

async fn send(state: Arc<AppState>, request: Request<Body>) -> Response {
    app(state, None).unwrap().oneshot(request).await.unwrap()
}

async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn body_bytes(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn preview_batch_pages(bytes: &[u8]) -> BTreeMap<usize, Vec<u8>> {
    assert_eq!(bytes.get(..8), Some(b"PDFPV001".as_slice()));
    let count = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut offset = 12usize;
    let mut pages = BTreeMap::new();
    for _ in 0..count {
        let page = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let length = u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        offset += 8;
        let end = offset + length;
        assert!(pages.insert(page, bytes[offset..end].to_vec()).is_none());
        offset = end;
    }
    assert_eq!(offset, bytes.len());
    pages
}

fn content_disposition(response: &Response) -> &str {
    response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

fn zip_entries(bytes: Vec<u8>) -> Vec<(String, Vec<u8>)> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        entries.push((file.name().to_string(), bytes));
    }
    entries
}

fn sample_png_bytes() -> Vec<u8> {
    let image = ImageBuffer::from_fn(12, 12, |x, y| {
        if (x + y) % 2 == 0 {
            Rgba([220, 30, 30, 255])
        } else {
            Rgba([30, 90, 220, 255])
        }
    });
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn solid_png_bytes(color: Rgba<u8>) -> Vec<u8> {
    let image = ImageBuffer::from_pixel(1050, 600, color);
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn solid_business_card_pdf_bytes(red: f32, green: f32, blue: f32) -> Vec<u8> {
    let mut document = LoDocument::with_version("1.7");
    let pages_id = document.new_object_id();
    let content_id = document.add_object(Stream::new(
        Dictionary::new(),
        format!("{red} {green} {blue} rg 0 0 252 144 re f").into_bytes(),
    ));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 252.into(), 144.into()],
        "CropBox" => vec![0.into(), 0.into(), 252.into(), 144.into()],
        "TrimBox" => vec![0.into(), 0.into(), 252.into(), 144.into()],
        "Resources" => Dictionary::new(),
        "Contents" => content_id,
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn sample_jpeg_bytes() -> Vec<u8> {
    let image = ImageBuffer::from_fn(24, 12, |x, _| image::Rgb([x as u8 * 8, 90, 180]));
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    bytes
}

fn sample_pdf_bytes(pdfium: &Pdfium, page_count: usize) -> Vec<u8> {
    let mut doc = pdfium.create_new_pdf().unwrap();
    for _ in 0..page_count {
        doc.pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .unwrap();
    }
    doc.save_to_bytes().unwrap()
}

fn sample_pdf_bytes_with_size(
    pdfium: &Pdfium,
    page_count: usize,
    width_in: f32,
    height_in: f32,
) -> Vec<u8> {
    let mut doc = pdfium.create_new_pdf().unwrap();
    for _ in 0..page_count {
        doc.pages_mut()
            .create_page_at_end(PdfPagePaperSize::Custom(
                PdfPoints::new(width_in * 72.0),
                PdfPoints::new(height_in * 72.0),
            ))
            .unwrap();
    }
    doc.save_to_bytes().unwrap()
}

fn sample_mixed_size_pdf_bytes(pdfium: &Pdfium) -> Vec<u8> {
    let mut doc = pdfium.create_new_pdf().unwrap();
    for (width, height) in [(3.5, 2.0), (4.0, 2.0)] {
        doc.pages_mut()
            .create_page_at_end(PdfPagePaperSize::Custom(
                PdfPoints::new(width * 72.0),
                PdfPoints::new(height * 72.0),
            ))
            .unwrap();
    }
    doc.save_to_bytes().unwrap()
}

fn sample_equivalent_rotated_pdf_bytes() -> Vec<u8> {
    let mut document = LoDocument::with_version("1.7");
    let pages_id = document.new_object_id();
    let rotated_page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 378.into(), 522.into()],
        "CropBox" => vec![0.into(), 0.into(), 378.into(), 522.into()],
        "TrimBox" => vec![0.into(), 0.into(), 378.into(), 522.into()],
        "Rotate" => 270,
        "Resources" => Dictionary::new(),
    });
    let unrotated_page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
        "CropBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
        "TrimBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
        "Rotate" => 0,
        "Resources" => Dictionary::new(),
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(rotated_page), Object::Reference(unrotated_page)],
            "Count" => 2,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn sample_unoriented_quarter_turned_pdf_bytes() -> Vec<u8> {
    let mut document = LoDocument::with_version("1.7");
    let pages_id = document.new_object_id();
    let landscape = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
        "CropBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
        "TrimBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
        "Resources" => Dictionary::new(),
    });
    let portrait = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 378.into(), 522.into()],
        "CropBox" => vec![0.into(), 0.into(), 378.into(), 522.into()],
        "TrimBox" => vec![0.into(), 0.into(), 378.into(), 522.into()],
        "Resources" => Dictionary::new(),
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(landscape), Object::Reference(portrait)],
            "Count" => 2,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn sample_colored_trim_pdf_bytes(page_count: usize) -> Vec<u8> {
    let mut document = LoDocument::with_version("1.7");
    let pages_id = document.new_object_id();
    let mut page_ids = Vec::new();
    let content = b"\
        1 0 0 rg 0 126 36 36 re f \
        0 1 0 rg 234 126 36 36 re f \
        0 0 1 rg 0 0 36 36 re f \
        1 1 0 rg 234 0 36 36 re f \
        0 0 0 rg 117 63 36 36 re f"
        .to_vec();

    for _ in 0..page_count {
        let content_id = document.add_object(Stream::new(Dictionary::new(), content.clone()));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 270.into(), 162.into()],
            "CropBox" => vec![0.into(), 0.into(), 270.into(), 162.into()],
            "TrimBox" => vec![14.into(), 9.into(), 266.into(), 153.into()],
            "Resources" => Dictionary::new(),
            "Contents" => content_id,
        });
        page_ids.push(page_id);
    }
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => page_ids.len() as i64,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn sample_business_card_with_bleed_boxes_pdf_bytes() -> Vec<u8> {
    let mut document = LoDocument::with_version("1.7");
    let pages_id = document.new_object_id();
    let content_id = document.add_object(Stream::new(
        Dictionary::new(),
        b"0.2 0.55 0.9 rg 0 0 162 270 re f".to_vec(),
    ));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 162.into(), 270.into()],
        "BleedBox" => vec![0.into(), 0.into(), 162.into(), 270.into()],
        "CropBox" => vec![9.into(), 9.into(), 153.into(), 261.into()],
        "TrimBox" => vec![9.into(), 9.into(), 153.into(), 261.into()],
        "Resources" => Dictionary::new(),
        "Contents" => content_id,
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn sample_printable_annotation_pdf_bytes() -> Vec<u8> {
    let mut document = LoDocument::with_version("1.7");
    let pages_id = document.new_object_id();
    let page_content_id = document.add_object(Stream::new(
        Dictionary::new(),
        b"0.95 g 0 0 252 144 re f".to_vec(),
    ));
    let appearance_id = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "FormType" => 1,
            "BBox" => vec![0.into(), 0.into(), 100.into(), 50.into()],
            "Resources" => Dictionary::new(),
        },
        b"1 0 0 rg 0 0 100 50 re f".to_vec(),
    ));
    let annotation_id = document.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Stamp",
        "Rect" => vec![76.into(), 47.into(), 176.into(), 97.into()],
        "F" => 4,
        "AP" => dictionary! { "N" => appearance_id },
        "Contents" => Object::string_literal("Printable approval stamp"),
    });
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 252.into(), 144.into()],
        "CropBox" => vec![0.into(), 0.into(), 252.into(), 144.into()],
        "TrimBox" => vec![0.into(), 0.into(), 252.into(), 144.into()],
        "Resources" => Dictionary::new(),
        "Contents" => page_content_id,
        "Annots" => vec![Object::Reference(annotation_id)],
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn gang_up_layout_request(sides: &str) -> serde_json::Value {
    let duplex = if sides == "double" {
        serde_json::json!({
            "flipEdge": "longEdge",
            "rotateBack180": false,
            "backAlignment": ""
        })
    } else {
        serde_json::Value::Null
    };
    serde_json::json!({
        "sourcePdfSize": { "width": 3.5, "height": 2.0 },
        "finishedCutSize": { "width": 3.5, "height": 2.0 },
        "parentSheetSize": { "width": 12.0, "height": 18.0 },
        "quantityRequested": 61,
        "sides": sides,
        "duplex": duplex,
        "layoutMode": "auto",
        "bleedOption": "useAsIs",
        "gutter": { "horizontal": 0.0, "vertical": 0.0 },
        "manual": null
    })
}

fn parse_status_lines(body: &str) -> std::collections::HashMap<String, String> {
    body.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

const JOB_COMPLETION_TIMEOUT: Duration = Duration::from_secs(90);

async fn wait_for_job_status(state: Arc<AppState>, id: &str, status: &str) -> String {
    let deadline = Instant::now() + JOB_COMPLETION_TIMEOUT;
    loop {
        let response = send(state.clone(), get_request(&format!("/jobs/{id}"))).await;
        let body = body_text(response).await;
        let fields = parse_status_lines(&body);
        if fields.get("status").map(String::as_str) == Some(status) {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "job did not reach {status}: {body}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn health_endpoint_returns_ok() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let response = send(state, get_request("/health")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ok");
}

#[tokio::test]
#[serial(pdfium)]
async fn json_body_validation_happens_before_operation_admission() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let _held_operation = state.admit_operation().unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .uri("/gang-up/layout")
        .body(Body::from("not valid json"))
        .unwrap();

    let response = send(state, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial(pdfium)]
async fn operation_admission_is_released_when_the_handler_returns() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let response = send(
        state.clone(),
        json_request(
            "/gang-up/layout",
            serde_json::json!({
                "sourcePdfSize": { "width": 3.5, "height": 2.0 },
                "finishedCutSize": { "width": 3.5, "height": 2.0 },
                "parentSheetSize": { "width": 12.0, "height": 18.0 },
                "quantityRequested": 1,
                "sides": "single",
                "layoutMode": "auto",
                "bleedOption": "useAsIs",
                "gutter": { "horizontal": 0.0, "vertical": 0.0 },
                "manual": null
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    assert!(state.admit_operation().is_ok());
}

#[tokio::test]
#[serial(pdfium)]
async fn upload_and_operation_admission_have_independent_capacity() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let upload_permit = state.admit_upload().unwrap();
    assert!(state.admit_upload().is_err());

    let operation_permit = state.admit_operation().unwrap();
    assert!(state.admit_operation().is_err());

    drop(upload_permit);
    assert!(state.admit_upload().is_ok());
    drop(operation_permit);
    assert!(state.admit_operation().is_ok());
}

#[tokio::test]
#[serial(pdfium)]
async fn multipart_uploads_are_unlimited_when_no_limit_is_configured() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let response = app(state, None)
        .unwrap()
        .oneshot(multipart_request(
            "/jobs",
            vec![
                text_part("action", "convert"),
                upload_part("files", "large.pdf", vec![0; 2 * 1024 * 1024 + 1]),
            ],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
#[serial(pdfium)]
async fn multipart_uploads_over_configured_limit_return_413() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let request = multipart_request("/jobs", vec![text_part("action", "unknown")]);
    let response = app(state, Some(0)).unwrap().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body_text(response).await,
        "upload is larger than the configured limit"
    );
}

#[tokio::test]
#[serial(pdfium)]
async fn oversized_multipart_text_fields_return_413_before_json_parsing() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let oversized_layout = " ".repeat(64 * 1024 + 1);
    let response = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![text_part("layoutRequest", &oversized_layout)],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body_text(response).await.contains("65536 bytes"));
}

#[tokio::test]
#[serial(pdfium)]
async fn multipart_uploads_under_configured_limit_reach_handler_validation() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let response = app(state.clone(), Some(1))
        .unwrap()
        .oneshot(multipart_request(
            "/jobs",
            vec![text_part("action", "unknown")],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "unknown job action");
}

#[tokio::test]
#[serial(pdfium)]
async fn invalid_job_forms_release_active_capacity() {
    let Some(state) = state_or_skip() else {
        return;
    };

    for _ in 0..3 {
        let response = send(
            state.clone(),
            multipart_request("/jobs", vec![text_part("not-action", "convert")]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let response = send(
        state.clone(),
        multipart_request("/jobs", vec![text_part("action", "unknown")]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial(pdfium)]
async fn convert_endpoint_validation_errors_are_precise() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let missing_target = send(
        state.clone(),
        multipart_request(
            "/convert",
            vec![upload_part("files", "image.png", sample_png_bytes())],
        ),
    )
    .await;
    assert_eq!(missing_target.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(missing_target)
        .await
        .contains("missing field `target`"));

    let bad_image = send(
        state.clone(),
        multipart_request(
            "/convert",
            vec![
                text_part("target", "pdf"),
                upload_part("files", "notes.txt", b"not an image".to_vec()),
            ],
        ),
    )
    .await;
    assert_eq!(bad_image.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(bad_image).await.contains("notes.txt"));

    let bad_pdf = send(
        state,
        multipart_request(
            "/convert",
            vec![
                text_part("target", "png"),
                upload_part("files", "notes.txt", b"not a pdf".to_vec()),
            ],
        ),
    )
    .await;
    assert_eq!(bad_pdf.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(bad_pdf).await.contains("notes.txt"));
}

#[tokio::test]
#[serial(pdfium)]
async fn merge_and_split_validation_errors_do_not_enter_pdfium_work() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let merge = send(
        state.clone(),
        multipart_request(
            "/merge",
            vec![upload_part("files", "one.pdf", b"%PDF-pretend".to_vec())],
        ),
    )
    .await;
    assert_eq!(merge.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(merge).await.contains("at least two"));

    let split = send(
        state,
        multipart_request(
            "/split",
            vec![upload_part("file", "source.pdf", b"%PDF-pretend".to_vec())],
        ),
    )
    .await;
    assert_eq!(split.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(split).await.contains("missing field `pages`"));
}

#[tokio::test]
#[serial(pdfium)]
async fn convert_png_to_pdf_endpoint_returns_valid_pdf_download() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/convert",
            vec![
                text_part("target", "pdf"),
                text_part("layout", "single"),
                upload_part("files", "image.png", sample_png_bytes()),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/pdf"
    );
    assert!(content_disposition(&response).contains("converted.pdf"));

    let bytes = body_bytes(response).await;
    let document = pdfium.load_pdf_from_byte_vec(bytes, None).unwrap();
    assert_eq!(document.pages().len(), 1);
}

#[tokio::test]
#[serial(pdfium)]
async fn configured_download_limit_is_enforced() {
    let Some(state) = state_with_download_limit_or_skip(1) else {
        return;
    };

    let response = send(
        state,
        multipart_request(
            "/convert",
            vec![
                text_part("target", "pdf"),
                text_part("layout", "single"),
                upload_part("files", "image.png", sample_png_bytes()),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body_text(response)
        .await
        .contains("configured 1-byte limit"));
}

#[tokio::test]
#[serial(pdfium)]
async fn convert_more_than_one_hundred_images_in_one_job() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    let mut parts = vec![text_part("target", "pdf"), text_part("layout", "single")];
    for index in 0..116 {
        parts.push(upload_part(
            "files",
            &format!("image-{index:03}.png"),
            sample_png_bytes(),
        ));
    }

    let response = send(state, multipart_request("/convert", parts)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let document = pdfium
        .load_pdf_from_byte_vec(body_bytes(response).await, None)
        .unwrap();
    assert_eq!(document.pages().len(), 116);
}

#[tokio::test]
#[serial(pdfium)]
async fn convert_pdf_to_png_endpoint_returns_png_payload() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/convert",
            vec![
                text_part("target", "png"),
                text_part("pages", "1"),
                upload_part("files", "source.pdf", sample_pdf_bytes(&pdfium, 1)),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    assert!(content_disposition(&response).contains("page-0001.png"));
    assert!(body_bytes(response).await.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[tokio::test]
#[serial(pdfium)]
async fn convert_pdf_to_jpeg_endpoint_returns_jpeg_payload() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/convert",
            vec![
                text_part("target", "jpeg"),
                text_part("pages", "1"),
                upload_part("files", "source.pdf", sample_pdf_bytes(&pdfium, 1)),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg"
    );
    assert!(content_disposition(&response).contains("page-0001.jpg"));
    assert!(body_bytes(response).await.starts_with(&[0xff, 0xd8]));
}

#[tokio::test]
#[serial(pdfium)]
async fn merge_endpoint_returns_valid_combined_pdf() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/merge",
            vec![
                upload_part("files", "one.pdf", sample_pdf_bytes(&pdfium, 1)),
                upload_part("files", "two.pdf", sample_pdf_bytes(&pdfium, 2)),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(content_disposition(&response).contains("one-combined.pdf"));
    let document = pdfium
        .load_pdf_from_byte_vec(body_bytes(response).await, None)
        .unwrap();
    assert_eq!(document.pages().len(), 3);
}

#[tokio::test]
#[serial(pdfium)]
async fn split_endpoint_returns_selected_page_zip() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/split",
            vec![
                text_part("pages", "2"),
                upload_part("file", "source.pdf", sample_pdf_bytes(&pdfium, 2)),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/zip"
    );
    assert!(content_disposition(&response).contains("source-pages.zip"));
    let entries = zip_entries(body_bytes(response).await);

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "source-page-0002.pdf");
    let document = pdfium
        .load_pdf_from_byte_vec(entries[0].1.clone(), None)
        .unwrap();
    assert_eq!(document.pages().len(), 1);
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_layout_rejects_invalid_quantity_and_returns_expected_grid() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let invalid = send(
        state.clone(),
        json_request(
            "/gang-up/layout",
            serde_json::json!({
                "sourcePdfSize": { "width": 3.5, "height": 2.0 },
                "finishedCutSize": { "width": 3.5, "height": 2.0 },
                "parentSheetSize": { "width": 12.0, "height": 18.0 },
                "quantityRequested": 0,
                "sides": "single",
                "layoutMode": "auto",
                "bleedOption": "useAsIs",
                "gutter": { "horizontal": 0.0, "vertical": 0.0 },
                "manual": null
            }),
        ),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(invalid).await.contains("quantity"));

    let response = send(
        state,
        json_request(
            "/gang-up/layout",
            serde_json::json!({
                "sourcePdfSize": { "width": 3.5, "height": 2.0 },
                "finishedCutSize": { "width": 3.5, "height": 2.0 },
                "parentSheetSize": { "width": 12.0, "height": 18.0 },
                "quantityRequested": 500,
                "sides": "single",
                "layoutMode": "auto",
                "bleedOption": "useAsIs",
                "gutter": { "horizontal": 0.0, "vertical": 0.0 },
                "manual": null
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body["rows"], 5);
    assert_eq!(body["columns"], 6);
    assert_eq!(body["piecesPerSheet"], 30);
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_layout_replaces_spoofed_back_alignment_with_derived_text() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let mut request = gang_up_layout_request("double");
    request["duplex"] = serde_json::json!({
        "flipEdge": "shortEdge",
        "rotateBack180": true,
        "backAlignment": "spoofed alignment"
    });

    let response = send(state, json_request("/gang-up/layout", request)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    let alignment = body["duplex"]["backAlignment"].as_str().unwrap();
    assert!(!alignment.contains("spoofed"));
    assert!(alignment.contains("short-edge flip"));
    assert!(alignment.contains("rotated 180 degrees"));
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_layout_accepts_manual_source_bleed_and_reports_clipped_gutters() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let mut request = gang_up_layout_request("single");
    request["sourcePdfSize"] = serde_json::json!({ "width": 3.75, "height": 2.25 });
    request["sourceTrimBox"] = serde_json::json!({
        "left": 0.2,
        "bottom": 0.1,
        "right": 3.7,
        "top": 2.1,
        "width": 3.5,
        "height": 2.0
    });
    request["sourceBleedOverride"] = serde_json::json!(0.125);

    let response = send(
        state.clone(),
        json_request("/gang-up/layout", request.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body["bleed"]["source"], "manual");
    assert_eq!(body["bleed"]["effectiveAmountPerSide"], 0.125);
    assert_eq!(body["sourceTrimBox"]["left"], 0.125);
    assert!(body["piecesPerSheet"].as_u64().unwrap() > 1);
    assert!(body["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["problem"] == "Bleed is wider than the available gutter."));

    request["finishedCutSize"] = serde_json::json!({ "width": 3.25, "height": 2.0 });
    let invalid = send(state, json_request("/gang-up/layout", request)).await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(invalid)
        .await
        .contains("finished cut size must equal the source PDF size"));
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_layout_routes_enforce_work_limits() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let mut oversized = gang_up_layout_request("single");
    oversized["parentSheetSize"] = serde_json::json!({ "width": 101.0, "height": 18.0 });

    let mut too_many_impressions = gang_up_layout_request("single");
    too_many_impressions["quantityRequested"] = serde_json::json!(10_001);

    let mut too_many_placements = gang_up_layout_request("single");
    too_many_placements["sourcePdfSize"] = serde_json::json!({ "width": 0.01, "height": 0.01 });
    too_many_placements["finishedCutSize"] = serde_json::json!({ "width": 0.01, "height": 0.01 });

    let mut too_many_sides = gang_up_layout_request("single");
    too_many_sides["finishedCutSize"] = serde_json::json!({ "width": 3.5, "height": 2.0 });
    too_many_sides["parentSheetSize"] = serde_json::json!({ "width": 3.5, "height": 2.0 });
    too_many_sides["quantityRequested"] = serde_json::json!(1_001);

    let bounded = send(
        state.clone(),
        json_request("/gang-up/layout", too_many_placements),
    )
    .await;
    assert_eq!(bounded.status(), StatusCode::OK);
    let bounded: serde_json::Value = serde_json::from_str(&body_text(bounded).await).unwrap();
    assert!(bounded["piecesPerSheet"].as_u64().unwrap() <= 512);

    for (request, expected) in [
        (oversized, "cannot exceed"),
        (too_many_impressions, "requested pieces"),
        (too_many_sides, "sheet sides"),
    ] {
        let response = send(state.clone(), json_request("/gang-up/layout", request)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains(expected));
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_analyze_reports_dimensions_bleed_and_duplex() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let exact = send(
        state.clone(),
        multipart_request(
            "/gang-up/analyze",
            vec![upload_part(
                "file",
                "card.pdf",
                sample_pdf_bytes_with_size(&pdfium, 1, 3.5, 2.0),
            )],
        ),
    )
    .await;
    assert_eq!(exact.status(), StatusCode::OK);
    let exact_body: serde_json::Value = serde_json::from_str(&body_text(exact).await).unwrap();
    assert_eq!(exact_body["pageCount"], 1);
    assert_eq!(exact_body["sourcePdfSize"]["width"], 3.5);
    assert_eq!(exact_body["matchedPresetId"], serde_json::Value::Null);
    assert_eq!(exact_body["likelyBleed"]["detected"], false);

    let bleed = send(
        state.clone(),
        multipart_request(
            "/gang-up/analyze",
            vec![upload_part(
                "file",
                "card-bleed.pdf",
                sample_pdf_bytes_with_size(&pdfium, 2, 3.75, 2.25),
            )],
        ),
    )
    .await;
    assert_eq!(bleed.status(), StatusCode::OK);
    let bleed_body: serde_json::Value = serde_json::from_str(&body_text(bleed).await).unwrap();
    assert_eq!(bleed_body["pageCount"], 2);
    assert_eq!(bleed_body["appearsDuplex"], true);
    assert_eq!(bleed_body["likelyBleed"]["detected"], true);
    assert_eq!(bleed_body["likelyBleed"]["amountPerSide"], 0.125);

    let boxed_bleed = send(
        state,
        multipart_request(
            "/gang-up/analyze",
            vec![upload_part(
                "file",
                "portrait-card-with-bleed.pdf",
                sample_business_card_with_bleed_boxes_pdf_bytes(),
            )],
        ),
    )
    .await;
    assert_eq!(boxed_bleed.status(), StatusCode::OK);
    let boxed_bleed: serde_json::Value =
        serde_json::from_str(&body_text(boxed_bleed).await).unwrap();
    assert_eq!(
        boxed_bleed["sourcePdfSize"],
        serde_json::json!({ "width": 2.25, "height": 3.75 })
    );
    assert_eq!(
        boxed_bleed["suggestedFinishedCutSize"],
        serde_json::json!({ "width": 2.0, "height": 3.5 })
    );
    assert_eq!(boxed_bleed["likelyBleed"]["detected"], true);
    assert_eq!(boxed_bleed["likelyBleed"]["amountPerSide"], 0.125);
    assert_eq!(boxed_bleed["cropBox"]["left"], 0.125);
    assert_eq!(boxed_bleed["bleedBox"]["width"], 2.25);
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_accepts_png_and_jpeg_artwork_for_analysis_and_export() {
    let Some(state) = state_or_skip() else {
        return;
    };

    for (filename, bytes) in [
        ("logo.final.png", sample_png_bytes()),
        ("photo.jpg", sample_jpeg_bytes()),
    ] {
        let analysis_response = send(
            state.clone(),
            multipart_request(
                "/gang-up/analyze",
                vec![upload_part("file", filename, bytes.clone())],
            ),
        )
        .await;
        assert_eq!(analysis_response.status(), StatusCode::OK);
        let analysis: serde_json::Value =
            serde_json::from_str(&body_text(analysis_response).await).unwrap();
        assert_eq!(analysis["filename"], filename);
        assert_eq!(analysis["pageCount"], 1);

        let request = serde_json::json!({
            "sourcePdfSize": analysis["sourcePdfSize"],
            "sourcePageCount": 1,
            "finishedCutSize": analysis["sourcePdfSize"],
            "parentSheetSize": { "width": 12.0, "height": 18.0 },
            "quantityRequested": 1,
            "impositionMode": "unique",
            "impressionQuantities": null,
            "sides": "single",
            "duplex": null,
            "layoutMode": "auto",
            "bleedOption": "useAsIs",
            "gutter": { "horizontal": 0.0, "vertical": 0.0 },
            "manual": null
        });
        let export_response = send(
            state.clone(),
            multipart_request(
                "/gang-up/export",
                vec![
                    text_part("layoutRequest", &request.to_string()),
                    upload_part("file", filename, bytes),
                ],
            ),
        )
        .await;
        assert_eq!(export_response.status(), StatusCode::OK);
        assert!(
            content_disposition(&export_response).contains(if filename.ends_with(".png") {
                "logo-final-imposed.pdf"
            } else {
                "photo-imposed.pdf"
            })
        );
        assert!(body_bytes(export_response).await.starts_with(b"%PDF-"));
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_uses_supplied_outer_artwork_without_changing_finished_size() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let request = serde_json::json!({
        "sourcePdfSize": { "width": 2.25, "height": 3.75 },
        "sourceTrimBox": {
            "left": 0.125,
            "bottom": 0.125,
            "right": 2.125,
            "top": 3.625,
            "width": 2.0,
            "height": 3.5
        },
        "sourcePageCount": 1,
        "finishedCutSize": { "width": 2.0, "height": 3.5 },
        "parentSheetSize": { "width": 12.0, "height": 18.0 },
        "quantityRequested": 1,
        "impositionMode": "repeat",
        "impressionQuantities": [1],
        "orientationPreference": "portrait",
        "sides": "single",
        "duplex": null,
        "layoutMode": "manual",
        "bleedOption": "useAsIs",
        "sourceBleedOverride": 0.125,
        "createdBleedAmount": 0.125,
        "gutter": { "horizontal": 0.25, "vertical": 0.25 },
        "manual": { "rows": 1, "columns": 1, "rotationDegrees": 0, "margins": null }
    });
    let response = send(
        state.clone(),
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part(
                    "file",
                    "portrait-card-with-bleed.pdf",
                    sample_business_card_with_bleed_boxes_pdf_bytes(),
                ),
            ],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response).await;
    let exported = LoDocument::load_mem(&bytes).unwrap();
    let supplied_artwork_form = exported.objects.values().find_map(|object| {
        let stream = object.as_stream().ok()?;
        if stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|value| value.as_name().ok())
            != Some(b"Form")
        {
            return None;
        }
        let bbox = stream.dict.get(b"BBox").ok()?.as_array().ok()?;
        let values = bbox
            .iter()
            .map(|value| value.as_float().ok().map(f64::from))
            .collect::<Option<Vec<_>>>()?;
        (values == vec![0.0, 0.0, 162.0, 270.0]).then_some(stream)
    });
    assert!(
        supplied_artwork_form.is_some(),
        "export should retain the full 2.25 x 3.75 artwork form"
    );

    let pdfium = state.pdfium();
    let invalid = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part(
                    "file",
                    "exact-size-card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 1, 2.0, 3.5),
                ),
            ],
        ),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(invalid)
        .await
        .contains("does not contain 0.1250 in of artwork outside every finished edge"));
}

#[tokio::test]
#[serial(pdfium)]
async fn general_pdf_intake_accepts_mixed_geometry_and_supports_extraction() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    for bytes in [
        sample_mixed_size_pdf_bytes(&pdfium),
        sample_unoriented_quarter_turned_pdf_bytes(),
    ] {
        let inspected = send(
            state.clone(),
            multipart_request(
                "/pdf/inspect",
                vec![upload_part("files", "mixed.pdf", bytes.clone())],
            ),
        )
        .await;
        assert_eq!(inspected.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_str(&body_text(inspected).await).unwrap();
        assert_eq!(result["pageCount"], 2);

        let extracted = send(
            state.clone(),
            multipart_request(
                "/split",
                vec![
                    upload_part("file", "mixed.pdf", bytes),
                    text_part("pages", "all"),
                    text_part("outputMode", "combined"),
                ],
            ),
        )
        .await;
        assert_eq!(extracted.status(), StatusCode::OK);
        let body = to_bytes(extracted.into_body(), usize::MAX).await.unwrap();
        let document = pdfium.load_pdf_from_byte_slice(&body, None).unwrap();
        assert_eq!(document.pages().len(), 2);
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn general_pdf_intake_rejects_corrupt_and_ambiguous_sources() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    for parts in [
        vec![],
        vec![upload_part(
            "files",
            "corrupt.pdf",
            b"%PDF-not-readable".to_vec(),
        )],
        vec![
            upload_part("files", "one.pdf", sample_pdf_bytes(&pdfium, 1)),
            upload_part("files", "two.pdf", sample_pdf_bytes(&pdfium, 1)),
        ],
    ] {
        let response = send(state.clone(), multipart_request("/pdf/inspect", parts)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_analysis_reports_each_mixed_source_page_authoritatively() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    let response = send(
        state,
        multipart_request(
            "/gang-up/analyze",
            vec![upload_part(
                "file",
                "mixed.pdf",
                sample_mixed_size_pdf_bytes(&pdfium),
            )],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let analysis: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(
        analysis["sourcePages"][0]["sourcePdfSize"],
        serde_json::json!({"width":3.5,"height":2.0})
    );
    assert_eq!(
        analysis["sourcePages"][1]["sourcePdfSize"],
        serde_json::json!({"width":4.0,"height":2.0})
    );
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_accepts_equivalent_rotated_page_geometry_during_analysis() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let response = send(
        state,
        multipart_request(
            "/gang-up/analyze",
            vec![upload_part(
                "file",
                "orientation-fixed.pdf",
                sample_equivalent_rotated_pdf_bytes(),
            )],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body["pageCount"], 2);
    assert_eq!(body["sourcePdfSize"]["width"], 7.25);
    assert_eq!(body["sourcePdfSize"]["height"], 5.25);
}

#[tokio::test]
#[serial(pdfium)]
async fn impose_preparation_preserves_quarter_turned_page_dimensions() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let response = send(
        state.clone(),
        multipart_request(
            "/gang-up/sources",
            vec![upload_part(
                "files",
                "mixed-orientation.pdf",
                sample_unoriented_quarter_turned_pdf_bytes(),
            )],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job_id = body_text(response).await;
    wait_for_job_status(state.clone(), &job_id, "done").await;
    let prepared: serde_json::Value = serde_json::from_slice(
        &body_bytes(
            send(
                state.clone(),
                get_request(&format!("/jobs/{job_id}/download")),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(prepared["analysis"]["pageCount"], 2);
    assert_eq!(prepared["analysis"]["sourcePdfSize"]["width"], 7.25);
    assert_eq!(prepared["analysis"]["sourcePdfSize"]["height"], 5.25);
    assert_eq!(prepared["analysis"]["orientationAdjustedPages"], 0);
    assert_eq!(
        prepared["analysis"]["sourcePages"][1]["sourcePdfSize"],
        serde_json::json!({"width":5.25,"height":7.25})
    );

    let source_id = prepared["sourceId"].as_str().unwrap();
    let mut preview_sizes = Vec::new();
    for page in [1, 2] {
        let preview = send(
            state.clone(),
            get_request(&format!("/gang-up/sources/{source_id}/preview/{page}")),
        )
        .await;
        assert_eq!(preview.status(), StatusCode::OK);
        let image = image::load_from_memory(&body_bytes(preview).await).unwrap();
        preview_sizes.push((image.width(), image.height()));
    }
    assert!(preview_sizes[0].0 > preview_sizes[0].1);
    assert!(preview_sizes[1].0 < preview_sizes[1].1);

    assert_eq!(
        send(
            state.clone(),
            delete_request(&format!("/gang-up/sources/{source_id}")),
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(state, delete_request(&format!("/jobs/{job_id}")))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_returns_pdf_download_with_parent_sheet_pages() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    let mut request = gang_up_layout_request("single");
    request["sourcePdfSize"] = serde_json::json!({ "width": 99.0, "height": 88.0 });
    request["sourcePageCount"] = serde_json::json!(999);

    let response = send(
        state.clone(),
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 1, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/pdf"
    );
    assert!(content_disposition(&response).contains("imposed.pdf"));
    let export_id = response
        .headers()
        .get("x-gang-up-export-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let exported_bytes = body_bytes(response).await;
    let exported = pdfium
        .load_pdf_from_byte_vec(exported_bytes.clone(), None)
        .unwrap();
    assert_eq!(exported.pages().len(), 3);
    let first = exported.pages().get(0).unwrap();
    assert!((first.width().value - 12.0 * 72.0).abs() < 0.01);
    assert!((first.height().value - 18.0 * 72.0).abs() < 0.01);

    let history = send(state.clone(), get_request("/gang-up/export-history")).await;
    assert_eq!(history.status(), StatusCode::OK);
    let history: serde_json::Value = serde_json::from_str(&body_text(history).await).unwrap();
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["id"], export_id);
    assert_eq!(history[0]["outputType"], "cleanPdf");
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_flattens_printable_annotations_into_the_artwork() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    let mut request = gang_up_layout_request("single");
    request["quantityRequested"] = serde_json::json!(1);
    request["layoutMode"] = serde_json::json!("manual");
    request["manual"] =
        serde_json::json!({ "rows": 1, "columns": 1, "rotationDegrees": 0, "margins": null });

    let response = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part(
                    "file",
                    "annotated-card.pdf",
                    sample_printable_annotation_pdf_bytes(),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response).await;
    let exported = pdfium.load_pdf_from_byte_vec(bytes.clone(), None).unwrap();
    let image = exported
        .pages()
        .get(0)
        .unwrap()
        .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(1.0))
        .unwrap()
        .as_image()
        .unwrap();
    let red_pixels = image
        .pixels()
        .filter(|(_, _, pixel)| pixel[0] > 220 && pixel[1] < 40 && pixel[2] < 40)
        .count();
    assert!(
        red_pixels > 1_000,
        "flattened annotation rendered only {red_pixels} red pixels"
    );

    let exported = LoDocument::load_mem(&bytes).unwrap();
    assert!(exported.get_pages().into_values().all(|page_id| {
        exported
            .get_dictionary(page_id)
            .is_ok_and(|page| page.get(b"Annots").is_err())
    }));
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_download_limit_is_checked_before_export_history_is_written() {
    let Some(state) = state_with_download_limit_or_skip(1) else {
        return;
    };
    let pdfium = state.pdfium();
    let response = send(
        state.clone(),
        multipart_request(
            "/gang-up/export",
            vec![
                text_part(
                    "layoutRequest",
                    &gang_up_layout_request("single").to_string(),
                ),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 1, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let history = send(state, get_request("/gang-up/export-history")).await;
    assert_eq!(history.status(), StatusCode::OK);
    assert_eq!(body_text(history).await, "[]");
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_download_survives_unavailable_history_storage() {
    let Some(pdfium) = test_pdfium() else {
        eprintln!("skipping backend API test: PDFium is unavailable");
        return;
    };
    let data_path = std::env::temp_dir().join(format!(
        "pdf-tools-history-unavailable-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&data_path).unwrap();
    fs::write(data_path.join("gang-up-export-files"), b"not a directory").unwrap();
    let state =
        Arc::new(AppState::for_tests_with_data_dir(pdfium.clone(), data_path.clone()).unwrap());

    let response = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![
                text_part(
                    "layoutRequest",
                    &gang_up_layout_request("single").to_string(),
                ),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 1, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-gang-up-export-id").is_none());
    assert!(response
        .headers()
        .get("x-gang-up-cut-plan-export-id")
        .is_none());
    assert!(body_bytes(response).await.starts_with(b"%PDF"));
    fs::remove_dir_all(data_path).unwrap();
}

#[tokio::test]
#[serial(pdfium)]
async fn impose_management_routes_roundtrip_user_saved_setups() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let presets = send(state.clone(), get_request("/gang-up/presets")).await;
    assert_eq!(presets.status(), StatusCode::OK);
    let presets: serde_json::Value = serde_json::from_str(&body_text(presets).await).unwrap();
    assert_eq!(presets.as_array().unwrap().len(), 1);
    assert_eq!(presets[0]["name"], "5x7 on 12x18");
    assert_eq!(presets[0]["builtIn"], false);
    assert_eq!(presets[0]["impositionMode"], "repeat");

    let preset_input = serde_json::json!({
        "name": "Manual card",
        "finishedCutSize": { "width": 3.5, "height": 2.0 },
        "parentSheetSize": { "width": 12.0, "height": 18.0 },
        "bleedHandling": "useAsIs",
        "gutter": { "horizontal": 0.125, "vertical": 0.125 },
        "orientationPreference": "landscape",
        "sides": "double",
        "layoutPreference": "manual",
        "manual": { "rows": 4, "columns": 5, "rotationDegrees": 90, "margins": null },
        "duplex": { "flipEdge": "shortEdge", "rotateBack180": true, "backAlignment": "" },
        "outputPreference": "cleanPdf"
    });
    let created = send(
        state.clone(),
        json_request("/gang-up/presets", preset_input.clone()),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created: serde_json::Value = serde_json::from_str(&body_text(created).await).unwrap();
    let preset_id = created["id"].as_str().unwrap();
    assert_eq!(created["manual"]["columns"], 5);
    assert_eq!(created["duplex"]["flipEdge"], "shortEdge");
    assert_eq!(created["impositionMode"], "repeat");

    let mut updated_input = preset_input;
    updated_input["name"] = serde_json::json!("Manual card updated");
    updated_input["impositionMode"] = serde_json::json!("unique");
    let updated = send(
        state.clone(),
        method_json_request(
            Method::PUT,
            &format!("/gang-up/presets/{preset_id}"),
            updated_input,
        ),
    )
    .await;
    assert_eq!(updated.status(), StatusCode::OK);
    let updated: serde_json::Value = serde_json::from_str(&body_text(updated).await).unwrap();
    assert_eq!(updated["name"], "Manual card updated");
    assert_eq!(updated["impositionMode"], "unique");
    assert_eq!(updated["outputPreference"], "cleanPdf");

    let saved_presets = send(state.clone(), get_request("/gang-up/presets")).await;
    let saved_presets: serde_json::Value =
        serde_json::from_str(&body_text(saved_presets).await).unwrap();
    assert_eq!(saved_presets.as_array().unwrap().len(), 2);
    assert_eq!(saved_presets[0]["builtIn"], false);
    assert_eq!(saved_presets[1]["impositionMode"], "unique");

    let selected_request = gang_up_layout_request("single");
    let layout = send(
        state.clone(),
        json_request("/gang-up/layout", selected_request.clone()),
    )
    .await;
    assert_eq!(layout.status(), StatusCode::OK);
    let _: serde_json::Value = serde_json::from_str(&body_text(layout).await).unwrap();

    let recent = send(
        state.clone(),
        json_request(
            "/gang-up/recent-jobs",
            serde_json::json!({
                "name": "Monday cards",
                "sourceFilename": "cards.pdf",
                "request": selected_request,
                "layoutSummary": null
            }),
        ),
    )
    .await;
    assert_eq!(recent.status(), StatusCode::CREATED);
    let recent: serde_json::Value = serde_json::from_str(&body_text(recent).await).unwrap();
    let recent_id = recent["id"].as_str().unwrap();
    let recent_list = send(state.clone(), get_request("/gang-up/recent-jobs")).await;
    assert!(body_text(recent_list).await.contains("Monday cards"));

    assert_eq!(
        send(
            state.clone(),
            delete_request(&format!("/gang-up/recent-jobs/{recent_id}")),
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            state.clone(),
            delete_request(&format!("/gang-up/presets/{preset_id}")),
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_unique_pages_places_each_source_page_once() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    let mut request = gang_up_layout_request("single");
    request["impositionMode"] = serde_json::json!("unique");
    request["sourcePageCount"] = serde_json::json!(35);

    let response = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part(
                    "file",
                    "catalog.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 35, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let exported = pdfium
        .load_pdf_from_byte_vec(body_bytes(response).await, None)
        .unwrap();
    assert_eq!(exported.pages().len(), 2);
    assert_eq!(exported.pages().get(0).unwrap().objects().len(), 30);
    assert_eq!(exported.pages().get(1).unwrap().objects().len(), 5);
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_returns_front_back_sheet_pairs_for_double_sided_jobs() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![
                text_part(
                    "layoutRequest",
                    &gang_up_layout_request("double").to_string(),
                ),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 2, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let exported = pdfium
        .load_pdf_from_byte_vec(body_bytes(response).await, None)
        .unwrap();
    assert_eq!(exported.pages().len(), 6);
    for index in 0..exported.pages().len() {
        assert_eq!(exported.pages().get(index).unwrap().objects().len(), 30);
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn impose_preserves_asymmetric_trim_bleed_and_rotates_back_artwork_pixels() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let source = sample_colored_trim_pdf_bytes(2);
    let analysis_response = send(
        state.clone(),
        multipart_request(
            "/gang-up/analyze",
            vec![upload_part("file", "colored-trim.pdf", source.clone())],
        ),
    )
    .await;
    assert_eq!(analysis_response.status(), StatusCode::OK);
    let analysis: serde_json::Value =
        serde_json::from_str(&body_text(analysis_response).await).unwrap();
    assert_eq!(
        analysis["sourcePdfSize"],
        serde_json::json!({ "width": 3.75, "height": 2.25 })
    );
    assert_eq!(analysis["trimBox"]["width"], 3.5);
    assert_eq!(analysis["trimBox"]["height"], 2.0);
    assert_eq!(analysis["trimBox"]["left"], 0.1944);

    let request = serde_json::json!({
        "sourcePdfSize": analysis["sourcePdfSize"],
        "sourceTrimBox": analysis["trimBox"],
        "sourcePageCount": 2,
        "finishedCutSize": { "width": 3.5, "height": 2.0 },
        "parentSheetSize": { "width": 12.0, "height": 18.0 },
        "quantityRequested": 2,
        "impositionMode": "repeat",
        "impressionQuantities": [2],
        "orientationPreference": "landscape",
        "sides": "double",
        "duplex": { "flipEdge": "longEdge", "rotateBack180": true, "backAlignment": "" },
        "layoutMode": "manual",
        "bleedOption": "useAsIs",
        "createdBleedAmount": 0.125,
        "gutter": { "horizontal": 0.25, "vertical": 0.25 },
        "manual": { "rows": 1, "columns": 2, "rotationDegrees": 0, "margins": null }
    });
    let export_response = send(
        state.clone(),
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part("file", "colored-trim.pdf", source),
            ],
        ),
    )
    .await;
    assert_eq!(export_response.status(), StatusCode::OK);

    let pdfium = state.pdfium();
    let exported = pdfium
        .load_pdf_from_byte_vec(body_bytes(export_response).await, None)
        .unwrap();
    assert_eq!(exported.pages().len(), 2);
    let render = |page_index| {
        exported
            .pages()
            .get(page_index)
            .unwrap()
            .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(1.0))
            .unwrap()
            .as_image()
            .unwrap()
    };
    let front = render(0);
    let back = render(1);
    assert_eq!(front.dimensions(), (864, 1296));

    let assert_color = |image: &image::DynamicImage, x, y, expected: [u8; 3]| {
        let pixel = image.get_pixel(x, y).0;
        assert!(
            pixel[0].abs_diff(expected[0]) <= 4
                && pixel[1].abs_diff(expected[1]) <= 4
                && pixel[2].abs_diff(expected[2]) <= 4,
            "pixel at ({x}, {y}) was {:?}, expected {expected:?}",
            &pixel[..3]
        );
    };

    for (left, right) in [(175, 409), (445, 679)] {
        assert_color(&front, left, 585, [255, 0, 0]);
        assert_color(&front, right, 585, [0, 255, 0]);
        assert_color(&front, left, 711, [0, 0, 255]);
        assert_color(&front, right, 711, [255, 255, 0]);
        assert_color(&back, right, 711, [255, 0, 0]);
        assert_color(&back, left, 711, [0, 255, 0]);
        assert_color(&back, right, 585, [0, 0, 255]);
        assert_color(&back, left, 585, [255, 255, 0]);
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_rejects_double_sided_jobs_without_a_back_page() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![
                text_part(
                    "layoutRequest",
                    &gang_up_layout_request("double").to_string(),
                ),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 1, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response)
        .await
        .contains("double-sided gang-up export requires a source PDF with at least two pages"));
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_rejects_invalid_layout_request_json() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", "{not json"),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 1, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "layoutRequest is not valid JSON");
}

#[tokio::test]
#[serial(pdfium)]
async fn gang_up_export_contains_no_added_mark_objects() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state,
        multipart_request(
            "/gang-up/export",
            vec![
                text_part(
                    "layoutRequest",
                    &gang_up_layout_request("single").to_string(),
                ),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 1, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let exported = pdfium
        .load_pdf_from_byte_vec(body_bytes(response).await, None)
        .unwrap();
    let first = exported.pages().get(0).unwrap();
    assert_eq!(first.objects().len(), 30);
    assert!(first
        .objects()
        .iter()
        .all(|object| object.as_x_object_form_object().is_some()));
}

#[tokio::test]
#[serial(pdfium)]
async fn job_split_download_returns_selected_page_zip() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state.clone(),
        multipart_request(
            "/jobs",
            vec![
                text_part("action", "split"),
                text_part("pages", "1-2"),
                upload_part("file", "source.pdf", sample_pdf_bytes(&pdfium, 2)),
            ],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let id = body_text(response).await;

    let body = wait_for_job_status(state.clone(), &id, "done").await;
    let fields = parse_status_lines(&body);
    assert_eq!(
        fields.get("filename").map(String::as_str),
        Some("source-pages.zip")
    );
    assert_eq!(
        fields.get("content_type").map(String::as_str),
        Some("application/zip")
    );

    let download = send(state, get_request(&format!("/jobs/{id}/download"))).await;
    assert_eq!(download.status(), StatusCode::OK);
    assert!(content_disposition(&download).contains("source-pages.zip"));
    let entries = zip_entries(body_bytes(download).await);

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "source-page-0001.pdf");
    assert_eq!(entries[1].0, "source-page-0002.pdf");
    for (_, bytes) in entries {
        let document = pdfium.load_pdf_from_byte_vec(bytes, None).unwrap();
        assert_eq!(document.pages().len(), 1);
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn job_gang_up_analysis_returns_downloadable_json() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();

    let response = send(
        state.clone(),
        multipart_request(
            "/jobs",
            vec![
                text_part("action", "gang-up-analyze"),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 3, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let id = body_text(response).await;

    let status = wait_for_job_status(state.clone(), &id, "done").await;
    let fields = parse_status_lines(&status);
    assert_eq!(fields.get("percent").map(String::as_str), Some("100"));
    assert_eq!(
        fields.get("filename").map(String::as_str),
        Some("gang-up-analysis.json")
    );
    assert_eq!(
        fields.get("content_type").map(String::as_str),
        Some("application/json")
    );

    let download = send(state, get_request(&format!("/jobs/{id}/download"))).await;
    assert_eq!(download.status(), StatusCode::OK);
    let analysis: serde_json::Value = serde_json::from_slice(&body_bytes(download).await).unwrap();
    assert_eq!(analysis["pageCount"], 3);
    assert_eq!(analysis["matchedPresetId"], serde_json::Value::Null);
}

#[tokio::test]
#[serial(pdfium)]
async fn prepared_gang_up_source_exports_without_a_second_upload() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let prepare_response = send(
        state.clone(),
        multipart_request(
            "/gang-up/sources",
            vec![upload_part(
                "file",
                "card.pdf",
                sample_printable_annotation_pdf_bytes(),
            )],
        ),
    )
    .await;
    assert_eq!(prepare_response.status(), StatusCode::ACCEPTED);
    let prepare_id = body_text(prepare_response).await;
    wait_for_job_status(state.clone(), &prepare_id, "done").await;
    let prepared: serde_json::Value = serde_json::from_slice(
        &body_bytes(
            send(
                state.clone(),
                get_request(&format!("/jobs/{prepare_id}/download")),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    let source_id = prepared["sourceId"].as_str().unwrap();
    assert_eq!(prepared["analysis"]["pageCount"], 1);

    let preview = send(
        state.clone(),
        get_request(&format!("/gang-up/sources/{source_id}/preview/1")),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    assert_eq!(
        preview.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    let preview_image = image::load_from_memory(&body_bytes(preview).await).unwrap();
    let red_pixels = preview_image
        .pixels()
        .filter(|(_, _, pixel)| pixel[0] > 220 && pixel[1] < 40 && pixel[2] < 40)
        .count();
    assert!(red_pixels > 100, "canonical preview omitted the annotation");

    let missing_page = send(
        state.clone(),
        get_request(&format!("/gang-up/sources/{source_id}/preview/2")),
    )
    .await;
    assert_eq!(missing_page.status(), StatusCode::BAD_REQUEST);

    let renewed = send(
        state.clone(),
        put_request(&format!("/gang-up/sources/{source_id}/lease")),
    )
    .await;
    assert_eq!(renewed.status(), StatusCode::NO_CONTENT);

    let layout_value = gang_up_layout_request("single");
    let layout = send(
        state.clone(),
        json_request("/gang-up/layout", layout_value.clone()),
    )
    .await;
    assert_eq!(layout.status(), StatusCode::OK);
    let layout_result: serde_json::Value =
        serde_json::from_slice(&body_bytes(layout).await).unwrap();
    assert!(layout_result["piecesPerSheet"]
        .as_u64()
        .is_some_and(|count| count > 0));
    let layout_request = layout_value.to_string();
    let export_response = send(
        state.clone(),
        multipart_request(
            "/jobs",
            vec![
                text_part("action", "gang-up-export-source"),
                text_part("sourceId", source_id),
                text_part("layoutRequest", &layout_request),
            ],
        ),
    )
    .await;
    assert_eq!(export_response.status(), StatusCode::ACCEPTED);
    let export_id = body_text(export_response).await;
    wait_for_job_status(state.clone(), &export_id, "done").await;
    let exported = send(
        state.clone(),
        get_request(&format!("/jobs/{export_id}/download")),
    )
    .await;
    assert_eq!(exported.status(), StatusCode::OK);
    let exported_bytes = body_bytes(exported).await;
    assert!(exported_bytes.starts_with(b"%PDF-"));
    let exported_document = LoDocument::load_mem(&exported_bytes).unwrap();
    assert!(exported_document
        .get_pages()
        .into_values()
        .all(|page_id| exported_document
            .get_dictionary(page_id)
            .is_ok_and(|page| page.get(b"Annots").is_err())));

    let deleted = send(
        state.clone(),
        delete_request(&format!("/gang-up/sources/{source_id}")),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let expired_export = send(
        state.clone(),
        multipart_request(
            "/jobs",
            vec![
                text_part("action", "gang-up-export-source"),
                text_part("sourceId", source_id),
                text_part("layoutRequest", &layout_request),
            ],
        ),
    )
    .await;
    let expired_id = body_text(expired_export).await;
    let expired_status = wait_for_job_status(state, &expired_id, "error").await;
    assert!(expired_status.contains("prepared source PDF was not found or expired"));
}

#[tokio::test]
#[serial(pdfium)]
async fn prepared_source_accepts_101_image_files_and_cleans_up() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let staging_before = impose_staging_paths(&state);
    let png = solid_png_bytes(Rgba([30, 90, 150, 255]));
    let files = (0..101)
        .map(|index| upload_part("files", &format!("page-{index:03}.png"), png.clone()))
        .collect();
    let response = send(state.clone(), multipart_request("/gang-up/sources", files)).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job_id = body_text(response).await;
    assert!(!job_id.trim().is_empty());
    wait_for_job_status(state.clone(), &job_id, "done").await;

    let prepared: serde_json::Value = serde_json::from_slice(
        &body_bytes(
            send(
                state.clone(),
                get_request(&format!("/jobs/{job_id}/download")),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(prepared["analysis"]["pageCount"], 101);
    let source_id = prepared["sourceId"].as_str().unwrap();

    for requested_pages in [[74, 75, 76, 77], [87, 88, 89, 90]] {
        let preview = send(
            state.clone(),
            json_request(
                &format!("/gang-up/sources/{source_id}/previews"),
                serde_json::json!({ "pageNumbers": requested_pages }),
            ),
        )
        .await;
        assert_eq!(preview.status(), StatusCode::OK);
        let preview_pages = preview_batch_pages(&body_bytes(preview).await);
        assert_eq!(
            preview_pages.keys().copied().collect::<Vec<_>>(),
            requested_pages
        );
        for page in requested_pages {
            let png = &preview_pages[&page];
            assert!(!png.is_empty(), "page {page} preview was empty");
            let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap();
            assert!(image.width() > 0, "page {page} preview had no pixel width");
            assert!(
                image.height() > 0,
                "page {page} preview had no pixel height"
            );
            assert!(
                !image.as_bytes().is_empty(),
                "page {page} preview had no pixels"
            );
        }
    }

    assert_eq!(
        send(
            state.clone(),
            delete_request(&format!("/gang-up/sources/{source_id}")),
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(state.clone(), delete_request(&format!("/jobs/{job_id}")))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(impose_staging_paths(&state), staging_before);
}

#[tokio::test]
#[serial(pdfium)]
async fn prepared_gang_up_source_preserves_order_across_pdfs_and_images_and_cleans_staging() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let staging_before = impose_staging_paths(&state);
    let prepare_response = send(
        state.clone(),
        multipart_request(
            "/gang-up/sources",
            vec![
                upload_part(
                    "files",
                    "red.pdf",
                    solid_business_card_pdf_bytes(1.0, 0.0, 0.0),
                ),
                upload_part(
                    "files",
                    "green.png",
                    solid_png_bytes(Rgba([0, 255, 0, 255])),
                ),
                upload_part(
                    "files",
                    "yellow.png",
                    solid_png_bytes(Rgba([255, 255, 0, 255])),
                ),
                upload_part(
                    "files",
                    "blue.pdf",
                    solid_business_card_pdf_bytes(0.0, 0.0, 1.0),
                ),
            ],
        ),
    )
    .await;
    assert_eq!(prepare_response.status(), StatusCode::ACCEPTED);
    let prepare_id = body_text(prepare_response).await;
    wait_for_job_status(state.clone(), &prepare_id, "done").await;
    assert_eq!(impose_staging_paths(&state), staging_before);

    let prepared: serde_json::Value = serde_json::from_slice(
        &body_bytes(
            send(
                state.clone(),
                get_request(&format!("/jobs/{prepare_id}/download")),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(prepared["analysis"]["pageCount"], 4);
    let source_id = prepared["sourceId"].as_str().unwrap();

    let preview = send(
        state.clone(),
        json_request(
            &format!("/gang-up/sources/{source_id}/previews"),
            serde_json::json!({ "pageNumbers": [1, 2, 3, 4] }),
        ),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    assert_eq!(
        preview.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.pdf-tools.preview-batch"
    );
    let preview_pages = preview_batch_pages(&body_bytes(preview).await);
    assert_eq!(
        preview_pages.keys().copied().collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );

    for (page, expected) in [
        (1, [true, false, false]),
        (2, [false, true, false]),
        (3, [true, true, false]),
        (4, [false, false, true]),
    ] {
        let image = image::load_from_memory(&preview_pages[&page]).unwrap();
        let center = image.get_pixel(image.width() / 2, image.height() / 2);
        for channel in 0..3 {
            let correct = if expected[channel] {
                center[channel] > 220
            } else {
                center[channel] < 40
            };
            assert!(correct, "page {page} color was {center:?}");
        }
    }

    let oversized_batch = send(
        state.clone(),
        json_request(
            &format!("/gang-up/sources/{source_id}/previews"),
            serde_json::json!({ "pageNumbers": [1, 2, 3, 4, 5] }),
        ),
    )
    .await;
    assert_eq!(oversized_batch.status(), StatusCode::BAD_REQUEST);

    let duplicate_page = send(
        state.clone(),
        json_request(
            &format!("/gang-up/sources/{source_id}/previews"),
            serde_json::json!({ "pageNumbers": [1, 1] }),
        ),
    )
    .await;
    assert_eq!(duplicate_page.status(), StatusCode::BAD_REQUEST);

    let deleted = send(
        state.clone(),
        delete_request(&format!("/gang-up/sources/{source_id}")),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let released = send(
        state.clone(),
        delete_request(&format!("/jobs/{prepare_id}")),
    )
    .await;
    assert_eq!(released.status(), StatusCode::NO_CONTENT);
    assert_eq!(impose_staging_paths(&state), staging_before);
}

#[tokio::test]
#[serial(pdfium)]
async fn prepared_gang_up_source_retains_mixed_geometry_and_original_file_pages() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    let staging_before = impose_staging_paths(&state);
    let prepare_response = send(
        state.clone(),
        multipart_request(
            "/gang-up/sources",
            vec![
                upload_part(
                    "files",
                    "imposed.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 3, 12.0, 18.0),
                ),
                upload_part(
                    "files",
                    "converted.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 2, 2.75, 3.75),
                ),
            ],
        ),
    )
    .await;
    assert_eq!(prepare_response.status(), StatusCode::ACCEPTED);
    let prepare_id = body_text(prepare_response).await;

    wait_for_job_status(state.clone(), &prepare_id, "done").await;
    let response = send(
        state.clone(),
        get_request(&format!("/jobs/{prepare_id}/download")),
    )
    .await;
    let prepared: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    let pages = prepared["analysis"]["sourcePages"].as_array().unwrap();
    assert_eq!(pages.len(), 5);
    assert_eq!(
        pages[0]["sourcePdfSize"],
        serde_json::json!({"width":12.0,"height":18.0})
    );
    assert_eq!(
        pages[3]["sourcePdfSize"],
        serde_json::json!({"width":2.75,"height":3.75})
    );
    assert_eq!(pages[3]["filename"], "converted.pdf");
    assert_eq!(pages[3]["originalPageNumber"], 1);
    assert_eq!(pages[4]["originalPageNumber"], 2);
    assert_eq!(impose_staging_paths(&state), staging_before);
}

#[tokio::test]
#[serial(pdfium)]
async fn streamed_impose_source_intake_enforces_its_own_total_byte_limit() {
    let Some(pdfium) = test_pdfium() else {
        return;
    };
    let state = Arc::new(
        AppState::for_tests_with_impose_upload_limit(pdfium, TEST_IMPOSE_UPLOAD_LIMIT_BYTES)
            .unwrap(),
    );
    let staging_before = impose_staging_paths(&state);
    let request = oversized_streaming_pdf_request("/gang-up/sources");
    assert!(
        request
            .headers()
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > TEST_IMPOSE_UPLOAD_LIMIT_BYTES as u64
    );
    let response = app(state.clone(), Some(1))
        .unwrap()
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body_text(response)
        .await
        .contains("configured 1 MiB total upload limit"));
    assert_eq!(impose_staging_paths(&state), staging_before);
}

#[tokio::test]
#[serial(pdfium)]
async fn job_gang_up_exports_return_pdf_download() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let pdfium = state.pdfium();
    let layout_request = gang_up_layout_request("single").to_string();

    let pdf_response = send(
        state.clone(),
        multipart_request(
            "/jobs",
            vec![
                text_part("action", "gang-up-export"),
                text_part("layoutRequest", &layout_request),
                upload_part(
                    "file",
                    "card.pdf",
                    sample_pdf_bytes_with_size(&pdfium, 1, 3.5, 2.0),
                ),
            ],
        ),
    )
    .await;
    assert_eq!(pdf_response.status(), StatusCode::ACCEPTED);
    let pdf_id = body_text(pdf_response).await;
    let pdf_status = wait_for_job_status(state.clone(), &pdf_id, "done").await;
    assert!(pdf_status.contains("filename=card-imposed.pdf"));
    let pdf_download = send(
        state.clone(),
        get_request(&format!("/jobs/{pdf_id}/download")),
    )
    .await;
    assert_eq!(pdf_download.status(), StatusCode::OK);
    assert!(body_bytes(pdf_download).await.starts_with(b"%PDF-"));
}

#[tokio::test]
#[serial(pdfium)]
async fn job_impose_action_is_not_a_generic_job_tool() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let response = send(
        state.clone(),
        multipart_request(
            "/jobs",
            vec![
                text_part("action", "impose"),
                upload_part("file", "source.pdf", b"%PDF placeholder".to_vec()),
            ],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "unknown job action");
}

#[tokio::test]
#[serial(pdfium)]
async fn jobs_unknown_action_is_rejected_at_the_boundary() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let response = send(
        state.clone(),
        multipart_request("/jobs", vec![text_part("action", "unknown")]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "unknown job action");
}

#[tokio::test]
#[serial(pdfium)]
async fn job_status_and_download_errors_are_reported() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let missing = send(state.clone(), get_request("/jobs/missing")).await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    let id = state.create_test_job().unwrap();
    let pending_download = send(state.clone(), get_request(&format!("/jobs/{id}/download"))).await;
    assert_eq!(pending_download.status(), StatusCode::CONFLICT);

    state.fail_test_job(&id, "bad\nthing").unwrap();
    let status = send(state.clone(), get_request(&format!("/jobs/{id}"))).await;
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(
        status.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let fields = parse_status_lines(&body_text(status).await);
    assert_eq!(fields.get("status").map(String::as_str), Some("error"));
    assert_eq!(fields.get("error").map(String::as_str), Some("bad thing"));

    let cancelled_id = state.create_test_job().unwrap();
    let cancelled = send(
        state.clone(),
        delete_request(&format!("/jobs/{cancelled_id}")),
    )
    .await;
    assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);
    let cancelled_status = send(state, get_request(&format!("/jobs/{cancelled_id}"))).await;
    let cancelled_fields = parse_status_lines(&body_text(cancelled_status).await);
    assert_eq!(
        cancelled_fields.get("error").map(String::as_str),
        Some("operation was cancelled")
    );
}

#[tokio::test]
#[serial(pdfium)]
async fn job_convert_pdf_download_can_be_retried() {
    let Some(state) = state_or_skip() else {
        return;
    };

    let response = send(
        state.clone(),
        multipart_request(
            "/jobs",
            vec![
                text_part("action", "convert"),
                text_part("target", "pdf"),
                upload_part("files", "image.png", sample_png_bytes()),
            ],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let id = body_text(response).await;

    let body = wait_for_job_status(state.clone(), &id, "done").await;
    let fields = parse_status_lines(&body);
    assert_eq!(
        fields.get("filename").map(String::as_str),
        Some("image-converted.pdf")
    );

    let download = send(state.clone(), get_request(&format!("/jobs/{id}/download"))).await;
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        download.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert!(content_disposition(&download).contains("converted.pdf"));
    assert!(body_bytes(download).await.starts_with(b"%PDF-"));

    let second_download = send(state, get_request(&format!("/jobs/{id}/download"))).await;
    assert_eq!(second_download.status(), StatusCode::OK);
    assert!(body_bytes(second_download).await.starts_with(b"%PDF-"));
}
