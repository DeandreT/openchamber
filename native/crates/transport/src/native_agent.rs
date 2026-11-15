//! Strict adapter for SDK native-agent v1 over OpenCode's authenticated RPC.
//! The installed client sends `{input}` and reads `{output}` without a location
//! response envelope. Provider entry and answer policy are enforced by the plugin.

use openchamber_native_core::native_agent::*;
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;

const MAX_BODY: usize = 8 * 1024 * 1024;
type Parsed<T> = Result<T, TransportError>;
fn malformed() -> TransportError {
    TransportError::Malformed("invalid native-agent v1 payload".into())
}

/// Constructed only from the explicit strict v1 provider settings marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeAgentProvider {
    provider_id: String,
}
impl NativeAgentProvider {
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

pub(crate) fn discovery(
    provider_id: &str,
    settings: Option<&Value>,
) -> Option<NativeAgentProvider> {
    let marker = settings?.pointer("/openchamber/nativeAgent")?.as_object()?;
    (valid_id(provider_id)
        && marker.len() == 1
        && marker
            .get("protocolVersion")
            .is_some_and(|v| number(v).ok() == Some(1)))
    .then(|| NativeAgentProvider {
        provider_id: provider_id.to_owned(),
    })
}

/// Captures the authenticated server client and exact directory/provider/session.
#[derive(Clone, Debug)]
pub struct NativeAgentApi {
    client: ServerClient,
    key: OwnerKey,
}
impl NativeAgentApi {
    pub fn new(
        client: ServerClient,
        key: OwnerKey,
        provider: &NativeAgentProvider,
    ) -> Parsed<Self> {
        if !valid_scope(&key.scope)
            || key.server.is_empty()
            || key.scope.provider_id != provider.provider_id
        {
            return Err(malformed());
        }
        Ok(Self { client, key })
    }
    pub fn key(&self) -> &OwnerKey {
        &self.key
    }
    fn request(&self, ticket: &Ticket) -> Parsed<crate::request::RequestBuilder> {
        if ticket.key != self.key {
            return Err(malformed());
        }
        let input = parameters(ticket)?;
        let rpc_id = format!("openchamber.native-agent.{}.v1", self.key.scope.provider_id);
        let mut url = self.client.opencode_url("rpc");
        url.path_segments_mut()
            .map_err(|_| malformed())?
            .push(&rpc_id)
            .push(ticket.call.method());
        Ok(self
            .client
            .authorized(Method::POST, url)
            .header(
                "x-opencode-directory",
                encode_directory(&self.key.scope.directory),
            )
            .json(&json!({"input": input})))
    }
    pub async fn call(&self, ticket: &Ticket) -> Parsed<Reply> {
        let mut response = self
            .request(ticket)?
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(TransportError::Redirected);
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(TransportError::AuthenticationRequired);
        }
        if status.as_u16() != 200 {
            return Err(TransportError::Http {
                status: status.as_u16(),
            });
        }
        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"))
            || response
                .content_length()
                .is_some_and(|n| n > MAX_BODY as u64)
        {
            return Err(malformed());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(TransportError::from_request)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
                return Err(malformed());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
        let envelope = object(&body, &["output"], &[])?;
        let reply = parse_reply(&envelope["output"])?;
        if reply.request_id() != ticket.request_id
            || reply.state().is_some_and(|s| s.scope != self.key.scope)
        {
            return Err(malformed());
        }
        Ok(reply)
    }
}

fn object<'a>(
    v: &'a Value,
    required: &[&str],
    optional: &[&str],
) -> Parsed<&'a Map<String, Value>> {
    let o = v.as_object().ok_or_else(malformed)?;
    if required.iter().any(|k| !o.contains_key(*k))
        || o.keys()
            .any(|k| !required.contains(&k.as_str()) && !optional.contains(&k.as_str()))
    {
        return Err(malformed());
    }
    Ok(o)
}
fn string(v: &Value, min: usize, max: usize) -> Parsed<String> {
    let s = v.as_str().ok_or_else(malformed)?;
    if !(min..=max).contains(&s.encode_utf16().count()) {
        return Err(malformed());
    }
    Ok(s.to_owned())
}
fn id(v: &Value) -> Parsed<String> {
    let s = string(v, 1, 256)?;
    if !valid_id(&s) {
        return Err(malformed());
    }
    Ok(s)
}
fn boolean(v: &Value) -> Parsed<bool> {
    v.as_bool().ok_or_else(malformed)
}
fn number(v: &Value) -> Parsed<u64> {
    let n = v.as_f64().ok_or_else(malformed)?;
    if !n.is_finite() || n < 0.0 || n > MAX_SAFE_INTEGER as f64 || n.fract() != 0.0 {
        return Err(malformed());
    }
    Ok(n as u64)
}
fn version(v: &Value) -> Parsed<()> {
    if number(v)? == 1 {
        Ok(())
    } else {
        Err(malformed())
    }
}
fn array(v: &Value, min: usize, max: usize) -> Parsed<&[Value]> {
    let values = v.as_array().ok_or_else(malformed)?;
    if !(min..=max).contains(&values.len()) {
        return Err(malformed());
    }
    Ok(values)
}
fn unique<T>(values: &[T], key: impl Fn(&T) -> &str) -> Parsed<()> {
    let mut seen = std::collections::BTreeSet::new();
    if values.iter().all(|v| seen.insert(key(v))) {
        Ok(())
    } else {
        Err(malformed())
    }
}
fn parse_scope(v: &Value) -> Parsed<Scope> {
    let o = object(v, &["directory", "providerID", "sessionID"], &[])?;
    let scope = Scope {
        directory: string(&o["directory"], 1, 4096)?,
        provider_id: id(&o["providerID"])?,
        session_id: if o["sessionID"].is_null() {
            None
        } else {
            Some(id(&o["sessionID"])?)
        },
    };
    if !valid_scope(&scope) {
        return Err(malformed());
    }
    Ok(scope)
}
fn choices(v: &Value) -> Parsed<Vec<Choice>> {
    let values = array(v, 0, 256)?
        .iter()
        .map(|v| {
            let o = object(v, &["id", "label"], &[])?;
            Ok(Choice {
                id: id(&o["id"])?,
                label: string(&o["label"], 1, 512)?,
            })
        })
        .collect::<Parsed<Vec<_>>>()?;
    unique(&values, |v| &v.id)?;
    Ok(values)
}
fn config(v: &Value) -> Parsed<Config> {
    let o = object(v, &["id", "label", "role", "value", "choices"], &[])?;
    let config = Config {
        id: id(&o["id"])?,
        label: string(&o["label"], 1, 512)?,
        value: id(&o["value"])?,
        choices: choices(&o["choices"])?,
        role: match o["role"].as_str() {
            Some("model") => ConfigRole::Model,
            Some("effort") => ConfigRole::Effort,
            Some("mode") => ConfigRole::Mode,
            Some("other") => ConfigRole::Other,
            _ => return Err(malformed()),
        },
    };
    if !config.choices.iter().any(|c| c.id == config.value) {
        return Err(malformed());
    }
    Ok(config)
}
fn request(v: &Value) -> Parsed<PendingRequest> {
    let kind = match v.get("kind").and_then(Value::as_str) {
        Some("permission") => {
            let o = object(v, &["id", "title", "description", "kind", "choices"], &[])?;
            let choices = array(&o["choices"], 1, 16)?
                .iter()
                .map(|v| {
                    let o = object(v, &["id", "label", "decision"], &[])?;
                    Ok(PermissionChoice {
                        id: id(&o["id"])?,
                        label: string(&o["label"], 1, 512)?,
                        decision: match o["decision"].as_str() {
                            Some("allow") => Decision::Allow,
                            Some("deny") => Decision::Deny,
                            _ => return Err(malformed()),
                        },
                    })
                })
                .collect::<Parsed<Vec<_>>>()?;
            unique(&choices, |c| &c.id)?;
            RequestKind::Permission { choices }
        }
        Some("question") => {
            let o = object(v, &["id", "title", "description", "kind", "fields"], &[])?;
            let fields = array(&o["fields"], 1, 32)?
                .iter()
                .map(|v| {
                    let o = object(
                        v,
                        &[
                            "id",
                            "label",
                            "required",
                            "multiple",
                            "allowText",
                            "choices",
                        ],
                        &[],
                    )?;
                    let field = QuestionField {
                        id: id(&o["id"])?,
                        label: string(&o["label"], 1, 512)?,
                        required: boolean(&o["required"])?,
                        multiple: boolean(&o["multiple"])?,
                        allow_text: boolean(&o["allowText"])?,
                        choices: choices(&o["choices"])?,
                    };
                    if !field.allow_text && field.choices.is_empty() {
                        return Err(malformed());
                    }
                    Ok(field)
                })
                .collect::<Parsed<Vec<_>>>()?;
            unique(&fields, |f| &f.id)?;
            RequestKind::Question { fields }
        }
        _ => return Err(malformed()),
    };
    Ok(PendingRequest {
        id: id(&v["id"])?,
        title: string(&v["title"], 1, 512)?,
        description: string(&v["description"], 0, 16_384)?,
        kind,
    })
}
fn error_code(v: &Value) -> Parsed<ErrorCode> {
    Ok(match v.as_str() {
        Some("invalid_request") => ErrorCode::InvalidRequest,
        Some("stale_activation") => ErrorCode::StaleActivation,
        Some("stale_revision") => ErrorCode::StaleRevision,
        Some("not_prepared") => ErrorCode::NotPrepared,
        Some("authentication_required") => ErrorCode::AuthenticationRequired,
        Some("unsupported") => ErrorCode::Unsupported,
        Some("policy_denied") => ErrorCode::PolicyDenied,
        Some("request_not_found") => ErrorCode::RequestNotFound,
        Some("invalid_answer") => ErrorCode::InvalidAnswer,
        Some("invalid_config") => ErrorCode::InvalidConfig,
        Some("unavailable") => ErrorCode::Unavailable,
        Some("outcome_unknown") => ErrorCode::OutcomeUnknown,
        _ => return Err(malformed()),
    })
}
fn parse_state(v: &Value) -> Parsed<State> {
    let o = object(
        v,
        &[
            "protocolVersion",
            "scope",
            "activationID",
            "revision",
            "status",
            "authentication",
            "permissionOwner",
            "capabilities",
            "config",
            "requests",
            "submission",
            "diagnostic",
            "execution",
        ],
        &["configComplete"],
    )?;
    version(&o["protocolVersion"])?;
    let c = object(
        &o["capabilities"],
        &["authenticate", "setConfig", "answer", "toolProgress"],
        &["prepareAfterSession"],
    )?;
    let config = array(&o["config"], 0, 32)?
        .iter()
        .map(config)
        .collect::<Parsed<Vec<_>>>()?;
    unique(&config, |c| &c.id)?;
    let requests = array(&o["requests"], 0, 64)?
        .iter()
        .map(request)
        .collect::<Parsed<Vec<_>>>()?;
    unique(&requests, |r| &r.id)?;
    let permission_owner = match o["permissionOwner"].as_str() {
        Some("opencode") => PermissionOwner::OpenCode,
        Some("native") => PermissionOwner::Native,
        _ => return Err(malformed()),
    };
    if permission_owner == PermissionOwner::OpenCode && !requests.is_empty() {
        return Err(malformed());
    }
    let submission = match o["submission"].get("status").and_then(Value::as_str) {
        Some("none") => {
            object(&o["submission"], &["status"], &[])?;
            Submission::None
        }
        Some(status @ ("pending" | "outcome-unknown")) => {
            let s = object(&o["submission"], &["status", "id"], &[])?;
            if status == "pending" {
                Submission::Pending(id(&s["id"])?)
            } else {
                Submission::OutcomeUnknown(id(&s["id"])?)
            }
        }
        _ => return Err(malformed()),
    };
    let diagnostic = if o["diagnostic"].is_null() {
        None
    } else {
        let d = object(&o["diagnostic"], &["code", "retryable"], &[])?;
        Some(Diagnostic {
            code: error_code(&d["code"])?,
            retryable: boolean(&d["retryable"])?,
        })
    };
    let scope = parse_scope(&o["scope"])?;
    let execution = if o["execution"].is_null() {
        None
    } else {
        let e = object(&o["execution"], &["providerID", "modelID", "variant"], &[])?;
        let binding = ExecutionBinding {
            provider_id: id(&e["providerID"])?,
            model_id: id(&e["modelID"])?,
            variant: id(&e["variant"])?,
        };
        if binding.provider_id != scope.provider_id {
            return Err(malformed());
        }
        Some(binding)
    };
    let prepare_after_session = c
        .get("prepareAfterSession")
        .map(boolean)
        .transpose()?
        .unwrap_or(false);
    let config_complete = o
        .get("configComplete")
        .map(boolean)
        .transpose()?
        .unwrap_or(false);
    if prepare_after_session && execution.is_some() && !config_complete {
        return Err(malformed());
    }
    if prepare_after_session
        && scope.session_id.is_none()
        && (config_complete
            || o["status"] != "unprepared"
            || execution.is_some()
            || !requests.is_empty()
            || !matches!(submission, Submission::None))
    {
        return Err(malformed());
    }
    Ok(State {
        scope,
        execution,
        activation_id: id(&o["activationID"])?,
        revision: number(&o["revision"])?,
        status: match o["status"].as_str() {
            Some("unprepared") => Status::Unprepared,
            Some("preparing") => Status::Preparing,
            Some("ready") => Status::Ready,
            Some("stopped") => Status::Stopped,
            Some("error") => Status::Error,
            _ => return Err(malformed()),
        },
        authentication: match o["authentication"].as_str() {
            Some("unknown") => Authentication::Unknown,
            Some("required") => Authentication::Required,
            Some("authenticated") => Authentication::Authenticated,
            _ => return Err(malformed()),
        },
        permission_owner,
        capabilities: Capabilities {
            prepare_after_session,
            authenticate: boolean(&c["authenticate"])?,
            set_config: boolean(&c["setConfig"])?,
            answer: boolean(&c["answer"])?,
            tool_progress: boolean(&c["toolProgress"])?,
        },
        config_complete,
        config,
        requests,
        submission,
        diagnostic,
    })
}
fn parse_reply(v: &Value) -> Parsed<Reply> {
    match v.get("ok").and_then(Value::as_bool) {
        Some(true) => {
            let o = object(v, &["protocolVersion", "requestID", "ok", "state"], &[])?;
            version(&o["protocolVersion"])?;
            Ok(Reply::Success {
                request_id: id(&o["requestID"])?,
                state: parse_state(&o["state"])?,
            })
        }
        Some(false) => {
            let o = object(
                v,
                &["protocolVersion", "requestID", "ok", "error"],
                &["state"],
            )?;
            version(&o["protocolVersion"])?;
            let e = object(&o["error"], &["code", "outcome"], &[])?;
            Ok(Reply::Failure {
                request_id: id(&o["requestID"])?,
                code: error_code(&e["code"])?,
                outcome: match e["outcome"].as_str() {
                    Some("rejected") => Outcome::Rejected,
                    Some("unknown") => Outcome::Unknown,
                    _ => return Err(malformed()),
                },
                state: o.get("state").map(parse_state).transpose()?,
            })
        }
        _ => Err(malformed()),
    }
}
fn parse_event(v: &Value) -> Parsed<AgentEvent> {
    match v.get("type").and_then(Value::as_str) {
        Some("stateChanged") => {
            let o = object(v, &["protocolVersion", "type", "state"], &[])?;
            version(&o["protocolVersion"])?;
            Ok(AgentEvent::StateChanged(parse_state(&o["state"])?))
        }
        Some("toolProgress") => {
            let o = object(
                v,
                &[
                    "protocolVersion",
                    "type",
                    "scope",
                    "activationID",
                    "revision",
                    "messageID",
                    "partID",
                    "callID",
                    "sequence",
                    "text",
                ],
                &[],
            )?;
            version(&o["protocolVersion"])?;
            Ok(AgentEvent::ToolProgress(ToolProgress {
                scope: parse_scope(&o["scope"])?,
                activation_id: id(&o["activationID"])?,
                revision: number(&o["revision"])?,
                tool: ToolIdentity {
                    message_id: id(&o["messageID"])?,
                    part_id: id(&o["partID"])?,
                    call_id: id(&o["callID"])?,
                },
                sequence: number(&o["sequence"])?,
                text: string(&o["text"], 0, 16_384)?,
            }))
        }
        _ => Err(malformed()),
    }
}

/// The outer OpenCode RPC namespace and location are checked independently of
/// the inner SDK schema. A provider ID may itself contain dots.
pub(crate) fn published(kind: &str, payload: &Value) -> Parsed<AgentEvent> {
    let event = parse_event(payload.get("data").ok_or_else(malformed)?)?;
    let (scope, name) = match &event {
        AgentEvent::StateChanged(s) => (&s.scope, "stateChanged"),
        AgentEvent::ToolProgress(p) => (&p.scope, "toolProgress"),
    };
    if kind
        != format!(
            "rpc.openchamber.native-agent.{}.v1.{name}",
            scope.provider_id
        )
        || payload
            .pointer("/location/directory")
            .and_then(Value::as_str)
            != Some(scope.directory.as_str())
    {
        return Err(malformed());
    }
    Ok(event)
}

fn scope_json(scope: &Scope) -> Value {
    json!({"directory":scope.directory,"providerID":scope.provider_id,"sessionID":scope.session_id})
}
fn answer_json(answer: &Answer) -> Value {
    match answer {
        Answer::Permission {
            request_id,
            choice_id,
        } => json!({"kind":"permission","requestID":request_id,"choiceID":choice_id}),
        Answer::Question { request_id, values } => {
            json!({"kind":"question","requestID":request_id,"values":values.iter().map(|v| {
            let mut value = json!({"fieldID":v.field_id,"choiceIDs":v.choice_ids});
            if let Some(text) = &v.text { value["text"] = json!(text); }
            value
        }).collect::<Vec<_>>()})
        }
    }
}
fn parameters(ticket: &Ticket) -> Parsed<Value> {
    if !valid_scope(&ticket.key.scope)
        || !valid_id(&ticket.request_id)
        || ticket
            .activation_id
            .as_deref()
            .is_some_and(|s| !valid_id(s))
    {
        return Err(malformed());
    }
    let mut v = json!({"protocolVersion":1,"requestID":ticket.request_id,"scope":scope_json(&ticket.key.scope),"activationID":ticket.activation_id});
    if ticket.call.is_mutation() {
        let revision = ticket
            .expected_revision
            .filter(|r| *r <= MAX_SAFE_INTEGER)
            .ok_or_else(malformed)?;
        if ticket.activation_id.is_none() {
            return Err(malformed());
        }
        v["expectedRevision"] = json!(revision);
    }
    match &ticket.call {
        Call::Authenticate => v["userInitiated"] = json!(true),
        Call::SetConfig(changes) => {
            v["changes"] = json!(
                changes
                    .iter()
                    .map(|c| json!({"id":c.id,"value":c.value}))
                    .collect::<Vec<_>>()
            )
        }
        Call::Answer(answer) => v["answer"] = answer_json(answer),
        _ => {}
    }
    Ok(v)
}

#[cfg(test)]
mod tests;
