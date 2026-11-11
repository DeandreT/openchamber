//! What a `question` row shows opened: the answers its result reports, or
//! the questions its input asked (`parseQuestionOutput` and the question
//! branch of `ToolExpandedContent` in `ToolPart.tsx`).

/// One question of a `question` call's input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AskedQuestion {
    pub header: Option<String>,
    /// Markdown; empty when the input carried no text for it.
    pub question: String,
    /// The labels of the options offered.
    pub options: Vec<String>,
}

/// One answered question of a `question` call's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionAnswer {
    pub question: String,
    pub answer: String,
}

const ANSWERED_PREFIX: &str = "User has answered your questions:";
const ANSWERED_SUFFIX: &str = "You can now";

/// The question and answer pairs of a result that reads
/// `User has answered your questions: "Q"="A", "Q"="A". You can now …`;
/// `None` for any other text, which the row prints as it is.
#[must_use]
pub fn answers(output: &str) -> Option<Vec<QuestionAnswer>> {
    let listed = answered_list(output)?;
    let mut pairs = Vec::new();
    let mut rest = listed;
    while let Some(open) = rest.find('"') {
        let after_open = &rest[open + 1..];
        match pair(after_open) {
            Some((question, answer, after_pair)) => {
                pairs.push(QuestionAnswer {
                    question: question.to_owned(),
                    answer: answer.to_owned(),
                });
                rest = after_pair;
            }
            None => rest = after_open,
        }
    }
    (!pairs.is_empty()).then_some(pairs)
}

/// The text between the prefix and the first `. You can now`.
fn answered_list(output: &str) -> Option<&str> {
    let listed = output.strip_prefix(ANSWERED_PREFIX)?.trim_start();
    let mut search = 0;
    while let Some(found) = listed[search..].find('.') {
        let dot = search + found;
        // The reference's lazy `.+?` needs at least one character before it.
        if dot > 0 && listed[dot + 1..].trim_start().starts_with(ANSWERED_SUFFIX) {
            return Some(&listed[..dot]);
        }
        search = dot + 1;
    }
    None
}

/// `Q"="A"` at the start of `text`, just after an opening quote: the
/// question, the answer and what follows the pair. Neither holds a quote,
/// and the question is not empty.
fn pair(text: &str) -> Option<(&str, &str, &str)> {
    let close = text.find('"')?;
    if close == 0 {
        return None;
    }
    let answer_start = text[close..].strip_prefix("\"=\"")?;
    let answer_end = answer_start.find('"')?;
    Some((
        &text[..close],
        &answer_start[..answer_end],
        &answer_start[answer_end + 1..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(question: &str, answer: &str) -> QuestionAnswer {
        QuestionAnswer {
            question: question.to_owned(),
            answer: answer.to_owned(),
        }
    }

    #[test]
    fn reads_every_answered_pair() {
        let output = "User has answered your questions: \"Which one?\"=\"The first\", \"Why?\"=\"Speed.\nAnd size\". You can now continue with the user's answers in mind.";
        assert_eq!(
            answers(output),
            Some(vec![
                answered("Which one?", "The first"),
                answered("Why?", "Speed.\nAnd size"),
            ])
        );
    }

    #[test]
    fn an_empty_answer_is_still_an_answer() {
        assert_eq!(
            answers("User has answered your questions: \"Skip?\"=\"\". You can now go on"),
            Some(vec![answered("Skip?", "")])
        );
    }

    #[test]
    fn other_results_are_not_answers() {
        assert_eq!(answers("The user dismissed the question."), None);
        assert_eq!(
            answers("User has answered your questions: nothing quoted. You can now go on"),
            None
        );
        assert_eq!(
            answers("User has answered your questions: \"Q\"=\"A\""),
            None,
            "a result cut before its closing sentence is printed whole"
        );
        assert_eq!(
            answers(" User has answered your questions: \"Q\"=\"A\". You can now go on"),
            None,
            "the sentence starts the result"
        );
        assert_eq!(answers(""), None);
    }

    #[test]
    fn a_malformed_pair_does_not_hide_the_ones_after_it() {
        assert_eq!(
            answers(
                "User has answered your questions: \"\"=\"lost\", \"Kept?\"=\"Yes\". You can now go on"
            ),
            Some(vec![answered("Kept?", "Yes")])
        );
    }
}
