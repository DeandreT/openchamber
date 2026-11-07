//! The web client's list matchers, `lib/search/fuzzySearch.ts` and
//! `lib/search/subsequenceSearch.ts`, for the native pickers.
//!
//! Offsets count characters where the web counts UTF-16 units; the two differ
//! only for characters outside the Basic Multilingual Plane, which no ranked
//! field relies on. Fuse.js's typo-tolerant fallback is ported as the edit
//! distance Fuse's Bitap search computes with `ignoreLocation`: the fewest
//! insertions, deletions and substitutions that place the pattern anywhere in
//! the text, divided by the pattern's length.

/// `matchesFuzzyQuery`'s and `rankByQuery`'s typo tolerance.
const FUZZY_MATCH_THRESHOLD: f64 = 0.4;
const RANK_FUZZY_THRESHOLD: f64 = 0.35;

/// `matchesFuzzyQuery`: a substring, or a Fuse match within 0.4 of the query's
/// length in edits. An empty query matches everything, an empty target nothing.
pub fn matches_fuzzy_query(target: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if target.is_empty() {
        return false;
    }
    let target = target.to_lowercase();
    let query = query.to_lowercase();
    target.contains(&query) || fuse_score(&target, &query, FUZZY_MATCH_THRESHOLD, 1).is_some()
}

fn rank_tokens(query: &str) -> Vec<String> {
    query
        .trim()
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn compact(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        .collect()
}

struct RankFields {
    fields: Vec<Vec<char>>,
    compact: Vec<String>,
}

impl RankFields {
    fn new<'a>(texts: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        let mut fields = Vec::new();
        let mut compacted = Vec::new();
        for text in texts.into_iter().flatten() {
            if text.is_empty() {
                continue;
            }
            let lower = text.to_lowercase();
            compacted.push(compact(&lower));
            fields.push(lower.chars().collect());
        }
        Self {
            fields,
            compact: compacted,
        }
    }

    fn joined(&self) -> String {
        self.fields
            .iter()
            .map(|field| field.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn char_index(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `scoreRankToken`: lower is better; a field prefix, a word-boundary
/// substring, a mid-word substring, then a punctuation-insensitive substring.
/// Earlier fields win ties.
fn score_rank_token(token: &str, fields: &RankFields) -> Option<f64> {
    let token_chars: Vec<char> = token.chars().collect();
    let compact_token = compact(token);
    let mut best: Option<f64> = None;
    for (field_index, field) in fields.fields.iter().enumerate() {
        let penalty = field_index as f64 * 0.01;
        let score = match char_index(field, &token_chars) {
            Some(0) => Some(penalty),
            Some(index) => {
                let before = field[index - 1];
                let boundary = !(before.is_ascii_lowercase() || before.is_ascii_digit());
                Some(if boundary { 0.1 } else { 0.2 } + index as f64 / 1000.0 + penalty)
            }
            None if !compact_token.is_empty() && token_chars.len() > 1 => fields.compact
                [field_index]
                .find(&compact_token)
                .map(|index| 0.4 + index as f64 / 1000.0 + penalty),
            None => None,
        };
        if let Some(score) = score
            && best.is_none_or(|best| score < best)
        {
            best = Some(score);
        }
    }
    best
}

/// `rankByQuery`: the items whose fields hold every query token, best first,
/// original order breaking ties. A single token of three or more characters
/// also finds typos when `fuzzy` is set. Returns indices into `items`.
pub fn rank_by_query<T, F>(items: &[T], query: &str, texts: F, fuzzy: bool) -> Vec<usize>
where
    F: Fn(&T) -> Vec<Option<&str>>,
{
    let tokens = rank_tokens(query);
    if tokens.is_empty() {
        return (0..items.len()).collect();
    }
    let mut scored: Vec<(f64, usize)> = Vec::new();
    let mut missed: Vec<(String, usize)> = Vec::new();
    for (order, item) in items.iter().enumerate() {
        let fields = RankFields::new(texts(item));
        let mut total = Some(0.0);
        for token in &tokens {
            total = match (total, score_rank_token(token, &fields)) {
                (Some(total), Some(score)) => Some(total + score),
                _ => None,
            };
            if total.is_none() {
                break;
            }
        }
        match total {
            Some(score) => scored.push((score, order)),
            None => missed.push((fields.joined(), order)),
        }
    }
    if fuzzy && tokens.len() == 1 && tokens[0].chars().count() >= 3 {
        for (joined, order) in &missed {
            if let Some(score) = fuse_score(joined, &tokens[0], RANK_FUZZY_THRESHOLD, 2) {
                scored.push((1.0 + score, *order));
            }
        }
    }
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, order)| order).collect()
}

/// `matchesRankQuery`: every token matches one of the fields, without the
/// fuzzy fallback. A query of punctuation alone matches nothing.
pub fn matches_rank_query<'a>(
    texts: impl IntoIterator<Item = Option<&'a str>>,
    query: &str,
) -> bool {
    let tokens = rank_tokens(query);
    if tokens.is_empty() {
        return true;
    }
    let fields = RankFields::new(texts);
    tokens
        .iter()
        .all(|token| score_rank_token(token, &fields).is_some())
}

/// Fuse.js's score for `pattern` in `text`, both lowercased, or None above
/// `threshold`. With `ignoreLocation` a Bitap match costs its edits over the
/// pattern's length; Fuse floors it at 0.001 and raises it to the field's
/// norm, one over the square root of its word count. `min_run` is Fuse's
/// `minMatchCharLength`: some run of that many pattern characters must
/// appear unbroken in the text.
fn fuse_score(text: &str, pattern: &str, threshold: f64, min_run: usize) -> Option<f64> {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    if pattern.is_empty() || text.is_empty() {
        return None;
    }
    // Sellers' algorithm: the edit distance of the pattern to the best
    // substring of the text, a free start anywhere.
    let mut previous: Vec<usize> = (0..=pattern.len()).collect();
    let mut best = pattern.len();
    for &character in &text {
        let mut current = vec![0; pattern.len() + 1];
        for (index, &expected) in pattern.iter().enumerate() {
            let substitution = previous[index] + usize::from(expected != character);
            current[index + 1] = substitution
                .min(previous[index + 1] + 1)
                .min(current[index] + 1);
        }
        best = best.min(current[pattern.len()]);
        previous = current;
    }
    let score = best as f64 / pattern.len() as f64;
    if score > threshold {
        return None;
    }
    if min_run > 1
        && !pattern
            .windows(min_run.min(pattern.len()))
            .any(|run| text.windows(run.len()).any(|window| window == run))
    {
        return None;
    }
    let words = text
        .iter()
        .collect::<String>()
        .split(' ')
        .filter(|word| !word.is_empty())
        .count()
        .max(1);
    let norm = (1000.0 / (words as f64).sqrt()).round() / 1000.0;
    Some(score.max(0.001).powf(norm))
}

const SCORE_MATCH: f64 = 16.0;
const GAP_START: f64 = -3.0;
const GAP_EXTENSION: f64 = -1.0;
const BONUS_AFTER_WHITESPACE: i8 = 10;
const BONUS_AFTER_DELIMITER: i8 = 9;
const BONUS_AFTER_PUNCTUATION: i8 = 8;
const BONUS_CAMEL: i8 = 7;
const BONUS_CONSECUTIVE: i8 = 4;
const FIRST_LETTER_MULTIPLIER: f64 = 2.0;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CharClass {
    Whitespace,
    Delimiter,
    Punctuation,
    Lower,
    Upper,
    Digit,
}

fn classify(character: char) -> CharClass {
    if character == '/' || character == '\\' {
        return CharClass::Delimiter;
    }
    if character.is_ascii_digit() {
        return CharClass::Digit;
    }
    let lower: String = character.to_lowercase().collect();
    if lower != character.to_string() {
        return CharClass::Upper;
    }
    let upper: String = character.to_uppercase().collect();
    if upper != character.to_string() {
        return CharClass::Lower;
    }
    if character.is_whitespace() {
        CharClass::Whitespace
    } else {
        CharClass::Punctuation
    }
}

fn position_bonuses(text: &[char]) -> Vec<i8> {
    let mut bonuses = vec![0; text.len()];
    let mut previous = CharClass::Whitespace;
    for (index, &character) in text.iter().enumerate() {
        let current = classify(character);
        let letter_or_digit = current >= CharClass::Lower;
        bonuses[index] = if letter_or_digit && previous == CharClass::Whitespace {
            BONUS_AFTER_WHITESPACE
        } else if letter_or_digit && previous == CharClass::Delimiter {
            BONUS_AFTER_DELIMITER
        } else if letter_or_digit && previous == CharClass::Punctuation {
            BONUS_AFTER_PUNCTUATION
        } else if (current == CharClass::Upper && previous == CharClass::Lower)
            || (current == CharClass::Digit
                && matches!(previous, CharClass::Lower | CharClass::Upper))
            || (current == CharClass::Upper
                && previous == CharClass::Upper
                && text
                    .get(index + 1)
                    .is_some_and(|next| classify(*next) == CharClass::Lower))
        {
            BONUS_CAMEL
        } else if current == CharClass::Punctuation {
            BONUS_AFTER_PUNCTUATION
        } else {
            0
        };
        previous = current;
    }
    bonuses
}

/// Lowercase per position, so an offset in the lowered text is one in the text.
fn lower_per_position(text: &[char]) -> Vec<char> {
    text.iter()
        .map(|&character| {
            let mut lowered = character.to_lowercase();
            match (lowered.next(), lowered.next()) {
                (Some(single), None) => single,
                _ => character,
            }
        })
        .collect()
}

struct TokenMatch {
    score: f64,
    positions: Vec<usize>,
}

fn is_subsequence(lower: &[char], token: &[char]) -> bool {
    let mut at = 0;
    for character in token {
        match lower[at..]
            .iter()
            .position(|candidate| candidate == character)
        {
            Some(offset) => at += offset + 1,
            None => return false,
        }
    }
    true
}

/// The best placement of the token's letters in order, its first on a word start.
fn align_token(lower: &[char], bonuses: &[i8], token: &[char]) -> Option<TokenMatch> {
    let m = token.len();
    let n = lower.len();
    let mut scores = vec![f64::NEG_INFINITY; m * n];
    let mut run_bonuses = vec![0i8; m * n];
    let mut previous_at = vec![usize::MAX; m * n];
    for j in 0..n {
        if lower[j] == token[0] && bonuses[j] >= BONUS_CAMEL {
            scores[j] = SCORE_MATCH + f64::from(bonuses[j]) * FIRST_LETTER_MULTIPLIER;
            run_bonuses[j] = bonuses[j];
        }
    }
    for (i, &expected) in token.iter().enumerate().skip(1) {
        let row = i * n;
        let previous_row = row - n;
        let mut gap_best = f64::NEG_INFINITY;
        let mut gap_best_at = usize::MAX;
        for j in i..n {
            if j >= 2 {
                let from_further = gap_best + GAP_EXTENSION;
                let from_adjacent = scores[previous_row + j - 2] + GAP_START;
                if from_adjacent >= from_further {
                    gap_best = from_adjacent;
                    gap_best_at = j - 2;
                } else {
                    gap_best = from_further;
                }
            }
            if lower[j] != expected {
                continue;
            }
            let bonus = bonuses[j];
            let via_gap = gap_best + SCORE_MATCH + f64::from(bonus);
            let consecutive = scores[previous_row + j - 1];
            let run_bonus = run_bonuses[previous_row + j - 1]
                .max(BONUS_CONSECUTIVE)
                .max(bonus);
            let via_run = consecutive + SCORE_MATCH + f64::from(run_bonus);
            if via_run >= via_gap && via_run > f64::NEG_INFINITY {
                scores[row + j] = via_run;
                run_bonuses[row + j] = run_bonus;
                previous_at[row + j] = j - 1;
            } else if via_gap > f64::NEG_INFINITY {
                scores[row + j] = via_gap;
                run_bonuses[row + j] = bonus;
                previous_at[row + j] = gap_best_at;
            }
        }
    }
    let last_row = (m - 1) * n;
    let mut best_at: Option<usize> = None;
    for j in m - 1..n {
        let score = scores[last_row + j];
        if score > f64::NEG_INFINITY && best_at.is_none_or(|best| score > scores[last_row + best]) {
            best_at = Some(j);
        }
    }
    let best_at = best_at?;
    let mut positions = vec![0; m];
    let mut at = best_at;
    for i in (0..m).rev() {
        positions[i] = at;
        at = previous_at[i * n + at];
    }
    Some(TokenMatch {
        score: scores[last_row + best_at],
        positions,
    })
}

/// The token unbroken at the first place it appears.
fn substring_token(lower: &[char], bonuses: &[i8], token: &[char]) -> Option<TokenMatch> {
    let at = char_index(lower, token)?;
    let first = f64::from(bonuses[at]);
    let length = token.len() as f64;
    let score = SCORE_MATCH * length
        + first * FIRST_LETTER_MULTIPLIER
        + (length - 1.0) * first.max(f64::from(BONUS_CONSECUTIVE));
    Some(TokenMatch {
        score,
        positions: (at..at + token.len()).collect(),
    })
}

fn match_token(lower: &[char], bonuses: &[i8], token: &[char]) -> Option<TokenMatch> {
    match (
        align_token(lower, bonuses, token),
        substring_token(lower, bonuses, token),
    ) {
        (None, unbroken) => unbroken,
        (aligned, None) => aligned,
        (Some(aligned), Some(unbroken)) => Some(if unbroken.score > aligned.score {
            unbroken
        } else {
            aligned
        }),
    }
}

fn perfect_token_score(length: usize) -> f64 {
    let length = length as f64;
    SCORE_MATCH * length
        + f64::from(BONUS_AFTER_WHITESPACE) * FIRST_LETTER_MULTIPLIER
        + (length - 1.0) * f64::from(BONUS_AFTER_WHITESPACE)
}

fn to_ranges(positions: &[usize]) -> Vec<(usize, usize)> {
    let mut sorted = positions.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for position in sorted {
        match ranges.last_mut() {
            Some(last) if last.1 == position => last.1 = position + 1,
            _ => ranges.push((position, position + 1)),
        }
    }
    ranges
}

/// How well and where a query matched, in character offsets.
#[derive(Clone, Debug, PartialEq)]
pub struct SubsequenceMatch {
    /// 0 to 1, higher is better; 1 is the query at the start of the text.
    pub score: f64,
    pub ranges: Vec<(usize, usize)>,
}

fn match_tokens(text: &[char], tokens: &[Vec<char>]) -> Option<SubsequenceMatch> {
    if text.is_empty() {
        return None;
    }
    let lower = lower_per_position(text);
    if !tokens
        .iter()
        .all(|token| token.len() <= lower.len() && is_subsequence(&lower, token))
    {
        return None;
    }
    let bonuses = position_bonuses(text);
    let mut total = 0.0;
    let mut perfect = 0.0;
    let mut positions = Vec::new();
    for token in tokens {
        let matched = match_token(&lower, &bonuses, token)?;
        total += matched.score;
        perfect += perfect_token_score(token.len());
        positions.extend(matched.positions);
    }
    Some(SubsequenceMatch {
        score: (total / perfect).min(1.0),
        ranges: to_ranges(&positions),
    })
}

/// A path's name ranks above a match spread across its folders; a query with
/// a slash is about folders and matches the whole path.
fn match_path_tokens(path: &[char], tokens: &[Vec<char>]) -> Option<SubsequenceMatch> {
    if !tokens.iter().any(|token| token.contains(&'/')) {
        let end = if path.last() == Some(&'/') {
            path.len() - 1
        } else {
            path.len()
        };
        let start = path[..end]
            .iter()
            .rposition(|character| *character == '/')
            .map_or(0, |index| index + 1);
        if let Some(in_name) = match_tokens(&path[start..end], tokens) {
            return Some(SubsequenceMatch {
                score: 0.5 + in_name.score / 2.0,
                ranges: in_name
                    .ranges
                    .into_iter()
                    .map(|(from, to)| (from + start, to + start))
                    .collect(),
            });
        }
    }
    match_tokens(path, tokens).map(|matched| SubsequenceMatch {
        score: matched.score / 2.0,
        ranges: matched.ranges,
    })
}

/// `rankBySubsequence`: the items whose text holds the query's letters in
/// order, best first, the shorter text then the original order breaking ties.
/// Without a query every item comes back in order. Returns indices and matches.
pub fn rank_by_subsequence<T, F>(
    items: &[T],
    query: &str,
    text: F,
    limit: usize,
    paths: bool,
) -> Vec<(usize, SubsequenceMatch)>
where
    F: Fn(&T) -> &str,
{
    let tokens: Vec<Vec<char>> = rank_tokens(query)
        .iter()
        .map(|token| token.chars().collect())
        .collect();
    if tokens.is_empty() {
        return (0..items.len().min(limit))
            .map(|index| {
                (
                    index,
                    SubsequenceMatch {
                        score: 1.0,
                        ranges: Vec::new(),
                    },
                )
            })
            .collect();
    }
    let mut ranked: Vec<(usize, usize, SubsequenceMatch)> = Vec::new();
    for (order, item) in items.iter().enumerate() {
        let characters: Vec<char> = text(item).chars().collect();
        let matched = if paths {
            match_path_tokens(&characters, &tokens)
        } else {
            match_tokens(&characters, &tokens)
        };
        if let Some(matched) = matched {
            ranked.push((order, characters.len(), matched));
        }
    }
    ranked.sort_by(|a, b| {
        b.2.score
            .total_cmp(&a.2.score)
            .then(a.1.cmp(&b.1))
            .then(a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(order, _, matched)| (order, matched))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank<'a>(items: &[&'a str], query: &str) -> Vec<&'a str> {
        rank_by_query(items, query, |item| vec![Some(*item)], true)
            .into_iter()
            .map(|index| items[index])
            .collect()
    }

    #[test]
    fn rank_orders_boundaries_prefixes_and_ties_as_the_web() {
        assert_eq!(
            rank(
                &["prefixed-thing", "workspace-fix", "feat/fix-scroll"],
                "fix"
            ),
            ["feat/fix-scroll", "workspace-fix", "prefixed-thing"]
        );
        let ranked = rank(
            &["main", "feat/main-menu", "maintenance", "release/main"],
            "main",
        );
        assert_eq!(
            ranked,
            ["main", "maintenance", "feat/main-menu", "release/main"]
        );
        assert_eq!(
            rank(
                &[
                    "feat/scroll-anchored-chat",
                    "fix/chat-header",
                    "feat/scroll-perf"
                ],
                "chat scroll"
            ),
            ["feat/scroll-anchored-chat"]
        );
        assert_eq!(
            rank(&["gpt-4o-mini", "claude-sonnet-5"], "gpt4o"),
            ["gpt-4o-mini"]
        );
        assert!(rank(&["alpha", "beta"], "--").is_empty());
        assert_eq!(rank(&["b", "a"], "  "), ["b", "a"]);
    }

    #[test]
    fn single_tokens_tolerate_typos_unless_fuzzy_is_off() {
        let items = ["workspace-rail-layout", "unrelated"];
        assert_eq!(rank(&items, "worskpace"), ["workspace-rail-layout"]);
        assert!(rank_by_query(&items, "worskpace", |item| vec![Some(*item)], false).is_empty());
    }

    #[test]
    fn earlier_fields_outrank_later_ones() {
        let items = [
            ("docs", "/repo/build-agent"),
            ("build-agent", "/repo/build-agent"),
        ];
        let ranked = rank_by_query(
            &items,
            "build",
            |item| vec![Some(item.0), Some(item.1)],
            true,
        );
        assert_eq!(ranked, [1, 0]);
    }

    #[test]
    fn rank_query_matching_requires_every_token() {
        assert!(matches_rank_query(
            [Some("GLM-5.3"), Some("Zhipu")],
            "zhipu glm"
        ));
        assert!(!matches_rank_query(
            [Some("GLM-5.3"), Some("Zhipu")],
            "zhipu gpt"
        ));
        assert!(matches_rank_query(
            [None, Some("claude-sonnet-5")],
            "sonnet5"
        ));
        assert!(!matches_rank_query([Some("alpha")], "... alpha"));
    }

    #[test]
    fn fuzzy_queries_accept_substrings_and_near_misses() {
        assert!(matches_fuzzy_query("Review", "rev"));
        assert!(matches_fuzzy_query("workspace", "wrkspace"));
        assert!(!matches_fuzzy_query("compact", "xyz"));
        assert!(matches_fuzzy_query("anything", ""));
        assert!(!matches_fuzzy_query("", "a"));
    }

    fn marked(text: &str, query: &str) -> Option<Vec<String>> {
        let characters: Vec<char> = text.chars().collect();
        rank_by_subsequence(&[text], query, |item| item, usize::MAX, false)
            .first()
            .map(|(_, matched)| {
                matched
                    .ranges
                    .iter()
                    .map(|(from, to)| characters[*from..*to].iter().collect())
                    .collect()
            })
    }

    #[test]
    fn subsequences_place_letters_on_word_starts() {
        assert_eq!(
            marked("CommandPalette.tsx", "cmdpal").expect("a match"),
            ["C", "m", "dPal"]
        );
        assert_eq!(
            marked("FileMentionAutocomplete.tsx", "fma").expect("a match"),
            ["F", "M", "A"]
        );
        assert_eq!(
            marked("useUIStore.ts", "store").expect("a match"),
            ["Store"]
        );
        assert!(marked("machine-learning/tensorflow/", "solo").is_none());
        assert_eq!(
            marked("reconfigure", "config").expect("a match"),
            ["config"]
        );
        assert!(marked("workspace rail layout", "worskpace").is_none());
        assert_eq!(
            marked("Рефакторинг палітри", "палітр").expect("a match"),
            ["палітр"]
        );
    }

    #[test]
    fn subsequence_ties_go_to_shorter_text_then_order() {
        let items = ["maintenance", "feat/main-menu", "main", "release/main"];
        let ranked: Vec<&str> = rank_by_subsequence(&items, "main", |item| item, 20, false)
            .into_iter()
            .map(|(index, _)| items[index])
            .collect();
        assert_eq!(
            ranked,
            ["main", "maintenance", "release/main", "feat/main-menu"]
        );
    }

    #[test]
    fn paths_rank_their_names_first() {
        let paths = ["ui/stores/index.ts", "lib/useUIStore.ts"];
        let ranked = rank_by_subsequence(&paths, "uistore", |item| item, 20, true);
        assert_eq!(paths[ranked[0].0], "lib/useUIStore.ts");
        let folders = ["solo-is-a-team-size/index.md", "solo-is-a-team-size/"];
        let ranked: Vec<&str> = rank_by_subsequence(&folders, "solo", |item| item, 20, true)
            .into_iter()
            .map(|(index, _)| folders[index])
            .collect();
        assert_eq!(
            ranked,
            ["solo-is-a-team-size/", "solo-is-a-team-size/index.md"]
        );
        let slashed = ["src/ui/Button.tsx", "src/Button.tsx"];
        let ranked = rank_by_subsequence(&slashed, "ui/but", |item| item, 20, true);
        assert_eq!(ranked.len(), 1);
        assert_eq!(slashed[ranked[0].0], "src/ui/Button.tsx");
    }
}
