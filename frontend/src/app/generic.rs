use std::rc::Rc;

use leptos::{ev, html, prelude::*};
use wasm_bindgen_futures::spawn_local;

use crate::{
    browser::{self, BrowserCancellation, JobProgress, ProgressSink},
    files::SelectedFile,
    presentation::aria_bool,
    workspace::{
        build_job_form, execute_job, extract_chunk_size, operation_uses_file_order,
        ExtractOutputMode, FileQueue, GenericJobOperation, GenericJobSettings, ImageTarget,
        Operation, PageSelection, RasterQuality,
    },
};

#[derive(Clone, Copy)]
pub(super) struct GenericWorkflow {
    pub(super) files: RwSignal<Vec<SelectedFile>>,
    pub(super) operation: RwSignal<Operation>,
    pub(super) target: RwSignal<ImageTarget>,
    pub(super) quality: RwSignal<RasterQuality>,
    pub(super) extract_pages: RwSignal<PageSelection>,
    pub(super) export_pages: RwSignal<PageSelection>,
    pub(super) extract_output_mode: RwSignal<ExtractOutputMode>,
    pub(super) extract_chunk_size_draft: RwSignal<String>,
    pub(super) selected_pdf_page_count: Signal<Option<usize>>,
    pub(super) busy: RwSignal<bool>,
    pub(super) message: RwSignal<String>,
    pub(super) progress: RwSignal<JobProgress>,
    pub(super) cancellation: StoredValue<Option<BrowserCancellation>, LocalStorage>,
    pub(super) workspace_grid: NodeRef<html::Div>,
    pub(super) clear_workspace_callback: UnsyncCallback<()>,
    pub(super) remove_callback: UnsyncCallback<u64>,
    pub(super) reorder_callback: UnsyncCallback<(u64, u64)>,
}

pub(super) fn render(state: GenericWorkflow) -> AnyView {
    let GenericWorkflow {
        files,
        operation,
        target,
        quality,
        extract_pages,
        export_pages,
        extract_output_mode,
        extract_chunk_size_draft,
        selected_pdf_page_count,
        busy,
        message,
        progress,
        cancellation,
        workspace_grid,
        clear_workspace_callback,
        remove_callback,
        reorder_callback,
    } = state;
    let cancellation_for_submit = cancellation;
    let submit = move |event: ev::SubmitEvent| {
        event.prevent_default();
        if busy.get_untracked() {
            return;
        }
        let Some(generic_operation) =
            GenericJobOperation::from_operation(operation.get_untracked())
        else {
            return;
        };
        message.set(String::new());
        progress.set(JobProgress {
            percent: 0,
            stage: "Preparing".into(),
            detail: String::new(),
        });
        let settings = GenericJobSettings {
            target: target.get_untracked(),
            quality: quality.get_untracked(),
            extract_pages: extract_pages.get_untracked(),
            export_pages: export_pages.get_untracked(),
            extract_output_mode: extract_output_mode.get_untracked(),
            extract_chunk_size_draft: extract_chunk_size_draft.get_untracked(),
        };
        let form = build_job_form(generic_operation, &files.get_untracked(), &settings);
        let form = match form {
            Ok(form) => form,
            Err(error) => {
                message.set(error);
                return;
            }
        };
        let controller = match BrowserCancellation::new() {
            Ok(controller) => controller,
            Err(error) => {
                message.set(error);
                return;
            }
        };
        cancellation_for_submit.update_value(|active| *active = Some(controller.clone()));
        busy.set(true);
        let active_cancellation = cancellation_for_submit;
        let progress_controller = controller.clone();
        let progress_sink: ProgressSink = Rc::new(move |next| {
            let is_active = active_cancellation
                .try_with_value(|active| {
                    active
                        .as_ref()
                        .is_some_and(|active| active.same_job(&progress_controller))
                })
                .unwrap_or(false);
            if is_active {
                progress.set(next);
            }
        });
        spawn_local(async move {
            let result = execute_job(form, progress_sink, controller.clone()).await;
            let is_active = active_cancellation
                .try_with_value(|active| {
                    active
                        .as_ref()
                        .is_some_and(|active| active.same_job(&controller))
                })
                .unwrap_or(false);
            if is_active {
                if let Err(error) = result {
                    message.set(error);
                }
                active_cancellation.update_value(|active| *active = None);
                busy.set(false);
            }
        });
    };

    let cancellation_for_button = cancellation;
    let cancel = move |_| {
        cancellation_for_button.with_value(|active| {
            if let Some(controller) = active {
                controller.abort();
                message.set("Cancelling the current job…".into());
            }
        });
    };

    let submit_callback = UnsyncCallback::new(submit);
    let cancel_callback = UnsyncCallback::new(move |_| cancel(()));
    view! {
        <div
            node_ref=workspace_grid
            class="workspace-grid"
            class:ordered-files=move || operation_uses_file_order(
                operation.get(),
                files.read().len(),
            )
        >
            <FileQueue
                files
                operation
                busy
                on_clear=clear_workspace_callback
                on_remove=remove_callback
                on_reorder=reorder_callback
            />
            <For
                each=move || [operation.get()]
                key=|selected| selected.id()
                children=move |_| view! {
                    <ToolForm
                        operation
                        target
                        quality
                        extract_pages
                        export_pages
                        extract_output_mode
                        extract_chunk_size_draft
                        page_count=selected_pdf_page_count
                        busy
                        message
                        progress
                        on_submit=submit_callback
                        on_cancel=cancel_callback
                    />
                }
            />
        </div>
    }
    .into_any()
}

#[component]
fn ToolForm(
    operation: RwSignal<Operation>,
    target: RwSignal<ImageTarget>,
    quality: RwSignal<RasterQuality>,
    extract_pages: RwSignal<PageSelection>,
    export_pages: RwSignal<PageSelection>,
    extract_output_mode: RwSignal<ExtractOutputMode>,
    extract_chunk_size_draft: RwSignal<String>,
    page_count: Signal<Option<usize>>,
    busy: RwSignal<bool>,
    message: RwSignal<String>,
    progress: RwSignal<JobProgress>,
    on_submit: UnsyncCallback<ev::SubmitEvent>,
    on_cancel: UnsyncCallback<()>,
) -> impl IntoView {
    let selection_issue = move || match operation.get() {
        Operation::Split => extract_pages
            .read()
            .validate_for_page_count(page_count.get())
            .err()
            .or_else(|| {
                (extract_output_mode.get() == ExtractOutputMode::Chunks)
                    .then(|| extract_chunk_size(&extract_chunk_size_draft.get()).err())
                    .flatten()
                    .map(str::to_owned)
            }),
        Operation::PdfImage => export_pages
            .read()
            .validate_for_page_count(page_count.get())
            .err(),
        Operation::ImagePdf | Operation::Merge | Operation::Impose => None,
    };
    view! {
        <form
            class="tool-form"
            aria-labelledby="operation-settings-heading"
            aria-describedby="operation-description"
            aria-busy=move || aria_bool(busy.get())
            on:submit=move |event| on_submit.run(event)
        >
            <div class="tool-form-head">
                <h2 id="operation-settings-heading">{move || operation.get().label()}</h2>
                <p id="operation-description" class="tool-description">{move || operation.get().description()}</p>
            </div>
            <div class="tool-form-fields">
                <Show when=move || operation.get() == Operation::Split>
                    <PageSelectionControl id="extract-pages" value=extract_pages page_count />
                    <div class="field">
                        <span>"Output"</span>
                        <div class="segmented-control" role="group" aria-label="Extracted page output">
                            <button type="button" aria-pressed=move || aria_bool(extract_output_mode.get() == ExtractOutputMode::Individual) on:click=move |_| extract_output_mode.set(ExtractOutputMode::Individual)>"Individual"</button>
                            <button type="button" aria-pressed=move || aria_bool(extract_output_mode.get() == ExtractOutputMode::Combined) on:click=move |_| extract_output_mode.set(ExtractOutputMode::Combined)>"Combined"</button>
                            <button type="button" aria-pressed=move || aria_bool(extract_output_mode.get() == ExtractOutputMode::Chunks) on:click=move |_| extract_output_mode.set(ExtractOutputMode::Chunks)>"Chunks"</button>
                        </div>
                        <small class="field-help">{move || match extract_output_mode.get() {
                            ExtractOutputMode::Individual => "One PDF per selected page, packaged in a ZIP.",
                            ExtractOutputMode::Combined => "All selected pages in one PDF, in source-document order.",
                            ExtractOutputMode::Chunks => "Consecutive groups of selected pages, packaged in a ZIP.",
                        }}</small>
                    </div>
                    <Show when=move || extract_output_mode.get() == ExtractOutputMode::Chunks>
                        <label class="field" for="extract-chunk-size">
                            <span>"Pages per chunk"</span>
                            <input
                                id="extract-chunk-size"
                                type="number"
                                min="1"
                                max="1000"
                                step="1"
                                prop:value=move || extract_chunk_size_draft.get()
                                aria-invalid=move || aria_bool(extract_chunk_size(&extract_chunk_size_draft.get()).is_err())
                                aria-describedby="extract-chunk-size-help"
                                on:input=move |event| extract_chunk_size_draft.set(event_target_value(&event))
                            />
                            <small id="extract-chunk-size-help" class=move || if extract_chunk_size(&extract_chunk_size_draft.get()).is_err() { "field-error" } else { "field-help" }>
                                {move || extract_chunk_size(&extract_chunk_size_draft.get()).err().unwrap_or("Each file contains this many selected pages; the last may contain fewer.")}
                            </small>
                        </label>
                    </Show>
                </Show>
                <Show when=move || operation.get() == Operation::PdfImage>
                    <PageSelectionControl id="export-pages" value=export_pages page_count />
                    <div class="field">
                        <span>"Image format"</span>
                        <div class="segmented-control" role="group" aria-label="Image format">
                            <button type="button" aria-pressed=move || aria_bool(target.get() == ImageTarget::Png) on:click=move |_| target.set(ImageTarget::Png)>"PNG"</button>
                            <button type="button" aria-pressed=move || aria_bool(target.get() == ImageTarget::Jpeg) on:click=move |_| target.set(ImageTarget::Jpeg)>"JPEG"</button>
                        </div>
                    </div>
                    <div class="field">
                        <span>"Quality"</span>
                        <div class="segmented-control" role="group" aria-label="Raster quality">
                            <For each=|| [RasterQuality::Screen, RasterQuality::Print, RasterQuality::High] key=|item| item.id() children=move |item| view! {
                                <button type="button" aria-pressed=move || aria_bool(quality.get() == item) on:click=move |_| quality.set(item)>{item.label()}</button>
                            } />
                        </div>
                        <small class="field-help">{move || quality.get().help()}</small>
                    </div>
                </Show>
                <Show when=move || operation.get() == Operation::ImagePdf>
                    <p class="field-help">"Each page matches its image at 300 DPI. One image per page."</p>
                </Show>
            </div>
            <div class="submit-row">
                <Show when=move || busy.get()><ProgressStatus progress /></Show>
                <Show when=move || !busy.get() && progress.get().percent == 100 && message.read().is_empty()>
                    <p class="export-notice" role="status">{move || format!("{} was sent to your browser.", progress.get().detail)}</p>
                </Show>
                <div class="submit-actions">
                    <button class="primary-button" type="submit" disabled=move || busy.get() || selection_issue().is_some()>
                        {move || if operation.get() == Operation::Split {
                            extract_output_mode.get().submit_label()
                        } else {
                            operation.get().submit_label()
                        }}
                    </button>
                    <Show when=move || busy.get()>
                        <button class="cancel-button" type="button" on:click=move |_| on_cancel.run(())>"Cancel"</button>
                    </Show>
                </div>
                <Show when=move || !message.read().is_empty()>
                    <p class="error" role="alert" aria-live="assertive">{move || message.get()}</p>
                </Show>
            </div>
        </form>
    }
}

#[component]
fn PageSelectionControl(
    id: &'static str,
    value: RwSignal<PageSelection>,
    page_count: Signal<Option<usize>>,
) -> impl IntoView {
    let range_draft = RwSignal::new("1".to_owned());
    let range_editor_open = RwSignal::new(false);
    let range_dialog = NodeRef::<html::Dialog>::new();
    let modal_error = RwSignal::new(Option::<String>::None);
    let range_issue = move || {
        PageSelection::Range(range_draft.get())
            .validate_for_page_count(page_count.get())
            .err()
    };
    let close_editor = move || {
        range_editor_open.set(false);
        browser::restore_modal_focus(format!("{id}-range-trigger"));
    };
    let apply_range = move || {
        if range_issue().is_none() {
            value.set(PageSelection::Range(range_draft.get_untracked()));
            close_editor();
        }
    };
    view! {
        <div class="page-selection">
            <div class="field">
                <span>"Pages"</span>
                <div class="segmented-control" role="group" aria-label="Pages">
                    <button type="button" aria-pressed=move || aria_bool(value.get() == PageSelection::All) on:click=move |_| value.set(PageSelection::All)>"All pages"</button>
                    <button
                        id=format!("{id}-range-trigger")
                        type="button"
                        aria-haspopup="dialog"
                        aria-expanded=move || aria_bool(range_editor_open.get())
                        aria-pressed=move || aria_bool(matches!(value.get(), PageSelection::Range(_)))
                        on:click=move |_| {
                            if let PageSelection::Range(current) = value.get_untracked() {
                                range_draft.set(current);
                            }
                            modal_error.set(None);
                            range_editor_open.set(true);
                            browser::show_modal(
                                range_dialog,
                                format!("{id}-range-input"),
                                move |result| {
                                    if let Err(error) = result {
                                        range_editor_open.set(false);
                                        modal_error.set(Some(error));
                                        browser::restore_modal_focus(format!("{id}-range-trigger"));
                                    }
                                },
                            );
                        }
                    >
                        {move || match value.get() {
                            PageSelection::All => "Range".to_owned(),
                            PageSelection::Range(range) => format!("Range: {range}"),
                        }}
                    </button>
                </div>
            </div>
            <Show when=move || value.read().validate_for_page_count(page_count.get()).is_err()>
                <p class="field-error" role="alert">{move || value.read().validate_for_page_count(page_count.get()).err().unwrap_or_default()}" Choose All pages or update the range."</p>
            </Show>
            <Show when=move || range_editor_open.get()>
                <dialog
                    node_ref=range_dialog
                    id=format!("{id}-range-dialog")
                    class="quantity-dialog range-dialog"
                    aria-labelledby=format!("{id}-range-title")
                    aria-describedby=format!("{id}-range-help")
                    on:cancel=move |event: web_sys::Event| {
                        event.prevent_default();
                        close_editor();
                    }
                >
                    <header class="quantity-dialog-head">
                        <div><p class="eyebrow">"Page selection"</p><h2 id=format!("{id}-range-title")>"Choose a page range"</h2></div>
                        <button id=format!("{id}-range-close") class="quantity-dialog-close" type="button" aria-label="Close page range editor" on:click=move |_| close_editor()><span aria-hidden="true">"×"</span></button>
                    </header>
                    <p id=format!("{id}-range-help")>
                        {move || match page_count.get() {
                            Some(count) => format!("Enter individual pages or inclusive ranges, separated by commas. This source has {count} {}.", if count == 1 { "page" } else { "pages" }),
                            None => "Enter individual pages or inclusive ranges, separated by commas.".into(),
                        }}
                    </p>
                    <label class="field" for=format!("{id}-range-input")>
                        <span>"Pages"</span>
                        <input
                            id=format!("{id}-range-input")
                            autofocus
                            prop:value=move || range_draft.get()
                            placeholder="1-3, 5, 8-10"
                            aria-invalid=move || aria_bool(range_issue().is_some())
                            aria-describedby=move || format!("{id}-range-{}", if range_issue().is_some() { "error" } else { "example" })
                            on:input=move |event| range_draft.set(event_target_value(&event))
                            on:keydown=move |event: web_sys::KeyboardEvent| {
                                if event.key() == "Enter" && range_issue().is_none() {
                                    event.prevent_default();
                                    apply_range();
                                }
                            }
                        />
                        {move || if let Some(issue) = range_issue() {
                            view! { <small class="field-error" id=format!("{id}-range-error") role="alert">{issue}</small> }.into_any()
                        } else {
                            view! { <small id=format!("{id}-range-example")>"Example: 1-3, 5, 8-10"</small> }.into_any()
                        }}
                    </label>
                    <footer class="quantity-dialog-actions range-dialog-actions">
                        <button id=format!("{id}-range-cancel") class="ghost-button" type="button" on:click=move |_| close_editor()>"Cancel"</button>
                        <button id=format!("{id}-range-apply") class="primary-button" type="button" disabled=move || range_issue().is_some() on:click=move |_| apply_range()>"Apply range"</button>
                    </footer>
                </dialog>
            </Show>
            <Show when=move || modal_error.get().is_some()>
                <p class="field-error" role="alert">{move || modal_error.get().unwrap_or_default()}</p>
            </Show>
        </div>
    }
}

#[component]
fn ProgressStatus(progress: RwSignal<JobProgress>) -> impl IntoView {
    view! {
        <div class="progress-status" class:active=move || (1..100).contains(&progress.get().percent) aria-live="polite" aria-atomic="true">
            <div><span>{move || progress.get().stage}</span><strong>{move || format!("{}%", progress.get().percent)}</strong></div>
            <progress max="100" value=move || progress.get().percent></progress>
            <small>{move || progress.get().detail}</small>
        </div>
    }
}
