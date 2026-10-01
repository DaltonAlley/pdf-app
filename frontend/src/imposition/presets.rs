//! Reusable impose presets.

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use super::{
    browser as impose_browser, model::*, set_imposition_mode_preserving_repeat_drafts,
    set_sides_preserving_repeat_drafts, supports_duplex,
};
use crate::presentation::aria_bool;

const CLEAN_PDF: &str = "cleanPdf";

/// Compact catalog and CRUD controls for reusable imposition setups.
#[component]
pub(super) fn PresetModules(
    request: RwSignal<LayoutRequest>,
    quantity_drafts: RwSignal<RepeatQuantityDrafts>,
    finished_size_revision: RwSignal<u64>,
    #[prop(default = false)] toolbar: bool,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let presets = RwSignal::new(Vec::<GangUpPreset>::new());
    let selected_id = RwSignal::new(String::new());
    // Keep the name field blank so the suggestion follows the current dimensions. The
    // backend still receives a concrete name when Save current setup is pressed.
    let name = RwSignal::new(String::new());
    let status = RwSignal::new(Option::<String>::None);
    let loading = RwSignal::new(true);
    let saving = RwSignal::new(false);

    {
        spawn_local(async move {
            match impose_browser::list_presets().await {
                Ok(values) => {
                    presets.set(values);
                    status.set(None);
                }
                Err(error) => status.set(Some(error)),
            }
            loading.set(false);
        });
    }

    let selected_preset = move || {
        let id = selected_id.get();
        presets.get().into_iter().find(|preset| preset.id == id)
    };
    let set_error = move |error: String| {
        status.set(Some(error));
        saving.set(false);
    };
    let apply_selected = move |_| {
        let Some(preset) = selected_preset() else {
            status.set(Some("Choose a preset first.".into()));
            return;
        };
        apply_preset(request, quantity_drafts, &preset);
        // A preset replaces finished-size drafts even when its committed values are unchanged.
        finished_size_revision.update(|revision| *revision = revision.wrapping_add(1));
        name.set(preset.name.clone());
        status.set(Some(format!("Applied {}.", preset.name)));
    };
    let save_new = move |_| {
        let preset_name = normalized_name(&name.get(), &request.get_untracked());
        let input = input_from_request(&preset_name, &request.get_untracked());
        saving.set(true);
        status.set(Some("Saving preset…".into()));
        let presets = presets;
        let selected_id = selected_id;
        let name = name;
        let status = status;
        spawn_local(async move {
            match impose_browser::create_preset(&input).await {
                Ok(created) => {
                    let created_name = created.name.clone();
                    presets.update(|values| values.push(created.clone()));
                    selected_id.set(created.id);
                    name.set(created_name.clone());
                    status.set(Some(format!("Saved {created_name}.")));
                }
                Err(error) => set_error(error),
            }
            saving.set(false);
        });
    };
    let update_selected = move |_| {
        let Some(preset) = selected_preset() else {
            status.set(Some("Choose a preset to update.".into()));
            return;
        };
        let input = input_from_request(&preset.name, &request.get_untracked());
        let id = preset.id.clone();
        saving.set(true);
        status.set(Some("Updating preset…".into()));
        let presets = presets;
        let status = status;
        spawn_local(async move {
            match impose_browser::update_preset(&id, &input).await {
                Ok(updated) => {
                    presets.update(|values| {
                        if let Some(existing) =
                            values.iter_mut().find(|value| value.id == updated.id)
                        {
                            *existing = updated.clone();
                        }
                    });
                    status.set(Some(format!("Updated {}.", updated.name)));
                }
                Err(error) => set_error(error),
            }
            saving.set(false);
        });
    };
    let rename_selected = move |_| {
        let Some(preset) = selected_preset() else {
            status.set(Some("Choose a preset to rename.".into()));
            return;
        };
        let next_name = normalized_name(&name.get(), &request.get_untracked());
        let input = input_from_preset(&preset, &next_name);
        let id = preset.id.clone();
        saving.set(true);
        status.set(Some("Renaming preset…".into()));
        let presets = presets;
        let name = name;
        let status = status;
        spawn_local(async move {
            match impose_browser::update_preset(&id, &input).await {
                Ok(updated) => {
                    let updated_name = updated.name.clone();
                    presets.update(|values| {
                        if let Some(existing) =
                            values.iter_mut().find(|value| value.id == updated.id)
                        {
                            *existing = updated;
                        }
                    });
                    name.set(updated_name.clone());
                    status.set(Some(format!("Renamed to {updated_name}.")));
                }
                Err(error) => set_error(error),
            }
            saving.set(false);
        });
    };
    let delete_selected = move |_| {
        let Some(preset) = selected_preset() else {
            status.set(Some("Choose a preset to delete.".into()));
            return;
        };
        let confirmed = web_sys::window()
            .and_then(|window| {
                window
                    .confirm_with_message(&format!("Delete {}?", preset.name))
                    .ok()
            })
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        let id = preset.id.clone();
        let deleted_name = preset.name.clone();
        saving.set(true);
        status.set(Some("Deleting preset…".into()));
        let presets = presets;
        let selected_id = selected_id;
        let status = status;
        spawn_local(async move {
            match impose_browser::delete_preset(&id).await {
                Ok(()) => {
                    presets.update(|values| values.retain(|value| value.id != id));
                    selected_id.set(String::new());
                    status.set(Some(format!("Deleted {deleted_name}.")));
                }
                Err(error) => set_error(error),
            }
            saving.set(false);
        });
    };

    view! {
        <section class=move || if toolbar { "presets preset-toolbar" } else { "presets" } aria-label="Presets">
            <div class="presets-head">
                <Show when=move || !toolbar>
                    <div>
                        <h3 id="presets-title">"Presets"</h3>
                        <p>"Save a setup you use again."</p>
                    </div>
                </Show>
                <button class=move || if toolbar { "text-button preset-toolbar-trigger" } else { "text-button" } type="button" aria-expanded=move || aria_bool(open.get()) aria-controls="preset-panel" on:click=move |_| open.update(|value| *value = !*value)>
                    {move || if open.get() { "Hide presets" } else if toolbar { "Presets" } else { "Show presets" }}
                </button>
            </div>
            <Show when=move || open.get()>
                <div id="preset-panel" class="preset-panel">
                    <Show when=move || loading.get()>
                        <p class="field-help" role="status">"Loading presets…"</p>
                    </Show>
                    <label class="field" for="preset-select">
                        <span>"Use a saved setup"</span>
                        <select id="preset-select" aria-label="Preset" prop:value=move || selected_id.get() on:change=move |event| {
                            let id = event_target_value(&event);
                            selected_id.set(id.clone());
                            if let Some(preset) = presets.get_untracked().into_iter().find(|value| value.id == id) {
                                name.set(preset.name);
                            }
                        } disabled=move || loading.get() || saving.get()>
                            <option value="">"Choose a preset"</option>
                            <For each=move || presets.get() key=|preset| preset.id.clone() children=move |preset| view! {
                                <option value=preset.id.clone()>{preset.name.clone()}</option>
                            } />
                        </select>
                    </label>
                    <Show when=move || selected_id.get().is_empty() fallback=move || view! {
                        <p class="field-help">{move || selected_preset().map(|preset| preset_summary(&preset)).unwrap_or_default()}</p>
                    }>
                        <p class="field-help">"The starter 5x7 on 12x18 preset is ready to use."</p>
                    </Show>
                    <div class="preset-actions">
                        <button class="ghost-button" type="button" on:click=apply_selected disabled=move || saving.get() || selected_id.get().is_empty()>"Apply preset"</button>
                        <button class="ghost-button" type="button" on:click=save_new disabled=move || saving.get()>"Save current setup"</button>
                        <button class="ghost-button" type="button" on:click=update_selected disabled=move || saving.get() || selected_id.get().is_empty()>"Update preset"</button>
                        <button class="ghost-button" type="button" on:click=rename_selected disabled=move || saving.get() || selected_id.get().is_empty()>"Rename"</button>
                        <button class="text-button danger-text-button" type="button" on:click=delete_selected disabled=move || saving.get() || selected_id.get().is_empty()>"Delete"</button>
                    </div>
                    <label class="field" for="preset-name">
                        <span>"Preset name (optional)"</span>
                        <input id="preset-name" type="text" maxlength="200" placeholder="Leave blank for an automatic name" prop:value=move || name.get() on:input=move |event| name.set(event_target_value(&event)) />
                    </label>
                    <Show when=move || status.get().is_some()>
                        <p class=move || if status.get().is_some_and(|value| value.contains("failed") || value.contains("invalid")) { "field-error" } else { "field-help" } role="status">{move || status.get().unwrap_or_default()}</p>
                    </Show>
                </div>
            </Show>
        </section>
    }
}

fn input_from_request(name: &str, request: &LayoutRequest) -> PresetInput {
    PresetInput {
        imposition_mode: request.imposition_mode,
        finished_size_mode: request.finished_size_mode,
        artwork_fit: request.artwork_fit,
        source_bleed_override: request.source_bleed_override,
        id: None,
        name: name.to_owned(),
        finished_cut_size: request.finished_cut_size,
        parent_sheet_size: request.parent_sheet_size,
        bleed_handling: request.bleed_option,
        created_bleed_amount: request.created_bleed_amount,
        gutter: request.gutter,
        orientation_preference: request.orientation_preference,
        sides: request.sides,
        layout_preference: request.layout_mode,
        manual: request.manual,
        duplex: request.duplex.clone(),
        output_preference: Some(CLEAN_PDF.to_owned()),
    }
}

fn input_from_preset(preset: &GangUpPreset, name: &str) -> PresetInput {
    PresetInput {
        imposition_mode: preset.imposition_mode,
        finished_size_mode: preset.finished_size_mode,
        artwork_fit: preset.artwork_fit,
        source_bleed_override: preset.source_bleed_override,
        id: None,
        name: name.to_owned(),
        finished_cut_size: preset.finished_cut_size,
        parent_sheet_size: preset.parent_sheet_size,
        bleed_handling: preset.bleed_handling,
        created_bleed_amount: preset.created_bleed_amount,
        gutter: preset.gutter,
        orientation_preference: preset.orientation_preference,
        sides: preset.sides,
        layout_preference: preset.layout_preference,
        manual: preset.manual,
        duplex: preset.duplex.clone(),
        output_preference: Some(if preset.output_preference.is_empty() {
            CLEAN_PDF.to_owned()
        } else {
            preset.output_preference.clone()
        }),
    }
}

fn apply_preset(
    request: RwSignal<LayoutRequest>,
    quantity_drafts: RwSignal<RepeatQuantityDrafts>,
    preset: &GangUpPreset,
) {
    request.update(|current| {
        current.finished_size_mode = preset.finished_size_mode;
        current.artwork_fit = preset.artwork_fit;
        current.source_bleed_override = preset.source_bleed_override;
        current.finished_cut_size = preset.finished_cut_size;
        current.finished_dimensions_chosen = [true; 2];
        current.parent_sheet_size = preset.parent_sheet_size;
        current.orientation_preference = preset.orientation_preference;
        current.layout_mode = preset.layout_preference;
        current.manual = preset.manual;
        current.bleed_option = preset.bleed_handling;
        current.created_bleed_amount = preset.created_bleed_amount;
        current.gutter = preset.gutter;
        let next_sides =
            if preset.sides == Sides::Double && supports_duplex(current.source_page_count) {
                Sides::Double
            } else {
                Sides::Single
            };
        quantity_drafts.update(|drafts| {
            set_sides_preserving_repeat_drafts(current, drafts, next_sides);
            set_imposition_mode_preserving_repeat_drafts(current, drafts, preset.imposition_mode);
        });
        if current.sides == Sides::Double {
            current.duplex = preset.duplex.clone();
        } else {
            current.duplex = None;
        }
    });
}

fn normalized_name(value: &str, request: &LayoutRequest) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        preset_name_suggestion(request)
    } else {
        trimmed.to_owned()
    }
}

fn preset_name_suggestion(request: &LayoutRequest) -> String {
    format!(
        "{} × {} on {} × {}",
        compact_inches(request.finished_cut_size.width),
        compact_inches(request.finished_cut_size.height),
        compact_inches(request.parent_sheet_size.width),
        compact_inches(request.parent_sheet_size.height),
    )
}

fn preset_summary(preset: &GangUpPreset) -> String {
    format!(
        "{} × {} on {} × {}",
        compact_inches(preset.finished_cut_size.width),
        compact_inches(preset.finished_cut_size.height),
        compact_inches(preset.parent_sheet_size.width),
        compact_inches(preset.parent_sheet_size.height),
    )
}

fn compact_inches(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_name_uses_finished_and_sheet_dimensions() {
        let mut request = initial_request();
        request.finished_cut_size = SizeInches {
            width: 5.0,
            height: 7.0,
        };
        assert_eq!(normalized_name("  ", &request), "5 × 7 on 12 × 18");
    }
}
