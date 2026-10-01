//! Leptos component tree for the generic PDF workflows.

use std::{cell::Cell, collections::BTreeMap, rc::Rc};

use leptos::{ev, html, prelude::*};
use wasm_bindgen_futures::spawn_local;
use web_sys::{DragEvent, File};

use crate::{
    browser::{self, BrowserCancellation, JobProgress},
    files::{classify_file, FileDescriptor, FileKind, SelectedFile},
    presentation::aria_bool,
    workspace::{
        move_item_to, normalize_selection, preflight_pdf_files, visible_operations,
        ExtractOutputMode, ImageTarget, Operation, PageSelection, RasterQuality, SelectionIntent,
    },
};

mod generic;
mod lazy;
use lazy::WorkflowLoader;

/// Root PDF Tools client application.
#[component]
pub(crate) fn App() -> impl IntoView {
    let files = RwSignal::new(Vec::<SelectedFile>::new());
    let pdf_page_counts = RwSignal::new(BTreeMap::<u64, usize>::new());
    let operation = RwSignal::new(Operation::PdfImage);
    // Replacing artwork can select the same operation again. Keep its mounted
    // workspace (and explicit print settings) until the workflow kind changes.
    let is_imposition = Memo::new(move |_| operation.get() == Operation::Impose);
    let target = RwSignal::new(ImageTarget::Png);
    let quality = RwSignal::new(RasterQuality::Screen);
    let extract_pages = RwSignal::new(PageSelection::All);
    let export_pages = RwSignal::new(PageSelection::All);
    let extract_output_mode = RwSignal::new(ExtractOutputMode::Individual);
    let extract_chunk_size_draft = RwSignal::new("10".to_owned());
    let busy = RwSignal::new(false);
    let checking_pdfs = RwSignal::new(false);
    let drag_active = RwSignal::new(false);
    let message = RwSignal::new(String::new());
    let progress = RwSignal::new(JobProgress::default());
    let cancellation = StoredValue::new_local(Option::<BrowserCancellation>::None);
    let append_mode = RwSignal::new(false);
    let theme = RwSignal::new(browser::initial_theme());
    let clear_workspace_open = RwSignal::new(false);
    let file_input = NodeRef::<html::Input>::new();
    let clear_workspace_dialog = NodeRef::<html::Dialog>::new();
    let ready_shell = NodeRef::<html::Section>::new();
    let ready_card = NodeRef::<html::Section>::new();
    let workspace_grid = NodeRef::<html::Div>::new();
    let ready_card_height = RwSignal::new(Option::<f64>::None);
    let workspace_resize_listener =
        StoredValue::new_local(Option::<browser::ElementResizeListener>::None);
    let next_file_id = Rc::new(Cell::new(1_u64));

    Effect::new(move |_| {
        // Conditional generic controls do not resize their clipped grid owner until the
        // outer card is remeasured, so make those visibility choices explicit dependencies.
        let _ = extract_output_mode.get();
        let Some(card) = ready_card.get() else {
            ready_card_height.set(None);
            workspace_resize_listener.set_value(None);
            return;
        };
        if operation.get() == Operation::Impose {
            let Some(shell) = ready_shell.get() else {
                return;
            };
            let measure = {
                let shell = shell.clone();
                move || ready_card_height.set(Some(f64::from(shell.offset_height())))
            };
            measure();
            workspace_resize_listener
                .set_value(browser::observe_element_resizes(&[shell.as_ref()], measure).ok());
            return;
        }
        let Some(workspace) = workspace_grid.get() else {
            workspace_resize_listener.set_value(None);
            return;
        };

        let file_queue = workspace.query_selector(".file-queue").ok().flatten();
        let tool_form = workspace.query_selector(".tool-form").ok().flatten();
        let measure = {
            let card = card.clone();
            let workspace = workspace.clone();
            let file_queue = file_queue.clone();
            let tool_form = tool_form.clone();
            Rc::new(move || {
                let children = [file_queue.as_ref(), tool_form.as_ref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                if let Some(height) =
                    browser::natural_content_height(card.as_ref(), workspace.as_ref(), &children)
                {
                    ready_card_height.set(Some(height));
                }
            })
        };
        measure();
        let resize_measure = measure.clone();
        let mut observed = vec![workspace.as_ref()];
        if let Some(file_queue) = file_queue.as_ref() {
            observed.push(file_queue);
        }
        if let Some(tool_form) = tool_form.as_ref() {
            observed.push(tool_form);
        }
        workspace_resize_listener
            .set_value(browser::observe_element_resizes(&observed, move || resize_measure()).ok());
    });

    browser::apply_theme(&theme.get_untracked(), false);
    let system_theme_listener = StoredValue::new_local(
        browser::system_theme_listener(move |next| {
            theme.set(next.into());
            browser::apply_theme(next, false);
        })
        .ok()
        .flatten(),
    );

    let select_files: Rc<dyn Fn(Vec<File>, SelectionIntent, bool)> = {
        let next_file_id = next_file_id.clone();
        Rc::new(move |browser_files, intent, append| {
            if browser_files.is_empty() || busy.get_untracked() {
                return;
            }
            let mut selected = if append {
                files.get_untracked()
            } else {
                Vec::new()
            };
            selected.extend(browser_files.into_iter().map(|file| {
                let id = next_file_id.get();
                next_file_id.set(id.saturating_add(1));
                SelectedFile::new(id, file)
            }));
            let descriptors = descriptors(&selected);
            let normalized =
                match normalize_selection(&descriptors, operation.get_untracked(), intent) {
                    Ok(normalized) => normalized,
                    Err(error) => {
                        message.set(error.to_string());
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
            cancellation.set_value(Some(controller.clone()));
            checking_pdfs.set(true);
            message.set(String::new());
            progress.set(JobProgress {
                percent: 0,
                stage: "Checking PDFs".into(),
                detail: "Confirming that each PDF can be read".into(),
            });
            busy.set(true);
            spawn_local(async move {
                let result = preflight_pdf_files(&selected, &controller).await;
                let is_active = cancellation
                    .try_with_value(|active| {
                        active
                            .as_ref()
                            .is_some_and(|active| active.same_job(&controller))
                    })
                    .unwrap_or(false);
                if !is_active {
                    return;
                }
                // Cancellation can race a completed response. Never commit
                // the replacement after the user has cancelled inspection.
                let result = if controller.is_cancelled() {
                    Err("PDF inspection was cancelled.".into())
                } else {
                    result
                };
                match result {
                    Ok(counts) => {
                        pdf_page_counts.set(counts.into_iter().collect());
                        files.set(selected);
                        operation.set(normalized.operation);
                        message.set(String::new());
                    }
                    Err(error) => message.set(error),
                }
                progress.set(JobProgress::default());
                cancellation.set_value(None);
                checking_pdfs.set(false);
                busy.set(false);
            });
        })
    };

    let cancel_inspection = UnsyncCallback::new(move |_| {
        cancellation.with_value(|active| {
            if let Some(controller) = active {
                controller.abort();
            }
        });
    });

    let browse = move |append: bool| {
        if busy.get_untracked() {
            return;
        }
        append_mode.set(append);
        if let Some(input) = file_input.get() {
            input.set_value("");
            input.click();
        }
    };

    let on_files = {
        let select_files = select_files.clone();
        move |event: ev::Event| {
            let input = event_target::<web_sys::HtmlInputElement>(&event);
            let selected = input.files().map(files_from_list).unwrap_or_default();
            let append = append_mode.get_untracked();
            let intent = if append || !files.read_untracked().is_empty() {
                SelectionIntent::ContinueWorkflow
            } else {
                SelectionIntent::NewUpload
            };
            append_mode.set(false);
            select_files(selected, intent, append);
        }
    };

    let reset_workspace: Rc<dyn Fn()> = Rc::new(move || {
        if !busy.get_untracked() {
            files.set(Vec::new());
            pdf_page_counts.set(BTreeMap::new());
            operation.set(Operation::PdfImage);
            target.set(ImageTarget::Png);
            quality.set(RasterQuality::Screen);
            extract_pages.set(PageSelection::All);
            export_pages.set(PageSelection::All);
            extract_output_mode.set(ExtractOutputMode::Individual);
            extract_chunk_size_draft.set("10".to_owned());
            message.set(String::new());
            progress.set(JobProgress::default());
            if let Some(input) = file_input.get() {
                input.set_value("");
            }
        }
    });

    let remove_file: Rc<dyn Fn(u64)> = Rc::new(move |id| {
        if busy.get_untracked() {
            return;
        }
        let mut selected = files.get_untracked();
        selected.retain(|file| file.id != id);
        pdf_page_counts.update(|counts| {
            counts.remove(&id);
        });
        if selected.is_empty() {
            files.set(selected);
            operation.set(Operation::PdfImage);
        } else {
            let descriptors = descriptors(&selected);
            let available = visible_operations(&descriptors);
            let next_operation = if available.contains(&operation.get_untracked()) {
                operation.get_untracked()
            } else {
                normalize_selection(
                    &descriptors,
                    operation.get_untracked(),
                    SelectionIntent::ContinueWorkflow,
                )
                .map(|normalized| normalized.operation)
                .unwrap_or(Operation::Impose)
            };
            files.set(selected);
            operation.set(next_operation);
        }
        message.set(String::new());
        progress.set(JobProgress::default());
    });

    let reorder_file: Rc<dyn Fn(u64, u64)> = Rc::new(move |dragged_id, target_id| {
        if busy.get_untracked() || dragged_id == target_id {
            return;
        }
        let mut changed = false;
        files.update(|selected| {
            let from = selected.iter().position(|file| file.id == dragged_id);
            let to = selected.iter().position(|file| file.id == target_id);
            if let (Some(from), Some(to)) = (from, to) {
                changed = move_item_to(selected, from, to);
            }
        });
        if changed {
            message.set(String::new());
            progress.set(JobProgress::default());
        }
    });

    let toggle_theme = move |_| {
        let next = if theme.get_untracked() == "dark" {
            "light"
        } else {
            "dark"
        };
        theme.set(next.into());
        browser::apply_theme(next, true);
        system_theme_listener.update_value(|listener| *listener = None);
    };

    on_cleanup(move || system_theme_listener.update_value(|listener| *listener = None));

    let visible = Signal::derive(move || visible_operations(&descriptors(&files.get())));
    let generic_loader = WorkflowLoader::new(
        false,
        Signal::derive(move || !files.read().is_empty() && operation.get() != Operation::Impose),
    );
    let impose_loader = WorkflowLoader::new(
        true,
        Signal::derive(move || !files.read().is_empty() && operation.get() == Operation::Impose),
    );
    let opening_generic = Signal::derive(move || {
        !files.read().is_empty() && operation.get() != Operation::Impose && !generic_loader.ready()
    });
    let show_upload = Signal::derive(move || files.read().is_empty() || opening_generic.get());
    let selected_pdf_page_count = Signal::derive(move || {
        let selected = files.read();
        let counts = pdf_page_counts.read();
        let mut total = 0_usize;
        let mut found_pdf = false;
        for selected_file in selected.iter() {
            if classify_file(&selected_file.descriptor) != FileKind::Pdf {
                continue;
            }
            found_pdf = true;
            let count = counts.get(&selected_file.id)?;
            total = total.checked_add(*count)?;
        }
        found_pdf.then_some(total)
    });

    let workflow_motion = browser::WorkflowMotion::default();
    let select_operation = UnsyncCallback::new(move |next| {
        let current = operation.get_untracked();
        if busy.get_untracked() || current == next {
            return;
        }
        if (current == Operation::Impose) != (next == Operation::Impose) {
            workflow_motion.run(|| operation.set(next));
        } else {
            operation.set(next);
        }
        message.set(String::new());
        progress.set(JobProgress::default());
    });

    let clear_workspace_callback = UnsyncCallback::new(move |_| {
        if busy.get_untracked() || files.read_untracked().is_empty() {
            return;
        }
        clear_workspace_open.set(true);
        browser::show_modal(
            clear_workspace_dialog,
            "clear-workspace-cancel".into(),
            move |result| {
                if let Err(error) = result {
                    clear_workspace_open.set(false);
                    message.set(error);
                    browser::restore_modal_focus("clear-workspace-trigger".into());
                }
            },
        );
    });
    let cancel_clear_workspace = UnsyncCallback::new(move |_| {
        clear_workspace_open.set(false);
        browser::restore_modal_focus("clear-workspace-trigger".into());
    });
    let confirm_clear_workspace = UnsyncCallback::new({
        let reset_workspace = reset_workspace.clone();
        move |_| {
            clear_workspace_open.set(false);
            reset_workspace();
        }
    });
    let browse_append = UnsyncCallback::new(move |_| browse(true));
    let browse_replace = UnsyncCallback::new(move |_| browse(false));
    let theme_callback = UnsyncCallback::new(move |_| toggle_theme(()));
    let remove_callback = UnsyncCallback::new({
        let remove_file = remove_file.clone();
        move |id| remove_file(id)
    });
    let reorder_callback = UnsyncCallback::new({
        let reorder_file = reorder_file.clone();
        move |(dragged_id, target_id)| reorder_file(dragged_id, target_id)
    });
    let drop_callback = UnsyncCallback::new({
        let select_files = select_files.clone();
        move |dropped| select_files(dropped, SelectionIntent::NewUpload, false)
    });

    let generic_workflow = generic::GenericWorkflow {
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
    };

    let cancellation_for_cleanup = cancellation;
    on_cleanup(move || {
        cancellation_for_cleanup.with_value(|active| {
            if let Some(controller) = active {
                controller.abort();
            }
        });
    });

    view! {
        <main
            class="shell"
            class:empty-shell=move || show_upload.get()
            class:generic-shell=move || !show_upload.get() && operation.get() != Operation::Impose
        >
            <Show when=move || !show_upload.get()>
                <AppHeader
                    files
                    operation
                    busy
                    theme
                    on_clear=clear_workspace_callback
                    on_append=browse_append
                    on_replace=browse_replace
                    on_toggle_theme=theme_callback
                />
            </Show>
            <input
                node_ref=file_input
                id="file-input"
                class="file-input"
                type="file"
                accept="application/pdf,image/png,image/jpeg"
                multiple
                tabindex="-1"
                aria-label="Upload a PDF or image"
                on:change=on_files
            />
            <Show when=move || clear_workspace_open.get()>
                <dialog
                    node_ref=clear_workspace_dialog
                    class="quantity-dialog range-dialog clear-workspace-dialog"
                    aria-labelledby="clear-workspace-title"
                    aria-describedby="clear-workspace-description"
                    on:cancel=move |event: web_sys::Event| {
                        event.prevent_default();
                        cancel_clear_workspace.run(());
                    }
                >
                    <header class="quantity-dialog-head">
                        <div>
                            <p class="eyebrow">"Clear workspace"</p>
                            <h2 id="clear-workspace-title">"Clear all files?"</h2>
                        </div>
                        <button class="quantity-dialog-close" type="button" aria-label="Close clear workspace confirmation" on:click=move |_| cancel_clear_workspace.run(())>
                            <span aria-hidden="true">"×"</span>
                        </button>
                    </header>
                    <p id="clear-workspace-description">"This removes every selected file and resets all tool settings. This action cannot be undone."</p>
                    <footer class="quantity-dialog-actions range-dialog-actions">
                        <button id="clear-workspace-cancel" class="ghost-button" type="button" on:click=move |_| cancel_clear_workspace.run(())>"Keep working"</button>
                        <button class="danger-button" type="button" on:click=move |_| confirm_clear_workspace.run(())>"Clear files"</button>
                    </footer>
                </dialog>
            </Show>
            <Show
                when=move || show_upload.get()
                fallback=move || view! {
                    <section
                        node_ref=ready_shell
                        class="ready-shell"
                        class:generic-ready=move || operation.get() != Operation::Impose
                        aria-label="PDF workspace"
                    >
                        <section
                            node_ref=ready_card
                            class="ready-card"
                            class:imposing=move || operation.get() == Operation::Impose
                            style:height=move || ready_card_height.get().map(|height| format!("{height}px"))
                        >
                            <div>
                            <OperationChooser
                                operation
                                visible
                                busy
                                on_select=select_operation
                            />
                            <Show when=move || operation.get() == Operation::Impose && !message.read().is_empty()>
                                <p class="error" role="alert">{move || message.get()}</p>
                            </Show>
                            </div>
                            {move || if is_imposition.get() {
                                impose_loader.render(generic_workflow)
                            } else {
                                generic_loader.render(generic_workflow)
                            }}
                            <Show when=move || checking_pdfs.get() && operation.get() == Operation::Impose>
                                <div class="inspection-feedback">
                                    <p role="status">"Checking PDFs…"</p>
                                    <button class="cancel-button" type="button" on:click=move |_| cancel_inspection.run(())>"Cancel inspection"</button>
                                </div>
                            </Show>
                        </section>
                    </section>
                }
            >
                <EmptyUploadState
                    drag_active
                    busy
                    message
                    on_browse=browse_replace
                    on_drop=drop_callback
                    on_cancel=cancel_inspection
                    opening=opening_generic
                    loader=generic_loader
                />
            </Show>
        </main>
    }
}

#[component]
fn AppHeader(
    files: RwSignal<Vec<SelectedFile>>,
    operation: RwSignal<Operation>,
    busy: RwSignal<bool>,
    theme: RwSignal<String>,
    on_clear: UnsyncCallback<()>,
    on_append: UnsyncCallback<()>,
    on_replace: UnsyncCallback<()>,
    on_toggle_theme: UnsyncCallback<()>,
) -> impl IntoView {
    let file_title = move || {
        let selected = files.read();
        let all_pdfs = selected
            .iter()
            .all(|file| classify_file(&file.descriptor) == FileKind::Pdf);
        let all_images = selected
            .iter()
            .all(|file| classify_file(&file.descriptor) == FileKind::Image);
        match selected.as_slice() {
            [] => String::new(),
            [file] if operation.get() == Operation::Impose => file.descriptor.name.clone(),
            many if operation.get() == Operation::Impose => format!(
                "{} artwork {}",
                many.len(),
                if many.len() == 1 { "file" } else { "files" },
            ),
            many if many.len() > 1 && all_pdfs => format!("{} PDFs", many.len()),
            many if all_images => format!(
                "{} {}",
                many.len(),
                if many.len() == 1 { "image" } else { "images" },
            ),
            [file] => file.descriptor.name.clone(),
            many => format!("{} files", many.len()),
        }
    };
    let file_detail = move || {
        let selected = files.read();
        let total = selected
            .iter()
            .map(|file| file.descriptor.size)
            .sum::<u64>();
        if selected.len() > 1 {
            format!("{} total", format_bytes(total))
        } else {
            format_bytes(total)
        }
    };
    view! {
        <header class="app-header">
            <div class="app-brand">
                <span class="app-brand-label"><strong>"PDF Tools"</strong></span>
            </div>
            <Show when=move || !files.read().is_empty()>
                <div class="app-file-context">
                    <span>"Current file"</span>
                    <strong title=file_title>{file_title}</strong>
                    <small>{file_detail}</small>
                </div>
            </Show>
            <div class="app-header-actions">
                <Show when=move || !files.read().is_empty()>
                    <button class="ghost-button" type="button" disabled=move || busy.get() on:click=move |_| on_append.run(())>
                        <span class="button-label-wide">{move || if operation.get() == Operation::Impose { "Add artwork" } else { "Add files" }}</span>
                        <span class="button-label-short">"Add"</span>
                    </button>
                    <Show when=move || operation.get() == Operation::Impose fallback=move || view! {
                        <button class="ghost-button" type="button" disabled=move || busy.get() on:click=move |_| on_replace.run(())>
                            <span class="button-label-wide">"Replace files"</span>
                            <span class="button-label-short">"Replace"</span>
                        </button>
                        <button id="clear-workspace-trigger" class="clear-workspace-button" type="button" disabled=move || busy.get() on:click=move |_| on_clear.run(())>"Clear"</button>
                    }>
                        <details class="impose-more-menu">
                            <summary>"More"</summary>
                            <div class="impose-more-menu-items">
                                <button class="ghost-button" type="button" disabled=move || busy.get() on:click=move |_| on_replace.run(())>"Replace artwork"</button>
                                <button id="clear-workspace-trigger" class="clear-workspace-button" type="button" disabled=move || busy.get() on:click=move |_| on_clear.run(())>"Clear"</button>
                            </div>
                        </details>
                    </Show>
                </Show>
                <button
                    class="theme-toggle"
                    type="button"
                    role="switch"
                    aria-checked=move || aria_bool(theme.get() == "dark")
                    aria-label=move || if theme.get() == "dark" { "Switch to light mode" } else { "Switch to dark mode" }
                    on:click=move |_| on_toggle_theme.run(())
                >
                    <span class="theme-toggle-track" aria-hidden="true">
                        <svg class="theme-toggle-glyph theme-toggle-sun" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                            <circle cx="12" cy="12" r="3.5"/>
                            <path d="M12 2.5v2M12 19.5v2M4.93 4.93l1.42 1.42M17.65 17.65l1.42 1.42M2.5 12h2M19.5 12h2M4.93 19.07l1.42-1.42M17.65 6.35l1.42-1.42"/>
                        </svg>
                        <svg class="theme-toggle-glyph theme-toggle-moon" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                            <path d="M20.5 15.1A8.5 8.5 0 0 1 8.9 3.5 8.5 8.5 0 1 0 20.5 15.1Z"/>
                        </svg>
                        <span class="theme-toggle-thumb"></span>
                    </span>
                    <span class="visually-hidden">{move || if theme.get() == "dark" { "Dark theme active" } else { "Light theme active" }}</span>
                </button>
            </div>
        </header>
    }
}

#[component]
fn EmptyUploadState(
    drag_active: RwSignal<bool>,
    busy: RwSignal<bool>,
    message: RwSignal<String>,
    on_browse: UnsyncCallback<()>,
    on_drop: UnsyncCallback<Vec<File>>,
    on_cancel: UnsyncCallback<()>,
    opening: Signal<bool>,
    loader: WorkflowLoader,
) -> impl IntoView {
    let drag_depth = Rc::new(Cell::new(0_u32));
    let on_drag_enter = {
        let drag_depth = drag_depth.clone();
        move |event: DragEvent| {
            event.prevent_default();
            drag_depth.set(drag_depth.get().saturating_add(1));
            drag_active.set(true);
        }
    };
    let on_drag_leave = {
        let drag_depth = drag_depth.clone();
        move |event: DragEvent| {
            event.prevent_default();
            drag_depth.set(drag_depth.get().saturating_sub(1));
            if drag_depth.get() == 0 {
                drag_active.set(false);
            }
        }
    };
    let on_drag_over = move |event: DragEvent| {
        event.prevent_default();
        if let Some(data) = event.data_transfer() {
            data.set_drop_effect("copy");
        }
    };
    let on_drop_event = {
        let drag_depth = drag_depth.clone();
        move |event: DragEvent| {
            event.prevent_default();
            if busy.get_untracked() {
                return;
            }
            drag_depth.set(0);
            drag_active.set(false);
            let dropped = event
                .data_transfer()
                .and_then(|data| data.files())
                .map(files_from_list)
                .unwrap_or_default();
            on_drop.run(dropped);
        }
    };
    view! {
        <section
            class="empty-state"
            class:drag-active=move || drag_active.get()
            aria-labelledby="empty-state-title"
            aria-describedby="empty-state-description"
            on:dragenter=on_drag_enter
            on:dragover=on_drag_over
            on:dragleave=on_drag_leave
            on:drop=on_drop_event
        >
            <div class="empty-state-content">
                <div class="empty-intro">
                    <p class="eyebrow">"PDF Tools"</p>
                    <h1 id="empty-state-title">{move || if drag_active.get() { "Drop files to upload" } else { "Upload files" }}</h1>
                </div>
                <button class="primary-button" type="button"
                    disabled=move || busy.get() || (opening.get() && !loader.failed())
                    on:click=move |_| if opening.get() { loader.retry(); } else { on_browse.run(()); }
                >
                    {move || if busy.get() { "Checking PDFs…" } else if opening.get() {
                        if loader.failed() { "Retry loading tools" } else { "Opening PDF tools…" }
                    } else { "Choose files" }}
                </button>
                <Show when=move || busy.get()>
                    <button class="cancel-button" type="button" on:click=move |_| on_cancel.run(())>"Cancel inspection"</button>
                </Show>
                <p id="empty-state-description" class="empty-formats"
                    role=move || if opening.get() && loader.failed() { Some("alert") } else { None }
                    aria-live="polite"
                >
                    {move || if opening.get() && loader.failed() {
                        "These tools couldn’t load. Your selected files are still here."
                    } else if drag_active.get() { "Release to upload · PDF, PNG, or JPEG" } else { "PDF, PNG, or JPEG · You can also drag and drop" }}
                </p>
                <Show when=move || opening.get()>{loader.recovery_hint()}</Show>
                <Show when=move || !message.read().is_empty()>
                    <p class="error empty-error" role="alert">{move || message.get()}</p>
                </Show>
            </div>
        </section>
    }
}

#[component]
fn OperationChooser(
    operation: RwSignal<Operation>,
    visible: Signal<Vec<Operation>>,
    busy: RwSignal<bool>,
    on_select: UnsyncCallback<Operation>,
) -> impl IntoView {
    view! {
        <section class="operation-chooser" aria-label="PDF tools">
            <div class="operation-tabs" role="group" aria-label="Available PDF tools">
                <For
                    each=move || visible.get()
                    key=|item| item.id()
                    children=move |item| view! {
                        <button
                            type="button"
                            class:active=move || operation.get() == item
                            aria-pressed=move || aria_bool(operation.get() == item)
                            disabled=move || busy.get()
                            on:click=move |_| on_select.run(item)
                        >
                            <span>{item.label()}</span>
                        </button>
                    }
                />
            </div>
        </section>
    }
}

fn descriptors(files: &[SelectedFile]) -> Vec<FileDescriptor> {
    files.iter().map(|file| file.descriptor.clone()).collect()
}

fn files_from_list(list: web_sys::FileList) -> Vec<File> {
    (0..list.length())
        .filter_map(|index| list.get(index))
        .collect()
}

fn format_bytes(bytes: u64) -> String {
    format!("{:.2} MB", bytes as f64 / 1024.0 / 1024.0)
}
