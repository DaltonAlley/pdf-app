//! Finished-product choices and artwork positioning. Geometry always comes from the server plan.
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::{browser::PreviewUrlCache, model::*, view::NumberField};

#[component]
pub(super) fn FinishedSizeControls(
    request: RwSignal<LayoutRequest>,
    finished_size_revision: RwSignal<u64>,
) -> impl IntoView {
    let dimensions_chosen = move || {
        request
            .get()
            .finished_dimensions_chosen
            .into_iter()
            .all(|value| value)
    };
    view! {
        <section class="impose-finished-first" aria-labelledby="finished-size-title">
            <div class="control-section-head"><h3 id="finished-size-title">"Finished size"</h3><p>"Set the product dimensions first. Artwork fitting uses this cut frame."</p></div>
            <For each=move || vec![finished_size_revision.get()] key=|revision| *revision children=move |_| view! {
                <div class="field-grid two">
                    <NumberField id="finished-width" label="Finished width (in)" empty=Signal::derive(move || !request.get().finished_dimensions_chosen[0]) value=Signal::derive(move || request.get().finished_cut_size.width) min=0.01 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |value| request.update(|current| { current.finished_cut_size.width=value; current.finished_dimensions_chosen[0]=true; current.finished_size_mode=FinishedSizeMode::Common; })) />
                    <NumberField id="finished-height" label="Finished height (in)" empty=Signal::derive(move || !request.get().finished_dimensions_chosen[1]) value=Signal::derive(move || request.get().finished_cut_size.height) min=0.01 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |value| request.update(|current| { current.finished_cut_size.height=value; current.finished_dimensions_chosen[1]=true; current.finished_size_mode=FinishedSizeMode::Common; })) />
                </div>
            } />
            <label class="field"><span>"Finished orientation"</span><select aria-label="Finished orientation" disabled=move || !dimensions_chosen() prop:value=move || { if request.get().finished_cut_size.width > request.get().finished_cut_size.height { "landscape" } else { "portrait" } } on:change=move |event| {
                let landscape = event_target_value(&event) == "landscape";
                request.update(|current| { let size=current.finished_cut_size; current.finished_cut_size=SizeInches { width: if landscape { size.width.max(size.height) } else { size.width.min(size.height) }, height: if landscape { size.width.min(size.height) } else { size.width.max(size.height) } }; });
            }><option value="portrait">"Portrait"</option><option value="landscape">"Landscape"</option></select></label>
        </section>
    }
}

#[component]
pub(super) fn ArtworkControls(
    request: RwSignal<LayoutRequest>,
    layout: RwSignal<LayoutLifecycle>,
    preview_cache: RwSignal<PreviewUrlCache>,
    selected_page: RwSignal<usize>,
    busy: RwSignal<bool>,
    exporting: RwSignal<bool>,
) -> impl IntoView {
    let individual = RwSignal::new(false);
    let page_scope = move || individual.get().then(|| selected_page.get());
    let fit = move || effective_artwork_fit(&request.get(), page_scope());
    let selected_plan = move || {
        layout.get().last_good().and_then(|layout| {
            layout
                .page_plans
                .iter()
                .find(|plan| plan.page_number == selected_page.get())
                .cloned()
        })
    };
    let update = move |next: ArtworkFit| {
        request.update(|current| update_artwork_fit(current, page_scope(), next))
    };
    let drag = RwSignal::new(None::<(f64, f64, ArtworkFit, f64, f64)>);
    let crop_node = NodeRef::<leptos::html::Div>::new();
    let source_count = move || request.get().source_page_count.unwrap_or(1);
    let raster_identity = move || {
        preview_raster_identity(
            request.get().source_id.as_deref().unwrap_or_default(),
            layout
                .get()
                .last_good()
                .and_then(|layout| layout.source_bleed_override),
        )
    };
    let selected_has_fit_override =
        move || {
            request.get().page_overrides.iter().any(|value| {
                value.page_number == selected_page.get() && value.artwork_fit.is_some()
            })
        };
    let override_size = move || {
        request
            .get()
            .page_overrides
            .iter()
            .find(|value| value.page_number == selected_page.get())
            .and_then(|value| value.finished_cut_size)
    };
    let selected_size = move || {
        override_size()
            .or_else(|| selected_plan().map(|plan| plan.finished_cut_size))
            .unwrap_or(request.get().finished_cut_size)
    };
    let fit_label = move || match fit().mode {
        ArtworkFitMode::Cover => "Fill",
        ArtworkFitMode::Contain => "Fit",
        ArtworkFitMode::Stretch => "Stretch",
    };
    let impression_label = move || match request.get().orientation_preference {
        OrientationPreference::Upright => "Upright",
        OrientationPreference::QuarterTurn => "Quarter-turn",
        _ => "Auto",
    };
    view! {
        <section class="impose-artwork-controls" aria-labelledby="artwork-fit-title">
            <header class="impose-artwork-head">
                <div class="impose-artwork-heading">
                    <h3 id="artwork-fit-title">"Artwork"</h3>
                    <p>"How each page fills its finished size."</p>
                </div>
                <p class="impose-artwork-state" aria-live="polite">
                    <strong>{move || fit_label()}</strong>
                    <span>{move || impression_label()}</span>
                </p>
            </header>
            <fieldset class="impose-artwork-mutation-controls" prop:disabled=move || busy.get() || exporting.get()>
            <div class="impose-artwork-quick-grid">
                <label class="field"><span>"Artwork fitting"</span><select aria-label="Artwork fitting" prop:value=move || match fit().mode { ArtworkFitMode::Cover => "cover", ArtworkFitMode::Stretch => "stretch", ArtworkFitMode::Contain => "contain" } on:change=move |event| { let mut next=fit(); next.mode=match event_target_value(&event).as_str() { "cover" => ArtworkFitMode::Cover, "stretch" => ArtworkFitMode::Stretch, _ => ArtworkFitMode::Contain }; update(next); }><option value="contain">"Fit"</option><option value="stretch">"Stretch"</option><option value="cover">"Fill"</option></select></label>
                <label class="field"><span>"Impression orientation"</span><select aria-label="Impression orientation" prop:value=move || match request.get().orientation_preference { OrientationPreference::Upright => "upright", OrientationPreference::QuarterTurn => "quarterTurn", _ => "auto" } on:change=move |event| request.update(|current| {
                    current.orientation_preference=match event_target_value(&event).as_str() { "upright" => OrientationPreference::Upright, "quarterTurn" => OrientationPreference::QuarterTurn, _ => OrientationPreference::Auto };
                    if let Some(manual)=current.manual.as_mut() { manual.rotation_degrees=if current.orientation_preference==OrientationPreference::QuarterTurn {90} else {0}; }
                })><option value="auto">"Auto · best sheet fit"</option><option value="upright">"Upright · 0°"</option><option value="quarterTurn">"Quarter-turn · 90°"</option></select></label>
            </div>
            <p class="impose-fit-summary" aria-live="polite">{move || {
                let scope=if individual.get() {format!("Artwork {}",selected_page.get())} else {"Shared settings".into()};
                let detail=match fit().mode { ArtworkFitMode::Cover => "Fills the cut size and crops excess artwork.", ArtworkFitMode::Contain => "Keeps all artwork visible. May leave borders.", ArtworkFitMode::Stretch => "Stretches nonproportionally to fill the cut size. Artwork may look distorted." };
                format!("{scope}: {detail}")
            }}</p>
            <Show when=move || selected_has_fit_override() && !individual.get()><p class="status-message">"Selected artwork has its own fitting settings. Enable Adjust only selected artwork to edit or reset them. Shared fitting changes do not replace this override."</p></Show>
            <Show when=move || fit().mode == ArtworkFitMode::Cover && (individual.get() || !selected_has_fit_override())>
                <div class="impose-crop-editor">
                    <p>"Drag artwork to choose what remains inside the cut. Or use the keyboard-accessible position sliders."</p>
                    <div node_ref=crop_node class="impose-crop-drag" role="group" aria-label="Artwork crop positioning" on:pointerdown=move |event| {
                        if busy.get_untracked() || exporting.get_untracked() {
                            drag.set(None);
                            event.prevent_default();
                            return;
                        }
                        let (Some(node),Some(plan))=(crop_node.get(),selected_plan()) else {return;};
                        let bounds=node.get_bounding_client_rect();
                        let sx=bounds.width()/plan.finished_cut_size.width;
                        let sy=bounds.height()/plan.finished_cut_size.height;
                        let Some(travel)=plan.position_travel else {return;};
                        drag.set(Some((f64::from(event.client_x()),f64::from(event.client_y()),fit(),travel.x*sx,travel.y*sy)));
                        if let Some(target)=event.current_target().and_then(|target|target.dyn_into::<web_sys::Element>().ok()) { let _=target.set_pointer_capture(event.pointer_id()); }
                        event.prevent_default();
                    } on:pointermove=move |event| {
                        if busy.get_untracked() || exporting.get_untracked() {
                            drag.set(None);
                            event.prevent_default();
                            return;
                        }
                        let Some((x,y,mut next,overflow_x,overflow_y))=drag.get() else {return;};
                        next.position=drag_crop_position(next.position,ArtworkPosition{x:f64::from(event.client_x())-x,y:f64::from(event.client_y())-y},ArtworkPosition{x:overflow_x,y:overflow_y});
                        update(next);
                    } on:pointerup=move |_| drag.set(None) on:pointercancel=move |_| drag.set(None) style=move || {
                        let size = selected_plan().map(|plan| plan.finished_cut_size).unwrap_or(SizeInches { width: 8.5, height: 11.0 });
                        // Constrain both axes in the setup rail while retaining the cut aspect ratio.
                        format!("aspect-ratio: {} / {}; --crop-rail-width: {}px", size.width, size.height, 220.0 * size.width / size.height)
                    }>
                        {move || selected_plan().map(|plan| {
                            let url=preview_cache.with(|cache|cache.url_for_source(&raster_identity(),plan.page_number).map(str::to_owned));
                            let bounds=plan.preview_box.unwrap_or(PdfBox {left:0.0,bottom:0.0,right:plan.source_pdf_size.width,top:plan.source_pdf_size.height,width:plan.source_pdf_size.width,height:plan.source_pdf_size.height});
                            let sx=plan.artwork.width/plan.source_pdf_size.width;
                            let sy=plan.artwork.height/plan.source_pdf_size.height;
                            view! {<svg viewBox=format!("0 0 {} {}",plan.finished_cut_size.width,plan.finished_cut_size.height) role="img" aria-label="Selected artwork inside finished cut boundary"><rect width=plan.finished_cut_size.width height=plan.finished_cut_size.height fill="white"/>{url.map(|url|view!{<image href=url x=plan.artwork.x+bounds.left*sx y=plan.artwork.y+(plan.source_pdf_size.height-bounds.top)*sy width=bounds.width*sx height=bounds.height*sy preserveAspectRatio="none"/>})}<rect class="piece-cut" x="0" y="0" width=plan.finished_cut_size.width height=plan.finished_cut_size.height/></svg>}
                        })}
                    </div>
                    <div class="field-grid two">
                        <label class="field"><span>{move ||format!("Horizontal position · {:.0}%",fit().position.x*100.0)}</span><input id="horizontal-crop-position" type="range" min="0" max="100" step="1" aria-label="Horizontal crop position" prop:value=move ||fit().position.x*100.0 on:input=move |event| {if let Ok(value)=event_target_value(&event).parse::<f64>() {let mut next=fit();next.position.x=value/100.0;update(next);}}/></label>
                        <label class="field"><span>{move ||format!("Vertical position · {:.0}%",fit().position.y*100.0)}</span><input type="range" min="0" max="100" step="1" aria-label="Vertical crop position" prop:value=move ||fit().position.y*100.0 on:input=move |event| {if let Ok(value)=event_target_value(&event).parse::<f64>() {let mut next=fit();next.position.y=value/100.0;update(next);}}/></label>
                    </div>
                    <div class="crop-anchor-grid" role="group" aria-label="Crop position anchors">
                        {[(0.0, 0.0, "Top left"), (0.5, 0.0, "Top center"), (1.0, 0.0, "Top right"), (0.0, 0.5, "Middle left"), (0.5, 0.5, "Center"), (1.0, 0.5, "Middle right"), (0.0, 1.0, "Bottom left"), (0.5, 1.0, "Bottom center"), (1.0, 1.0, "Bottom right")].into_iter().map(|(x, y, label)| view! {
                            <button type="button" aria-label=label title=label on:click=move |_| { let mut next=fit(); next.position=ArtworkPosition { x, y }; update(next); }><span aria-hidden="true"></span></button>
                        }).collect_view()}
                    </div>
                    <button type="button" class="ghost-button" on:click=move |_| {let mut next=fit();next.position=ArtworkPosition::default();update(next);}>"Reset crop position"</button>
                </div>
            </Show>
            <details class="impose-artwork-advanced">
                <summary>
                    <span>{move || if source_count()>1 { "Adjust individual artwork" } else { "Artwork details" }}</span>
                    <small>{move || if source_count()>1 { "Optional overrides for mixed artwork" } else { "Source and bleed information" }}</small>
                </summary>
                <div class="impose-artwork-advanced-body">
                    <Show when=move || { source_count()>1 }>
                        <div class="impose-artwork-scope">
                            <div><p class="control-label">"Artwork scope"</p><p class="field-help">"Shared settings apply to every page unless you opt into a per-artwork adjustment."</p></div>
                            <label class="field"><span>"Selected artwork"</span><select aria-label="Selected artwork" prop:value=move || selected_page.get().to_string() on:change=move |event| { if let Ok(page)=event_target_value(&event).parse::<usize>() { selected_page.set(page); } }>{move || (1..=source_count()).map(|page| view! {<option value=page.to_string()>{format!("Artwork {page}")}</option>}).collect_view()}</select></label>
                            <label class="field checkbox-field"><input type="checkbox" prop:checked=move || individual.get() on:change=move |event| individual.set(event_target_checked(&event))/><span>"Adjust only selected artwork"</span></label>
                            <Show when=move || individual.get()><button type="button" class="text-button" on:click=move |_| request.update(|current| current.page_overrides.retain(|value|value.page_number != selected_page.get()))>"Use shared settings for this artwork"</button></Show>
                            <Show when=move || individual.get()>
                                <label class="field checkbox-field"><input type="checkbox" prop:checked=move || override_size().is_some() on:change=move |event| { let size=event_target_checked(&event).then(selected_size); request.update(|current| update_finished_override(current,selected_page.get(),size)); }/><span>"Override this artwork's finished size"</span></label>
                                <Show when=move || override_size().is_some()>
                                    <div class="field-grid two">
                                        <NumberField label="Artwork finished width (in)" value=Signal::derive(move ||selected_size().width) min=0.01 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |width| { let mut size=selected_size();size.width=width;request.update(|current|update_finished_override(current,selected_page.get(),Some(size))); }) />
                                        <NumberField label="Artwork finished height (in)" value=Signal::derive(move ||selected_size().height) min=0.01 max=100.0 step=0.01 on_change=UnsyncCallback::new(move |height| { let mut size=selected_size();size.height=height;request.update(|current|update_finished_override(current,selected_page.get(),Some(size))); }) />
                                    </div>
                                </Show>
                            </Show>
                        </div>
                    </Show>
                    <details class="impose-artwork-source-details">
                        <summary>"Source details"</summary>
                        <Show when=move || selected_plan().is_some()>
                            <p class="impose-selected-details">{move || selected_plan().map(|plan| {
                                let provenance=request.get().source_pages.as_ref().and_then(|pages|pages.get(plan.page_number.saturating_sub(1))).map(|page|format!("{} · page {}",page.filename.as_deref().unwrap_or("Artwork"),page.original_page_number.unwrap_or(plan.page_number))).unwrap_or_else(||format!("Artwork {}",plan.page_number));
                                let mode=match effective_artwork_fit(&request.get(),Some(plan.page_number)).mode { ArtworkFitMode::Cover => "Fill", ArtworkFitMode::Contain => "Fit", ArtworkFitMode::Stretch => "Stretch (nonproportional)" };
                                format!("{provenance} · source {} · cut {} · {mode} · bleed {:.3} in per side",size_label(plan.source_pdf_size),size_label(plan.finished_cut_size),plan.bleed_amount)
                            }).unwrap_or_default()}</p>
                        </Show>
                        <p class="impose-bleed-explainer">"Bleed extends beyond the cut and is trimmed away. Scaling to add bleed can remove more artwork at the cut edge."</p>
                    </details>
                </div>
            </details>
            </fieldset>
        </section>
    }
}
