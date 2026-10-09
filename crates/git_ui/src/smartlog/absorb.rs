use super::*;
use git::repository::AbsorbPlan;

pub(super) struct AbsorbModal {
    smartlog: WeakEntity<Smartlog>,
    base: Oid,
    plan: Option<AbsorbPlan>,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    _load_task: Task<()>,
}

/// How a hunk is shown in the plan: `path:line`, with the number of lines it touches.
pub(super) fn describe_hunk(path: &str, old_start: u32, old_lines: u32, new_lines: u32) -> String {
    let location = format!("{path}:{old_start}");
    match (old_lines, new_lines) {
        (0, added) => format!("{location} (+{added})"),
        (removed, 0) => format!("{location} (−{removed})"),
        (_, _) => location,
    }
}

impl AbsorbModal {
    pub(super) fn new(
        smartlog: WeakEntity<Smartlog>,
        repository: Entity<Repository>,
        base: Oid,
        cx: &mut Context<Self>,
    ) -> Self {
        let receiver = repository.update(cx, |repository, _| {
            repository.absorb(base.to_string(), false)
        });
        let load_task = cx.spawn(async move |this, cx| {
            let result = match receiver.await {
                Ok(result) => result,
                Err(_) => return,
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(plan) => this.plan = Some(plan),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        });
        Self {
            smartlog,
            base,
            plan: None,
            error: None,
            focus_handle: cx.focus_handle(),
            _load_task: load_task,
        }
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let base = self.base;
        self.smartlog
            .update(cx, |smartlog, cx| smartlog.apply_absorb(base, window, cx))
            .log_err();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for AbsorbModal {}
impl ModalView for AbsorbModal {}

impl Focusable for AbsorbModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AbsorbModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let can_apply = self
            .plan
            .as_ref()
            .is_some_and(|plan| !plan.assignments.is_empty());

        let body = match (&self.plan, &self.error) {
            (_, Some(error)) => v_flex()
                .px_3()
                .child(Label::new(error.clone()).color(Color::Error))
                .into_any_element(),
            (None, None) => v_flex()
                .px_3()
                .child(
                    Label::new("Looking for the commit each change belongs to…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            (Some(plan), None) if plan.assignments.is_empty() && plan.unassigned.is_empty() => {
                v_flex()
                    .px_3()
                    .child(
                        Label::new("There are no uncommitted changes to absorb.")
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
            (Some(plan), None) => {
                let smartlog = self.smartlog.upgrade();
                v_flex()
                    .px_3()
                    .gap_2()
                    .children(plan.assignments.iter().map(|assignment| {
                        let subject = assignment
                            .commit
                            .parse::<Oid>()
                            .ok()
                            .and_then(|sha| {
                                smartlog.as_ref().and_then(|smartlog| {
                                    smartlog
                                        .read(cx)
                                        .commits
                                        .get(&sha)
                                        .map(|commit| commit.subject.to_string())
                                })
                            })
                            .unwrap_or_else(|| assignment.commit.chars().take(8).collect());
                        v_flex()
                            .gap_0p5()
                            .child(Label::new(subject).weight(gpui::FontWeight::BOLD))
                            .children(assignment.hunks.iter().map(|hunk| {
                                Label::new(describe_hunk(
                                    hunk.path.as_unix_str(),
                                    hunk.old_start,
                                    hunk.old_lines,
                                    hunk.new_lines,
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                            }))
                    }))
                    .when(!plan.unassigned.is_empty(), |this| {
                        this.child(
                            Label::new(format!(
                                "{} changes stay uncommitted because they touch lines from more than one commit, or from outside this stack.",
                                plan.unassigned.len()
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .key_context("SmartlogAbsorb")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(DismissEvent)))
            .elevation_2(cx)
            .w(rems(44.))
            .max_h(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_2()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::ArrowDown).size(IconSize::XSmall))
                    .child(Headline::new("Absorb Changes").size(HeadlineSize::XSmall)),
            )
            .child(
                div()
                    .id("smartlog-absorb-plan")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb_2()
                    .child(body),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .justify_end()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Button::new("smartlog-absorb-cancel", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("smartlog-absorb-apply", "Absorb")
                            .style(ButtonStyle::Filled)
                            .disabled(!can_apply)
                            .tooltip(Tooltip::text(
                                "Fold each change into the commit that last touched its lines",
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply(window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_are_described_by_location_and_size() {
        assert_eq!(describe_hunk("src/a.rs", 12, 3, 3), "src/a.rs:12");
        assert_eq!(describe_hunk("src/a.rs", 12, 0, 4), "src/a.rs:12 (+4)");
        assert_eq!(describe_hunk("src/a.rs", 12, 2, 0), "src/a.rs:12 (−2)");
    }
}
