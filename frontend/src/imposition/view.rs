//! Leptos owner for the complete imposition lifecycle and workspace.

use std::{cell::Cell, collections::HashSet, rc::Rc};

use gloo_timers::{callback::Interval, future::TimeoutFuture};
use leptos::{html, prelude::*};
use wasm_bindgen_futures::spawn_local;

use crate::{
    browser::{self, save_job_download, BrowserCancellation, JobProgress},
    files::SelectedFile,
    presentation::aria_bool,
};

use super::model::{effective_artwork_fit, numeric_draft_matches_value, parse_numeric_input};
use super::{
    browser as impose_browser, initial_preparation_percent, initial_preview_pages, initial_request,
    preview::{
        request_preview_batch_with_retry, ImposeWorkspaceToolbar, PreviewToolbarState, SheetPreview,
    },
    request_batches as preview_request_batches, request_from_analysis, request_issue_with_analysis,
    request_signature, request_with_manual_source_bleed, restore_request_from_analysis,
    select_layout_mode, set_all_repeat_quantities, set_imposition_mode_preserving_repeat_drafts,
    set_sides_preserving_repeat_drafts, size_label, update_repeat_quantity, BleedOption,
    DuplexFlipEdge, ExportLifecycle, ImpositionMode, InitialPreparationPhase, LayoutLifecycle,
    LayoutMode, LayoutRequest, LayoutResult, PdfAnalysis, PreparationLifecycle, PreparedPdfSource,
    RepeatQuantityDrafts, Sides, SizeInches,
};

#[component]
pub(crate) fn ImposeWorkspace(
    files: RwSignal<Vec<SelectedFile>>,
    busy: RwSignal<bool>,
) -> impl IntoView {
    let request = RwSignal::new(initial_request());
    provide_context(NumericInputValidity(
        RwSignal::new(HashSet::<String>::new()),
    ));
    let selected_page = RwSignal::new(1_usize);
    let quantity_drafts =
        RwSignal::new(RepeatQuantityDrafts::from_request(&request.get_untracked()));
    let source = RwSignal::new(Option::<PreparedPdfSource>::None);
    let preparation = RwSignal::new(PreparationLifecycle::Idle);
    let layout = RwSignal::new(LayoutLifecycle::Empty);
    let export = RwSignal::new(ExportLifecycle::Idle);
    let exporting = RwSignal::new(false);
    let preparation_progress = RwSignal::new(JobProgress::default());
    let preview_cache = RwSignal::new(impose_browser::PreviewUrlCache::default());
    let preview_toolbar = PreviewToolbarState::new();
    let finished_size_revision = RwSignal::new(0_u64);
    let export_progress = RwSignal::new(JobProgress::default());
    let retry_generation = RwSignal::new(0_u64);
    let layout_retry_generation = RwSignal::new(0_u64);
    let preparation_id = Rc::new(Cell::new(0_u64));
    let layout_id = Rc::new(Cell::new(0_u64));
    let export_id = Rc::new(Cell::new(0_u64));
    let attempted_files = StoredValue::new_local(Vec::<u64>::new());
    let committed_files = StoredValue::new_local(Vec::<SelectedFile>::new());
    let retry_files = StoredValue::new_local(Vec::<SelectedFile>::new());
    let preparation_cancel = StoredValue::new_local(Option::<BrowserCancellation>::None);
    let layout_cancel = StoredValue::new_local(Option::<BrowserCancellation>::None);
    let export_cancel = StoredValue::new_local(Option::<BrowserCancellation>::None);
    let lease_timer = StoredValue::new_local(Option::<Interval>::None);
    let visibility_lease =
        StoredValue::new_local(Option::<impose_browser::VisibilityLeaseListener>::None);

    Effect::new({
        let preparation_id = preparation_id.clone();
        move |_| {
            let selected = files.get();
            let _retry = retry_generation.get();
            if selected.is_empty() {
                return;
            }
            let ids = selected.iter().map(|file| file.id).collect::<Vec<_>>();
            let already_attempted = attempted_files
                .try_with_value(|attempted| attempted == &ids)
                .unwrap_or(true);
            if already_attempted {
                return;
            }
            attempted_files.update_value(|attempted| *attempted = ids);
            retry_files.update_value(|retry| *retry = selected.clone());
            preparation_cancel.update_value(|active| {
                if let Some(controller) = active.take() {
                    controller.abort();
                }
            });
            let Ok(controller) = BrowserCancellation::new() else {
                preparation.set(PreparationLifecycle::Failed {
                    request_id: preparation_id.get(),
                    message: "The browser could not start source preparation.".into(),
                });
                return;
            };
            preparation_cancel.update_value(|active| *active = Some(controller.clone()));
            let id = preparation_id.get().saturating_add(1);
            preparation_id.set(id);
            preparation.set(PreparationLifecycle::Preparing { request_id: id });
            preparation_progress.set(JobProgress {
                percent: 0,
                stage: "Preparing source artwork".into(),
                detail: selected
                    .iter()
                    .map(|file| file.descriptor.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
            busy.set(true);
            let progress_controller = controller.clone();
            let sink = impose_browser::progress_sink(move |next| {
                let active = initial_preparation_request_is_active(
                    preparation,
                    preparation_cancel,
                    &progress_controller,
                    id,
                );
                if active {
                    preparation_progress.set(JobProgress {
                        percent: initial_preparation_percent(InitialPreparationPhase::Analysis {
                            percent: f64::from(next.percent),
                        }),
                        stage: next.stage,
                        detail: next.detail,
                    });
                }
            });
            spawn_local(async move {
                let result =
                    impose_browser::prepare_source(&selected, sink, controller.clone()).await;
                if !initial_preparation_request_is_active(
                    preparation,
                    preparation_cancel,
                    &controller,
                    id,
                ) {
                    if let Ok(prepared) = result {
                        let _ = impose_browser::delete_source(&prepared.source_id).await;
                    }
                    return;
                }
                let prepared = match result {
                    Ok(prepared) => prepared,
                    Err(message) => {
                        preparation_cancel.update_value(|current| *current = None);
                        InitialPreparationFailureContext {
                            files,
                            committed_files,
                            attempted_files,
                            preparation,
                            busy,
                        }
                        .record(id, message);
                        return;
                    }
                };
                let fail_prepared = |message| {
                    discard_prepared_source(prepared.source_id.clone());
                    preparation_cancel.update_value(|current| *current = None);
                    InitialPreparationFailureContext {
                        files,
                        committed_files,
                        attempted_files,
                        preparation,
                        busy,
                    }
                    .record(id, message);
                };
                let old_source = source.get_untracked().map(|value| value.source_id);
                let mut next_request = if old_source.is_some() {
                    restore_request_from_analysis(request.get_untracked(), &prepared.analysis, true)
                } else {
                    request_from_analysis(request.get_untracked(), &prepared.analysis)
                };
                next_request.source_id = Some(prepared.source_id.clone());
                let signature = match request_signature(&next_request) {
                    Ok(signature) => signature,
                    Err(message) => {
                        fail_prepared(message);
                        return;
                    }
                };
                preparation_progress.set(JobProgress {
                    percent: initial_preparation_percent(InitialPreparationPhase::Layout),
                    stage: "Calculating initial sheet layout".into(),
                    detail: prepared.analysis.filename.clone(),
                });
                let initial_layout =
                    impose_browser::request_layout(&next_request, &controller).await;
                if !initial_preparation_request_is_active(
                    preparation,
                    preparation_cancel,
                    &controller,
                    id,
                ) {
                    let _ = impose_browser::delete_source(&prepared.source_id).await;
                    return;
                }
                let initial_layout = match initial_layout {
                    Ok(initial_layout) => initial_layout,
                    Err(message) => {
                        // A readable source is not an invalid upload just because the retained
                        // finished size cannot fit the current sheet. Publish its editable setup.
                        preparation_cancel.update_value(|current| *current = None);
                        layout_cancel.update_value(|active| {
                            if let Some(controller) = active.take() {
                                controller.abort();
                            }
                        });
                        committed_files.update_value(|committed| *committed = selected);
                        retry_files.update_value(Vec::clear);
                        preview_cache.update(|cache| cache.select_source(&prepared.source_id));
                        layout.set(LayoutLifecycle::Failed {
                            signature,
                            message,
                            previous: None,
                        });
                        quantity_drafts
                            .update(|drafts| drafts.rebase(next_request.source_page_count));
                        request.set(next_request);
                        source.set(Some(prepared.clone()));
                        preparation.set(PreparationLifecycle::Ready {
                            request_id: id,
                            source_id: prepared.source_id.clone(),
                        });
                        busy.set(false);
                        if let Some(old_source) =
                            old_source.filter(|old| old != &prepared.source_id)
                        {
                            discard_prepared_source(old_source);
                        }
                        return;
                    }
                };
                let initial_pages = initial_preview_pages(&initial_layout);
                if initial_pages.is_empty() {
                    fail_prepared("The initial sheet did not contain previewable artwork.".into());
                    return;
                }
                let total_initial_pages = initial_pages.len();
                preparation_progress.set(JobProgress {
                    percent: initial_preparation_percent(InitialPreparationPhase::Artwork {
                        completed: 0,
                        total: total_initial_pages,
                    }),
                    stage: "Rendering the first sheet".into(),
                    detail: format!(
                        "{} — 0 of {total_initial_pages} artwork pages",
                        prepared.analysis.filename
                    ),
                });
                let protected = initial_pages.iter().copied().collect::<HashSet<_>>();
                let raster_identity = super::model::preview_raster_identity(
                    &prepared.source_id,
                    initial_layout.source_bleed_override,
                );
                let mut initial_cache = match impose_browser::PreviewUrlCache::staging_for_source(
                    &raster_identity,
                    &protected,
                ) {
                    Ok(cache) => cache,
                    Err(message) => {
                        fail_prepared(message);
                        return;
                    }
                };
                let mut completed = 0_usize;
                for batch in preview_request_batches(&initial_pages) {
                    let rendered = request_preview_batch_with_retry(
                        &prepared.source_id,
                        &batch,
                        initial_layout.source_bleed_override,
                        &controller,
                    )
                    .await;
                    if !initial_preparation_request_is_active(
                        preparation,
                        preparation_cancel,
                        &controller,
                        id,
                    ) {
                        let _ = impose_browser::delete_source(&prepared.source_id).await;
                        return;
                    }
                    match rendered {
                        Ok(rendered) => {
                            for asset in rendered {
                                let page = asset.page();
                                match initial_cache.insert_for_current_source(
                                    &raster_identity,
                                    page,
                                    asset,
                                ) {
                                    Ok(true) => completed = completed.saturating_add(1),
                                    Ok(false) => {
                                        fail_prepared("The prepared artwork source changed before its preview was ready.".into());
                                        return;
                                    }
                                    Err(message) => {
                                        fail_prepared(message);
                                        return;
                                    }
                                }
                            }
                        }
                        Err(message) => {
                            fail_prepared(message);
                            return;
                        }
                    }
                    preparation_progress.set(JobProgress {
                        percent: initial_preparation_percent(InitialPreparationPhase::Artwork {
                            completed,
                            total: total_initial_pages,
                        }),
                        stage: "Rendering the first sheet".into(),
                        detail: format!(
                            "{} — {completed} of {total_initial_pages} artwork pages",
                            prepared.analysis.filename
                        ),
                    });
                }
                let published = preview_cache
                    .try_update(|cache| cache.replace_with(initial_cache))
                    .is_some();
                if !published {
                    fail_prepared("The artwork preview cache is unavailable.".into());
                    return;
                }
                preparation_progress.set(JobProgress {
                    percent: initial_preparation_percent(InitialPreparationPhase::Ready),
                    stage: "Imposition workspace ready".into(),
                    detail: prepared.analysis.filename.clone(),
                });
                preparation_cancel.update_value(|current| *current = None);
                layout_cancel.update_value(|active| {
                    if let Some(controller) = active.take() {
                        controller.abort();
                    }
                });
                committed_files.update_value(|committed| *committed = selected);
                retry_files.update_value(Vec::clear);
                layout.set(LayoutLifecycle::Ready {
                    signature,
                    layout: initial_layout,
                });
                quantity_drafts.update(|drafts| drafts.rebase(next_request.source_page_count));
                request.set(next_request);
                source.set(Some(prepared.clone()));
                preparation.set(PreparationLifecycle::Ready {
                    request_id: id,
                    source_id: prepared.source_id.clone(),
                });
                busy.set(false);
                if let Some(old_source) = old_source {
                    if old_source != prepared.source_id {
                        spawn_local(async move {
                            let _ = impose_browser::delete_source(&old_source).await;
                        });
                    }
                }
            });
        }
    });

    Effect::new({
        let layout_id = layout_id.clone();
        move |_| {
            let _retry = layout_retry_generation.get();
            let Some(prepared) = source.get() else {
                return;
            };
            let next_request = request.get();
            let Some(signature) = request_signature(&next_request).ok() else {
                return;
            };
            if request_issue_with_analysis(&next_request, Some(&prepared.analysis)).is_some() {
                layout_cancel.update_value(|active| {
                    if let Some(controller) = active.take() {
                        controller.abort();
                    }
                });
                let previous = layout.get_untracked().last_good().cloned();
                layout.set(LayoutLifecycle::Invalid { previous });
                return;
            }
            let unchanged = matches!(layout.get_untracked(), LayoutLifecycle::Ready { signature: ref current, .. } if current == &signature);
            if unchanged {
                return;
            }
            layout_cancel.update_value(|active| {
                if let Some(controller) = active.take() {
                    controller.abort();
                }
            });
            let Ok(controller) = BrowserCancellation::new() else {
                let previous = layout.get_untracked().last_good().cloned();
                layout.set(LayoutLifecycle::Failed {
                    signature,
                    message: "The browser could not start the sheet layout request.".into(),
                    previous,
                });
                return;
            };
            layout_cancel.update_value(|active| *active = Some(controller.clone()));
            let id = layout_id.get().saturating_add(1);
            layout_id.set(id);
            let previous = layout.get_untracked().last_good().cloned();
            layout.set(LayoutLifecycle::Loading {
                request_id: id,
                signature: signature.clone(),
                previous,
            });
            let source_id = prepared.source_id;
            spawn_local(async move {
                let result = impose_browser::request_layout(&next_request, &controller).await;
                let active = layout_cancel
                    .try_with_value(|current| {
                        current
                            .as_ref()
                            .is_some_and(|value| value.same_job(&controller))
                    })
                    .unwrap_or(false);
                let source_current = source
                    .try_get_untracked()
                    .flatten()
                    .is_some_and(|value| value.source_id == source_id);
                if !active || !source_current {
                    return;
                }
                layout_cancel.update_value(|current| *current = None);
                let accepts = layout
                    .try_get_untracked()
                    .is_some_and(|state| state.accepts(id, &signature));
                if !accepts {
                    return;
                }
                match result {
                    Ok(result) => layout.set(LayoutLifecycle::Ready {
                        signature,
                        layout: result,
                    }),
                    Err(message) => {
                        let previous = layout.get_untracked().last_good().cloned();
                        layout.set(LayoutLifecycle::Failed {
                            signature,
                            message,
                            previous,
                        });
                    }
                }
            });
        }
    });

    Effect::new(move |_| {
        let next_id = source.get().map(|prepared| prepared.source_id);
        lease_timer.update_value(|timer| {
            *timer = next_id.map(|source_id| {
                spawn_local({
                    let source_id = source_id.clone();
                    async move {
                        let _ = impose_browser::renew_source(&source_id).await;
                    }
                });
                Interval::new(10 * 60 * 1_000, move || {
                    let source_id = source_id.clone();
                    spawn_local(async move {
                        let _ = impose_browser::renew_source(&source_id).await;
                    });
                })
            });
        });
    });

    visibility_lease.update_value(|listener| {
        *listener = impose_browser::visibility_lease_listener(move || {
            if let Some(prepared) = source.get_untracked() {
                spawn_local(async move {
                    let _ = impose_browser::renew_source(&prepared.source_id).await;
                });
            }
        })
        .ok();
    });

    let cancel_preparation = move |_| {
        preparation_cancel.update_value(|active| {
            if let Some(controller) = active.take() {
                controller.abort();
            }
        });
        preparation.set(PreparationLifecycle::Cancelled {
            request_id: preparation_id.get(),
        });
        restore_committed_selection(files, committed_files, attempted_files);
        busy.set(false);
    };

    let retry_preparation = move |_| {
        retry_files.with_value(|retry| {
            if !retry.is_empty() {
                files.set(retry.clone());
            }
        });
        attempted_files.update_value(Vec::clear);
        retry_generation.update(|value| *value = value.saturating_add(1));
    };

    let cancel_export = move |_| {
        export_cancel.update_value(|active| {
            if let Some(controller) = active.take() {
                controller.abort();
            }
        });
        export.set(ExportLifecycle::Cancelled);
        exporting.set(false);
        busy.set(false);
    };

    let download = {
        let export_id = export_id.clone();
        move |_| {
            if busy.get_untracked() {
                return;
            }
            let Some(prepared) = source.get_untracked() else {
                export.set(ExportLifecycle::Failed {
                    message: "Prepare source artwork before exporting.".into(),
                });
                return;
            };
            let current_request = request.get_untracked();
            let signature = request_signature(&current_request).unwrap_or_default();
            let current_layout = layout.get_untracked();
            let ready = matches!(current_layout, LayoutLifecycle::Ready { signature: ref current, .. } if current == &signature);
            if !ready
                || request_issue_with_analysis(&current_request, Some(&prepared.analysis)).is_some()
            {
                export.set(ExportLifecycle::Failed {
                    message: "Wait for the current valid sheet layout before exporting.".into(),
                });
                return;
            }
            let Ok(controller) = BrowserCancellation::new() else {
                export.set(ExportLifecycle::Failed {
                    message: "The browser could not start the PDF export.".into(),
                });
                return;
            };
            export_cancel.update_value(|active| *active = Some(controller.clone()));
            let id = export_id.get().saturating_add(1);
            export_id.set(id);
            export.set(ExportLifecycle::Exporting { request_id: id });
            export_progress.set(JobProgress {
                percent: 0,
                stage: "Preparing imposed PDF".into(),
                detail: prepared.analysis.filename.clone(),
            });
            exporting.set(true);
            busy.set(true);
            let progress_controller = controller.clone();
            let sink = impose_browser::progress_sink(move |next| {
                let active = export_cancel
                    .try_with_value(|current| {
                        current
                            .as_ref()
                            .is_some_and(|value| value.same_job(&progress_controller))
                    })
                    .unwrap_or(false);
                if active {
                    export_progress.set(next);
                }
            });
            spawn_local(async move {
                let exported_source_id = prepared.source_id.clone();
                let result = impose_browser::export_pdf(
                    &prepared.source_id,
                    &current_request,
                    sink,
                    controller.clone(),
                )
                .await;
                let active = export_cancel
                    .try_with_value(|current| {
                        current
                            .as_ref()
                            .is_some_and(|value| value.same_job(&controller))
                    })
                    .unwrap_or(false);
                let source_current = source
                    .try_get_untracked()
                    .flatten()
                    .is_some_and(|value| value.source_id == exported_source_id);
                let export_current = export.try_get_untracked().is_some_and(
                    |state| matches!(state, ExportLifecycle::Exporting { request_id } if request_id == id),
                );
                if !active || !source_current || !export_current {
                    if active {
                        export_cancel.update_value(|current| *current = None);
                        export.set(ExportLifecycle::Cancelled);
                        exporting.set(false);
                        busy.set(false);
                    }
                    return;
                }
                export_cancel.update_value(|current| *current = None);
                match result {
                    Ok(download) => match save_job_download(download) {
                        Ok(filename) => export.set(ExportLifecycle::Complete { filename }),
                        Err(message) => export.set(ExportLifecycle::Failed { message }),
                    },
                    Err(message) => export.set(ExportLifecycle::Failed { message }),
                }
                exporting.set(false);
                busy.set(false);
            });
        }
    };

    on_cleanup(move || {
        preparation_cancel.with_value(|active| {
            if let Some(controller) = active {
                controller.abort();
            }
        });
        layout_cancel.with_value(|active| {
            if let Some(controller) = active {
                controller.abort();
            }
        });
        export_cancel.with_value(|active| {
            if let Some(controller) = active {
                controller.abort();
            }
        });
        lease_timer.update_value(|timer| *timer = None);
        visibility_lease.update_value(|listener| *listener = None);
        exporting.set(false);
        preview_cache.update(impose_browser::PreviewUrlCache::clear);
        if let Some(prepared) = source.get_untracked() {
            spawn_local(async move {
                let _ = impose_browser::delete_source(&prepared.source_id).await;
            });
        }
        busy.set(false);
    });

    let download = UnsyncCallback::new(download);
    let cancel_export = UnsyncCallback::new(cancel_export);
    let retry_layout = move || {
        layout_retry_generation.update(|generation| {
            *generation = generation.saturating_add(1);
        });
    };
    let retry_layout_status = UnsyncCallback::new(move |_| retry_layout());
    let active_rail = RwSignal::new(RailPanel::Setup);
    let previous_rail = RwSignal::new(RailPanel::Setup);
    let controls_collapsed = RwSignal::new(false);
    let preview_is_rail_tab = RwSignal::new(false);
    Effect::new(move |_| {
        if source.get().is_some() {
            browser::focus_element_after_render("finished-width".to_owned());
        }
    });
    let _rail_breakpoint_listener = StoredValue::new_local(
        browser::media_query_listener("(max-width: 1100px)", move |matches| {
            let focused = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.active_element());
            preview_is_rail_tab.set(matches);
            if matches {
                if controls_collapsed.get_untracked()
                    || focused.as_ref().is_some_and(|element| {
                        element
                            .closest("#gang-preview-panel")
                            .ok()
                            .flatten()
                            .is_some()
                    })
                {
                    active_rail.set(RailPanel::Preview);
                }
                controls_collapsed.set(false);
                if focused
                    .as_ref()
                    .is_some_and(|element| element.id() == "impose-controls-toggle")
                {
                    browser::focus_element(active_rail.get_untracked().tab_id());
                }
            } else {
                active_rail.set(RailPanel::Setup);
                if focused.as_ref().is_some_and(|element| {
                    element.id() == RailPanel::Setup.tab_id()
                        || element.id() == RailPanel::Preview.tab_id()
                }) {
                    browser::focus_element("impose-controls-toggle");
                }
            }
        })
        .ok(),
    );

    view! {
        <section class="gang-workspace embedded compact has-layout" class:controls-collapsed=move || controls_collapsed.get() class:preview-active=move || active_rail.get() == RailPanel::Preview aria-label="Imposition job workspace">
            <PreparationStatus
                preparation
                files
                progress=preparation_progress
                on_cancel=UnsyncCallback::new(cancel_preparation)
                on_retry=UnsyncCallback::new(retry_preparation)
            />
            <Show when=move || source.get().is_some()>
                <LayoutFailureStatus layout on_retry=retry_layout_status />
                <div class="gang-grid">
                    <RailSwitcher active=active_rail previous=previous_rail />
                    <button
                        id="impose-controls-toggle"
                        class="impose-controls-toggle"
                        type="button"
                        aria-controls="gang-setup-panel"
                        aria-expanded=move || aria_bool(!controls_collapsed.get())
                        aria-label=move || if controls_collapsed.get() { "Expand setup panel" } else { "Collapse setup panel" }
                        title=move || if controls_collapsed.get() { "Expand setup panel" } else { "Collapse setup panel" }
                        on:click=move |_| controls_collapsed.update(|collapsed| *collapsed = !*collapsed)
                    >
                        <span>{move || if controls_collapsed.get() { "Show setup toolbar" } else { "Hide setup toolbar" }}</span>
                    </button>
                    <ImpositionControls request quantity_drafts source layout export export_progress busy active_rail preview_is_rail_tab finished_size_revision on_download=download on_cancel_export=cancel_export />
                    <section id="gang-artwork-panel" class="gang-panel gang-artwork" class:rail-inactive=move || active_rail.get() != RailPanel::Artwork role=move || preview_is_rail_tab.get().then_some("tabpanel") aria-labelledby=move || preview_is_rail_tab.get().then_some(RailPanel::Artwork.tab_id()) aria-label="Artwork inspector">
                        <super::finished::ArtworkControls request layout preview_cache selected_page busy exporting />
                        <button class="artwork-done-button" type="button" on:click=move |_| {
                            let next = previous_rail.get_untracked();
                            active_rail.set(next);
                            let focus_target = if effective_artwork_fit(&request.get_untracked(), Some(selected_page.get_untracked())).mode == super::model::ArtworkFitMode::Cover {
                                "impose-position-trigger"
                            } else {
                                next.tab_id()
                            };
                            browser::focus_element_after_render(focus_target.to_owned());
                        } disabled=move || busy.get() || exporting.get()>"Done"</button>
                    </section>
                    <ImposeWorkspaceToolbar request layout preview_cache selected_page busy exporting active_rail previous_rail quantity_drafts finished_size_revision preview=preview_toolbar />
                    <SheetPreview source layout selected_page preview_cache active_rail preview_is_rail_tab toolbar=preview_toolbar />
                </div>
            </Show>
        </section>
    }
}

fn initial_preparation_request_is_active(
    preparation: RwSignal<PreparationLifecycle>,
    active: StoredValue<Option<BrowserCancellation>, LocalStorage>,
    controller: &BrowserCancellation,
    request_id: u64,
) -> bool {
    let controller_is_active = active
        .try_with_value(|current| {
            current
                .as_ref()
                .is_some_and(|value| value.same_job(controller))
        })
        .unwrap_or(false);
    controller_is_active
        && preparation
            .try_get_untracked()
            .is_some_and(|state| state.accepts(request_id))
}

fn discard_prepared_source(source_id: String) {
    spawn_local(async move {
        let _ = impose_browser::delete_source(&source_id).await;
    });
}

fn restore_committed_selection(
    files: RwSignal<Vec<SelectedFile>>,
    committed_files: StoredValue<Vec<SelectedFile>, LocalStorage>,
    attempted_files: StoredValue<Vec<u64>, LocalStorage>,
) {
    let committed = committed_files
        .try_with_value(Clone::clone)
        .unwrap_or_default();
    if committed.is_empty() {
        return;
    }
    let committed_ids = committed.iter().map(|file| file.id).collect();
    attempted_files.update_value(|attempted| *attempted = committed_ids);
    files.set(committed);
}

struct InitialPreparationFailureContext {
    files: RwSignal<Vec<SelectedFile>>,
    committed_files: StoredValue<Vec<SelectedFile>, LocalStorage>,
    attempted_files: StoredValue<Vec<u64>, LocalStorage>,
    preparation: RwSignal<PreparationLifecycle>,
    busy: RwSignal<bool>,
}

impl InitialPreparationFailureContext {
    fn record(self, request_id: u64, message: String) {
        restore_committed_selection(self.files, self.committed_files, self.attempted_files);
        self.preparation.set(PreparationLifecycle::Failed {
            request_id,
            message,
        });
        self.busy.set(false);
    }
}

#[component]
fn LayoutFailureStatus(
    layout: RwSignal<LayoutLifecycle>,
    on_retry: UnsyncCallback<()>,
) -> impl IntoView {
    view! {
        <Show when=move || matches!(layout.get(), LayoutLifecycle::Failed { .. })>
            {move || match layout.get() {
                LayoutLifecycle::Failed { message, .. } => view! { <section class="status-message layout-failure-status" role="alert"><strong>"Sheet layout needs attention"</strong><p>{message}</p><button class="ghost-button" type="button" on:click=move |_| on_retry.run(())>"Retry sheet layout"</button></section> }.into_any(),
                _ => ().into_any(),
            }}
        </Show>
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RailPanel {
    Setup,
    Artwork,
    Preview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupStep {
    Pdf,
    Job,
    Sheet,
    Bleed,
}

impl SetupStep {
    const ALL: [Self; 4] = [Self::Pdf, Self::Job, Self::Sheet, Self::Bleed];

    fn position(self) -> usize {
        match self {
            Self::Pdf => 1,
            Self::Job => 2,
            Self::Sheet => 3,
            Self::Bleed => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pdf => "Size",
            Self::Job => "Quantity & sheet",
            Self::Sheet => "Arrangement",
            Self::Bleed => "Bleed",
        }
    }

    fn heading_id(self) -> &'static str {
        match self {
            Self::Pdf => "setup-pdf-title",
            Self::Job => "job-settings-title",
            Self::Sheet => "arrangement-title",
            Self::Bleed => "bleed-title",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Pdf => Self::Job,
            Self::Job => Self::Sheet,
            Self::Sheet | Self::Bleed => Self::Bleed,
        }
    }
}

fn activate_setup_step(active: RwSignal<SetupStep>, next: SetupStep) {
    active.set(next);
    spawn_local(async move {
        TimeoutFuture::new(0).await;
        browser::focus_element(next.heading_id());
    });
}

fn setup_step_is_reached(step: SetupStep, furthest_reached: SetupStep) -> bool {
    step.position() <= furthest_reached.position()
}

fn continue_setup(
    active: RwSignal<SetupStep>,
    furthest_reached: RwSignal<SetupStep>,
    issue: Option<String>,
) {
    if issue.is_some() {
        return;
    }
    let next = active.get_untracked().next();
    furthest_reached.update(|furthest| {
        if next.position() > furthest.position() {
            *furthest = next;
        }
    });
    activate_setup_step(active, next);
}

impl RailPanel {
    pub(super) fn tab_id(self) -> &'static str {
        match self {
            Self::Setup => "gang-setup-tab",
            Self::Artwork => "gang-artwork-tab",
            Self::Preview => "gang-preview-tab",
        }
    }
}

fn rail_panel_for_key(current: RailPanel, key: &str) -> Option<RailPanel> {
    let panels = [RailPanel::Setup, RailPanel::Artwork, RailPanel::Preview];
    let index = panels
        .iter()
        .position(|panel| *panel == current)
        .unwrap_or(0);
    match key {
        "ArrowRight" | "ArrowDown" => Some(panels[(index + 1) % panels.len()]),
        "ArrowLeft" | "ArrowUp" => Some(panels[(index + panels.len() - 1) % panels.len()]),
        "Home" => Some(RailPanel::Setup),
        "End" => Some(RailPanel::Preview),
        _ => None,
    }
}

#[component]
fn RailSwitcher(active: RwSignal<RailPanel>, previous: RwSignal<RailPanel>) -> impl IntoView {
    let select = move |next: RailPanel| {
        let current = active.get_untracked();
        if next == RailPanel::Artwork && current != RailPanel::Artwork {
            previous.set(current);
        }
        active.set(next);
    };
    let handle_key = move |event: web_sys::KeyboardEvent, current| {
        let Some(next) = rail_panel_for_key(current, &event.key()) else {
            return;
        };
        event.prevent_default();
        select(next);
        browser::focus_element(next.tab_id());
    };
    view! {
        <div class="gang-rail-switcher" role="tablist" aria-label="Imposition workspace sections">
            <button id=RailPanel::Setup.tab_id() type="button" role="tab" aria-controls="gang-setup-panel" aria-selected=move || aria_bool(active.get() == RailPanel::Setup) tabindex=move || if active.get() == RailPanel::Setup { 0 } else { -1 } on:click=move |_| select(RailPanel::Setup) on:keydown=move |event| handle_key(event, RailPanel::Setup)>"Setup"</button>
            <button id=RailPanel::Artwork.tab_id() type="button" role="tab" aria-controls="gang-artwork-panel" aria-selected=move || aria_bool(active.get() == RailPanel::Artwork) tabindex=move || if active.get() == RailPanel::Artwork { 0 } else { -1 } on:click=move |_| select(RailPanel::Artwork) on:keydown=move |event| handle_key(event, RailPanel::Artwork)>"Artwork"</button>
            <button id=RailPanel::Preview.tab_id() type="button" role="tab" aria-controls="gang-preview-panel" aria-selected=move || aria_bool(active.get() == RailPanel::Preview) tabindex=move || if active.get() == RailPanel::Preview { 0 } else { -1 } on:click=move |_| select(RailPanel::Preview) on:keydown=move |event| handle_key(event, RailPanel::Preview)>"Preview"</button>
        </div>
    }
}

#[component]
fn PreparationStatus(
    preparation: RwSignal<PreparationLifecycle>,
    files: RwSignal<Vec<SelectedFile>>,
    progress: RwSignal<JobProgress>,
    on_cancel: UnsyncCallback<()>,
    on_retry: UnsyncCallback<()>,
) -> impl IntoView {
    view! {
        {move || match preparation.get() {
            PreparationLifecycle::Preparing { .. } => view! {
                <div class="impose-preparation-region">
                    <section class="impose-preparation" role="status" aria-live="polite">
                        <p class="eyebrow">"Preparing artwork"</p>
                        <h2>"Building the first sheet"</h2>
                        <ProgressBar progress />
                        <small>"Your files stay in order while the first sheet preview is prepared."</small>
                        <button class="ghost-button" type="button" on:click=move |_| on_cancel.run(())>"Cancel preparation"</button>
                    </section>
                </div>
            }.into_any(),
            PreparationLifecycle::Failed { message, .. } => view! {
                <section class="status-message impose-preparation-error" role="alert">
                    <strong>"Source preparation failed"</strong>
                    <p>{message}</p>
                    <Show when=move || !files.read().is_empty()>
                        <div class="file-summary" role="list" aria-label="Selected artwork available for repair">
                            <For
                                each=move || files.get()
                                key=|file| file.id
                                children=move |file| {
                                    let id = file.id;
                                    let name = file.descriptor.name.clone();
                                    let remove_name = name.clone();
                                    view! {
                                        <div class="file-row" role="listitem">
                                            <div class="file-meta"><span title=name.clone()>{name.clone()}</span></div>
                                            <button
                                                class="icon-button remove-file"
                                                type="button"
                                                aria-label=format!("Remove {remove_name}")
                                                on:click=move |_| {
                                                    files.update(|selected| selected.retain(|candidate| candidate.id != id));
                                                    if files.read_untracked().is_empty() {
                                                        preparation.set(PreparationLifecycle::Idle);
                                                    }
                                                }
                                            >"×"</button>
                                        </div>
                                    }
                                }
                            />
                        </div>
                    </Show>
                    <button class="ghost-button" type="button" on:click=move |_| on_retry.run(())>"Retry preparation"</button>
                </section>
            }.into_any(),
            PreparationLifecycle::Cancelled { .. } => view! {
                <section class="status-message impose-preparation-error" role="status">
                    <strong>"Preparation cancelled"</strong>
                    <p>"Server work stopped. Your files and previous setup are unchanged."</p>
                    <button class="ghost-button" type="button" on:click=move |_| on_retry.run(())>"Retry preparation"</button>
                </section>
            }.into_any(),
            PreparationLifecycle::Idle | PreparationLifecycle::Ready { .. } => ().into_any(),
        }}
    }
}

#[component]
fn ProgressBar(progress: RwSignal<JobProgress>) -> impl IntoView {
    view! {
        <div class="progress-status">
            <div><span>{move || progress.get().stage}</span><strong>{move || format!("{}%", progress.get().percent)}</strong></div>
            <progress max="100" value=move || progress.get().percent></progress>
            <small>{move || progress.get().detail}</small>
        </div>
    }
}

#[component]
fn ImpositionControls(
    request: RwSignal<LayoutRequest>,
    quantity_drafts: RwSignal<RepeatQuantityDrafts>,
    source: RwSignal<Option<PreparedPdfSource>>,
    layout: RwSignal<LayoutLifecycle>,
    export: RwSignal<ExportLifecycle>,
    export_progress: RwSignal<JobProgress>,
    busy: RwSignal<bool>,
    active_rail: RwSignal<RailPanel>,
    preview_is_rail_tab: RwSignal<bool>,
    finished_size_revision: RwSignal<u64>,
    on_download: UnsyncCallback<()>,
    on_cancel_export: UnsyncCallback<()>,
) -> impl IntoView {
    let invalid_inputs = expect_context::<NumericInputValidity>().0;
    let invalid_quantity_inputs = RwSignal::new(HashSet::<String>::new());
    provide_context(NumericInputValidity(invalid_inputs));
    let set_number = move |field: &'static str, value: f64| {
        request.update(|current| match field {
            "cut-width" | "cut-height" => {
                if field == "cut-width" {
                    current.finished_cut_size.width = value;
                } else {
                    current.finished_cut_size.height = value;
                }
                if let Some(bleed) = current.source_bleed_override {
                    if let Some(next) = request_with_manual_source_bleed(current.clone(), bleed) {
                        *current = next;
                    } else {
                        current.source_bleed_override = None;
                    }
                }
            }
            "sheet-width" => current.parent_sheet_size.width = value,
            "sheet-height" => current.parent_sheet_size.height = value,
            "gutter-horizontal" => current.gutter.horizontal = value,
            "gutter-vertical" => current.gutter.vertical = value,
            "created-bleed" => current.created_bleed_amount = value,
            "source-bleed" => {
                if let Some(next) = request_with_manual_source_bleed(current.clone(), value) {
                    *current = next;
                } else {
                    current.source_bleed_override = Some(value);
                }
            }
            _ => {}
        });
    };
    let set_manual_axis = move |rows: bool, value: usize| {
        request.update(|current| {
            if let Some(manual) = current.manual.as_mut() {
                if rows {
                    manual.rows = value;
                } else {
                    manual.columns = value;
                }
            }
        });
    };
    let analysis = move || source.get().map(|prepared| prepared.analysis);
    let issue = move || {
        if !request
            .get()
            .finished_dimensions_chosen
            .into_iter()
            .all(|chosen| chosen)
        {
            Some("Enter the finished width and height to continue.".to_owned())
        } else if !invalid_inputs.read().is_empty() {
            Some("Correct the highlighted number before continuing.".to_owned())
        } else {
            request_issue_with_analysis(&request.get(), analysis().as_ref())
        }
    };
    let active_step = RwSignal::new(SetupStep::Pdf);
    let furthest_reached = RwSignal::new(SetupStep::Pdf);
    let current_layout = move || layout.get().last_good().cloned();
    let current_request_has_ready_layout = move || {
        let signature = request_signature(&request.get()).unwrap_or_default();
        matches!(layout.get(), LayoutLifecycle::Ready { signature: current, .. } if current == signature)
    };
    let can_export = move || issue().is_none() && current_request_has_ready_layout();
    let exporting = move || matches!(export.get(), ExportLifecycle::Exporting { .. });
    let quantity_editor_open = RwSignal::new(false);
    let advanced_sheet_settings_open = RwSignal::new(false);
    let quantity_dialog = NodeRef::<html::Dialog>::new();
    let modal_error = RwSignal::new(Option::<String>::None);
    let bulk_quantity = RwSignal::new(1_usize);
    let quantity_apply_revision = RwSignal::new(0_u64);
    let close_quantity_editor = move || {
        quantity_editor_open.set(false);
        browser::restore_modal_focus("edit-copy-quantities".to_owned());
    };
    let sheet_preset =
        RwSignal::new(sheet_preset_for_size(request.get_untracked().parent_sheet_size).to_owned());
    view! {
        <section id="gang-setup-panel" class="gang-panel gang-setup" class:rail-inactive=move || active_rail.get() != RailPanel::Setup role=move || preview_is_rail_tab.get().then_some("tabpanel") aria-labelledby=move || preview_is_rail_tab.get().then_some(RailPanel::Setup.tab_id()) aria-label="Print setup">
            <nav class="setup-stepper" aria-label="Print setup progress">
                <ol>
                    {SetupStep::ALL.into_iter().map(|step| view! {
                        <li class:reached=move || setup_step_is_reached(step, furthest_reached.get()) && active_step.get() != step>
                            <button
                                type="button"
                                aria-current=move || (active_step.get() == step).then_some("step")
                                disabled=move || {
                                    let current = active_step.get();
                                    !setup_step_is_reached(step, furthest_reached.get())
                                        || (step.position() > current.position()
                                            && (issue().is_some()
                                                || (step == SetupStep::Bleed
                                                    && !current_request_has_ready_layout())))
                                }
                                on:click=move |_| {
                                    let current = active_step.get_untracked();
                                    let forward_is_blocked = step.position() > current.position()
                                        && (issue().is_some()
                                            || (step == SetupStep::Bleed
                                                && !current_request_has_ready_layout()));
                                    if !forward_is_blocked {
                                        activate_setup_step(active_step, step);
                                    }
                                }
                            >
                                <span class="setup-step-label">{step.label()}</span>
                            </button>
                        </li>
                    }).collect_view()}
                </ol>
            </nav>
            <fieldset class="gang-setup-fields" disabled=move || busy.get()>
                <Show when=move || active_step.get() == SetupStep::Pdf>
                <div class="setup-step-panel" id="setup-step-pdf">
                <h3 id="setup-pdf-title" class="visually-hidden setup-step-focus-heading" tabindex="-1">"Size"</h3>
                <super::finished::FinishedSizeControls request finished_size_revision />
                <section class="impose-source-summary" aria-label="Artwork analysis">
                    <dl class="source-fact-row" aria-label="Artwork page count">
                        <div><dt>"Pages"</dt><dd>{move || analysis().map(|value| value.page_count.to_string()).unwrap_or_default()}</dd></div>
                    </dl>
                    <details><summary>"Detected artwork details"</summary>
                    <dl>
                        <div><dt>"PDF page size"</dt><dd>{move || analysis().map(|value| size_label(value.source_pdf_size)).unwrap_or_default()}</dd></div>
                        <div><dt>"Bleed"</dt><dd>{move || bleed_status(&request.get(), analysis().as_ref())}</dd></div>
                    </dl>
                    </details>
                    <Show when=move || analysis().is_some_and(|value| value.orientation_adjusted_pages > 0)>
                        <p class="status-message" role="status">
                            <strong>"Orientation corrected automatically."</strong>
                            {move || analysis().map(|value| {
                                let count = value.orientation_adjusted_pages;
                                let page = if count == 1 { "page" } else { "pages" };
                                format!(" Rotated {count} {page} to match the first page. No manual correction is needed.")
                            }).unwrap_or_default()}
                        </p>
                    </Show>
                    <Show when=move || analysis().is_some_and(|value| value.page_count > 1)>
                        <div>
                            <p class="control-label">"How should these pages print?"</p>
                            <div class="impose-mode-grid rail-choice-grid" role="group" aria-label="PDF page handling">
                                <button type="button" class="impose-mode-button option-card" aria-pressed=move || aria_bool(request.get().imposition_mode == ImpositionMode::Repeat) on:click=move |_| set_imposition_mode(request, quantity_drafts, ImpositionMode::Repeat)><span>"Repeat pages"</span><small>"Choose copies for each page or page pair."</small></button>
                                <button type="button" class="impose-mode-button option-card" aria-pressed=move || aria_bool(request.get().imposition_mode == ImpositionMode::Unique) on:click=move |_| set_imposition_mode(request, quantity_drafts, ImpositionMode::Unique)><span>"Use each page once"</span><small>"Keep pages in order across sheets."</small></button>
                            </div>
                        </div>
                    </Show>
                </section>
                </div>
                </Show>

                <Show when=move || active_step.get() == SetupStep::Job>
                <div class="setup-step-panel" id="setup-step-job">
                <section class="impose-primary-controls" aria-labelledby="job-settings-title">
                    <div class="control-section-head"><h3 id="job-settings-title" tabindex="-1">"Quantity and sheet"</h3><p>"Your finished size is set in the first setup step."</p></div>
                    <Show when=move || request.get().imposition_mode == ImpositionMode::Repeat>
                        <section class="repeat-quantity-section" aria-labelledby="repeat-quantity-title">
                            <div class="repeat-quantity-heading">
                                <div>
                                    <h3 id="repeat-quantity-title">"Copy quantities"</h3>
                                    <p>{move || {
                                        let current=request.get();
                                        let values=current.impression_quantities.unwrap_or_default();
                                        let unit=if current.sides==Sides::Double { "page pairs" } else { "pages" };
                                        let detail=values.first().filter(|first| values.iter().all(|value| value==*first)).map(|value|format!("{value} each")).unwrap_or_else(||"custom amounts".into());
                                        format!("{} {unit} · {detail}", values.len())
                                    }}</p>
                                </div>
                                <strong aria-live="polite">{move || format!("{} total", request.get().quantity_requested)}</strong>
                            </div>
                            <Show when=move || request.get().source_page_count.unwrap_or(1) == 1>
                                <NumberField id="single-page-quantity" label="Quantity" value=Signal::derive(move || request.get().impression_quantities.and_then(|values| values.first().copied()).unwrap_or(1) as f64) min=0.0 max=10000.0 step=1.0 on_change=UnsyncCallback::new(move |value| request.update(|current| quantity_drafts.update(|drafts| update_repeat_quantity(current, drafts, 0, value as usize)))) />
                            </Show>
                            <Show when=move || { request.get().source_page_count.unwrap_or(1) > 1 }>
                            <button id="edit-copy-quantities" class="ghost-button edit-quantities-button" type="button" on:click=move |_| {
                                let seed=request.get_untracked().impression_quantities.and_then(|values| values.first().copied()).unwrap_or(1);
                                bulk_quantity.set(seed);
                                modal_error.set(None);
                                quantity_editor_open.set(true);
                                browser::show_modal(
                                    quantity_dialog,
                                    "bulk-copy-quantity".to_owned(),
                                    move |result| {
                                        if let Err(error) = result {
                                            quantity_editor_open.set(false);
                                            modal_error.set(Some(error));
                                            browser::restore_modal_focus("edit-copy-quantities".to_owned());
                                        }
                                    },
                                );
                            }>"Edit copy quantities"</button>
                            </Show>
                            <Show when=move || modal_error.get().is_some()><p class="field-error" role="alert">{move || modal_error.get().unwrap_or_default()}</p></Show>
                        </section>
                    </Show>
                    <section class="impose-sheet-controls">
                        <label class="field"><span>"Print sheet size"</span><select prop:value=move || sheet_preset.get() on:change=move |event| { let value=event_target_value(&event); sheet_preset.set(value.clone()); apply_sheet_preset(request, &value); }><option value="12x18">"12 × 18 in"</option><option value="8.5x11">"8.5 × 11 in"</option><option value="11x17">"11 × 17 in"</option><option value="13x19">"13 × 19 in"</option><option value="custom">"Custom size"</option></select></label>
                        <Show when=move || sheet_preset.get() == "custom">
                            <div class="field-grid two">
                                <NumberField label="Sheet width (in)" value=Signal::derive(move || request.get().parent_sheet_size.width) min=0.01 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |value| set_number("sheet-width", value)) />
                                <NumberField label="Sheet height (in)" value=Signal::derive(move || request.get().parent_sheet_size.height) min=0.01 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |value| set_number("sheet-height", value)) />
                            </div>
                        </Show>
                    </section>
                    <section class="printing-control"><label class="field"><span>"Printing"</span><select prop:value=move || sides_value(request.get().sides) on:change=move |event| set_sides(request, quantity_drafts, &event_target_value(&event))><option value="single">"Single-sided"</option><option value="double" disabled=move || !super::supports_duplex(request.get().source_page_count)>"Double-sided"</option></select></label></section>
                    <Show when=move || current_request_has_ready_layout()>
                        <p class="impose-result-summary" aria-live="polite">{move || current_layout().map(|value| {
                            let piece_label=if value.pieces_per_sheet==1 { "piece" } else { "pieces" };
                            let sheet_label=if value.sheets_required==1 { "sheet" } else { "sheets" };
                            format!("{} {piece_label} per sheet · {} {sheet_label} required",value.pieces_per_sheet,value.sheets_required)
                        }).unwrap_or_default()}</p>
                    </Show>
                </section>
                </div>
                </Show>

                <Show when=move || active_step.get() == SetupStep::Sheet>
                <div class="setup-step-panel" id="setup-step-layout">
                <div class="impose-primary-controls">
                <section class="n-up-control" aria-labelledby="arrangement-title">
                    <div class="control-section-head"><h3 id="arrangement-title" tabindex="-1">"Arrangement"</h3><p>"Choose automatic sheet capacity or an exact grid."</p></div>
                    <div class="impose-mode-grid rail-choice-grid" role="group" aria-label="Sheet arrangements">
                        <button type="button" class="impose-mode-button option-card" aria-pressed=move || aria_bool(request.get().layout_mode == LayoutMode::MaxPieces) on:click=move |_| request.update(|current| select_layout_mode(current, LayoutMode::MaxPieces, None))><span>"Max per sheet"</span><small>"Automatically fit the most finished pieces."</small></button>
                        <button type="button" class="impose-mode-button option-card" aria-pressed=move || aria_bool(request.get().layout_mode == LayoutMode::Manual) on:click=move |_| { let seed=layout.get_untracked().last_good().cloned(); request.update(|current| select_layout_mode(current, LayoutMode::Manual, seed.as_ref())); }><span>"Custom grid"</span><small>"Set exact rows and columns."</small></button>
                    </div>
                    <Show when=move || request.get().layout_mode == LayoutMode::Manual>
                        <div class="field-grid two">
                            <NumberField label="Rows" value=Signal::derive(move || request.get().manual.map(|value| value.rows).unwrap_or(1) as f64) min=1.0 max=100.0 step=1.0 on_change=UnsyncCallback::new(move |value| set_manual_axis(true, value as usize)) />
                            <NumberField label="Columns" value=Signal::derive(move || request.get().manual.map(|value| value.columns).unwrap_or(1) as f64) min=1.0 max=100.0 step=1.0 on_change=UnsyncCallback::new(move |value| set_manual_axis(false, value as usize)) />
                        </div>
                    </Show>
                    <Show when=move || current_request_has_ready_layout()>
                        <p class="impose-result-summary layout-result-summary" aria-live="polite">{move || current_layout().map(|value| {
                            let piece_label=if value.pieces_per_sheet==1 { "piece" } else { "pieces" };
                            format!("{} × {} grid · {} {piece_label} per sheet",value.columns,value.rows,value.pieces_per_sheet)
                        }).unwrap_or_default()}</p>
                    </Show>
                </section>
                    <div class="production-details advanced-layout-details" class:open=move || advanced_sheet_settings_open.get()>
                        <button
                            class="text-button"
                            type="button"
                            aria-label=move || if advanced_sheet_settings_open.get() { "Hide advanced sheet settings" } else { "Advanced sheet settings" }
                            aria-expanded=move || aria_bool(advanced_sheet_settings_open.get())
                            aria-controls="advanced-sheet-settings"
                            on:click=move |event| {
                                event.stop_propagation();
                                advanced_sheet_settings_open.update(|open| *open = !*open);
                            }
                        ><span>{move || if advanced_sheet_settings_open.get() { "Hide advanced sheet settings" } else { "Advanced sheet settings" }}</span><span class="disclosure-chevron" aria-hidden="true">"⌄"</span></button>
                        <Show when=move || advanced_sheet_settings_open.get()>
                        <div id="advanced-sheet-settings" class="field-grid advanced-layout-grid">
                            <NumberField label="Horizontal gutter (in)" value=Signal::derive(move || request.get().gutter.horizontal) min=0.0 max=100.0 step=0.001 on_change=UnsyncCallback::new(move |value| set_number("gutter-horizontal", value)) />
                            <NumberField label="Vertical gutter (in)" value=Signal::derive(move || request.get().gutter.vertical) min=0.0 max=100.0 step=0.001 on_change=UnsyncCallback::new(move |value| set_number("gutter-vertical", value)) />
                            <Show when=move || request.get().sides == Sides::Double>
                                <label class="field"><span>"Back-side flip"</span><select prop:value=move || request.get().duplex.as_ref().map(|value| if value.flip_edge == DuplexFlipEdge::LongEdge { "longEdge" } else { "shortEdge" }).unwrap_or("longEdge") on:change=move |event| request.update(|current| if let Some(duplex)=current.duplex.as_mut(){ duplex.flip_edge=if event_target_value(&event)=="shortEdge"{DuplexFlipEdge::ShortEdge}else{DuplexFlipEdge::LongEdge}; })><option value="longEdge">"Long edge"</option><option value="shortEdge">"Short edge"</option></select></label>
                                <label class="field checkbox-field"><input type="checkbox" prop:checked=move || request.get().duplex.as_ref().is_some_and(|value| value.rotate_back_180) on:change=move |event| request.update(|current| if let Some(duplex)=current.duplex.as_mut(){ duplex.rotate_back_180=event_target_checked(&event); }) /><span>"Rotate back side 180°"</span></label>
                            </Show>
                            <Show when=move || request.get().layout_mode == LayoutMode::Manual>
                                <label class="field"><span>"Item rotation"</span><select prop:value=move || request.get().manual.map(|value| value.rotation_degrees.to_string()).unwrap_or_default() on:change=move |event| { if let Ok(value)=event_target_value(&event).parse(){ request.update(|current| if let Some(manual)=current.manual.as_mut(){manual.rotation_degrees=value}) } }><option value="0">"0°"</option><option value="90">"90°"</option></select></label>
                                <label class="field checkbox-field"><input type="checkbox" prop:checked=move || request.get().manual.is_some_and(|value| value.margins.is_none()) on:change=move |event| toggle_centering(request, event_target_checked(&event), current_layout()) /><span>"Center grid automatically"</span></label>
                                <Show when=move || request.get().manual.is_some_and(|value| value.margins.is_some())>
                                    <MarginFields request />
                                </Show>
                            </Show>
                        </div>
                        </Show>
                    </div>
                </div>
                </div>
                </Show>

                <Show when=move || active_step.get() == SetupStep::Bleed>
                <div class="setup-step-panel" id="setup-step-bleed">
                <div class="impose-primary-controls">
                <section class="bleed-control" aria-labelledby="bleed-title">
                    <p class="bleed-status" class:manual=move || request.get().source_bleed_override.is_some() aria-live="polite">
                        <strong>{move || bleed_status(&request.get(), analysis().as_ref())}</strong>
                        <span>{move || if request.get().source_bleed_override.is_some() { "Using your PDF bleed amount." } else if analysis().is_some_and(|value| value.likely_bleed.detected) { "The PDF already extends past the cut line." } else { "Choose how artwork should cover the cut edge." }}</span>
                    </p>
                    <div class="bleed-source-setting">
                        <Show when=move || request.get().source_bleed_override.is_none() fallback=move || view! {
                            <div id="bleed-source-override" class="bleed-inline-field">
                                <NumberField label="PDF bleed per side (in)" value=Signal::derive(move || request.get().source_bleed_override.unwrap_or(0.125)) min=0.001 max=1.0 step=0.001 on_change=UnsyncCallback::new(move |value| set_number("source-bleed", value)) />
                                <button class="text-button" type="button" on:click=move |_| { let detected=analysis(); request.update(|current| { current.source_bleed_override=None; if let Some(value)=&detected { current.source_pdf_size=value.source_pdf_size; current.source_trim_box=value.trim_box; } }); }>{move || if analysis().is_some_and(|value| value.likely_bleed.detected) { "Use detected value" } else { "Clear manual amount" }}</button>
                            </div>
                        }>
                            <button class="bleed-override-button" type="button" aria-expanded="false" aria-controls="bleed-source-override" on:click=move |_| set_number("source-bleed", 0.125)><span>{move || if analysis().is_some_and(|value| value.likely_bleed.detected) { "Override amount" } else { "Enter bleed manually" }}</span><span class="disclosure-chevron" aria-hidden="true">"⌄"</span></button>
                        </Show>
                    </div>
                    <div class="bleed-handling-heading"><h3 id="bleed-title" tabindex="-1">"Edge artwork"</h3><p>"Choose how artwork should meet the cut line."</p></div>
                    <Show when=move || request.get().bleed_option == BleedOption::FitInside><p class="status-message" role="status"><strong>"Legacy mode: Fit within the cut size."</strong>" Choose an option below to replace it."</p></Show>
                    <div class="bleed-handling-grid" role="group" aria-label="Artwork at the cut line">
                        <button type="button" class="bleed-choice option-card" aria-pressed=move || aria_bool(request.get().bleed_option == BleedOption::UseAsIs) on:click=move |_| request.update(|current| current.bleed_option=BleedOption::UseAsIs)><span class="bleed-choice-copy"><strong>"Keep fitted placement"</strong><small>"Use only the bleed already in the PDF."</small></span></button>
                        <button type="button" class="bleed-choice option-card" aria-pressed=move || aria_bool(request.get().bleed_option == BleedOption::ScaleToBleed) on:click=move |_| request.update(|current| current.bleed_option=BleedOption::ScaleToBleed)><span class="bleed-choice-copy"><strong>"Scale to add bleed"</strong><small>"Enlarge the fitted artwork uniformly, preserving its crop position. Contain may retain borders."</small></span></button>
                    </div>
                    <Show when=move || request.get().bleed_option == BleedOption::ScaleToBleed><div class="bleed-inline-field bleed-extension-field"><NumberField label="Extend per side (in)" value=Signal::derive(move || request.get().created_bleed_amount) min=0.001 max=1.0 step=0.001 on_change=UnsyncCallback::new(move |value| set_number("created-bleed", value)) /></div></Show>
                </section>
                </div>
                </div>
                </Show>
            </fieldset>
            <Show when=move || quantity_editor_open.get()>
                <dialog
                    node_ref=quantity_dialog
                    id="quantity-editor-dialog"
                    class="quantity-dialog"
                    aria-labelledby="quantity-dialog-title"
                    aria-describedby="quantity-dialog-help"
                    on:cancel=move |event: web_sys::Event| {
                        event.prevent_default();
                        close_quantity_editor();
                    }
                    on:keydown=move |event: web_sys::KeyboardEvent| {
                        if event.key() != "Tab" {
                            return;
                        }
                        let active_id = web_sys::window()
                            .and_then(|window| window.document())
                            .and_then(|document| document.active_element())
                            .map(|element| element.id())
                            .unwrap_or_default();
                        let destination = if event.shift_key() && active_id == "quantity-editor-close" {
                            Some("quantity-editor-done")
                        } else if !event.shift_key() && active_id == "quantity-editor-done" {
                            Some("quantity-editor-close")
                        } else {
                            None
                        };
                        if let Some(destination) = destination {
                            event.prevent_default();
                            browser::focus_element(destination);
                        }
                    }
                >
                    <header class="quantity-dialog-head">
                        <div><p class="eyebrow">"Print job"</p><h2 id="quantity-dialog-title">"Edit copy quantities"</h2></div>
                        <button id="quantity-editor-close" class="quantity-dialog-close" type="button" aria-label="Close copy quantity editor" on:click=move |_| close_quantity_editor()><span aria-hidden="true">"×"</span></button>
                    </header>
                    <p id="quantity-dialog-help">{move || if request.get().sides==Sides::Double { "Set a default for every page pair, then adjust exceptions below." } else { "Set a default for every page, then adjust exceptions below." }}</p>
                    <div class="quantity-bulk-editor">
                        <label class="field"><span>"Copies for all"</span><NumberInput id="bulk-copy-quantity" label="Copies for all" local_validity=invalid_quantity_inputs value=Signal::derive(move || bulk_quantity.get() as f64) min=0.0 max=10000.0 step=1.0 on_change=UnsyncCallback::new(move |value| bulk_quantity.set(value as usize)) /></label>
                        <button class="ghost-button" type="button" disabled=move || invalid_quantity_inputs.read().contains("Copies for all") on:click=move |_| {
                            if invalid_quantity_inputs.read_untracked().contains("Copies for all") { return; }
                            request.update(|current| quantity_drafts.update(|drafts| set_all_repeat_quantities(current,drafts,bulk_quantity.get_untracked())));
                            // Applying a default replaces individual drafts even
                            // when their committed quantities already match it.
                            quantity_apply_revision.update(|revision| *revision = revision.wrapping_add(1));
                            invalid_quantity_inputs.update(|invalid| invalid.clear());
                            close_quantity_editor();
                        }>"Apply to all"</button>
                    </div>
                    <div class="quantity-dialog-total" aria-live="polite"><span>"Total impressions"</span><strong>{move || request.get().quantity_requested}</strong></div>
                    <div class="quantity-dialog-list" aria-label="Individual copy quantities">
                        <For each=move || { let current=request.get(); let revision=quantity_apply_revision.get(); current.impression_quantities.unwrap_or_default().into_iter().enumerate().map(|(index, quantity)| (format!("{:?}-{}-{revision}", current.sides, index + 1), index, quantity)).collect::<Vec<_>>() } key=|(key, _, _)| key.clone() children=move |(_, index, _)| {
                            let label = if request.get_untracked().sides == Sides::Double { format!("Pages {}–{}", index * 2 + 1, index * 2 + 2) } else { format!("Page {}", index + 1) };
                            let input_label=format!("{label} copies");
                            view! { <label class="quantity-dialog-row"><span>{label}</span><NumberInput label=input_label local_validity=invalid_quantity_inputs value=Signal::derive(move || request.get().impression_quantities.as_ref().and_then(|values| values.get(index)).copied().unwrap_or(0) as f64) min=0.0 max=10000.0 step=1.0 on_change=UnsyncCallback::new(move |value| request.update(|current| quantity_drafts.update(|drafts| update_repeat_quantity(current, drafts, index, value as usize)))) /></label> }
                        } />
                    </div>
                    <footer class="quantity-dialog-actions"><button id="quantity-editor-done" class="primary-button" type="button" aria-disabled=move || aria_bool(!invalid_quantity_inputs.read().is_empty()) on:click=move |_| { if invalid_quantity_inputs.read_untracked().is_empty() { close_quantity_editor(); } }>"Done"</button></footer>
                </dialog>
            </Show>
            <Show when=move || issue().is_some()><p id="imposition-setup-error" class="field-error" role="alert">{move || issue().unwrap_or_default()}</p></Show>
            <Show when=move || active_step.get() == SetupStep::Sheet && issue().is_none() && !current_request_has_ready_layout()>
                <p id="imposition-layout-readiness" class="field-help" role="status">
                    {move || match layout.get() {
                        LayoutLifecycle::Loading { .. } | LayoutLifecycle::Empty => "Updating the sheet layout…",
                        LayoutLifecycle::Failed { .. } | LayoutLifecycle::Invalid { .. } => "Adjust the layout settings or retry before continuing.",
                        LayoutLifecycle::Ready { .. } => "Waiting for the current sheet layout…",
                    }}
                </p>
            </Show>
            <div class="setup-step-actions">
                <Show when=move || active_step.get() != SetupStep::Pdf>
                    <button class="text-button setup-back-button" type="button" on:click=move |_| { let next=match active_step.get_untracked() { SetupStep::Pdf | SetupStep::Job => SetupStep::Pdf, SetupStep::Sheet => SetupStep::Job, SetupStep::Bleed => SetupStep::Sheet }; activate_setup_step(active_step, next); }>"Back"</button>
                </Show>
                <Show when=move || active_step.get() != SetupStep::Bleed>
                    <button
                        class="primary-button setup-continue-button"
                        type="button"
                        disabled=move || issue().is_some() || (active_step.get() == SetupStep::Sheet && !current_request_has_ready_layout())
                        aria-describedby=move || if issue().is_some() {
                            Some("imposition-setup-error")
                        } else if active_step.get() == SetupStep::Sheet && !current_request_has_ready_layout() {
                            Some("imposition-layout-readiness")
                        } else {
                            None
                        }
                        on:click=move |_| {
                            if active_step.get_untracked() == SetupStep::Sheet
                                && !current_request_has_ready_layout()
                            {
                                return;
                            }
                            continue_setup(active_step, furthest_reached, issue());
                        }
                    >
                        {move || match active_step.get() { SetupStep::Pdf => "Continue to copies", SetupStep::Job => "Continue to layout", SetupStep::Sheet | SetupStep::Bleed => "Continue to bleed" }}
                    </button>
                </Show>
                <Show when=move || active_step.get() == SetupStep::Bleed>
                    <button id="gang-download-pdf" class="primary-button setup-download-button" type="button" disabled=move || !can_export() || exporting() on:click=move |_| on_download.run(())>{move || if exporting() { "Preparing PDF…" } else if matches!(export.get(), ExportLifecycle::Failed { .. }) { "Retry download" } else { "Download PDF" }}</button>
                    {move || match export.get() {
                        ExportLifecycle::Exporting { .. } => view! { <div class="setup-download-feedback setup-download-progress" role="status"><ProgressBar progress=export_progress /><button class="ghost-button" type="button" on:click=move |_| on_cancel_export.run(())>"Cancel"</button></div> }.into_any(),
                        ExportLifecycle::Complete { filename } => view! { <p class="export-notice setup-download-feedback" role="status">{format!("{filename} was sent to your browser.")}</p> }.into_any(),
                        ExportLifecycle::Failed { message } => view! { <p class="status-message setup-download-feedback" role="alert">{format!("{message} Your setup is still available; retry the download.")}</p> }.into_any(),
                        ExportLifecycle::Cancelled => view! { <p class="status-message setup-download-feedback" role="status">"Export cancelled. Your setup is still ready."</p> }.into_any(),
                        ExportLifecycle::Idle => ().into_any(),
                    }}
                </Show>
            </div>
        </section>
    }
}

#[component]
pub(super) fn NumberField(
    #[prop(default = "")] id: &'static str,
    label: &'static str,
    value: Signal<f64>,
    min: f64,
    max: f64,
    step: f64,
    on_change: UnsyncCallback<f64>,
    #[prop(optional)] empty: Option<Signal<bool>>,
) -> impl IntoView {
    view! { <label class="field"><span>{label}</span><NumberInput id label value min max step on_change empty=empty /></label> }
}

#[derive(Clone, Copy)]
struct NumericInputValidity(RwSignal<HashSet<String>>);

/// Keep incomplete numeric drafts visible without silently submitting an old value.
#[component]
fn NumberInput(
    #[prop(into)] label: String,
    value: Signal<f64>,
    min: f64,
    max: f64,
    step: f64,
    on_change: UnsyncCallback<f64>,
    #[prop(default = "")] id: &'static str,
    #[prop(optional)] local_validity: Option<RwSignal<HashSet<String>>>,
    #[prop(optional_no_strip)] empty: Option<Signal<bool>>,
) -> impl IntoView {
    let validities = [
        Some(expect_context::<NumericInputValidity>().0),
        local_validity,
    ];
    let error_id = StoredValue::new(format!("impose-number-{}-error", label.replace(' ', "-")));
    let label = StoredValue::new(label);
    let draft = RwSignal::new(value.get_untracked().to_string());
    let committed = Memo::new(move |_| value.get());
    let input_node = NodeRef::<html::Input>::new();
    let parse = move |text: &str| parse_numeric_input(text, min, max, step);
    Effect::new(move |_| {
        let next = committed.get();
        let is_empty = empty.is_some_and(|empty| empty.get());
        let current_text = input_node
            .get()
            .map(|input| input.value())
            .unwrap_or_default();
        if is_empty {
            if let Some(input) = input_node.get() {
                input.set_value("");
            }
        } else if !numeric_draft_matches_value(&current_text, next, min, max, step) {
            draft.set(next.to_string());
            if let Some(input) = input_node.get() {
                input.set_value(&next.to_string());
            }
        }
        for validity in validities.into_iter().flatten() {
            validity.update(|invalid| {
                invalid.remove(&label.get_value());
            });
        }
    });
    on_cleanup(move || {
        for validity in validities.into_iter().flatten() {
            validity.try_update(|invalid| {
                invalid.remove(&label.get_value());
            });
        }
    });
    let invalid = move || parse(&draft.get()).is_none();
    view! {
        <input node_ref=input_node id=(!id.is_empty()).then_some(id) type="number" aria-label=label.get_value() min=min max=max step=step
            value=value.get_untracked().to_string() aria-invalid=move || aria_bool(invalid())
            aria-describedby=move || invalid().then(|| error_id.get_value())
            on:input=move |event| {
                let text = event_target_value(&event);
                let parsed = parse(&text);
                draft.set(text);
                for validity in validities.into_iter().flatten() {
                    validity.update(|invalid| {
                        if parsed.is_some() { invalid.remove(&label.get_value()); }
                        else { invalid.insert(label.get_value()); }
                    });
                }
                if let Some(number) = parsed { on_change.run(number); }
            }
        />
        <Show when=invalid>
            <small id=error_id.get_value() class="field-error" role="alert">{format!("Enter {} from {min} to {max}.", if step == 1.0 { "a whole number" } else { "a number" })}</small>
        </Show>
    }
}

#[component]
fn MarginFields(request: RwSignal<LayoutRequest>) -> impl IntoView {
    view! {
        <NumberField label="Top margin (in)" value=Signal::derive(move || request.get().manual.and_then(|value| value.margins).map(|value| value.top).unwrap_or(0.0)) min=0.0 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |value| set_margin(request,"top",value)) />
        <NumberField label="Right margin (in)" value=Signal::derive(move || request.get().manual.and_then(|value| value.margins).map(|value| value.right).unwrap_or(0.0)) min=0.0 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |value| set_margin(request,"right",value)) />
        <NumberField label="Bottom margin (in)" value=Signal::derive(move || request.get().manual.and_then(|value| value.margins).map(|value| value.bottom).unwrap_or(0.0)) min=0.0 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |value| set_margin(request,"bottom",value)) />
        <NumberField label="Left margin (in)" value=Signal::derive(move || request.get().manual.and_then(|value| value.margins).map(|value| value.left).unwrap_or(0.0)) min=0.0 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |value| set_margin(request,"left",value)) />
    }
}

fn set_imposition_mode(
    request: RwSignal<LayoutRequest>,
    drafts: RwSignal<RepeatQuantityDrafts>,
    mode: ImpositionMode,
) {
    request.update(|current| {
        drafts.update(|drafts| set_imposition_mode_preserving_repeat_drafts(current, drafts, mode));
    });
}

fn set_sides(
    request: RwSignal<LayoutRequest>,
    drafts: RwSignal<RepeatQuantityDrafts>,
    value: &str,
) {
    request.update(|current| {
        let next = if value == "double" && super::supports_duplex(current.source_page_count) {
            Sides::Double
        } else {
            Sides::Single
        };
        drafts.update(|drafts| set_sides_preserving_repeat_drafts(current, drafts, next));
    })
}

fn apply_sheet_preset(request: RwSignal<LayoutRequest>, value: &str) {
    let size = match value {
        "8.5x11" => Some((8.5, 11.0)),
        "11x17" => Some((11.0, 17.0)),
        "13x19" => Some((13.0, 19.0)),
        "12x18" => Some((12.0, 18.0)),
        _ => None,
    };
    if let Some((width, height)) = size {
        request.update(|current| current.parent_sheet_size = SizeInches { width, height });
    }
}

fn sheet_preset_for_size(size: SizeInches) -> &'static str {
    const PRESETS: [(&str, f64, f64); 4] = [
        ("8.5x11", 8.5, 11.0),
        ("11x17", 11.0, 17.0),
        ("13x19", 13.0, 19.0),
        ("12x18", 12.0, 18.0),
    ];
    PRESETS
        .into_iter()
        .find_map(|(name, width, height)| {
            ((size.width - width).abs() < 0.001 && (size.height - height).abs() < 0.001)
                .then_some(name)
        })
        .unwrap_or("custom")
}

fn sides_value(value: Sides) -> &'static str {
    if value == Sides::Double {
        "double"
    } else {
        "single"
    }
}
fn toggle_centering(
    request: RwSignal<LayoutRequest>,
    centered: bool,
    layout: Option<LayoutResult>,
) {
    request.update(|current| {
        if let Some(manual) = current.manual.as_mut() {
            manual.margins = if centered {
                None
            } else {
                Some(layout.map(|value| value.margins).unwrap_or_default())
            }
        }
    })
}
fn set_margin(request: RwSignal<LayoutRequest>, side: &str, value: f64) {
    request.update(|current| {
        if let Some(margins) = current
            .manual
            .as_mut()
            .and_then(|manual| manual.margins.as_mut())
        {
            match side {
                "top" => margins.top = value,
                "right" => margins.right = value,
                "bottom" => margins.bottom = value,
                "left" => margins.left = value,
                _ => {}
            }
        }
    })
}
fn bleed_status(request: &LayoutRequest, analysis: Option<&PdfAnalysis>) -> String {
    if let Some(value) = request.source_bleed_override {
        format!("Manual · {value} in per side")
    } else if analysis.is_some_and(|value| value.likely_bleed.detected) {
        format!(
            "Detected · {} in per side",
            analysis
                .map(|value| value.likely_bleed.amount_per_side)
                .unwrap_or(0.0)
        )
    } else {
        "No uniform bleed detected".into()
    }
}
