use anyhow::Context as _;
use collections::HashMap;
use git::{
    Oid,
    repository::{InitialGraphCommitData, LogOrder, LogSource},
};
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    Pixels, SharedString, Task, WeakEntity, Window, actions, div, prelude::*, px,
};
use project::git_store::{
    CommitDataState, GitStore, GitStoreEvent, Repository, RepositoryEvent, RepositoryId,
};
use std::sync::Arc;
use time::OffsetDateTime;
use time_format::TimestampFormat;
use ui::{Chip, IconButtonShape, Tooltip, prelude::*};
use util::ResultExt;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
    notifications::DetachAndPromptErr as _,
};

use crate::commit_view::CommitView;

const ROW_HEIGHT: Pixels = px(48.0);
const ELBOW_RADIUS: Pixels = px(8.0);
const DASH_LENGTH: Pixels = px(4.0);
const DASH_GAP: Pixels = px(3.0);
const LIST_VERTICAL_PADDING: Pixels = px(24.0);
const LANE_WIDTH: Pixels = px(24.0);
const LEFT_PADDING: Pixels = px(20.0);
const LINE_WIDTH: Pixels = px(2.0);
const NODE_DIAMETER: Pixels = px(10.0);

actions!(
    smartlog,
    [
        /// Opens the Smartlog, showing only draft commits that are not on the trunk branch.
        Open,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            open(workspace, window, cx);
        });
    })
    .detach();
}

fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
        return;
    };
    let repository_id = repository.read(cx).id;
    let trunk_receiver = repository.update(cx, |repository, _| repository.default_branch(true));
    let workspace_handle = workspace.weak_handle();

    cx.spawn_in(window, async move |workspace, cx| {
        let trunk = trunk_receiver
            .await
            .context("default branch request was canceled")??
            .context("could not determine the trunk branch for this repository")?;
        workspace.update_in(cx, |workspace, window, cx| {
            let existing = workspace
                .items_of_type::<Smartlog>(cx)
                .find(|smartlog| smartlog.read(cx).repository_id == repository_id);
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let git_store = workspace.project().read(cx).git_store().clone();
            let smartlog = cx.new(|cx| {
                Smartlog::new(
                    repository_id,
                    git_store,
                    workspace_handle,
                    trunk,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(smartlog), None, true, window, cx);
        })
    })
    .detach_and_prompt_err("Failed to open Smartlog", window, cx, |_, _, _| None);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    /// A commit on the trunk that draft commits are based on.
    Public,
    Draft,
    /// Stands in for the uncommitted changes in the working tree, directly above `HEAD`.
    Uncommitted,
    /// Closes the trunk line below the oldest trunk commit, signalling that history continues.
    Terminator,
    /// Blank row in front of a commit where stacks branching off it curve back into its column.
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LayoutRow {
    sha: Option<Oid>,
    kind: RowKind,
    column: usize,
    is_head: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LayoutEdge {
    child_row: usize,
    parent_row: usize,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Layout {
    rows: Vec<LayoutRow>,
    edges: Vec<LayoutEdge>,
    /// Edges between consecutive trunk commits, drawn as a muted line since the
    /// commits between them are intentionally not shown.
    trunk_edges: Vec<LayoutEdge>,
    column_count: usize,
}

/// Arranges draft commits the way Sapling's smartlog does: the trunk is a line
/// in column zero, each draft stack branches off the trunk commit it is based
/// on, and a trunk commit without draft children is hidden unless it is `HEAD`.
///
/// `drafts` must be ordered newest first, which is the order `git log` yields.
fn build_layout(
    drafts: &[(Oid, Option<Oid>)],
    head: Option<Oid>,
    has_uncommitted_changes: bool,
) -> Layout {
    let draft_shas: collections::HashSet<Oid> = drafts.iter().map(|(sha, _)| *sha).collect();

    let mut children_by_parent: HashMap<Oid, Vec<Oid>> = HashMap::default();
    let mut roots: Vec<Oid> = Vec::new();
    let mut parentless_drafts: Vec<Oid> = Vec::new();
    for (sha, parent) in drafts {
        match parent {
            Some(parent) => {
                children_by_parent.entry(*parent).or_default().push(*sha);
                if !draft_shas.contains(parent) && !roots.contains(parent) {
                    roots.push(*parent);
                }
            }
            None => parentless_drafts.push(*sha),
        }
    }
    if let Some(head) = head
        && !draft_shas.contains(&head)
        && !roots.contains(&head)
    {
        roots.insert(0, head);
    }

    let mut builder = LayoutBuilder {
        children_by_parent,
        head,
        has_uncommitted_changes,
        layout: Layout::default(),
    };

    let mut trunk_rows = Vec::new();
    for root in roots {
        trunk_rows.push(builder.place(root, RowKind::Public, 0, 1));
    }
    for sha in parentless_drafts {
        builder.place(sha, RowKind::Draft, 1, 1);
    }

    for pair in trunk_rows.windows(2) {
        builder.layout.trunk_edges.push(LayoutEdge {
            child_row: pair[0],
            parent_row: pair[1],
        });
    }

    if let Some(&last_trunk_row) = trunk_rows.last() {
        builder.layout.rows.push(LayoutRow {
            sha: None,
            kind: RowKind::Terminator,
            column: 0,
            is_head: false,
        });
        builder.layout.trunk_edges.push(LayoutEdge {
            child_row: last_trunk_row,
            parent_row: builder.layout.rows.len() - 1,
        });
    }

    let mut layout = builder.layout;
    layout.column_count = layout
        .rows
        .iter()
        .map(|row| row.column + 1)
        .max()
        .unwrap_or(1);
    layout
}

struct LayoutBuilder {
    children_by_parent: HashMap<Oid, Vec<Oid>>,
    head: Option<Oid>,
    has_uncommitted_changes: bool,
    layout: Layout,
}

impl LayoutBuilder {
    /// Emits the rows for `sha` and its descendants, children first so that
    /// newer commits appear above their parents. Returns the row index of `sha`.
    fn place(
        &mut self,
        sha: Oid,
        kind: RowKind,
        column: usize,
        first_child_column: usize,
    ) -> usize {
        let children = self.children_by_parent.remove(&sha).unwrap_or_default();

        let mut child_rows = Vec::with_capacity(children.len());
        let mut next_free_column = first_child_column;
        for (index, child) in children.into_iter().enumerate() {
            let child_column = if index == 0 {
                first_child_column
            } else {
                next_free_column
            };
            let (child_row, rightmost) = self.place_draft(child, child_column);
            child_rows.push(child_row);
            next_free_column = next_free_column.max(rightmost + 1);
        }

        let link_row = child_rows
            .iter()
            .any(|&child_row| self.layout.rows[child_row].column != column)
            .then(|| {
                self.layout.rows.push(LayoutRow {
                    sha: None,
                    kind: RowKind::Link,
                    column,
                    is_head: false,
                });
                self.layout.rows.len() - 1
            });

        let is_head = self.head == Some(sha);
        let uncommitted_row = (is_head && self.has_uncommitted_changes).then(|| {
            self.layout.rows.push(LayoutRow {
                sha: None,
                kind: RowKind::Uncommitted,
                column,
                is_head: false,
            });
            self.layout.rows.len() - 1
        });

        self.layout.rows.push(LayoutRow {
            sha: Some(sha),
            kind,
            column,
            is_head,
        });
        let row = self.layout.rows.len() - 1;

        for child_row in child_rows {
            let parent_row = match link_row {
                Some(link_row) if self.layout.rows[child_row].column != column => link_row,
                _ => row,
            };
            self.layout.edges.push(LayoutEdge {
                child_row,
                parent_row,
            });
        }
        for child_row in link_row.into_iter().chain(uncommitted_row) {
            self.layout.edges.push(LayoutEdge {
                child_row,
                parent_row: row,
            });
        }
        row
    }

    /// Places a draft subtree and returns its root row together with the
    /// rightmost column it occupies.
    fn place_draft(&mut self, sha: Oid, column: usize) -> (usize, usize) {
        let first_row = self.layout.rows.len();
        let row = self.place(sha, RowKind::Draft, column, column);
        let rightmost = self.layout.rows[first_row..]
            .iter()
            .map(|row| row.column)
            .max()
            .unwrap_or(column);
        (row, rightmost)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Segment {
    /// Full-height vertical line.
    Vertical { column: usize, color: usize },
    /// Vertical line from the row's center downward.
    Down { column: usize, color: usize },
    /// Vertical line from the row's top to its center.
    Up { column: usize, color: usize },
    /// Comes down the `to` column from the row's top edge and curves left to meet the `from`
    /// column at the row's center.
    Elbow {
        from: usize,
        to: usize,
        color: usize,
    },
}

/// Splits every edge into the pieces each row has to draw.
fn row_segments(layout: &Layout) -> Vec<Vec<Segment>> {
    let mut segments: Vec<Vec<Segment>> = vec![Vec::new(); layout.rows.len()];

    let mut add_edge = |edge: &LayoutEdge, color: usize| {
        let (Some(child), Some(parent)) = (
            layout.rows.get(edge.child_row),
            layout.rows.get(edge.parent_row),
        ) else {
            return;
        };
        if let Some(row_segments) = segments.get_mut(edge.child_row) {
            row_segments.push(Segment::Down {
                column: child.column,
                color,
            });
        }
        for row in edge.child_row + 1..edge.parent_row {
            if let Some(row_segments) = segments.get_mut(row) {
                row_segments.push(Segment::Vertical {
                    column: child.column,
                    color,
                });
            }
        }
        if let Some(row_segments) = segments.get_mut(edge.parent_row) {
            if child.column == parent.column {
                row_segments.push(Segment::Up {
                    column: child.column,
                    color,
                });
            } else {
                row_segments.push(Segment::Elbow {
                    from: parent.column,
                    to: child.column,
                    color,
                });
            }
        }
    };

    for edge in &layout.edges {
        let color = layout.rows.get(edge.child_row).map_or(0, |row| row.column);
        add_edge(edge, color);
    }
    for edge in &layout.trunk_edges {
        add_edge(edge, 0);
    }
    segments
}

pub struct Smartlog {
    focus_handle: FocusHandle,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    repository_id: RepositoryId,
    trunk: SharedString,
    layout: Layout,
    segments: Vec<Vec<Segment>>,
    commits: HashMap<Oid, Arc<git::repository::CommitData>>,
    ref_names: HashMap<Oid, Vec<SharedString>>,
    selected_row: Option<usize>,
    error: Option<SharedString>,
    pending_commit_loads: Vec<Task<()>>,
}

impl EventEmitter<ItemEvent> for Smartlog {}

impl Focusable for Smartlog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Smartlog {
    fn new(
        repository_id: RepositoryId,
        git_store: Entity<GitStore>,
        workspace: WeakEntity<Workspace>,
        trunk: SharedString,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&git_store, |this, _, event, cx| {
            let GitStoreEvent::RepositoryUpdated(updated_id, event, _) = event else {
                return;
            };
            if *updated_id != this.repository_id {
                return;
            }
            match event {
                RepositoryEvent::GraphEvent((LogSource::Draft(_), _), _)
                | RepositoryEvent::HeadChanged
                | RepositoryEvent::BranchListChanged
                | RepositoryEvent::StatusesChanged => this.refresh(cx),
                _ => {}
            }
        })
        .detach();

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            git_store,
            workspace,
            repository_id,
            trunk,
            layout: Layout::default(),
            segments: Vec::new(),
            commits: HashMap::default(),
            ref_names: HashMap::default(),
            selected_row: None,
            error: None,
            pending_commit_loads: Vec::new(),
        };
        this.refresh(cx);
        this
    }

    fn repository(&self, cx: &App) -> Option<Entity<Repository>> {
        self.git_store
            .read(cx)
            .repositories()
            .get(&self.repository_id)
            .cloned()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let source = LogSource::Draft(self.trunk.clone());

        let (draft_commits, error): (Vec<Arc<InitialGraphCommitData>>, _) =
            repository.update(cx, |repository, cx| {
                let response =
                    repository.graph_data(source, LogOrder::DateOrder, 0..usize::MAX, cx);
                (response.commits.to_vec(), response.error)
            });
        self.error = error;

        let head = repository
            .read(cx)
            .head_commit
            .as_ref()
            .and_then(|commit| commit.sha.parse::<Oid>().ok());
        let has_uncommitted_changes = repository.read(cx).status_summary().count > 0;

        let drafts: Vec<(Oid, Option<Oid>)> = draft_commits
            .iter()
            .map(|commit| (commit.sha, commit.parents.first().copied()))
            .collect();
        for commit in &draft_commits {
            self.ref_names.insert(commit.sha, commit.ref_names.clone());
        }

        let selected_sha = self
            .selected_row
            .and_then(|row| self.layout.rows.get(row))
            .and_then(|row| row.sha);
        self.layout = build_layout(&drafts, head, has_uncommitted_changes);
        self.segments = row_segments(&self.layout);
        self.selected_row = selected_sha
            .and_then(|sha| self.layout.rows.iter().position(|row| row.sha == Some(sha)));

        let shas: Vec<Oid> = self.layout.rows.iter().filter_map(|row| row.sha).collect();
        for sha in shas {
            self.load_commit(&repository, sha, cx);
        }
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn load_commit(&mut self, repository: &Entity<Repository>, sha: Oid, cx: &mut Context<Self>) {
        if self.commits.contains_key(&sha) {
            return;
        }
        let state = repository.update(cx, |repository, cx| {
            repository.fetch_commit_data(sha, true, cx).clone()
        });
        match state {
            CommitDataState::Loaded(data) => {
                self.commits.insert(sha, data);
            }
            CommitDataState::Loading(Some(receiver)) => {
                let task = cx.spawn(async move |this, cx| {
                    if let Ok(data) = receiver.await {
                        this.update(cx, |this, cx| {
                            this.commits.insert(sha, data);
                            cx.notify();
                        })
                        .log_err();
                    }
                });
                self.pending_commit_loads.push(task);
            }
            CommitDataState::Loading(None) => {}
        }
    }

    fn goto_commit(&mut self, sha: Oid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        cx.spawn_in(window, async move |_, cx| {
            repository
                .update(cx, |repository, _| {
                    repository.change_branch(sha.to_string())
                })
                .await??;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to go to commit", window, cx, |_, _, _| None);
    }

    fn open_commit(&self, sha: Oid, window: &mut Window, cx: &mut App) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        CommitView::open(
            sha.to_string(),
            repository.downgrade(),
            self.workspace.clone(),
            None,
            None,
            window,
            cx,
        );
    }

    fn lane_color(&self, lane: usize, cx: &App) -> Hsla {
        if lane == 0 {
            return cx.theme().colors().text_muted;
        }
        cx.theme().accents().color_for_index(lane as u32 - 1)
    }

    fn lane_x(column: usize) -> Pixels {
        LEFT_PADDING + LANE_WIDTH * column as f32 + LANE_WIDTH / 2.0
    }

    fn dashed_vertical(
        &self,
        column: usize,
        top: Pixels,
        height: Pixels,
        color: usize,
        cx: &App,
    ) -> AnyElement {
        let dash_color = self.lane_color(color, cx);
        let dash_count = (height / (DASH_LENGTH + DASH_GAP)).ceil() as usize;
        v_flex()
            .absolute()
            .left(Self::lane_x(column) - LINE_WIDTH / 2.0)
            .top(top)
            .h(height)
            .overflow_hidden()
            .gap(DASH_GAP)
            .children((0..dash_count).map(|_| {
                div()
                    .flex_none()
                    .w(LINE_WIDTH)
                    .h(DASH_LENGTH)
                    .bg(dash_color)
            }))
            .into_any_element()
    }

    fn render_gutter(&self, row_index: usize, row: &LayoutRow, cx: &App) -> AnyElement {
        let width = LEFT_PADDING * 2.0 + LANE_WIDTH * self.layout.column_count as f32;
        let half_row = ROW_HEIGHT / 2.0;
        let mut gutter = div().relative().flex_none().w(width).h(ROW_HEIGHT);

        for segment in self.segments.get(row_index).into_iter().flatten() {
            let line = match *segment {
                Segment::Vertical { column, color } => div()
                    .absolute()
                    .left(Self::lane_x(column) - LINE_WIDTH / 2.0)
                    .top_0()
                    .w(LINE_WIDTH)
                    .h(ROW_HEIGHT)
                    .bg(self.lane_color(color, cx)),
                Segment::Down { column, color } => div()
                    .absolute()
                    .left(Self::lane_x(column) - LINE_WIDTH / 2.0)
                    .top(half_row)
                    .w(LINE_WIDTH)
                    .h(half_row)
                    .bg(self.lane_color(color, cx)),
                Segment::Up { column, color } if row.kind == RowKind::Terminator => {
                    gutter = gutter.child(self.dashed_vertical(
                        column,
                        Pixels::ZERO,
                        half_row + ELBOW_RADIUS,
                        color,
                        cx,
                    ));
                    continue;
                }
                Segment::Up { column, color } => div()
                    .absolute()
                    .left(Self::lane_x(column) - LINE_WIDTH / 2.0)
                    .top_0()
                    .w(LINE_WIDTH)
                    .h(half_row)
                    .bg(self.lane_color(color, cx)),
                Segment::Elbow { from, to, color } => div()
                    .absolute()
                    .left(Self::lane_x(from))
                    .top_0()
                    .w(Self::lane_x(to) - Self::lane_x(from) + LINE_WIDTH / 2.0)
                    .h(half_row + LINE_WIDTH / 2.0)
                    .border_r(LINE_WIDTH)
                    .border_b(LINE_WIDTH)
                    .border_color(self.lane_color(color, cx))
                    .rounded_br(ELBOW_RADIUS),
            };
            gutter = gutter.child(line);
        }

        let colors = cx.theme().colors();
        let accent = self.lane_color(row.column, cx);
        if matches!(row.kind, RowKind::Terminator | RowKind::Link) {
            return gutter.into_any_element();
        }

        let node = div()
            .absolute()
            .left(Self::lane_x(row.column) - NODE_DIAMETER / 2.0)
            .top(half_row - NODE_DIAMETER / 2.0)
            .size(NODE_DIAMETER)
            .rounded_full()
            .border_2()
            .map(|node| match row.kind {
                RowKind::Draft => node.border_color(accent).bg(accent),
                RowKind::Public => node.border_color(accent).bg(colors.background),
                RowKind::Uncommitted | RowKind::Terminator | RowKind::Link => node
                    .border_color(colors.text_muted)
                    .border_dashed()
                    .bg(colors.background),
            })
            .when(row.is_head, |node| {
                node.border_color(cx.theme().status().info)
            });
        gutter.child(node).into_any_element()
    }

    fn render_row(&self, index: usize, row: &LayoutRow, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let is_selected = self.selected_row == Some(index);
        let sha = row.sha;

        let summary = match (row.kind, sha) {
            (RowKind::Terminator | RowKind::Link, _) => h_flex(),
            (RowKind::Uncommitted, _) | (_, None) => {
                let count = self
                    .repository(cx)
                    .map_or(0, |repository| repository.read(cx).status_summary().count);
                h_flex()
                    .child(Label::new(format!("Uncommitted changes ({count})")).color(Color::Muted))
            }
            (_, Some(sha)) => {
                let commit = self.commits.get(&sha);
                let subject = commit.map_or_else(
                    || SharedString::from("Loading…"),
                    |commit| commit.subject.clone(),
                );
                let details = commit.map(|commit| {
                    OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
                        .map(|timestamp| {
                            time_format::format_local_timestamp(
                                timestamp,
                                OffsetDateTime::now_utc(),
                                TimestampFormat::Relative,
                            )
                        })
                        .unwrap_or_default()
                });

                h_flex()
                    .gap_3()
                    .min_w_0()
                    .child(
                        Label::new(subject)
                            .truncate()
                            .when(row.kind == RowKind::Public, |label| {
                                label.color(Color::Muted)
                            }),
                    )
                    .when_some(details, |this, details| {
                        this.child(
                            Label::new(details)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .flex_none(),
                        )
                    })
                    .children(
                        self.ref_names
                            .get(&sha)
                            .into_iter()
                            .flatten()
                            .filter(|name| !name.as_ref().starts_with("tag: "))
                            .map(|name| {
                                let name = name
                                    .strip_prefix("HEAD -> ")
                                    .unwrap_or(name.as_ref())
                                    .to_string();
                                Chip::new(name).label_size(LabelSize::Small)
                            }),
                    )
                    .when(row.is_head, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .px_2()
                                .rounded_full()
                                .bg(cx.theme().status().info_border)
                                .child(
                                    Label::new("You are here")
                                        .size(LabelSize::Small)
                                        .color(Color::Default),
                                ),
                        )
                    })
            }
        };

        h_flex()
            .id(("smartlog-row", index))
            .h(ROW_HEIGHT)
            .w_full()
            .group("smartlog-row")
            .when(is_selected, |this| this.bg(colors.element_selected))
            .hover(|this| this.bg(colors.element_hover))
            .child(self.render_gutter(index, row, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .child(summary),
            )
            .when(row.kind == RowKind::Uncommitted, |this| {
                this.child(
                    h_flex().pr_2().child(
                        Button::new("smartlog-commit", "Commit")
                            .style(ButtonStyle::Filled)
                            .tooltip(Tooltip::text(
                                "Stage all changes and write a commit message",
                            ))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(git::StageAll), cx);
                                window.dispatch_action(Box::new(git::ExpandCommitEditor), cx);
                            }),
                    ),
                )
            })
            .when_some(sha, |this, sha| {
                this.child(
                    h_flex()
                        .pr_2()
                        .gap_1()
                        .invisible()
                        .group_hover("smartlog-row", |style| style.visible())
                        .when(row.is_head && row.kind == RowKind::Draft, |this| {
                            this.child(
                                Button::new(("smartlog-uncommit", index), "Uncommit")
                                    .style(ButtonStyle::Filled)
                                    .tooltip(Tooltip::text(
                                        "Undo this commit, keeping its changes in the working tree",
                                    ))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(git::Uncommit), cx);
                                    }),
                            )
                        })
                        .when(!row.is_head, |this| {
                            this.child(
                                IconButton::new(("smartlog-goto", index), IconName::ArrowRight)
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Go to this commit"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.goto_commit(sha, window, cx);
                                    })),
                            )
                        })
                        .child(
                            IconButton::new(("smartlog-copy", index), IconName::Copy)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Copy commit hash"))
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        sha.to_string(),
                                    ));
                                }),
                        ),
                )
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.selected_row = Some(index);
                if let Some(sha) = sha
                    && event.click_count() == 2
                {
                    this.open_commit(sha, window, cx);
                }
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Render for Smartlog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = self
            .layout
            .rows
            .clone()
            .iter()
            .enumerate()
            .map(|(index, row)| self.render_row(index, row, cx))
            .collect();

        let header = h_flex()
            .flex_none()
            .w_full()
            .px_4()
            .py_2()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("smartlog-pull", "Pull")
                    .start_icon(Icon::new(IconName::ArrowDown).size(IconSize::Small))
                    .style(ButtonStyle::Filled)
                    .tooltip(Tooltip::text("Fetch new commits from the remote"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(git::Fetch), cx);
                    }),
            )
            .child(
                IconButton::new("smartlog-refresh", IconName::ArrowCircle)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh(cx);
                    })),
            );

        v_flex()
            .id("smartlog")
            .key_context("Smartlog")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(header)
            .child(
                v_flex()
                    .id("smartlog-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pt(LIST_VERTICAL_PADDING)
                    .pb(LIST_VERTICAL_PADDING)
                    .when_some(self.error.clone(), |this, error| {
                        this.child(Label::new(error).color(Color::Error).m_2())
                    })
                    .when(rows.is_empty() && self.error.is_none(), |this| {
                        this.child(
                            Label::new(format!(
                                "No draft commits. Everything is already on {}.",
                                self.trunk
                            ))
                            .color(Color::Muted)
                            .m_2(),
                        )
                    })
                    .children(rows),
            )
    }
}

impl Item for Smartlog {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitGraph))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Smartlog".into()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        Oid::from_bytes(&[byte; 20]).expect("20 bytes form a valid sha1 oid")
    }

    fn shas(layout: &Layout) -> Vec<(Option<Oid>, usize)> {
        layout
            .rows
            .iter()
            .map(|row| (row.sha, row.column))
            .collect()
    }

    #[test]
    fn linear_stack_sits_next_to_its_trunk_commit() {
        let (trunk, a, b) = (oid(1), oid(2), oid(3));
        let layout = build_layout(&[(b, Some(a)), (a, Some(trunk))], Some(b), false);

        assert_eq!(
            shas(&layout),
            vec![
                (Some(b), 1),
                (Some(a), 1),
                (None, 0),
                (Some(trunk), 0),
                (None, 0)
            ]
        );
        assert!(layout.rows[0].is_head);
        assert_eq!(layout.rows[3].kind, RowKind::Public);
        assert_eq!(layout.edges.len(), 3);
    }

    #[test]
    fn forks_get_their_own_column() {
        let (trunk, a, b, c) = (oid(1), oid(2), oid(3), oid(4));
        let layout = build_layout(&[(c, Some(a)), (b, Some(a)), (a, Some(trunk))], None, false);

        let columns: HashMap<Oid, usize> = layout
            .rows
            .iter()
            .filter_map(|row| Some((row.sha?, row.column)))
            .collect();
        assert_eq!(columns[&a], 1);
        assert_eq!(columns[&c], 1);
        assert_eq!(columns[&b], 2);
        assert_eq!(layout.column_count, 3);
    }

    #[test]
    fn head_on_trunk_is_shown_with_uncommitted_changes_above_it() {
        let trunk = oid(1);
        let layout = build_layout(&[], Some(trunk), true);

        assert_eq!(
            layout.rows,
            vec![
                LayoutRow {
                    sha: None,
                    kind: RowKind::Uncommitted,
                    column: 0,
                    is_head: false,
                },
                LayoutRow {
                    sha: Some(trunk),
                    kind: RowKind::Public,
                    column: 0,
                    is_head: true,
                },
                LayoutRow {
                    sha: None,
                    kind: RowKind::Terminator,
                    column: 0,
                    is_head: false,
                },
            ]
        );
        assert_eq!(
            layout.edges,
            vec![LayoutEdge {
                child_row: 0,
                parent_row: 1
            }]
        );
    }

    #[test]
    fn separate_trunk_commits_are_joined_by_a_trunk_edge() {
        let (trunk_a, trunk_b, a, b) = (oid(1), oid(2), oid(3), oid(4));
        let layout = build_layout(&[(a, Some(trunk_a)), (b, Some(trunk_b))], None, false);

        assert_eq!(layout.trunk_edges.len(), 2);
        let public_rows = layout
            .rows
            .iter()
            .filter(|row| row.kind == RowKind::Public)
            .count();
        assert_eq!(public_rows, 2);
    }

    #[test]
    fn segments_connect_a_branch_back_to_the_trunk() {
        let (trunk, a) = (oid(1), oid(2));
        let layout = build_layout(&[(a, Some(trunk))], None, false);
        let segments = row_segments(&layout);

        assert!(segments[0].contains(&Segment::Down {
            column: 1,
            color: 1
        }));
        assert_eq!(layout.rows[1].kind, RowKind::Link);
        assert!(segments[1].contains(&Segment::Elbow {
            from: 0,
            to: 1,
            color: 1
        }));
        assert!(segments[2].contains(&Segment::Up {
            column: 0,
            color: 0
        }));
    }
}
