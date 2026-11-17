use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

pub fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn fold(text: &str) -> String {
    squash(text).to_lowercase()
}

pub fn words(text: &str) -> usize {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.chars().count() >= 2 && token.chars().all(char::is_alphabetic))
        .count()
}

pub fn nfkc_squash(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    squash(&nfkc).replace('−', "-")
}

pub fn strip_accents(text: &str) -> String {
    text.nfkd().filter(|c| !is_combining_mark(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_keeps_each_behaviour() {
        assert_eq!(squash("  a \n b\tc "), "a b c");
        assert_eq!(fold(" Pump  P-101 "), "pump p-101");
        assert_eq!(nfkc_squash("７,５\u{a0}kW  −5 °C"), "7,5 kW -5 °C");
        assert_eq!(strip_accents("Café Noël"), "Cafe Noel");
    }

    #[test]
    fn words_are_runs_of_two_or_more_letters() {
        assert_eq!(words("| 12 | 3.5 kW | 400 V |"), 1);
        assert_eq!(words("Suction scrubber V-07, 10K4"), 2);
        assert_eq!(words("Pompe à eau"), 2);
    }
}
