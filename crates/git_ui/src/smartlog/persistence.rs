use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use workspace::WorkspaceDb;

pub(super) struct SmartlogsDb(ThreadSafeConnection);

impl Domain for SmartlogsDb {
    const NAME: &str = stringify!(SmartlogsDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE smartlogs (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,
                repo_working_path TEXT,
                trunk TEXT,
                selected_sha TEXT,
                sidebar_collapsed INTEGER,
                sidebar_list_permille INTEGER,
                show_hidden INTEGER,
                filter TEXT,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        ),
        sql!(
            ALTER TABLE smartlogs ADD COLUMN settings TEXT;
        ),
    ];
}

db::static_connection!(SmartlogsDb, [WorkspaceDb]);

/// Per-tab preferences, mirroring the ISL settings that make sense in an editor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SmartlogSettings {
    /// Clicking a changed file's name opens its diff rather than the file.
    pub(super) click_opens_diff: bool,
    pub(super) compact: bool,
    pub(super) scroll_to_current: bool,
    pub(super) copy_short_hash: bool,
    pub(super) confirm_rebase_onto_trunk: bool,
}

impl Default for SmartlogSettings {
    fn default() -> Self {
        Self {
            click_opens_diff: true,
            compact: false,
            scroll_to_current: true,
            copy_short_hash: false,
            confirm_rebase_onto_trunk: false,
        }
    }
}

impl SmartlogSettings {
    pub(super) fn from_json(json: Option<&str>) -> Self {
        json.and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default()
    }

    pub(super) fn to_json(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }
}

pub(super) struct SerializedSmartlog {
    pub(super) repo_working_path: PathBuf,
    pub(super) trunk: Option<String>,
    pub(super) selected_sha: Option<String>,
    pub(super) sidebar_collapsed: Option<bool>,
    pub(super) sidebar_list_permille: Option<i32>,
    pub(super) show_hidden: Option<bool>,
    pub(super) filter: Option<String>,
    pub(super) settings: SmartlogSettings,
}

impl SmartlogsDb {
    query! {
        pub(super) async fn save_smartlog(
            item_id: workspace::ItemId,
            workspace_id: workspace::WorkspaceId,
            repo_working_path: String,
            trunk: String,
            selected_sha: Option<String>,
            sidebar_collapsed: bool,
            sidebar_list_permille: i32,
            show_hidden: bool,
            filter: Option<String>,
            settings: Option<String>
        ) -> Result<()> {
            INSERT OR REPLACE INTO smartlogs(
                item_id, workspace_id, repo_working_path, trunk, selected_sha,
                sidebar_collapsed, sidebar_list_permille, show_hidden, filter, settings
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        }
    }

    query! {
        fn load_smartlog(
            item_id: workspace::ItemId,
            workspace_id: workspace::WorkspaceId
        ) -> Result<Option<(
            PathBuf,
            Option<String>,
            Option<String>,
            Option<bool>,
            Option<i32>,
            Option<bool>,
            Option<String>,
            Option<String>
        )>> {
            SELECT
                repo_working_path,
                trunk,
                selected_sha,
                sidebar_collapsed,
                sidebar_list_permille,
                show_hidden,
                filter,
                settings
            FROM smartlogs
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    pub(super) fn get_smartlog(
        &self,
        item_id: workspace::ItemId,
        workspace_id: workspace::WorkspaceId,
    ) -> anyhow::Result<Option<SerializedSmartlog>> {
        Ok(self.load_smartlog(item_id, workspace_id)?.map(
            |(
                repo_working_path,
                trunk,
                selected_sha,
                sidebar_collapsed,
                sidebar_list_permille,
                show_hidden,
                filter,
                settings,
            )| SerializedSmartlog {
                repo_working_path,
                trunk,
                selected_sha,
                sidebar_collapsed,
                sidebar_list_permille,
                show_hidden,
                filter,
                settings: SmartlogSettings::from_json(settings.as_deref()),
            },
        ))
    }
}

/// The share of the width the commit list gets, stored as thousandths so no float is needed.
pub(super) fn ratio_to_permille(ratio: f32) -> i32 {
    (ratio.clamp(0.0, 1.0) * 1000.0).round() as i32
}

pub(super) fn permille_to_ratio(permille: i32) -> f32 {
    permille.clamp(0, 1000) as f32 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn a_saved_smartlog_can_be_loaded_again(_cx: &mut gpui::TestAppContext) {
        let connection =
            db::open_test_db::<(WorkspaceDb, SmartlogsDb)>("a_saved_smartlog_can_be_loaded_again")
                .await;
        connection
            .write(|connection| {
                connection.exec("INSERT INTO workspaces(workspace_id) VALUES (1)")?()
            })
            .await
            .unwrap();
        let database = SmartlogsDb(connection);

        let item_id = 7;
        let workspace_id = workspace::WorkspaceId::from_i64(1);
        database
            .save_smartlog(
                item_id,
                workspace_id,
                "/project".to_string(),
                "origin/main".to_string(),
                Some("abc".to_string()),
                true,
                580,
                true,
                Some("fix".to_string()),
                SmartlogSettings {
                    compact: true,
                    ..Default::default()
                }
                .to_json(),
            )
            .await
            .unwrap();

        let loaded = database
            .get_smartlog(item_id, workspace_id)
            .unwrap()
            .expect("the saved Smartlog should load");
        assert_eq!(loaded.repo_working_path, PathBuf::from("/project"));
        assert_eq!(loaded.trunk.as_deref(), Some("origin/main"));
        assert_eq!(loaded.selected_sha.as_deref(), Some("abc"));
        assert_eq!(loaded.sidebar_collapsed, Some(true));
        assert_eq!(loaded.sidebar_list_permille, Some(580));
        assert_eq!(loaded.show_hidden, Some(true));
        assert_eq!(loaded.filter.as_deref(), Some("fix"));
        assert!(loaded.settings.compact);
        assert!(loaded.settings.click_opens_diff);
    }

    #[test]
    fn settings_round_trip_and_fall_back_to_defaults() {
        let custom = SmartlogSettings {
            compact: true,
            copy_short_hash: true,
            click_opens_diff: false,
            ..Default::default()
        };
        assert_eq!(
            SmartlogSettings::from_json(custom.to_json().as_deref()),
            custom
        );

        assert_eq!(
            SmartlogSettings::from_json(None),
            SmartlogSettings::default()
        );
        assert_eq!(
            SmartlogSettings::from_json(Some("not json")),
            SmartlogSettings::default()
        );
        assert_eq!(
            SmartlogSettings::from_json(Some(r#"{"compact": true}"#)),
            SmartlogSettings {
                compact: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn the_split_ratio_survives_a_round_trip_and_stays_in_range() {
        assert_eq!(ratio_to_permille(0.58), 580);
        assert_eq!(permille_to_ratio(580), 0.58);
        assert_eq!(ratio_to_permille(-1.0), 0);
        assert_eq!(ratio_to_permille(7.0), 1000);
        assert_eq!(permille_to_ratio(5000), 1.0);
    }
}
