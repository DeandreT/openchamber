//! Provider logos for the composer and the sidebar, as
//! `packages/ui/src/components/ui/ProviderLogo.tsx` draws them.
//!
//! A bundled logo (`resources/provider-logos.json`, exported from the web's
//! assets by `bun run native:resources`) is drawn at once. Any other provider
//! gets models.dev's logo, fetched once per logo for the app's lifetime;
//! while it loads the logo keeps its box, as the web's image does, and a logo
//! that fails to load or parse is left out, as the web drops a broken image.
//! Each logo is prepared for both appearances: as authored for light themes,
//! inverted for dark ones (the web's `dark:invert`).

use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, OnceLock};

use gpui_kit::{Context, Image, ImageFormat, Task};
use openchamber_native_core::provider_logo;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::public_asset::PublicAssetClient;
use serde::Deserialize;

use crate::send_catalog::ModelControl;
use crate::theme::Appearance;
use crate::workspace::Workspace;

const PROVIDER_LOGOS_JSON: &str = include_str!("../resources/provider-logos.json");

#[derive(Deserialize)]
struct BundledLogos {
    aliases: HashMap<String, String>,
    logos: HashMap<String, String>,
}

fn bundled() -> &'static BundledLogos {
    static BUNDLED: OnceLock<BundledLogos> = OnceLock::new();
    BUNDLED.get_or_init(|| {
        serde_json::from_str(PROVIDER_LOGOS_JSON)
            .expect("resources/provider-logos.json is generated JSON")
    })
}

/// Where a provider's logo comes from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LogoSource {
    /// A bundled logo, by file name.
    Bundled(String),
    /// models.dev's logo, by candidate.
    Remote(String),
}

impl LogoSource {
    /// `useProviderLogo`'s choice: the first candidate with a bundled logo,
    /// else the first candidate from models.dev.
    fn of(provider_id: &str) -> Option<Self> {
        let bundled = bundled();
        let candidates = provider_logo::candidates(provider_id, &bundled.aliases);
        candidates
            .iter()
            .find(|candidate| bundled.logos.contains_key(candidate.as_str()))
            .map(|candidate| Self::Bundled(candidate.clone()))
            .or_else(|| candidates.into_iter().next().map(Self::Remote))
    }
}

/// One logo, drawn for each appearance.
#[derive(Clone, Debug)]
struct Drawings {
    light: Arc<Image>,
    dark: Arc<Image>,
}

impl Drawings {
    /// Both drawings of `svg`, or `None` when it does not parse.
    fn new(svg: &str, cx: &Context<Workspace>) -> Option<Self> {
        let inverted = provider_logo::invert_svg_colors(svg);
        let renderer = cx.svg_renderer();
        for drawing in [svg, inverted.as_str()] {
            renderer.parse_svg(drawing.as_bytes()).ok()?;
        }
        Some(Self {
            light: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.as_bytes().to_vec())),
            dark: Arc::new(Image::from_bytes(ImageFormat::Svg, inverted.into_bytes())),
        })
    }

    fn for_appearance(&self, appearance: Appearance) -> Arc<Image> {
        match appearance {
            Appearance::Light => self.light.clone(),
            Appearance::Dark => self.dark.clone(),
        }
    }
}

enum LogoState {
    Loading { _fetch: Task<()> },
    Ready(Drawings),
    Missing,
}

/// What a provider's logo slot shows.
#[derive(Clone, Debug)]
pub(crate) enum ProviderLogo {
    /// Not drawn yet: the slot keeps its size.
    Loading,
    Ready(Arc<Image>),
    /// No logo: the slot is left out.
    Absent,
}

/// The logos loaded so far, by source.
#[derive(Default)]
pub(crate) struct ProviderLogos {
    states: HashMap<LogoSource, LogoState>,
    client: Option<PublicAssetClient>,
}

impl Workspace {
    /// Starts loading the logos the composer and the sidebar rows show.
    pub(crate) fn ensure_provider_logos(&mut self, cx: &mut Context<Self>) {
        let sidebar = mem::take(&mut self.sidebar_providers);
        for provider_id in &sidebar {
            self.ensure_provider_logo(provider_id, cx);
        }
        self.sidebar_providers = sidebar;
        if let ModelControl::Model { provider_id, .. } = self.model_control() {
            self.ensure_provider_logo(&provider_id, cx);
        }
        // The turn footers of the selected session.
        let mut footers: Vec<String> = Vec::new();
        if let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        {
            for message in open.transcript.messages() {
                if let Some(model) = message
                    .info
                    .assistant
                    .as_ref()
                    .and_then(|facts| facts.model.as_ref())
                    && !footers.contains(&model.provider_id)
                {
                    footers.push(model.provider_id.clone());
                }
            }
        }
        for provider_id in &footers {
            self.ensure_provider_logo(provider_id, cx);
        }
    }

    /// Starts loading the logo of `provider_id` unless it is known.
    pub(crate) fn ensure_provider_logo(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        let Some(source) = LogoSource::of(provider_id) else {
            return;
        };
        if self.provider_logos.states.contains_key(&source) {
            return;
        }
        let state = match &source {
            LogoSource::Bundled(name) => bundled()
                .logos
                .get(name)
                .and_then(|svg| Drawings::new(svg, cx))
                .map_or(LogoState::Missing, LogoState::Ready),
            LogoSource::Remote(candidate) => match self.fetch_provider_logo(candidate, cx) {
                Some(fetch) => LogoState::Loading { _fetch: fetch },
                // Without a connection there is no network runtime; the
                // logo is tried again once there is one.
                None => return,
            },
        };
        self.provider_logos.states.insert(source, state);
    }

    /// The logo slot of `provider_id` in `appearance`.
    pub(crate) fn provider_logo(&self, provider_id: &str, appearance: Appearance) -> ProviderLogo {
        let Some(source) = LogoSource::of(provider_id) else {
            return ProviderLogo::Absent;
        };
        match self.provider_logos.states.get(&source) {
            None | Some(LogoState::Loading { .. }) => ProviderLogo::Loading,
            Some(LogoState::Ready(drawings)) => {
                ProviderLogo::Ready(drawings.for_appearance(appearance))
            }
            Some(LogoState::Missing) => ProviderLogo::Absent,
        }
    }

    fn fetch_provider_logo(&mut self, candidate: &str, cx: &mut Context<Self>) -> Option<Task<()>> {
        let network = &self.connection.as_ref()?.network;
        let client = match &self.provider_logos.client {
            Some(client) => client.clone(),
            None => {
                let built = {
                    let _runtime = network.enter();
                    PublicAssetClient::new()
                };
                match built {
                    Ok(client) => self.provider_logos.client.insert(client).clone(),
                    Err(error) => {
                        tracing::warn!(%error, "provider logos cannot be fetched");
                        return None;
                    }
                }
            }
        };
        let url = provider_logo::remote_url(candidate);
        let task = network.run(async move { client.fetch(&url).await });
        let candidate = candidate.to_owned();
        Some(cx.spawn(async move |this, cx| {
            let outcome = task.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update(cx, |this, cx| {
                this.finish_provider_logo(candidate, outcome, cx);
            });
        }))
    }

    fn finish_provider_logo(
        &mut self,
        candidate: String,
        outcome: Result<Vec<u8>, TransportError>,
        cx: &mut Context<Self>,
    ) {
        let state = match outcome {
            // A fetch cut short by a closing connection is tried again.
            Err(TransportError::Cancelled) => {
                self.provider_logos
                    .states
                    .remove(&LogoSource::Remote(candidate));
                return;
            }
            Err(error) => {
                tracing::debug!(logo = %candidate, %error, "provider logo unavailable");
                LogoState::Missing
            }
            Ok(body) => String::from_utf8(body)
                .ok()
                .and_then(|svg| Drawings::new(&svg, cx))
                .map_or(LogoState::Missing, LogoState::Ready),
        };
        self.provider_logos
            .states
            .insert(LogoSource::Remote(candidate), state);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_logos_win_over_models_dev() {
        assert_eq!(
            LogoSource::of("Claude"),
            Some(LogoSource::Bundled("anthropic".to_owned()))
        );
        assert_eq!(
            LogoSource::of("perf"),
            Some(LogoSource::Remote("perf".to_owned()))
        );
        assert_eq!(LogoSource::of("  "), None);
    }
}
