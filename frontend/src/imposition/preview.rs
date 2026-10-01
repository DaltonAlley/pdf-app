//! Sheet preview presentation and artwork cache lifecycle.

use std::{collections::HashSet, sync::Arc};

use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::{browser::BrowserCancellation, presentation::aria_bool};

use super::model::{effective_artwork_fit, ArtworkFitMode, OrientationPreference};
use super::{
    browser as impose_browser, desired_artwork_box, duplex_mirror_axes, gutter_bands,
    presented_preview_view, preview_artwork_image, preview_artwork_rect, preview_cache_window,
    preview_clip_rect, preview_failure_availability, preview_layout_identity,
    preview_loading_label, preview_navigation_label, preview_page_number, preview_page_numbers,
    preview_placement_key, preview_protected_pages, preview_source_image_rect, raster_artwork_box,
    should_settle_preview_navigation, size_label, view::RailPanel, BatchAttempt, LayoutLifecycle,
    LayoutResult, PdfAnalysis, PiecePlacement, PreparedPdfSource, PreviewImageRect,
    PreviewLifecycle, PreviewPlan, PreviewRect, PreviewScheduler, PreviewSide, PreviewViewIdentity,
    RepeatQuantityDrafts,
};

#[derive(Clone)]
struct DisplayPlacement {
    key: String,
    page: Option<usize>,
    image: Option<PreviewImageRect>,
    index: usize,
    finished_width: f64,
    finished_height: f64,
    art: PreviewRect,
    clip: PreviewRect,
    cut_x: f64,
    cut_y: f64,
}

#[derive(Clone)]
struct PreviewPresentation {
    view: PreviewViewIdentity,
    layout: Arc<LayoutResult>,
    placements: Arc<Vec<DisplayPlacement>>,
}

impl PreviewPresentation {
    fn new(
        view: PreviewViewIdentity,
        layout: Arc<LayoutResult>,
        source_id: &str,
        analysis: &PdfAnalysis,
    ) -> Self {
        let placements = layout
            .placements
            .iter()
            .map(|item| {
                let page = preview_page_number(&layout, item.index, view.side, view.sheet_index);
                let mut placement = display_placement(
                    source_id,
                    analysis,
                    &layout,
                    item,
                    view.side,
                    view.sheet_index,
                    page,
                );
                placement
                    .key
                    .push_str(&format!(":{}", view.retry_generation));
                placement
            })
            .collect();
        Self {
            view,
            layout,
            placements: Arc::new(placements),
        }
    }
}

#[derive(Clone)]
struct PreviewPageImage {
    key: String,
    page: usize,
    url: String,
    view: PreviewViewIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PreviewCacheLifecycle {
    Idle,
    Loading {
        completed: usize,
        total: usize,
    },
    Ready,
    Failed {
        completed: usize,
        total: usize,
        message: String,
    },
}

#[derive(Clone, Copy)]
pub(super) struct PreviewToolbarState {
    pub(super) side: RwSignal<PreviewSide>,
    pub(super) sheet_index: RwSignal<usize>,
    pub(super) show_cut: RwSignal<bool>,
    pub(super) show_bleed: RwSignal<bool>,
    pub(super) show_gutters: RwSignal<bool>,
    pub(super) show_paths: RwSignal<bool>,
    pub(super) zoom: RwSignal<f64>,
}

impl PreviewToolbarState {
    pub(super) fn new() -> Self {
        Self {
            side: RwSignal::new(PreviewSide::Front),
            sheet_index: RwSignal::new(0_usize),
            show_cut: RwSignal::new(true),
            show_bleed: RwSignal::new(true),
            show_gutters: RwSignal::new(false),
            show_paths: RwSignal::new(false),
            zoom: RwSignal::new(1.0),
        }
    }
}

#[component]
pub(super) fn ImposeWorkspaceToolbar(
    request: RwSignal<super::model::LayoutRequest>,
    layout: RwSignal<LayoutLifecycle>,
    preview_cache: RwSignal<impose_browser::PreviewUrlCache>,
    selected_page: RwSignal<usize>,
    busy: RwSignal<bool>,
    exporting: RwSignal<bool>,
    active_rail: RwSignal<RailPanel>,
    previous_rail: RwSignal<RailPanel>,
    quantity_drafts: RwSignal<RepeatQuantityDrafts>,
    finished_size_revision: RwSignal<u64>,
    preview: PreviewToolbarState,
) -> impl IntoView {
    let _ = preview_cache;
    let sheet_count = move || {
        layout.with(|state| {
            state
                .last_good()
                .map(|value| value.sheets_required.max(1))
                .unwrap_or(1)
        })
    };
    let has_duplex = move || {
        layout.with(|state| {
            state
                .last_good()
                .is_some_and(|value| value.duplex.is_some())
        })
    };
    let effective_side = move || {
        if has_duplex() {
            preview.side.get()
        } else {
            PreviewSide::Front
        }
    };
    let fit_label =
        move || match effective_artwork_fit(&request.get(), Some(selected_page.get())).mode {
            ArtworkFitMode::Cover => "Fill",
            ArtworkFitMode::Contain => "Fit",
            ArtworkFitMode::Stretch => "Stretch",
        };
    let impression_label = move || match request.get().orientation_preference {
        OrientationPreference::Upright => "Upright",
        OrientationPreference::QuarterTurn => "Quarter-turn",
        _ => "Auto",
    };
    let mixed_override_count = move || {
        let current = request.get();
        if current.source_page_count.unwrap_or(1) <= 1 {
            return 0;
        }
        current
            .page_overrides
            .iter()
            .filter(|value| value.artwork_fit.is_some() || value.finished_cut_size.is_some())
            .count()
    };
    let artwork_aria_label = move || {
        let overrides = mixed_override_count();
        let override_label = if overrides == 0 {
            String::new()
        } else if overrides == 1 {
            ", one mixed-artwork override".to_owned()
        } else {
            format!(", {overrides} mixed-artwork overrides")
        };
        format!(
            "Artwork settings: {} fitting, {} impression orientation{}",
            fit_label(),
            impression_label(),
            override_label
        )
    };
    view! {
        <section class="impose-workspace-toolbar" aria-label="Artwork and sheet preview controls">
            <div class="impose-workspace-toolbar-controls">
                <fieldset class="impose-toolbar-mutation-controls" prop:disabled=move || busy.get() || exporting.get()>
                <super::presets::PresetModules request quantity_drafts finished_size_revision toolbar=true />
                <Show when=move || effective_artwork_fit(&request.get(), Some(selected_page.get())).mode == ArtworkFitMode::Cover>
                    <button
                        id="impose-position-trigger"
                        class="text-button impose-position-trigger"
                        type="button"
                        on:click=move |_| {
                            let current = active_rail.get_untracked();
                            if current != RailPanel::Artwork {
                                previous_rail.set(current);
                            }
                            active_rail.set(RailPanel::Artwork);
                            crate::browser::focus_element_after_render("horizontal-crop-position".to_owned());
                        }
                    >"Position…"</button>
                </Show>
                <details
                    class="artwork-toolbar-disclosure"
                    on:keydown=move |event: web_sys::KeyboardEvent| {
                        if event.key() != "Escape" {
                            return;
                        }
                        event.prevent_default();
                        let Some(target) = event
                            .current_target()
                            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                        else {
                            return;
                        };
                        let summary = target
                            .first_element_child()
                            .and_then(|child| child.dyn_into::<web_sys::HtmlElement>().ok());
                        let _ = target.remove_attribute("open");
                        if let Some(summary) = summary {
                            let _ = summary.focus();
                        }
                    }
                >
                    <summary
                        id="impose-artwork-trigger"
                        class="artwork-toolbar-trigger"
                        aria-controls="impose-artwork-panel"
                        aria-label=artwork_aria_label
                    >
                        <span class="artwork-toolbar-trigger-copy">
                            <strong>"Artwork"</strong>
                            <span aria-hidden="true">"·"</span>
                            <span>{move || fit_label()}</span>
                            <span aria-hidden="true">"·"</span>
                            <span>{move || impression_label()}</span>
                        </span>
                        <Show when=move || { mixed_override_count() > 0 }>
                            <span class="artwork-toolbar-override" aria-live="polite">
                                {move || {
                                    let count = mixed_override_count();
                                    if count == 1 { "1 override".to_owned() } else { format!("{count} overrides") }
                                }}
                            </span>
                        </Show>
                    </summary>
                    <div id="impose-artwork-panel" class="artwork-toolbar-panel" role="region" aria-label="Artwork settings">
                        <div class="artwork-toolbar-quick-fields">
                            <label class="field"><span>"Artwork fitting"</span><select aria-label="Artwork fitting" prop:value=move || match effective_artwork_fit(&request.get(), Some(selected_page.get())).mode { ArtworkFitMode::Cover => "cover", ArtworkFitMode::Stretch => "stretch", ArtworkFitMode::Contain => "contain" } on:change=move |event| request.update(|current| { let mut next=effective_artwork_fit(current, Some(selected_page.get())); next.mode=match event_target_value(&event).as_str() { "cover" => ArtworkFitMode::Cover, "stretch" => ArtworkFitMode::Stretch, _ => ArtworkFitMode::Contain }; super::model::update_artwork_fit(current, Some(selected_page.get()), next); })><option value="contain">"Fit"</option><option value="stretch">"Stretch"</option><option value="cover">"Fill"</option></select></label>
                            <label class="field"><span>"Impression orientation"</span><select aria-label="Impression orientation" prop:value=move || match request.get().orientation_preference { OrientationPreference::Upright => "upright", OrientationPreference::QuarterTurn => "quarterTurn", _ => "auto" } on:change=move |event| request.update(|current| current.orientation_preference=match event_target_value(&event).as_str() { "upright" => OrientationPreference::Upright, "quarterTurn" => OrientationPreference::QuarterTurn, _ => OrientationPreference::Auto })><option value="auto">"Auto"</option><option value="upright">"Upright"</option><option value="quarterTurn">"Quarter-turn"</option></select></label>
                        </div>
                    </div>
                </details>
                </fieldset>
                <Show when=has_duplex>
                    <div class="duplex-preview-tabs" role="group" aria-label="Preview side">
                        <button type="button" aria-pressed=move || aria_bool(effective_side()==PreviewSide::Front) on:click=move |_| preview.side.set(PreviewSide::Front)>"Front"</button>
                        <button type="button" aria-pressed=move || aria_bool(effective_side()==PreviewSide::Back) on:click=move |_| preview.side.set(PreviewSide::Back)>"Back"</button>
                    </div>
                </Show>
                <Show when=move || { sheet_count() > 1 }>
                    <nav class="sheet-preview-nav" aria-label="Preview sheet navigation">
                        <button type="button" aria-label="Show previous sheet" disabled=move || preview.sheet_index.get()==0 on:click=move |_| preview.sheet_index.update(|value|*value=value.saturating_sub(1))>"Previous"</button>
                        <label>
                            <span>"Sheet"</span>
                            <input type="number" aria-label="Sheet number" min="1" max=sheet_count prop:value=move || preview.sheet_index.get().saturating_add(1) on:change=move |event| {
                                if let Ok(value)=event_target_value(&event).parse::<usize>() {
                                    let max=sheet_count();
                                    preview.sheet_index.set(value.saturating_sub(1).min(max.saturating_sub(1)));
                                }
                            } />
                            <span aria-live="polite">{move || format!("of {}",sheet_count())}</span>
                        </label>
                        <button type="button" aria-label="Show next sheet" disabled={move || preview.sheet_index.get().saturating_add(1) >= sheet_count()} on:click=move |_| preview.sheet_index.update(|value|*value=value.saturating_add(1).min(sheet_count().saturating_sub(1)))>"Next"</button>
                    </nav>
                </Show>
                <div class="preview-zoom-controls" role="group" aria-label="Preview zoom">
                    <button type="button" aria-pressed=move || aria_bool(preview.zoom.get() == 1.0) on:click=move |_| preview.zoom.set(1.0)>"Fit sheet"</button>
                    <button type="button" aria-pressed=move || aria_bool(preview.zoom.get() == 1.25) on:click=move |_| preview.zoom.set(1.25)>"100%"</button>
                </div>
            </div>
        </section>
    }
}

#[component]
pub(super) fn SheetPreview(
    source: RwSignal<Option<PreparedPdfSource>>,
    layout: RwSignal<LayoutLifecycle>,
    selected_page: RwSignal<usize>,
    preview_cache: RwSignal<impose_browser::PreviewUrlCache>,
    active_rail: RwSignal<RailPanel>,
    preview_is_rail_tab: RwSignal<bool>,
    toolbar: PreviewToolbarState,
) -> impl IntoView {
    let side = toolbar.side;
    let sheet_index = toolbar.sheet_index;
    let show_cut = toolbar.show_cut;
    let show_bleed = toolbar.show_bleed;
    let show_gutters = toolbar.show_gutters;
    let show_paths = toolbar.show_paths;
    let zoom = toolbar.zoom;
    let retry_generation = RwSignal::new(0_u64);
    let preparation = RwSignal::new(PreviewLifecycle::Idle);
    let presented = RwSignal::new(Option::<PreviewPresentation>::None);
    let cache_preparation = RwSignal::new(PreviewCacheLifecycle::Idle);
    let preview_cancel = StoredValue::new_local(Option::<BrowserCancellation>::None);
    let preview_work = RwSignal::new(PreviewScheduler::default());
    const NAVIGATION_SETTLE_MILLIS: u32 = 180;
    let placements = move || {
        presented
            .get()
            .map(|value| value.placements.as_ref().clone())
            .unwrap_or_default()
    };
    let preview_images = move || {
        presented
            .get()
            .map(|presented| {
                presented
                    .view
                    .pages
                    .iter()
                    .copied()
                    .filter_map(|page| {
                        let url = preview_cache.with(|cache| {
                            cache
                                .url_for_source(&presented.view.source_id, page)
                                .map(str::to_owned)
                        })?;
                        Some(PreviewPageImage {
                            key: url.clone(),
                            page,
                            url,
                            view: presented.view.clone(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    Effect::new(move |_| {
        let generation = retry_generation.get();
        preview_cancel.update_value(|active| {
            if let Some(controller) = active.take() {
                controller.abort();
            }
        });
        let Some(prepared) = source.get() else {
            preview_cache.update(impose_browser::PreviewUrlCache::clear);
            preparation.set(PreviewLifecycle::Idle);
            presented.set(None);
            preview_work.update(PreviewScheduler::invalidate);
            return;
        };
        let Some(current_layout) = layout
            .with(|state| state.last_good().cloned())
            .map(Arc::new)
        else {
            preparation.set(PreviewLifecycle::Idle);
            preview_work.update(PreviewScheduler::invalidate);
            return;
        };
        let requested_side = if current_layout.duplex.is_some() {
            side.get()
        } else {
            PreviewSide::Front
        };
        let selected_sheet = sheet_index.get();
        let requested_sheet = selected_sheet.min(current_layout.sheets_required.saturating_sub(1));
        if requested_sheet != selected_sheet {
            sheet_index.set(requested_sheet);
            return;
        }
        let Ok(layout_identity) = preview_layout_identity(&current_layout) else {
            preparation.set(PreviewLifecycle::Idle);
            preview_work.update(PreviewScheduler::invalidate);
            return;
        };
        let mut pages = preview_page_numbers(&current_layout, requested_side, requested_sheet);
        let selected = selected_page.get().min(prepared.analysis.page_count).max(1);
        if selected != selected_page.get_untracked() {
            selected_page.set(selected);
        }
        if !pages.contains(&selected) {
            pages.push(selected);
            pages.sort_unstable();
        }
        let view = PreviewViewIdentity {
            source_id: super::model::preview_raster_identity(
                &prepared.source_id,
                current_layout.source_bleed_override,
            ),
            layout_identity,
            pages: pages.clone(),
            retry_generation: generation,
            side: requested_side,
            sheet_index: requested_sheet,
            sheet_count: current_layout.sheets_required,
        };
        let target = PreviewPresentation::new(
            view.clone(),
            current_layout.clone(),
            &prepared.source_id,
            &prepared.analysis,
        );
        if presented.with_untracked(|current| {
            current
                .as_ref()
                .is_some_and(|current| current.view.source_id != view.source_id)
        }) {
            presented.set(None);
        }
        preview_cache.update(|cache| cache.select_source(&view.source_id));
        let protected = presented
            .with_untracked(|current| {
                preview_protected_pages(&view, current.as_ref().map(|value| &value.view))
            })
            .into_iter()
            .collect::<HashSet<_>>();
        let protected_result =
            preview_cache.try_update(|cache| cache.set_protected(&view.source_id, &protected));
        if !matches!(protected_result, Some(Ok(true))) {
            presented.set(None);
            let target_only = pages.iter().copied().collect::<HashSet<_>>();
            let target_result = preview_cache
                .try_update(|cache| cache.set_protected(&view.source_id, &target_only));
            if !matches!(target_result, Some(Ok(true))) {
                let message = match target_result {
                    Some(Err(message)) => message,
                    Some(Ok(false)) => {
                        "The artwork preview source changed before it could be displayed.".into()
                    }
                    None => "The artwork preview cache is unavailable.".into(),
                    Some(Ok(true)) => String::new(),
                };
                preparation.set(PreviewLifecycle::Failed {
                    view,
                    completed: 0,
                    message,
                });
                preview_work.update(PreviewScheduler::invalidate);
                return;
            }
        }
        preview_cache.update(|cache| cache.touch_visible(&pages));
        if pages.is_empty() {
            presented.set(Some(target));
            preparation.set(PreviewLifecycle::Ready { view });
            preview_work.update(PreviewScheduler::invalidate);
            return;
        }
        let completed = preview_cache.with_untracked(|cache| cache.completed(&pages));
        let missing = preview_cache.with_untracked(|cache| cache.missing(&pages));
        let chosen = presented.with_untracked(|current| {
            presented_preview_view(current.as_ref().map(|value| &value.view), &view, completed)
        });
        if chosen.as_ref() == Some(&view) {
            presented.set(Some(target.clone()));
        }
        let settle_navigation = preparation.with_untracked(|previous| {
            should_settle_preview_navigation(previous, &view, missing.len())
        });
        if missing.is_empty() {
            preparation.set(PreviewLifecycle::Ready { view: view.clone() });
        } else {
            preparation.set(PreviewLifecycle::loading(view.clone(), completed));
        }
        cache_preparation.set(PreviewCacheLifecycle::Idle);
        let cache_pages = preview_cache_window(&current_layout, requested_side, requested_sheet);
        let missing_prefetch = preview_cache.with_untracked(|cache| cache.missing(&cache_pages));
        let Ok(controller) = BrowserCancellation::new() else {
            if completed == pages.len() {
                cache_preparation.set(PreviewCacheLifecycle::Failed {
                    completed: cache_pages.len().saturating_sub(missing_prefetch.len()),
                    total: cache_pages.len(),
                    message: "The browser could not start background artwork caching.".into(),
                });
            } else {
                preparation.update(|state| {
                    state.record_failure(
                        &view,
                        completed,
                        "The browser could not start artwork preview rendering.".into(),
                    );
                });
            }
            preview_work.update(PreviewScheduler::invalidate);
            return;
        };
        let Some(foreground_plan) = preview_work
            .try_update(|work| work.schedule(view.clone(), &missing, &missing_prefetch))
        else {
            return;
        };
        preview_cancel.update_value(|active| *active = Some(controller.clone()));
        let source_id = prepared.source_id;
        let source_bleed_override = current_layout.source_bleed_override;
        spawn_local(async move {
            let commit = PreviewCommitContext {
                preparation,
                presented,
                target,
                cache: preview_cache,
                active: preview_cancel,
                view: view.clone(),
            };
            if settle_navigation {
                TimeoutFuture::new(NAVIGATION_SETTLE_MILLIS).await;
                if !preview_plan_is_active(
                    preview_work,
                    &foreground_plan,
                    preview_cancel,
                    &controller,
                ) {
                    return;
                }
            }
            let mut failed_pages = Vec::new();
            let mut last_error = String::new();
            for batch in foreground_plan.foreground() {
                if !preview_plan_is_active(
                    preview_work,
                    &foreground_plan,
                    preview_cancel,
                    &controller,
                ) {
                    return;
                }
                let result = request_preview_batch_with_retry(
                    &source_id,
                    batch,
                    source_bleed_override,
                    &controller,
                )
                .await;
                if !preview_plan_is_active(
                    preview_work,
                    &foreground_plan,
                    preview_cancel,
                    &controller,
                ) {
                    return;
                }
                match result {
                    Ok(rendered) => match commit.commit(&controller, rendered) {
                        Ok(true) => {}
                        Ok(false) => return,
                        Err(error) => {
                            failed_pages.extend(batch.iter().copied());
                            last_error = error;
                        }
                    },
                    Err(error) if !controller.is_cancelled() => {
                        failed_pages.extend(batch.iter().copied());
                        last_error = error;
                    }
                    Err(_) => return,
                }
            }
            if !preview_plan_is_active(preview_work, &foreground_plan, preview_cancel, &controller)
            {
                return;
            }
            if !failed_pages.is_empty() {
                failed_pages.sort_unstable();
                failed_pages.dedup();
                let completed = preview_cache.with_untracked(|cache| cache.completed(&view.pages));
                let page_label = failed_pages
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let message = format!(
                    "Could not load artwork preview {} {page_label}: {last_error}",
                    if failed_pages.len() == 1 {
                        "page"
                    } else {
                        "pages"
                    }
                );
                preparation.update(|state| state.record_failure(&view, completed, message));
                preview_cancel.update_value(|active| *active = None);
                preview_work.update(|work| {
                    work.finish(&foreground_plan);
                });
                return;
            }

            let total = cache_pages.len();
            let completed = preview_cache.with_untracked(|cache| cache.completed(&cache_pages));
            if foreground_plan.prefetch().is_empty() {
                cache_preparation.set(PreviewCacheLifecycle::Ready);
            } else {
                cache_preparation.set(PreviewCacheLifecycle::Loading { completed, total });
            }
            for batch in foreground_plan.prefetch() {
                if !preview_plan_is_active(
                    preview_work,
                    &foreground_plan,
                    preview_cancel,
                    &controller,
                ) {
                    return;
                }
                let result = request_preview_batch_with_retry(
                    &source_id,
                    batch,
                    source_bleed_override,
                    &controller,
                )
                .await;
                if !preview_plan_is_active(
                    preview_work,
                    &foreground_plan,
                    preview_cancel,
                    &controller,
                ) {
                    return;
                }
                let rendered = match result {
                    Ok(rendered) => rendered,
                    Err(message) if !controller.is_cancelled() => {
                        let completed =
                            preview_cache.with_untracked(|cache| cache.completed(&cache_pages));
                        cache_preparation.set(PreviewCacheLifecycle::Failed {
                            completed,
                            total,
                            message,
                        });
                        preview_cancel.update_value(|active| *active = None);
                        preview_work.update(|work| {
                            work.finish(&foreground_plan);
                        });
                        return;
                    }
                    Err(_) => return,
                };
                match insert_prefetch_batch(preview_cache, &view.source_id, rendered) {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(message) => {
                        let completed =
                            preview_cache.with_untracked(|cache| cache.completed(&cache_pages));
                        cache_preparation.set(PreviewCacheLifecycle::Failed {
                            completed,
                            total,
                            message,
                        });
                        preview_cancel.update_value(|active| *active = None);
                        preview_work.update(|work| {
                            work.finish(&foreground_plan);
                        });
                        return;
                    }
                }
                let completed = preview_cache.with_untracked(|cache| cache.completed(&cache_pages));
                cache_preparation.set(PreviewCacheLifecycle::Loading { completed, total });
            }
            let completed = preview_cache.with_untracked(|cache| cache.completed(&cache_pages));
            if completed == total {
                cache_preparation.set(PreviewCacheLifecycle::Ready);
            } else {
                cache_preparation.set(PreviewCacheLifecycle::Failed {
                    completed,
                    total,
                    message: "Some decoded artwork could not be retained within the preview cache budget.".into(),
                });
            }
            preview_cancel.update_value(|active| *active = None);
            preview_work.update(|work| {
                work.finish(&foreground_plan);
            });
        });
    });
    on_cleanup(move || {
        preview_cancel.update_value(|active| {
            if let Some(controller) = active.take() {
                controller.abort();
            }
        });
    });
    let vertical_gutters = move || {
        gutter_bands(
            placements()
                .iter()
                .flat_map(|value| [value.cut_x, value.cut_x + value.finished_width]),
            presented
                .get()
                .map(|value| value.layout.gutters.horizontal)
                .unwrap_or(0.0),
        )
    };
    let horizontal_gutters = move || {
        gutter_bands(
            placements()
                .iter()
                .flat_map(|value| [value.cut_y, value.cut_y + value.finished_height]),
            presented
                .get()
                .map(|value| value.layout.gutters.vertical)
                .unwrap_or(0.0),
        )
    };
    let active_guide_count = move || {
        [
            show_cut.get(),
            show_bleed.get(),
            show_gutters.get(),
            show_paths.get(),
        ]
        .into_iter()
        .filter(|visible| *visible)
        .count()
    };
    view! {
        <details id="gang-preview-panel" class="gang-preview-region" class:rail-inactive=move || active_rail.get() != RailPanel::Preview role=move || preview_is_rail_tab.get().then_some("tabpanel") aria-labelledby=move || preview_is_rail_tab.get().then_some(RailPanel::Preview.tab_id()) open=move || !preview_is_rail_tab.get() || active_rail.get() == RailPanel::Preview>
            <summary><span>"Preview"</span><small>{move || presented.get().map(|value| size_label(value.layout.parent_sheet_size)).unwrap_or_else(||"Waiting for layout".into())}</small></summary>
            <section class="gang-panel gang-preview" aria-label="Complete sheet preview">
                <div class="sheet-stage" aria-busy=move || aria_bool(matches!(layout.get(),LayoutLifecycle::Loading{..}) || matches!(preparation.get(),PreviewLifecycle::Loading{..}))>
                    <details class="preview-guide-palette">
                        <summary
                            aria-label=move || format!("Preview guides, {} active", active_guide_count())
                            title="Preview guides"
                        >
                            <span class="preview-guide-icon" aria-hidden="true"><i></i><i></i><i></i></span>
                            <span class="preview-guide-count" aria-hidden="true">{active_guide_count}</span>
                        </summary>
                        <div class="preview-guide-toggles" role="group" aria-label="Preview guides">
                            <header><strong>"Preview guides"</strong><span>{move || format!("{} active", active_guide_count())}</span></header>
                            <label><span class="guide-swatch guide-swatch-cut" aria-hidden="true"></span><span>"Final cut"</span><input type="checkbox" prop:checked=move ||show_cut.get() on:change=move |event|show_cut.set(event_target_checked(&event))/></label>
                            <label><span class="guide-swatch guide-swatch-bleed" aria-hidden="true"></span><span>"Bleed edge"</span><input type="checkbox" prop:checked=move ||show_bleed.get() on:change=move |event|show_bleed.set(event_target_checked(&event))/></label>
                            <label><span class="guide-swatch guide-swatch-gutter" aria-hidden="true"></span><span>"Gutters"</span><input type="checkbox" prop:checked=move ||show_gutters.get() on:change=move |event|show_gutters.set(event_target_checked(&event))/></label>
                            <label><span class="guide-swatch guide-swatch-path" aria-hidden="true"></span><span>"Cut paths"</span><input type="checkbox" prop:checked=move ||show_paths.get() on:change=move |event|show_paths.set(event_target_checked(&event))/></label>
                            <Show when=move || presented.get().is_some_and(|value| value.layout.duplex.is_some())>
                                <p class="duplex-preview-note">{move || presented.get().and_then(|value|value.layout.duplex.as_ref().map(|duplex|duplex.back_alignment.clone())).unwrap_or_default()}</p>
                            </Show>
                        </div>
                    </details>
                    <svg class="sheet-svg" style:transform=move || format!("scale({})", zoom.get()) viewBox=move || presented.get().map(|value|format!("0 0 {} {}",value.layout.parent_sheet_size.width,value.layout.parent_sheet_size.height)).unwrap_or_else(||"0 0 12 18".into()) preserveAspectRatio="xMidYMid meet" role="img" aria-label=move ||presented.get().map(|value|preview_navigation_label(value.view.sheet_index,value.layout.sheets_required,value.view.side)).unwrap_or_else(||"Imposition preview is loading".into())>
                        <defs><For each=preview_images key=|value: &PreviewPageImage|value.key.clone() children=move |preview| {
                            let failed_view=preview.view.clone();
                            let failed_url=preview.url.clone();
                            view!{<symbol id=format!("impose-page-{}",preview.page) viewBox="0 0 1 1" preserveAspectRatio="none"><image href=preview.url x="0" y="0" width="1" height="1" preserveAspectRatio="none" on:error=move |_|record_preview_decode_failed(preparation,preview_cache,&failed_view,preview.page,&failed_url)/></symbol>}
                        } /></defs>
                        <rect class="sheet-page" x="0" y="0" width=move ||presented.get().map(|value|value.layout.parent_sheet_size.width).unwrap_or(12.0) height=move ||presented.get().map(|value|value.layout.parent_sheet_size.height).unwrap_or(18.0)/>
                        <For each=placements key=|value|value.key.clone() children=move |placement| {
                            let clip_id=format!("impose-clip-{}",placement.index);
                            let art_clip_id=format!("impose-art-clip-{}",placement.index);
                            placement.page.zip(placement.image.clone()).map(|(page,image)| {
                                view!{<g><defs><clipPath id=clip_id.clone()><rect x=placement.clip.x y=placement.clip.y width=placement.clip.width height=placement.clip.height/></clipPath><clipPath id=art_clip_id.clone()><rect x=placement.art.x y=placement.art.y width=placement.art.width height=placement.art.height/></clipPath></defs><g clip-path=format!("url(#{clip_id})")><g clip-path=format!("url(#{art_clip_id})")><use class="piece-page" href=format!("#impose-page-{page}") x=image.rect.x y=image.rect.y width=image.rect.width height=image.rect.height transform=image.transform /></g></g><Show when=move ||show_bleed.get()><rect class="piece-art-boundary" x=placement.art.x y=placement.art.y width=placement.art.width height=placement.art.height/></Show><Show when=move ||show_cut.get()><rect class="piece-cut" x=placement.cut_x y=placement.cut_y width=placement.finished_width height=placement.finished_height/></Show></g>}
                            })
                        } />
                        <Show when=move ||show_gutters.get()>{move ||presented.get().map(|value|view!{<g>{vertical_gutters().into_iter().map(|band|view!{<rect class="piece-gutter-band" x=band.position y="0" width=band.size height=value.layout.parent_sheet_size.height/>}).collect_view()}{horizontal_gutters().into_iter().map(|band|view!{<rect class="piece-gutter-band" x="0" y=band.position width=value.layout.parent_sheet_size.width height=band.size/>}).collect_view()}</g>})}</Show>
                        <Show when=move ||show_paths.get()>{move ||presented.get().map(|value|cut_paths(&value.layout,&placements()))}</Show>
                    </svg>
                    {move || match cache_preparation.get() {
                        PreviewCacheLifecycle::Loading { completed, total } => view! { <div class="preview-cache-status" title="Caching artwork for later sheets"><span>{format!("Caching later sheets {completed}/{total}")}</span><progress aria-label="Background artwork caching progress" max=total value=completed></progress></div> }.into_any(),
                        PreviewCacheLifecycle::Failed { completed, total, message } => view! { <div class="preview-cache-status paused" role="status" title=format!("{message} Cached {completed} of {total} artwork pages.")><span>"Later sheets load when opened"</span></div> }.into_any(),
                        PreviewCacheLifecycle::Idle | PreviewCacheLifecycle::Ready => ().into_any(),
                    }}
                    {move || match preparation.get() {
                        PreviewLifecycle::Loading { completed, view } => {
                            let current = presented.get_untracked().map(|value| value.view);
                            view! { <div class="preview-update-status" role="status" aria-live="polite"><span>{preview_loading_label(&view,current.as_ref(),completed)}</span><progress aria-label="Artwork preview decoding progress" max=view.pages.len() value=completed></progress></div> }.into_any()
                        },
                        PreviewLifecycle::Failed { completed, view, message } => view! { <div class="status-message preview-failure-status" role="alert"><strong>"Artwork preview needs attention"</strong><p>{format!("{message} {} Your source and layout are still available.",preview_failure_availability(completed,view.pages.len()))}</p><button class="ghost-button" type="button" on:click=move |_|retry_generation.update(|value|*value=value.saturating_add(1))>"Retry artwork"</button></div> }.into_any(),
                        PreviewLifecycle::Ready { .. } | PreviewLifecycle::Idle => ().into_any(),
                    }}
                    <Show when=move ||matches!(layout.get(),LayoutLifecycle::Loading{..})><div class="preview-update-status" role="status">"Updating layout…"</div></Show>
                </div>
            </section>
        </details>
    }
}

pub(super) async fn request_preview_batch_with_retry(
    source_id: &str,
    pages: &[usize],
    source_bleed_override: Option<f64>,
    cancellation: &BrowserCancellation,
) -> Result<Vec<impose_browser::DecodedPreviewAsset>, String> {
    let mut attempt = BatchAttempt::new(pages);
    let first = impose_browser::request_preview_pages(
        source_id,
        pages,
        source_bleed_override,
        cancellation,
    )
    .await;
    let first_error = match first {
        Ok(rendered) => {
            return impose_browser::decode_preview_assets(rendered)
                .await
                .map_err(impose_browser::PreviewRequestError::into_message);
        }
        Err(error) if cancellation.is_cancelled() || !error.should_retry() => {
            return Err(error.into_message());
        }
        Err(error) => error,
    };
    let Some(retry_pages) = attempt.retry_missing(pages) else {
        return Err(first_error.into_message());
    };
    TimeoutFuture::new(250).await;
    if cancellation.is_cancelled() {
        return Err("Artwork preview loading was cancelled.".into());
    }
    let rendered = impose_browser::request_preview_pages(
        source_id,
        &retry_pages,
        source_bleed_override,
        cancellation,
    )
    .await
    .map_err(impose_browser::PreviewRequestError::into_message)?;
    impose_browser::decode_preview_assets(rendered)
        .await
        .map_err(impose_browser::PreviewRequestError::into_message)
}

fn preview_request_is_active(
    active: StoredValue<Option<BrowserCancellation>, LocalStorage>,
    controller: &BrowserCancellation,
) -> bool {
    active
        .try_with_value(|current| {
            current
                .as_ref()
                .is_some_and(|value| value.same_job(controller))
        })
        .unwrap_or(false)
}

fn preview_plan_is_active(
    scheduler: RwSignal<PreviewScheduler>,
    plan: &PreviewPlan,
    active: StoredValue<Option<BrowserCancellation>, LocalStorage>,
    controller: &BrowserCancellation,
) -> bool {
    preview_request_is_active(active, controller)
        && scheduler.with_untracked(|scheduler| scheduler.accepts(plan))
}

fn insert_prefetch_batch(
    cache: RwSignal<impose_browser::PreviewUrlCache>,
    source_id: &str,
    rendered: Vec<impose_browser::DecodedPreviewAsset>,
) -> Result<bool, String> {
    let mut insertion_error = None;
    let source_matches = cache
        .try_update(|cache| {
            for asset in rendered {
                let page = asset.page();
                match cache.insert_for_current_source(source_id, page, asset) {
                    Ok(true) => {}
                    Ok(false) => return false,
                    Err(error) => {
                        insertion_error = Some(error);
                        return true;
                    }
                }
            }
            true
        })
        .unwrap_or(false);
    match insertion_error {
        Some(error) => Err(error),
        None => Ok(source_matches),
    }
}

struct PreviewCommitContext {
    preparation: RwSignal<PreviewLifecycle>,
    presented: RwSignal<Option<PreviewPresentation>>,
    target: PreviewPresentation,
    cache: RwSignal<impose_browser::PreviewUrlCache>,
    active: StoredValue<Option<BrowserCancellation>, LocalStorage>,
    view: PreviewViewIdentity,
}

impl PreviewCommitContext {
    fn commit(
        &self,
        controller: &BrowserCancellation,
        rendered: Vec<impose_browser::DecodedPreviewAsset>,
    ) -> Result<bool, String> {
        if !preview_request_is_active(self.active, controller)
            || !self
                .preparation
                .with_untracked(|state| state.accepts_completion(&self.view))
        {
            return Ok(false);
        }
        if self
            .cache
            .with_untracked(|cache| cache.ensure_admissible(&rendered))
            .is_err()
        {
            // Retaining the old composed view is optional. If the two views together exceed a
            // hard cache bound, release the old view and retry admission for the requested sheet
            // before reporting that the requested sheet itself is too large.
            self.presented.set(None);
            let protected = self.view.pages.iter().copied().collect::<HashSet<_>>();
            self.cache
                .try_update(|cache| {
                    if !cache.set_protected(&self.view.source_id, &protected)? {
                        return Err::<(), String>(
                            "The artwork preview source changed before it could be displayed."
                                .into(),
                        );
                    }
                    cache.ensure_admissible(&rendered)
                })
                .unwrap_or_else(|| Err("The artwork preview cache is unavailable.".into()))?;
        }
        self.cache
            .try_update(|cache| {
                for asset in rendered {
                    let page = asset.page();
                    if !cache.insert_for_current_source(&self.view.source_id, page, asset)? {
                        return Err::<(), String>(
                            "The artwork preview source changed before it could be displayed."
                                .into(),
                        );
                    }
                }
                Ok(())
            })
            .unwrap_or_else(|| Err("The artwork preview cache is unavailable.".into()))?;
        let completed = self
            .cache
            .with_untracked(|cache| cache.completed(&self.view.pages));
        self.preparation
            .update(|state| state.record_completed(&self.view, completed));
        if completed == self.view.pages.len() {
            let protected = self.view.pages.iter().copied().collect::<HashSet<_>>();
            let retained = self
                .cache
                .try_update(|cache| cache.set_protected(&self.view.source_id, &protected))
                .unwrap_or_else(|| Err("The artwork preview cache is unavailable.".into()))?;
            if !retained {
                return Err(
                    "The artwork preview source changed before it could be displayed.".into(),
                );
            }
            self.presented.set(Some(self.target.clone()));
        }
        Ok(true)
    }
}

fn record_preview_decode_failed(
    preparation: RwSignal<PreviewLifecycle>,
    cache: RwSignal<impose_browser::PreviewUrlCache>,
    view: &PreviewViewIdentity,
    page: usize,
    failed_url: &str,
) {
    if !preparation.with_untracked(|state| state.accepts_failure(view)) {
        return;
    }
    let source_matches = cache
        .try_update(|cache| cache.remove_failed_url(&view.source_id, page, failed_url))
        .unwrap_or(false);
    if !source_matches {
        return;
    }
    let completed = cache.with_untracked(|cache| cache.completed(&view.pages));
    preparation.update(|state| {
        state.record_failure(
            view,
            completed,
            format!("The browser could not decode artwork preview page {page}."),
        );
    });
}

fn display_placement(
    source_id: &str,
    analysis: &PdfAnalysis,
    layout: &LayoutResult,
    item: &PiecePlacement,
    side: PreviewSide,
    sheet_index: usize,
    page: Option<usize>,
) -> DisplayPlacement {
    if let Some(plan) = page.and_then(|page| {
        layout
            .page_plans
            .iter()
            .find(|plan| plan.page_number == page)
    }) {
        let (cut, clip, image) = super::model::planned_preview_geometry(layout, item, plan, side);
        return DisplayPlacement {
            key: format!(
                "{}:{:?}",
                preview_placement_key(source_id, layout, item, side, sheet_index, page),
                plan
            ),
            page,
            image: Some(image),
            index: item.index,
            finished_width: cut.width,
            finished_height: cut.height,
            art: clip,
            clip,
            cut_x: cut.x,
            cut_y: cut.y,
        };
    }
    let mut art = preview_artwork_rect(layout, item);
    let mut clip = preview_clip_rect(layout, item);
    let mut cut_x = item.finished_x;
    let mut cut_y = item.finished_y;
    if side == PreviewSide::Back && layout.duplex.is_some() {
        let (mx, my) = duplex_mirror_axes(layout);
        let mirror = |rect: PreviewRect| PreviewRect {
            x: if mx {
                layout.parent_sheet_size.width - rect.x - rect.width
            } else {
                rect.x
            },
            y: if my {
                layout.parent_sheet_size.height - rect.y - rect.height
            } else {
                rect.y
            },
            ..rect
        };
        art = mirror(art);
        clip = mirror(clip);
        let cut = mirror(PreviewRect {
            x: cut_x,
            y: cut_y,
            width: item.finished_width,
            height: item.finished_height,
        });
        cut_x = cut.x;
        cut_y = cut.y;
    }
    let image = Some(preview_source_image_rect(
        preview_artwork_image(layout, art, side),
        raster_artwork_box(analysis),
        desired_artwork_box(layout, analysis),
    ));
    let key = preview_placement_key(source_id, layout, item, side, sheet_index, page);
    DisplayPlacement {
        key,
        page,
        image,
        index: item.index,
        finished_width: item.finished_width,
        finished_height: item.finished_height,
        art,
        clip,
        cut_x,
        cut_y,
    }
}

fn cut_paths(layout: &LayoutResult, placements: &[DisplayPlacement]) -> impl IntoView {
    let mixed = layout
        .page_plans
        .windows(2)
        .any(|pair| pair[0].finished_cut_size != pair[1].finished_cut_size);
    if mixed {
        return view! {<g>{placements.iter().filter(|item|item.page.is_some()).map(|item|view!{<rect class="piece-cut-path" x=item.cut_x y=item.cut_y width=item.finished_width height=item.finished_height fill="none"/>}).collect_view()}</g>}.into_any();
    }
    let mut xs = placements
        .iter()
        .flat_map(|value| [value.cut_x, value.cut_x + value.finished_width])
        .filter(|value| *value > 0.01 && *value < layout.parent_sheet_size.width - 0.01)
        .collect::<Vec<_>>();
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|a, b| (*a - *b).abs() < 0.001);
    let mut ys = placements
        .iter()
        .flat_map(|value| [value.cut_y, value.cut_y + value.finished_height])
        .filter(|value| *value > 0.01 && *value < layout.parent_sheet_size.height - 0.01)
        .collect::<Vec<_>>();
    ys.sort_by(f64::total_cmp);
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.001);
    view! {<g>{xs.into_iter().map(|x|view!{<line class="piece-cut-path" x1=x y1="0" x2=x y2=layout.parent_sheet_size.height/>}).collect_view()}{ys.into_iter().map(|y|view!{<line class="piece-cut-path" x1="0" y1=y x2=layout.parent_sheet_size.width y2=y/>}).collect_view()}</g>}.into_any()
}
