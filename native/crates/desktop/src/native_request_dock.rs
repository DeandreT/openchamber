//! Native approval drafts retain their complete provider/session/activation owner.

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div, px,
};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::native_agent::{
    Answer, Authentication, Call, Decision, ErrorCode, OwnerKey, PendingRequest, PermissionOwner,
    QuestionField, QuestionValue, RequestKind, Status, validate_answer,
};
use openchamber_native_core::pending_requests::RequestId;
use openchamber_native_core::permission::PermissionReply;

use crate::baseline::CssTextExt;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::request_dock::{Control, button};
use crate::theme::Palette;
use crate::workspace::Workspace;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RequestKey {
    pub owner: OwnerKey,
    pub activation_id: String,
    pub request_id: String,
}

#[derive(Default)]
struct QuestionDraft {
    choices: BTreeMap<String, Vec<String>>,
    inputs: BTreeMap<String, Entity<TextareaState>>,
    step: usize,
    show_errors: bool,
}

pub(crate) struct NativeRequests {
    drafts: BTreeMap<RequestKey, QuestionDraft>,
    selected: Option<RequestKey>,
    collapsed: bool,
    pub dirty: bool,
}
impl Default for NativeRequests {
    fn default() -> Self {
        Self {
            drafts: BTreeMap::new(),
            selected: None,
            collapsed: false,
            dirty: true,
        }
    }
}

fn request_identity(key: &RequestKey) -> String {
    format!(
        "native-request:{}:{}:{}:{}:{}",
        key.owner.scope.directory,
        key.owner.scope.provider_id,
        key.owner.scope.session_id.as_deref().unwrap_or("draft"),
        key.activation_id,
        key.request_id
    )
}

impl Workspace {
    pub(crate) fn render_native_status(
        &self,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let model = self.catalog_selection()?.1.model?;
        if !self.native_provider_advertised(&model.provider_id) {
            return None;
        }
        let key = self.native_owner_key(&model.provider_id)?;
        let state = self.native_agents.owner.state(&key);
        let failure = self.native_agents.owner.failure(&key);
        let uncertain = self.native_agents.owner.submission_uncertain(&key);
        let needs_auth =
            state.is_some_and(|state| state.authentication == Authentication::Required);
        let enterprise = self.enterprise.enterprise_mode();
        let message = if uncertain {
            Some("chat.nativeAgent.error.outcomeUnknown")
        } else if let Some(code) = failure {
            Some(error_key(code))
        } else if needs_auth {
            Some(if enterprise {
                "chat.nativeAgent.error.policyDenied"
            } else {
                "chat.nativeAgent.error.authenticationRequired"
            })
        } else if state.is_none_or(|state| {
            matches!(state.status, Status::Unprepared | Status::Preparing)
                || state.authentication == Authentication::Unknown
        }) {
            Some("chat.nativeAgent.preparing")
        } else if let Some(diagnostic) = state.and_then(|state| state.diagnostic.as_ref()) {
            Some(error_key(diagnostic.code))
        } else if state.is_some_and(|state| matches!(state.status, Status::Stopped | Status::Error))
        {
            Some("chat.nativeAgent.error.notPrepared")
        } else if state.is_some_and(|state| state.execution.is_none()) {
            Some("chat.nativeAgent.executionUnavailable")
        } else {
            None
        };
        let strings = locale::strings(cx).clone();
        let mut row = div()
            .id("native-agent-status")
            .accessibility_id("native-agent-status")
            .test_support()
            .min_h(px(28.))
            .px(px(12.))
            .flex()
            .flex_row_from(strings.direction())
            .items_center()
            .gap(px(8.))
            .text_color(palette.muted_foreground)
            .css_text(0.75, 1.0625, cx);
        if let Some(message) = message {
            let message = strings.get(message);
            row = row
                .role(Role::Status)
                .aria_label(message.clone())
                .child(div().flex_1().min_w_0().truncate().child(message));
        }
        if needs_auth {
            let enabled = !enterprise
                && self.native_agents.owner.can_authenticate(&key)
                && !self.native_action_pending(&key);
            let owner = key.clone();
            row = row.child(button(
                "native-agent-authenticate".into(),
                strings.get("chat.nativeAgent.signIn"),
                enabled,
                Control::Raised,
                palette,
                (window, cx),
                move |this, _, cx| {
                    this.native_call(owner.clone(), Call::Authenticate, false, cx);
                },
            ));
        }
        if message.is_some() {
            row = row.child(button(
                "native-agent-check".into(),
                strings.get("chat.nativeRecovery.check"),
                !self.native_action_pending(&key),
                Control::Ghost,
                palette,
                (window, cx),
                move |this, _, cx| {
                    this.native_call(key.clone(), Call::Prepare, true, cx);
                },
            ));
        }
        Some(row.into_any_element())
    }

    fn native_request_visible(&self, key: &OwnerKey) -> bool {
        if key.server != self.runtime.as_str() {
            return false;
        }
        let Some(owner) = key
            .scope
            .session_id
            .as_ref()
            .map(|id| SessionId::from(id.as_str()))
        else {
            return self.selected.is_none()
                && self.native_owner_key(&key.scope.provider_id).as_ref() == Some(key);
        };
        if self
            .sessions
            .get(&owner)
            .and_then(|record| record.resolved_directory())
            .as_deref()
            != Some(key.scope.directory.as_str())
        {
            return false;
        }
        let mut current = Some(owner);
        let mut visited = BTreeSet::new();
        while let Some(id) = current {
            if self.selected.as_ref() == Some(&id) {
                return true;
            }
            if !visited.insert(id.clone()) {
                break;
            }
            current = self
                .sessions
                .get(&id)
                .and_then(|record| record.parent_id.clone());
        }
        false
    }

    fn visible_native_requests(&self) -> Vec<(RequestKey, PendingRequest, u64)> {
        let mut requests: Vec<_> = self
            .native_agents
            .owner
            .states()
            .filter(|(key, state)| {
                state.permission_owner == PermissionOwner::Native
                    && self.native_request_visible(key)
            })
            .flat_map(|(owner, state)| {
                state.requests.iter().map(|request| {
                    (
                        RequestKey {
                            owner: owner.clone(),
                            activation_id: state.activation_id.clone(),
                            request_id: request.id.clone(),
                        },
                        request.clone(),
                        state.revision,
                    )
                })
            })
            .collect();
        requests
            .sort_by_key(|(_, request, _)| matches!(request.kind, RequestKind::Question { .. }));
        requests
    }

    fn active_native_request(&self) -> Option<(RequestKey, PendingRequest, u64)> {
        let requests = self.visible_native_requests();
        let permission = matches!(requests.first()?.1.kind, RequestKind::Permission { .. });
        requests
            .iter()
            .find(|(key, request, _)| {
                self.native_requests.selected.as_ref() == Some(key)
                    && matches!(request.kind, RequestKind::Permission { .. }) == permission
            })
            .cloned()
            .or_else(|| requests.into_iter().next())
    }

    pub(crate) fn native_dock_precedes_ordinary(&self) -> bool {
        let Some((key, request, _)) = self.active_native_request() else {
            return false;
        };
        self.active_request().is_none_or(|ordinary| {
            let native_permission = matches!(request.kind, RequestKind::Permission { .. });
            let ordinary_permission = matches!(ordinary.id, RequestId::Permission(_));
            (!ordinary_permission && native_permission)
                || (native_permission == ordinary_permission
                    && self.native_requests.selected.as_ref() == Some(&key))
        })
    }

    pub(crate) fn native_dock_visible(&self) -> bool {
        self.active_native_request().is_some()
    }

    pub(crate) fn native_request_tabs(
        &self,
        permission: bool,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.visible_native_requests()
            .into_iter()
            .filter(|(_, request, _)| {
                matches!(request.kind, RequestKind::Permission { .. }) == permission
            })
            .map(|(key, request, _)| {
                button(
                    format!("{}:select", request_identity(&key)),
                    format!("{}: {}", key.owner.scope.provider_id, request.title).into(),
                    true,
                    Control::Tab(false),
                    palette,
                    (window, cx),
                    move |this, _, cx| {
                        this.native_requests.selected = Some(key.clone());
                        cx.notify();
                    },
                )
            })
            .collect()
    }

    pub(crate) fn sync_native_request_drafts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.native_requests.dirty {
            return;
        }
        self.native_requests.dirty = false;
        let questions: Vec<_> = self
            .native_agents
            .owner
            .states()
            .flat_map(|(owner, state)| {
                state.requests.iter().filter_map(|request| {
                    let RequestKind::Question { fields } = &request.kind else {
                        return None;
                    };
                    Some((
                        RequestKey {
                            owner: owner.clone(),
                            activation_id: state.activation_id.clone(),
                            request_id: request.id.clone(),
                        },
                        fields.clone(),
                    ))
                })
            })
            .collect();
        let current: BTreeSet<_> = questions.iter().map(|(key, _)| key.clone()).collect();
        self.native_requests
            .drafts
            .retain(|key, _| current.contains(key));
        for (key, fields) in questions {
            let mut draft = self.native_requests.drafts.remove(&key).unwrap_or_default();
            draft
                .choices
                .retain(|id, _| fields.iter().any(|field| &field.id == id));
            draft.inputs.retain(|id, _| {
                fields
                    .iter()
                    .any(|field| &field.id == id && field.allow_text)
            });
            for field in &fields {
                if let Some(choices) = draft.choices.get_mut(&field.id) {
                    choices.retain(|id| field.choices.iter().any(|choice| &choice.id == id));
                    if !field.multiple {
                        choices.truncate(1);
                    }
                }
                if field.allow_text && !draft.inputs.contains_key(&field.id) {
                    let label = field.label.clone();
                    let input = cx.new(|cx| {
                        TextareaState::new(window, cx)
                            .auto_grow(1, 5)
                            .submit_on_enter(false)
                            .placeholder(label)
                    });
                    draft.inputs.insert(field.id.clone(), input);
                }
            }
            draft.step = draft.step.min(fields.len().saturating_sub(1));
            self.native_requests.drafts.insert(key, draft);
        }
    }

    fn native_question_values(
        &self,
        key: &RequestKey,
        fields: &[QuestionField],
        cx: &App,
    ) -> Vec<QuestionValue> {
        let Some(draft) = self.native_requests.drafts.get(key) else {
            return Vec::new();
        };
        fields
            .iter()
            .filter_map(|field| {
                let choice_ids = draft.choices.get(&field.id).cloned().unwrap_or_default();
                let text = draft
                    .inputs
                    .get(&field.id)
                    .map(|input| input.read(cx).value().to_string())
                    .filter(|text| !text.is_empty());
                (!choice_ids.is_empty() || text.is_some()).then_some(QuestionValue {
                    field_id: field.id.clone(),
                    choice_ids,
                    text,
                })
            })
            .collect()
    }

    fn native_answer_request(
        &mut self,
        key: &RequestKey,
        revision: u64,
        answer: Answer,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.native_agents.owner.state(&key.owner) else {
            return;
        };
        if state.activation_id != key.activation_id || state.revision != revision {
            return;
        }
        let Some(request) = state
            .requests
            .iter()
            .find(|request| request.id == key.request_id)
        else {
            return;
        };
        if !validate_answer(request, &answer) {
            return;
        }
        self.native_call(key.owner.clone(), Call::Answer(answer), false, cx);
    }

    fn advance_native_question(&mut self, key: &RequestKey, revision: u64, cx: &mut Context<Self>) {
        let Some(state) = self.native_agents.owner.state(&key.owner) else {
            return;
        };
        if state.activation_id != key.activation_id || state.revision != revision {
            return;
        }
        let Some(request) = state
            .requests
            .iter()
            .find(|request| request.id == key.request_id)
            .cloned()
        else {
            return;
        };
        let RequestKind::Question { fields } = &request.kind else {
            return;
        };
        let Some(draft) = self.native_requests.drafts.get(key) else {
            return;
        };
        let step = draft.step;
        let current = fields.get(step).cloned().into_iter().collect::<Vec<_>>();
        let answer = Answer::Question {
            request_id: request.id.clone(),
            values: self.native_question_values(key, &current, cx),
        };
        let single = PendingRequest {
            kind: RequestKind::Question { fields: current },
            ..request.clone()
        };
        if !validate_answer(&single, &answer) {
            if let Some(draft) = self.native_requests.drafts.get_mut(key) {
                draft.show_errors = true;
            }
            cx.notify();
            return;
        }
        if step + 1 < fields.len() {
            if let Some(draft) = self.native_requests.drafts.get_mut(key) {
                draft.step += 1;
                draft.show_errors = false;
            }
        } else {
            let answer = Answer::Question {
                request_id: request.id.clone(),
                values: self.native_question_values(key, fields, cx),
            };
            if validate_answer(&request, &answer) {
                self.native_answer_request(key, revision, answer, cx);
            } else if let Some(draft) = self.native_requests.drafts.get_mut(key) {
                draft.show_errors = true;
            }
        }
        cx.notify();
    }

    pub(crate) fn render_native_request_dock(
        &self,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (key, request, revision) = self.active_native_request()?;
        let id = request_identity(&key);
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let permission = matches!(request.kind, RequestKind::Permission { .. });
        let enabled = self
            .native_agents
            .owner
            .can_answer(&key.owner, &key.request_id)
            && !self.native_action_pending(&key.owner);
        let relevant = self.visible_native_requests();
        let mut header = div()
            .flex()
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(6.))
            .min_h(px(32.));
        header = header
            .child(button(
                format!("{id}:collapse"),
                request.title.clone().into(),
                true,
                Control::Details(!self.native_requests.collapsed),
                palette,
                (window, cx),
                |this, _, cx| {
                    this.native_requests.collapsed = !this.native_requests.collapsed;
                    cx.notify();
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(palette.muted_foreground)
                    .child(key.owner.scope.provider_id.clone()),
            );
        header = header.child(button(
            format!("{id}:check"),
            strings.get("chat.nativeRecovery.check"),
            !self.native_action_pending(&key.owner),
            Control::Ghost,
            palette,
            (window, cx),
            {
                let owner = key.owner.clone();
                move |this, _, cx| {
                    this.native_call(owner.clone(), Call::GetState, true, cx);
                }
            },
        ));
        let mut panel = div()
            .id("native-request-dock")
            .accessibility_id("native-request-dock")
            .role(Role::Dialog)
            .aria_label(request.title.clone())
            .test_support()
            .w_full()
            .mb(px(12.))
            .rounded(px(12.))
            .bg(palette.elevated)
            .text_color(palette.elevated_foreground)
            .shadow(crate::elevation::floating(palette))
            .css_text(0.78125, 1.125, cx)
            .flex()
            .flex_col();
        if self.native_requests.collapsed {
            return Some(panel.child(header).into_any_element());
        }
        if relevant.len() > 1 {
            header = header.child(
                div().flex().flex_row_from(direction).gap(px(4.)).children(
                    relevant
                        .into_iter()
                        .filter(|(_, candidate, _)| {
                            matches!(candidate.kind, RequestKind::Permission { .. }) == permission
                        })
                        .map(|(candidate, request, _)| {
                            let selected = candidate == key;
                            button(
                                format!("{}:tab", request_identity(&candidate)),
                                format!("{}: {}", candidate.owner.scope.provider_id, request.title)
                                    .into(),
                                true,
                                Control::Tab(selected),
                                palette,
                                (window, cx),
                                move |this, _, cx| {
                                    this.native_requests.selected = Some(candidate.clone());
                                    cx.notify();
                                },
                            )
                        }),
                ),
            );
        }
        if let Some(ordinary) = self.active_request()
            && matches!(ordinary.id, RequestId::Permission(_)) == permission
        {
            let label = self
                .blocking
                .stores
                .get(&ordinary.scope)
                .and_then(|store| store.get(&ordinary))
                .map(|entry| match &entry.request {
                    openchamber_native_core::pending_requests::PendingRequest::Permission(_) => {
                        strings.get("chat.permissionCard.title")
                    }
                    openchamber_native_core::pending_requests::PendingRequest::Form(form) => {
                        form.title.clone().into()
                    }
                })?;
            header = header.child(button(
                format!("{id}:ordinary"),
                label,
                true,
                Control::Tab(false),
                palette,
                (window, cx),
                |this, _, cx| {
                    this.native_requests.selected = None;
                    cx.notify();
                },
            ));
        }
        let mut body = div()
            .id(SharedString::from(format!("{id}:body")))
            .max_h(px((f32::from(crate::chrome::content_size(window).height)
                * 0.5)
                .min(320.)))
            .overflow_y_scroll()
            .px(px(12.))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(6.));
        if !request.description.is_empty() {
            body = body.child(request.description.clone());
        }
        if let Some(code) = self.native_agents.owner.failure(&key.owner) {
            body = body.child(
                div()
                    .text_color(palette.status_error)
                    .child(strings.get(error_key(code))),
            );
        }
        let mut footer = div()
            .flex()
            .flex_row_from(direction)
            .items_center()
            .justify_line_end()
            .gap(px(6.))
            .px(px(12.))
            .py(px(8.));
        match &request.kind {
            RequestKind::Permission { choices } => {
                for choice in choices {
                    let answer = Answer::Permission {
                        request_id: request.id.clone(),
                        choice_id: choice.id.clone(),
                    };
                    let captured = key.clone();
                    footer = footer.child(button(
                        format!("{id}:choice:{}", choice.id),
                        choice.label.clone().into(),
                        enabled,
                        Control::Permission(
                            if choice.decision == Decision::Allow {
                                PermissionReply::Once
                            } else {
                                PermissionReply::Reject
                            },
                            None,
                        ),
                        palette,
                        (window, cx),
                        move |this, _, cx| {
                            this.native_answer_request(&captured, revision, answer.clone(), cx)
                        },
                    ));
                }
            }
            RequestKind::Question { fields } => {
                let draft = self.native_requests.drafts.get(&key)?;
                let field = fields.get(draft.step)?;
                body = body.child(div().child(field.label.clone()));
                if fields.len() > 1 {
                    let progress = strings.format(
                        "chat.formDock.progress",
                        &[
                            ("current", &(draft.step + 1).to_string()),
                            ("total", &fields.len().to_string()),
                        ],
                    );
                    body = body.child(div().text_color(palette.muted_foreground).child(progress));
                    body = body.child(div().flex().flex_row_from(direction).gap(px(4.)).children(
                        fields.iter().enumerate().map(|(step, field)| {
                            let captured = key.clone();
                            button(
                                format!("{id}:field:{}", field.id),
                                field.label.clone().into(),
                                enabled,
                                Control::Tab(step == draft.step),
                                palette,
                                (window, cx),
                                move |this, _, cx| {
                                    if let Some(draft) =
                                        this.native_requests.drafts.get_mut(&captured)
                                    {
                                        draft.step = step;
                                        draft.show_errors = false;
                                    }
                                    cx.notify();
                                },
                            )
                        }),
                    ));
                }
                for choice in &field.choices {
                    let selected = draft
                        .choices
                        .get(&field.id)
                        .is_some_and(|choices| choices.contains(&choice.id));
                    let captured = key.clone();
                    let field_id = field.id.clone();
                    let choice_id = choice.id.clone();
                    let multiple = field.multiple;
                    body = body.child(button(
                        format!("{id}:field:{}:choice:{}", field.id, choice.id),
                        choice.label.clone().into(),
                        enabled,
                        if multiple {
                            Control::Check(selected)
                        } else {
                            Control::Radio(selected)
                        },
                        palette,
                        (window, cx),
                        move |this, _, cx| {
                            if let Some(draft) = this.native_requests.drafts.get_mut(&captured) {
                                let choices = draft.choices.entry(field_id.clone()).or_default();
                                if multiple {
                                    if let Some(index) =
                                        choices.iter().position(|id| id == &choice_id)
                                    {
                                        choices.remove(index);
                                    } else {
                                        choices.push(choice_id.clone());
                                    }
                                } else {
                                    choices.clear();
                                    choices.push(choice_id.clone());
                                }
                                draft.show_errors = false;
                            }
                            cx.notify();
                        },
                    ));
                }
                if let Some(input) = draft.inputs.get(&field.id) {
                    body = body.child(
                        Textarea::new(input)
                            .disabled(!enabled)
                            .aria_label(field.label.clone()),
                    );
                }
                if draft.show_errors {
                    body = body.child(
                        div()
                            .text_color(palette.status_error)
                            .child(strings.get("chat.formCard.missingRequired")),
                    );
                }
                let captured = key.clone();
                footer = footer.child(button(
                    format!("{id}-advance"),
                    strings.get(if draft.step + 1 == fields.len() {
                        "chat.formCard.submit"
                    } else {
                        "chat.questionCard.next"
                    }),
                    enabled,
                    Control::Advance(draft.step + 1 == fields.len()),
                    palette,
                    (window, cx),
                    move |this, _, cx| this.advance_native_question(&captured, revision, cx),
                ));
            }
        }
        panel = panel.child(header).child(body).child(footer);
        Some(panel.into_any_element())
    }
}

fn error_key(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest => "chat.nativeAgent.error.invalidRequest",
        ErrorCode::StaleActivation => "chat.nativeAgent.error.staleActivation",
        ErrorCode::StaleRevision => "chat.nativeAgent.error.staleRevision",
        ErrorCode::NotPrepared => "chat.nativeAgent.error.notPrepared",
        ErrorCode::AuthenticationRequired => "chat.nativeAgent.error.authenticationRequired",
        ErrorCode::Unsupported => "chat.nativeAgent.error.unsupported",
        ErrorCode::PolicyDenied => "chat.nativeAgent.error.policyDenied",
        ErrorCode::RequestNotFound => "chat.nativeAgent.error.requestNotFound",
        ErrorCode::InvalidAnswer => "chat.nativeAgent.error.invalidAnswer",
        ErrorCode::InvalidConfig => "chat.nativeAgent.error.invalidConfig",
        ErrorCode::Unavailable => "chat.nativeAgent.error.unavailable",
        ErrorCode::OutcomeUnknown => "chat.nativeAgent.error.outcomeUnknown",
    }
}
