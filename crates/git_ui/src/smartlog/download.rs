use super::*;

/// What to ask the remote for, given what was typed: a pull request number such as `123` or
/// `#123` becomes the ref GitHub publishes for it, and anything else is passed through as a
/// branch, tag or commit.
pub(super) fn download_refspec(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() || input.chars().any(char::is_whitespace) {
        return None;
    }
    let number = input.strip_prefix('#').unwrap_or(input);
    if !number.is_empty() && number.chars().all(|character| character.is_ascii_digit()) {
        return Some(format!("pull/{number}/head"));
    }
    Some(input.to_string())
}

/// The name of the local branch that keeps the downloaded commits reachable.
pub(super) fn download_bookmark_name(refspec: &str) -> String {
    if let Some(number) = refspec
        .strip_prefix("pull/")
        .and_then(|rest| rest.strip_suffix("/head"))
    {
        return format!("pr-{number}");
    }
    let looks_like_a_hash = refspec.len() >= 7
        && refspec.len() <= 64
        && refspec
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    if looks_like_a_hash {
        return format!("download-{}", &refspec[..refspec.len().min(8)]);
    }
    refspec
        .strip_prefix("refs/heads/")
        .or_else(|| refspec.strip_prefix("refs/tags/"))
        .unwrap_or(refspec)
        .to_string()
}

pub(super) struct DownloadModal {
    smartlog: WeakEntity<Smartlog>,
    editor: Entity<Editor>,
    go_to_it: bool,
}

impl DownloadModal {
    pub(super) fn new(
        smartlog: WeakEntity<Smartlog>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "Branch, tag, commit hash or pull request number",
                window,
                cx,
            );
            editor
        });
        Self {
            smartlog,
            editor,
            go_to_it: true,
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(refspec) = download_refspec(&self.editor.read(cx).text(cx)) else {
            return;
        };
        let go_to_it = self.go_to_it;
        self.smartlog
            .update(cx, |smartlog, cx| {
                smartlog.download_commits(refspec, go_to_it, window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for DownloadModal {}
impl ModalView for DownloadModal {}

impl Focusable for DownloadModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for DownloadModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SmartlogDownload")
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(DismissEvent)))
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
                    .child(Icon::new(IconName::Download).size(IconSize::XSmall))
                    .child(Headline::new("Download Commits").size(HeadlineSize::XSmall)),
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
                            "smartlog-download-go-to-it",
                            if self.go_to_it {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .label("Go to the downloaded commit")
                        .on_click(cx.listener(
                            |this, state: &ToggleState, _, cx| {
                                this.go_to_it = *state == ToggleState::Selected;
                                cx.notify();
                            },
                        )),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_typed_becomes_a_refspec() {
        assert_eq!(download_refspec("  main "), Some("main".to_string()));
        assert_eq!(download_refspec("#123"), Some("pull/123/head".to_string()));
        assert_eq!(download_refspec("42"), Some("pull/42/head".to_string()));
        assert_eq!(
            download_refspec("feature/login"),
            Some("feature/login".to_string())
        );
        assert_eq!(download_refspec("two words"), None);
        assert_eq!(download_refspec("   "), None);
        assert_eq!(download_refspec("#"), Some("#".to_string()));
    }

    #[test]
    fn downloaded_commits_get_a_readable_bookmark() {
        assert_eq!(download_bookmark_name("pull/123/head"), "pr-123");
        assert_eq!(
            download_bookmark_name("0123456789abcdef0123456789abcdef01234567"),
            "download-01234567"
        );
        assert_eq!(download_bookmark_name("refs/heads/topic"), "topic");
        assert_eq!(download_bookmark_name("refs/tags/v1.2"), "v1.2");
        assert_eq!(download_bookmark_name("feature/login"), "feature/login");
    }
}
