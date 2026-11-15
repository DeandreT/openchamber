//! The server's shared session folders (`/api/session-folders`): a read
//! of the stored copy and a `PATCH` of user changes, both answered with the
//! stored copy and its revision (`useSessionFoldersStore.ts`).

use openchamber_native_core::folders::{FolderChange, FoldersMap, SessionFolder};
use reqwest::{Method, StatusCode};
use serde_json::{Map, Value, json};

use crate::client::ServerClient;
use crate::error::TransportError;

/// The stored folders and their revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSnapshot {
    pub folders: FoldersMap,
    pub updated_at: i64,
}

/// What a write established.
#[derive(Debug)]
pub enum FolderWrite {
    Stored(FolderSnapshot),
    /// The server cannot store these changes (`400`, `413`); they are
    /// dropped and the stored copy read again.
    Rejected,
}

fn change_json(change: &FolderChange) -> Value {
    match change {
        FolderChange::Create { scope, folder } => json!({
            "type": "create",
            "scopeKey": scope,
            "folder": {
                "id": folder.id,
                "name": folder.name,
                "parentId": folder.parent_id,
                "createdAt": folder.created_at,
            },
        }),
        FolderChange::Rename {
            scope,
            folder_id,
            name,
        } => json!({ "type": "rename", "scopeKey": scope, "folderId": folder_id, "name": name }),
        FolderChange::Delete { scope, folder_ids } => {
            json!({ "type": "delete", "scopeKey": scope, "folderIds": folder_ids })
        }
        FolderChange::Assign {
            scope,
            folder_id,
            session_ids,
        } => json!({
            "type": "assign",
            "scopeKey": scope,
            "folderId": folder_id,
            "sessionIds": session_ids,
        }),
        FolderChange::Unassign { scope, session_ids } => {
            json!({ "type": "unassign", "scopeKey": scope, "sessionIds": session_ids })
        }
        FolderChange::ForgetSessions { session_ids } => {
            json!({ "type": "forget-sessions", "sessionIds": session_ids })
        }
        FolderChange::ForgetScopes { scopes } => {
            json!({ "type": "forget-scopes", "scopeKeys": scopes })
        }
    }
}

/// `folderSnapshotSchema`: folders need a trimmed ID and name; blank
/// session IDs are dropped and an odd creation time reads as 0.
fn decode_snapshot(body: &Value) -> Option<FolderSnapshot> {
    let object = body.as_object()?;
    let updated_at = match object.get("updatedAt") {
        None => 0,
        Some(value) => {
            let number = value.as_f64().filter(|number| number.is_finite())?;
            number as i64
        }
    };
    let mut folders = FoldersMap::new();
    let empty = Map::new();
    let stored = match object.get("foldersMap") {
        None => &empty,
        Some(value) => value.as_object()?,
    };
    for (scope, list) in stored {
        let mut scope_folders = Vec::new();
        for entry in list.as_array()? {
            let entry = entry.as_object()?;
            let text = |key: &str| {
                entry
                    .get(key)?
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
            };
            let (Some(id), Some(name)) = (text("id"), text("name")) else {
                return None;
            };
            let session_ids = entry
                .get("sessionIds")?
                .as_array()?
                .iter()
                .map(|id| id.as_str().map(str::to_owned))
                .collect::<Option<Vec<String>>>()?
                .into_iter()
                .filter(|id| !id.trim().is_empty())
                .collect();
            let parent_id = match entry.get("parentId") {
                None | Some(Value::Null) => None,
                Some(Value::String(parent)) => Some(parent.clone()),
                Some(_) => return None,
            };
            scope_folders.push(SessionFolder {
                id: id.to_owned(),
                name: name.to_owned(),
                session_ids,
                created_at: entry
                    .get("createdAt")
                    .and_then(Value::as_f64)
                    .map_or(0, |time| time as i64),
                parent_id,
            });
        }
        folders.insert(scope.clone(), scope_folders);
    }
    Some(FolderSnapshot {
        folders,
        updated_at,
    })
}

fn malformed() -> TransportError {
    TransportError::Malformed("unrecognized session folders snapshot".to_owned())
}

async fn snapshot_body(response: reqwest::Response) -> Result<FolderSnapshot, TransportError> {
    let body: Value = response.json().await.map_err(|_| malformed())?;
    decode_snapshot(&body).ok_or_else(malformed)
}

fn check_status(status: StatusCode) -> Result<(), TransportError> {
    if status.is_redirection() {
        return Err(TransportError::Redirected);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(TransportError::AuthenticationRequired);
    }
    if !status.is_success() {
        return Err(TransportError::Http {
            status: status.as_u16(),
        });
    }
    Ok(())
}

impl ServerClient {
    /// The stored folders.
    pub async fn session_folders(&self) -> Result<FolderSnapshot, TransportError> {
        let response = self
            .authorized(Method::GET, self.endpoint().url("api/session-folders"))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        check_status(response.status())?;
        snapshot_body(response).await
    }

    /// Sends changes in order; the server applies them to its copy.
    pub async fn change_session_folders(
        &self,
        changes: &[FolderChange],
    ) -> Result<FolderWrite, TransportError> {
        let body: Vec<Value> = changes.iter().map(change_json).collect();
        let response = self
            .authorized(Method::PATCH, self.endpoint().url("api/session-folders"))
            .json(&json!({ "changes": body }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let status = response.status();
        if matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
        ) {
            return Ok(FolderWrite::Rejected);
        }
        check_status(status)?;
        Ok(FolderWrite::Stored(snapshot_body(response).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchamber_native_core::folders::NewFolder;

    #[test]
    fn changes_use_the_web_wire_names() {
        let created = change_json(&FolderChange::Create {
            scope: "/p".into(),
            folder: NewFolder {
                id: "f".into(),
                name: "Docs".into(),
                parent_id: None,
                created_at: 5,
            },
        });
        assert_eq!(
            created,
            json!({"type":"create","scopeKey":"/p","folder":{"id":"f","name":"Docs","parentId":null,"createdAt":5}})
        );
        assert_eq!(
            change_json(&FolderChange::ForgetScopes {
                scopes: vec!["/p".into()]
            }),
            json!({"type":"forget-scopes","scopeKeys":["/p"]})
        );
    }

    #[test]
    fn snapshots_keep_valid_folders_and_reject_broken_ones() {
        let snapshot = decode_snapshot(&json!({
            "foldersMap": {"/p": [{"id": " f ", "name": "Docs", "sessionIds": ["a", " "], "createdAt": "x"}]},
            "updatedAt": 7
        }))
        .expect("valid");
        let folder = &snapshot.folders["/p"][0];
        assert_eq!(folder.id, "f");
        assert_eq!(folder.session_ids, ["a"]);
        assert_eq!(folder.created_at, 0);
        assert_eq!(snapshot.updated_at, 7);
        assert_eq!(
            decode_snapshot(&json!({})).map(|snapshot| snapshot.updated_at),
            Some(0),
            "a server with no folders file answers an empty copy"
        );
        assert!(
            decode_snapshot(
                &json!({"foldersMap": {"/p": [{"id": "", "name": "x", "sessionIds": []}]}})
            )
            .is_none()
        );
    }
}
