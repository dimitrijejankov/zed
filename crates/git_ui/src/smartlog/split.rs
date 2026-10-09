use super::*;
use project::git_store::CommitDiff;

pub(super) struct SplitHunk {
    pub(super) index: u32,
    pub(super) header: String,
    pub(super) body: String,
    pub(super) in_first: bool,
}

pub(super) struct SplitFile {
    pub(super) path: RepoPath,
    pub(super) status: git::status::FileStatus,
    /// Only used for a file without `hunks`, which moves as a whole.
    pub(super) in_first: bool,
    pub(super) hunks: Vec<SplitHunk>,
    pub(super) expanded: bool,
}

impl SplitFile {
    fn units(&self) -> (usize, usize) {
        if self.hunks.is_empty() {
            (usize::from(self.in_first), 1)
        } else {
            (
                self.hunks.iter().filter(|hunk| hunk.in_first).count(),
                self.hunks.len(),
            )
        }
    }

    fn toggle_state(&self) -> ToggleState {
        match self.units() {
            (0, _) => ToggleState::Unselected,
            (first, total) if first == total => ToggleState::Selected,
            _ => ToggleState::Indeterminate,
        }
    }

    fn set_all(&mut self, in_first: bool) {
        self.in_first = in_first;
        for hunk in &mut self.hunks {
            hunk.in_first = in_first;
        }
    }
}

/// A split needs changes on both sides, otherwise one of the commits would be empty.
pub(super) fn can_split(files: &[SplitFile]) -> bool {
    let (first, total) = files.iter().fold((0, 0), |(first, total), file| {
        let (file_first, file_total) = file.units();
        (first + file_first, total + file_total)
    });
    first > 0 && first < total
}

/// Splits the choices into whole files and, for files only partly in the first commit, the hunks.
pub(super) fn first_commit_selection(
    files: &[SplitFile],
) -> (Vec<RepoPath>, Vec<git::repository::HunkSelection>) {
    let mut paths = Vec::new();
    let mut selections = Vec::new();
    for file in files {
        match file.units() {
            (0, _) => {}
            (first, total) if first == total => paths.push(file.path.clone()),
            _ => selections.push(git::repository::HunkSelection {
                path: file.path.clone(),
                hunks: file
                    .hunks
                    .iter()
                    .filter(|hunk| hunk.in_first)
                    .map(|hunk| hunk.index)
                    .collect(),
            }),
        }
    }
    (paths, selections)
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
        let hunks_task =
            repository.update(cx, |repository, _| repository.commit_hunks(sha.to_string()));
        let load_task = cx.spawn(async move |this, cx| {
            let Some(diff) = diff_task.await.log_err() else {
                return;
            };
            let hunks = match hunks_task.await {
                Ok(Ok(hunks)) => hunks,
                Ok(Err(error)) => {
                    log::error!("Failed to list the hunks of the commit: {error:#}");
                    Vec::new()
                }
                Err(error) => {
                    log::error!("Failed to list the hunks of the commit: {error:#}");
                    Vec::new()
                }
            };
            this.update(cx, |this, cx| {
                this.files = Some(files_of(&diff, hunks, cx));
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
        let (first_paths, first_hunks) = first_commit_selection(files);
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
                smartlog.apply_split(
                    sha,
                    first_paths,
                    first_hunks,
                    first_message,
                    second_message,
                    window,
                    cx,
                );
            })
            .log_err();
        cx.emit(DismissEvent);
    }
}

fn files_of(
    diff: &CommitDiff,
    hunks: Vec<git::repository::CommitHunk>,
    cx: &App,
) -> Vec<SplitFile> {
    let mut files: Vec<SplitFile> = diff
        .files
        .iter()
        .map(|file| SplitFile {
            path: file.path.clone(),
            status: crate::git_graph::ChangedFileEntry::from_commit_file(file, cx).status,
            in_first: false,
            hunks: hunks
                .iter()
                .filter(|hunk| hunk.path == file.path)
                .map(|hunk| SplitHunk {
                    index: hunk.index,
                    header: hunk.header.clone(),
                    body: hunk.body.clone(),
                    in_first: false,
                })
                .collect(),
            expanded: false,
        })
        .collect();
    if let Some(first) = files.first_mut() {
        first.set_all(true);
    }
    files
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

        let mut file_rows: Vec<AnyElement> = Vec::new();
        for (index, file) in self.files.iter().flatten().enumerate() {
            let has_hunks = !file.hunks.is_empty();
            let (first_units, total_units) = file.units();
            file_rows.push(
                h_flex()
                    .h_8()
                    .gap_2()
                    .px_3()
                    .child(if has_hunks {
                        IconButton::new(
                            ("smartlog-split-expand", index),
                            if file.expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                        )
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(file) =
                                this.files.as_mut().and_then(|files| files.get_mut(index))
                            {
                                file.expanded = !file.expanded;
                            }
                            cx.notify();
                        }))
                        .into_any_element()
                    } else {
                        div().size_6().into_any_element()
                    })
                    .child(
                        Checkbox::new(("smartlog-split-file", index), file.toggle_state())
                            .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                                if let Some(file) =
                                    this.files.as_mut().and_then(|files| files.get_mut(index))
                                {
                                    file.set_all(*state == ToggleState::Selected);
                                }
                                cx.notify();
                            })),
                    )
                    .child(crate::git_status_icon(file.status))
                    .child(Label::new(file.path.as_unix_str().to_string()).truncate())
                    .child(div().flex_1())
                    .child(
                        Label::new(
                            if has_hunks && first_units > 0 && first_units < total_units {
                                format!("{first_units} of {total_units} hunks in the first commit")
                            } else if first_units > 0 {
                                "First commit".to_string()
                            } else {
                                "Second commit".to_string()
                            },
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element(),
            );
            if !file.expanded {
                continue;
            }
            for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                file_rows.push(
                    v_flex()
                        .pl_12()
                        .pr_3()
                        .py_1()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Checkbox::new(
                                        ("smartlog-split-hunk", index * 10_000 + hunk_index),
                                        if hunk.in_first {
                                            ToggleState::Selected
                                        } else {
                                            ToggleState::Unselected
                                        },
                                    )
                                    .on_click(cx.listener(
                                        move |this, state: &ToggleState, _, cx| {
                                            if let Some(hunk) = this
                                                .files
                                                .as_mut()
                                                .and_then(|files| files.get_mut(index))
                                                .and_then(|file| file.hunks.get_mut(hunk_index))
                                            {
                                                hunk.in_first = *state == ToggleState::Selected;
                                            }
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    Label::new(hunk.header.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        )
                        .child(
                            div().pl_6().child(
                                Label::new(
                                    hunk.body.lines().take(6).collect::<Vec<_>>().join("\n"),
                                )
                                .size(LabelSize::XSmall)
                                .buffer_font(cx),
                            ),
                        )
                        .into_any_element(),
                );
            }
        }

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
                        Label::new("Tick the files, or expand one to tick single hunks, for the first commit.")
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
                                "Put at least one change in each commit"
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
            hunks: Vec::new(),
            expanded: false,
        }
    }

    fn hunked_file(name: &str, hunks: &[bool]) -> SplitFile {
        SplitFile {
            hunks: hunks
                .iter()
                .enumerate()
                .map(|(index, in_first)| SplitHunk {
                    index: index as u32,
                    header: String::new(),
                    body: String::new(),
                    in_first: *in_first,
                })
                .collect(),
            ..file(name, false)
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
    fn hunks_of_one_file_can_be_split_between_the_commits() {
        let files = [hunked_file("a", &[true, false])];
        assert!(can_split(&files));
        assert_eq!(files[0].toggle_state(), ToggleState::Indeterminate);
        let (paths, selections) = first_commit_selection(&files);
        assert!(paths.is_empty());
        assert_eq!(selections.len(), 1);
        assert_eq!(selections[0].hunks, vec![0]);

        let all = [hunked_file("a", &[true, true])];
        assert!(!can_split(&all));
        assert_eq!(first_commit_selection(&all).0.len(), 1);
        assert!(first_commit_selection(&all).1.is_empty());

        let mixed = [hunked_file("a", &[false, false]), file("b", true)];
        assert!(can_split(&mixed));
        let (paths, selections) = first_commit_selection(&mixed);
        assert_eq!(paths.len(), 1);
        assert!(selections.is_empty());
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
