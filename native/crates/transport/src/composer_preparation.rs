//! Read-only composer preparation bound to one captured server and directory.
//!
//! The desktop retains the original source and the completed expansion separately.
//! These requests never consult the selected runtime, enumerate projects, prepare
//! files, or publish results. Failed or incomplete catalogs grant no empty authority.

use std::fmt;

use openchamber_native_core::send_config::AgentEntry;
use reqwest::{Method, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::client::{self, ServerClient};
use crate::error::TransportError;
use crate::request::RequestBuilder;
use crate::wire;

const CATALOG_BYTES: usize = 4 * 1024 * 1024;
const EXPANSION_BYTES: usize = 50 * 1024 * 1024;
const SOURCE_MAP_BYTES: usize = 4 * 1024 * 1024;
const SOURCE_RANGES: usize = 4096;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Unchanged original-input positions, measured in UTF-16 code units. These
/// offsets carry text-position evidence only, with no file or queue authority.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComposerSourceRange {
    pub input_start: u64,
    pub input_end: u64,
    pub output_start: u64,
    pub output_end: u64,
}

/// Expanded text and validated unchanged-original ranges. Generated text has no
/// range, even when its spelling happens to equal the original input.
pub struct ComposerExpansion {
    text: String,
    source_ranges: Vec<ComposerSourceRange>,
}

impl fmt::Debug for ComposerExpansion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComposerExpansion")
            .field("ranges", &self.source_ranges.len())
            .finish_non_exhaustive()
    }
}

impl ComposerExpansion {
    /// Validates position evidence against the exact requested input and actual
    /// result. This constructor grants no source, file, or queue ownership.
    pub fn from_parts(
        input: &str,
        text: String,
        source_ranges: Vec<ComposerSourceRange>,
    ) -> Result<Self, TransportError> {
        if input.len() > EXPANSION_BYTES
            || text.len() > EXPANSION_BYTES
            || source_ranges.len() > SOURCE_RANGES
            || serde_json::to_vec(&source_ranges)
                .map_err(|_| malformed())?
                .len()
                > SOURCE_MAP_BYTES
        {
            return Err(malformed());
        }
        let mut input_cursor = Utf16Cursor::new(input);
        let mut output_cursor = Utf16Cursor::new(&text);
        let mut checked: Vec<ComposerSourceRange> = Vec::with_capacity(source_ranges.len());
        for range in source_ranges {
            if range.input_start >= range.input_end
                || range.output_start >= range.output_end
                || range.input_end > MAX_SAFE_INTEGER
                || range.output_end > MAX_SAFE_INTEGER
                || range.input_end - range.input_start != range.output_end - range.output_start
                || checked.last().is_some_and(|previous| {
                    previous.input_end > range.input_start
                        || previous.output_end > range.output_start
                })
            {
                return Err(malformed());
            }
            let input_start = input_cursor
                .advance_to(range.input_start)
                .ok_or_else(malformed)?;
            let input_end = input_cursor
                .advance_to(range.input_end)
                .ok_or_else(malformed)?;
            let output_start = output_cursor
                .advance_to(range.output_start)
                .ok_or_else(malformed)?;
            let output_end = output_cursor
                .advance_to(range.output_end)
                .ok_or_else(malformed)?;
            if input[input_start..input_end] != text[output_start..output_end] {
                return Err(malformed());
            }
            if let Some(previous) = checked.last_mut().filter(|previous| {
                previous.input_end == range.input_start && previous.output_end == range.output_start
            }) {
                previous.input_end = range.input_end;
                previous.output_end = range.output_end;
            } else {
                checked.push(range);
            }
        }
        Ok(Self {
            text,
            source_ranges: checked,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn source_ranges(&self) -> &[ComposerSourceRange] {
        &self.source_ranges
    }
    pub fn into_text(self) -> String {
        self.text
    }

    fn identity(input: &str) -> Result<Self, TransportError> {
        let length = input.encode_utf16().count() as u64;
        let ranges = if length == 0 {
            Vec::new()
        } else {
            vec![ComposerSourceRange {
                input_start: 0,
                input_end: length,
                output_start: 0,
                output_end: length,
            }]
        };
        Self::from_parts(input, input.to_owned(), ranges)
    }
}

struct Utf16Cursor<'a> {
    characters: std::str::CharIndices<'a>,
    units: u64,
    byte: usize,
}

impl<'a> Utf16Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            characters: text.char_indices(),
            units: 0,
            byte: 0,
        }
    }
    fn advance_to(&mut self, offset: u64) -> Option<usize> {
        if offset < self.units {
            return None;
        }
        while self.units < offset {
            let (byte, character) = self.characters.next()?;
            self.units = self.units.checked_add(character.len_utf16() as u64)?;
            self.byte = byte + character.len_utf8();
        }
        (self.units == offset).then_some(self.byte)
    }
}

/// A known composer skill and the ID OpenCode can attach, when it lists one.
#[derive(Clone, PartialEq, Eq)]
pub struct ComposerSkill {
    pub name: String,
    pub id: Option<String>,
}

impl fmt::Debug for ComposerSkill {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ComposerSkill([redacted])")
    }
}

/// Captured request authority. The caller binds this client to its verified owner
/// before starting preparation and retains that owner through every await.
#[derive(Clone)]
pub struct ComposerPreparation {
    client: ServerClient,
    directory: String,
}

impl fmt::Debug for ComposerPreparation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ComposerPreparation([redacted])")
    }
}

impl ComposerPreparation {
    /// Captures the exact directory without issuing a request.
    pub fn new(client: &ServerClient, directory: &str) -> Result<Self, TransportError> {
        if directory.is_empty()
            || directory.trim() != directory
            || directory.encode_utf16().count() > 16_384
        {
            return Err(malformed());
        }
        Ok(Self {
            client: client.clone(),
            directory: directory.to_owned(),
        })
    }

    /// The exact submitted source directory, including its original spelling.
    pub fn directory(&self) -> &str {
        &self.directory
    }

    /// Agents in the catalog's order, which determines upstream mention precedence.
    pub async fn agents(&self) -> Result<Vec<AgentEntry>, TransportError> {
        let agents: Located<Vec<wire::AgentInfo>> =
            read(self.request(Method::GET, "api/agent", false), CATALOG_BYTES).await?;
        self.check_location(&agents.location)?;
        let agents: Vec<_> = agents
            .data
            .into_iter()
            .map(crate::projection::agent)
            .collect();
        if agents.iter().any(|agent| agent.name.is_empty()) {
            return Err(malformed());
        }
        Ok(agents)
    }

    /// The same available skill names as the composer registry, narrowed by
    /// external-root flags and integration.use policy. Partial scans are errors.
    /// IDs come from OpenCode's own list; unlisted names require an instruction.
    pub async fn skills(&self) -> Result<Vec<ComposerSkill>, TransportError> {
        let discovered: DiscoveredSkills = read(
            self.request(Method::GET, "api/config/skills", true),
            CATALOG_BYTES,
        )
        .await?;
        if discovered.open_code_skills_unavailable {
            return Err(malformed());
        }
        let config: Vec<ConfigEntry> = read(
            self.request(Method::GET, "api/config", false),
            CATALOG_BYTES,
        )
        .await?;
        let listed: Located<Vec<ListedSkill>> =
            read(self.request(Method::GET, "api/skill", false), CATALOG_BYTES).await?;
        self.check_location(&listed.location)?;
        available_skills(discovered, config, listed.data)
    }

    /// Calls the existing snippet expansion route with exactly {text}. A missing
    /// or malformed text field is an error, including on an otherwise valid 200.
    pub async fn expand_text(&self, text: &str) -> Result<String, TransportError> {
        if !has_snippet_token(text) {
            return Ok(text.to_owned());
        }
        let response: Expansion = read(self.expansion_request(text), EXPANSION_BYTES).await?;
        Ok(response.text)
    }

    /// Opts into positional evidence on the same expansion route. Older servers
    /// or malformed maps fail explicitly and never become guessed identity maps.
    pub async fn expand_text_mapped(
        &self,
        text: &str,
    ) -> Result<ComposerExpansion, TransportError> {
        if !has_snippet_token(text) {
            return ComposerExpansion::identity(text);
        }
        let response: MappedExpansion = read(
            self.mapped_expansion_request(text),
            EXPANSION_BYTES + SOURCE_MAP_BYTES,
        )
        .await?;
        ComposerExpansion::from_parts(text, response.text, response.source_ranges)
    }

    fn request(&self, method: Method, route: &str, query_directory: bool) -> RequestBuilder {
        let mut url = self.client.endpoint().url(route);
        if query_directory {
            url.query_pairs_mut()
                .append_pair("directory", &self.directory);
        }
        self.client
            .authorized(method, url)
            .header(
                "x-opencode-directory",
                client::encode_directory(&self.directory),
            )
            .header("x-opencode-directory-encoding", "uri")
    }

    fn expansion_request(&self, text: &str) -> RequestBuilder {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            text: &'a str,
        }
        self.request(Method::POST, "api/config/snippets/expand", true)
            .json(&Body { text })
    }

    fn mapped_expansion_request(&self, text: &str) -> RequestBuilder {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            text: &'a str,
            source_map: bool,
        }
        self.request(Method::POST, "api/config/snippets/expand", true)
            .json(&Body {
                text,
                source_map: true,
            })
    }

    fn check_location(&self, location: &Location) -> Result<(), TransportError> {
        if location.directory != self.directory {
            return Err(malformed());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Located<T> {
    location: Location,
    data: T,
}

#[derive(Deserialize)]
struct Location {
    directory: String,
}

#[derive(Deserialize)]
struct Expansion {
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MappedExpansion {
    text: String,
    #[serde(deserialize_with = "bounded_source_ranges")]
    source_ranges: Vec<ComposerSourceRange>,
}

fn bounded_source_ranges<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ComposerSourceRange>, D::Error> {
    struct Ranges;
    impl<'de> serde::de::Visitor<'de> for Ranges {
        type Value = Vec<ComposerSourceRange>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("bounded source ranges")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut ranges = Vec::new();
            while let Some(range) = sequence.next_element()? {
                if ranges.len() == SOURCE_RANGES {
                    return Err(serde::de::Error::custom("source range limit exceeded"));
                }
                ranges.push(range);
            }
            Ok(ranges)
        }
    }
    deserializer.deserialize_seq(Ranges)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveredSkills {
    skills: Vec<DiscoveredSkill>,
    external_skills: Option<ExternalSkills>,
    #[serde(default)]
    open_code_skills_unavailable: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExternalSkills {
    claude_disabled: bool,
    all_disabled: bool,
}

#[derive(Deserialize)]
struct DiscoveredSkill {
    id: Option<String>,
    name: String,
    path: String,
}

#[derive(Deserialize)]
struct ListedSkill {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct ConfigEntry {
    #[serde(rename = "type")]
    kind: String,
    info: Option<Value>,
}

struct Policy {
    resource: String,
    deny: bool,
}

fn available_skills(
    discovered: DiscoveredSkills,
    config: Vec<ConfigEntry>,
    listed: Vec<ListedSkill>,
) -> Result<Vec<ComposerSkill>, TransportError> {
    if discovered.open_code_skills_unavailable {
        return Err(malformed());
    }
    let mut skills: Vec<DiscoveredSkill> = Vec::new();
    for skill in discovered.skills {
        if skill.name.is_empty()
            || skill.path.is_empty()
            || skill.id.as_ref().is_some_and(String::is_empty)
        {
            return Err(malformed());
        }
        let root = skill_root(&skill.path);
        if discovered.external_skills.as_ref().is_some_and(|flags| {
            root != "opencode" && (flags.all_disabled || root == "claude" && flags.claude_disabled)
        }) {
            continue;
        }
        // With no flags, the upstream registry keeps its original scan unchanged.
        if discovered.external_skills.is_some()
            && let Some(previous) = skills
                .iter_mut()
                .find(|previous| previous.name == skill.name)
        {
            if skill_root(&previous.path) == "claude" && root == "agents" {
                *previous = skill;
            }
            continue;
        }
        skills.push(skill);
    }
    if listed
        .iter()
        .any(|skill| skill.id.is_empty() || skill.name.is_empty())
    {
        return Err(malformed());
    }
    let policies = policies(config)?;
    let mut available = Vec::new();
    for skill in skills {
        let id = skill
            .id
            .as_deref()
            .unwrap_or_else(|| skill_id(&skill.path, &skill.name));
        let resource = format!("skill:{id}");
        if policies
            .iter()
            .rev()
            .find(|policy| wildcard_matches(&resource, &policy.resource))
            .is_some_and(|policy| policy.deny)
        {
            continue;
        }
        let attached = listed
            .iter()
            .find(|listed| listed.name == skill.name)
            .or_else(|| listed.iter().find(|listed| listed.id == skill.name));
        available.push(ComposerSkill {
            name: skill.name,
            id: attached.map(|skill| skill.id.clone()),
        });
    }
    Ok(available)
}

fn skill_root(path: &str) -> &'static str {
    let path = path.replace('\\', "/");
    let mut parts = path.split('/');
    if parts.clone().any(|part| part == ".claude") {
        "claude"
    } else if parts.any(|part| part == ".agents") {
        "agents"
    } else {
        "opencode"
    }
}

fn skill_id<'a>(path: &'a str, name: &'a str) -> &'a str {
    if path == "<built-in>" {
        return name;
    }
    let mut parts = path.rsplit(['/', '\\']).filter(|part| !part.is_empty());
    let file = parts.next().unwrap_or_default();
    if file == "SKILL.md" {
        return parts.next().unwrap_or(name);
    }
    file.strip_suffix(".md")
        .filter(|id| !id.is_empty())
        .unwrap_or(name)
}

fn policies(config: Vec<ConfigEntry>) -> Result<Vec<Policy>, TransportError> {
    let mut policies = Vec::new();
    for entry in config
        .into_iter()
        .rev()
        .filter(|entry| entry.kind == "document")
    {
        let info = entry
            .info
            .as_ref()
            .and_then(Value::as_object)
            .ok_or_else(malformed)?;
        for value in [
            info.get("experimental")
                .and_then(|value| value.get("policies")),
            info.get("policies"),
        ]
        .into_iter()
        .flatten()
        {
            let entries = value.as_array().ok_or_else(malformed)?;
            for entry in entries {
                let action = entry
                    .get("action")
                    .and_then(Value::as_str)
                    .ok_or_else(malformed)?;
                if action != "integration.use" {
                    continue;
                }
                let resource = entry
                    .get("resource")
                    .and_then(Value::as_str)
                    .ok_or_else(malformed)?;
                let deny = match entry.get("effect").and_then(Value::as_str) {
                    Some("deny") => true,
                    Some("allow") => false,
                    _ => return Err(malformed()),
                };
                policies.push(Policy {
                    resource: resource.to_owned(),
                    deny,
                });
            }
        }
    }
    Ok(policies)
}

/// Upstream Wildcard.match uses UTF-16 units and allows a trailing " *" to
/// match nothing. The greedy star scanner avoids a quadratic backtracking table.
fn wildcard_matches(input: &str, pattern: &str) -> bool {
    let input: Vec<_> = input.replace('\\', "/").encode_utf16().collect();
    let pattern = pattern.replace('\\', "/");
    let full: Vec<_> = pattern.encode_utf16().collect();
    wildcard_units(&input, &full)
        || pattern
            .strip_suffix(" *")
            .is_some_and(|short| wildcard_units(&input, &short.encode_utf16().collect::<Vec<_>>()))
}

fn wildcard_units(input: &[u16], pattern: &[u16]) -> bool {
    let (mut index, mut position) = (0, 0);
    let mut star = None;
    let mut retry = 0;
    while index < input.len() {
        if position < pattern.len()
            && (pattern[position] == u16::from(b'?') || pattern[position] == input[index])
        {
            index += 1;
            position += 1;
        } else if position < pattern.len() && pattern[position] == u16::from(b'*') {
            star = Some(position);
            position += 1;
            retry = index;
        } else if let Some(saved) = star {
            retry += 1;
            index = retry;
            position = saved + 1;
        } else {
            return false;
        }
    }
    while position < pattern.len() && pattern[position] == u16::from(b'*') {
        position += 1;
    }
    position == pattern.len()
}

/// Whether `text` holds a `#snippet` token the server would expand.
pub fn has_snippet_token(text: &str) -> bool {
    text.as_bytes().windows(2).any(|pair| {
        pair[0] == b'#' && (pair[1].is_ascii_alphanumeric() || b"_-".contains(&pair[1]))
    })
}

fn malformed() -> TransportError {
    TransportError::Malformed("composer preparation response is incomplete or malformed".to_owned())
}

async fn read<T: DeserializeOwned>(
    request: RequestBuilder,
    limit: usize,
) -> Result<T, TransportError> {
    let response = request.send().await.map_err(TransportError::from_request)?;
    let bytes = response_bytes(response, limit).await?;
    serde_json::from_slice(&bytes).map_err(|_| malformed())
}

async fn response_bytes(mut response: Response, limit: usize) -> Result<Vec<u8>, TransportError> {
    let status = response.status();
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
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
        || response
            .content_length()
            .is_some_and(|length| length > limit as u64)
    {
        return Err(malformed());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(TransportError::from_request)?
    {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(malformed());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "composer_preparation_tests.rs"]
mod tests;
