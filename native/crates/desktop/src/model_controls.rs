//! Composer toolbar triggers, retaining the web's responsive label order.

use gpui_kit::{AnyElement, Context, IntoElement, ParentElement, Styled, div, img, px};

use crate::composer_picker::{PickerAppearance, PickerKind};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::provider_logos::ProviderLogo;
use crate::send_catalog::{AgentControl, ModelControl, VariantControl};
use crate::theme::Palette;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Labels {
    variant: bool,
    model: bool,
    agent: bool,
}

impl Labels {
    fn for_width(width: f32) -> Self {
        Self {
            variant: width > 224.0,
            model: width > 160.0,
            agent: width > 352.0,
        }
    }
}

impl Workspace {
    pub(crate) fn render_model_controls(
        &self,
        palette: &Palette,
        direction: Direction,
        width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let labels = Labels::for_width(width);
        let strings = locale::strings(cx);
        let model = self.model_control();
        let auto = model == ModelControl::Auto;
        let variant = match &model {
            ModelControl::Model { variant, .. } => variant.clone(),
            _ => VariantControl::Hidden,
        };
        let (label, icon, color, provider) = match model {
            ModelControl::Pending { unavailable } => (
                strings.get(if unavailable {
                    "common.unavailable"
                } else {
                    "common.loading"
                }),
                if unavailable {
                    "error-warning"
                } else {
                    "loader-4"
                },
                palette.muted_foreground,
                None,
            ),
            ModelControl::Unselected => (
                strings.get("chat.modelControls.selectModel"),
                "pencil-ai",
                palette.foreground,
                None,
            ),
            ModelControl::Auto => (
                strings.get("chat.modelControls.autoModel"),
                "openchamber",
                palette.foreground,
                None,
            ),
            ModelControl::Model {
                label, provider_id, ..
            } => (
                label.into(),
                "pencil-ai",
                palette.foreground,
                Some(provider_id),
            ),
        };
        let model = self.picker_control(
            PickerKind::Model,
            PickerAppearance {
                label,
                icon,
                color,
                label_color: color,
                provider,
            },
            labels.model,
            direction,
            cx,
        );
        // The effort is a muted suffix whether it is Default or a choice.
        let effort = match variant {
            VariantControl::Hidden => None,
            VariantControl::Default => Some(strings.get("chat.modelControls.default")),
            VariantControl::Selected(name) => Some(name.into()),
        }
        .map(|label| {
            self.picker_control(
                PickerKind::Effort,
                PickerAppearance {
                    label,
                    icon: "brain-ai-3",
                    color: palette.muted_foreground,
                    label_color: palette.muted_foreground,
                    provider: None,
                },
                labels.variant,
                direction,
                cx,
            )
        });
        let agent = (!auto).then(|| {
            // The agent's color marks its icon; its name stays in the text color.
            let (label, icon, color, label_color) = match self.agent_control(palette) {
                AgentControl::Pending { unavailable } => (
                    strings.get(if unavailable {
                        "common.unavailable"
                    } else {
                        "common.loading"
                    }),
                    if unavailable {
                        "error-warning"
                    } else {
                        "loader-4"
                    },
                    palette.muted_foreground,
                    palette.muted_foreground,
                ),
                AgentControl::Default { label } => (
                    label.map_or_else(|| strings.get("chat.modelControls.selectAgent"), Into::into),
                    "ai-agent",
                    palette.foreground,
                    palette.foreground,
                ),
                AgentControl::Agent { label, color } => (
                    label.into(),
                    "ai-agent",
                    palette.agent_color(&color),
                    palette.foreground,
                ),
            };
            self.picker_control(
                PickerKind::Agent,
                PickerAppearance {
                    label,
                    icon,
                    color,
                    label_color,
                    provider: None,
                },
                labels.agent,
                direction,
                cx,
            )
        });
        // The effort follows the model as its suffix, so the two sit closer
        // than the other controls: 4px between them and 12px before the
        // agent, as gap-x-1 and gap-x-3 place them. Each trigger's combobox
        // already leaves 4px after it, so the gaps here are 4px less and the
        // row ends 4px past its edge.
        div()
            .flex_1()
            .min_w_0()
            .flex_row_from(direction)
            .items_center()
            .justify_end()
            .margin_end(direction, px(-4.0))
            .gap(px(8.0))
            .child(
                div()
                    .min_w_0()
                    .flex_row_from(direction)
                    .items_center()
                    .child(model)
                    .children(effort),
            )
            .children(agent)
    }

    pub(crate) fn render_provider_logo(
        &self,
        provider_id: &str,
        size: f32,
        opacity: f32,
        palette: &Palette,
    ) -> Option<AnyElement> {
        let slot = div().size(px(size)).flex_shrink_0().opacity(opacity);
        match self.provider_logo(provider_id, palette.appearance) {
            ProviderLogo::Loading => Some(slot.into_any_element()),
            ProviderLogo::Ready(image) => {
                Some(slot.child(img(image).size_full()).into_any_element())
            }
            ProviderLogo::Absent => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_hide_in_the_web_order() {
        let shown = |variant, model, agent| Labels {
            variant,
            model,
            agent,
        };
        assert_eq!(Labels::for_width(372.0), shown(true, true, true));
        assert_eq!(Labels::for_width(340.0), shown(true, true, false));
        assert_eq!(Labels::for_width(352.0), shown(true, true, false));
        assert_eq!(Labels::for_width(200.0), shown(false, true, false));
        assert_eq!(Labels::for_width(160.0), shown(false, false, false));
    }
}
