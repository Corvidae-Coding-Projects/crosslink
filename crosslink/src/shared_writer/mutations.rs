use anyhow::Result;
use chrono::{DateTime, Utc};
use std::cell::Cell;
use uuid::Uuid;

use crate::db::Database;

use super::core::{SharedWriter, WriteSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedIssueSpec {
    pub uuid: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub priority: String,
    pub parent_uuid: Option<Uuid>,
    pub closed: bool,
    pub labels: Vec<String>,
    pub comments: Vec<ImportedCommentSpec>,

    pub blockers: Vec<Uuid>,

    pub display_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedCommentSpec {
    pub author: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DescriptionUpdate<'a> {
    #[default]
    Unchanged,

    Clear,

    Set(&'a str),
}

impl<'a> From<Option<Option<&'a str>>> for DescriptionUpdate<'a> {
    fn from(opt: Option<Option<&'a str>>) -> Self {
        match opt {
            None => Self::Unchanged,
            Some(None) => Self::Clear,
            Some(Some(s)) => Self::Set(s),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FieldUpdate<T> {
    #[default]
    Unchanged,

    Clear,

    Set(T),
}

impl<T> From<Option<Option<T>>> for FieldUpdate<T> {
    fn from(opt: Option<Option<T>>) -> Self {
        match opt {
            None => Self::Unchanged,
            Some(None) => Self::Clear,
            Some(Some(v)) => Self::Set(v),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IssueUpdate<'a> {
    pub title: Option<&'a str>,
    pub description: DescriptionUpdate<'a>,
    pub status: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub scheduled_at: FieldUpdate<chrono::DateTime<chrono::Utc>>,
    pub due_at: FieldUpdate<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Copy)]
struct IssueCreate<'a> {
    title: &'a str,
    description: Option<&'a str>,
    priority: &'a str,
    parent_uuid: Option<Uuid>,
    scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
    due_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone)]
struct CommentParams {
    content: String,
    kind: String,
    trigger_type: Option<String>,
    intervention_context: Option<String>,
    driver_key_fingerprint: Option<String>,
}

impl SharedWriter {
    fn create_issue_inner(
        &self,
        db: &Database,
        create: IssueCreate<'_>,
        commit_msg: &str,
    ) -> Result<i64> {
        crate::db::validate_issue_title(create.title)?;
        crate::db::validate_issue_description(create.description)?;
        let uuid = Uuid::new_v4();
        let title_owned = create.title.to_string();
        let desc_owned = create.description.map(std::string::ToString::to_string);
        let priority_parsed: crate::models::Priority = create.priority.parse()?;
        let agent_id = self.agent.agent_id.clone();
        let parent_uuid = create.parent_uuid;
        let scheduled_at = create.scheduled_at;
        let due_at = create.due_at;

        self.write_commit_push(
            db,
            |_writer| {
                let event = crate::events::Event::IssueCreated {
                    uuid,
                    title: title_owned.clone(),
                    description: desc_owned.clone(),
                    priority: priority_parsed.to_string(),
                    labels: vec![],
                    parent_uuid,
                    created_by: agent_id.clone(),
                    display_id: None,
                    scheduled_at,
                    due_at,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            commit_msg,
        )?;

        let sqlite_id = db.get_issue_id_by_uuid(&uuid.to_string());
        if let Some(id) = self.v3_assigned_display_id(&uuid) {
            if sqlite_id.is_err() {
                anyhow::bail!(
                    "issue {uuid} was committed to the hub (display id {id}) but is not \
                     visible in the local database after hydration; run `crosslink sync` \
                     to recover, then verify with `crosslink list`"
                );
            }
            return Ok(id);
        }
        sqlite_id
    }

    pub fn create_issue(
        &self,
        db: &Database,
        title: &str,
        description: Option<&str>,
        priority: &str,
        scheduled_at: Option<DateTime<Utc>>,
        due_at: Option<DateTime<Utc>>,
    ) -> Result<i64> {
        self.create_issue_inner(
            db,
            IssueCreate {
                title,
                description,
                priority,
                parent_uuid: None,
                scheduled_at,
                due_at,
            },
            &format!("create issue: {title}"),
        )
    }

    pub fn import_issues(
        &self,
        db: &Database,
        specs: &[ImportedIssueSpec],
    ) -> Result<Vec<(Uuid, i64)>> {
        let mut events = Vec::new();
        for spec in specs {
            crate::db::validate_issue_title(&spec.title)?;
            crate::db::validate_issue_description(spec.description.as_deref())?;
            let priority: crate::models::Priority = spec.priority.parse()?;
            events.push(crate::events::Event::IssueCreated {
                uuid: spec.uuid,
                title: spec.title.clone(),
                description: spec.description.clone(),
                priority: priority.to_string(),
                labels: spec.labels.clone(),
                parent_uuid: spec.parent_uuid,
                created_by: self.agent.agent_id.clone(),
                display_id: spec.display_id,
                scheduled_at: None,
                due_at: None,
            });
            for c in &spec.comments {
                events.push(crate::events::Event::CommentAdded {
                    issue_uuid: spec.uuid,
                    comment_uuid: Uuid::new_v4(),
                    display_id: None,
                    author: c.author.clone(),
                    content: c.content.clone(),
                    created_at: c.created_at,
                    kind: c.kind.clone(),
                    trigger_type: None,
                    intervention_context: None,
                    driver_key_fingerprint: None,
                    signed_by: None,
                    signature: None,
                });
            }
            for blocker in &spec.blockers {
                events.push(crate::events::Event::DependencyAdded {
                    blocked_uuid: spec.uuid,
                    blocker_uuid: *blocker,
                });
            }
            if spec.closed {
                events.push(crate::events::Event::StatusChanged {
                    uuid: spec.uuid,
                    new_status: "closed".to_string(),
                    closed_at: Some(Utc::now()),
                });
            }
        }

        self.write_commit_push(
            db,
            |_writer| {
                Ok(WriteSet {
                    events: events.clone(),
                })
            },
            &format!("import {} issues", specs.len()),
        )?;

        let mut assigned = Vec::with_capacity(specs.len());
        for spec in specs {
            let Ok(sqlite_id) = db.get_issue_id_by_uuid(&spec.uuid.to_string()) else {
                anyhow::bail!(
                    "imported issue {} ('{}') was committed to the hub but is not \
                     visible in the local database after hydration; run \
                     `crosslink sync` to recover, then verify with `crosslink list`",
                    spec.uuid,
                    spec.title
                );
            };
            let id = self.v3_assigned_display_id(&spec.uuid).unwrap_or(sqlite_id);
            assigned.push((spec.uuid, id));
        }
        Ok(assigned)
    }

    pub fn create_subissue(
        &self,
        db: &Database,
        parent_id: i64,
        title: &str,
        description: Option<&str>,
        priority: &str,
    ) -> Result<i64> {
        let parent_uuid = self.resolve_uuid(parent_id, db)?;
        self.create_issue_inner(
            db,
            IssueCreate {
                title,
                description,
                priority,
                parent_uuid: Some(parent_uuid),
                scheduled_at: None,
                due_at: None,
            },
            &format!("create subissue under #{parent_id}: {title}"),
        )
    }

    pub fn update_issue(
        &self,
        db: &Database,
        display_id: i64,
        update: IssueUpdate<'_>,
    ) -> Result<()> {
        if let Some(title) = update.title {
            crate::db::validate_issue_title(title)?;
        }
        if let DescriptionUpdate::Set(description) = update.description {
            crate::db::validate_issue_description(Some(description))?;
        }
        let title_owned = update.title.map(std::string::ToString::to_string);
        let desc_update = update.description;
        let status_parsed = update
            .status
            .map(str::parse::<crate::models::IssueStatus>)
            .transpose()?;
        let priority_parsed = update
            .priority
            .map(str::parse::<crate::models::Priority>)
            .transpose()?;
        let scheduled_at = update.scheduled_at;
        let due_at = update.due_at;

        let _ = self.write_commit_push(
            db,
            |writer| {
                let mut issue = writer.load_issue_by_id(display_id, db)?;
                if let Some(ref t) = title_owned {
                    issue.title.clone_from(t);
                }
                match &desc_update {
                    DescriptionUpdate::Unchanged => {}
                    DescriptionUpdate::Clear => issue.description = None,
                    DescriptionUpdate::Set(s) => issue.description = Some((*s).to_string()),
                }
                if let Some(s) = status_parsed {
                    issue.status = s;
                }
                if let Some(p) = priority_parsed {
                    issue.priority = p;
                }
                let schedule_changed = !matches!(scheduled_at, FieldUpdate::Unchanged)
                    || !matches!(due_at, FieldUpdate::Unchanged);
                match scheduled_at {
                    FieldUpdate::Unchanged => {}
                    FieldUpdate::Clear => issue.scheduled_at = None,
                    FieldUpdate::Set(dt) => issue.scheduled_at = Some(dt),
                }
                match due_at {
                    FieldUpdate::Unchanged => {}
                    FieldUpdate::Clear => issue.due_at = None,
                    FieldUpdate::Set(dt) => issue.due_at = Some(dt),
                }
                issue.updated_at = Utc::now();

                let mut events = Vec::new();
                let upd_description = match &desc_update {
                    DescriptionUpdate::Set(s) => Some((*s).to_string()),
                    DescriptionUpdate::Unchanged | DescriptionUpdate::Clear => None,
                };
                if title_owned.is_some() || upd_description.is_some() || priority_parsed.is_some() {
                    events.push(crate::events::Event::IssueUpdated {
                        uuid: issue.uuid,
                        title: title_owned.clone(),
                        description: upd_description,
                        priority: priority_parsed.map(|p| p.to_string()),
                    });
                }
                if schedule_changed {
                    events.push(crate::events::Event::ScheduleChanged {
                        issue_uuid: issue.uuid,
                        scheduled_at: issue.scheduled_at,
                        due_at: issue.due_at,
                    });
                }

                if status_parsed.is_some() {
                    events.push(crate::events::Event::StatusChanged {
                        uuid: issue.uuid,
                        new_status: issue.status.to_string(),
                        closed_at: issue.closed_at,
                    });
                }

                Ok(WriteSet { events })
            },
            &format!("update issue #{display_id}"),
        )?;
        Ok(())
    }

    pub fn close_issue(&self, db: &Database, display_id: i64) -> Result<()> {
        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(display_id, db)?;
                let now = Utc::now();
                let event = crate::events::Event::StatusChanged {
                    uuid: issue.uuid,
                    new_status: "closed".to_string(),
                    closed_at: Some(now),
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("close issue #{display_id}"),
        )?;
        Ok(())
    }

    pub fn reopen_issue(&self, db: &Database, display_id: i64) -> Result<()> {
        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(display_id, db)?;
                let event = crate::events::Event::StatusChanged {
                    uuid: issue.uuid,
                    new_status: "open".to_string(),
                    closed_at: None,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("reopen issue #{display_id}"),
        )?;
        Ok(())
    }

    pub fn delete_issue(&self, db: &Database, display_id: i64) -> Result<()> {
        let issue = self.load_issue_by_id(display_id, db)?;
        let uuid = issue.uuid;

        let _ = self.write_commit_push(
            db,
            |_writer| {
                let event = crate::events::Event::IssueDeleted { uuid };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("delete issue #{display_id}"),
        )?;
        Ok(())
    }

    fn add_comment_inner(
        &self,
        db: &Database,
        display_id: i64,
        params: &CommentParams,
        commit_msg: &str,
    ) -> Result<i64> {
        let agent_id = self.agent.agent_id.clone();

        let comment_uuid_cell: Cell<Option<Uuid>> = Cell::new(None);

        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(display_id, db)?;

                let created_at = Utc::now();
                let comment_uuid = Uuid::new_v4();
                comment_uuid_cell.set(Some(comment_uuid));

                let (signed_by, signature) = writer.sign_comment(&params.content, &agent_id, 0);

                let event = crate::events::Event::CommentAdded {
                    issue_uuid: issue.uuid,
                    comment_uuid,
                    display_id: None,
                    author: agent_id.clone(),
                    content: params.content.clone(),
                    created_at,
                    kind: params.kind.clone(),
                    trigger_type: params.trigger_type.clone(),
                    intervention_context: params.intervention_context.clone(),
                    driver_key_fingerprint: params.driver_key_fingerprint.clone(),
                    signed_by,
                    signature,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            commit_msg,
        )?;

        if let Some(cuuid) = comment_uuid_cell.get() {
            if let Some(id) = self.v3_assigned_comment_id(display_id, &cuuid) {
                return Ok(id);
            }
            return db.get_comment_id_by_uuid(&cuuid.to_string());
        }

        anyhow::bail!("comment uuid was not captured during write")
    }

    pub fn add_comment(
        &self,
        db: &Database,
        display_id: i64,
        content: &str,
        kind: &str,
    ) -> Result<i64> {
        self.add_comment_inner(
            db,
            display_id,
            &CommentParams {
                content: content.to_string(),
                kind: kind.to_string(),
                trigger_type: None,
                intervention_context: None,
                driver_key_fingerprint: None,
            },
            &format!("comment on issue #{display_id}"),
        )
    }

    pub fn add_intervention_comment(
        &self,
        db: &Database,
        display_id: i64,
        content: &str,
        trigger_type: &str,
        intervention_context: Option<&str>,
        driver_key_fingerprint: Option<&str>,
    ) -> Result<i64> {
        self.add_comment_inner(
            db,
            display_id,
            &CommentParams {
                content: content.to_string(),
                kind: super::core::KIND_INTERVENTION.to_string(),
                trigger_type: Some(trigger_type.to_string()),
                intervention_context: intervention_context.map(std::string::ToString::to_string),
                driver_key_fingerprint: driver_key_fingerprint
                    .map(std::string::ToString::to_string),
            },
            &format!("intervention on issue #{display_id}"),
        )
    }

    pub fn add_label(&self, db: &Database, display_id: i64, label: &str) -> Result<bool> {
        let label_owned = label.to_string();

        let current = self.load_issue_by_id(display_id, db)?;
        if current.labels.contains(&label_owned) {
            return Ok(false);
        }

        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(display_id, db)?;
                let event = crate::events::Event::LabelAdded {
                    issue_uuid: issue.uuid,
                    label: label_owned.clone(),
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("label issue #{display_id} with {label}"),
        )?;
        Ok(true)
    }

    pub fn remove_label(&self, db: &Database, display_id: i64, label: &str) -> Result<bool> {
        let label_owned = label.to_string();

        let current = self.load_issue_by_id(display_id, db)?;
        if !current.labels.contains(&label_owned) {
            return Ok(false);
        }

        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(display_id, db)?;
                let event = crate::events::Event::LabelRemoved {
                    issue_uuid: issue.uuid,
                    label: label_owned.clone(),
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("unlabel {label} from issue #{display_id}"),
        )?;
        Ok(true)
    }

    pub fn add_blocker(
        &self,
        db: &Database,
        issue_id: i64,
        blocking_issue_id: i64,
    ) -> Result<bool> {
        let blocker_uuid = self.resolve_uuid(blocking_issue_id, db)?;

        let current = self.load_issue_by_id(issue_id, db)?;
        if current.blockers.contains(&blocker_uuid) {
            return Ok(false);
        }

        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(issue_id, db)?;

                let event = crate::events::Event::DependencyAdded {
                    blocked_uuid: issue.uuid,
                    blocker_uuid,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("block issue #{issue_id} on #{blocking_issue_id}"),
        )?;
        Ok(true)
    }

    pub fn remove_blocker(
        &self,
        db: &Database,
        issue_id: i64,
        blocking_issue_id: i64,
    ) -> Result<bool> {
        let blocker_uuid = self.resolve_uuid(blocking_issue_id, db)?;

        let current = self.load_issue_by_id(issue_id, db)?;
        if !current.blockers.contains(&blocker_uuid) {
            return Ok(false);
        }

        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(issue_id, db)?;
                let event = crate::events::Event::DependencyRemoved {
                    blocked_uuid: issue.uuid,
                    blocker_uuid,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("unblock issue #{issue_id} from #{blocking_issue_id}"),
        )?;
        Ok(true)
    }

    pub fn add_relation(&self, db: &Database, issue_id: i64, related_id: i64) -> Result<bool> {
        let related_uuid = self.resolve_uuid(related_id, db)?;

        let current = self.load_issue_by_id(issue_id, db)?;
        if current.related.contains(&related_uuid) {
            return Ok(false);
        }

        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(issue_id, db)?;
                let event = crate::events::Event::RelationAdded {
                    uuid_a: issue.uuid,
                    uuid_b: related_uuid,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("relate issue #{issue_id} to #{related_id}"),
        )?;
        Ok(true)
    }

    pub fn remove_relation(&self, db: &Database, issue_id: i64, related_id: i64) -> Result<bool> {
        let related_uuid = self.resolve_uuid(related_id, db)?;

        let current = self.load_issue_by_id(issue_id, db)?;
        if !current.related.contains(&related_uuid) {
            return Ok(false);
        }

        let _ = self.write_commit_push(
            db,
            |writer| {
                let issue = writer.load_issue_by_id(issue_id, db)?;
                let event = crate::events::Event::RelationRemoved {
                    uuid_a: issue.uuid,
                    uuid_b: related_uuid,
                };
                Ok(WriteSet {
                    events: vec![event],
                })
            },
            &format!("unrelate issue #{issue_id} from #{related_id}"),
        )?;
        Ok(true)
    }
}
