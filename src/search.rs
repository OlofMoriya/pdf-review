//! Text search over a page's characters, with Vim's smartcase: case-insensitive
//! unless the query contains an uppercase letter. Whitespace in the query matches
//! any run of whitespace in the text (line breaks, double spaces). Look-alike characters
//! (curly and straight quotes, the various hyphens and dashes) match each other, since
//! quotes reach us through many layers of copying.

/// Start and end (inclusive) indices into `hay` for every match, left to right.
pub fn find(hay: &[char], query: &str) -> Vec<(usize, usize)> {
    find_with(hay, query, query.chars().any(|c| c.is_uppercase()))
}

/// Like `find`, with case sensitivity chosen by the caller.
pub fn find_with(hay: &[char], query: &str, exact: bool) -> Vec<(usize, usize)> {
    let words: Vec<Vec<char>> = query.split_whitespace().map(|w| w.chars().collect()).collect();
    if words.is_empty() {
        return vec![];
    }
    let eq = |a: char, b: char| {
        let (a, b) = (fold(a), fold(b));
        if exact { a == b } else { a.to_lowercase().eq(b.to_lowercase()) }
    };

    let mut out = vec![];
    let mut i = 0;
    while i < hay.len() {
        if let Some(end) = match_at(hay, i, &words, &eq) {
            out.push((i, end));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// One representative for characters that look alike.
fn fold(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' | '\u{00B4}' | '`' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' | '\u{00AB}' | '\u{00BB}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' | '\u{FE63}' | '\u{FF0D}' => '-',
        _ => c,
    }
}

fn match_at(hay: &[char], start: usize, words: &[Vec<char>], eq: &impl Fn(char, char) -> bool) -> Option<usize> {
    let mut j = start;
    for (wi, w) in words.iter().enumerate() {
        if wi > 0 {
            // at least one whitespace char between words
            if !hay.get(j).is_some_and(|c| c.is_whitespace()) {
                return None;
            }
            while hay.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
        }
        for &q in w {
            if !hay.get(j).is_some_and(|&c| eq(c, q)) {
                return None;
            }
            j += 1;
        }
    }
    Some(j - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(h: &str, q: &str) -> Vec<(usize, usize)> {
        find(&h.chars().collect::<Vec<_>>(), q)
    }

    #[test]
    fn smartcase() {
        assert_eq!(f("BRP and brp", "brp"), vec![(0, 2), (8, 10)]);
        assert_eq!(f("BRP and brp", "BRP"), vec![(0, 2)]);
        assert_eq!(f("Åsa åsa", "åsa"), vec![(0, 2), (4, 6)]);
    }

    #[test]
    fn whitespace_spans_lines() {
        assert_eq!(f("payment\n  providers", "payment providers"), vec![(0, 18)]);
        assert_eq!(f("paymentproviders", "payment providers"), vec![]);
        assert_eq!(f("abc", "  "), vec![]);
    }

    #[test]
    fn look_alikes_match() {
        assert_eq!(f("CERT\u{2011}SE reports", "CERT-SE reports"), vec![(0, 14)]);
        assert_eq!(f("CERT-SE", "CERT\u{2013}SE"), vec![(0, 6)]);
        assert_eq!(f("the member\u{2019}s card", "member's"), vec![(4, 11)]);
        assert_eq!(f("\u{201C}fail\u{201D}", "\"fail\""), vec![(0, 5)]);
        assert_eq!(f("24/7\u{00A0}access", "24/7 access"), vec![(0, 10)], "non-breaking space is whitespace");
    }

    #[test]
    fn non_overlapping() {
        assert_eq!(f("aaaa", "aa"), vec![(0, 1), (2, 3)]);
    }
}
