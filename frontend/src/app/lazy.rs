use super::generic::{self, GenericWorkflow};
use crate::imposition::ImposeWorkspace;
use leptos::prelude::*;
use leptos::wasm_split::{wasm_split, SplitLoaderError};
use wasm_bindgen_futures::spawn_local;

type RenderWorkflow = fn(GenericWorkflow) -> AnyView;

// Return a view factory rather than constructing the view after an await:
// signals, effects and cleanup must belong to the mounted workflow's owner.
#[wasm_split(generic_tools, wasm_split_path = leptos::wasm_split, fallible)]
fn load_generic() -> Result<RenderWorkflow, SplitLoaderError> {
    Ok(generic::render)
}

#[wasm_split(imposition, wasm_split_path = leptos::wasm_split, fallible)]
fn load_imposition() -> Result<RenderWorkflow, SplitLoaderError> {
    Ok(|state| view! { <ImposeWorkspace files=state.files busy=state.busy /> }.into_any())
}

#[derive(Clone, Copy)]
pub(super) struct WorkflowLoader {
    renderer: RwSignal<Option<RenderWorkflow>>,
    failed: RwSignal<bool>,
    failed_once: RwSignal<bool>,
    attempt: RwSignal<u64>,
}

impl WorkflowLoader {
    pub(super) fn new(impose: bool, active: Signal<bool>) -> Self {
        let loader = Self {
            renderer: RwSignal::new(None),
            failed: RwSignal::new(false),
            failed_once: RwSignal::new(false),
            attempt: RwSignal::new(0),
        };
        let loading = RwSignal::new(false);
        Effect::new(move |_| {
            loader.attempt.track();
            if !active.get() || loader.ready() || loading.get_untracked() {
                return;
            }
            loader.failed.set(false);
            loading.set(true);
            spawn_local(async move {
                let result = if impose {
                    load_imposition().await
                } else {
                    load_generic().await
                };
                // Cache only the factory at the app owner. A late completion
                // cannot select a workflow or replace files, and rendering still
                // occurs under the currently mounted workflow's owner.
                match result {
                    Ok(render) => {
                        loader.renderer.try_set(Some(render));
                    }
                    Err(_) => {
                        loader.failed.try_set(true);
                        loader.failed_once.try_set(true);
                    }
                }
                loading.try_set(false);
            });
        });
        loader
    }

    pub(super) fn ready(self) -> bool {
        self.renderer.get().is_some()
    }
    pub(super) fn failed(self) -> bool {
        self.failed.get()
    }
    pub(super) fn retry(self) {
        self.attempt.update(|value| *value = value.wrapping_add(1));
    }

    pub(super) fn recovery_hint(self) -> impl IntoView {
        // Keep the hint mounted throughout Retry so the upload card does not shrink.
        view! {
            <Show when=move || self.failed_once.get()>
                <p class="workflow-recovery-hint">
                    "If Retry still fails after an app update, reload to get the new tools. Reloading clears your selected files and settings. "
                    <a href="/">"Reload and clear workspace"</a>
                </p>
            </Show>
        }
    }

    pub(super) fn render(self, state: GenericWorkflow) -> AnyView {
        match self.renderer.get() {
            Some(render) => render(state),
            None => view! {
            <div class="workflow-loading" aria-live="polite">
                <Show
                    when=move || self.failed()
                    fallback=move || view! {
                        <p role="status">"Opening imposition…"</p>
                    }
                >
                    <p role="alert">"These tools couldn’t load. Your selected files are still here."</p>
                    <button
                        type="button"
                        class="primary-button"
                        on:click=move |_| self.retry()
                    >"Retry loading tools"</button>
                </Show>
                {self.recovery_hint()}
            </div>
        }.into_any(),
    }
    }
}
