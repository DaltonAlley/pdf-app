//! Browser transport for the imposition workflow.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    rc::Rc,
};

use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, FormData, HtmlImageElement, Request, RequestInit, Response, Url};

use crate::{
    browser::{
        execute_job_download, http_response_failure, response_failure, response_text,
        BrowserCancellation, FetchDeadline, JobDownload, JobProgress, ProgressSink,
    },
    files::SelectedFile,
    transport::HttpRequestContext,
};

use super::{
    decode_preview_batch, http_failure_kind, preview_cache_evictions,
    should_remove_failed_preview_url, validate_layout_result, validate_prepared_source,
    GangUpPreset, LayoutRequest, LayoutResult, PreparedPdfSource, PresetInput, PreviewCacheLimits,
    PreviewFailureKind, MAX_PLACEMENTS,
};

pub(crate) struct VisibilityLeaseListener {
    document: web_sys::Document,
    callback: Closure<dyn FnMut(web_sys::Event)>,
}

const DEFAULT_PREVIEW_CACHE_LIMITS: PreviewCacheLimits = PreviewCacheLimits {
    pages: MAX_PLACEMENTS + 1,
    bytes: 64 * 1024 * 1024,
};

#[derive(Debug)]
pub(crate) struct PreviewRequestError {
    message: String,
    kind: PreviewFailureKind,
}

impl PreviewRequestError {
    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: PreviewFailureKind::Retryable,
        }
    }

    fn deterministic(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: PreviewFailureKind::Deterministic,
        }
    }

    pub(crate) fn should_retry(&self) -> bool {
        self.kind == PreviewFailureKind::Retryable
    }

    pub(crate) fn into_message(self) -> String {
        self.message
    }
}

#[derive(Debug)]
pub(crate) struct DecodedPreviewAsset {
    page: usize,
    url: Option<String>,
    bytes: usize,
}

impl DecodedPreviewAsset {
    pub(crate) fn page(&self) -> usize {
        self.page
    }

    fn into_parts(mut self) -> (usize, String, usize) {
        let url = self.url.take().unwrap_or_default();
        (self.page, url, self.bytes)
    }
}

impl Drop for DecodedPreviewAsset {
    fn drop(&mut self) {
        if let Some(url) = self.url.take() {
            let _ = Url::revoke_object_url(&url);
        }
    }
}

struct PreviewCacheEntry {
    url: String,
    bytes: usize,
}

impl Drop for PreviewCacheEntry {
    fn drop(&mut self) {
        let _ = Url::revoke_object_url(&self.url);
    }
}

/// Browser-owned preview URLs with bounded least-recently-used retention.
#[derive(Default)]
pub(crate) struct PreviewUrlCache {
    source_id: Option<String>,
    entries: BTreeMap<usize, PreviewCacheEntry>,
    recent: VecDeque<usize>,
    protected: BTreeSet<usize>,
}

impl PreviewUrlCache {
    pub(crate) fn select_source(&mut self, source_id: &str) {
        if self.source_id.as_deref() == Some(source_id) {
            return;
        }
        self.clear();
        self.source_id = Some(source_id.to_owned());
    }

    pub(crate) fn url(&self, page: usize) -> Option<&str> {
        self.entries.get(&page).map(|entry| entry.url.as_str())
    }

    pub(crate) fn url_for_source(&self, raster_identity: &str, page: usize) -> Option<&str> {
        (self.source_id.as_deref() == Some(raster_identity))
            .then(|| self.url(page))
            .flatten()
    }

    pub(crate) fn completed(&self, pages: &[usize]) -> usize {
        pages
            .iter()
            .filter(|page| self.entries.contains_key(page))
            .count()
    }

    pub(crate) fn missing(&self, pages: &[usize]) -> Vec<usize> {
        pages
            .iter()
            .copied()
            .filter(|page| !self.entries.contains_key(page))
            .collect()
    }

    pub(crate) fn touch_visible(&mut self, pages: &[usize]) {
        for page in pages {
            if self.entries.contains_key(page) {
                self.touch(*page);
            }
        }
    }

    /// Inserts a rendered page only while the cache still belongs to its source.
    ///
    /// A source replacement can publish its first page before the old preview task observes
    /// cancellation. Keeping the ownership check beside the mutation prevents that old task from
    /// overwriting replacement URLs whose page numbers happen to match.
    pub(crate) fn insert_for_current_source(
        &mut self,
        source_id: &str,
        page: usize,
        asset: DecodedPreviewAsset,
    ) -> Result<bool, String> {
        if self.source_id.as_deref() != Some(source_id) {
            return Ok(false);
        }
        if page != asset.page() {
            return Err("The decoded artwork preview did not match its requested page.".into());
        }
        self.insert_asset(asset)?;
        Ok(true)
    }

    /// Creates an unpublished cache for incrementally admitting a newly prepared first sheet.
    pub(crate) fn staging_for_source(
        source_id: &str,
        protected: &HashSet<usize>,
    ) -> Result<Self, String> {
        let mut cache = Self::default();
        cache.select_source(source_id);
        if !cache.set_protected(source_id, protected)? {
            return Err("The artwork preview source changed before caching began.".into());
        }
        Ok(cache)
    }

    /// Atomically publishes a fully admitted staging cache and releases the previous source.
    pub(crate) fn replace_with(&mut self, replacement: Self) {
        let previous = std::mem::replace(self, replacement);
        drop(previous);
    }

    pub(crate) fn set_protected(
        &mut self,
        source_id: &str,
        protected: &HashSet<usize>,
    ) -> Result<bool, String> {
        if self.source_id.as_deref() != Some(source_id) {
            return Ok(false);
        }
        let protected = protected.iter().copied().collect::<BTreeSet<_>>();
        let evictions = preview_cache_evictions(
            &self.sizes(),
            &self.recent,
            &protected,
            DEFAULT_PREVIEW_CACHE_LIMITS,
        )?;
        self.protected = protected;
        self.remove_pages(&evictions);
        Ok(true)
    }

    pub(crate) fn ensure_admissible(&self, assets: &[DecodedPreviewAsset]) -> Result<(), String> {
        let mut projected = self.sizes();
        let mut recent = self.recent.clone();
        for asset in assets {
            projected.insert(asset.page, asset.bytes);
            recent.retain(|page| *page != asset.page);
            recent.push_back(asset.page);
        }
        preview_cache_evictions(
            &projected,
            &recent,
            &self.protected,
            DEFAULT_PREVIEW_CACHE_LIMITS,
        )?;
        Ok(())
    }

    /// Removes a page only while this cache still belongs to the expected source.
    ///
    /// Image decode callbacks can outlive a source replacement, while page numbers are reused by
    /// every source. Checking ownership here prevents an old callback from revoking the current
    /// source's URL for the same page.
    pub(crate) fn remove_failed_url(
        &mut self,
        source_id: &str,
        page: usize,
        failed_url: &str,
    ) -> bool {
        if self.source_id.as_deref() != Some(source_id) {
            return false;
        }
        if !should_remove_failed_preview_url(self.url(page), failed_url) {
            return false;
        }
        self.remove(page);
        true
    }

    fn remove(&mut self, page: usize) {
        self.entries.remove(&page);
        self.recent.retain(|current| *current != page);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.recent.clear();
        self.protected.clear();
        self.source_id = None;
    }

    fn touch(&mut self, page: usize) {
        self.recent.retain(|current| *current != page);
        self.recent.push_back(page);
    }

    fn insert_asset(&mut self, asset: DecodedPreviewAsset) -> Result<(), String> {
        let (page, url, bytes) = asset.into_parts();
        let mut projected = self.sizes();
        projected.insert(page, bytes);
        let mut recent = self.recent.clone();
        recent.retain(|current| *current != page);
        recent.push_back(page);
        let evictions = match preview_cache_evictions(
            &projected,
            &recent,
            &self.protected,
            DEFAULT_PREVIEW_CACHE_LIMITS,
        ) {
            Ok(evictions) => evictions,
            Err(error) => {
                let _ = Url::revoke_object_url(&url);
                return Err(error);
            }
        };
        self.entries.insert(page, PreviewCacheEntry { url, bytes });
        self.touch(page);
        self.remove_pages(&evictions);
        Ok(())
    }

    fn sizes(&self) -> BTreeMap<usize, usize> {
        self.entries
            .iter()
            .map(|(page, entry)| (*page, entry.bytes))
            .collect()
    }

    fn remove_pages(&mut self, pages: &[usize]) {
        for page in pages {
            self.remove(*page);
        }
    }
}

impl Drop for PreviewUrlCache {
    fn drop(&mut self) {
        self.clear();
    }
}

fn preview_object_url(blob: &Blob) -> Result<String, String> {
    Url::create_object_url_with_blob(blob).map_err(|error| {
        error
            .as_string()
            .unwrap_or_else(|| "The browser could not create an artwork preview URL.".into())
    })
}

pub(crate) async fn decode_preview_assets(
    rendered: Vec<(usize, Blob)>,
) -> Result<Vec<DecodedPreviewAsset>, PreviewRequestError> {
    let mut decoded = Vec::with_capacity(rendered.len());
    for (page, blob) in rendered {
        let url = preview_object_url(&blob).map_err(PreviewRequestError::deterministic)?;
        let image = HtmlImageElement::new().map_err(|error| {
            let _ = Url::revoke_object_url(&url);
            PreviewRequestError::deterministic(js_error(error))
        })?;
        image.set_src(&url);
        if let Err(error) = JsFuture::from(image.decode()).await {
            let _ = Url::revoke_object_url(&url);
            return Err(PreviewRequestError::deterministic(format!(
                "The browser could not decode artwork preview page {page}: {}",
                js_error(error)
            )));
        }
        let bytes = blob.size().clamp(0.0, usize::MAX as f64) as usize;
        decoded.push(DecodedPreviewAsset {
            page,
            url: Some(url),
            bytes,
        });
    }
    Ok(decoded)
}

impl Drop for VisibilityLeaseListener {
    fn drop(&mut self) {
        let _ = self.document.remove_event_listener_with_callback(
            "visibilitychange",
            self.callback.as_ref().unchecked_ref(),
        );
    }
}

pub(crate) async fn prepare_source(
    files: &[SelectedFile],
    progress: ProgressSink,
    cancellation: BrowserCancellation,
) -> Result<PreparedPdfSource, String> {
    let form = FormData::new().map_err(js_error)?;
    for selected in files {
        form.append_with_blob_and_filename("files", &selected.file, &selected.descriptor.name)
            .map_err(js_error)?;
    }
    let result = execute_job_download(form, "/gang-up/sources", progress, cancellation).await?;
    let text = blob_text(&result.blob).await?;
    let prepared: PreparedPdfSource = serde_json::from_str(&text)
        .map_err(|error| format!("The prepared PDF source had an invalid shape: {error}"))?;
    validate_prepared_source(&prepared)?;
    Ok(prepared)
}

pub(crate) async fn request_layout(
    request: &LayoutRequest,
    cancellation: &BrowserCancellation,
) -> Result<LayoutResult, String> {
    let deadline = FetchDeadline::new(Some(cancellation), 120_000)?;
    let body = super::model::wire_layout_request(request)
        .map_err(|error| format!("Could not encode the layout request: {error}"))?;
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(&body));
    init.set_signal(Some(&deadline.signal()));
    let web_request = Request::new_with_str_and_init("/gang-up/layout", &init).map_err(js_error)?;
    web_request
        .headers()
        .set("content-type", "application/json")
        .map_err(js_error)?;
    let response = JsFuture::from(window()?.fetch_with_request(&web_request))
        .await
        .map_err(|error| {
            deadline.request_error(
                Some(cancellation),
                "The layout request was cancelled.",
                "Calculating the sheet layout took longer than 120 seconds. Retry the layout.",
                HttpRequestContext::Layout,
                error,
            )
        })?
        .dyn_into::<Response>()
        .map_err(js_error)?;
    let text = response_text(&response).await?;
    if !response.ok() {
        return Err(http_response_failure(
            &response,
            &text,
            HttpRequestContext::Layout,
        ));
    }
    let layout: LayoutResult = serde_json::from_str(&text)
        .map_err(|error| format!("The layout result had an invalid shape: {error}"))?;
    validate_layout_result(&layout)?;
    if request.artwork_fit.is_some() && layout.page_plans.is_empty() {
        return Err("The server did not return an authoritative artwork placement plan. Reload after the server is updated.".into());
    }
    Ok(layout)
}

pub(crate) async fn export_pdf(
    source_id: &str,
    request: &LayoutRequest,
    progress: ProgressSink,
    cancellation: BrowserCancellation,
) -> Result<JobDownload, String> {
    let form = FormData::new().map_err(js_error)?;
    form.append_with_str("action", "gang-up-export-source")
        .map_err(js_error)?;
    form.append_with_str("sourceId", source_id)
        .map_err(js_error)?;
    let layout = super::model::wire_layout_request(request)
        .map_err(|error| format!("Could not encode the export layout: {error}"))?;
    form.append_with_str("layoutRequest", &layout)
        .map_err(js_error)?;
    execute_job_download(form, "/jobs", progress, cancellation).await
}

pub(crate) async fn delete_source(source_id: &str) -> Result<(), String> {
    source_request(source_id, "", "DELETE", HttpRequestContext::DeleteSource).await
}

async fn preset_request(path: &str, method: &str, body: Option<String>) -> Result<String, String> {
    let init = RequestInit::new();
    init.set_method(method);
    if let Some(body) = body.as_deref() {
        init.set_body(&JsValue::from_str(body));
    }
    let request = Request::new_with_str_and_init(path, &init).map_err(js_error)?;
    if body.is_some() {
        request
            .headers()
            .set("content-type", "application/json")
            .map_err(js_error)?;
    }
    let response = JsFuture::from(window()?.fetch_with_request(&request))
        .await
        .map_err(js_error)?
        .dyn_into::<Response>()
        .map_err(js_error)?;
    let text = response_text(&response).await?;
    if !response.ok() {
        return Err(format!(
            "Preset request failed (HTTP {}): {}",
            response.status(),
            text.trim()
        ));
    }
    Ok(text)
}

pub(crate) async fn list_presets() -> Result<Vec<GangUpPreset>, String> {
    let text = preset_request("/gang-up/presets", "GET", None).await?;
    serde_json::from_str(&text)
        .map_err(|error| format!("The preset response had an invalid shape: {error}"))
}

pub(crate) async fn create_preset(input: &PresetInput) -> Result<GangUpPreset, String> {
    let text = preset_request(
        "/gang-up/presets",
        "POST",
        Some(serde_json::to_string(input).map_err(|e| e.to_string())?),
    )
    .await?;
    serde_json::from_str(&text)
        .map_err(|error| format!("The created preset had an invalid shape: {error}"))
}

pub(crate) async fn update_preset(id: &str, input: &PresetInput) -> Result<GangUpPreset, String> {
    let text = preset_request(
        &format!("/gang-up/presets/{}", encoded_source_id(id)),
        "PUT",
        Some(serde_json::to_string(input).map_err(|e| e.to_string())?),
    )
    .await?;
    serde_json::from_str(&text)
        .map_err(|error| format!("The updated preset had an invalid shape: {error}"))
}

pub(crate) async fn delete_preset(id: &str) -> Result<(), String> {
    let _ = preset_request(
        &format!("/gang-up/presets/{}", encoded_source_id(id)),
        "DELETE",
        None,
    )
    .await?;
    Ok(())
}

pub(crate) async fn renew_source(source_id: &str) -> Result<(), String> {
    source_request(source_id, "/lease", "PUT", HttpRequestContext::RenewSource).await
}

pub(crate) async fn request_preview_pages(
    source_id: &str,
    page_numbers: &[usize],
    source_bleed_override: Option<f64>,
    cancellation: &BrowserCancellation,
) -> Result<Vec<(usize, Blob)>, PreviewRequestError> {
    let deadline =
        FetchDeadline::new(Some(cancellation), 120_000).map_err(PreviewRequestError::retryable)?;
    let body = serde_json::to_string(&serde_json::json!({ "pageNumbers": page_numbers, "sourceBleedOverride": source_bleed_override })).map_err(
        |error| {
            PreviewRequestError::deterministic(format!(
                "Could not encode the preview request: {error}"
            ))
        },
    )?;
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(&body));
    init.set_signal(Some(&deadline.signal()));
    let endpoint = format!("/gang-up/sources/{}/previews", encoded_source_id(source_id));
    let request = Request::new_with_str_and_init(&endpoint, &init)
        .map_err(js_error)
        .map_err(PreviewRequestError::deterministic)?;
    request
        .headers()
        .set("content-type", "application/json")
        .map_err(js_error)
        .map_err(PreviewRequestError::deterministic)?;
    let browser_window = window().map_err(PreviewRequestError::deterministic)?;
    let response = JsFuture::from(browser_window.fetch_with_request(&request))
        .await
        .map_err(|error| {
            PreviewRequestError::retryable(deadline.request_error(
                Some(cancellation),
                "Artwork preview loading was cancelled.",
                "Artwork preview rendering took longer than 120 seconds.",
                HttpRequestContext::Preview,
                error,
            ))
        })?
        .dyn_into::<Response>()
        .map_err(js_error)
        .map_err(PreviewRequestError::deterministic)?;
    if !response.ok() {
        let status = response.status();
        let message = response_failure(&response, HttpRequestContext::Preview)
            .await
            .map_err(PreviewRequestError::deterministic)?;
        return Err(PreviewRequestError {
            message,
            kind: http_failure_kind(status),
        });
    }
    let content_type = response
        .headers()
        .get("content-type")
        .map_err(js_error)
        .map_err(PreviewRequestError::deterministic)?
        .and_then(|value| value.split(';').next().map(str::trim).map(str::to_owned));
    if content_type.as_deref() != Some("application/vnd.pdf-tools.preview-batch") {
        return Err(PreviewRequestError::deterministic(
            "Artwork preview response had an unsupported format.",
        ));
    }
    let buffer = JsFuture::from(
        response
            .array_buffer()
            .map_err(js_error)
            .map_err(PreviewRequestError::deterministic)?,
    )
    .await
    .map_err(js_error)
    .map_err(PreviewRequestError::deterministic)?;
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
    decode_preview_batch(&bytes, page_numbers)
        .map_err(PreviewRequestError::deterministic)?
        .into_iter()
        .map(|(page, png)| {
            let array = js_sys::Array::new();
            array.push(&js_sys::Uint8Array::from(png.as_slice()));
            Blob::new_with_u8_array_sequence(array.as_ref())
                .map(|blob| (page, blob))
                .map_err(js_error)
                .map_err(PreviewRequestError::deterministic)
        })
        .collect()
}

pub(crate) fn visibility_lease_listener(
    callback: impl Fn() + 'static,
) -> Result<VisibilityLeaseListener, String> {
    let document = window()?
        .document()
        .ok_or_else(|| "The browser document is unavailable.".to_string())?;
    let callback_document = document.clone();
    let callback = Closure::wrap(Box::new(move |_event: web_sys::Event| {
        if !callback_document.hidden() {
            callback();
        }
    }) as Box<dyn FnMut(_)>);
    document
        .add_event_listener_with_callback("visibilitychange", callback.as_ref().unchecked_ref())
        .map_err(js_error)?;
    Ok(VisibilityLeaseListener { document, callback })
}

pub(crate) fn progress_sink(callback: impl Fn(JobProgress) + 'static) -> ProgressSink {
    Rc::new(callback)
}

async fn source_request(
    source_id: &str,
    suffix: &str,
    method: &str,
    context: HttpRequestContext,
) -> Result<(), String> {
    let deadline = FetchDeadline::new(None, 120_000)?;
    let encoded = encoded_source_id(source_id);
    let init = RequestInit::new();
    init.set_method(method);
    init.set_signal(Some(&deadline.signal()));
    let request =
        Request::new_with_str_and_init(&format!("/gang-up/sources/{encoded}{suffix}"), &init)
            .map_err(js_error)?;
    let response = JsFuture::from(window()?.fetch_with_request(&request))
        .await
        .map_err(|error| {
            deadline.request_error(
                None,
                "The prepared-source request was cancelled.",
                "The prepared-source request took longer than 120 seconds.",
                context,
                error,
            )
        })?
        .dyn_into::<Response>()
        .map_err(js_error)?;
    if response.ok() {
        Ok(())
    } else {
        Err(response_failure(&response, context).await?)
    }
}

fn encoded_source_id(source_id: &str) -> String {
    js_sys::encode_uri_component(source_id)
        .as_string()
        .unwrap_or_else(|| source_id.to_owned())
}

async fn blob_text(blob: &Blob) -> Result<String, String> {
    JsFuture::from(blob.text())
        .await
        .map_err(js_error)?
        .as_string()
        .ok_or_else(|| "The prepared source response was not text.".into())
}

fn window() -> Result<web_sys::Window, String> {
    web_sys::window().ok_or_else(|| "The browser window is unavailable.".into())
}

fn js_error(value: JsValue) -> String {
    value
        .as_string()
        .unwrap_or_else(|| "The browser could not complete the request.".into())
}
