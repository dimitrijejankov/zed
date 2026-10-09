use super::*;
use git::repository::StackStep;

/// The commits of the stack rooted at `root`, oldest first, when it is one unbroken line with
/// nothing forking off it. Anything else can't be rebuilt as a list of steps.
pub(super) fn linear_stack(parents: &HashMap<Oid, Option<Oid>>, root: Oid) -> Option<Vec<Oid>> {
    let mut chain = vec![root];
    loop {
        let last = *chain.last()?;
        let mut children = parents
            .iter()
            .filter(|(_, parent)| **parent == Some(last))
            .map(|(child, _)| *child);
        let Some(child) = children.next() else {
            return Some(chain);
        };
        if children.next().is_some() || chain.contains(&child) {
            return None;
        }
        chain.push(child);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StackEntry {
    pub(super) sha: Oid,
    pub(super) subject: SharedString,
    pub(super) message: String,
    pub(super) dropped: bool,
    /// Combine this commit with the kept commit just before it.
    pub(super) fold_into_previous: bool,
}

/// The steps that produce the edited stack, oldest first. A folded step gets the messages of all
/// its commits joined, since none of them should be silently lost.
pub(super) fn build_steps(entries: &[StackEntry]) -> Vec<StackStep> {
    let mut steps: Vec<StackStep> = Vec::new();
    let mut messages: Vec<Vec<&str>> = Vec::new();
    for entry in entries.iter().filter(|entry| !entry.dropped) {
        match (
            entry.fold_into_previous,
            steps.last_mut(),
            messages.last_mut(),
        ) {
            (true, Some(step), Some(step_messages)) => {
                step.sources.push(entry.sha.to_string());
                step_messages.push(entry.message.trim());
            }
            _ => {
                steps.push(StackStep {
                    sources: vec![entry.sha.to_string()],
                    message: None,
                });
                messages.push(vec![entry.message.trim()]);
            }
        }
    }
    for (step, step_messages) in steps.iter_mut().zip(messages) {
        if step.sources.len() > 1 {
            step.message = Some(step_messages.join("\n\n"));
        }
    }
    steps
}

pub(super) fn is_changed(entries: &[StackEntry], original_order: &[Oid]) -> bool {
    let order: Vec<Oid> = entries.iter().map(|entry| entry.sha).collect();
    order != original_order
        || entries
            .iter()
            .any(|entry| entry.dropped || entry.fold_into_previous)
}

pub(super) struct EditStackModal {
    smartlog: WeakEntity<Smartlog>,
    base: Oid,
    tip: Oid,
    original_order: Vec<Oid>,
    /// Oldest first.
    entries: Vec<StackEntry>,
    focus_handle: FocusHandle,
}

impl EditStackModal {
    pub(super) fn new(
        smartlog: WeakEntity<Smartlog>,
        base: Oid,
        tip: Oid,
        entries: Vec<StackEntry>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            smartlog,
            base,
            tip,
            original_order: entries.iter().map(|entry| entry.sha).collect(),
            entries,
            focus_handle: cx.focus_handle(),
        }
    }

    fn move_entry(&mut self, index: usize, towards_newer: bool, cx: &mut Context<Self>) {
        let other = if towards_newer {
            index + 1
        } else {
            match index.checked_sub(1) {
                Some(other) => other,
                None => return,
            }
        };
        if other >= self.entries.len() {
            return;
        }
        self.entries.swap(index, other);
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let steps = build_steps(&self.entries);
        let (base, tip) = (self.base, self.tip);
        self.smartlog
            .update(cx, |smartlog, cx| {
                smartlog.apply_stack_edit(base, tip, steps, window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for EditStackModal {}
impl ModalView for EditStackModal {}

impl Focusable for EditStackModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditStackModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let changed = is_changed(&self.entries, &self.original_order);
        let kept = self.entries.iter().filter(|entry| !entry.dropped).count();
        let last = self.entries.len().saturating_sub(1);
        let first_kept = self.entries.iter().position(|entry| !entry.dropped);

        let rows = self.entries.iter().enumerate().rev().map(|(index, entry)| {
            let can_fold = first_kept.is_some_and(|first| index > first) && !entry.dropped;
            h_flex()
                .h_9()
                .gap_2()
                .px_3()
                .child(
                    Label::new(entry.sha.display_short())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(entry.subject.clone())
                            .truncate()
                            .when(entry.dropped, |label| label.strikethrough())
                            .color(if entry.dropped {
                                Color::Muted
                            } else {
                                Color::Default
                            }),
                    ),
                )
                .child(
                    IconButton::new(("smartlog-stack-newer", index), IconName::ArrowUp)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .disabled(index == last)
                        .tooltip(Tooltip::text("Move later"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.move_entry(index, true, cx);
                        })),
                )
                .child(
                    IconButton::new(("smartlog-stack-older", index), IconName::ArrowDown)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .disabled(index == 0)
                        .tooltip(Tooltip::text("Move earlier"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.move_entry(index, false, cx);
                        })),
                )
                .child(
                    IconButton::new(("smartlog-stack-fold", index), IconName::FoldVertical)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .toggle_state(entry.fold_into_previous && can_fold)
                        .disabled(!can_fold)
                        .tooltip(Tooltip::text("Combine with the commit before it"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(entry) = this.entries.get_mut(index) {
                                entry.fold_into_previous = !entry.fold_into_previous;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    IconButton::new(
                        ("smartlog-stack-drop", index),
                        if entry.dropped {
                            IconName::Undo
                        } else {
                            IconName::Trash
                        },
                    )
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(if entry.dropped {
                        "Keep this commit"
                    } else {
                        "Drop this commit"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(entry) = this.entries.get_mut(index) {
                            entry.dropped = !entry.dropped;
                        }
                        cx.notify();
                    })),
                )
        });

        v_flex()
            .key_context("SmartlogEditStack")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(DismissEvent)))
            .elevation_2(cx)
            .w(rems(44.))
            .max_h(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::ListTree).size(IconSize::XSmall))
                    .child(Headline::new("Edit Stack").size(HeadlineSize::XSmall)),
            )
            .child(
                v_flex()
                    .id("smartlog-edit-stack-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .justify_between()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Label::new(format!(
                            "{} commits become {kept}",
                            self.entries.len()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("smartlog-edit-stack-cancel", "Cancel")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new("smartlog-edit-stack-apply", "Apply")
                                    .style(ButtonStyle::Filled)
                                    .disabled(!changed || kept == 0)
                                    .tooltip(Tooltip::text(
                                        "Rebuild the stack. Nothing changes if the new order conflicts.",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.apply(window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        Oid::from_bytes(&[byte; 20]).expect("20 bytes form a valid sha1 oid")
    }

    fn entry(byte: u8) -> StackEntry {
        StackEntry {
            sha: oid(byte),
            subject: SharedString::default(),
            message: format!("commit {byte}"),
            dropped: false,
            fold_into_previous: false,
        }
    }

    #[test]
    fn only_an_unbroken_line_is_a_linear_stack() {
        let (trunk, a, b, c, d) = (oid(1), oid(2), oid(3), oid(4), oid(5));
        let parents: HashMap<Oid, Option<Oid>> = [(a, Some(trunk)), (b, Some(a)), (c, Some(b))]
            .into_iter()
            .collect();
        assert_eq!(linear_stack(&parents, a), Some(vec![a, b, c]));
        assert_eq!(linear_stack(&parents, b), Some(vec![b, c]));

        let forked: HashMap<Oid, Option<Oid>> =
            [(a, Some(trunk)), (b, Some(a)), (c, Some(a)), (d, Some(b))]
                .into_iter()
                .collect();
        assert_eq!(linear_stack(&forked, a), None);
        assert_eq!(linear_stack(&forked, b), Some(vec![b, d]));
    }

    #[test]
    fn untouched_entries_are_one_step_each_and_not_a_change() {
        let entries = vec![entry(1), entry(2), entry(3)];
        let original: Vec<Oid> = entries.iter().map(|entry| entry.sha).collect();

        assert!(!is_changed(&entries, &original));
        let steps = build_steps(&entries);
        assert_eq!(steps.len(), 3);
        assert!(
            steps
                .iter()
                .all(|step| step.sources.len() == 1 && step.message.is_none())
        );
    }

    #[test]
    fn reordering_dropping_and_folding_are_changes() {
        let original = vec![oid(1), oid(2), oid(3)];

        let mut reordered = vec![entry(1), entry(2), entry(3)];
        reordered.swap(0, 2);
        assert!(is_changed(&reordered, &original));
        assert_eq!(
            build_steps(&reordered)
                .iter()
                .map(|step| step.sources[0].clone())
                .collect::<Vec<_>>(),
            vec![oid(3).to_string(), oid(2).to_string(), oid(1).to_string()]
        );

        let mut dropped = vec![entry(1), entry(2), entry(3)];
        dropped[1].dropped = true;
        assert!(is_changed(&dropped, &original));
        assert_eq!(build_steps(&dropped).len(), 2);

        let mut folded = vec![entry(1), entry(2), entry(3)];
        folded[1].fold_into_previous = true;
        assert!(is_changed(&folded, &original));
    }

    #[test]
    fn folding_joins_messages_and_skips_over_dropped_commits() {
        let mut entries = vec![entry(1), entry(2), entry(3), entry(4)];
        entries[1].dropped = true;
        entries[2].fold_into_previous = true;
        entries[3].fold_into_previous = true;

        let steps = build_steps(&entries);
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].sources,
            vec![oid(1).to_string(), oid(3).to_string(), oid(4).to_string()]
        );
        assert_eq!(
            steps[0].message.as_deref(),
            Some("commit 1\n\ncommit 3\n\ncommit 4")
        );
    }

    #[test]
    fn folding_the_first_commit_has_nothing_to_fold_into() {
        let mut entries = vec![entry(1), entry(2)];
        entries[0].fold_into_previous = true;

        let steps = build_steps(&entries);
        assert_eq!(steps.len(), 2);
        assert!(steps[0].message.is_none());
    }
}
