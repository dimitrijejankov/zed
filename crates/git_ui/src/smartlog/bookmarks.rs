use super::*;

/// The local branches that can be deleted because their tip is already on the trunk: they are
/// not in the draft log, they are not checked out, and they are not the trunk itself.
pub(super) fn merged_bookmarks(
    bookmarks: &[Bookmark],
    draft_shas: &HashSet<String>,
    trunk: &str,
) -> Vec<String> {
    bookmarks
        .iter()
        .filter(|bookmark| {
            !bookmark.is_head
                && bookmark.name != trunk
                && bookmark
                    .tip
                    .as_ref()
                    .is_some_and(|tip| !draft_shas.contains(tip))
        })
        .map(|bookmark| bookmark.name.clone())
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Bookmark {
    pub(super) name: String,
    pub(super) subject: SharedString,
    pub(super) tip: Option<String>,
    pub(super) is_head: bool,
}

pub(super) struct BookmarksModal {
    smartlog: WeakEntity<Smartlog>,
    repository: Entity<Repository>,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl BookmarksModal {
    pub(super) fn new(
        smartlog: WeakEntity<Smartlog>,
        repository: Entity<Repository>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&repository, |_, _, event: &RepositoryEvent, cx| {
            if matches!(
                event,
                RepositoryEvent::BranchListChanged | RepositoryEvent::HeadChanged
            ) {
                cx.notify();
            }
        });
        Self {
            smartlog,
            repository,
            focus_handle: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    fn bookmarks(&self, cx: &App) -> Vec<Bookmark> {
        self.repository
            .read(cx)
            .branch_list
            .iter()
            .filter(|branch| !branch.is_remote())
            .map(|branch| Bookmark {
                name: branch.name().to_string(),
                subject: branch
                    .most_recent_commit
                    .as_ref()
                    .map(|commit| commit.subject.clone())
                    .unwrap_or_default(),
                tip: branch
                    .most_recent_commit
                    .as_ref()
                    .map(|commit| commit.sha.to_string()),
                is_head: branch.is_head,
            })
            .collect()
    }
}

impl EventEmitter<DismissEvent> for BookmarksModal {}
impl ModalView for BookmarksModal {}

impl Focusable for BookmarksModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BookmarksModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bookmarks = self.bookmarks(cx);
        let merged = self
            .smartlog
            .read_with(cx, |smartlog, _| {
                let draft_shas: HashSet<String> = smartlog
                    .all_draft_shas
                    .iter()
                    .map(|sha| sha.to_string())
                    .collect();
                merged_bookmarks(&bookmarks, &draft_shas, &smartlog.trunk)
            })
            .unwrap_or_default();
        let merged_count = merged.len();
        let colors = cx.theme().colors().clone();

        v_flex()
            .key_context("SmartlogBookmarks")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(DismissEvent)))
            .elevation_2(cx)
            .w(rems(38.))
            .max_h(rems(30.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::Bookmark).size(IconSize::XSmall))
                    .child(Headline::new("Bookmarks").size(HeadlineSize::XSmall)),
            )
            .child(
                v_flex()
                    .id("smartlog-bookmarks-list")
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(bookmarks.is_empty(), |this| {
                        this.child(Label::new("No bookmarks").color(Color::Muted))
                    })
                    .children(bookmarks.into_iter().enumerate().map(|(index, bookmark)| {
                        let name = bookmark.name.clone();
                        let goto_name = bookmark.name.clone();
                        let delete_name = bookmark.name.clone();
                        h_flex()
                            .h_8()
                            .gap_2()
                            .child(
                                Label::new(name)
                                    .weight(if bookmark.is_head {
                                        gpui::FontWeight::BOLD
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .truncate(),
                            )
                            .child(
                                Label::new(bookmark.subject)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .child(div().flex_1())
                            .child(
                                IconButton::new(
                                    ("smartlog-bookmark-goto", index),
                                    IconName::ArrowRight,
                                )
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::Small)
                                .disabled(bookmark.is_head)
                                .tooltip(Tooltip::text("Go to this bookmark"))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        let name = goto_name.clone();
                                        this.smartlog
                                            .update(cx, |smartlog, cx| {
                                                smartlog.goto_bookmark(name, window, cx)
                                            })
                                            .log_err();
                                        cx.emit(DismissEvent);
                                    },
                                )),
                            )
                            .child(
                                IconButton::new(
                                    ("smartlog-bookmark-delete", index),
                                    IconName::Trash,
                                )
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::Small)
                                .disabled(bookmark.is_head)
                                .tooltip(Tooltip::text("Delete this bookmark"))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        let name = delete_name.clone();
                                        this.smartlog
                                            .update(cx, |smartlog, cx| {
                                                smartlog.delete_bookmarks(vec![name], window, cx)
                                            })
                                            .log_err();
                                    },
                                )),
                            )
                    })),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .justify_end()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Button::new("smartlog-clean-up-bookmarks", "Clean up")
                            .start_icon(Icon::new(IconName::Trash).size(IconSize::Small))
                            .style(ButtonStyle::Filled)
                            .disabled(merged_count == 0)
                            .tooltip(Tooltip::text(format!(
                                "Delete the {merged_count} bookmarks that are already on the trunk"
                            )))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let merged = merged.clone();
                                this.smartlog
                                    .update(cx, |smartlog, cx| {
                                        smartlog.delete_bookmarks(merged, window, cx)
                                    })
                                    .log_err();
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bookmark(name: &str, tip: Option<&str>, is_head: bool) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            subject: SharedString::default(),
            tip: tip.map(str::to_string),
            is_head,
        }
    }

    #[test]
    fn only_bookmarks_already_on_the_trunk_are_merged() {
        let drafts: HashSet<String> = HashSet::from_iter(["draft-tip".to_string()]);
        let bookmarks = [
            bookmark("main", Some("trunk-tip"), false),
            bookmark("landed", Some("old-commit"), false),
            bookmark("in-progress", Some("draft-tip"), false),
            bookmark("checked-out", Some("old-commit"), true),
            bookmark("unknown", None, false),
        ];

        assert_eq!(
            merged_bookmarks(&bookmarks, &drafts, "main"),
            vec!["landed".to_string()]
        );
    }
}
