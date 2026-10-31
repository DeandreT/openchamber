//! The composer's prompt-pair slash commands (`submit/slashCommands.ts`) and
//! the template rendering of `lib/magicPrompts.ts`.
//!
//! A command such as `/explore` renders two magic prompts: the visible one
//! the user sees as their message and the instructions the model receives
//! as synthetic context. The templates are the server's overrides for the
//! directory, else the built-in text the resource generator exports.

use std::collections::HashMap;

/// A command and the text typed after it (`parseSlashCommand`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    /// The first word after the slash, lowercased.
    pub name: String,
    /// Everything after the name, trimmed.
    pub argument: String,
}

/// Reads the leading slash command of `text`, if it has one.
pub fn parse_slash_command(text: &str) -> Option<SlashCommand> {
    let without_slash = text.trim_start().strip_prefix('/')?;
    let name = without_slash.split_whitespace().next()?.to_lowercase();
    // The argument starts after the name's length in the original text, as
    // the reference slices it.
    let argument = without_slash
        .get(name.len()..)
        .unwrap_or_default()
        .trim()
        .to_owned();
    Some(SlashCommand { name, argument })
}

/// Which prompt a template variable belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSide {
    Visible,
    Instructions,
}

/// One variable a command derives from its argument: `empty` without an
/// argument, else `with_argument` with `{{argument}}` replaced by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandVariable {
    pub side: PromptSide,
    pub key: String,
    pub empty: String,
    pub with_argument: String,
}

/// A prompt-pair command (`MagicPromptCommand`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicCommand {
    pub name: String,
    pub visible: String,
    pub instructions: String,
    /// Whether it needs an existing session (`requires: 'session'`).
    pub requires_session: bool,
    pub variables: Vec<CommandVariable>,
}

/// The variables of each side for one invocation (`buildCommandVariables`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InvocationVariables {
    pub visible: HashMap<String, String>,
    pub instructions: HashMap<String, String>,
}

impl MagicCommand {
    pub fn variables(&self, argument: &str) -> InvocationVariables {
        let mut variables = InvocationVariables::default();
        for variable in &self.variables {
            let value = if argument.is_empty() {
                variable.empty.clone()
            } else {
                variable.with_argument.replace("{{argument}}", argument)
            };
            let side = match variable.side {
                PromptSide::Visible => &mut variables.visible,
                PromptSide::Instructions => &mut variables.instructions,
            };
            side.insert(variable.key.clone(), value);
        }
        variables
    }
}

/// `replaceTemplateVariables`: every `{{ name }}` (letters, digits and
/// underscores, optional inner spaces) becomes its value, or nothing when
/// the variable is not given.
pub fn render(template: &str, variables: &HashMap<String, String>) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match placeholder(after) {
            Some((name, consumed)) => {
                output.push_str(variables.get(name).map(String::as_str).unwrap_or_default());
                rest = &after[consumed..];
            }
            None => {
                output.push_str("{{");
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

/// The name of a placeholder whose `{{` was just read, and how many bytes
/// through its closing `}}` it spans.
fn placeholder(text: &str) -> Option<(&str, usize)> {
    let leading = text.len() - text.trim_start_matches(char::is_whitespace).len();
    let body = &text[leading..];
    let name_length = body
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(body.len());
    if name_length == 0 {
        return None;
    }
    let name = &body[..name_length];
    let tail = &body[name_length..];
    let trailing = tail.len() - tail.trim_start_matches(char::is_whitespace).len();
    tail[trailing..]
        .starts_with("}}")
        .then_some((name, leading + name_length + trailing + 2))
}

/// One operation's prompts: the overrides read for its directory over the
/// built-in templates (`loadMagicPrompts`). Presence decides, so an empty
/// override still wins.
#[derive(Debug)]
pub struct MagicPrompts<'a> {
    pub overrides: &'a HashMap<String, String>,
    pub defaults: &'a HashMap<String, String>,
}

impl MagicPrompts<'_> {
    /// Renders prompt `id`; `None` for an ID without a built-in template.
    pub fn render(&self, id: &str, variables: &HashMap<String, String>) -> Option<String> {
        let template = self.overrides.get(id).or_else(|| self.defaults.get(id))?;
        Some(render(template, variables))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn parses_like_the_reference() {
        assert_eq!(
            parse_slash_command("  /Craft-Goal  ship it \n"),
            Some(SlashCommand {
                name: "craft-goal".to_owned(),
                argument: "ship it".to_owned()
            })
        );
        assert_eq!(parse_slash_command("/").map(|c| c.name), None);
        assert_eq!(parse_slash_command("explore"), None);
    }

    #[test]
    fn renders_placeholders_and_drops_unknown_ones() {
        let rendered = render(
            "a {{ idea_block }}b{{missing}}c{{ not valid}}{{x",
            &vars(&[("idea_block", "!")]),
        );
        assert_eq!(rendered, "a !bc{{ not valid}}{{x");
    }

    #[test]
    fn overrides_win_by_presence() {
        let overrides = vars(&[("v", "")]);
        let defaults = vars(&[("v", "built-in"), ("i", "{{topic}}")]);
        let prompts = MagicPrompts {
            overrides: &overrides,
            defaults: &defaults,
        };
        assert_eq!(prompts.render("v", &HashMap::new()).as_deref(), Some(""));
        assert_eq!(
            prompts.render("i", &vars(&[("topic", "t")])).as_deref(),
            Some("t")
        );
        assert_eq!(prompts.render("nope", &HashMap::new()), None);
    }

    #[test]
    fn variables_come_from_the_argument() {
        let command = MagicCommand {
            name: "craft-goal".to_owned(),
            visible: "v".to_owned(),
            instructions: "i".to_owned(),
            requires_session: false,
            variables: vec![CommandVariable {
                side: PromptSide::Visible,
                key: "idea_block".to_owned(),
                empty: String::new(),
                with_argument: "\n\nidea:\n{{argument}}".to_owned(),
            }],
        };
        assert_eq!(command.variables("").visible["idea_block"], "");
        assert_eq!(
            command.variables("go").visible["idea_block"],
            "\n\nidea:\ngo"
        );
    }
}
