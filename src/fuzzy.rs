pub fn fuzzy_score(pattern: &str, candidate: &str) -> Option<i32> {
    let pattern: Vec<char> = pattern
        .chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    if pattern.is_empty() {
        return Some(0);
    }

    let candidate: Vec<char> = candidate
        .chars()
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    if candidate.len() < pattern.len() {
        return None;
    }

    let mut score = 0;
    let mut search_from = 0;
    let mut first_match = None;
    let mut previous_match = None;

    for pat in pattern {
        let matched = candidate
            .iter()
            .enumerate()
            .skip(search_from)
            .find_map(|(index, ch)| (*ch == pat).then_some(index))?;

        first_match.get_or_insert(matched);
        score += 1_000;

        if matched == 0 {
            score += 250;
        } else if is_word_start(candidate[matched - 1]) {
            score += 180;
        }

        if let Some(previous) = previous_match {
            if matched == previous + 1 {
                score += 350;
            } else {
                score -= (matched - previous - 1) as i32 * 12;
            }
        }

        previous_match = Some(matched);
        search_from = matched + 1;
    }

    score -= first_match.unwrap_or(0) as i32 * 24;
    score -= candidate.len() as i32;
    Some(score)
}

fn is_word_start(ch: char) -> bool {
    matches!(ch, ' ' | '-' | '_' | '/' | '\\' | ':' | '(' | '[')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ordered_characters() {
        assert!(fuzzy_score("wm", "Webcam Microphone").is_some());
        assert!(fuzzy_score("mw", "Webcam Microphone").is_none());
    }

    #[test]
    fn rewards_tighter_matches() {
        let tight = fuzzy_score("mic", "USB Microphone").unwrap();
        let loose = fuzzy_score("mic", "M audio input capture").unwrap();
        assert!(tight > loose);
    }
}
