//! Shared browser transport and DOM boundaries.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use futures_channel::oneshot;
use gloo_timers::{callback::Timeout, future::TimeoutFuture};
use leptos::{
    html,
    prelude::{Get, NodeRef},
};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    AbortController, Blob, Document, Event, FormData, HtmlAnchorElement, HtmlDialogElement,
    ProgressEvent, Request, RequestCache, RequestInit, ResizeObserver, Response, Url, Window,
    XmlHttpRequest, XmlHttpRequestResponseType, XmlHttpRequestUpload,
};

use crate::transport::{
    http_failure_message, job_poll_delay, job_progress_detail, parse_job_status, HttpFailure,
    HttpRequestContext, JobPhase, JobStatus,
};

/// Keeps a resize callback alive for as long as its observed workflow elements exist.
pub(crate) struct ElementResizeListener {
    observer: ResizeObserver,
    _callback: Closure<dyn FnMut()>,
}

impl Drop for ElementResizeListener {
    fn drop(&mut self) {
        self.observer.disconnect();
    }
}

pub(crate) fn observe_element_resizes(
    elements: &[&web_sys::Element],
    on_resize: impl FnMut() + 'static,
) -> Result<ElementResizeListener, String> {
    let callback = Closure::<dyn FnMut()>::new(on_resize);
    let observer = ResizeObserver::new(callback.as_ref().unchecked_ref()).map_err(js_error)?;
    for element in elements {
        observer.observe(element);
    }
    Ok(ElementResizeListener {
        observer,
        _callback: callback,
    })
}

/// Carries the current visual position across the centered/full-height layout
/// boundary. Capture before changing the operation, then measure the committed
/// DOM before the next paint. A generation makes same-frame reversals harmless.
#[derive(Clone, Default)]
pub(crate) struct WorkflowMotion(Rc<Cell<u64>>);

impl WorkflowMotion {
    pub(crate) fn run(&self, change: impl FnOnce()) {
        let generation = self.0.get().wrapping_add(1);
        self.0.set(generation);
        let document = web_sys::window().and_then(|window| window.document());
        let elements = [".app-header", ".ready-shell"]
            .into_iter()
            .filter_map(|selector| {
                let element = document.as_ref()?.query_selector(selector).ok()??;
                let y = element.get_bounding_client_rect().y();
                Some((element.dyn_into::<web_sys::HtmlElement>().ok()?, y))
            })
            .collect::<Vec<_>>();
        let current = self.0.clone();
        change();
        // Leptos commits its queued effects first. Invert in the following
        // microtask, before any animation-frame observers or browser paint.
        spawn_local(async move {
            if current.get() != generation {
                return;
            }
            let reduced = web_sys::window()
                .and_then(|window| {
                    window
                        .match_media("(prefers-reduced-motion: reduce)")
                        .ok()
                        .flatten()
                })
                .is_some_and(|query| query.matches());
            for (element, previous_y) in elements {
                if !element.is_connected() {
                    continue;
                }
                let style = element.style();
                let _ = style.set_property("transition", "none");
                let _ = style.set_property("transform", "none");
                let offset = previous_y - element.get_bounding_client_rect().y();
                if !reduced {
                    let _ = style.set_property("transform", &format!("translateY({offset}px)"));
                    // Commit the inverse position before enabling the transition.
                    let _ = element.get_bounding_client_rect();
                }
                let _ = style.remove_property("transition");
                let _ = style.remove_property("transform");
            }
        });
    }
}

pub(crate) fn natural_content_height(
    container: &web_sys::Element,
    content: &web_sys::Element,
    children: &[&web_sys::Element],
) -> Option<f64> {
    let window = web_sys::window()?;
    let content_style = window.get_computed_style(content).ok()??;
    let container_style = window.get_computed_style(container).ok()??;
    let padding_bottom = css_pixels(&content_style, "padding-bottom")?;
    let border_bottom = css_pixels(&container_style, "border-bottom-width")?;
    let container_rect = container.get_bounding_client_rect();
    let content_rect = content.get_bounding_client_rect();
    let content_bottom = children
        .iter()
        .map(|child| {
            let rect = child.get_bounding_client_rect();
            let is_tool_form = child.get_attribute("class").is_some_and(|classes| {
                classes.split_whitespace().any(|class| class == "tool-form")
            });
            if is_tool_form {
                natural_grid_height(&window, child)
                    .map_or_else(|| rect.bottom(), |height| rect.top() + height)
            } else {
                child
                    .last_element_child()
                    .and_then(|last| {
                        let style = window.get_computed_style(child).ok()??;
                        Some(
                            last.get_bounding_client_rect().bottom()
                                + css_pixels(&style, "padding-bottom")?
                                + css_pixels(&style, "border-bottom-width")?,
                        )
                    })
                    .unwrap_or_else(|| rect.bottom())
            }
        })
        .reduce(f64::max)?;
    Some(
        content_rect.top() - container_rect.top() + content_bottom - content_rect.top()
            + padding_bottom
            + border_bottom,
    )
}

fn natural_grid_height(window: &web_sys::Window, element: &web_sys::Element) -> Option<f64> {
    let style = window.get_computed_style(element).ok()??;
    let row_gap = css_pixels(&style, "row-gap")?;
    let mut height = css_pixels(&style, "padding-top")?
        + css_pixels(&style, "padding-bottom")?
        + css_pixels(&style, "border-top-width")?
        + css_pixels(&style, "border-bottom-width")?;
    let mut child = element.first_element_child();
    let mut has_child = false;
    while let Some(current) = child {
        if has_child {
            height += row_gap;
        }
        let current_style = window.get_computed_style(&current).ok()??;
        height += css_pixels(&current_style, "margin-top")?;
        let is_fields = current.get_attribute("class").is_some_and(|classes| {
            classes
                .split_whitespace()
                .any(|class| class == "tool-form-fields")
        });
        height += if is_fields {
            natural_grid_height(window, &current)?
        } else {
            current.get_bounding_client_rect().height()
        };
        height += css_pixels(&current_style, "margin-bottom")?;
        has_child = true;
        child = current.next_element_sibling();
    }
    Some(height)
}

fn css_pixels(style: &web_sys::CssStyleDeclaration, property: &str) -> Option<f64> {
    style
        .get_property_value(property)
        .ok()?
        .strip_suffix("px")?
        .parse()
        .ok()
}

/// User-visible progress for the upload, processing, and download phases.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct JobProgress {
    pub(crate) percent: u8,
    pub(crate) stage: String,
    pub(crate) detail: String,
}

pub(crate) type ProgressSink = Rc<dyn Fn(JobProgress)>;

/// Browser cancellation shared across all phases of a job.
#[derive(Clone)]
pub(crate) struct BrowserCancellation(Rc<CancellationInner>);

struct CancellationInner {
    controller: AbortController,
    xhr: RefCell<Option<XmlHttpRequest>>,
    job_id: RefCell<Option<String>>,
    cancelled: Cell<bool>,
}

impl BrowserCancellation {
    pub(crate) fn new() -> Result<Self, String> {
        let controller = AbortController::new().map_err(js_error)?;
        Ok(Self(Rc::new(CancellationInner {
            controller,
            xhr: RefCell::new(None),
            job_id: RefCell::new(None),
            cancelled: Cell::new(false),
        })))
    }

    pub(crate) fn abort(&self) {
        self.0.cancelled.set(true);
        self.0.controller.abort();
        if let Some(xhr) = self.0.xhr.borrow().as_ref() {
            let _ = xhr.abort();
        }
        if let Some(job_id) = self.take_job_id() {
            spawn_local(async move {
                let _ = cancel_server_job(&job_id).await;
            });
        }
    }

    pub(crate) fn same_job(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn signal(&self) -> web_sys::AbortSignal {
        self.0.controller.signal()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.cancelled.get()
    }

    fn register_xhr(&self, xhr: Option<XmlHttpRequest>) {
        *self.0.xhr.borrow_mut() = xhr;
    }

    fn set_job_id(&self, job_id: Option<String>) {
        *self.0.job_id.borrow_mut() = job_id;
    }

    fn take_job_id(&self) -> Option<String> {
        self.0.job_id.borrow_mut().take()
    }
}

/// Composes user cancellation with a deadline and detaches its listener on every exit path.
pub(crate) struct FetchDeadline {
    controller: AbortController,
    user_signal: Option<web_sys::AbortSignal>,
    user_abort: Option<Closure<dyn FnMut(Event)>>,
    _timeout: Timeout,
    timed_out: Rc<Cell<bool>>,
}

impl FetchDeadline {
    pub(crate) fn new(
        cancellation: Option<&BrowserCancellation>,
        timeout_millis: u32,
    ) -> Result<Self, String> {
        let controller = AbortController::new().map_err(js_error)?;
        let timed_out = Rc::new(Cell::new(false));
        let timeout_controller = controller.clone();
        let timeout_flag = timed_out.clone();
        let timeout = Timeout::new(timeout_millis, move || {
            timeout_flag.set(true);
            timeout_controller.abort();
        });
        let (user_signal, user_abort) = if let Some(cancellation) = cancellation {
            let signal = cancellation.signal();
            if signal.aborted() {
                controller.abort();
            }
            let user_controller = controller.clone();
            let callback = Closure::<dyn FnMut(Event)>::new(move |_| user_controller.abort());
            signal
                .add_event_listener_with_callback("abort", callback.as_ref().unchecked_ref())
                .map_err(js_error)?;
            (Some(signal), Some(callback))
        } else {
            (None, None)
        };
        Ok(Self {
            controller,
            user_signal,
            user_abort,
            _timeout: timeout,
            timed_out,
        })
    }

    pub(crate) fn signal(&self) -> web_sys::AbortSignal {
        self.controller.signal()
    }

    pub(crate) fn request_error(
        &self,
        cancellation: Option<&BrowserCancellation>,
        cancelled: &str,
        timed_out: &str,
        context: HttpRequestContext,
        _error: JsValue,
    ) -> String {
        if self.timed_out.get() {
            timed_out.into()
        } else if cancellation.is_some_and(BrowserCancellation::is_cancelled) {
            cancelled.into()
        } else {
            http_failure_message(
                context,
                HttpFailure {
                    status: 0,
                    status_text: "",
                    body: None,
                },
            )
        }
    }
}

impl Drop for FetchDeadline {
    fn drop(&mut self) {
        if let (Some(signal), Some(callback)) = (&self.user_signal, &self.user_abort) {
            let _ = signal
                .remove_event_listener_with_callback("abort", callback.as_ref().unchecked_ref());
        }
    }
}

enum ProgressTarget {
    Request,
    Upload(XmlHttpRequestUpload),
}

/// Keeps JavaScript callbacks alive only while an XHR is active and always detaches them on exit.
struct XhrHandlers {
    xhr: XmlHttpRequest,
    progress_target: ProgressTarget,
    cancellation: BrowserCancellation,
    _callbacks: XhrCallbackSet,
}

struct XhrCallbackSet {
    onload: Closure<dyn FnMut(Event)>,
    onerror: Closure<dyn FnMut(Event)>,
    onabort: Closure<dyn FnMut(Event)>,
    ontimeout: Closure<dyn FnMut(Event)>,
    onprogress: Closure<dyn FnMut(ProgressEvent)>,
}

impl XhrHandlers {
    fn attach(
        xhr: &XmlHttpRequest,
        progress_target: ProgressTarget,
        cancellation: &BrowserCancellation,
        callbacks: XhrCallbackSet,
    ) -> Self {
        xhr.set_onload(Some(callbacks.onload.as_ref().unchecked_ref()));
        xhr.set_onerror(Some(callbacks.onerror.as_ref().unchecked_ref()));
        xhr.set_onabort(Some(callbacks.onabort.as_ref().unchecked_ref()));
        xhr.set_ontimeout(Some(callbacks.ontimeout.as_ref().unchecked_ref()));
        match &progress_target {
            ProgressTarget::Request => {
                xhr.set_onprogress(Some(callbacks.onprogress.as_ref().unchecked_ref()));
            }
            ProgressTarget::Upload(upload) => {
                upload.set_onprogress(Some(callbacks.onprogress.as_ref().unchecked_ref()));
            }
        }
        cancellation.register_xhr(Some(xhr.clone()));
        Self {
            xhr: xhr.clone(),
            progress_target,
            cancellation: cancellation.clone(),
            _callbacks: callbacks,
        }
    }
}

impl Drop for XhrHandlers {
    fn drop(&mut self) {
        self.xhr.set_onload(None);
        self.xhr.set_onerror(None);
        self.xhr.set_onabort(None);
        self.xhr.set_ontimeout(None);
        match &self.progress_target {
            ProgressTarget::Request => self.xhr.set_onprogress(None),
            ProgressTarget::Upload(upload) => upload.set_onprogress(None),
        }
        self.cancellation.register_xhr(None);
    }
}

pub(crate) struct JobDownload {
    pub(crate) blob: Blob,
    pub(crate) filename: String,
}

pub(crate) fn save_job_download(download: JobDownload) -> Result<String, String> {
    save_blob(download.blob, &download.filename)?;
    Ok(download.filename)
}

/// Runs a background job at an upload endpoint and returns its downloaded result.
pub(crate) async fn execute_job_download(
    form: FormData,
    upload_url: &str,
    progress: ProgressSink,
    cancellation: BrowserCancellation,
) -> Result<JobDownload, String> {
    let job_id = upload_job(form, upload_url, progress.clone(), &cancellation).await?;
    cancellation.set_job_id(Some(job_id.clone()));
    if cancellation.is_cancelled() {
        cancel_tracked_job(&job_id, &cancellation).await;
        return Err("The operation was cancelled.".into());
    }

    progress(JobProgress {
        percent: 21,
        stage: "Upload complete".into(),
        detail: "Waiting for processing to start".into(),
    });
    let result = finish_job(&job_id, progress, &cancellation).await;
    if result.is_err() {
        cancel_tracked_job(&job_id, &cancellation).await;
    } else {
        cancellation.take_job_id();
    }
    result
}

async fn finish_job(
    job_id: &str,
    progress: ProgressSink,
    cancellation: &BrowserCancellation,
) -> Result<JobDownload, String> {
    let final_status = poll_job(job_id, progress.clone(), cancellation).await?;
    progress(JobProgress {
        percent: 95,
        stage: "Downloading result".into(),
        detail: final_status.filename.clone().unwrap_or_default(),
    });
    let (blob, disposition_name) = download_job(job_id, progress.clone(), cancellation).await?;
    let filename = disposition_name
        .or(final_status.filename)
        .unwrap_or_else(|| "download".into());
    Ok(JobDownload { blob, filename })
}

async fn cancel_tracked_job(job_id: &str, cancellation: &BrowserCancellation) {
    if cancellation.take_job_id().is_some() {
        let _ = cancel_server_job(job_id).await;
    }
}

async fn upload_job(
    form: FormData,
    upload_url: &str,
    progress: ProgressSink,
    cancellation: &BrowserCancellation,
) -> Result<String, String> {
    let xhr = XmlHttpRequest::new().map_err(js_error)?;
    xhr.open_with_async("POST", upload_url, true)
        .map_err(js_error)?;
    xhr.set_timeout(10 * 60 * 1_000);
    let upload = xhr.upload().map_err(js_error)?;
    let (sender, receiver) = oneshot::channel::<Result<String, String>>();
    let sender = Rc::new(RefCell::new(Some(sender)));

    let load_sender = sender.clone();
    let load_xhr = xhr.clone();
    let onload = Closure::<dyn FnMut(Event)>::new(move |_| {
        let result = if (200..300).contains(&load_xhr.status().unwrap_or(0)) {
            load_xhr
                .response_text()
                .ok()
                .flatten()
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_owned())
                .ok_or_else(|| "The server did not return a job identifier.".into())
        } else {
            let status = load_xhr.status().unwrap_or(0);
            let status_text = load_xhr.status_text().unwrap_or_default();
            let body = load_xhr.response_text().ok().flatten();
            Err(http_failure_message(
                HttpRequestContext::Upload,
                HttpFailure {
                    status,
                    status_text: &status_text,
                    body: body.as_deref(),
                },
            ))
        };
        if let Some(sender) = load_sender.borrow_mut().take() {
            let _ = sender.send(result);
        }
    });

    let error_sender = sender.clone();
    let onerror = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(sender) = error_sender.borrow_mut().take() {
            let _ = sender.send(Err(http_failure_message(
                HttpRequestContext::Upload,
                HttpFailure {
                    status: 0,
                    status_text: "",
                    body: None,
                },
            )));
        }
    });
    let abort_sender = sender.clone();
    let onabort = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(sender) = abort_sender.borrow_mut().take() {
            let _ = sender.send(Err("The operation was cancelled.".into()));
        }
    });
    let timeout_sender = sender.clone();
    let ontimeout = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(sender) = timeout_sender.borrow_mut().take() {
            let _ = sender.send(Err("The upload took too long.".into()));
        }
    });
    let onprogress = Closure::<dyn FnMut(ProgressEvent)>::new(move |event: ProgressEvent| {
        let (percent, detail) = if event.length_computable() && event.total() > 0.0 {
            let upload_percent = ((event.loaded() / event.total()) * 18.0).round() as u8;
            (
                2_u8.saturating_add(upload_percent).min(20),
                format!(
                    "{} of {}",
                    format_bytes(event.loaded()),
                    format_bytes(event.total())
                ),
            )
        } else {
            (8, String::from("Sending source files"))
        };
        progress(JobProgress {
            percent,
            stage: "Uploading".into(),
            detail,
        });
    });

    let _handlers = XhrHandlers::attach(
        &xhr,
        ProgressTarget::Upload(upload),
        cancellation,
        XhrCallbackSet {
            onload,
            onerror,
            onabort,
            ontimeout,
            onprogress,
        },
    );
    xhr.send_with_opt_form_data(Some(&form)).map_err(js_error)?;
    let result = receiver
        .await
        .map_err(|_| "The upload ended unexpectedly.".to_owned())?;
    result
}

async fn poll_job(
    job_id: &str,
    progress: ProgressSink,
    cancellation: &BrowserCancellation,
) -> Result<JobStatus, String> {
    let mut poll_count = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err("The operation was cancelled.".into());
        }
        let status = fetch_status(job_id, cancellation).await?;
        let reported = status
            .percent
            .unwrap_or(21)
            .min(if status.phase == JobPhase::Done {
                95
            } else {
                94
            });
        let stage = status.stage.clone().unwrap_or_else(|| "Processing".into());
        progress(JobProgress {
            percent: reported.max(21),
            detail: job_progress_detail(&stage, status.filename.as_deref()),
            stage,
        });
        match status.phase {
            JobPhase::Done => return Ok(status),
            JobPhase::Error => {
                return Err(status
                    .error
                    .or(status.stage)
                    .unwrap_or_else(|| "Job failed.".into()));
            }
            JobPhase::Queued | JobPhase::Running => {}
        }
        poll_count += 1;
        TimeoutFuture::new(job_poll_delay(100, poll_count)).await;
    }
}

async fn fetch_status(
    job_id: &str,
    cancellation: &BrowserCancellation,
) -> Result<JobStatus, String> {
    let deadline = FetchDeadline::new(Some(cancellation), 30_000)?;
    let init = RequestInit::new();
    init.set_method("GET");
    init.set_cache(RequestCache::NoStore);
    init.set_signal(Some(&deadline.signal()));
    let request =
        Request::new_with_str_and_init(&format!("/jobs/{job_id}"), &init).map_err(js_error)?;
    let response = JsFuture::from(window()?.fetch_with_request(&request))
        .await
        .map_err(|error| {
            deadline.request_error(
                Some(cancellation),
                "The operation was cancelled.",
                "Reading job status took longer than 30 seconds. Retry the job.",
                HttpRequestContext::JobStatus,
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
            HttpRequestContext::JobStatus,
        ));
    }
    parse_job_status(&text).map_err(str::to_owned)
}

async fn download_job(
    job_id: &str,
    progress: ProgressSink,
    cancellation: &BrowserCancellation,
) -> Result<(Blob, Option<String>), String> {
    let xhr = XmlHttpRequest::new().map_err(js_error)?;
    xhr.open_with_async("GET", &format!("/jobs/{job_id}/download"), true)
        .map_err(js_error)?;
    xhr.set_response_type(XmlHttpRequestResponseType::Blob);
    xhr.set_timeout(10 * 60 * 1_000);
    let (sender, receiver) = oneshot::channel::<Result<(Blob, Option<String>), String>>();
    let sender = Rc::new(RefCell::new(Some(sender)));

    let load_sender = sender.clone();
    let load_xhr = xhr.clone();
    let onload = Closure::<dyn FnMut(Event)>::new(move |_| {
        let result = if (200..300).contains(&load_xhr.status().unwrap_or(0)) {
            load_xhr
                .response()
                .map_err(js_error)
                .and_then(|value| value.dyn_into::<Blob>().map_err(js_error))
                .map(|blob| {
                    let filename = load_xhr
                        .get_response_header("content-disposition")
                        .ok()
                        .flatten()
                        .and_then(|value| filename_from_disposition(&value));
                    (blob, filename)
                })
        } else {
            let status = load_xhr.status().unwrap_or(0);
            let status_text = load_xhr.status_text().unwrap_or_default();
            let body = load_xhr
                .response()
                .ok()
                .and_then(|value| value.dyn_into::<Blob>().ok());
            let sender = load_sender.clone();
            spawn_local(async move {
                let text = match body {
                    Some(blob) => blob_text(&blob).await.ok(),
                    None => None,
                };
                if let Some(sender) = sender.borrow_mut().take() {
                    let _ = sender.send(Err(http_failure_message(
                        HttpRequestContext::Download,
                        HttpFailure {
                            status,
                            status_text: &status_text,
                            body: text.as_deref(),
                        },
                    )));
                }
            });
            return;
        };
        if let Some(sender) = load_sender.borrow_mut().take() {
            let _ = sender.send(result);
        }
    });
    let error_sender = sender.clone();
    let onerror = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(sender) = error_sender.borrow_mut().take() {
            let _ = sender.send(Err(http_failure_message(
                HttpRequestContext::Download,
                HttpFailure {
                    status: 0,
                    status_text: "",
                    body: None,
                },
            )));
        }
    });
    let abort_sender = sender.clone();
    let onabort = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(sender) = abort_sender.borrow_mut().take() {
            let _ = sender.send(Err("The operation was cancelled.".into()));
        }
    });
    let timeout_sender = sender.clone();
    let ontimeout = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(sender) = timeout_sender.borrow_mut().take() {
            let _ = sender.send(Err("The download took too long.".into()));
        }
    });
    let onprogress = Closure::<dyn FnMut(ProgressEvent)>::new(move |event: ProgressEvent| {
        let percent = if event.length_computable() && event.total() > 0.0 {
            95 + (((event.loaded() / event.total()) * 4.0).round() as u8).min(4)
        } else {
            95
        };
        progress(JobProgress {
            percent,
            stage: "Downloading result".into(),
            detail: if event.length_computable() {
                format!(
                    "{} of {}",
                    format_bytes(event.loaded()),
                    format_bytes(event.total())
                )
            } else {
                format_bytes(event.loaded())
            },
        });
    });
    let _handlers = XhrHandlers::attach(
        &xhr,
        ProgressTarget::Request,
        cancellation,
        XhrCallbackSet {
            onload,
            onerror,
            onabort,
            ontimeout,
            onprogress,
        },
    );
    xhr.send().map_err(js_error)?;
    let result = receiver
        .await
        .map_err(|_| "The download ended unexpectedly.".to_owned())?;
    result
}

async fn cancel_server_job(job_id: &str) -> Result<(), String> {
    let deadline = FetchDeadline::new(None, 30_000)?;
    let init = RequestInit::new();
    init.set_method("DELETE");
    init.set_signal(Some(&deadline.signal()));
    let request =
        Request::new_with_str_and_init(&format!("/jobs/{job_id}"), &init).map_err(js_error)?;
    let response = JsFuture::from(window()?.fetch_with_request(&request))
        .await
        .map_err(|error| {
            deadline.request_error(
                None,
                "The operation was cancelled.",
                "Cancelling the server job took longer than 30 seconds.",
                HttpRequestContext::CancelJob,
                error,
            )
        })?
        .dyn_into::<Response>()
        .map_err(js_error)?;
    if response.ok() || response.status() == 400 {
        Ok(())
    } else {
        Err(response_failure(&response, HttpRequestContext::CancelJob).await?)
    }
}

pub(crate) async fn response_text(response: &Response) -> Result<String, String> {
    let value = JsFuture::from(response.text().map_err(js_error)?)
        .await
        .map_err(js_error)?;
    value
        .as_string()
        .ok_or_else(|| "The server returned invalid text.".into())
}

pub(crate) async fn response_failure(
    response: &Response,
    context: HttpRequestContext,
) -> Result<String, String> {
    // Preserve the response status even when a proxy or browser does not expose
    // a decodable error body. Treating that case as a generic JavaScript error
    // hides the distinction between an HTTP rejection and a transport outage.
    let text = response_text(response).await.unwrap_or_default();
    Ok(http_response_failure(response, &text, context))
}

pub(crate) fn http_response_failure(
    response: &Response,
    body: &str,
    context: HttpRequestContext,
) -> String {
    let status_text = response.status_text();
    http_failure_message(
        context,
        HttpFailure {
            status: response.status(),
            status_text: &status_text,
            body: Some(body),
        },
    )
}

async fn blob_text(blob: &Blob) -> Result<String, String> {
    JsFuture::from(blob.text())
        .await
        .map_err(js_error)?
        .as_string()
        .ok_or_else(|| "The server returned invalid text.".into())
}

fn save_blob(blob: Blob, filename: &str) -> Result<(), String> {
    let url = Url::create_object_url_with_blob(&blob).map_err(js_error)?;
    let document = document()?;
    let link = document
        .create_element("a")
        .map_err(js_error)?
        .dyn_into::<HtmlAnchorElement>()
        .map_err(|_| "Could not create the download link.".to_owned())?;
    link.set_href(&url);
    link.set_download(filename);
    link.style()
        .set_property("display", "none")
        .map_err(js_error)?;
    let body = document
        .body()
        .ok_or_else(|| "The page has no document body.".to_owned())?;
    body.append_child(&link).map_err(js_error)?;
    link.click();
    link.remove();
    let revoke_url = url.clone();
    spawn_local(async move {
        TimeoutFuture::new(1_000).await;
        let _ = Url::revoke_object_url(&revoke_url);
    });
    Ok(())
}

/// Applies and persists a user-selected theme.
pub(crate) fn apply_theme(theme: &str, persist: bool) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    if let Some(root) = document.document_element() {
        let _ = root.set_attribute("data-theme", theme);
    }
    if persist {
        if let Ok(Some(storage)) =
            window().and_then(|window| window.local_storage().map_err(js_error))
        {
            let _ = storage.set_item("pdf-tools-theme", theme);
        }
    }
}

/// Resolves stored theme, then the system preference.
pub(crate) fn initial_theme() -> String {
    let stored = stored_theme();
    let prefers_dark = window()
        .and_then(|window| {
            window
                .match_media("(prefers-color-scheme: dark)")
                .map_err(js_error)
        })
        .ok()
        .flatten()
        .is_some_and(|media| media.matches());
    resolved_theme(stored.as_deref(), prefers_dark).into()
}

fn resolved_theme(stored: Option<&str>, prefers_dark: bool) -> &'static str {
    match stored {
        Some("light") => "light",
        Some("dark") => "dark",
        _ if prefers_dark => "dark",
        _ => "light",
    }
}

fn follows_system_theme(stored: Option<&str>) -> bool {
    !matches!(stored, Some("light" | "dark"))
}

pub(crate) struct SystemThemeListener {
    media: web_sys::MediaQueryList,
    callback: Closure<dyn FnMut(Event)>,
}

pub(crate) struct MediaQueryListener {
    media: web_sys::MediaQueryList,
    callback: Closure<dyn FnMut(Event)>,
}

impl Drop for MediaQueryListener {
    fn drop(&mut self) {
        let _ = self
            .media
            .remove_event_listener_with_callback("change", self.callback.as_ref().unchecked_ref());
    }
}

pub(crate) fn media_query_listener(
    query: &str,
    on_change: impl Fn(bool) + 'static,
) -> Result<MediaQueryListener, String> {
    let media = window()?
        .match_media(query)
        .map_err(js_error)?
        .ok_or_else(|| "The browser does not expose responsive media queries.".to_owned())?;
    on_change(media.matches());
    let callback_media = media.clone();
    let callback = Closure::<dyn FnMut(Event)>::new(move |_| on_change(callback_media.matches()));
    media
        .add_event_listener_with_callback("change", callback.as_ref().unchecked_ref())
        .map_err(js_error)?;
    Ok(MediaQueryListener { media, callback })
}

impl Drop for SystemThemeListener {
    fn drop(&mut self) {
        let _ = self
            .media
            .remove_event_listener_with_callback("change", self.callback.as_ref().unchecked_ref());
    }
}

pub(crate) fn system_theme_listener(
    callback: impl Fn(&'static str) + 'static,
) -> Result<Option<SystemThemeListener>, String> {
    if !follows_system_theme(stored_theme().as_deref()) {
        return Ok(None);
    }
    let media = window()?
        .match_media("(prefers-color-scheme: dark)")
        .map_err(js_error)?
        .ok_or_else(|| "The browser does not expose system theme changes.".to_owned())?;
    let callback_media = media.clone();
    let callback = Closure::<dyn FnMut(Event)>::new(move |_| {
        callback(if callback_media.matches() {
            "dark"
        } else {
            "light"
        });
    });
    media
        .add_event_listener_with_callback("change", callback.as_ref().unchecked_ref())
        .map_err(js_error)?;
    Ok(Some(SystemThemeListener { media, callback }))
}

fn stored_theme() -> Option<String> {
    window()
        .and_then(|window| window.local_storage().map_err(js_error))
        .ok()
        .flatten()
        .and_then(|storage| storage.get_item("pdf-tools-theme").ok().flatten())
}

fn filename_from_disposition(value: &str) -> Option<String> {
    value
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("filename=\"")?.strip_suffix('"'))
        .map(str::to_owned)
}

fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 * 1024.0 {
        format!("{} KB", (bytes / 1024.0).round().max(1.0))
    } else {
        format!("{:.2} MB", bytes / 1024.0 / 1024.0)
    }
}

fn window() -> Result<Window, String> {
    web_sys::window().ok_or_else(|| "The browser window is unavailable.".into())
}

fn document() -> Result<Document, String> {
    window()?
        .document()
        .ok_or_else(|| "The browser document is unavailable.".into())
}

pub(crate) fn focus_element(id: &str) {
    let Some(element) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return;
    };
    let _ = element.focus();
}

/// Restores focus after a reactive list has committed its keyed DOM move.
pub(crate) fn focus_element_after_render(id: String) {
    Timeout::new(0, move || focus_element(&id)).forget();
}

/// Opens a conditionally mounted native dialog and reports mount/API failures to its owner.
pub(crate) fn show_modal(
    dialog: NodeRef<html::Dialog>,
    initial_focus_id: String,
    on_result: impl FnOnce(Result<(), String>) + 'static,
) {
    spawn_local(async move {
        TimeoutFuture::new(0).await;
        let result = dialog
            .get()
            .ok_or_else(|| "The dialog could not be mounted.".to_owned())
            .and_then(|dialog: HtmlDialogElement| dialog.show_modal().map_err(js_error));
        if result.is_ok() {
            focus_element(&initial_focus_id);
        }
        on_result(result);
    });
}

pub(crate) fn restore_modal_focus(trigger_id: String) {
    spawn_local(async move {
        TimeoutFuture::new(0).await;
        focus_element(&trigger_id);
    });
}

fn js_error(value: JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            value
                .dyn_ref::<js_sys::Error>()
                .map(|error| error.message().into())
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "A browser operation failed.".into())
}
