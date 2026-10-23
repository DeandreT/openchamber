//! The composer toolbar's model and agent controls.
//!
//! Mirrors the desktop layout of
//! `packages/ui/src/components/chat/ModelControls.tsx`: the reasoning effort
//! when the model offers efforts, the provider logo and model name, then the
//! agent glyph and name in the agent's color, all in `typography-meta` at
//! medium weight. The menus behind them arrive with sending; the controls
//! show what a send would use.

use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div, img, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::provider_logos::ProviderLogo;
use crate::send_catalog::{AgentControl, ModelControl, VariantControl};
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `buttonHeight` of the desktop controls.
const CONTROL_HEIGHT: f32 = 32.0;
const CONTROL_GLYPH: f32 = 16.0;
/// Provider logos read smaller than sprite glyphs, so they get a step up.
const PROVIDER_LOGO: f32 = 17.0;
const CONTROL_GAP: f32 = 6.0;
/// `typography-meta`: 12.5/18 px, in rems of the 16 px root.
const META_SIZE: f32 = 0.78125;
const META_LINE: f32 = 1.125;
const MODEL_LABEL_MAX: f32 = 260.0;
const VARIANT_LABEL_MAX: f32 = 180.0;
const AGENT_LABEL_MAX: f32 = 220.0;
/// `hover:opacity-70`.
const HOVER_OPACITY: f32 = 0.7;
/// The container widths at or below which `index.css` hides a label
/// (`22rem`, `14rem` and `10rem` of `@container/model-controls`).
const AGENT_LABEL_HIDDEN_AT: f32 = 352.0;
const VARIANT_LABEL_HIDDEN_AT: f32 = 224.0;
const MODEL_LABEL_HIDDEN_AT: f32 = 160.0;

/// Which control labels fit the controls' width; a hidden label leaves the
/// glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Labels {
    variant: bool,
    model: bool,
    agent: bool,
}

impl Labels {
    fn for_width(width: f32) -> Self {
        Self {
            variant: width > VARIANT_LABEL_HIDDEN_AT,
            model: width > MODEL_LABEL_HIDDEN_AT,
            agent: width > AGENT_LABEL_HIDDEN_AT,
        }
    }
}

impl Workspace {
    /// The effort, model and agent controls, end-aligned in `width`.
    pub(crate) fn render_model_controls(
        &self,
        palette: &Palette,
        direction: Direction,
        width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let labels = Labels::for_width(width);
        let model = self.model_control();
        let variant = match &model {
            ModelControl::Model { variant, .. } => variant.clone(),
            _ => VariantControl::Hidden,
        };
        div()
            .flex_1()
            .min_w_0()
            .flex_row_from(direction)
            .items_center()
            .justify_end()
            .gap(px(12.0))
            .children(render_variant_control(
                &variant,
                labels.variant,
                palette,
                direction,
                cx,
            ))
            .child(self.render_model_control(model, labels.model, palette, direction, cx))
            .child(self.render_agent_control(labels.agent, palette, direction, cx))
    }

    fn render_model_control(
        &self,
        model: ModelControl,
        show_label: bool,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let (leading, label, color): (Option<AnyElement>, SharedString, _) = match model {
            ModelControl::Pending { unavailable } => (
                Some(readiness_glyph(unavailable, palette)),
                readiness_label(unavailable, cx),
                palette.muted_foreground,
            ),
            ModelControl::Unselected => (
                Some(
                    glyph("pencil-ai", CONTROL_GLYPH, palette.muted_foreground).into_any_element(),
                ),
                strings.get("chat.modelControls.selectModel"),
                palette.foreground,
            ),
            ModelControl::Auto => (
                Some(glyph("openchamber", CONTROL_GLYPH, palette.foreground).into_any_element()),
                strings.get("chat.modelControls.autoModel"),
                palette.foreground,
            ),
            ModelControl::Model {
                provider_id, label, ..
            } => (
                self.render_provider_logo(&provider_id, PROVIDER_LOGO, 1.0, palette),
                label.into(),
                palette.foreground,
            ),
        };
        control(
            "composer-model",
            label.clone(),
            direction,
            cx,
            leading,
            show_label.then(|| meta_label(label, color, MODEL_LABEL_MAX, cx)),
        )
    }

    fn render_agent_control(
        &self,
        show_label: bool,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let (leading, label, color): (AnyElement, SharedString, _) =
            match self.agent_control(palette) {
                AgentControl::Pending { unavailable } => (
                    readiness_glyph(unavailable, palette),
                    readiness_label(unavailable, cx),
                    palette.muted_foreground,
                ),
                AgentControl::Default { label } => (
                    glyph("ai-agent", CONTROL_GLYPH, palette.muted_foreground).into_any_element(),
                    label.map_or_else(
                        || strings.get("chat.modelControls.selectAgent"),
                        SharedString::from,
                    ),
                    palette.foreground,
                ),
                AgentControl::Agent { label, color } => {
                    let color = palette.agent_color(&color);
                    (
                        glyph("ai-agent", CONTROL_GLYPH, color).into_any_element(),
                        label.into(),
                        color,
                    )
                }
            };
        let leading = Some(leading);
        control(
            "composer-agent",
            label.clone(),
            direction,
            cx,
            leading,
            show_label.then(|| meta_label(label, color, AGENT_LABEL_MAX, cx)),
        )
    }
}

impl Workspace {
    /// A provider's logo at `size`: an empty box while it loads, nothing
    /// when there is none (`ProviderLogo` renders no element then).
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

/// The reasoning effort: the model's default muted, a chosen effort in the
/// info status color.
fn render_variant_control(
    variant: &VariantControl,
    show_label: bool,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> Option<AnyElement> {
    let (label, color): (SharedString, _) = match variant {
        VariantControl::Hidden => return None,
        VariantControl::Default => (
            locale::strings(cx).get("chat.modelControls.default"),
            palette.muted_foreground,
        ),
        VariantControl::Selected(variant) => (variant.clone().into(), palette.status_info),
    };
    Some(control(
        "composer-variant",
        label.clone(),
        direction,
        cx,
        Some(glyph("brain-ai-3", CONTROL_GLYPH, color).into_any_element()),
        show_label.then(|| meta_label(label, color, VARIANT_LABEL_MAX, cx)),
    ))
}

/// One trigger: a glyph and a label, dimmed under the pointer.
fn control(
    id: &'static str,
    label: SharedString,
    direction: Direction,
    cx: &Context<Workspace>,
    leading: Option<AnyElement>,
    text: Option<AnyElement>,
) -> AnyElement {
    on_activate(
        div()
            .id(id)
            .role(Role::Button)
            .accessibility_id(id)
            .aria_label(label)
            .h(px(CONTROL_HEIGHT))
            .min_w_0()
            .flex_row_from(direction)
            .items_center()
            .gap(px(CONTROL_GAP))
            .cursor_pointer()
            .hover(|style| style.opacity(HOVER_OPACITY))
            .children(leading)
            .children(text),
        cx,
        |_, _, _| {},
    )
    .test_support()
    .into_any_element()
}

fn meta_label(
    text: SharedString,
    color: gpui_kit::Hsla,
    max_width: f32,
    cx: &Context<Workspace>,
) -> AnyElement {
    div()
        .min_w_0()
        .max_w(px(max_width))
        .truncate()
        .css_text(META_SIZE, META_LINE, cx)
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// The spinner's place while loading, a warning once unavailable.
fn readiness_glyph(unavailable: bool, palette: &Palette) -> AnyElement {
    glyph(
        if unavailable {
            "error-warning"
        } else {
            "loader-4"
        },
        CONTROL_GLYPH,
        palette.muted_foreground,
    )
    .into_any_element()
}

fn readiness_label(unavailable: bool, cx: &Context<Workspace>) -> SharedString {
    locale::strings(cx).get(if unavailable {
        "common.unavailable"
    } else {
        "common.loading"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The widths the reference's composers give the controls: the
    /// new-session view's 372 px and a session's 340 px.
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
