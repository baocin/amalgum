//! Command-palette matching (§5.19). Case-insensitive subsequence match with bonuses for
//! consecutive characters, word starts (after space, `/`, `-`, `_`, `.`, or a lowercase→upper
//! transition), and a prefix match; shorter candidates win ties.

/// Palette result groups, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Actions,
    Workspaces,
    Branches,
    Tags,
    Stashes,
    Worktrees,
    Locations,
    Commits,
}

/// Split a leading group prefix: `>` actions, `@` branches, `#` commits, `$` stashes,
/// `~` locations, `!` workspaces. Returns the group (if any) and the remaining query, trimmed.
pub fn split_prefix(input: &str) -> (Option<Group>, &str) {
    let mut chars = input.chars();
    if let Some(c) = chars.next() {
        let group = match c {
            '>' => Some(Group::Actions),
            '@' => Some(Group::Branches),
            '#' => Some(Group::Commits),
            '$' => Some(Group::Stashes),
            '~' => Some(Group::Locations),
            '!' => Some(Group::Workspaces),
            _ => None,
        };
        if let Some(g) = group {
            return (Some(g), chars.as_str().trim());
        }
    }
    (None, input.trim())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i64,
    /// Char indices of matched characters in the candidate, for highlighting.
    pub positions: Vec<usize>,
}

const SCORE_MATCH: i64 = 16;
const SCORE_CONSECUTIVE: i64 = 8;
const SCORE_WORD_START: i64 = 12;
const SCORE_MATCH_AT_START: i64 = 20;
const MAX_LEADING_PENALTY: i64 = 15;
/// Below any real score (16 + 12 + 20 for a single char is at most 48; this is never reached).
const NEG_INF: i64 = i64::MIN / 2;

fn lower_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_word_start(chars: &[char], idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    let prev = chars[idx - 1];
    if matches!(prev, ' ' | '/' | '-' | '_' | '.') {
        return true;
    }
    let cur = chars[idx];
    prev.is_lowercase() && cur.is_uppercase()
}

/// `None` if `query` is not a subsequence of `candidate`. Empty query matches with score 0.
pub fn score(query: &str, candidate: &str) -> Option<Match> {
    let q: Vec<char> = query.chars().map(lower_char).collect();
    if q.is_empty() {
        return Some(Match { score: 0, positions: Vec::new() });
    }
    let cand_chars: Vec<char> = candidate.chars().collect();
    let c: Vec<char> = cand_chars.iter().map(|&ch| lower_char(ch)).collect();
    if q.len() > c.len() {
        return None;
    }
    let word_start: Vec<bool> = (0..cand_chars.len()).map(|j| is_word_start(&cand_chars, j)).collect();

    // dp[j] = best score matching q[..=i] with the i-th query char landing on candidate[j].
    let mut dp = vec![NEG_INF; c.len()];
    let mut back: Vec<Vec<usize>> = vec![vec![usize::MAX; c.len()]; q.len()];

    for (j, &cj) in c.iter().enumerate() {
        if cj == q[0] {
            let leading_penalty = (j as i64).min(MAX_LEADING_PENALTY);
            let start_bonus = if j == 0 { SCORE_MATCH_AT_START } else { 0 };
            let ws_bonus = if word_start[j] { SCORE_WORD_START } else { 0 };
            dp[j] = SCORE_MATCH + ws_bonus + start_bonus - leading_penalty;
        }
    }

    for (i, &qi) in q.iter().enumerate().skip(1) {
        let mut new_dp = vec![NEG_INF; c.len()];
        // Rolling best of dp[0..=j-1] from the previous row, tracked as we scan j upward.
        let mut running_max = NEG_INF;
        let mut running_max_pos = usize::MAX;
        let mut prev_dp = NEG_INF;
        let mut prev_pos = usize::MAX;
        for (j, &cj) in c.iter().enumerate() {
            if prev_dp > running_max {
                running_max = prev_dp;
                running_max_pos = prev_pos;
            }
            if cj == qi {
                let via_adjacent = if j > 0 && prev_dp > NEG_INF {
                    Some((prev_dp + SCORE_CONSECUTIVE, j - 1))
                } else {
                    None
                };
                let via_gap = if running_max > NEG_INF { Some((running_max, running_max_pos)) } else { None };
                let best_prev = match (via_adjacent, via_gap) {
                    (Some(a), Some(b)) => Some(if a.0 >= b.0 { a } else { b }),
                    (a @ Some(_), None) => a,
                    (None, b @ Some(_)) => b,
                    (None, None) => None,
                };
                if let Some((prev_score, prev_j)) = best_prev {
                    let ws_bonus = if word_start[j] { SCORE_WORD_START } else { 0 };
                    new_dp[j] = SCORE_MATCH + ws_bonus + prev_score;
                    back[i][j] = prev_j;
                }
            }
            prev_dp = dp[j];
            prev_pos = j;
        }
        dp = new_dp;
    }

    let (best_j, best_score) = dp.iter().copied().enumerate().max_by_key(|&(_, s)| s)?;
    if best_score <= NEG_INF {
        return None;
    }

    let mut positions = vec![0usize; q.len()];
    let mut cur_j = best_j;
    for i in (0..q.len()).rev() {
        positions[i] = cur_j;
        if i > 0 {
            cur_j = back[i][cur_j];
        }
    }
    Some(Match { score: best_score, positions })
}

/// Indices of `candidates` that match, best first (stable for equal scores).
pub fn rank(query: &str, candidates: &[&str]) -> Vec<(usize, Match)> {
    let mut matches: Vec<(usize, Match)> =
        candidates.iter().enumerate().filter_map(|(i, cand)| score(query, cand).map(|m| (i, m))).collect();
    // Stable sort: score desc, then shorter candidate, then original order (preserved by
    // stability once the first two keys tie).
    matches.sort_by(|a, b| {
        b.1.score
            .cmp(&a.1.score)
            .then_with(|| candidates[a.0].chars().count().cmp(&candidates[b.0].chars().count()))
    });
    matches
}

/// The Commits group appears only for 7+ hex chars or a `#` prefix.
pub fn looks_like_hash(query: &str) -> bool {
    let (had_hash, rest) = match query.strip_prefix('#') {
        Some(r) => (true, r),
        None => (false, query),
    };
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    had_hash || rest.chars().count() >= 7
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- split_prefix (§5.19) ----

    #[test]
    fn split_prefix_recognizes_every_documented_prefix() {
        assert_eq!(split_prefix(">push"), (Some(Group::Actions), "push"));
        assert_eq!(split_prefix("@main"), (Some(Group::Branches), "main"));
        assert_eq!(split_prefix("#ab12cd3"), (Some(Group::Commits), "ab12cd3"));
        assert_eq!(split_prefix("$wip"), (Some(Group::Stashes), "wip"));
        assert_eq!(split_prefix("~/repo"), (Some(Group::Locations), "/repo"));
        assert_eq!(split_prefix("!conduit"), (Some(Group::Workspaces), "conduit"));
    }

    #[test]
    fn split_prefix_trims_and_handles_no_prefix() {
        assert_eq!(split_prefix(">  push  "), (Some(Group::Actions), "push"));
        assert_eq!(split_prefix("  push  "), (None, "push"));
        assert_eq!(split_prefix(""), (None, ""));
    }

    #[test]
    fn split_prefix_has_no_shortcut_for_tags_or_worktrees() {
        // §5.19 lists only 6 prefixes; Tags and Worktrees are reachable only unprefixed.
        let (group, _) = split_prefix("tag-name");
        assert_eq!(group, None);
    }

    // ---- looks_like_hash ----

    #[test]
    fn looks_like_hash_examples() {
        assert!(looks_like_hash("ab12cd3")); // exactly 7 hex chars
        assert!(looks_like_hash("ab12cd3ef4")); // longer, still all hex
        assert!(!looks_like_hash("ab12cd")); // only 6 hex chars, no '#'
        assert!(looks_like_hash("#a")); // '#' prefix needs only 1 hex char
        assert!(looks_like_hash("#ab12cd3"));
        assert!(!looks_like_hash("#")); // '#' alone: no hex chars at all
        assert!(!looks_like_hash("ab12cdz")); // non-hex char
        assert!(!looks_like_hash(""));
    }

    // ---- score / rank: basic subsequence behaviour ----

    #[test]
    fn empty_query_matches_everything_with_score_zero() {
        let m = score("", "anything").unwrap();
        assert_eq!(m.score, 0);
        assert!(m.positions.is_empty());
    }

    #[test]
    fn non_subsequence_returns_none() {
        assert_eq!(score("xyz", "abc"), None);
        assert_eq!(score("push", "pus"), None); // query longer than candidate
    }

    #[test]
    fn case_insensitive_subsequence_match() {
        let m = score("PUSH", "push to origin").unwrap();
        assert_eq!(m.positions, vec![0, 1, 2, 3]);
        let m2 = score("push", "Push to Origin").unwrap();
        assert_eq!(m2.positions, vec![0, 1, 2, 3]);
    }

    #[test]
    fn unicode_case_insensitive_match() {
        // 'É' lowercases to 'é'; must match through non-ASCII case folding.
        let m = score("café", "CAFÉ terrace").unwrap();
        assert_eq!(m.positions, vec![0, 1, 2, 3]);
    }

    // ---- Scoring bonuses ----

    #[test]
    fn consecutive_chars_score_higher_than_scattered() {
        // "ab" is a run in "abx"; scattered in "axb".
        let consecutive = score("ab", "abx").unwrap();
        let scattered = score("ab", "axb").unwrap();
        assert!(consecutive.score > scattered.score);
    }

    #[test]
    fn match_starting_at_index_zero_scores_higher() {
        let at_start = score("ab", "abx").unwrap();
        let not_at_start = score("ab", "xabx").unwrap();
        assert!(at_start.score > not_at_start.score);
    }

    #[test]
    fn word_start_bonus_after_separator() {
        // "a" matches the word-start right after "/" in "src/auth" at index 4.
        let m = score("a", "src/auth").unwrap();
        assert_eq!(m.positions, vec![4]);
        // Compare against a candidate where the only 'a' is mid-word (no bonus).
        let m2 = score("a", "banana").unwrap();
        assert!(m.score > m2.score);
    }

    #[test]
    fn camel_case_boundary_is_a_word_start() {
        // "gp" matches "GitPane": 'g' at 0 (start of string) and 'P' at the lower->upper
        // camel boundary (index 3), both word starts.
        let m = score("gp", "GitPane").unwrap();
        assert_eq!(m.positions, vec![0, 3]);
        // A candidate where the second letter isn't a camel boundary scores lower.
        let m2 = score("gp", "gap between").unwrap();
        assert!(m.score > m2.score);
    }

    #[test]
    fn path_like_candidate_matches_across_segments() {
        // "sra" matches "src/auth": s(0) r(1, consecutive) a(4, word start after '/').
        let m = score("sra", "src/auth").unwrap();
        assert_eq!(m.positions, vec![0, 1, 4]);
    }

    #[test]
    fn unmatched_leading_chars_are_penalized_and_capped() {
        let early = score("x", "xylophone").unwrap();
        let late = score("x", "aaaaaaaaaaaaaaaaaaaaax").unwrap();
        assert!(early.score > late.score);
        // Penalty caps at 15, so two candidates with a long and a very long unmatched
        // prefix score identically apart from the word-start bonus (neither is a word start).
        let far1 = score("x", &format!("{}x", "a".repeat(20))).unwrap();
        let far2 = score("x", &format!("{}x", "a".repeat(40))).unwrap();
        assert_eq!(far1.score, far2.score);
    }

    // ---- rank: W8 palette examples (§5.19, W8) ----

    #[test]
    fn rank_prefix_match_beats_longer_alternative_with_same_prefix() {
        // Both "Push to origin" and "Push with lease" match "push" identically as an exact
        // 4-char prefix (same score); the shorter candidate breaks the tie (W8).
        let candidates = ["Push to origin", "Push with lease", "Push all tags"];
        let ranked = rank("push", &candidates);
        assert_eq!(ranked.len(), 3, "all three are prefix matches and must all be returned");
        let pos = |name: &str| ranked.iter().position(|(i, _)| candidates[*i] == name).unwrap();
        assert!(
            pos("Push to origin") < pos("Push with lease"),
            "the shorter of two equal-score prefix matches ranks first"
        );
    }

    #[test]
    fn rank_is_stable_for_true_ties() {
        let candidates = ["ab-one", "ab-two"];
        let ranked = rank("ab", &candidates);
        // Identical score and identical length: original order wins.
        assert_eq!(ranked.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn rank_filters_non_matches_and_sorts_by_score() {
        let candidates = ["push", "pull", "unrelated"];
        let ranked = rank("pu", &candidates);
        let names: Vec<&str> = ranked.iter().map(|(i, _)| candidates[*i]).collect();
        assert_eq!(names, vec!["push", "pull"]);
    }

    #[test]
    fn rank_empty_query_returns_all_candidates_shorter_first() {
        let candidates = ["longer name", "short"];
        let ranked = rank("", &candidates);
        assert_eq!(ranked.len(), 2);
        assert!(ranked.iter().all(|(_, m)| m.score == 0));
        assert_eq!(candidates[ranked[0].0], "short");
    }
}
