use anyhow::Result;
use chrono::Utc;
use uuid::Uuid;

use crate::db::Database;

use super::core::{SharedWriter, WriteSet};

impl SharedWriter {
    pub fn create_milestone(
        &self,
        db: &Database,
        name: &str,
        description: Option<&str>,
    ) -> Result<i64> {
        let uuid = Uuid::new_v4();
        let now = Utc::now();
        let name_owned = name.to_string();
        let desc_owned = description.map(std::string::ToString::to_string);

        let _ = self.write_commit_push(
            db,
            |_writer| {
                let event = crate::events::Event::MilestoneCreated {
                    uuid,
                    display_id: None,
                    name: name_owned.clone(),
                    description: desc_owned.clone(),
                    created_at: now,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("create milestone: {name}"),
        )?;

        if let Some(id) = self.v3_assigned_milestone_id(&uuid) {
            return Ok(id);
        }
        db.get_milestone_id_by_uuid(&uuid.to_string())
    }

    pub fn close_milestone(&self, db: &Database, milestone_id: i64) -> Result<()> {
        let _ = self.write_commit_push(
            db,
            |writer| {
                let entry = writer.load_milestone_by_id(milestone_id)?;
                let closed_at = Utc::now();
                let event = crate::events::Event::MilestoneClosed {
                    uuid: entry.uuid,
                    closed_at,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("close milestone #{milestone_id}"),
        )?;
        Ok(())
    }

    pub fn delete_milestone(&self, db: &Database, milestone_id: i64) -> Result<()> {
        let entry = self.load_milestone_by_id(milestone_id)?;

        let _ = self.write_commit_push(
            db,
            |_writer| {
                let event = crate::events::Event::MilestoneDeleted { uuid: entry.uuid };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("delete milestone #{milestone_id}"),
        )?;
        Ok(())
    }

    pub fn set_milestone_on_issues(
        &self,
        db: &Database,
        milestone_id: i64,
        issue_ids: &[i64],
    ) -> Result<()> {
        let milestone = self.load_milestone_by_id(milestone_id)?;
        let ms_uuid = milestone.uuid;

        let ids: Vec<i64> = issue_ids.to_vec();
        let _ = self.write_commit_push(
            db,
            |writer| {
                let mut events = Vec::new();
                for &issue_id in &ids {
                    let issue = writer.load_issue_by_id(issue_id, db)?;
                    events.push(crate::events::Event::MilestoneAssigned {
                        issue_uuid: issue.uuid,
                        milestone_uuid: Some(ms_uuid),
                    });
                }
                Ok(WriteSet { events })
            },
            &format!("add {} issue(s) to milestone #{}", ids.len(), milestone_id),
        )?;
        Ok(())
    }

    pub fn clear_milestone_on_issue(&self, db: &Database, issue_id: i64) -> Result<()> {
        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(issue_id, db)?;
                let event = crate::events::Event::MilestoneAssigned {
                    issue_uuid: issue.uuid,
                    milestone_uuid: None,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("remove issue #{issue_id} from milestone"),
        )?;
        Ok(())
    }
}
