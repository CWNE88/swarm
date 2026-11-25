//! Sizes and lengths as people say them.

pub use swarm_core::text::human_size;

/// Seconds, roughly: `40 s`, `5 min`, `2 h 10 min`.
pub fn rough_duration(seconds: f64) -> String {
    let s = seconds.round() as u64;
    if s < 60 {
        format!("{} s", s.max(1))
    } else if s < 3600 {
        format!("{} min", (s + 30) / 60)
    } else {
        format!("{} h {} min", s / 3600, (s % 3600) / 60)
    }
}

/// `name` in at most `chars` characters: cut in the middle with `…`,
/// keeping a short extension, so a file still shows what kind it is
/// wherever it has to fit one line.
pub fn shorten_middle(name: &str, chars: usize) -> String {
    let count = name.chars().count();
    if count <= chars {
        return name.to_string();
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e).filter(|e| e.chars().count() <= 12 && e.chars().count() + 3 < chars);
    let tail = ext.map(|e| format!(".{e}")).unwrap_or_default();
    let head: String = name.chars().take(chars.saturating_sub(tail.chars().count() + 1)).collect();
    format!("{head}…{tail}")
}

/// `text` with its first letter a capital: a question asked in a
/// line of its own, rather than the status line.
pub fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_name_keeps_its_extension() {
        assert_eq!(shorten_middle("a.pdf", 10), "a.pdf");
        let cut = shorten_middle(&format!("{}.exe", "invoice".repeat(10)), 30);
        assert!(cut.ends_with("….exe") && cut.chars().count() <= 30, "{cut}");
    }

    #[test]
    fn a_question_starts_with_a_capital() {
        assert_eq!(capitalised("remove éva? Yes"), "Remove éva? Yes");
        assert_eq!(capitalised(""), "");
    }
}
