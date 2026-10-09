use anyhow::Context as _;
use collections::{HashMap, HashSet};
use editor::{Editor, EditorEvent};
use git::{
    Oid,
    repository::{
        CommitOptions, FetchOptions, HIDDEN_COMMIT_REF_PREFIX, InitialGraphCommitData, LogOrder,
        LogSource, ResetMode,
    },
};
use gpui::{
    Anchor, AnyElement, App, ClickEvent, ClipboardItem, DefiniteLength, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, Hsla, MouseButton, MouseDownEvent, Pixels, Point,
    PromptLevel, ScrollHandle, SharedString, Subscription, Task, WeakEntity, Window, actions,
    anchored, deferred, div, prelude::*, px,
};
use menu::{Cancel, Confirm};
use project::git_store::{
    CommitDataState, GitStore, GitStoreEvent, Repository, RepositoryEvent, RepositoryId,
    StatusEntry,
};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, SystemTime},
};
use time::OffsetDateTime;
use time_format::TimestampFormat;
use ui::{
    Checkbox, Chip, CommonAnimationExt as _, ContextMenu, ContextMenuEntry, Headline, HeadlineSize,
    IconButtonShape, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt;
use workspace::{
    ModalView, Workspace,
    item::{Item, ItemEvent},
    notifications::DetachAndPromptErr as _,
};

use crate::{
    commit_view::CommitView,
    git_graph::{DraggedSplitHandle, RESIZE_HANDLE_WIDTH, SplitState},
    git_panel::GitStatusEntry,
    solo_diff_view::SoloDiffView,
};
use askpass::AskPassDelegate;
use git::repository::RepoPath;
use git_ui_core::askpass_modal::AskPassModal;
use workspace::{Toast, notifications::NotificationId};

mod absorb;
mod bookmarks;
mod download;
mod edit_stack;
mod persistence;
mod sidebar;
mod split;

use persistence::SmartlogSettings;
use sidebar::SidebarState;

const ROW_HEIGHT: Pixels = px(48.0);
const COMPACT_ROW_HEIGHT: Pixels = px(32.0);
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
        /// Selects the commit above the current one.
        SelectPreviousCommit,
        /// Selects the commit below the current one.
        SelectNextCommit,
        /// Extends the selection to the commit above.
        ExtendSelectionUp,
        /// Extends the selection to the commit below.
        ExtendSelectionDown,
        /// Shows the Commit Info sidebar for the selection.
        OpenDetails,
        /// Clears the selection.
        ClearSelection,
        /// Selects every draft commit.
        SelectAllCommits,
        /// Hides the selected commits and everything built on them.
        HideSelectedCommits,
        /// Shows or hides the Commit Info sidebar.
        ToggleSidebar,
        /// Moves focus to the commit filter.
        FocusFilter,
        /// Fetches new commits from the remote.
        Pull,
        /// Rebases the stack containing the checked-out commit onto the trunk.
        RebaseOntoTrunk,
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<Smartlog>(cx);

    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            open(workspace, window, cx);
        });
    })
    .detach();
}

pub(crate) fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
        return;
    };
    let repository_id = repository.read(cx).id;
    let trunk_receiver = repository.update(cx, |repository, _| repository.default_branch(true));
    let workspace_handle = workspace.weak_handle();

    cx.spawn_in(window, async move |workspace, cx| {
        let default_branch = trunk_receiver
            .await
            .context("default branch request was canceled")??;
        // A repository with no remote, or no commits yet, has no default branch, so the Smartlog
        // falls back to the branch that is checked out.
        let trunk = match default_branch {
            Some(trunk) => trunk,
            None => repository
                .read_with(cx, |repository, _| {
                    repository
                        .branch
                        .as_ref()
                        .map(|branch| SharedString::from(branch.name().to_string()))
                })
                .unwrap_or_else(|| "main".into()),
        };
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
    /// A row of the uncommitted changes section, which sits directly above `HEAD`.
    Uncommitted(UncommittedPart),
    /// Closes the trunk line below the oldest trunk commit, signalling that history continues.
    Terminator,
    /// Blank row in front of a commit where stacks branching off it curve back into its column.
    Link,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UncommittedPart {
    Header,
    Toolbar,
    File(usize),
    /// Amend, Commit and the commit title field.
    Actions,
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
    uncommitted_file_count: usize,
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
        uncommitted_file_count,
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
    uncommitted_file_count: usize,
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
        let uncommitted_row = (is_head && self.uncommitted_file_count > 0).then(|| {
            let first_row = self.layout.rows.len();
            let parts = [UncommittedPart::Header, UncommittedPart::Toolbar]
                .into_iter()
                .chain((0..self.uncommitted_file_count).map(UncommittedPart::File))
                .chain([UncommittedPart::Actions]);
            for part in parts {
                self.layout.rows.push(LayoutRow {
                    sha: None,
                    kind: RowKind::Uncommitted(part),
                    column,
                    is_head: false,
                });
            }
            first_row
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct RebasePlan {
    old_base: Oid,
    branch: Option<String>,
}

/// `root` and every commit built on it.
fn stack_members(parents: &HashMap<Oid, Option<Oid>>, root: Oid) -> HashSet<Oid> {
    let mut members: HashSet<Oid> = HashSet::default();
    members.insert(root);
    loop {
        let before = members.len();
        for (child, parent) in parents {
            if parent.is_some_and(|parent| members.contains(&parent)) {
                members.insert(*child);
            }
        }
        if members.len() == before {
            return members;
        }
    }
}

/// How `root` and everything built on it can be rebased. A subtree containing `HEAD` is rebased
/// in place. Any other subtree needs exactly one tip with a local branch, because `git rebase`
/// moves a single branch at a time.
fn plan_subtree_rebase(
    parents: &HashMap<Oid, Option<Oid>>,
    head: Option<Oid>,
    root: Oid,
    local_branch_at: impl Fn(Oid) -> Option<String>,
) -> Option<RebasePlan> {
    let old_base = parents.get(&root).copied().flatten()?;
    let members = stack_members(parents, root);

    if head.is_some_and(|head| members.contains(&head)) {
        return Some(RebasePlan {
            old_base,
            branch: None,
        });
    }

    let mut tips = members
        .iter()
        .copied()
        .filter(|member| !parents.values().any(|parent| *parent == Some(*member)));
    let tip = tips.next()?;
    if tips.next().is_some() {
        return None;
    }
    Some(RebasePlan {
        old_base,
        branch: Some(local_branch_at(tip)?),
    })
}

/// Decides how the stack rooted at `root`, whose parent is on the trunk, can be rebased onto
/// the trunk.
fn plan_rebase(
    parents: &HashMap<Oid, Option<Oid>>,
    head: Option<Oid>,
    root: Oid,
    local_branch_at: impl Fn(Oid) -> Option<String>,
) -> Option<RebasePlan> {
    let old_base = parents.get(&root).copied().flatten()?;
    if parents.contains_key(&old_base) {
        return None;
    }
    plan_subtree_rebase(parents, head, root, local_branch_at)
}

/// Decides whether dropping `source` and its descendants onto `target` is a real, possible move:
/// not into itself, not where it already is, and with a way to run it.
fn plan_drag_rebase(
    parents: &HashMap<Oid, Option<Oid>>,
    head: Option<Oid>,
    source: Oid,
    target: Oid,
    local_branch_at: impl Fn(Oid) -> Option<String>,
) -> Option<RebasePlan> {
    let plan = plan_subtree_rebase(parents, head, source, local_branch_at)?;
    let moved = stack_members(parents, source);
    (!moved.contains(&target) && target != plan.old_base).then_some(plan)
}

/// Why dropping `source` onto `target` can't be turned into a rebase, for when
/// `plan_drag_rebase` finds none.
fn drag_rebase_refusal(
    parents: &HashMap<Oid, Option<Oid>>,
    source: Oid,
    target: Oid,
) -> &'static str {
    let Some(old_base) = parents.get(&source).copied().flatten() else {
        return "Only draft commits can be moved.";
    };
    if stack_members(parents, source).contains(&target) {
        return "A commit can't be moved onto itself or onto something built on it.";
    }
    if target == old_base {
        return "That commit is already based on the one it was dropped on.";
    }
    let tips = stack_members(parents, source)
        .into_iter()
        .filter(|member| !parents.values().any(|parent| *parent == Some(*member)))
        .count();
    if tips > 1 {
        return "Several branches are built on this commit. Move them one at a time.";
    }
    "This commit can't be rebased there."
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectableRow {
    sha: Oid,
    is_public: bool,
}

/// The commit a cursor moves to when stepping down (`forward`) or up the list. Without a cursor it
/// starts at the checked-out commit, or at the first or last commit.
fn step_cursor(
    rows: &[SelectableRow],
    cursor: Option<Oid>,
    head: Option<Oid>,
    forward: bool,
) -> Option<Oid> {
    let current = cursor
        .or(head)
        .and_then(|sha| rows.iter().position(|row| row.sha == sha));
    let next = match (current, forward) {
        (Some(current), true) => (current + 1).min(rows.len().checked_sub(1)?),
        (Some(current), false) => current.saturating_sub(1),
        (None, true) => 0,
        (None, false) => rows.len().checked_sub(1)?,
    };
    rows.get(next).map(|row| row.sha)
}

/// Applies a click to the commit selection the way ISL does: a plain click selects one commit
/// (or clears the selection when it is already the only one), cmd-click toggles one, and
/// shift-click selects the range from the anchor. Trunk commits are never part of a multi-selection.
fn apply_click(
    rows: &[SelectableRow],
    selection: &HashSet<Oid>,
    anchor: Option<Oid>,
    clicked: Oid,
    shift: bool,
    toggle: bool,
) -> (HashSet<Oid>, Option<Oid>) {
    let Some(clicked_row) = rows.iter().find(|row| row.sha == clicked) else {
        return (selection.clone(), anchor);
    };
    let only_clicked = HashSet::from_iter([clicked]);

    if clicked_row.is_public {
        return (only_clicked, Some(clicked));
    }

    if shift
        && let Some(anchor) = anchor
        && let (Some(from), Some(to)) = (
            rows.iter().position(|row| row.sha == anchor),
            rows.iter().position(|row| row.sha == clicked),
        )
    {
        let range = from.min(to)..=from.max(to);
        let selected = rows[range]
            .iter()
            .filter(|row| !row.is_public)
            .map(|row| row.sha)
            .collect();
        return (selected, Some(anchor));
    }

    if toggle {
        let mut selected: HashSet<Oid> = rows
            .iter()
            .filter(|row| !row.is_public && selection.contains(&row.sha))
            .map(|row| row.sha)
            .collect();
        if !selected.remove(&clicked) {
            selected.insert(clicked);
        }
        return (selected, Some(clicked));
    }

    if *selection == only_clicked {
        return (HashSet::default(), None);
    }
    (only_clicked, Some(clicked))
}

/// Orders `selected` oldest first when it forms one unbroken chain of at least two commits,
/// each the only parent of the next, which is what folding needs.
fn fold_chain(parents: &HashMap<Oid, Option<Oid>>, selected: &HashSet<Oid>) -> Option<Vec<Oid>> {
    if selected.len() < 2 {
        return None;
    }
    let mut roots = selected.iter().copied().filter(|sha| {
        !parents
            .get(sha)
            .copied()
            .flatten()
            .is_some_and(|parent| selected.contains(&parent))
    });
    let oldest = roots.next()?;
    if roots.next().is_some() {
        return None;
    }

    let mut chain = vec![oldest];
    while chain.len() < selected.len() {
        let last = *chain.last()?;
        let mut next = selected
            .iter()
            .copied()
            .filter(|sha| parents.get(sha).copied().flatten() == Some(last));
        let following = next.next()?;
        if next.next().is_some() {
            return None;
        }
        chain.push(following);
    }
    Some(chain)
}

/// `head` together with every commit below it that is one of the listed drafts.
fn lineage_of(parents: &HashMap<Oid, Option<Oid>>, head: Option<Oid>) -> HashSet<Oid> {
    let mut lineage = HashSet::default();
    let mut current = head;
    while let Some(sha) = current {
        if !lineage.insert(sha) {
            break;
        }
        current = parents.get(&sha).copied().flatten();
    }
    lineage
}

/// The commits hidden by `roots` and, as in Sapling, everything built on them, except commits
/// in `protected`, which stay visible so the checked-out commit never disappears.
fn hidden_closure(
    parents: &HashMap<Oid, Option<Oid>>,
    roots: &HashSet<Oid>,
    protected: &HashSet<Oid>,
) -> HashSet<Oid> {
    let mut closure: HashSet<Oid> = roots.difference(protected).copied().collect();
    loop {
        let before = closure.len();
        for (child, parent) in parents {
            if !protected.contains(child) && parent.is_some_and(|parent| closure.contains(&parent))
            {
                closure.insert(*child);
            }
        }
        if closure.len() == before {
            return closure;
        }
    }
}

/// Whether a commit matches the text typed into the filter, which is already lowercase. It
/// looks at the subject, author, hash prefix and branch or tag names.
fn commit_matches_filter(
    filter: &str,
    sha: Oid,
    subject: Option<&str>,
    author: Option<(&str, &str)>,
    ref_names: &[SharedString],
) -> bool {
    if filter.is_empty() {
        return true;
    }
    subject.is_some_and(|subject| subject.to_lowercase().contains(filter))
        || author.is_some_and(|(name, email)| {
            name.to_lowercase().contains(filter) || email.to_lowercase().contains(filter)
        })
        || sha.to_string().starts_with(filter)
        || ref_names
            .iter()
            .any(|name| name.to_lowercase().contains(filter))
}

/// What a shelved change is called in the list: the message git recorded, which for an
/// unnamed stash is `WIP on <branch>: <hash> <subject>`.
fn shelf_entry_label(message: &str) -> &str {
    let message = message.trim();
    if message.is_empty() {
        "Shelved changes"
    } else {
        message
    }
}

/// Reads what the Goto Time field holds: a number of hours ago, or a local date such as
/// `2024-05-01`, `2024-05-01 13:30` or `2024-05-01T13:30`.
fn parse_goto_time(input: &str, now: OffsetDateTime, offset: time::UtcOffset) -> Option<i64> {
    let input = input.trim();
    if let Ok(hours) = input.parse::<f64>() {
        if !hours.is_finite() || hours < 0.0 {
            return None;
        }
        return Some(now.unix_timestamp() - (hours * 3600.0) as i64);
    }

    let local = [
        "[year]-[month]-[day] [hour]:[minute]",
        "[year]-[month]-[day]T[hour]:[minute]",
    ]
    .into_iter()
    .find_map(|description| {
        let description = time::format_description::parse(description).ok()?;
        time::PrimitiveDateTime::parse(input, &description).ok()
    })
    .or_else(|| {
        let date_only = time::format_description::parse("[year]-[month]-[day]").ok()?;
        time::Date::parse(input, &date_only)
            .ok()
            .map(|date| date.with_time(time::Time::MIDNIGHT))
    })?;
    Some(local.assume_offset(offset).unix_timestamp())
}

struct GotoTimeModal {
    editor: Entity<Editor>,
    rebase_onto_it: bool,
    smartlog: WeakEntity<Smartlog>,
}

impl GotoTimeModal {
    fn new(smartlog: WeakEntity<Smartlog>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Hours ago, or a date like 2024-05-01 13:30", window, cx);
            editor
        });
        Self {
            editor,
            rebase_onto_it: false,
            smartlog,
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let input = self.editor.read(cx).text(cx);
        let Some(timestamp) = parse_goto_time(&input, OffsetDateTime::now_utc(), offset) else {
            return;
        };
        let rebase_onto_it = self.rebase_onto_it;
        self.smartlog
            .update(cx, |smartlog, cx| {
                smartlog.goto_time(timestamp, rebase_onto_it, window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for GotoTimeModal {}
impl ModalView for GotoTimeModal {}

impl Focusable for GotoTimeModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for GotoTimeModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("GotoTimeModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::Clock).size(IconSize::XSmall))
                    .child(Headline::new("Go to Time").size(HeadlineSize::XSmall)),
            )
            .child(
                v_flex()
                    .px_3()
                    .pb_3()
                    .gap_2()
                    .w_full()
                    .child(self.editor.clone())
                    .child(
                        Checkbox::new(
                            "smartlog-goto-time-rebase",
                            if self.rebase_onto_it {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .label("Rebase current work onto it")
                        .on_click(cx.listener(
                            |this, state: &ToggleState, _, cx| {
                                this.rebase_onto_it = *state == ToggleState::Selected;
                                cx.notify();
                            },
                        )),
                    ),
            )
    }
}

/// How long ago something happened, as short as ISL's Pull badge: `now`, `5m`, `2h` or `3d`.
fn short_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => "now".to_string(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// A rebase that stopped on conflicts is not a failure: it is waiting for the user, so it is
/// reported with a toast and the conflict banner instead of an error.
fn report_rebase_conflicts(
    workspace: &WeakEntity<Workspace>,
    result: anyhow::Result<()>,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<()> {
    match result {
        Err(error) if stopped_on_conflicts(&error) => {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<Smartlog>(),
                        "Rebase stopped on conflicts. Resolve them, then press Continue.",
                    ),
                    cx,
                );
            })?;
            Ok(())
        }
        other => other,
    }
}

fn stopped_on_conflicts(error: &anyhow::Error) -> bool {
    error.to_string().contains("CONFLICT")
}

const MAX_OPERATION_HISTORY: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationState {
    Running,
    Succeeded,
    Failed,
}

struct OperationRecord {
    id: u64,
    name: SharedString,
    state: OperationState,
}

/// Turns a failure title such as `Failed to rebase` into the name shown in the history, `Rebase`.
fn operation_name(failure_title: &str) -> String {
    let name = failure_title
        .strip_prefix("Failed to ")
        .unwrap_or(failure_title);
    let mut characters = name.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

#[derive(Clone)]
struct DraggedCommit {
    sha: Oid,
    label: SharedString,
}

struct DraggedCommitPreview {
    label: SharedString,
}

impl Render for DraggedCommitPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

struct SmartlogContextMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    _subscription: Subscription,
}

struct CreateBookmarkModal {
    commit: Oid,
    editor: Entity<Editor>,
    repository: Entity<Repository>,
}

impl CreateBookmarkModal {
    fn new(
        commit: Oid,
        repository: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Bookmark name", window, cx);
            editor
        });
        Self {
            commit,
            editor,
            repository,
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            return;
        }
        let repository = self.repository.clone();
        let commit = self.commit.to_string();
        cx.spawn(async move |_, cx| {
            repository
                .update(cx, |repository, _| {
                    repository.create_ref(format!("refs/heads/{name}"), commit)
                })
                .await??;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to create bookmark", window, cx, |_, _, _| None);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for CreateBookmarkModal {}
impl ModalView for CreateBookmarkModal {}

impl Focusable for CreateBookmarkModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for CreateBookmarkModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CreateBookmarkModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::Bookmark).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!(
                            "Create Bookmark at {}",
                            self.commit.display_short()
                        ))
                        .size(HeadlineSize::XSmall),
                    ),
            )
            .child(div().px_3().pb_3().w_full().child(self.editor.clone()))
    }
}

pub struct Smartlog {
    focus_handle: FocusHandle,
    context_menu: Option<SmartlogContextMenu>,
    filter_editor: Entity<Editor>,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    repository_id: RepositoryId,
    trunk: SharedString,
    layout: Layout,
    segments: Vec<Vec<Segment>>,
    commits: HashMap<Oid, Arc<git::repository::CommitData>>,
    ref_names: HashMap<Oid, Vec<SharedString>>,
    selection: HashSet<Oid>,
    selection_anchor: Option<Oid>,
    selection_cursor: Option<Oid>,
    list_scroll_handle: ScrollHandle,
    head: Option<Oid>,
    parents: HashMap<Oid, Option<Oid>>,
    /// Every commit in the draft log, including hidden ones, unlike `parents`.
    all_draft_shas: HashSet<Oid>,
    hidden: HashSet<Oid>,
    hidden_closure: HashSet<Oid>,
    protected_from_hiding: HashSet<Oid>,
    show_hidden: bool,
    shelf_collapsed: bool,
    settings: SmartlogSettings,
    scroll_to_current_pending: bool,
    last_fetch: Option<SystemTime>,
    last_fetch_task: Option<Task<()>>,
    operations: VecDeque<OperationRecord>,
    next_operation_id: u64,
    history_expanded: bool,
    pending_selection: Option<Oid>,
    sidebar: SidebarState,
    uncommitted_files: Vec<StatusEntry>,
    /// Files the user unchecked. Everything else is included by Commit and Amend, so files that
    /// appear later are selected by default, as in ISL.
    deselected: HashSet<RepoPath>,
    title_editor: Entity<Editor>,
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Title", window, cx);
            editor
        });

        let sidebar = SidebarState::new(window, cx);
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter commits…", window, cx);
            editor
        });
        cx.subscribe(&filter_editor, |_, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.notify();
            }
        })
        .detach();

        cx.subscribe_in(&git_store, window, |this, _, event, window, cx| {
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
                | RepositoryEvent::StatusesChanged => this.refresh(window, cx),
                RepositoryEvent::StashEntriesChanged => cx.notify(),
                _ => {}
            }
        })
        .detach();

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            context_menu: None,
            filter_editor,
            git_store,
            workspace,
            repository_id,
            trunk,
            layout: Layout::default(),
            segments: Vec::new(),
            commits: HashMap::default(),
            ref_names: HashMap::default(),
            selection: HashSet::default(),
            selection_anchor: None,
            selection_cursor: None,
            list_scroll_handle: ScrollHandle::new(),
            head: None,
            parents: HashMap::default(),
            all_draft_shas: HashSet::default(),
            hidden: HashSet::default(),
            hidden_closure: HashSet::default(),
            protected_from_hiding: HashSet::default(),
            show_hidden: false,
            shelf_collapsed: false,
            settings: SmartlogSettings::default(),
            scroll_to_current_pending: true,
            last_fetch: None,
            last_fetch_task: None,
            operations: VecDeque::new(),
            next_operation_id: 0,
            history_expanded: false,
            pending_selection: None,
            sidebar,
            uncommitted_files: Vec::new(),
            deselected: HashSet::default(),
            title_editor,
            error: None,
            pending_commit_loads: Vec::new(),
        };
        this.refresh(window, cx);
        this
    }

    fn repository(&self, cx: &App) -> Option<Entity<Repository>> {
        self.git_store
            .read(cx)
            .repositories()
            .get(&self.repository_id)
            .cloned()
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        self.head = head;
        self.uncommitted_files = repository.read(cx).cached_status().collect();
        let present_paths: HashSet<&RepoPath> = self
            .uncommitted_files
            .iter()
            .map(|entry| &entry.repo_path)
            .collect();
        self.deselected.retain(|path| present_paths.contains(path));

        let mut drafts: Vec<(Oid, Option<Oid>)> = draft_commits
            .iter()
            .map(|commit| (commit.sha, commit.parents.first().copied()))
            .collect();
        for commit in &draft_commits {
            self.ref_names.insert(commit.sha, commit.ref_names.clone());
        }

        let all_parents: HashMap<Oid, Option<Oid>> = drafts.iter().copied().collect();
        self.all_draft_shas = all_parents.keys().copied().collect();
        self.hidden = draft_commits
            .iter()
            .filter(|commit| {
                commit
                    .ref_names
                    .iter()
                    .any(|name| name.starts_with(HIDDEN_COMMIT_REF_PREFIX))
            })
            .map(|commit| commit.sha)
            .collect();
        self.protected_from_hiding = lineage_of(&all_parents, head);
        self.hidden_closure =
            hidden_closure(&all_parents, &self.hidden, &self.protected_from_hiding);
        if !self.show_hidden {
            drafts.retain(|(sha, _)| !self.hidden_closure.contains(sha));
        }
        self.parents = drafts.iter().copied().collect();

        self.layout = build_layout(&drafts, head, self.uncommitted_files.len());
        self.segments = row_segments(&self.layout);
        if self.scroll_to_current_pending && self.settings.scroll_to_current && self.head.is_some()
        {
            self.scroll_to_current_commit();
            self.scroll_to_current_pending = false;
        }
        let present: HashSet<Oid> = self.layout.rows.iter().filter_map(|row| row.sha).collect();
        self.selection.retain(|sha| present.contains(sha));
        if self
            .selection_anchor
            .is_some_and(|anchor| !present.contains(&anchor))
        {
            self.selection_anchor = None;
        }
        if let Some(pending) = self.pending_selection
            && present.contains(&pending)
        {
            self.selection = HashSet::from_iter([pending]);
            self.selection_anchor = Some(pending);
            self.selection_cursor = Some(pending);
            self.pending_selection = None;
        }

        let shas: Vec<Oid> = self.layout.rows.iter().filter_map(|row| row.sha).collect();
        for sha in shas {
            self.load_commit(&repository, sha, window, cx);
        }
        self.sync_sidebar(window, cx);
        self.update_last_fetch(cx);
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn update_last_fetch(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        let path = repository
            .read(cx)
            .repository_dir_abs_path
            .join("FETCH_HEAD");
        // The workspace is read from inside the task because this can run while the workspace
        // itself is being updated, such as when the Smartlog is first opened.
        self.last_fetch_task = Some(cx.spawn(async move |this, cx| {
            let Some(fs) = workspace
                .read_with(cx, |workspace, cx| {
                    workspace.project().read(cx).fs().clone()
                })
                .log_err()
            else {
                return;
            };
            let modified = fs
                .metadata(&path)
                .await
                .log_err()
                .flatten()
                .map(|metadata| metadata.mtime.timestamp_for_user());
            this.update(cx, |this, cx| {
                if this.last_fetch != modified {
                    this.last_fetch = modified;
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn load_commit(
        &mut self,
        repository: &Entity<Repository>,
        sha: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                let task = cx.spawn_in(window, async move |this, cx| {
                    if let Ok(data) = receiver.await {
                        this.update_in(cx, |this, window, cx| {
                            this.commits.insert(sha, data);
                            this.sync_sidebar(window, cx);
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
        let task = cx.spawn_in(window, async move |_, cx| {
            repository
                .update(cx, |repository, _| {
                    repository.change_branch(sha.to_string())
                })
                .await??;
            anyhow::Ok(())
        });
        self.run_logged("Failed to go to commit", task, window, cx);
    }

    fn row_height(&self) -> Pixels {
        if self.settings.compact {
            COMPACT_ROW_HEIGHT
        } else {
            ROW_HEIGHT
        }
    }

    /// What copying a commit's hash puts on the clipboard, following the setting.
    fn copy_hash_text(&self, sha: Oid) -> String {
        if self.settings.copy_short_hash {
            sha.display_short()
        } else {
            sha.to_string()
        }
    }

    fn scroll_to_current_commit(&mut self) {
        if let Some(row) = self
            .layout
            .rows
            .iter()
            .position(|row| row.is_head && row.sha.is_some())
        {
            self.list_scroll_handle.scroll_to_item(row);
        }
    }

    fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut SmartlogSettings),
    ) {
        update(&mut self.settings);
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn deploy_settings_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let smartlog = cx.entity().downgrade();
        let focus_handle = self.focus_handle.clone();
        let settings = self.settings.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let toggle = |label: &'static str, checked: bool, update: fn(&mut SmartlogSettings)| {
                let smartlog = smartlog.clone();
                (label, checked, move |_: &mut Window, cx: &mut App| {
                    smartlog
                        .update(cx, |this, cx| this.update_settings(cx, update))
                        .log_err();
                })
            };
            let (label, checked, handler) = toggle(
                "Click Filename to Open Diff View",
                settings.click_opens_diff,
                |settings| settings.click_opens_diff = !settings.click_opens_diff,
            );
            let menu = menu
                .context(focus_handle)
                .header("Settings")
                .toggleable_entry(label, checked, IconPosition::Start, None, handler);
            let (label, checked, handler) = toggle("Compact Mode", settings.compact, |settings| {
                settings.compact = !settings.compact
            });
            let menu = menu.toggleable_entry(label, checked, IconPosition::Start, None, handler);
            let (label, checked, handler) = toggle(
                "Scroll to Current Commit on Open",
                settings.scroll_to_current,
                |settings| settings.scroll_to_current = !settings.scroll_to_current,
            );
            let menu = menu.toggleable_entry(label, checked, IconPosition::Start, None, handler);
            let (label, checked, handler) = toggle(
                "Copy Short Commit Hashes",
                settings.copy_short_hash,
                |settings| settings.copy_short_hash = !settings.copy_short_hash,
            );
            let menu = menu.toggleable_entry(label, checked, IconPosition::Start, None, handler);
            let (label, checked, handler) = toggle(
                "Confirm Before Rebasing onto the Trunk",
                settings.confirm_rebase_onto_trunk,
                |settings| settings.confirm_rebase_onto_trunk = !settings.confirm_rebase_onto_trunk,
            );
            menu.toggleable_entry(label, checked, IconPosition::Start, None, handler)
        });
        self.show_context_menu(menu, position, window, cx);
    }

    fn selectable_rows(&self) -> Vec<SelectableRow> {
        self.layout
            .rows
            .iter()
            .filter_map(|row| {
                Some(SelectableRow {
                    sha: row.sha?,
                    is_public: row.kind == RowKind::Public,
                })
            })
            .collect()
    }

    /// The selected commits, in the order they are displayed.
    fn selected_in_display_order(&self) -> Vec<Oid> {
        self.layout
            .rows
            .iter()
            .filter_map(|row| row.sha)
            .filter(|sha| self.selection.contains(sha))
            .collect()
    }

    fn click_commit(
        &mut self,
        sha: Oid,
        modifiers: gpui::Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (selection, anchor) = apply_click(
            &self.selectable_rows(),
            &self.selection,
            self.selection_anchor,
            sha,
            modifiers.shift,
            modifiers.secondary(),
        );
        self.selection = selection;
        self.selection_anchor = anchor;
        self.selection_cursor = Some(sha);
        window.focus(&self.focus_handle, cx);
        self.sync_sidebar(window, cx);
    }

    fn move_cursor(
        &mut self,
        forward: bool,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.selectable_rows();
        let Some(target) = step_cursor(&rows, self.selection_cursor, self.head, forward) else {
            return;
        };
        if extend {
            let anchor = self
                .selection_anchor
                .or(self.selection_cursor)
                .unwrap_or(target);
            let (selection, anchor) =
                apply_click(&rows, &self.selection, Some(anchor), target, true, false);
            self.selection = selection;
            self.selection_anchor = anchor;
        } else {
            self.selection = HashSet::from_iter([target]);
            self.selection_anchor = Some(target);
        }
        self.selection_cursor = Some(target);
        if let Some(row) = self
            .layout
            .rows
            .iter()
            .position(|row| row.sha == Some(target))
        {
            self.list_scroll_handle.scroll_to_item(row);
        }
        self.sync_sidebar(window, cx);
    }

    fn select_previous_commit(
        &mut self,
        _: &SelectPreviousCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(false, false, window, cx);
    }

    fn select_next_commit(
        &mut self,
        _: &SelectNextCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(true, false, window, cx);
    }

    fn extend_selection_up(
        &mut self,
        _: &ExtendSelectionUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(false, true, window, cx);
    }

    fn extend_selection_down(
        &mut self,
        _: &ExtendSelectionDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(true, true, window, cx);
    }

    fn open_details(&mut self, _: &OpenDetails, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.collapsed = false;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn clear_selection(&mut self, _: &ClearSelection, window: &mut Window, cx: &mut Context<Self>) {
        self.selection.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        self.sync_sidebar(window, cx);
    }

    fn select_all_commits(
        &mut self,
        _: &SelectAllCommits,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = self
            .selectable_rows()
            .into_iter()
            .filter(|row| !row.is_public)
            .map(|row| row.sha)
            .collect();
        self.sync_sidebar(window, cx);
    }

    fn hide_selected_commits(
        &mut self,
        _: &HideSelectedCommits,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hideable: Vec<Oid> = self
            .selected_in_display_order()
            .into_iter()
            .filter(|sha| {
                !self.protected_from_hiding.contains(sha) && !self.hidden_closure.contains(sha)
            })
            .collect();
        self.hide_commits(hideable, window, cx);
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.collapsed = !self.sidebar.collapsed;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.filter_editor.focus_handle(cx), cx);
    }

    fn pull(&mut self, _: &Pull, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(git::Fetch), cx);
    }

    fn rebase_onto_trunk(
        &mut self,
        _: &RebaseOntoTrunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(plan) = self.head_rebase_plan(cx) {
            self.rebase_stack(plan, window, cx);
        }
    }

    fn shelve_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let selected = self.selected_paths();
        if selected.is_empty() {
            return;
        }
        let title = self.title_editor.read(cx).text(cx);
        let title = title.trim();
        let message = (!title.is_empty()).then(|| title.to_string());

        let task = repository.update(cx, |repository, cx| {
            repository.stash_entries(selected, message, cx)
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            task.await?;
            this.update_in(cx, |this, window, cx| {
                this.title_editor
                    .update(cx, |editor, cx| editor.set_text("", window, cx));
            })?;
            anyhow::Ok(())
        });
        self.run_logged("Failed to shelve changes", task, window, cx);
    }

    fn unshelve(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let task = repository.update(cx, |repository, cx| repository.stash_pop(Some(index), cx));
        let task = cx.spawn_in(window, async move |_, _| {
            task.await?;
            anyhow::Ok(())
        });
        self.run_logged("Failed to unshelve changes", task, window, cx);
    }

    fn delete_shelved(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            "Delete these shelved changes?",
            Some("This can't be undone."),
            &["Delete", "Cancel"],
            cx,
        );
        let task = cx.spawn_in(window, async move |_, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }
            repository
                .update(cx, |repository, cx| repository.stash_drop(Some(index), cx))
                .await??;
            Ok(())
        });
        self.run_logged("Failed to delete shelved changes", task, window, cx);
    }

    fn render_shelved_changes(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let repository = self.repository(cx)?;
        let entries = repository.read(cx).stash_entries.entries.clone();
        if entries.is_empty() {
            return None;
        }
        let collapsed = self.shelf_collapsed;

        Some(
            v_flex()
                .mt_4()
                .px_4()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            IconButton::new(
                                "smartlog-toggle-shelf",
                                if collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                },
                            )
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.shelf_collapsed = !this.shelf_collapsed;
                                cx.notify();
                            })),
                        )
                        .child(
                            Label::new("SHELVED CHANGES")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .weight(gpui::FontWeight::BOLD),
                        )
                        .child(self.badge(entries.len().to_string(), cx)),
                )
                .when(!collapsed, |this| {
                    this.children(entries.iter().map(|entry| {
                        let index = entry.index;
                        let timestamp = OffsetDateTime::from_unix_timestamp(entry.timestamp)
                            .map(|timestamp| {
                                time_format::format_local_timestamp(
                                    timestamp,
                                    OffsetDateTime::now_utc(),
                                    TimestampFormat::Relative,
                                )
                            })
                            .unwrap_or_default();
                        h_flex()
                            .h_8()
                            .gap_2()
                            .child(
                                Label::new(shelf_entry_label(&entry.message).to_string())
                                    .truncate(),
                            )
                            .child(
                                Label::new(timestamp)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .flex_none(),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new(("smartlog-unshelve", index), "Unshelve")
                                    .style(ButtonStyle::Filled)
                                    .size(ButtonSize::Compact)
                                    .tooltip(Tooltip::text(
                                        "Apply these changes to the working tree and remove the shelf",
                                    ))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.unshelve(index, window, cx);
                                    })),
                            )
                            .child(
                                IconButton::new(("smartlog-delete-shelf", index), IconName::Trash)
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Delete shelved changes"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.delete_shelved(index, window, cx);
                                    })),
                            )
                    }))
                })
                .into_any_element(),
        )
    }

    /// Opens one diff of everything the selected, unbroken chain of commits changed.
    fn view_changes_across_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let Some(chain) = fold_chain(&self.parents, &self.selection) else {
            return;
        };
        let (Some(first), Some(last)) = (chain.first(), chain.last()) else {
            return;
        };
        let Some(base) = self.parents.get(first).copied().flatten() else {
            return;
        };
        let (base, tip) = (base.to_string(), last.to_string());
        let workspace = self.workspace.clone();

        let task = cx.spawn_in(window, async move |_, cx| {
            let sha = repository
                .update(cx, |repository, _| repository.range_commit(base, tip))
                .await??;
            cx.update(|window, cx| {
                window.defer(cx, move |window, cx| {
                    CommitView::open(
                        sha,
                        repository.downgrade(),
                        workspace,
                        None,
                        None,
                        window,
                        cx,
                    );
                });
            })?;
            anyhow::Ok(())
        });
        self.run_logged("Failed to view the changes", task, window, cx);
    }

    fn fold_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let Some(chain) = fold_chain(&self.parents, &self.selection) else {
            return;
        };
        let messages: Option<Vec<String>> = chain
            .iter()
            .map(|sha| {
                self.commits
                    .get(sha)
                    .map(|commit| commit.message.trim().to_string())
            })
            .collect();
        let Some(messages) = messages else {
            return;
        };
        let message = messages.join("\n\n");
        let shas: Vec<String> = chain.iter().map(|sha| sha.to_string()).collect();
        let count = chain.len();

        let prompt = window.prompt(
            PromptLevel::Warning,
            &format!("Combine {count} commits into one?"),
            Some("The commits are replaced by a single commit with their messages joined."),
            &["Fold", "Cancel"],
            cx,
        );
        let task = cx.spawn_in(window, async move |this, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }
            let folded = repository
                .update(cx, |repository, _| repository.fold_commits(shas, message))
                .await??;
            this.update_in(cx, |this, window, cx| {
                this.selection.clear();
                this.selection_anchor = None;
                this.pending_selection = folded.parse::<Oid>().ok();
                this.refresh(window, cx);
            })?;
            Ok(())
        });
        self.run_logged("Failed to fold commits", task, window, cx);
    }

    fn filter_text(&self, cx: &App) -> String {
        self.filter_editor.read(cx).text(cx).trim().to_lowercase()
    }

    fn matches_filter(&self, sha: Oid, filter: &str) -> bool {
        let commit = self.commits.get(&sha);
        commit_matches_filter(
            filter,
            sha,
            commit.map(|commit| commit.subject.as_ref()),
            commit.map(|commit| (commit.author_name.as_ref(), commit.author_email.as_ref())),
            self.ref_names.get(&sha).map_or(&[], Vec::as_slice),
        )
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        row_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.layout.rows.get(row_index).cloned() else {
            return;
        };
        let Some(sha) = row.sha else {
            return;
        };
        let Some(repository) = self.repository(cx) else {
            return;
        };
        if !self.selection.contains(&sha) {
            self.selection = HashSet::from_iter([sha]);
            self.selection_anchor = Some(sha);
            self.sync_sidebar(window, cx);
        }

        let is_head = row.is_head;
        let is_draft = row.kind == RowKind::Draft;
        let rebase_plan = if is_draft && !self.rebase_in_progress(cx) {
            self.rebase_plan(sha, cx)
        } else {
            None
        };
        let submit_stack = rebase_plan.is_some() && !self.stack_branches(sha, cx).is_empty();
        let can_hide = is_draft
            && !self.protected_from_hiding.contains(&sha)
            && !self.hidden_closure.contains(&sha);
        let is_hidden_root = self.hidden.contains(&sha);
        let edit_stack = if is_draft {
            self.editable_stack(sha, cx)
        } else {
            None
        };
        let can_split = is_draft
            && self
                .commits
                .get(&sha)
                .is_some_and(|commit| commit.parents.len() == 1)
            && !self.rebase_in_progress(cx);
        let split_details = self
            .commits
            .get(&sha)
            .map(|commit| (commit.subject.to_string(), commit.message.to_string()));
        let can_amend_to = is_draft
            && !is_head
            && self.protected_from_hiding.contains(&sha)
            && !self.selected_paths().is_empty()
            && !self.rebase_in_progress(cx);
        let trunk = self.trunk.clone();
        let copy_text = self.copy_hash_text(sha);
        let smartlog = cx.entity().downgrade();
        let workspace = self.workspace.clone();
        let focus_handle = self.focus_handle.clone();

        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus_handle)
                .header(format!("Commit {}", sha.display_short()))
                .when(!is_head, |menu| {
                    menu.entry("Goto", None, {
                        let smartlog = smartlog.clone();
                        move |window, cx| {
                            smartlog
                                .update(cx, |this, cx| this.goto_commit(sha, window, cx))
                                .log_err();
                        }
                    })
                })
                .entry("Copy Hash", None, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                })
                .entry("View Changes in Commit", None, {
                    let smartlog = smartlog.clone();
                    move |window, cx| {
                        smartlog
                            .update(cx, |this, cx| this.open_commit(sha, window, cx))
                            .log_err();
                    }
                })
                .separator()
                .when(can_amend_to, |menu| {
                    menu.entry("Amend Changes to Here", None, {
                        let smartlog = smartlog.clone();
                        move |window, cx| {
                            smartlog
                                .update(cx, |this, cx| this.amend_changes_to(sha, window, cx))
                                .log_err();
                        }
                    })
                })
                .when_some(
                    split_details.filter(|_| can_split),
                    |menu, (subject, message)| {
                        menu.entry("Split…", None, {
                            let smartlog = smartlog.clone();
                            let workspace = workspace.clone();
                            let repository = repository.clone();
                            move |window, cx| {
                                let smartlog = smartlog.clone();
                                let repository = repository.clone();
                                let subject = subject.clone();
                                let message = message.clone();
                                workspace
                                    .update(cx, |workspace, cx| {
                                        workspace.toggle_modal(window, cx, |window, cx| {
                                            split::SplitModal::new(
                                                smartlog, repository, sha, &subject, message,
                                                window, cx,
                                            )
                                        });
                                    })
                                    .log_err();
                            }
                        })
                    },
                )
                .when_some(edit_stack, |menu, (base, tip, entries)| {
                    menu.entry("Edit Stack…", None, {
                        let smartlog = smartlog.clone();
                        let workspace = workspace.clone();
                        move |window, cx| {
                            let smartlog = smartlog.clone();
                            let entries = entries.clone();
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.toggle_modal(window, cx, |_, cx| {
                                        edit_stack::EditStackModal::new(
                                            smartlog, base, tip, entries, cx,
                                        )
                                    });
                                })
                                .log_err();
                        }
                    })
                })
                .when_some(rebase_plan, |menu, plan| {
                    menu.entry(format!("Rebase onto {trunk}"), None, {
                        let smartlog = smartlog.clone();
                        move |window, cx| {
                            let plan = plan.clone();
                            smartlog
                                .update(cx, |this, cx| this.rebase_stack(plan, window, cx))
                                .log_err();
                        }
                    })
                })
                .when(submit_stack, |menu| {
                    menu.entry("Submit Stack", None, {
                        let smartlog = smartlog.clone();
                        move |window, cx| {
                            smartlog
                                .update(cx, |this, cx| this.submit_stack(sha, window, cx))
                                .log_err();
                        }
                    })
                })
                .entry("Create Bookmark…", None, {
                    let repository = repository.clone();
                    let workspace = workspace.clone();
                    move |window, cx| {
                        let repository = repository.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    CreateBookmarkModal::new(sha, repository, window, cx)
                                });
                            })
                            .log_err();
                    }
                })
                .entry("Create Tag…", None, {
                    let repository = repository.clone();
                    let workspace = workspace.clone();
                    move |window, cx| {
                        let repository = repository.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                crate::create_tag_at_commit(
                                    sha, is_head, repository, workspace, window, cx,
                                );
                            })
                            .log_err();
                    }
                })
                .when(
                    can_hide || is_hidden_root || (is_head && is_draft),
                    |menu| menu.separator(),
                )
                .when(can_hide, |menu| {
                    menu.entry("Hide Commit and Descendants", None, {
                        let smartlog = smartlog.clone();
                        move |window, cx| {
                            smartlog
                                .update(cx, |this, cx| this.hide_commit(sha, window, cx))
                                .log_err();
                        }
                    })
                })
                .when(is_hidden_root, |menu| {
                    menu.entry("Show Commit", None, {
                        let smartlog = smartlog.clone();
                        move |window, cx| {
                            smartlog
                                .update(cx, |this, cx| this.unhide_commit(sha, window, cx))
                                .log_err();
                        }
                    })
                })
                .when(is_head && is_draft, |menu| {
                    menu.entry("Uncommit", None, {
                        let smartlog = smartlog.clone();
                        move |window, cx| {
                            smartlog
                                .update(cx, |this, cx| this.uncommit(window, cx))
                                .log_err();
                        }
                    })
                })
        });

        self.show_context_menu(menu, position, window, cx);
    }

    fn show_context_menu(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, window, cx| {
                if menu.focus_handle(cx).contains_focused(window, cx) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            });
        self.context_menu = Some(SmartlogContextMenu {
            menu,
            position,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn goto_bookmark(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let task = cx.spawn_in(window, async move |_, cx| {
            repository
                .update(cx, |repository, _| repository.change_branch(name))
                .await??;
            anyhow::Ok(())
        });
        self.run_logged("Failed to go to bookmark", task, window, cx);
    }

    fn delete_bookmarks(
        &mut self,
        names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        if names.is_empty() {
            return;
        }
        let prompt = window.prompt(
            PromptLevel::Warning,
            &if names.len() == 1 {
                format!("Delete the bookmark {}?", names[0])
            } else {
                format!("Delete {} bookmarks?", names.len())
            },
            Some("The commits stay in git if another branch still contains them."),
            &["Delete", "Cancel"],
            cx,
        );
        let task = cx.spawn_in(window, async move |_, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }
            for name in names {
                repository
                    .update(cx, |repository, _| {
                        repository.delete_branch(false, name, true)
                    })
                    .await??;
            }
            Ok(())
        });
        self.run_logged("Failed to delete bookmarks", task, window, cx);
    }

    fn hide_commit(&mut self, sha: Oid, window: &mut Window, cx: &mut Context<Self>) {
        self.hide_commits(vec![sha], window, cx);
    }

    fn hide_commits(&mut self, shas: Vec<Oid>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        if shas.is_empty() {
            return;
        }
        let prompt = window.prompt(
            PromptLevel::Warning,
            if shas.len() == 1 {
                "Hide this commit and everything built on it?"
            } else {
                "Hide these commits and everything built on them?"
            },
            Some("The commits stay in git and can be shown again with Show hidden."),
            &["Hide", "Cancel"],
            cx,
        );
        let task = cx.spawn_in(window, async move |_, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }
            for sha in shas {
                repository
                    .update(cx, |repository, _| {
                        repository.set_commit_hidden(sha.to_string(), true)
                    })
                    .await??;
            }
            Ok(())
        });
        self.run_logged("Failed to hide commits", task, window, cx);
    }

    fn unhide_commit(&mut self, sha: Oid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let task = cx.spawn_in(window, async move |_, cx| {
            repository
                .update(cx, |repository, _| {
                    repository.set_commit_hidden(sha.to_string(), false)
                })
                .await??;
            anyhow::Ok(())
        });
        self.run_logged("Failed to show commit", task, window, cx);
    }

    /// The branch names shown beside a commit. Trunk commits aren't in the draft log, so their
    /// names come from the branch list, and hidden-commit markers are never shown.
    fn chip_names(&self, sha: Oid, kind: RowKind, cx: &App) -> Vec<String> {
        if kind == RowKind::Public {
            let Some(repository) = self.repository(cx) else {
                return Vec::new();
            };
            let sha = sha.to_string();
            let mut names: Vec<String> = repository
                .read(cx)
                .branch_list
                .iter()
                .filter(|branch| {
                    branch
                        .most_recent_commit
                        .as_ref()
                        .is_some_and(|commit| commit.sha.as_ref() == sha)
                })
                .map(|branch| branch.name().to_string())
                .collect();
            names.dedup();
            return names;
        }

        self.ref_names
            .get(&sha)
            .into_iter()
            .flatten()
            .filter(|name| {
                !name.starts_with("tag: ") && !name.starts_with(HIDDEN_COMMIT_REF_PREFIX)
            })
            .map(|name| name.strip_prefix("HEAD -> ").unwrap_or(name).to_string())
            .collect()
    }

    fn local_branch_at(&self, sha: Oid, cx: &App) -> Option<String> {
        let repository = self.repository(cx)?;
        let repository = repository.read(cx);
        let ref_names = self.ref_names.get(&sha)?;
        ref_names.iter().find_map(|ref_name| {
            let name = ref_name.strip_prefix("HEAD -> ").unwrap_or(ref_name);
            repository
                .branch_list
                .iter()
                .any(|branch| !branch.is_remote() && branch.name() == name)
                .then(|| name.to_string())
        })
    }

    /// What to rebase for a stack ending at `tip`: its branch, or the commit itself for a stack
    /// that only a kept ref holds up.
    fn rebase_target_at(&self, tip: Oid, cx: &App) -> Option<String> {
        Some(
            self.local_branch_at(tip, cx)
                .unwrap_or_else(|| tip.to_string()),
        )
    }

    fn rebase_plan(&self, root: Oid, cx: &App) -> Option<RebasePlan> {
        plan_rebase(&self.parents, self.head, root, |tip| {
            self.rebase_target_at(tip, cx)
        })
    }

    fn rebase_in_progress(&self, cx: &App) -> bool {
        self.repository(cx)
            .is_some_and(|repository| repository.read(cx).merge.rebase_in_progress)
    }

    fn unresolved_conflict_count(&self) -> usize {
        self.uncommitted_files
            .iter()
            .filter(|entry| entry.status.is_conflicted())
            .count()
    }

    fn run_rebase(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        failure_title: &'static str,
        operation: impl FnOnce(
            &mut Repository,
            &mut Context<Repository>,
        ) -> futures::channel::oneshot::Receiver<anyhow::Result<()>>
        + 'static,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        let task = cx.spawn_in(window, async move |_, cx| {
            let result = repository
                .update(cx, |repository, cx| operation(repository, cx))
                .await?;
            report_rebase_conflicts(&workspace, result, cx)
        });
        self.run_logged(failure_title, task, window, cx);
    }

    /// Folds the checked uncommitted changes into `sha`, an older commit on the checked-out
    /// branch, by committing them as a fixup and squashing it into place.
    fn amend_changes_to(&mut self, sha: Oid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let selected = self.selected_paths();
        if selected.is_empty() {
            return;
        }
        let unselected: Vec<RepoPath> = self.deselected.iter().cloned().collect();
        let workspace = self.workspace.clone();

        let task = cx.spawn_in(window, async move |_, cx| {
            repository
                .update(cx, |repository, cx| repository.stage_entries(selected, cx))
                .await?;
            if !unselected.is_empty() {
                repository
                    .update(cx, |repository, cx| {
                        repository.unstage_entries(unselected, cx)
                    })
                    .await?;
            }
            let result = repository
                .update(cx, |repository, cx| {
                    repository.amend_to(sha.to_string(), cx)
                })
                .await?;
            report_rebase_conflicts(&workspace, result, cx)
        });
        self.run_logged("Failed to amend changes to commit", task, window, cx);
    }

    /// Rebases every stack onto the trunk, one after another. Stacks that aren't checked out are
    /// rebased by checking their branch out, so the original branch is restored in between and
    /// the checked-out stack goes last. Stops at the first stack that needs attention.
    fn rebase_all_stacks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        if self.rebase_in_progress(cx) {
            return;
        }
        let original_branch = repository
            .read(cx)
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string());
        let mut roots: Vec<Oid> = self
            .parents
            .iter()
            .filter(|(_, parent)| parent.is_some_and(|parent| !self.parents.contains_key(&parent)))
            .map(|(root, _)| *root)
            .collect();
        roots.sort_by_key(|root| {
            self.layout
                .rows
                .iter()
                .position(|row| row.sha == Some(*root))
        });
        let mut plans: Vec<RebasePlan> = roots
            .into_iter()
            .filter_map(|root| self.rebase_plan(root, cx))
            .filter(|plan| plan.branch.is_none() || original_branch.is_some())
            .collect();
        plans.sort_by_key(|plan| plan.branch.is_none());
        if plans.is_empty() {
            return;
        }

        let trunk = self.trunk.to_string();
        let workspace = self.workspace.clone();
        let task = cx.spawn_in(window, async move |_, cx| {
            for plan in plans {
                let moves_other_branch = plan.branch.is_some();
                let result = repository
                    .update(cx, |repository, cx| {
                        repository.rebase_onto(
                            trunk.clone(),
                            plan.old_base.to_string(),
                            plan.branch,
                            cx,
                        )
                    })
                    .await?;
                let stopped = result.as_ref().is_err_and(stopped_on_conflicts);
                report_rebase_conflicts(&workspace, result, cx)?;
                if stopped {
                    return anyhow::Ok(());
                }
                if moves_other_branch && let Some(original_branch) = original_branch.clone() {
                    repository
                        .update(cx, |repository, _| {
                            repository.change_branch(original_branch)
                        })
                        .await??;
                }
            }
            Ok(())
        });
        self.run_logged("Failed to rebase all stacks", task, window, cx);
    }

    fn deploy_bulk_actions_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let smartlog = cx.entity().downgrade();
        let focus_handle = self.focus_handle.clone();
        let trunk = self.trunk.clone();
        let merged = self.merged_bookmark_names(cx);
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus_handle)
                .header("Bulk Actions")
                .entry("Scroll to Current Commit", None, {
                    let smartlog = smartlog.clone();
                    move |_, cx| {
                        smartlog
                            .update(cx, |this, cx| {
                                this.scroll_to_current_commit();
                                cx.notify();
                            })
                            .log_err();
                    }
                })
                .entry("Select All", None, {
                    let smartlog = smartlog.clone();
                    move |window, cx| {
                        smartlog
                            .update(cx, |this, cx| {
                                this.select_all_commits(&SelectAllCommits, window, cx)
                            })
                            .log_err();
                    }
                })
                .entry(format!("Rebase All onto {trunk}"), None, {
                    let smartlog = smartlog.clone();
                    move |window, cx| {
                        smartlog
                            .update(cx, |this, cx| this.rebase_all_stacks(window, cx))
                            .log_err();
                    }
                })
                .item(
                    ContextMenuEntry::new(format!("Clean Up All ({})", merged.len()))
                        .disabled(merged.is_empty())
                        .handler({
                            let smartlog = smartlog.clone();
                            move |window, cx| {
                                let merged = merged.clone();
                                smartlog
                                    .update(cx, |this, cx| {
                                        this.delete_bookmarks(merged, window, cx)
                                    })
                                    .log_err();
                            }
                        }),
                )
        });
        self.show_context_menu(menu, position, window, cx);
    }

    /// Where `sha` sits in a stack that can be edited, as the modal needs it: the commit the
    /// stack is based on, its tip, and its commits oldest first.
    fn editable_stack(
        &self,
        sha: Oid,
        cx: &App,
    ) -> Option<(Oid, Oid, Vec<edit_stack::StackEntry>)> {
        if self.rebase_in_progress(cx) {
            return None;
        }
        let mut root = sha;
        while let Some(parent) = self.parents.get(&root).copied().flatten()
            && self.parents.contains_key(&parent)
        {
            root = parent;
        }
        let base = self.parents.get(&root).copied().flatten()?;
        let chain = edit_stack::linear_stack(&self.parents, root)?;
        let tip = *chain.last()?;
        let entries = chain
            .into_iter()
            .map(|sha| {
                let commit = self.commits.get(&sha);
                edit_stack::StackEntry {
                    sha,
                    subject: commit
                        .map(|commit| commit.subject.clone())
                        .unwrap_or_else(|| sha.display_short().into()),
                    message: commit
                        .map(|commit| commit.message.to_string())
                        .unwrap_or_default(),
                    dropped: false,
                    fold_into_previous: false,
                }
            })
            .collect();
        Some((base, tip, entries))
    }

    fn apply_stack_edit(
        &mut self,
        base: Oid,
        tip: Oid,
        steps: Vec<git::repository::StackStep>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let new_tip = repository
                .update(cx, |repository, _| {
                    repository.edit_stack(base.to_string(), tip.to_string(), steps)
                })
                .await??;
            this.update_in(cx, |this, window, cx| {
                this.selection.clear();
                this.selection_anchor = None;
                this.pending_selection = new_tip.parse::<Oid>().ok();
                this.refresh(window, cx);
            })?;
            anyhow::Ok(())
        });
        self.run_logged("Failed to edit the stack", task, window, cx);
    }

    /// The commit the stack containing `HEAD` is based on, which is where absorbing starts.
    fn head_stack_base(&self) -> Option<Oid> {
        let mut root = self.head?;
        if !self.parents.contains_key(&root) {
            return None;
        }
        while let Some(parent) = self.parents.get(&root).copied().flatten()
            && self.parents.contains_key(&parent)
        {
            root = parent;
        }
        self.parents.get(&root).copied().flatten()
    }

    fn open_absorb(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(base) = self.head_stack_base() else {
            return;
        };
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let smartlog = cx.weak_entity();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_, cx| {
                    absorb::AbsorbModal::new(smartlog, repository, base, cx)
                });
            })
            .log_err();
    }

    fn apply_absorb(&mut self, base: Oid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            repository
                .update(cx, |repository, _| {
                    repository.absorb(base.to_string(), true)
                })
                .await??;
            this.update_in(cx, |this, window, cx| this.refresh(window, cx))?;
            anyhow::Ok(())
        });
        self.run_logged("Failed to absorb changes", task, window, cx);
    }

    /// Fetches `refspec` from the default remote, keeps it reachable under a new local branch and,
    /// if asked, checks it out.
    fn download_commits(
        &mut self,
        refspec: String,
        go_to_it: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let askpass = self.askpass_delegate("git fetch", window, cx);
        let bookmark = download::download_bookmark_name(&refspec);

        let task = cx.spawn_in(window, async move |_, cx| {
            let remote = repository
                .update(cx, |repository, _| repository.get_remotes(None, false))
                .await??
                .into_iter()
                .next()
                .context("No remote is available to download from")?;
            repository
                .update(cx, |repository, cx| {
                    repository.fetch(FetchOptions::Ref { remote, refspec }, askpass, cx)
                })
                .await??;
            let fetched = repository
                .update(cx, |repository, cx| {
                    repository.show_commit("FETCH_HEAD".to_string(), cx)
                })
                .await?;
            repository
                .update(cx, |repository, _| {
                    repository.create_ref(format!("refs/heads/{bookmark}"), fetched.sha.to_string())
                })
                .await??;
            if go_to_it {
                repository
                    .update(cx, |repository, _| repository.change_branch(bookmark))
                    .await??;
            }
            anyhow::Ok(())
        });
        self.run_logged("Failed to download commits", task, window, cx);
    }

    fn create_initial_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let askpass = self.askpass_delegate("git commit", window, cx);
        let task = cx.spawn_in(window, async move |_, cx| {
            repository
                .update(cx, |repository, cx| {
                    repository.commit(
                        "Initial commit".into(),
                        None,
                        CommitOptions {
                            allow_empty: true,
                            ..Default::default()
                        },
                        askpass,
                        cx,
                    )
                })
                .await??;
            anyhow::Ok(())
        });
        self.run_logged("Failed to create the initial commit", task, window, cx);
    }

    /// The name of another worktree that has `sha` checked out, if there is one.
    fn worktree_checked_out_at(&self, sha: Oid, cx: &App) -> Option<String> {
        let repository = self.repository(cx)?;
        let repository = repository.read(cx);
        let sha = sha.to_string();
        repository
            .linked_worktrees
            .iter()
            .find(|worktree| {
                worktree.sha.as_ref() == sha
                    && !worktree.is_bare
                    && worktree.path != repository.work_directory_abs_path.as_ref()
            })
            .and_then(|worktree| {
                worktree
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
    }

    fn apply_split(
        &mut self,
        sha: Oid,
        first_paths: Vec<RepoPath>,
        first_hunks: Vec<git::repository::HunkSelection>,
        first_message: String,
        second_message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let second = repository
                .update(cx, |repository, _| {
                    repository.split_commit(
                        sha.to_string(),
                        first_paths,
                        first_hunks,
                        first_message,
                        second_message,
                    )
                })
                .await??;
            this.update_in(cx, |this, window, cx| {
                this.selection.clear();
                this.selection_anchor = None;
                this.pending_selection = second.parse::<Oid>().ok();
                this.refresh(window, cx);
            })?;
            anyhow::Ok(())
        });
        self.run_logged("Failed to split the commit", task, window, cx);
    }

    fn drag_rebase_plan(&self, source: Oid, target: Oid, cx: &App) -> Option<RebasePlan> {
        plan_drag_rebase(&self.parents, self.head, source, target, |tip| {
            self.rebase_target_at(tip, cx)
        })
    }

    /// Handles dropping a commit and its descendants onto `target`, after asking.
    fn rebase_dragged(
        &mut self,
        source: Oid,
        target: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rebase_in_progress(cx) {
            return;
        }
        let Some(plan) = self.drag_rebase_plan(source, target, cx) else {
            let reason = drag_rebase_refusal(&self.parents, source, target);
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace
                        .show_toast(Toast::new(NotificationId::unique::<Smartlog>(), reason), cx);
                })
                .log_err();
            return;
        };
        let count = stack_members(&self.parents, source).len();
        let target_label = self.commits.get(&target).map_or_else(
            || target.display_short(),
            |commit| commit.subject.to_string(),
        );
        let prompt = window.prompt(
            PromptLevel::Warning,
            &format!(
                "Rebase {} onto {target_label}?",
                if count == 1 {
                    "this commit".to_string()
                } else {
                    format!("these {count} commits")
                }
            ),
            None,
            &["Rebase", "Cancel"],
            cx,
        );
        let this = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            if prompt.await? == 0 {
                this.update_in(cx, |this, window, cx| {
                    this.rebase_stack_onto(plan, target.to_string(), window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn rebase_stack(&mut self, plan: RebasePlan, window: &mut Window, cx: &mut Context<Self>) {
        let trunk = self.trunk.to_string();
        if !self.settings.confirm_rebase_onto_trunk {
            self.rebase_stack_onto(plan, trunk, window, cx);
            return;
        }
        let prompt = window.prompt(
            PromptLevel::Warning,
            &format!("Rebase onto {trunk}?"),
            None,
            &["Rebase", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if prompt.await? == 0 {
                this.update_in(cx, |this, window, cx| {
                    this.rebase_stack_onto(plan, trunk, window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn rebase_stack_onto(
        &mut self,
        plan: RebasePlan,
        new_base: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_rebase(window, cx, "Failed to rebase", move |repository, cx| {
            repository.rebase_onto(new_base, plan.old_base.to_string(), plan.branch, cx)
        });
    }

    /// The plan for rebasing the stack that contains `HEAD`, if `HEAD` is on one.
    fn head_rebase_plan(&self, cx: &App) -> Option<RebasePlan> {
        let mut root = self.head?;
        if !self.parents.contains_key(&root) {
            return None;
        }
        while let Some(parent) = self.parents.get(&root).copied().flatten()
            && self.parents.contains_key(&parent)
        {
            root = parent;
        }
        self.rebase_plan(root, cx)
    }

    fn goto_time(
        &mut self,
        unix_timestamp: i64,
        rebase_onto_it: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let trunk = self.trunk.to_string();
        let plan = if rebase_onto_it {
            self.head_rebase_plan(cx)
        } else {
            None
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let sha = repository
                .update(cx, |repository, _| {
                    repository.commit_before_time(trunk.clone(), unix_timestamp)
                })
                .await??
                .with_context(|| format!("No commit on {trunk} at or before that time"))?;

            match plan {
                Some(plan) => {
                    this.update_in(cx, |this, window, cx| {
                        this.rebase_stack_onto(plan, sha, window, cx);
                    })?;
                }
                None if rebase_onto_it => {
                    anyhow::bail!("There is no draft stack at HEAD to rebase");
                }
                None => {
                    repository
                        .update(cx, |repository, _| repository.change_branch(sha))
                        .await??;
                }
            }
            Ok(())
        });
        self.run_logged("Failed to go to that time", task, window, cx);
    }

    fn continue_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_rebase(
            window,
            cx,
            "Failed to continue the rebase",
            |repository, cx| repository.rebase_continue(cx),
        );
    }

    fn abort_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_rebase(
            window,
            cx,
            "Failed to abort the rebase",
            |repository, cx| repository.rebase_abort(cx),
        );
    }

    fn render_rebase_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let unresolved = self.unresolved_conflict_count();
        let all_resolved = unresolved == 0;
        let colors = cx.theme().colors();

        h_flex()
            .flex_none()
            .w_full()
            .px_4()
            .py_2()
            .gap_3()
            .justify_between()
            .border_b_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .child(
                v_flex()
                    .child(
                        Label::new(if all_resolved {
                            "All Merge Conflicts Resolved"
                        } else {
                            "Unresolved Merge Conflicts"
                        })
                        .weight(gpui::FontWeight::BOLD),
                    )
                    .when(!all_resolved, |this| {
                        this.child(
                            Label::new(format!(
                                "{unresolved} conflicted files. Resolve conflicts to continue git rebase"
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("smartlog-abort-rebase", "Abort")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.abort_rebase(window, cx);
                            })),
                    )
                    .child(
                        Button::new("smartlog-continue-rebase", "Continue")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .disabled(!all_resolved)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.continue_rebase(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn open_commit(&self, sha: Oid, window: &mut Window, cx: &mut App) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            CommitView::open(
                sha.to_string(),
                repository.downgrade(),
                workspace,
                None,
                None,
                window,
                cx,
            );
        });
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
        let row_height = self.row_height();
        let half_row = row_height / 2.0;
        let mut gutter = div().relative().flex_none().w(width).h(row_height);

        for segment in self.segments.get(row_index).into_iter().flatten() {
            let line = match *segment {
                Segment::Vertical { column, color } => div()
                    .absolute()
                    .left(Self::lane_x(column) - LINE_WIDTH / 2.0)
                    .top_0()
                    .w(LINE_WIDTH)
                    .h(row_height)
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
        if matches!(
            row.kind,
            RowKind::Terminator | RowKind::Link | RowKind::Uncommitted(_)
        ) {
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
                RowKind::Uncommitted(_) | RowKind::Terminator | RowKind::Link => node
                    .border_color(colors.text_muted)
                    .border_dashed()
                    .bg(colors.background),
            })
            .when(row.is_head, |node| {
                node.border_color(cx.theme().status().info)
            });
        gutter.child(node).into_any_element()
    }

    fn you_are_here_pill(cx: &App) -> AnyElement {
        div()
            .flex_none()
            .px_2()
            .rounded_full()
            .bg(cx.theme().status().info_border)
            .child(
                Label::new("You are here")
                    .size(LabelSize::Small)
                    .color(Color::Default),
            )
            .into_any_element()
    }

    /// Runs `task`, records it in the command history, and reports a failure to the user.
    fn run_logged(
        &mut self,
        failure_title: &'static str,
        task: Task<anyhow::Result<()>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_operation_id;
        self.next_operation_id += 1;
        self.operations.push_front(OperationRecord {
            id,
            name: operation_name(failure_title).into(),
            state: OperationState::Running,
        });
        self.operations.truncate(MAX_OPERATION_HISTORY);
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let state = if result.is_ok() {
                OperationState::Succeeded
            } else {
                OperationState::Failed
            };
            this.update(cx, |this, cx| {
                if let Some(record) = this.operations.iter_mut().find(|record| record.id == id) {
                    record.state = state;
                }
                cx.notify();
            })
            .log_err();
            result
        })
        .detach_and_prompt_err(failure_title, window, cx, |_, _, _| None);
    }

    fn operation_icon(state: OperationState) -> AnyElement {
        match state {
            OperationState::Running => Icon::new(IconName::ArrowCircle)
                .size(IconSize::Small)
                .color(Color::Muted)
                .with_rotate_animation(2)
                .into_any_element(),
            OperationState::Succeeded => Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Success)
                .into_any_element(),
            OperationState::Failed => Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error)
                .into_any_element(),
        }
    }

    fn render_operation_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let latest = self.operations.front()?;
        let colors = cx.theme().colors();
        let expanded = self.history_expanded;

        Some(
            v_flex()
                .flex_none()
                .w_full()
                .border_t_1()
                .border_color(colors.border_variant)
                .when(expanded, |this| {
                    this.child(v_flex().px_4().py_1().gap_1().children(
                        self.operations.iter().skip(1).map(|record| {
                            h_flex()
                                .gap_2()
                                .child(Self::operation_icon(record.state))
                                .child(
                                    Label::new(record.name.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                        }),
                    ))
                })
                .child(
                    h_flex()
                        .id("smartlog-operation-strip")
                        .px_4()
                        .py_1p5()
                        .gap_2()
                        .justify_between()
                        .cursor_pointer()
                        .hover(|this| this.bg(colors.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.history_expanded = !this.history_expanded;
                            cx.notify();
                        }))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Self::operation_icon(latest.state))
                                .child(Label::new(latest.name.clone())),
                        )
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronUp
                            })
                            .size(IconSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .into_any_element(),
        )
    }

    fn selected_paths(&self) -> Vec<RepoPath> {
        self.uncommitted_files
            .iter()
            .filter(|entry| !self.deselected.contains(&entry.repo_path))
            .map(|entry| entry.repo_path.clone())
            .collect()
    }

    fn head_is_draft(&self) -> bool {
        self.layout
            .rows
            .iter()
            .any(|row| row.is_head && row.kind == RowKind::Draft)
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.deselected.clear();
        cx.notify();
    }

    fn deselect_all(&mut self, cx: &mut Context<Self>) {
        self.deselected = self
            .uncommitted_files
            .iter()
            .map(|entry| entry.repo_path.clone())
            .collect();
        cx.notify();
    }

    fn set_selected(&mut self, path: RepoPath, selected: bool, cx: &mut Context<Self>) {
        if selected {
            self.deselected.remove(&path);
        } else {
            self.deselected.insert(path);
        }
        cx.notify();
    }

    fn askpass_delegate(
        &self,
        operation: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AskPassDelegate {
        let workspace = self.workspace.clone();
        let window_handle = window.window_handle();
        AskPassDelegate::new_with_cancellation(
            &mut cx.to_async(),
            move |prompt, tx, cancellation, cx| {
                window_handle
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                AskPassModal::new(
                                    operation.into(),
                                    prompt.into(),
                                    tx,
                                    cancellation,
                                    window,
                                    cx,
                                )
                            });
                        })
                    })
                    .ok();
            },
        )
    }

    /// Commits (or amends with) exactly the checked files: they are staged and every unchecked
    /// file is unstaged first, because the git index is what a commit is made from.
    fn commit_changes(
        &mut self,
        amend: bool,
        message: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let repository = self.repository(cx)?;
        let selected = self.selected_paths();
        if selected.is_empty() {
            return None;
        }
        let unselected: Vec<RepoPath> = self.deselected.iter().cloned().collect();

        let used_quick_title = message.is_none() && !amend;
        let message: SharedString = match message {
            Some(message) => message,
            None if amend => repository.read(cx).head_commit.as_ref()?.message.clone(),
            None => {
                let title = self.title_editor.read(cx).text(cx);
                let title = title.trim();
                if title.is_empty() {
                    let now = OffsetDateTime::now_utc();
                    let time = time_format::format_local_timestamp(
                        OffsetDateTime::now_local().unwrap_or(now),
                        now,
                        TimestampFormat::EnhancedAbsolute,
                    );
                    format!("Temporary Commit at {time}").into()
                } else {
                    title.to_string().into()
                }
            }
        };

        let askpass = self.askpass_delegate(
            if amend {
                "git commit --amend"
            } else {
                "git commit"
            },
            window,
            cx,
        );

        Some(cx.spawn_in(window, async move |this, cx| {
            repository
                .update(cx, |repository, cx| repository.stage_entries(selected, cx))
                .await?;
            if !unselected.is_empty() {
                repository
                    .update(cx, |repository, cx| {
                        repository.unstage_entries(unselected, cx)
                    })
                    .await?;
            }
            repository
                .update(cx, |repository, cx| {
                    repository.commit(
                        message,
                        None,
                        CommitOptions {
                            amend,
                            ..Default::default()
                        },
                        askpass,
                        cx,
                    )
                })
                .await??;
            this.update_in(cx, |this, window, cx| {
                if used_quick_title {
                    this.title_editor
                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                }
            })?;
            anyhow::Ok(())
        }))
    }

    fn commit_selection(&mut self, amend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let failure_title = if amend {
            "Failed to amend commit"
        } else {
            "Failed to commit"
        };
        if let Some(task) = self.commit_changes(amend, None, window, cx) {
            self.run_logged(failure_title, task, window, cx);
        }
    }

    fn discard_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        self.revert_paths(paths, window, cx);
    }

    /// Reverts tracked files to `HEAD` and trashes files that did not exist there, after asking.
    fn revert_paths(&mut self, paths: Vec<RepoPath>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let entries: Vec<StatusEntry> = self
            .uncommitted_files
            .iter()
            .filter(|entry| paths.contains(&entry.repo_path))
            .cloned()
            .collect();
        if entries.is_empty() {
            return;
        }

        let staged: Vec<RepoPath> = entries
            .iter()
            .filter(|entry| entry.status.staging().has_staged())
            .map(|entry| entry.repo_path.clone())
            .collect();
        let (untracked, tracked): (Vec<StatusEntry>, Vec<StatusEntry>) = entries
            .into_iter()
            .partition(|entry| entry.status.is_created());
        let tracked_paths: Vec<RepoPath> = tracked
            .iter()
            .map(|entry| entry.repo_path.clone())
            .collect();
        let project_paths: Vec<_> = untracked
            .iter()
            .filter_map(|entry| {
                repository
                    .read(cx)
                    .repo_path_to_project_path(&entry.repo_path, cx)
            })
            .collect();

        let (message, confirm_label) = match (tracked.len(), untracked.len()) {
            (1, 0) => (
                "Discard changes to this file? This can't be undone.".to_string(),
                "Discard",
            ),
            (tracked_count, 0) => (
                format!("Discard changes to {tracked_count} files? This can't be undone."),
                "Discard",
            ),
            (0, untracked_count) => (format!("Trash {untracked_count} files?"), "Trash"),
            (tracked_count, untracked_count) => (
                format!(
                    "Discard changes to {tracked_count} files and trash {untracked_count} files?"
                ),
                "Discard and Trash",
            ),
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &[confirm_label, "Cancel"],
            cx,
        );
        let workspace = self.workspace.clone();

        let task = cx.spawn_in(window, async move |_, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }

            if !staged.is_empty() {
                repository
                    .update(cx, |repository, cx| repository.unstage_entries(staged, cx))
                    .await?;
            }

            if !tracked_paths.is_empty() {
                let buffer_tasks: Vec<_> = workspace.update(cx, |workspace, cx| {
                    workspace.project().update(cx, |project, cx| {
                        tracked_paths
                            .iter()
                            .filter_map(|path| {
                                let project_path =
                                    repository.read(cx).repo_path_to_project_path(path, cx)?;
                                Some(project.open_buffer(project_path, cx))
                            })
                            .collect()
                    })
                })?;
                let buffers = futures::future::join_all(buffer_tasks).await;

                repository
                    .update(cx, |repository, cx| {
                        repository.checkout_files("HEAD", tracked_paths, cx)
                    })
                    .await?;

                let reload_tasks: Vec<_> = cx.update(|_, cx| {
                    buffers
                        .iter()
                        .filter_map(|buffer| {
                            buffer.as_ref().ok()?.update(cx, |buffer, cx| {
                                buffer.is_dirty().then(|| buffer.reload(cx))
                            })
                        })
                        .collect()
                })?;
                futures::future::join_all(reload_tasks).await;
            }

            for project_path in project_paths {
                let task = workspace.update(cx, |workspace, cx| {
                    workspace
                        .project()
                        .update(cx, |project, cx| project.trash_file(project_path, cx))
                })?;
                if let Some(task) = task {
                    task.await?;
                }
            }
            Ok(())
        });
        self.run_logged("Failed to discard changes", task, window, cx);
    }

    fn uncommit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            "Are you sure you want to Uncommit?",
            Some("The commit is undone and its changes are kept in your working tree."),
            &["Uncommit", "Cancel"],
            cx,
        );
        let task = cx.spawn_in(window, async move |_, cx| {
            if prompt.await? != 0 {
                return anyhow::Ok(());
            }
            repository
                .update(cx, |repository, cx| {
                    repository.reset("HEAD^".to_string(), ResetMode::Soft, cx)
                })
                .await??;
            Ok(())
        });
        self.run_logged("Failed to uncommit", task, window, cx);
    }

    fn open_file_diff(&self, entry: &StatusEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let entry = GitStatusEntry {
            repo_path: entry.repo_path.clone(),
            status: entry.status,
            staging: entry.status.staging(),
            diff_stat: entry.diff_stat,
        };
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            SoloDiffView::open_or_focus(entry, repository, workspace, window, cx)
                .detach_and_prompt_err("Failed to open diff", window, cx, |_, _, _| None);
        });
    }

    fn open_file(&self, path: &RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_path) = self
            .repository(cx)
            .and_then(|repository| repository.read(cx).repo_path_to_project_path(path, cx))
        else {
            return;
        };
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            let task = workspace
                .update(cx, |workspace, cx| {
                    workspace.open_path(project_path, None, true, window, cx)
                })
                .log_err();
            if let Some(task) = task {
                task.detach_and_prompt_err("Failed to open file", window, cx, |_, _, _| None);
            }
        });
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let everything_selected = self.deselected.is_empty();
        let nothing_selected = self.selected_paths().is_empty();

        h_flex()
            .gap_1()
            .child(
                Button::new("smartlog-view-changes", "View Changes")
                    .start_icon(Icon::new(IconName::Diff).size(IconSize::Small))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .tooltip(Tooltip::text("View all uncommitted changes"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::project_diff::DiffHead), cx);
                    }),
            )
            .child(
                Button::new("smartlog-select-all", "Select All")
                    .start_icon(Icon::new(IconName::Check).size(IconSize::Small))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .disabled(everything_selected)
                    .on_click(cx.listener(|this, _, _, cx| this.select_all(cx))),
            )
            .child(
                Button::new("smartlog-deselect-all", "Deselect All")
                    .start_icon(Icon::new(IconName::Close).size(IconSize::Small))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .disabled(nothing_selected)
                    .on_click(cx.listener(|this, _, _, cx| this.deselect_all(cx))),
            )
            .child(
                Button::new("smartlog-absorb", "Absorb")
                    .start_icon(Icon::new(IconName::ArrowDown).size(IconSize::Small))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .disabled(self.head_stack_base().is_none())
                    .tooltip(Tooltip::text(
                        "Fold each change into the commit of this stack that last touched its lines",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_absorb(window, cx);
                    })),
            )
            .child(
                Button::new("smartlog-discard", "Discard")
                    .start_icon(Icon::new(IconName::Trash).size(IconSize::Small))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .disabled(nothing_selected)
                    .tooltip(Tooltip::text(
                        "Discard the selected changes, including untracked files. This can't be undone.",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.discard_selected(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_file(
        &self,
        file_index: usize,
        place: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.uncommitted_files.get(file_index) else {
            return h_flex().into_any_element();
        };
        let status = entry.status;
        let color = if status.is_conflicted() {
            Color::Conflict
        } else if status.is_created() {
            Color::Created
        } else if status.is_deleted() {
            Color::Deleted
        } else {
            Color::Modified
        };
        let is_selected = !self.deselected.contains(&entry.repo_path);
        let group = SharedString::from(format!("smartlog-{place}-file-{file_index}"));

        let checkbox_path = entry.repo_path.clone();
        let diff_entry = entry.clone();
        let button_entry = entry.clone();
        let file_path = entry.repo_path.clone();
        let revert_path = entry.repo_path.clone();

        h_flex()
            .group(group.clone())
            .gap_2()
            .min_w_0()
            .pr_2()
            .child(
                Checkbox::new(
                    ("smartlog-file-checkbox", file_index),
                    if is_selected {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                    this.set_selected(checkbox_path.clone(), *state == ToggleState::Selected, cx);
                })),
            )
            .child(
                h_flex()
                    .id(("smartlog-file-path", file_index))
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .cursor_pointer()
                    .child(crate::git_status_icon(status))
                    .child(
                        Label::new(entry.repo_path.as_unix_str().to_string())
                            .color(color)
                            .truncate(),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.settings.click_opens_diff {
                            this.open_file_diff(&diff_entry, window, cx);
                        } else {
                            this.open_file(&diff_entry.repo_path, window, cx);
                        }
                    })),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .invisible()
                    .group_hover(group, |style| style.visible())
                    .child(
                        IconButton::new(("smartlog-file-open", file_index), IconName::File)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open file"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_file(&file_path, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new(("smartlog-file-diff", file_index), IconName::Diff)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open diff view"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_file_diff(&button_entry, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new(("smartlog-file-revert", file_index), IconName::Undo)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Revert this file"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.revert_paths(vec![revert_path.clone()], window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let nothing_selected = self.selected_paths().is_empty();
        let colors = cx.theme().colors();

        h_flex()
            .gap_2()
            .pr_2()
            .when(self.head_is_draft(), |this| {
                this.child(
                    Button::new("smartlog-amend", "Amend")
                        .start_icon(Icon::new(IconName::ArrowDown).size(IconSize::Small))
                        .style(ButtonStyle::Filled)
                        .size(ButtonSize::Compact)
                        .disabled(nothing_selected)
                        .tooltip(Tooltip::text(
                            "Add the selected changes to the current commit, keeping its message",
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.commit_selection(true, window, cx);
                        })),
                )
            })
            .child(
                Button::new("smartlog-commit", "Commit")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Compact)
                    .disabled(nothing_selected)
                    .tooltip(Tooltip::text("Commit the selected changes"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.commit_selection(false, window, cx);
                    })),
            )
            .child(
                h_flex()
                    .h_6()
                    .w_48()
                    .px_1p5()
                    .border_1()
                    .border_color(colors.border_variant)
                    .rounded_md()
                    .bg(colors.toolbar_background)
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.commit_selection(false, window, cx);
                    }))
                    .child(self.title_editor.clone()),
            )
            .child(
                Button::new("smartlog-shelve", "Shelve")
                    .start_icon(Icon::new(IconName::Archive).size(IconSize::Small))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .disabled(nothing_selected)
                    .tooltip(Tooltip::text(
                        "Save the selected changes for later and remove them from the working tree",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.shelve_selection(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_uncommitted_row(
        &self,
        index: usize,
        row: &LayoutRow,
        part: UncommittedPart,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match part {
            UncommittedPart::Header => h_flex()
                .child(Self::you_are_here_pill(cx))
                .into_any_element(),
            UncommittedPart::Toolbar => self.render_toolbar(cx),
            UncommittedPart::File(file_index) => self.render_file(file_index, "main", cx),
            UncommittedPart::Actions => self.render_actions(cx),
        };

        h_flex()
            .id(("smartlog-row", index))
            .h(self.row_height())
            .w_full()
            .child(self.render_gutter(index, row, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_row(&self, index: usize, row: &LayoutRow, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        if let RowKind::Uncommitted(part) = row.kind {
            return self.render_uncommitted_row(index, row, part, cx);
        }
        let is_selected = row.sha.is_some_and(|sha| self.selection.contains(&sha));
        let sha = row.sha;
        let rebase_plan = if row.kind == RowKind::Draft && !self.rebase_in_progress(cx) {
            sha.and_then(|sha| self.rebase_plan(sha, cx))
        } else {
            None
        };
        let trunk_name = self.trunk.clone();
        let copy_text = sha.map(|sha| self.copy_hash_text(sha)).unwrap_or_default();
        let drag_payload = match (row.kind, sha) {
            (RowKind::Draft, Some(sha)) if !self.rebase_in_progress(cx) => {
                self.commits.get(&sha).map(|commit| DraggedCommit {
                    sha,
                    label: commit.subject.clone(),
                })
            }
            _ => None,
        };
        let is_hidden = sha.is_some_and(|sha| self.hidden_closure.contains(&sha));
        let filter = self.filter_text(cx);
        let matches_filter = sha.is_none_or(|sha| self.matches_filter(sha, &filter));
        let is_hidden_root = sha.is_some_and(|sha| self.hidden.contains(&sha));
        let can_hide = row.kind == RowKind::Draft
            && sha.is_some_and(|sha| {
                !self.protected_from_hiding.contains(&sha) && !self.hidden_closure.contains(&sha)
            });

        let summary = match (row.kind, sha) {
            (RowKind::Terminator | RowKind::Link, _) => h_flex(),
            (_, None) => h_flex(),
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
                        self.chip_names(sha, row.kind, cx)
                            .into_iter()
                            .map(|name| Chip::new(name).label_size(LabelSize::Small)),
                    )
                    .when_some(self.worktree_checked_out_at(sha, cx), |this, worktree| {
                        this.child(
                            div()
                                .id(("smartlog-worktree", index))
                                .child(
                                    Chip::new(format!("In {worktree}"))
                                        .icon(IconName::FolderOpen)
                                        .label_size(LabelSize::Small),
                                )
                                .tooltip(Tooltip::text(
                                    "This commit is checked out in another worktree",
                                )),
                        )
                    })
                    .when(row.is_head && self.uncommitted_files.is_empty(), |this| {
                        this.child(Self::you_are_here_pill(cx))
                    })
            }
        };

        h_flex()
            .id(("smartlog-row", index))
            .when(is_hidden, |this| this.opacity(0.5))
            .when_some(drag_payload, |this, payload| {
                this.on_drag(payload, |dragged, _, _, cx| {
                    let label = dragged.label.clone();
                    cx.new(|_| DraggedCommitPreview { label })
                })
            })
            .when_some(sha, |this, target| {
                this.drag_over::<DraggedCommit>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(cx.listener(
                    move |this, dragged: &DraggedCommit, window, cx| {
                        this.rebase_dragged(dragged.sha, target, window, cx);
                    },
                ))
            })
            .when(!matches_filter, |this| this.opacity(0.3))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_context_menu(event.position, index, window, cx);
                }),
            )
            .h(self.row_height())
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
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.uncommit(window, cx);
                                    })),
                            )
                        })
                        .when(can_hide, |this| {
                            this.child(
                                IconButton::new(("smartlog-hide", index), IconName::EyeOff)
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Hide commit and descendants"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.hide_commit(sha, window, cx);
                                    })),
                            )
                        })
                        .when(is_hidden_root, |this| {
                            this.child(
                                IconButton::new(("smartlog-unhide", index), IconName::Eye)
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Show commit again"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.unhide_commit(sha, window, cx);
                                    })),
                            )
                        })
                        .when_some(rebase_plan, |this, plan| {
                            this.child(
                                IconButton::new(("smartlog-rebase", index), IconName::GitBranch)
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text(format!("Rebase onto {}", trunk_name)))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.rebase_stack(plan.clone(), window, cx);
                                    })),
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
                                        copy_text.clone(),
                                    ));
                                }),
                        ),
                )
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if let Some(sha) = sha {
                    this.click_commit(sha, event.modifiers(), window, cx);
                }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .when_some(self.last_fetch, |this, last_fetch| {
                let elapsed = SystemTime::now()
                    .duration_since(last_fetch)
                    .unwrap_or_default();
                this.child(
                    div()
                        .id("smartlog-last-fetch")
                        .child(
                            Label::new(short_duration(elapsed))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text("Time since the last fetch")),
                )
            })
            .child(
                h_flex()
                    .h_7()
                    .flex_1()
                    .max_w(rems(20.))
                    .px_2()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .bg(cx.theme().colors().toolbar_background)
                    .child(self.filter_editor.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        IconButton::new("smartlog-settings", IconName::Settings)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Settings"))
                            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                                this.deploy_settings_menu(event.position(), window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("smartlog-bulk-actions", IconName::PlayFilled)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Bulk actions"))
                            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                                this.deploy_bulk_actions_menu(event.position(), window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("smartlog-download", IconName::Download)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Download commits"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let smartlog = cx.weak_entity();
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        workspace.toggle_modal(window, cx, |window, cx| {
                                            download::DownloadModal::new(smartlog, window, cx)
                                        });
                                    })
                                    .log_err();
                            })),
                    )
                    .child(
                        IconButton::new("smartlog-bookmarks", IconName::Bookmark)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Manage bookmarks"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let Some(repository) = this.repository(cx) else {
                                    return;
                                };
                                let smartlog = cx.weak_entity();
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        workspace.toggle_modal(window, cx, |_, cx| {
                                            bookmarks::BookmarksModal::new(smartlog, repository, cx)
                                        });
                                    })
                                    .log_err();
                            })),
                    )
                    .child(
                        IconButton::new("smartlog-goto-time", IconName::Clock)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Go to a point in time"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let smartlog = cx.weak_entity();
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        workspace.toggle_modal(window, cx, |window, cx| {
                                            GotoTimeModal::new(smartlog, window, cx)
                                        });
                                    })
                                    .log_err();
                            })),
                    )
                    .when(!self.hidden.is_empty(), |this| {
                        this.child(
                            Button::new(
                                "smartlog-toggle-hidden",
                                if self.show_hidden {
                                    format!("Hide {} hidden", self.hidden.len())
                                } else {
                                    format!("Show {} hidden", self.hidden.len())
                                },
                            )
                            .start_icon(
                                Icon::new(if self.show_hidden {
                                    IconName::EyeOff
                                } else {
                                    IconName::Eye
                                })
                                .size(IconSize::Small),
                            )
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.show_hidden = !this.show_hidden;
                                    this.refresh(window, cx);
                                },
                            )),
                        )
                    })
                    .child(
                        IconButton::new("smartlog-refresh", IconName::ArrowCircle)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh(window, cx);
                            })),
                    ),
            );

        let filter = self.filter_text(cx);
        let filter_matches_anything = filter.is_empty()
            || self
                .layout
                .rows
                .iter()
                .filter_map(|row| row.sha)
                .any(|sha| self.matches_filter(sha, &filter));
        let sidebar_open = !self.sidebar.collapsed;
        let list_fraction = self.sidebar.split.read(cx).visible_left_ratio();

        let list = v_flex()
            .id("smartlog-rows")
            .track_scroll(&self.list_scroll_handle)
            .h_full()
            .min_w_0()
            .overflow_y_scroll()
            .pt(LIST_VERTICAL_PADDING)
            .pb(LIST_VERTICAL_PADDING)
            .map(|list| {
                if sidebar_open {
                    list.flex_basis(DefiniteLength::Fraction(list_fraction))
                } else {
                    list.flex_1()
                }
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error).m_2())
            })
            .when(!filter_matches_anything, |this| {
                this.child(
                    v_flex()
                        .m_2()
                        .gap_2()
                        .child(Label::new("No commits match your filter").color(Color::Muted))
                        .child(
                            Button::new("smartlog-clear-filter", "Clear filter")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.filter_editor
                                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                                })),
                        ),
                )
            })
            .when(
                rows.is_empty() && self.error.is_none() && self.head.is_some(),
                |this| {
                    this.child(
                        Label::new(format!(
                            "No draft commits. Everything is already on {}.",
                            self.trunk
                        ))
                        .color(Color::Muted)
                        .m_2(),
                    )
                },
            )
            .when(
                rows.is_empty() && self.error.is_none() && self.head.is_none(),
                |this| {
                    this.child(
                        v_flex()
                            .m_4()
                            .gap_2()
                            .items_start()
                            .child(
                                Label::new("This repository has no commits yet.")
                                    .color(Color::Muted),
                            )
                            .child(
                                Button::new(
                                    "smartlog-create-initial-commit",
                                    "Create empty initial commit",
                                )
                                .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.create_initial_commit(window, cx);
                                    },
                                )),
                            ),
                    )
                },
            )
            .children(rows)
            .children(self.render_shelved_changes(cx));

        let body = h_flex()
            .id("smartlog-body")
            .flex_1()
            .min_h_0()
            .w_full()
            .on_drag_move::<DraggedSplitHandle>(cx.listener(|this, event, window, cx| {
                this.sidebar.split.update(cx, |state, cx| {
                    state.on_drag_move(event, window, cx);
                });
            }))
            .on_drop::<DraggedSplitHandle>(cx.listener(|this, _event, _window, cx| {
                this.sidebar.split.update(cx, |state, _| {
                    state.commit_ratio();
                });
                cx.emit(ItemEvent::Edit);
            }))
            .child(list)
            .when(sidebar_open, |this| {
                this.child(self.render_sidebar_split_handle(cx))
                    .child(self.render_sidebar(window, cx))
            })
            .when(!sidebar_open, |this| {
                this.child(self.render_collapsed_sidebar(cx))
            });

        v_flex()
            .id("smartlog")
            .key_context("Smartlog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_previous_commit))
            .on_action(cx.listener(Self::select_next_commit))
            .on_action(cx.listener(Self::extend_selection_up))
            .on_action(cx.listener(Self::extend_selection_down))
            .on_action(cx.listener(Self::open_details))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(Self::select_all_commits))
            .on_action(cx.listener(Self::hide_selected_commits))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::pull))
            .on_action(cx.listener(Self::rebase_onto_trunk))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(header)
            .when(self.rebase_in_progress(cx), |this| {
                this.child(self.render_rebase_banner(cx))
            })
            .child(body)
            .children(self.render_operation_strip(cx))
            .children(self.context_menu.as_ref().map(|context_menu| {
                deferred(
                    anchored()
                        .position(context_menu.position)
                        .anchor(Anchor::TopLeft)
                        .child(context_menu.menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl workspace::SerializableItem for Smartlog {
    fn serialized_item_kind() -> &'static str {
        "Smartlog"
    }

    fn cleanup(
        workspace_id: workspace::WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "smartlogs",
            &persistence::SmartlogsDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let database = persistence::SmartlogsDb::global(cx);
        let state = match database.get_smartlog(item_id, workspace_id) {
            Ok(Some(state)) => state,
            Ok(None) => return Task::ready(Err(anyhow::anyhow!("No Smartlog to deserialize"))),
            Err(error) => return Task::ready(Err(error)),
        };
        let Some(trunk) = state.trunk.clone() else {
            return Task::ready(Err(anyhow::anyhow!(
                "The saved Smartlog has no trunk branch"
            )));
        };

        let window_handle = window.window_handle();
        let project = project.read(cx);
        let git_store = project.git_store().clone();
        let wait = project.wait_for_initial_scan(cx);

        cx.spawn(async move |cx| {
            wait.await;

            cx.update_window(window_handle, |_, window, cx| {
                let repository_id = git_store.read(cx).repositories().iter().find_map(
                    |(&repository_id, repository)| {
                        (repository
                            .read(cx)
                            .snapshot()
                            .work_directory_abs_path
                            .as_ref()
                            == state.repo_working_path.as_path())
                        .then_some(repository_id)
                    },
                );
                let Some(repository_id) = repository_id else {
                    anyhow::bail!(
                        "Repository not found for path: {:?}",
                        state.repo_working_path
                    );
                };

                let smartlog = cx.new(|cx| {
                    Smartlog::new(
                        repository_id,
                        git_store,
                        workspace,
                        trunk.into(),
                        window,
                        cx,
                    )
                });
                smartlog.update(cx, |smartlog, cx| {
                    smartlog.restore(&state, window, cx);
                });
                Ok(smartlog)
            })?
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let repository = self.repository(cx)?;
        let repo_working_path = repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_string_lossy()
            .to_string();
        let trunk = self.trunk.to_string();
        let selected_sha = self
            .selection_cursor
            .filter(|sha| self.selection.contains(sha))
            .map(|sha| sha.to_string());
        let sidebar_collapsed = self.sidebar.collapsed;
        let sidebar_list_permille =
            persistence::ratio_to_permille(self.sidebar.split.read(cx).visible_left_ratio());
        let show_hidden = self.show_hidden;
        let filter = Some(self.filter_editor.read(cx).text(cx)).filter(|text| !text.is_empty());
        let settings = self.settings.to_json();

        let database = persistence::SmartlogsDb::global(cx);
        Some(cx.background_spawn(async move {
            database
                .save_smartlog(
                    item_id,
                    workspace_id,
                    repo_working_path,
                    trunk,
                    selected_sha,
                    sidebar_collapsed,
                    sidebar_list_permille,
                    show_hidden,
                    filter,
                    settings,
                )
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab | ItemEvent::Edit)
    }
}

impl Smartlog {
    fn restore(
        &mut self,
        state: &persistence::SerializedSmartlog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.collapsed = state.sidebar_collapsed.unwrap_or(false);
        if let Some(permille) = state.sidebar_list_permille {
            let ratio = persistence::permille_to_ratio(permille);
            self.sidebar.split = cx.new(|_| SplitState::with_left_ratio(ratio));
        }
        self.settings = state.settings.clone();
        self.show_hidden = state.show_hidden.unwrap_or(false);
        if let Some(filter) = &state.filter {
            self.filter_editor.update(cx, |editor, cx| {
                editor.set_text(filter.as_str(), window, cx)
            });
        }
        self.pending_selection = state
            .selected_sha
            .as_deref()
            .and_then(|sha| sha.parse::<Oid>().ok());
        self.refresh(window, cx);
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
        let layout = build_layout(&[(b, Some(a)), (a, Some(trunk))], Some(b), 0);

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
        let layout = build_layout(&[(c, Some(a)), (b, Some(a)), (a, Some(trunk))], None, 0);

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
        let layout = build_layout(&[], Some(trunk), 2);

        let kinds: Vec<RowKind> = layout.rows.iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RowKind::Uncommitted(UncommittedPart::Header),
                RowKind::Uncommitted(UncommittedPart::Toolbar),
                RowKind::Uncommitted(UncommittedPart::File(0)),
                RowKind::Uncommitted(UncommittedPart::File(1)),
                RowKind::Uncommitted(UncommittedPart::Actions),
                RowKind::Public,
                RowKind::Terminator,
            ]
        );
        assert!(layout.rows[5].is_head);
        assert_eq!(
            layout.edges,
            vec![LayoutEdge {
                child_row: 0,
                parent_row: 5
            }]
        );
    }

    #[test]
    fn separate_trunk_commits_are_joined_by_a_trunk_edge() {
        let (trunk_a, trunk_b, a, b) = (oid(1), oid(2), oid(3), oid(4));
        let layout = build_layout(&[(a, Some(trunk_a)), (b, Some(trunk_b))], None, 0);

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
        let layout = build_layout(&[(a, Some(trunk))], None, 0);
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

    fn parents_of(entries: &[(Oid, Option<Oid>)]) -> HashMap<Oid, Option<Oid>> {
        entries.iter().copied().collect()
    }

    #[test]
    fn a_stack_containing_head_is_rebased_in_place() {
        let (trunk, a, b) = (oid(1), oid(2), oid(3));
        let parents = parents_of(&[(a, Some(trunk)), (b, Some(a))]);

        assert_eq!(
            plan_rebase(&parents, Some(b), a, |_| None),
            Some(RebasePlan {
                old_base: trunk,
                branch: None
            })
        );
    }

    #[test]
    fn a_stack_elsewhere_needs_one_tip_with_a_local_branch() {
        let (trunk, a, b, head) = (oid(1), oid(2), oid(3), oid(9));
        let parents = parents_of(&[(a, Some(trunk)), (b, Some(a)), (head, Some(trunk))]);

        assert_eq!(
            plan_rebase(&parents, Some(head), a, |tip| (tip == b)
                .then(|| "feature".to_string())),
            Some(RebasePlan {
                old_base: trunk,
                branch: Some("feature".to_string())
            })
        );
        assert_eq!(plan_rebase(&parents, Some(head), a, |_| None), None);
    }

    #[test]
    fn a_forked_stack_without_head_cannot_be_rebased() {
        let (trunk, a, b, c, head) = (oid(1), oid(2), oid(3), oid(4), oid(9));
        let parents = parents_of(&[
            (a, Some(trunk)),
            (b, Some(a)),
            (c, Some(a)),
            (head, Some(trunk)),
        ]);

        assert_eq!(
            plan_rebase(&parents, Some(head), a, |_| Some("branch".to_string())),
            None
        );
    }

    #[test]
    fn only_the_root_of_a_stack_can_be_rebased() {
        let (trunk, a, b) = (oid(1), oid(2), oid(3));
        let parents = parents_of(&[(a, Some(trunk)), (b, Some(a))]);

        assert_eq!(plan_rebase(&parents, Some(b), b, |_| None), None);
    }

    fn selectable(entries: &[(u8, bool)]) -> Vec<SelectableRow> {
        entries
            .iter()
            .map(|(byte, is_public)| SelectableRow {
                sha: oid(*byte),
                is_public: *is_public,
            })
            .collect()
    }

    fn set(bytes: &[u8]) -> HashSet<Oid> {
        bytes.iter().map(|byte| oid(*byte)).collect()
    }

    #[test]
    fn a_plain_click_selects_one_commit_and_a_second_click_clears_it() {
        let rows = selectable(&[(1, false), (2, false), (3, true)]);

        let (selection, anchor) =
            apply_click(&rows, &HashSet::default(), None, oid(2), false, false);
        assert_eq!((selection.clone(), anchor), (set(&[2]), Some(oid(2))));

        let (selection, anchor) = apply_click(&rows, &selection, anchor, oid(2), false, false);
        assert_eq!((selection, anchor), (HashSet::default(), None));
    }

    #[test]
    fn toggling_adds_and_removes_commits() {
        let rows = selectable(&[(1, false), (2, false), (3, false)]);

        let (selection, anchor) = apply_click(&rows, &set(&[1]), Some(oid(1)), oid(3), false, true);
        assert_eq!(selection, set(&[1, 3]));

        let (selection, _) = apply_click(&rows, &selection, anchor, oid(1), false, true);
        assert_eq!(selection, set(&[3]));
    }

    #[test]
    fn shift_click_selects_the_range_from_the_anchor_without_trunk_commits() {
        let rows = selectable(&[(1, false), (2, true), (3, false), (4, false)]);

        let (selection, anchor) = apply_click(&rows, &set(&[1]), Some(oid(1)), oid(4), true, false);
        assert_eq!(selection, set(&[1, 3, 4]));
        assert_eq!(anchor, Some(oid(1)));
    }

    #[test]
    fn clicking_a_trunk_commit_selects_only_it() {
        let rows = selectable(&[(1, false), (2, true)]);

        let (selection, _) = apply_click(&rows, &set(&[1]), Some(oid(1)), oid(2), false, true);
        assert_eq!(selection, set(&[2]));
    }

    #[test]
    fn hiding_a_commit_hides_everything_built_on_it() {
        let (trunk, a, b, c, other) = (oid(1), oid(2), oid(3), oid(4), oid(5));
        let parents = parents_of(&[
            (a, Some(trunk)),
            (b, Some(a)),
            (c, Some(b)),
            (other, Some(trunk)),
        ]);

        let closure = hidden_closure(&parents, &set(&[3]), &HashSet::default());
        assert_eq!(closure, set(&[3, 4]));
        assert!(!closure.contains(&other));
    }

    #[test]
    fn the_checked_out_lineage_is_never_hidden() {
        let (trunk, a, b, c) = (oid(1), oid(2), oid(3), oid(4));
        let parents = parents_of(&[(a, Some(trunk)), (b, Some(a)), (c, Some(b))]);

        let protected = lineage_of(&parents, Some(b));
        assert_eq!(protected, set(&[3, 2, 1]));

        assert_eq!(hidden_closure(&parents, &set(&[2]), &protected), set(&[]));
        assert_eq!(hidden_closure(&parents, &set(&[4]), &protected), set(&[4]));
    }

    #[test]
    fn a_contiguous_selection_folds_oldest_first() {
        let (trunk, a, b, c, d) = (oid(1), oid(2), oid(3), oid(4), oid(5));
        let parents = parents_of(&[(a, Some(trunk)), (b, Some(a)), (c, Some(b)), (d, Some(c))]);

        assert_eq!(fold_chain(&parents, &set(&[4, 3, 2])), Some(vec![a, b, c]));
        assert_eq!(fold_chain(&parents, &set(&[3, 4])), Some(vec![b, c]));
    }

    #[test]
    fn a_broken_or_forked_selection_cannot_be_folded() {
        let (trunk, a, b, c, d) = (oid(1), oid(2), oid(3), oid(4), oid(5));
        let parents = parents_of(&[(a, Some(trunk)), (b, Some(a)), (c, Some(b)), (d, Some(a))]);

        assert_eq!(fold_chain(&parents, &set(&[2, 4])), None);
        assert_eq!(fold_chain(&parents, &set(&[3, 5])), None);
        assert_eq!(fold_chain(&parents, &set(&[3])), None);
        assert_eq!(fold_chain(&parents, &set(&[2, 3, 5])), None);
    }

    #[test]
    fn the_filter_matches_subject_author_hash_and_refs() {
        let sha = oid(0xab);
        let refs = [SharedString::from("HEAD -> Feature/Login")];
        let matches = |filter: &str| {
            commit_matches_filter(
                filter,
                sha,
                Some("Fix the Parser"),
                Some(("Ada Lovelace", "ada@example.com")),
                &refs,
            )
        };

        assert!(matches(""));
        assert!(matches("parser"));
        assert!(matches("lovelace"));
        assert!(matches("ada@example"));
        assert!(matches("abab"));
        assert!(matches("feature/login"));
        assert!(!matches("unrelated"));
        assert!(!commit_matches_filter("parser", sha, None, None, &[]));
    }

    #[test]
    fn an_unnamed_shelf_still_has_a_label() {
        assert_eq!(shelf_entry_label("  "), "Shelved changes");
        assert_eq!(
            shelf_entry_label("WIP on main: abc1234 Fix it"),
            "WIP on main: abc1234 Fix it"
        );
    }

    #[test]
    fn goto_time_accepts_hours_and_local_dates() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let utc = time::UtcOffset::UTC;
        let plus_two = time::UtcOffset::from_hms(2, 0, 0).unwrap();

        assert_eq!(parse_goto_time("2", now, utc), Some(1_700_000_000 - 7200));
        assert_eq!(parse_goto_time("0.5", now, utc), Some(1_700_000_000 - 1800));
        assert_eq!(
            parse_goto_time("2024-01-02 03:04", now, utc),
            Some(1_704_164_640)
        );
        assert_eq!(
            parse_goto_time("2024-01-02T03:04", now, utc),
            Some(1_704_164_640)
        );
        assert_eq!(
            parse_goto_time("2024-01-02 03:04", now, plus_two),
            Some(1_704_164_640 - 7200)
        );
        assert_eq!(parse_goto_time("2024-01-02", now, utc), Some(1_704_153_600));
        assert_eq!(parse_goto_time("-3", now, utc), None);
        assert_eq!(parse_goto_time("yesterday", now, utc), None);
        assert_eq!(parse_goto_time("", now, utc), None);
    }

    #[test]
    fn operation_names_come_from_the_failure_title() {
        assert_eq!(operation_name("Failed to rebase"), "Rebase");
        assert_eq!(operation_name("Failed to go to commit"), "Go to commit");
        assert_eq!(operation_name("Uncommit"), "Uncommit");
        assert_eq!(operation_name(""), "");
    }

    #[test]
    fn the_cursor_steps_through_the_list_and_stops_at_the_ends() {
        let rows = selectable(&[(1, false), (2, false), (3, true)]);

        assert_eq!(step_cursor(&rows, Some(oid(1)), None, true), Some(oid(2)));
        assert_eq!(step_cursor(&rows, Some(oid(2)), None, false), Some(oid(1)));
        assert_eq!(step_cursor(&rows, Some(oid(3)), None, true), Some(oid(3)));
        assert_eq!(step_cursor(&rows, Some(oid(1)), None, false), Some(oid(1)));
    }

    #[test]
    fn without_a_cursor_stepping_starts_at_head_or_an_end() {
        let rows = selectable(&[(1, false), (2, false), (3, true)]);

        assert_eq!(step_cursor(&rows, None, Some(oid(2)), true), Some(oid(3)));
        assert_eq!(step_cursor(&rows, None, None, true), Some(oid(1)));
        assert_eq!(step_cursor(&rows, None, None, false), Some(oid(3)));
        assert_eq!(step_cursor(&[], None, None, true), None);
    }

    #[test]
    fn durations_are_shortened_like_the_pull_badge() {
        assert_eq!(short_duration(Duration::from_secs(5)), "now");
        assert_eq!(short_duration(Duration::from_secs(60)), "1m");
        assert_eq!(short_duration(Duration::from_secs(59 * 60)), "59m");
        assert_eq!(short_duration(Duration::from_secs(2 * 3600 + 5)), "2h");
        assert_eq!(short_duration(Duration::from_secs(3 * 86_400)), "3d");
    }

    #[test]
    fn dragging_a_commit_moves_it_and_what_is_built_on_it() {
        let (trunk, a, b, c, other) = (oid(1), oid(2), oid(3), oid(4), oid(5));
        let parents = parents_of(&[
            (a, Some(trunk)),
            (b, Some(a)),
            (c, Some(b)),
            (other, Some(trunk)),
        ]);

        let plan = plan_drag_rebase(&parents, Some(c), b, other, |_| None).unwrap();
        assert_eq!(
            plan,
            RebasePlan {
                old_base: a,
                branch: None
            }
        );
    }

    #[test]
    fn a_commit_cannot_be_dropped_onto_itself_its_descendants_or_its_parent() {
        let (trunk, a, b, c) = (oid(1), oid(2), oid(3), oid(4));
        let parents = parents_of(&[(a, Some(trunk)), (b, Some(a)), (c, Some(b))]);

        assert_eq!(plan_drag_rebase(&parents, Some(c), b, b, |_| None), None);
        assert_eq!(plan_drag_rebase(&parents, Some(c), b, c, |_| None), None);
        assert_eq!(plan_drag_rebase(&parents, Some(c), b, a, |_| None), None);
        assert!(plan_drag_rebase(&parents, Some(c), b, trunk, |_| None).is_some());
    }

    #[test]
    fn a_refused_drop_says_why() {
        let (trunk, a, b, c, d) = (oid(1), oid(2), oid(3), oid(4), oid(5));
        let parents = parents_of(&[(a, Some(trunk)), (b, Some(a)), (c, Some(b)), (d, Some(b))]);

        assert!(drag_rebase_refusal(&parents, trunk, a).contains("draft"));
        assert!(drag_rebase_refusal(&parents, b, c).contains("itself"));
        assert!(drag_rebase_refusal(&parents, c, b).contains("already based"));
        assert!(drag_rebase_refusal(&parents, b, trunk).contains("Several branches"));
    }

    #[test]
    fn conflict_output_is_recognised() {
        assert!(stopped_on_conflicts(&anyhow::anyhow!(
            "CONFLICT (content): Merge conflict in a.txt\nerror: could not apply abc"
        )));
        assert!(!stopped_on_conflicts(&anyhow::anyhow!(
            "fatal: not a git repository"
        )));
    }

    #[gpui::test]
    async fn smartlog_shows_uncommitted_changes(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use git::status::{FileStatus, StatusCode, TrackedStatus};
        use project::Project;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "a.txt": "changed" }),
        )
        .await;
        let head = oid(7);
        fs.set_head_for_repo(
            Path::new("/project/.git"),
            &[("a.txt", "original".to_string())],
            head.to_string(),
        );
        fs.set_status_for_repo(
            Path::new("/project/.git"),
            &[(
                "a.txt",
                FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Unmodified,
                    worktree_status: StatusCode::Modified,
                }),
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let smartlog = cx.new_window_entity(|window, cx| {
            Smartlog::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace,
                "main".into(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        smartlog.read_with(&*cx, |smartlog, cx| {
            assert_eq!(
                repository
                    .read(cx)
                    .head_commit
                    .as_ref()
                    .map(|commit| commit.sha.to_string()),
                Some(head.to_string())
            );
            assert_eq!(smartlog.uncommitted_files.len(), 1);
            assert!(
                smartlog
                    .layout
                    .rows
                    .iter()
                    .any(|row| matches!(row.kind, RowKind::Uncommitted(_)))
            );
        });
    }

    #[gpui::test]
    async fn smartlog_picks_up_changes_made_after_opening(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use git::status::{FileStatus, StatusCode, TrackedStatus};
        use project::Project;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "a.txt": "changed" }),
        )
        .await;
        let head = oid(7);
        fs.set_head_for_repo(
            Path::new("/project/.git"),
            &[("a.txt", "changed".to_string())],
            head.to_string(),
        );
        fs.set_head_and_index_for_repo(
            Path::new("/project/.git"),
            &[("a.txt", "changed".to_string())],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let smartlog = cx.new_window_entity(|window, cx| {
            Smartlog::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace,
                "main".into(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        smartlog.read_with(&*cx, |smartlog, _| {
            assert!(smartlog.uncommitted_files.is_empty());
        });

        fs.set_status_for_repo(
            Path::new("/project/.git"),
            &[(
                "a.txt",
                FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Unmodified,
                    worktree_status: StatusCode::Modified,
                }),
            )],
        );

        cx.run_until_parked();

        smartlog.read_with(&*cx, |smartlog, cx| {
            assert_eq!(
                repository
                    .read(cx)
                    .head_commit
                    .as_ref()
                    .map(|commit| commit.sha.to_string()),
                Some(head.to_string())
            );
            assert_eq!(smartlog.uncommitted_files.len(), 1);
            assert!(
                smartlog
                    .layout
                    .rows
                    .iter()
                    .any(|row| matches!(row.kind, RowKind::Uncommitted(_)))
            );
        });

        smartlog.update(cx, |smartlog, cx| {
            let path = smartlog.uncommitted_files[0].repo_path.clone();
            assert_eq!(smartlog.selected_paths(), vec![path.clone()]);

            smartlog.deselect_all(cx);
            assert!(smartlog.selected_paths().is_empty());

            smartlog.set_selected(path.clone(), true, cx);
            assert_eq!(smartlog.selected_paths(), vec![path]);

            smartlog.deselect_all(cx);
            smartlog.select_all(cx);
            assert!(smartlog.deselected.is_empty());
        });
    }

    #[gpui::test]
    async fn opening_a_diff_does_not_update_the_smartlog_while_it_is_updating(
        cx: &mut gpui::TestAppContext,
    ) {
        use fs::FakeFs;
        use project::Project;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "a.txt": "changed" }),
        )
        .await;
        fs.set_head_for_repo(
            Path::new("/project/.git"),
            &[("a.txt", "original".to_string())],
            oid(7).to_string(),
        );
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let smartlog = cx.new_window_entity(|window, cx| {
            Smartlog::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                "main".into(),
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(smartlog.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        let entry = smartlog.read_with(&*cx, |smartlog, _| smartlog.uncommitted_files[0].clone());
        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.open_file_diff(&entry, window, cx);
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&smartlog, true, true, window, cx);
        });
        cx.run_until_parked();

        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.open_file_diff(&entry, window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(&*cx, |workspace, cx| {
            assert!(workspace.active_item_as::<SoloDiffView>(cx).is_some());
        });
    }

    #[gpui::test]
    async fn operations_are_recorded_with_their_outcome(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use project::Project;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(Path::new("/project"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        fs.set_head_and_index_for_repo(Path::new("/project/.git"), &[("a.txt", "a".to_string())]);
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let smartlog = cx.new_window_entity(|window, cx| {
            Smartlog::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace,
                "main".into(),
                window,
                cx,
            )
        });

        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.run_logged("Failed to do a thing", Task::ready(Ok(())), window, cx);
            smartlog.run_logged(
                "Failed to do another thing",
                Task::ready(Err(anyhow::anyhow!("nope"))),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        smartlog.read_with(&*cx, |smartlog, _| {
            let recorded: Vec<(String, OperationState)> = smartlog
                .operations
                .iter()
                .map(|record| (record.name.to_string(), record.state))
                .collect();
            assert_eq!(
                recorded,
                vec![
                    ("Do another thing".to_string(), OperationState::Failed),
                    ("Do a thing".to_string(), OperationState::Succeeded),
                ]
            );
        });

        smartlog.update_in(cx, |smartlog, window, cx| {
            for _ in 0..MAX_OPERATION_HISTORY + 5 {
                smartlog.run_logged("Failed to repeat", Task::ready(Ok(())), window, cx);
            }
        });
        cx.run_until_parked();
        smartlog.read_with(&*cx, |smartlog, _| {
            assert_eq!(smartlog.operations.len(), MAX_OPERATION_HISTORY);
        });
    }

    #[gpui::test]
    async fn the_smartlog_can_be_created_while_the_workspace_is_updating(
        cx: &mut gpui::TestAppContext,
    ) {
        use fs::FakeFs;
        use project::Project;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(Path::new("/project"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        fs.set_head_and_index_for_repo(Path::new("/project/.git"), &[("a.txt", "a".to_string())]);
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository_id = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("repository")
                .read(cx)
                .id
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());

        // Opening the Smartlog creates it from inside an update of the workspace, so nothing
        // done while constructing it may read the workspace.
        workspace.update_in(cx, |workspace, window, cx| {
            let git_store = workspace.project().read(cx).git_store().clone();
            let workspace_handle = workspace.weak_handle();
            let smartlog = cx.new(|cx| {
                Smartlog::new(
                    repository_id,
                    git_store,
                    workspace_handle,
                    "main".into(),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(smartlog), None, true, window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(&*cx, |workspace, cx| {
            assert!(workspace.active_item_as::<Smartlog>(cx).is_some());
        });
    }

    #[gpui::test]
    async fn the_smartlog_renders_every_state_without_panicking(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use git::repository::CommitData;
        use gpui::{VisualContext as _, point};
        use project::Project;
        use serde_json::json;
        use smallvec::smallvec;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let (base, first, second, third) = (oid(10), oid(11), oid(12), oid(13));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "a.txt": "changed", "new.txt": "new" }),
        )
        .await;
        fs.set_head_for_repo(
            Path::new("/project/.git"),
            &[("a.txt", "original".to_string())],
            third.to_string(),
        );
        let graph_commit = |sha: Oid, parent: Oid, refs: &[&str]| {
            Arc::new(InitialGraphCommitData {
                sha,
                parents: smallvec![parent],
                ref_names: refs
                    .iter()
                    .map(|name| SharedString::from(name.to_string()))
                    .collect(),
            })
        };
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                graph_commit(third, second, &["HEAD -> feature"]),
                graph_commit(second, first, &[]),
                graph_commit(first, base, &["refs/smartlog/hidden/not-a-real-sha"]),
            ],
        );
        let commit_data = |sha: Oid, parent: Oid, subject: &str| {
            (
                CommitData {
                    sha,
                    parents: smallvec![parent],
                    author_name: "Ada".into(),
                    author_email: "ada@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: subject.to_string().into(),
                    message: format!("{subject}\n\nA longer description.").into(),
                },
                false,
            )
        };
        fs.set_commit_data(
            Path::new("/project/.git"),
            [
                commit_data(third, second, "third"),
                commit_data(second, first, "second"),
                commit_data(first, base, "first"),
                commit_data(base, oid(9), "trunk commit"),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let smartlog = cx.new_window_entity(|window, cx| {
            Smartlog::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                "main".into(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(smartlog.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        };

        draw(cx);
        smartlog.read_with(&*cx, |smartlog, _| {
            assert!(
                smartlog
                    .layout
                    .rows
                    .iter()
                    .any(|row| row.sha == Some(third))
            );
            assert!(!smartlog.uncommitted_files.is_empty());
        });

        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.click_commit(second, gpui::Modifiers::default(), window, cx);
        });
        draw(cx);

        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.click_commit(
                first,
                gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
                window,
                cx,
            );
        });
        draw(cx);
        smartlog.read_with(&*cx, |smartlog, _| assert_eq!(smartlog.selection.len(), 2));

        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.click_commit(third, gpui::Modifiers::default(), window, cx);
            smartlog.set_commit_mode(true, window, cx);
            smartlog.settings.compact = true;
            smartlog.show_hidden = true;
            smartlog.history_expanded = true;
            cx.notify();
        });
        draw(cx);

        smartlog.update_in(cx, |smartlog, window, cx| {
            let row = smartlog
                .layout
                .rows
                .iter()
                .position(|row| row.sha == Some(second))
                .expect("the second commit has a row");
            smartlog.deploy_context_menu(point(gpui::px(100.), gpui::px(100.)), row, window, cx);
            smartlog.deploy_settings_menu(point(gpui::px(10.), gpui::px(10.)), window, cx);
            smartlog.deploy_bulk_actions_menu(point(gpui::px(10.), gpui::px(10.)), window, cx);
        });
        draw(cx);

        let entries = smartlog
            .read_with(&*cx, |smartlog, cx| smartlog.editable_stack(third, cx))
            .expect("the stack is a straight line");
        let (stack_base, tip, entries) = entries;
        let smartlog_handle = smartlog.downgrade();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| {
                edit_stack::EditStackModal::new(smartlog_handle, stack_base, tip, entries, cx)
            });
        });
        draw(cx);

        smartlog.update_in(cx, |smartlog, window, cx| smartlog.open_absorb(window, cx));
        draw(cx);

        let download_smartlog = smartlog.downgrade();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                download::DownloadModal::new(download_smartlog, window, cx)
            });
        });
        draw(cx);

        let split_smartlog = smartlog.downgrade();
        let split_repository = repository.clone();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                split::SplitModal::new(
                    split_smartlog,
                    split_repository,
                    third,
                    "third",
                    "third\n\nA longer description.".to_string(),
                    window,
                    cx,
                )
            });
        });
        draw(cx);

        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.sidebar.collapsed = true;
            smartlog.rebase_in_progress(cx);
            smartlog.refresh(window, cx);
        });
        draw(cx);
    }

    #[allow(clippy::disallowed_methods)]
    fn run_git(directory: &std::path::Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@zed.dev")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@zed.dev")
            .env("GIT_EDITOR", "true")
            .output()
            .expect("failed to run git");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Runs the real git store against a real repository, because the fake one can't show what
    /// the commit graph looks like while a rebase is in the middle of rewriting it.
    #[gpui::test]
    async fn every_stack_stays_visible_while_one_is_being_rebased(cx: &mut gpui::TestAppContext) {
        use fs::RealFs;
        use project::Project;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        cx.executor().allow_parking();

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        run_git(path, &["init", "-q", "-b", "main"]);
        std::fs::write(path.join("f.txt"), "base\n").unwrap();
        run_git(path, &["add", "f.txt"]);
        run_git(path, &["commit", "-qm", "base"]);
        let base = run_git(path, &["rev-parse", "HEAD"]);

        run_git(path, &["switch", "-qc", "other"]);
        std::fs::write(path.join("o1.txt"), "o1\n").unwrap();
        run_git(path, &["add", "o1.txt"]);
        run_git(path, &["commit", "-qm", "other one"]);
        std::fs::write(path.join("o2.txt"), "o2\n").unwrap();
        run_git(path, &["add", "o2.txt"]);
        run_git(path, &["commit", "-qm", "other two"]);

        run_git(path, &["switch", "-qc", "feature", &base]);
        std::fs::write(path.join("a.txt"), "a\n").unwrap();
        run_git(path, &["add", "a.txt"]);
        run_git(path, &["commit", "-qm", "feature one"]);
        std::fs::write(path.join("f.txt"), "feature change\n").unwrap();
        run_git(path, &["commit", "-qam", "feature two conflicts"]);
        std::fs::write(path.join("c.txt"), "c\n").unwrap();
        run_git(path, &["add", "c.txt"]);
        run_git(path, &["commit", "-qm", "feature three"]);

        run_git(path, &["switch", "-q", "main"]);
        std::fs::write(path.join("f.txt"), "main change\n").unwrap();
        run_git(path, &["commit", "-qam", "main moves"]);
        run_git(path, &["switch", "-q", "feature"]);

        let project = Project::test(RealFs::new(None, cx.executor()), [path], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let smartlog = cx.new_window_entity(|window, cx| {
            Smartlog::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                "main".into(),
                window,
                cx,
            )
        });

        #[allow(clippy::disallowed_methods)]
        let wait_for = |cx: &mut gpui::VisualTestContext, condition: &dyn Fn(&Smartlog) -> bool| {
            for _ in 0..200 {
                cx.run_until_parked();
                if smartlog.read_with(&*cx, |smartlog, _| condition(smartlog)) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            false
        };
        let draft_subjects = |cx: &mut gpui::VisualTestContext| -> Vec<String> {
            smartlog.read_with(&*cx, |smartlog, _| {
                smartlog
                    .layout
                    .rows
                    .iter()
                    .filter(|row| row.kind == RowKind::Draft)
                    .filter_map(|row| row.sha)
                    .filter_map(|sha| smartlog.commits.get(&sha))
                    .map(|commit| commit.subject.to_string())
                    .collect()
            })
        };

        assert!(
            wait_for(cx, &|smartlog| smartlog
                .layout
                .rows
                .iter()
                .filter(|row| row.kind == RowKind::Draft)
                .count()
                == 5
                && smartlog.commits.len() >= 5),
            "all five draft commits should be shown before the rebase: {:?}",
            draft_subjects(cx)
        );

        let branches_of_stack = |subject: &str, cx: &mut gpui::VisualTestContext| {
            smartlog.read_with(&*cx, |smartlog, cx| {
                let root = smartlog
                    .commits
                    .iter()
                    .find(|(_, commit)| commit.subject.as_ref() == subject)
                    .map(|(sha, _)| *sha)
                    .expect("commit is shown");
                smartlog.stack_branches(root, cx)
            })
        };
        assert_eq!(branches_of_stack("feature one", cx), vec!["feature"]);
        assert_eq!(branches_of_stack("other one", cx), vec!["other"]);

        let plan = smartlog
            .read_with(&*cx, |smartlog, cx| smartlog.head_rebase_plan(cx))
            .expect("the checked-out stack can be rebased");
        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.rebase_stack(plan, window, cx);
        });

        #[allow(clippy::disallowed_methods)]
        let stopped = {
            let mut stopped = false;
            for _ in 0..200 {
                cx.run_until_parked();
                if repository.read_with(&*cx, |repository, _| repository.merge.rebase_in_progress) {
                    stopped = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            stopped
        };
        assert!(stopped, "the rebase should stop on the conflict");
        // Give the draft log time to reload after the head moved.
        wait_for(cx, &|smartlog| {
            smartlog
                .layout
                .rows
                .iter()
                .filter(|row| row.kind == RowKind::Draft)
                .count()
                >= 6
        });

        let subjects = draft_subjects(cx);
        for subject in [
            "other one",
            "other two",
            "feature one",
            "feature two conflicts",
            "feature three",
        ] {
            assert!(
                subjects.iter().any(|candidate| candidate == subject),
                "{subject:?} disappeared during the rebase: {subjects:?}"
            );
        }
        assert!(
            subjects.len() >= 6,
            "the replayed commit should be shown too: {subjects:?}"
        );

        // Resolve the conflict and continue instead of aborting.
        std::fs::write(path.join("f.txt"), "resolved\n").unwrap();
        run_git(path, &["add", "f.txt"]);
        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.continue_rebase(window, cx)
        });
        #[allow(clippy::disallowed_methods)]
        {
            for _ in 0..200 {
                cx.run_until_parked();
                if !repository.read_with(&*cx, |repository, _| repository.merge.rebase_in_progress)
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            for _ in 0..40 {
                cx.run_until_parked();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        let after_continue = draft_subjects(cx);
        assert_eq!(after_continue.len(), 5, "{after_continue:?}");
    }

    #[gpui::test]
    async fn an_empty_repository_opens_and_offers_an_initial_commit(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use project::Project;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(Path::new("/project"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.refs.remove("HEAD");
        })
        .expect("the fake repository exists");
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository_id = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("repository")
                .read(cx)
                .id
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            let git_store = workspace.project().read(cx).git_store().clone();
            let workspace_handle = workspace.weak_handle();
            let smartlog = cx.new(|cx| {
                Smartlog::new(
                    repository_id,
                    git_store,
                    workspace_handle,
                    "main".into(),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(smartlog), None, true, window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));

        workspace.read_with(&*cx, |workspace, cx| {
            let smartlog = workspace
                .active_item_as::<Smartlog>(cx)
                .expect("the Smartlog is open");
            let smartlog = smartlog.read(cx);
            assert!(smartlog.head.is_none());
            assert!(smartlog.layout.rows.is_empty());
        });
    }

    #[gpui::test]
    async fn a_stack_held_up_by_a_kept_ref_can_be_dragged_onto_a_commit(
        cx: &mut gpui::TestAppContext,
    ) {
        use fs::RealFs;
        use project::Project;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        cx.executor().allow_parking();

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        run_git(path, &["init", "-q", "-b", "main"]);
        std::fs::write(path.join("f.txt"), "base\n").unwrap();
        run_git(path, &["add", "f.txt"]);
        run_git(path, &["commit", "-qm", "base"]);

        run_git(path, &["switch", "-qc", "feature"]);
        std::fs::write(path.join("a.txt"), "a\n").unwrap();
        run_git(path, &["add", "a.txt"]);
        run_git(path, &["commit", "-qm", "feature one"]);
        let parent = run_git(path, &["rev-parse", "HEAD"]);
        std::fs::write(path.join("b.txt"), "b\n").unwrap();
        run_git(path, &["add", "b.txt"]);
        run_git(path, &["commit", "-qm", "feature two"]);

        run_git(path, &["switch", "-qc", "temporary", &parent]);
        std::fs::write(path.join("t.txt"), "t\n").unwrap();
        run_git(path, &["add", "t.txt"]);
        run_git(path, &["commit", "-qm", "temporary"]);
        let temporary = run_git(path, &["rev-parse", "HEAD"]);
        run_git(
            path,
            &[
                "update-ref",
                &format!("refs/smartlog/kept/{temporary}"),
                &temporary,
            ],
        );
        run_git(path, &["switch", "-q", "feature"]);
        run_git(path, &["branch", "-D", "temporary"]);

        let project = Project::test(RealFs::new(None, cx.executor()), [path], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let smartlog = cx.new_window_entity(|window, cx| {
            Smartlog::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                "main".into(),
                window,
                cx,
            )
        });

        #[allow(clippy::disallowed_methods)]
        let shown = {
            let mut shown = false;
            for _ in 0..200 {
                cx.run_until_parked();
                shown = smartlog.read_with(&*cx, |smartlog, _| smartlog.commits.len() >= 3);
                if shown {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            shown
        };
        assert!(shown, "the stack held up by a kept ref should be shown");

        let (source, target) = smartlog.read_with(&*cx, |smartlog, _| {
            let find = |subject: &str| {
                smartlog
                    .commits
                    .iter()
                    .find(|(_, commit)| commit.subject.as_ref() == subject)
                    .map(|(sha, _)| *sha)
                    .expect("commit is shown")
            };
            (find("temporary"), find("feature two"))
        });
        let plan = smartlog.read_with(&*cx, |smartlog, cx| {
            smartlog.drag_rebase_plan(source, target, cx)
        });
        assert!(
            plan.is_some(),
            "dragging the stack onto a commit gives a plan"
        );

        smartlog.update_in(cx, |smartlog, window, cx| {
            smartlog.rebase_stack_onto(plan.unwrap(), target.to_string(), window, cx);
        });
        #[allow(clippy::disallowed_methods)]
        for _ in 0..100 {
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let moved = run_git(
            path,
            &[
                "log",
                "--format=%s",
                &format!("feature..smartlog/{}", &temporary[..7]),
            ],
        );
        assert_eq!(moved, "temporary");
    }
}
