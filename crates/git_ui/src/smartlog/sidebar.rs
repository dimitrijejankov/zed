use super::*;
use crate::git_graph::{ChangedFileEntry, compute_diff_stats, format_timestamp};
use project::git_store::CommitDiff;
use ui::Divider;

const MIN_SIDEBAR_WIDTH: Pixels = px(300.0);
const DEFAULT_LIST_FRACTION: f32 = 0.58;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Message {
    pub(super) title: String,
    pub(super) description: String,
}

impl Message {
    fn parse(message: &str) -> Self {
        let message = message.trim_end();
        match message.split_once('\n') {
            Some((title, description)) => Self {
                title: title.trim_end().to_string(),
                description: description.trim().to_string(),
            },
            None => Self {
                title: message.to_string(),
                description: String::new(),
            },
        }
    }

    fn serialize(&self) -> String {
        let title = self.title.trim();
        let description = self.description.trim();
        if description.is_empty() {
            title.to_string()
        } else {
            format!("{title}\n\n{description}")
        }
    }

    fn is_equivalent_to(&self, other: &Self) -> bool {
        self.title.trim() == other.title.trim()
            && self.description.trim() == other.description.trim()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FilledMessage {
    sha: Oid,
    is_draft: bool,
    original: Message,
}

pub(super) struct SidebarState {
    pub(super) split: Entity<SplitState>,
    pub(super) collapsed: bool,
    commit: Option<Oid>,
    filled: Option<FilledMessage>,
    title_editor: Entity<Editor>,
    description_editor: Entity<Editor>,
    edited_messages: HashMap<Oid, Message>,
    commit_mode: bool,
    commit_draft: Message,
    diff: Option<CommitDiff>,
    diff_stats: Option<(usize, usize)>,
    diff_task: Option<Task<()>>,
    load_public_files: bool,
}

impl SidebarState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Smartlog>) -> Self {
        let title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Title", window, cx);
            editor
        });
        let description_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text("Description", window, cx);
            editor
        });
        Self {
            split: cx.new(|_| SplitState::with_left_ratio(DEFAULT_LIST_FRACTION)),
            collapsed: false,
            commit: None,
            filled: None,
            title_editor,
            description_editor,
            edited_messages: HashMap::default(),
            commit_mode: false,
            commit_draft: Message::default(),
            diff: None,
            diff_stats: None,
            diff_task: None,
            load_public_files: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimaryAction {
    Commit,
    Amend,
    AmendMessage,
}

/// Which of Commit, Amend and Amend Message the action bar offers, following ISL: the head
/// commit offers Commit/Amend when there is something to commit (or nothing is being edited),
/// and every other case can only rewrite the message.
fn primary_action(
    is_head: bool,
    commit_mode: bool,
    anything_to_commit: bool,
    editing: bool,
) -> PrimaryAction {
    let shows_commit_or_amend = is_head && (commit_mode || anything_to_commit || !editing);
    match (shows_commit_or_amend, commit_mode) {
        (true, true) => PrimaryAction::Commit,
        (true, false) => PrimaryAction::Amend,
        (false, _) => PrimaryAction::AmendMessage,
    }
}

fn submit_label(
    commit_mode: bool,
    anything_to_commit: bool,
    primary: PrimaryAction,
) -> &'static str {
    match primary {
        PrimaryAction::Commit if commit_mode => "Commit and Submit",
        PrimaryAction::Amend if anything_to_commit => "Amend and Submit",
        _ => "Submit",
    }
}

impl Smartlog {
    fn sidebar_target(&self) -> Option<Oid> {
        match self.selection.len() {
            0 => self.head,
            1 => self.selection.iter().next().copied(),
            _ => None,
        }
    }

    fn sidebar_target_is_public(&self) -> bool {
        let Some(target) = self.sidebar.commit else {
            return false;
        };
        self.layout
            .rows
            .iter()
            .any(|row| row.sha == Some(target) && row.kind == RowKind::Public)
    }

    fn sidebar_target_is_head(&self) -> bool {
        self.sidebar.commit.is_some() && self.sidebar.commit == self.head
    }

    fn sidebar_in_commit_mode(&self) -> bool {
        self.sidebar.commit_mode && self.sidebar_target_is_head()
    }

    fn current_editor_message(&self, cx: &App) -> Message {
        Message {
            title: self.sidebar.title_editor.read(cx).text(cx),
            description: self.sidebar.description_editor.read(cx).text(cx),
        }
    }

    fn sidebar_message_is_edited(&self, cx: &App) -> bool {
        match &self.sidebar.filled {
            Some(filled) if !filled.is_draft => !self
                .current_editor_message(cx)
                .is_equivalent_to(&filled.original),
            _ => false,
        }
    }

    fn stash_sidebar_edits(&mut self, cx: &App) {
        let Some(filled) = self.sidebar.filled.clone() else {
            return;
        };
        let current = self.current_editor_message(cx);
        if filled.is_draft {
            self.sidebar.commit_draft = current;
        } else if current.is_equivalent_to(&filled.original) {
            self.sidebar.edited_messages.remove(&filled.sha);
        } else {
            self.sidebar.edited_messages.insert(filled.sha, current);
        }
    }

    pub(super) fn sync_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.sidebar_target();
        if target != self.sidebar.commit {
            self.stash_sidebar_edits(cx);
            self.sidebar.commit = target;
            self.sidebar.filled = None;
            self.sidebar.diff = None;
            self.sidebar.diff_stats = None;
            self.sidebar.diff_task = None;
            self.sidebar.load_public_files = false;
            self.load_sidebar_diff(cx);
        }
        self.fill_sidebar_editors(window, cx);
        cx.notify();
    }

    fn fill_sidebar_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sha) = self.sidebar.commit else {
            return;
        };
        let Some(commit) = self.commits.get(&sha) else {
            return;
        };
        let is_draft = self.sidebar_in_commit_mode();
        let original = if is_draft {
            Message::default()
        } else {
            Message::parse(&commit.message)
        };
        let expected = FilledMessage {
            sha,
            is_draft,
            original: original.clone(),
        };

        if let Some(filled) = &self.sidebar.filled {
            if *filled == expected {
                return;
            }
            let was_edited = filled.sha == sha
                && filled.is_draft == is_draft
                && !self
                    .current_editor_message(cx)
                    .is_equivalent_to(&filled.original);
            if was_edited {
                self.sidebar.filled = Some(expected);
                return;
            }
        }

        let message = if is_draft {
            self.sidebar.commit_draft.clone()
        } else {
            self.sidebar
                .edited_messages
                .get(&sha)
                .cloned()
                .unwrap_or(original)
        };
        let read_only = self.sidebar_target_is_public() && !is_draft;
        self.sidebar.title_editor.update(cx, |editor, cx| {
            editor.set_text(message.title, window, cx);
            editor.set_read_only(read_only);
        });
        self.sidebar.description_editor.update(cx, |editor, cx| {
            editor.set_text(message.description, window, cx);
            editor.set_read_only(read_only);
        });
        self.sidebar.filled = Some(expected);
    }

    fn load_sidebar_diff(&mut self, cx: &mut Context<Self>) {
        let Some(sha) = self.sidebar.commit else {
            return;
        };
        if self.sidebar_target_is_public() && !self.sidebar.load_public_files {
            return;
        }
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let diff_task = repository.update(cx, |repository, cx| {
            repository.load_commit_diff(sha.to_string(), false, cx)
        });
        self.sidebar.diff_task = Some(cx.spawn(async move |this, cx| {
            let Some(diff) = diff_task.await.log_err() else {
                return;
            };
            let (diff, stats) = cx
                .background_spawn(async move {
                    let stats = compute_diff_stats(&diff);
                    (diff, stats)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.sidebar.commit == Some(sha) {
                    this.sidebar.diff = Some(diff);
                    this.sidebar.diff_stats = Some(stats);
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn set_commit_mode(&mut self, commit_mode: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar.commit_mode == commit_mode {
            return;
        }
        self.stash_sidebar_edits(cx);
        self.sidebar.commit_mode = commit_mode;
        self.sidebar.filled = None;
        self.fill_sidebar_editors(window, cx);
        cx.notify();
    }

    fn cancel_sidebar_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sidebar_message_is_edited(cx) {
            return;
        }
        let prompt = window.prompt(
            PromptLevel::Warning,
            "Are you sure you want to discard your edited message?",
            None,
            &["Discard", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }
            this.update_in(cx, |this, window, cx| {
                if let Some(sha) = this.sidebar.commit {
                    this.sidebar.edited_messages.remove(&sha);
                }
                this.sidebar.filled = None;
                this.fill_sidebar_editors(window, cx);
                cx.notify();
            })?;
            Ok(())
        })
        .detach_and_prompt_err("Failed to discard edits", window, cx, |_, _, _| None);
    }

    fn amend_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sha) = self.sidebar.commit else {
            return;
        };
        let Some(repository) = self.repository(cx) else {
            return;
        };
        if !self.sidebar_message_is_edited(cx) {
            return;
        }
        let message = self.current_editor_message(cx).serialize();
        if message.is_empty() {
            return;
        }

        let task = cx.spawn_in(window, async move |this, cx| {
            let new_sha = repository
                .update(cx, |repository, _| {
                    repository.reword_commit(sha.to_string(), message)
                })
                .await??;
            this.update_in(cx, |this, window, cx| {
                this.sidebar.edited_messages.remove(&sha);
                this.sidebar.filled = None;
                this.pending_selection = new_sha.parse::<Oid>().ok();
                this.refresh(window, cx);
            })?;
            anyhow::Ok(())
        });
        self.run_logged("Failed to amend commit message", task, window, cx);
    }

    fn sidebar_commit_message(&self, cx: &App) -> Option<SharedString> {
        let message = self.current_editor_message(cx);
        let serialized = message.serialize();
        if serialized.is_empty() {
            return None;
        }
        Some(serialized.into())
    }

    fn submit_branch(&self, cx: &App) -> Option<String> {
        let target = self.sidebar.commit?;
        if self.sidebar_target_is_head() {
            let repository = self.repository(cx)?;
            return repository
                .read(cx)
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string());
        }
        self.local_branch_at(target, cx)
    }

    fn submit(&mut self, commit_first: Option<bool>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let Some(branch) = self.submit_branch(cx) else {
            return;
        };
        let commit_task = commit_first.and_then(|amend| {
            let message = self.sidebar_commit_message(cx);
            self.commit_changes(amend, message, window, cx)
        });
        let has_upstream = repository.read(cx).branch_list.iter().any(|candidate| {
            !candidate.is_remote() && candidate.name() == branch && candidate.upstream.is_some()
        });
        let askpass = self.askpass_delegate("git push", window, cx);
        let workspace = self.workspace.clone();

        let task = cx.spawn_in(window, async move |_, cx| {
            if let Some(commit_task) = commit_task {
                commit_task.await?;
            }
            let remotes = repository
                .update(cx, |repository, _| {
                    repository.get_remotes(Some(branch.clone()), true)
                })
                .await??;
            let remote = remotes
                .into_iter()
                .next()
                .context("No remote is available to push to")?;
            let options = (!has_upstream).then_some(git::repository::PushOptions::SetUpstream);
            repository
                .update(cx, |repository, cx| {
                    repository.push(
                        branch.clone().into(),
                        branch.clone().into(),
                        remote.name.clone(),
                        options,
                        askpass,
                        cx,
                    )
                })
                .await??;
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<Smartlog>(),
                        format!("Pushed {branch} to {}", remote.name),
                    ),
                    cx,
                );
            })?;
            anyhow::Ok(())
        });
        self.run_logged("Failed to submit", task, window, cx);
    }

    fn open_all_changed_files(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(diff) = &self.sidebar.diff else {
            return;
        };
        let paths: Vec<RepoPath> = diff
            .files
            .iter()
            .filter(|file| file.new_text.is_some())
            .map(|file| file.path.clone())
            .take(25)
            .collect();
        for path in paths {
            self.open_file(&path, window, cx);
        }
    }

    pub(super) fn render_sidebar_split_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        let split = self.sidebar.split.clone();
        div()
            .id("smartlog-sidebar-resize-container")
            .relative()
            .h_full()
            .flex_shrink_0()
            .w(px(1.))
            .bg(cx.theme().colors().border_variant)
            .child(
                div()
                    .id("smartlog-sidebar-resize-handle")
                    .absolute()
                    .left(px(-RESIZE_HANDLE_WIDTH / 2.0))
                    .w(px(RESIZE_HANDLE_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_click(move |event: &ClickEvent, _, cx| {
                        if event.click_count() >= 2 {
                            split.update(cx, |state, _| state.on_double_click());
                        }
                        cx.stop_propagation();
                    })
                    .on_drag(DraggedSplitHandle, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            )
            .into_any_element()
    }

    pub(super) fn render_collapsed_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .h_full()
            .flex_none()
            .px_1()
            .py_2()
            .border_l_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                IconButton::new("smartlog-expand-sidebar", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Show Commit Info"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar.collapsed = false;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn section_title(&self, title: &'static str) -> Label {
        Label::new(title)
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .weight(gpui::FontWeight::BOLD)
    }

    pub(super) fn badge(&self, text: impl Into<SharedString>, cx: &App) -> AnyElement {
        div()
            .flex_none()
            .px_1p5()
            .rounded_full()
            .bg(cx.theme().colors().element_background)
            .child(Label::new(text.into()).size(LabelSize::XSmall))
            .into_any_element()
    }

    fn message_field(&self, editor: &Entity<Editor>, read_only: bool, cx: &App) -> AnyElement {
        let colors = cx.theme().colors();
        div()
            .w_full()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(if read_only {
                gpui::transparent_black()
            } else {
                colors.border_variant
            })
            .bg(colors.editor_background)
            .child(editor.clone())
            .into_any_element()
    }

    fn render_byline(&self, sha: Oid, cx: &mut Context<Self>) -> AnyElement {
        let is_head = self.sidebar_target_is_head();
        let is_public = self.sidebar_target_is_public();
        let commit = self.commits.get(&sha).cloned();
        let copy_text = self.copy_hash_text(sha);

        h_flex()
            .gap_2()
            .flex_wrap()
            .when(is_head, |this| this.child(Self::you_are_here_pill(cx)))
            .when(is_public, |this| {
                this.child(
                    div()
                        .id("smartlog-public-badge")
                        .flex_none()
                        .px_2()
                        .rounded_full()
                        .bg(cx.theme().colors().element_background)
                        .child(Label::new("Public").size(LabelSize::Small))
                        .tooltip(Tooltip::text(
                            "This commit is already on the trunk branch and can't be modified here.",
                        )),
                )
            })
            .when_some(commit, |this, commit| {
                let timestamp = OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
                    .map(|timestamp| {
                        time_format::format_local_timestamp(
                            timestamp,
                            OffsetDateTime::now_utc(),
                            TimestampFormat::Relative,
                        )
                    })
                    .unwrap_or_default();
                let full_date = format_timestamp(commit.commit_timestamp);
                this.child(
                    Label::new(format!("Created by {}", commit.author_name))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
                .child(
                    div()
                        .id("smartlog-commit-date")
                        .child(
                            Label::new(timestamp)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text(full_date)),
                )
            })
            .child(
                Button::new("smartlog-copy-hash", sha.display_short())
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .label_size(LabelSize::Small)
                    .end_icon(Icon::new(IconName::Copy).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Copy commit hash"))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    }),
            )
            .into_any_element()
    }

    fn render_changes_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let commit_mode = self.sidebar_in_commit_mode();
        let total = self.uncommitted_files.len();
        let selected = self.selected_paths().len();
        let count = if selected == total {
            total.to_string()
        } else {
            format!("{selected}/{total}")
        };

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(self.section_title(if commit_mode {
                        "CHANGES TO COMMIT"
                    } else {
                        "CHANGES TO AMEND"
                    }))
                    .child(self.badge(count, cx)),
            )
            .map(|section| {
                if total == 0 {
                    section.child(
                        Label::new(if commit_mode {
                            "No changes to commit"
                        } else {
                            "No changes to amend"
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                } else {
                    section.child(self.render_toolbar(cx)).children(
                        (0..total).map(|file_index| self.render_file(file_index, "sidebar", cx)),
                    )
                }
            })
            .into_any_element()
    }

    fn render_files_changed_section(&self, sha: Oid, cx: &mut Context<Self>) -> AnyElement {
        let is_public = self.sidebar_target_is_public();
        let file_count = self.sidebar.diff.as_ref().map(|diff| diff.files.len());
        let short_hash = sha.display_short();

        let files: Vec<AnyElement> = match (&self.sidebar.diff, self.repository(cx)) {
            (Some(diff), Some(repository)) => {
                let mut files: Vec<_> = diff.files.iter().collect();
                files.sort_by_key(|file| file.status());
                files
                    .into_iter()
                    .enumerate()
                    .map(|(index, file)| {
                        let entry = ChangedFileEntry::from_commit_file(file, cx);
                        let directory =
                            (!entry.dir_path.is_empty()).then(|| entry.dir_path.clone());
                        entry.render(
                            index,
                            0,
                            directory,
                            sha.to_string().into(),
                            repository.downgrade(),
                            self.workspace.clone(),
                            cx,
                        )
                    })
                    .collect()
            }
            _ => Vec::new(),
        };

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(self.section_title("FILES CHANGED"))
                    .when_some(file_count, |this, count| {
                        this.child(self.badge(count.to_string(), cx))
                    })
                    .when_some(self.sidebar.diff_stats, |this, (added, removed)| {
                        this.child(
                            Label::new(format!("+{added}"))
                                .size(LabelSize::Small)
                                .color(Color::Created),
                        )
                        .child(
                            Label::new(format!("−{removed}"))
                                .size(LabelSize::Small)
                                .color(Color::Deleted),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new("smartlog-view-commit-changes", format!("View Changes in {short_hash}"))
                            .start_icon(Icon::new(IconName::Diff).size(IconSize::Small))
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_commit(sha, window, cx);
                            })),
                    )
                    .child(
                        Button::new("smartlog-open-all-files", "Open All Files")
                            .start_icon(Icon::new(IconName::File).size(IconSize::Small))
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .disabled(self.sidebar.diff.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_all_changed_files(window, cx);
                            })),
                    ),
            )
            .map(|section| {
                if self.sidebar.diff.is_none() && is_public && !self.sidebar.load_public_files {
                    section.child(
                        Button::new("smartlog-load-files", "Load changed files")
                            .style(ButtonStyle::Filled)
                            .size(ButtonSize::Compact)
                            .tooltip(Tooltip::text(
                                "Changed files aren't loaded for trunk commits because that can be slow.",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar.load_public_files = true;
                                this.load_sidebar_diff(cx);
                                cx.notify();
                            })),
                    )
                } else if self.sidebar.diff.is_none() {
                    section.child(
                        Label::new("Loading…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                } else {
                    section.child(v_flex().children(files))
                }
            })
            .into_any_element()
    }

    fn render_sidebar_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let is_head = self.sidebar_target_is_head();
        let is_public = self.sidebar_target_is_public();
        if is_public {
            return None;
        }

        let commit_mode = self.sidebar_in_commit_mode();
        let editing = self.sidebar_message_is_edited(cx);
        let nothing_selected = self.selected_paths().is_empty();
        let anything_to_commit =
            !nothing_selected && ((!commit_mode && editing) || !self.uncommitted_files.is_empty());
        let primary = primary_action(is_head, commit_mode, anything_to_commit, editing);
        let can_submit = self.submit_branch(cx).is_some();
        let show_submit = if is_head {
            anything_to_commit || !editing
        } else {
            !editing
        };
        let submit_commit_first = match primary {
            PrimaryAction::Commit => Some(false),
            PrimaryAction::Amend if anything_to_commit => Some(true),
            _ => None,
        };

        let primary_button = match primary {
            PrimaryAction::Commit => Button::new("smartlog-sidebar-commit", "Commit")
                .disabled(!anything_to_commit)
                .tooltip(Tooltip::text(if anything_to_commit {
                    "Commit the selected changes"
                } else if nothing_selected {
                    "No selected changes to commit"
                } else {
                    "No changes to commit"
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    let message = this.sidebar_commit_message(cx);
                    if let Some(task) = this.commit_changes(false, message, window, cx) {
                        this.run_logged("Failed to commit", task, window, cx);
                    }
                })),
            PrimaryAction::Amend => Button::new("smartlog-sidebar-amend", "Amend")
                .disabled(!anything_to_commit)
                .tooltip(Tooltip::text(if anything_to_commit {
                    "Add the selected changes to the current commit"
                } else if nothing_selected {
                    "No selected changes to amend"
                } else {
                    "No changes to amend"
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    let message = if this.sidebar_message_is_edited(cx) {
                        this.sidebar_commit_message(cx)
                    } else {
                        None
                    };
                    if let Some(task) = this.commit_changes(true, message, window, cx) {
                        this.run_logged("Failed to amend commit", task, window, cx);
                    }
                })),
            PrimaryAction::AmendMessage => {
                Button::new("smartlog-sidebar-amend-message", "Amend Message")
                    .disabled(!editing)
                    .tooltip(Tooltip::text(if editing {
                        "Amend the commit message with the newly entered message."
                    } else {
                        "No message edits to amend"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.amend_message(window, cx);
                    }))
            }
        }
        .style(ButtonStyle::Filled);

        Some(
            h_flex()
                .flex_none()
                .w_full()
                .p_2()
                .gap_2()
                .justify_end()
                .flex_wrap()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .when(editing && !commit_mode, |this| {
                    this.child(
                        Button::new("smartlog-sidebar-cancel", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_sidebar_edits(window, cx);
                            })),
                    )
                })
                .child(primary_button)
                .when(show_submit, |this| {
                    this.child(
                        Button::new(
                            "smartlog-sidebar-submit",
                            submit_label(commit_mode, anything_to_commit, primary),
                        )
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .disabled(!can_submit)
                        .tooltip(Tooltip::text(if can_submit {
                            "Push this branch to the default remote"
                        } else {
                            "No local branch to push for this commit"
                        }))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.submit(submit_commit_first, window, cx);
                            },
                        )),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_multi_selection(&self) -> AnyElement {
        let selected = self.selected_in_display_order();
        v_flex()
            .p_3()
            .gap_3()
            .child(
                h_flex().justify_center().child(
                    Label::new(format!("{} Commits Selected", selected.len()))
                        .weight(gpui::FontWeight::BOLD),
                ),
            )
            .child(Divider::horizontal())
            .child(v_flex().gap_2().children(selected.into_iter().map(|sha| {
                let subject = self.commits.get(&sha).map_or_else(
                    || SharedString::from("Loading…"),
                    |commit| commit.subject.clone(),
                );
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(sha.display_short())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(subject).truncate())
            })))
            .into_any_element()
    }

    fn render_multi_selection_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .flex_none()
            .w_full()
            .p_2()
            .gap_2()
            .justify_end()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child({
                let can_fold = fold_chain(&self.parents, &self.selection).is_some();
                Button::new("smartlog-fold", "Fold")
                    .start_icon(Icon::new(IconName::FoldVertical).size(IconSize::Small))
                    .style(ButtonStyle::Filled)
                    .disabled(!can_fold)
                    .tooltip(Tooltip::text(if can_fold {
                        "Combine the selected commits into one commit"
                    } else {
                        "Select an unbroken chain of commits, with nothing else built on its middle, to fold them"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.fold_selection(window, cx);
                    }))
            })
            .child(
                Button::new("smartlog-clear-selection", "Deselect All")
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.selection.clear();
                        this.selection_anchor = None;
                        this.sync_sidebar(window, cx);
                    })),
            )
            .into_any_element()
    }

    pub(super) fn render_sidebar(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let right_fraction = self.sidebar.split.read(cx).right_ratio();
        let target = self.sidebar.commit;
        let is_head = self.sidebar_target_is_head();
        let is_public = self.sidebar_target_is_public();
        let commit_mode = self.sidebar_in_commit_mode();
        let read_only = is_public && !commit_mode;

        let header = h_flex()
            .flex_none()
            .w_full()
            .px_3()
            .py_2()
            .justify_between()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                Label::new("Commit Info")
                    .weight(gpui::FontWeight::BOLD)
                    .size(LabelSize::Small),
            )
            .child(
                IconButton::new("smartlog-collapse-sidebar", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Hide Commit Info"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar.collapsed = true;
                        cx.notify();
                    })),
            );

        let multiple_selected = self.selection.len() > 1;
        let content = match target {
            None if multiple_selected => v_flex().child(self.render_multi_selection()),
            None => v_flex().p_3().child(
                Label::new("Loading…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            Some(sha) => v_flex()
                .p_3()
                .gap_3()
                .when(is_head, |this| {
                    this.child(
                        h_flex()
                            .gap_1()
                            .justify_end()
                            .child(
                                Button::new("smartlog-mode-commit", "Commit")
                                    .style(ButtonStyle::Subtle)
                                    .size(ButtonSize::Compact)
                                    .toggle_state(commit_mode)
                                    .tooltip(Tooltip::text(
                                        "In Commit mode, you can edit the blank commit message for a new commit.",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_commit_mode(true, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("smartlog-mode-amend", "Amend")
                                    .style(ButtonStyle::Subtle)
                                    .size(ButtonSize::Compact)
                                    .toggle_state(!commit_mode)
                                    .tooltip(Tooltip::text(
                                        "In Amend mode, you can view and edit the commit message for the current head commit.",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_commit_mode(false, window, cx);
                                    })),
                            ),
                    )
                })
                .when(!commit_mode, |this| this.child(self.render_byline(sha, cx)))
                .child(
                    v_flex()
                        .gap_1()
                        .child(self.section_title("TITLE"))
                        .child(self.message_field(&self.sidebar.title_editor, read_only, cx)),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(self.section_title("DESCRIPTION"))
                        .child(self.message_field(
                            &self.sidebar.description_editor,
                            read_only,
                            cx,
                        )),
                )
                .child(Divider::horizontal())
                .when(is_head && !is_public, |this| {
                    this.child(self.render_changes_section(cx))
                })
                .when(!commit_mode, |this| {
                    this.child(self.render_files_changed_section(sha, cx))
                }),
        };

        v_flex()
            .id("smartlog-sidebar")
            .h_full()
            .min_w(MIN_SIDEBAR_WIDTH)
            .flex_basis(DefiniteLength::Fraction(right_fraction))
            .bg(colors.editor_background)
            .child(header)
            .child(
                div()
                    .id("smartlog-sidebar-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content),
            )
            .map(|sidebar| {
                if multiple_selected {
                    sidebar.child(self.render_multi_selection_actions(cx))
                } else {
                    sidebar.children(self.render_sidebar_actions(cx))
                }
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trips_title_and_description() {
        let message = Message::parse("Fix the thing\n\nIt was broken.\nSecond line.\n");
        assert_eq!(message.title, "Fix the thing");
        assert_eq!(message.description, "It was broken.\nSecond line.");
        assert_eq!(
            message.serialize(),
            "Fix the thing\n\nIt was broken.\nSecond line."
        );
    }

    #[test]
    fn message_without_a_description_serializes_to_just_the_title() {
        let message = Message::parse("Just a title");
        assert_eq!(message.description, "");
        assert_eq!(message.serialize(), "Just a title");
    }

    #[test]
    fn whitespace_only_changes_are_not_edits() {
        let original = Message::parse("Title\n\nBody");
        let edited = Message {
            title: "Title ".to_string(),
            description: "\nBody\n".to_string(),
        };
        assert!(edited.is_equivalent_to(&original));
    }

    #[test]
    fn head_offers_commit_or_amend_unless_only_the_message_changed() {
        assert_eq!(
            primary_action(true, false, false, false),
            PrimaryAction::Amend
        );
        assert_eq!(
            primary_action(true, false, true, true),
            PrimaryAction::Amend
        );
        assert_eq!(
            primary_action(true, false, false, true),
            PrimaryAction::AmendMessage
        );
        assert_eq!(
            primary_action(true, true, false, true),
            PrimaryAction::Commit
        );
    }

    #[test]
    fn other_commits_can_only_amend_their_message() {
        assert_eq!(
            primary_action(false, false, false, false),
            PrimaryAction::AmendMessage
        );
        assert_eq!(
            primary_action(false, false, true, true),
            PrimaryAction::AmendMessage
        );
    }

    #[test]
    fn submit_label_follows_what_will_be_committed() {
        assert_eq!(
            submit_label(true, true, PrimaryAction::Commit),
            "Commit and Submit"
        );
        assert_eq!(
            submit_label(false, true, PrimaryAction::Amend),
            "Amend and Submit"
        );
        assert_eq!(submit_label(false, false, PrimaryAction::Amend), "Submit");
        assert_eq!(
            submit_label(false, false, PrimaryAction::AmendMessage),
            "Submit"
        );
    }
}
