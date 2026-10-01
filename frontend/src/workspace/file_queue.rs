//! Ordered file queue interaction and its Leptos view.

use super::Operation;

const REORDER_HELP_ID: &str = "queue-reorder-help";
const REORDER_SHORTCUTS: &str = "Alt+ArrowUp Alt+ArrowDown";

pub(crate) const fn operation_uses_file_order(operation: Operation, file_count: usize) -> bool {
    file_count > 1
        && matches!(
            operation,
            Operation::PdfImage | Operation::Merge | Operation::ImagePdf | Operation::Impose
        )
}

fn position_announcement(name: &str, position: usize) -> String {
    format!("{name} is now position {position}.")
}

#[cfg(target_arch = "wasm32")]
fn queue_row_id(id: u64) -> String {
    format!("queue-file-{id}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MoveRequest {
    source_id: u64,
    target_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragOrigin {
    RowContent,
    InteractiveDescendant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueKey {
    ArrowUp,
    ArrowDown,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyCommand {
    key: QueueKey,
    alt: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueueContext {
    ordered: bool,
    busy: bool,
    ids: Vec<u64>,
}

impl QueueContext {
    fn can_reorder(&self) -> bool {
        self.ordered && !self.busy && self.ids.len() > 1
    }

    fn position(&self, id: u64) -> Option<usize> {
        self.ids.iter().position(|candidate| *candidate == id)
    }

    fn semantics(&self) -> QueueSemantics {
        let enabled = self.can_reorder();
        QueueSemantics {
            draggable: enabled,
            tab_index: enabled.then_some(0),
            described_by: enabled.then_some(REORDER_HELP_ID),
            key_shortcuts: enabled.then_some(REORDER_SHORTCUTS),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QueueSemantics {
    draggable: bool,
    tab_index: Option<i32>,
    described_by: Option<&'static str>,
    key_shortcuts: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct QueueInteraction {
    pending_drag_origin: Option<DragOrigin>,
    dragged_id: Option<u64>,
    target_id: Option<u64>,
}

impl QueueInteraction {
    fn prepare_drag(&mut self, origin: DragOrigin) {
        self.pending_drag_origin = Some(origin);
    }

    fn clear_pending_drag(&mut self) {
        self.pending_drag_origin = None;
    }

    fn start_drag(&mut self, context: &QueueContext, id: u64) -> bool {
        let origin = self.pending_drag_origin.take();
        if !context.can_reorder()
            || context.position(id).is_none()
            || origin != Some(DragOrigin::RowContent)
        {
            self.cancel();
            return false;
        }
        self.dragged_id = Some(id);
        self.target_id = None;
        true
    }

    fn update_target(&mut self, context: &QueueContext, id: u64) -> bool {
        let Some(dragged_id) = self.dragged_id else {
            return false;
        };
        if !context.can_reorder()
            || dragged_id == id
            || context.position(dragged_id).is_none()
            || context.position(id).is_none()
        {
            return false;
        }
        self.target_id = Some(id);
        true
    }

    fn drop_on(&mut self, context: &QueueContext, id: u64) -> Option<MoveRequest> {
        let request = self
            .dragged_id
            .filter(|dragged_id| {
                context.can_reorder()
                    && *dragged_id != id
                    && context.position(*dragged_id).is_some()
                    && context.position(id).is_some()
            })
            .map(|source_id| MoveRequest {
                source_id,
                target_id: id,
            });
        self.cancel();
        request
    }

    fn keyboard_move(context: &QueueContext, id: u64, command: KeyCommand) -> Option<MoveRequest> {
        if !context.can_reorder() || !command.alt {
            return None;
        }
        let source = context.position(id)?;
        let target = match command.key {
            QueueKey::ArrowUp => source.checked_sub(1)?,
            QueueKey::ArrowDown => source
                .checked_add(1)
                .filter(|next| *next < context.ids.len())?,
            QueueKey::Other => return None,
        };
        Some(MoveRequest {
            source_id: id,
            target_id: context.ids[target],
        })
    }

    fn cancel(&mut self) {
        self.pending_drag_origin = None;
        self.dragged_id = None;
        self.target_id = None;
    }
}

#[cfg(target_arch = "wasm32")]
mod view {
    use leptos::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{DragEvent, Element, KeyboardEvent};

    use crate::{
        browser,
        files::{classify_file, FileKind, SelectedFile},
    };

    use super::super::Operation;
    use super::{
        queue_row_id, DragOrigin, KeyCommand, QueueContext, QueueInteraction, QueueKey,
        REORDER_HELP_ID,
    };

    const INTERACTIVE_SELECTOR: &str =
        "button, a, input, select, textarea, [contenteditable='true']";

    #[component]
    pub(crate) fn FileQueue(
        files: RwSignal<Vec<SelectedFile>>,
        operation: RwSignal<Operation>,
        busy: RwSignal<bool>,
        on_clear: UnsyncCallback<()>,
        on_remove: UnsyncCallback<u64>,
        on_reorder: UnsyncCallback<(u64, u64)>,
    ) -> impl IntoView {
        let interaction = RwSignal::new(QueueInteraction::default());
        let reorder_announcement = RwSignal::new(String::new());
        let shows_reordering = Signal::derive(move || {
            super::operation_uses_file_order(operation.get(), files.read().len())
        });
        let total = move || {
            files
                .read()
                .iter()
                .map(|file| file.descriptor.size)
                .sum::<u64>()
        };

        Effect::new(move |_| {
            let current = context(files, operation, busy);
            let state = interaction.get();
            let invalid = !current.can_reorder()
                || state
                    .dragged_id
                    .is_some_and(|id| current.position(id).is_none());
            if invalid && state != QueueInteraction::default() {
                interaction.update(QueueInteraction::cancel);
            }
        });

        view! {
            <section class="file-queue" aria-labelledby="selected-files-heading">
                <div class="file-queue-head">
                    <div>
                        <h2 id="selected-files-heading">{move || {
                            let selected = files.read();
                            if selected.len() == 1
                                && classify_file(&selected[0].descriptor) == FileKind::Pdf
                            {
                                "Source"
                            } else {
                                "Files"
                            }
                        }}</h2>
                        <p>{move || {
                            let summary = format!(
                                "{} · {}",
                                file_count_label(files.read().len()),
                                format_bytes(total()),
                            );
                            let description = match operation.get() {
                                Operation::PdfImage => "Export order",
                                Operation::Merge => "PDF order",
                                Operation::ImagePdf => "Page order",
                                Operation::Split | Operation::Impose => "",
                            };
                            if description.is_empty() {
                                summary
                            } else {
                                format!("{summary} · {description}")
                            }
                        }}</p>
                        <Show when=move || shows_reordering.get()>
                            <p id=REORDER_HELP_ID class="queue-reorder-help">
                                <span aria-hidden="true">"Drag rows to set the output order."</span>
                                <span class="visually-hidden">
                                    " With a row focused, press Alt+Arrow Up or Alt+Arrow Down."
                                </span>
                            </p>
                        </Show>
                    </div>
                    <button class="queue-clear-button" type="button" disabled=move || busy.get() on:click=move |_| on_clear.run(())>"Clear"</button>
                </div>
                <p class="visually-hidden" aria-live="polite" aria-atomic="true">
                    {move || reorder_announcement.get()}
                </p>
                <div class="file-summary" role="list" aria-label=move || format!("{} selected files", files.read().len())>
                    <For
                        each=move || files.get()
                        key=|file| file.id
                        children=move |file| {
                            let id = file.id;
                            let name = file.descriptor.name.clone();
                            let row_name = name.clone();
                            let accessible_name = name.clone();
                            let remove_name = name.clone();
                            let drag_name = name.clone();
                            let keyboard_name = name.clone();
                            let size = format_bytes(file.descriptor.size);
                            let position = move || files.read().iter().position(|candidate| candidate.id == id).unwrap_or(0);
                            view! {
                                <div
                                    id=queue_row_id(id)
                                    class="file-row"
                                    class:ordered=move || shows_reordering.get()
                                    role="listitem"
                                    class:dragging=move || interaction.get().dragged_id == Some(id)
                                    class:drag-over-before=move || {
                                        interaction.get().target_id == Some(id)
                                            && interaction.get().dragged_id.is_some_and(|dragged_id| {
                                                files.read().iter().position(|candidate| candidate.id == dragged_id)
                                                    .is_some_and(|dragged_position| dragged_position > position())
                                            })
                                    }
                                    class:drag-over-after=move || {
                                        interaction.get().target_id == Some(id)
                                            && interaction.get().dragged_id.is_some_and(|dragged_id| {
                                                files.read().iter().position(|candidate| candidate.id == dragged_id)
                                                    .is_some_and(|dragged_position| dragged_position < position())
                                            })
                                    }
                                    draggable=move || context(files, operation, busy).semantics().draggable.then_some("true")
                                    tabindex=move || context(files, operation, busy).semantics().tab_index
                                    aria-describedby=move || context(files, operation, busy).semantics().described_by
                                    aria-keyshortcuts=move || context(files, operation, busy).semantics().key_shortcuts
                                    aria-label=move || shows_reordering.get().then(|| format!(
                                        "{accessible_name}, position {} of {}",
                                        position() + 1,
                                        files.read().len(),
                                    ))
                                    on:pointerdown=move |event| {
                                        let origin = if event_from_interactive_descendant(&event) {
                                            DragOrigin::InteractiveDescendant
                                        } else {
                                            DragOrigin::RowContent
                                        };
                                        interaction.update(|state| state.prepare_drag(origin));
                                    }
                                    on:pointerup=move |_| interaction.update(QueueInteraction::clear_pending_drag)
                                    on:pointercancel=move |_| interaction.update(QueueInteraction::clear_pending_drag)
                                    on:dragstart=move |event: DragEvent| {
                                        let current = context(files, operation, busy);
                                        let mut next = interaction.get_untracked();
                                        if !next.start_drag(&current, id) {
                                            event.prevent_default();
                                            interaction.set(next);
                                            return;
                                        }
                                        interaction.set(next);
                                        reorder_announcement.set(format!(
                                            "Dragging {drag_name}. Drop it on another file to move it."
                                        ));
                                        if let Some(data) = event.data_transfer() {
                                            data.set_effect_allowed("move");
                                            let _ = data.set_data("text/plain", &id.to_string());
                                        }
                                    }
                                    on:dragover=move |event: DragEvent| {
                                        let current = context(files, operation, busy);
                                        let mut next = interaction.get_untracked();
                                        if next.update_target(&current, id) {
                                            event.prevent_default();
                                            event.stop_propagation();
                                            if let Some(data) = event.data_transfer() {
                                                data.set_drop_effect("move");
                                            }
                                            interaction.set(next);
                                        }
                                    }
                                    on:drop=move |event: DragEvent| {
                                        let current = context(files, operation, busy);
                                        let mut next = interaction.get_untracked();
                                        let request = next.drop_on(&current, id);
                                        interaction.set(next);
                                        let Some(request) = request else {
                                            return;
                                        };
                                        event.prevent_default();
                                        event.stop_propagation();
                                        let moved_name = files
                                            .read_untracked()
                                            .iter()
                                            .find(|candidate| candidate.id == request.source_id)
                                            .map(|candidate| candidate.descriptor.name.clone())
                                            .unwrap_or_else(|| "File".to_owned());
                                        let next_position = current.position(request.target_id).unwrap_or(0) + 1;
                                        on_reorder.run((request.source_id, request.target_id));
                                        browser::focus_element_after_render(queue_row_id(request.source_id));
                                        reorder_announcement.set(super::position_announcement(
                                            &moved_name,
                                            next_position,
                                        ));
                                    }
                                    on:dragend=move |_| interaction.update(QueueInteraction::cancel)
                                    on:keydown=move |event: KeyboardEvent| {
                                        if event_from_interactive_descendant(&event) {
                                            return;
                                        }
                                        let current = context(files, operation, busy);
                                        let command = KeyCommand {
                                            alt: event.alt_key(),
                                            key: match event.key().as_str() {
                                                "ArrowUp" => QueueKey::ArrowUp,
                                                "ArrowDown" => QueueKey::ArrowDown,
                                                _ => QueueKey::Other,
                                            },
                                        };
                                        let Some(request) = QueueInteraction::keyboard_move(&current, id, command) else {
                                            return;
                                        };
                                        event.prevent_default();
                                        let next_position = current.position(request.target_id).unwrap_or(0) + 1;
                                        on_reorder.run((request.source_id, request.target_id));
                                        browser::focus_element_after_render(queue_row_id(request.source_id));
                                        reorder_announcement.set(super::position_announcement(
                                            &keyboard_name,
                                            next_position,
                                        ));
                                    }
                                >
                                    <Show when=move || shows_reordering.get()>
                                        <span class="file-index" aria-hidden="true">{move || position() + 1}</span>
                                    </Show>
                                    <div class="file-meta">
                                        <span title=name.clone()>{row_name}</span>
                                        <small>{size}</small>
                                    </div>
                                    <button class="icon-button remove-file" type="button" disabled=move || busy.get() aria-label=format!("Remove {remove_name}") on:click=move |_| on_remove.run(id)>"×"</button>
                                </div>
                            }
                        }
                    />
                </div>
            </section>
        }
    }

    fn context(
        files: RwSignal<Vec<SelectedFile>>,
        operation: RwSignal<Operation>,
        busy: RwSignal<bool>,
    ) -> QueueContext {
        QueueContext {
            ordered: super::operation_uses_file_order(operation.get(), files.read().len()),
            busy: busy.get(),
            ids: files.read().iter().map(|file| file.id).collect(),
        }
    }

    fn event_from_interactive_descendant(event: &web_sys::Event) -> bool {
        event
            .target()
            .and_then(|target| target.dyn_into::<Element>().ok())
            .and_then(|target| target.closest(INTERACTIVE_SELECTOR).ok().flatten())
            .is_some()
    }

    fn format_bytes(bytes: u64) -> String {
        format!("{:.2} MB", bytes as f64 / 1024.0 / 1024.0)
    }

    fn file_count_label(count: usize) -> String {
        format!("{count} {}", if count == 1 { "file" } else { "files" })
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use view::FileQueue;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::move_item_to;

    fn context(ids: &[u64]) -> QueueContext {
        QueueContext {
            ordered: true,
            busy: false,
            ids: ids.to_vec(),
        }
    }

    fn apply(ids: &mut [u64], request: MoveRequest) -> bool {
        let from = ids.iter().position(|id| *id == request.source_id);
        let to = ids.iter().position(|id| *id == request.target_id);
        match (from, to) {
            (Some(from), Some(to)) => move_item_to(ids, from, to),
            _ => false,
        }
    }

    #[test]
    fn whole_row_drag_moves_stable_ids_in_both_directions() {
        let mut ids = vec![10, 20, 30, 40];
        let mut interaction = QueueInteraction::default();
        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(interaction.start_drag(&context(&ids), 10));
        assert!(interaction.update_target(&context(&ids), 30));
        let request = interaction.drop_on(&context(&ids), 30);
        assert_eq!(
            request,
            Some(MoveRequest {
                source_id: 10,
                target_id: 30
            })
        );
        assert!(request.is_some_and(|request| apply(&mut ids, request)));
        assert_eq!(ids, vec![20, 30, 10, 40]);

        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(interaction.start_drag(&context(&ids), 40));
        assert!(interaction.update_target(&context(&ids), 20));
        let request = interaction.drop_on(&context(&ids), 20);
        assert!(request.is_some_and(|request| apply(&mut ids, request)));
        assert_eq!(ids, vec![40, 20, 30, 10]);
    }

    #[test]
    fn drag_cancel_end_and_drop_clear_all_transient_state() {
        let current = context(&[1, 2, 3]);
        let mut interaction = QueueInteraction::default();
        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(interaction.start_drag(&current, 1));
        assert!(interaction.update_target(&current, 2));
        interaction.cancel();
        assert_eq!(interaction, QueueInteraction::default());

        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(interaction.start_drag(&current, 1));
        assert!(interaction.update_target(&current, 3));
        assert!(interaction.drop_on(&current, 3).is_some());
        assert_eq!(interaction, QueueInteraction::default());
    }

    #[test]
    fn drag_refuses_busy_nonordered_single_and_interactive_origins() {
        let mut interaction = QueueInteraction::default();
        let busy = QueueContext {
            busy: true,
            ..context(&[1, 2])
        };
        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(!interaction.start_drag(&busy, 1));
        let unordered = QueueContext {
            ordered: false,
            ..context(&[1, 2])
        };
        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(!interaction.start_drag(&unordered, 1));
        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(!interaction.start_drag(&context(&[1]), 1));
        interaction.prepare_drag(DragOrigin::InteractiveDescendant);
        assert!(!interaction.start_drag(&context(&[1, 2]), 1));
        assert_eq!(interaction, QueueInteraction::default());
    }

    #[test]
    fn drag_requires_and_consumes_a_prepared_row_origin() {
        let current = context(&[1, 2]);
        let mut interaction = QueueInteraction::default();

        assert!(!interaction.start_drag(&current, 1));
        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(interaction.start_drag(&current, 1));
        assert_eq!(interaction.pending_drag_origin, None);

        interaction.cancel();
        assert!(!interaction.start_drag(&current, 1));
    }

    #[test]
    fn pointer_release_and_cancel_discard_prepared_origins() {
        let current = context(&[1, 2]);
        let mut interaction = QueueInteraction::default();

        interaction.prepare_drag(DragOrigin::InteractiveDescendant);
        interaction.clear_pending_drag();
        assert!(!interaction.start_drag(&current, 1));

        interaction.prepare_drag(DragOrigin::RowContent);
        interaction.clear_pending_drag();
        assert!(!interaction.start_drag(&current, 1));
        assert_eq!(interaction, QueueInteraction::default());
    }

    #[test]
    fn target_updates_require_an_active_valid_drag() {
        let current = context(&[1, 2, 3]);
        let mut interaction = QueueInteraction::default();
        assert!(!interaction.update_target(&current, 2));
        interaction.prepare_drag(DragOrigin::RowContent);
        assert!(interaction.start_drag(&current, 1));
        assert!(!interaction.update_target(&current, 1));
        assert!(!interaction.update_target(&current, 99));
        assert!(interaction.update_target(&current, 3));
        assert_eq!(interaction.target_id, Some(3));
    }

    #[test]
    fn alt_arrow_moves_with_boundaries_and_plain_arrows_do_nothing() {
        let current = context(&[10, 20, 30]);
        let alt_up = KeyCommand {
            key: QueueKey::ArrowUp,
            alt: true,
        };
        let alt_down = KeyCommand {
            key: QueueKey::ArrowDown,
            alt: true,
        };
        let plain_up = KeyCommand {
            key: QueueKey::ArrowUp,
            alt: false,
        };
        let alt_other = KeyCommand {
            key: QueueKey::Other,
            alt: true,
        };
        assert_eq!(QueueInteraction::keyboard_move(&current, 10, alt_up), None);
        assert_eq!(
            QueueInteraction::keyboard_move(&current, 30, alt_down),
            None
        );
        assert_eq!(
            QueueInteraction::keyboard_move(&current, 20, plain_up),
            None
        );
        assert_eq!(
            QueueInteraction::keyboard_move(&current, 20, alt_other),
            None
        );
        assert_eq!(
            QueueInteraction::keyboard_move(&current, 20, alt_up),
            Some(MoveRequest {
                source_id: 20,
                target_id: 10
            })
        );
        assert_eq!(
            QueueInteraction::keyboard_move(&current, 20, alt_down),
            Some(MoveRequest {
                source_id: 20,
                target_id: 30
            })
        );
    }

    #[test]
    fn keyboard_move_order_and_position_announcement_are_exact() {
        let mut ids = vec![1, 2, 3];
        let request = QueueInteraction::keyboard_move(
            &context(&ids),
            2,
            KeyCommand {
                key: QueueKey::ArrowDown,
                alt: true,
            },
        );
        let target_position = request
            .and_then(|request| context(&ids).position(request.target_id))
            .map(|position| position + 1);
        assert!(request.is_some_and(|request| apply(&mut ids, request)));
        assert_eq!(ids, vec![1, 3, 2]);
        assert_eq!(target_position, Some(3));
        assert_eq!(
            position_announcement("second.pdf", target_position.unwrap_or_default()),
            "second.pdf is now position 3."
        );
    }

    #[test]
    fn typed_semantics_only_enable_ordered_multi_file_idle_rows() {
        assert_eq!(
            context(&[1, 2]).semantics(),
            QueueSemantics {
                draggable: true,
                tab_index: Some(0),
                described_by: Some(REORDER_HELP_ID),
                key_shortcuts: Some(REORDER_SHORTCUTS),
            }
        );
        assert!(
            !QueueContext {
                busy: true,
                ..context(&[1, 2])
            }
            .semantics()
            .draggable
        );
        assert_eq!(context(&[1]).semantics().tab_index, None);
        assert_eq!(
            QueueContext {
                ordered: false,
                ..context(&[1, 2])
            }
            .semantics()
            .key_shortcuts,
            None
        );
    }

    #[test]
    fn output_order_matches_every_multi_file_workflow_that_consumes_files_in_sequence() {
        for operation in [
            Operation::PdfImage,
            Operation::Merge,
            Operation::ImagePdf,
            Operation::Impose,
        ] {
            assert!(operation_uses_file_order(operation, 2));
            assert!(!operation_uses_file_order(operation, 1));
        }
        assert!(!operation_uses_file_order(Operation::Split, 2));
    }
}
