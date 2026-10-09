use super::*;
use project::git_store::CommitDiff;

pub(super) struct SplitFile {
    pub(super) path: RepoPath,
    pub(super) status: git::status::FileStatus,
    pub(super) in_first: bool,
}

/// A split needs files on both sides, otherwise one of the commits would be empty.
pub(super) fn can_split(files: &[SplitFile]) -> bool {
    files.iter().any(|file| file.in_first) && files.iter().any(|file| !file.in_first)
}

/// The messages of the two commits: each gets its own title, and the second keeps the original
/// description, since that is where the rest of the change lands.
pub(super) fn split_messages(
    first_title: &str,
    second_title: &str,
    original_message: &str,
) -> (String, String) {
    let description = original_message
        .trim()
        .split_once('\n')
        .map(|(_, description)| description.trim())
        .unwrap_or_default();
    let first = first_title.trim().to_string();
    let second = if description.is_empty() {
        second_title.trim().to_string()
    } else {
        format!("{}\n\n{description}", second_title.trim())
    };
    (first, second)
}

pub(super) struct SplitModal {
    smartlog: WeakEntity<Smartlog>,
    sha: Oid,
    original_message: String,
    files: Option<Vec<SplitFile>>,
    first_title: Entity<Editor>,
    second_title: Entity<Editor>,
    focus_handle: FocusHandle,
    _load_task: Task<()>,
}

impl SplitModal {
    pub(super) fn new(
        smartlog: WeakEntity<Smartlog>,
        repository: Entity<Repository>,
        sha: Oid,
        subject: &str,
        original_message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let make_editor = |text: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(text, window, cx);
                editor
            })
        };
        let first_title = make_editor(format!("{subject} (part 1)"), window, cx);
        let second_title = make_editor(format!("{subject} (part 2)"), window, cx);

        let diff_task = repository.update(cx, |repository, cx| {
            repository.load_commit_diff(sha.to_string(), false, cx)
        });
        let load_task = cx.spawn(async move |this, cx| {
            let Some(diff) = diff_task.await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.files = Some(files_of(&diff, cx));
                cx.notify();
            })
            .log_err();
        });

        Self {
            smartlog,
            sha,
            original_message,
            files: None,
            first_title,
            second_title,
            focus_handle: cx.focus_handle(),
            _load_task: load_task,
        }
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(files) = &self.files else {
            return;
        };
        if !can_split(files) {
            return;
        }
        let first_paths: Vec<RepoPath> = files
            .iter()
            .filter(|file| file.in_first)
            .map(|file| file.path.clone())
            .collect();
        let (first_message, second_message) = split_messages(
            &self.first_title.read(cx).text(cx),
            &self.second_title.read(cx).text(cx),
            &self.original_message,
        );
        if first_message.is_empty() || second_message.is_empty() {
            return;
        }
        let sha = self.sha;
        self.smartlog
            .update(cx, |smartlog, cx| {
                smartlog.apply_split(sha, first_paths, first_message, second_message, window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }
}

fn files_of(diff: &CommitDiff, cx: &App) -> Vec<SplitFile> {
    diff.files
        .iter()
        .enumerate()
        .map(|(index, file)| SplitFile {
            path: file.path.clone(),
            status: crate::git_graph::ChangedFileEntry::from_commit_file(file, cx).status,
            in_first: index == 0,
        })
        .collect()
}

impl EventEmitter<DismissEvent> for SplitModal {}
impl ModalView for SplitModal {}

impl Focusable for SplitModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SplitModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let can_apply = self.files.as_deref().is_some_and(can_split);
        let field = |editor: &Entity<Editor>| {
            div()
                .w_full()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.editor_background)
                .child(editor.clone())
        };

        let file_rows: Vec<AnyElement> = self
            .files
            .iter()
            .flatten()
            .enumerate()
            .map(|(index, file)| {
                h_flex()
                    .h_8()
                    .gap_2()
                    .px_3()
                    .child(
                        Checkbox::new(
                            ("smartlog-split-file", index),
                            if file.in_first {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .on_click(cx.listener(
                            move |this, state: &ToggleState, _, cx| {
                                if let Some(file) =
                                    this.files.as_mut().and_then(|files| files.get_mut(index))
                                {
                                    file.in_first = *state == ToggleState::Selected;
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .child(crate::git_status_icon(file.status))
                    .child(Label::new(file.path.as_unix_str().to_string()).truncate())
                    .child(div().flex_1())
                    .child(
                        Label::new(if file.in_first {
                            "First commit"
                        } else {
                            "Second commit"
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .key_context("SmartlogSplit")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(DismissEvent)))
            .elevation_2(cx)
            .w(rems(44.))
            .max_h(rems(36.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitCommit).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!("Split {}", self.sha.display_short()))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(
                v_flex()
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .child(
                        Label::new("Tick the files that belong in the first commit.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(field(&self.first_title))
                    .child(field(&self.second_title)),
            )
            .child(
                v_flex()
                    .id("smartlog-split-files")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(self.files.is_none(), |this| {
                        this.child(
                            div().px_3().child(
                                Label::new("Loading…")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .children(file_rows),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .justify_end()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Button::new("smartlog-split-cancel", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("smartlog-split-apply", "Split")
                            .style(ButtonStyle::Filled)
                            .disabled(!can_apply)
                            .tooltip(Tooltip::text(if can_apply {
                                "Split this commit into two"
                            } else {
                                "Put at least one file in each commit"
                            }))
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

    fn file(name: &str, in_first: bool) -> SplitFile {
        SplitFile {
            path: git::repository::repo_path(name),
            status: git::status::FileStatus::Untracked,
            in_first,
        }
    }

    #[test]
    fn a_split_needs_files_on_both_sides() {
        assert!(can_split(&[file("a", true), file("b", false)]));
        assert!(!can_split(&[file("a", true), file("b", true)]));
        assert!(!can_split(&[file("a", false)]));
        assert!(!can_split(&[]));
    }

    #[test]
    fn the_second_commit_keeps_the_description() {
        assert_eq!(
            split_messages(" First ", " Second ", "Original\n\nWhy it matters.\n"),
            ("First".to_string(), "Second\n\nWhy it matters.".to_string())
        );
        assert_eq!(
            split_messages("First", "Second", "Original only"),
            ("First".to_string(), "Second".to_string())
        );
    }
}
