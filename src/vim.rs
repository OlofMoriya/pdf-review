//! Vim-style navigation keys, as a small state machine so multi-key commands
//! (`gg`, `g12`, `ma`, `'a`, `''`) can be tested without a window.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    /// j / k: scroll by a few lines (+1 down, -1 up).
    Lines(i32),
    /// ⌃d / ⌃u: half a screen.
    HalfScreen(i32),
    /// ⌃f / ⌃b, space / ⇧space: a full screen.
    Screen(i32),
    /// J / K: next / previous page.
    Page(i32),
    FirstPage,
    LastPage,
    /// g followed by digits; re-emitted as each digit is typed (g1, then g12).
    GotoPage(usize),
    SetMark(char),
    JumpMark(char),
    /// '' or ``: back to where the last jump started.
    JumpBack,
    SearchStart,
    SearchNext,
    SearchPrev,
}

#[derive(Debug, PartialEq)]
pub enum Feed {
    Cmd(Cmd),
    /// Part of a longer command; show `Vim::pending` to the user.
    Pending,
    /// Not a navigation key; handle it as a normal shortcut.
    Unhandled,
}

#[derive(Default, Clone, Debug, PartialEq)]
enum Pending {
    #[default]
    None,
    G(String),
    Mark,
    Quote,
}

#[derive(Default)]
pub struct Vim {
    pending: Pending,
}

impl Vim {
    /// What has been typed of an unfinished command, like Vim's showcmd.
    pub fn pending(&self) -> Option<String> {
        match &self.pending {
            Pending::None => None,
            Pending::G(d) => Some(format!("g{d}")),
            Pending::Mark => Some("m".into()),
            Pending::Quote => Some("'".into()),
        }
    }

    pub fn reset(&mut self) {
        self.pending = Pending::None;
    }

    /// `key` is GPUI's key name ("j", "space", "'", ...).
    pub fn feed(&mut self, key: &str, shift: bool, ctrl: bool) -> Feed {
        let ch: Option<char> = {
            let mut it = key.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Some(if shift { c.to_ascii_uppercase() } else { c }),
                _ => None,
            }
        };

        match std::mem::take(&mut self.pending) {
            Pending::G(mut digits) => match ch {
                Some('g') if digits.is_empty() => return Feed::Cmd(Cmd::FirstPage),
                Some(d) if d.is_ascii_digit() && !ctrl => {
                    digits.push(d);
                    let n: usize = digits.parse().unwrap_or(0);
                    self.pending = Pending::G(digits);
                    return if n > 0 { Feed::Cmd(Cmd::GotoPage(n)) } else { Feed::Pending };
                }
                _ if key == "enter" && !digits.is_empty() => return Feed::Pending,
                _ => {} // anything else ends the g-command and is handled fresh below
            },
            Pending::Mark => {
                return match ch {
                    Some(c) if c.is_ascii_alphabetic() => Feed::Cmd(Cmd::SetMark(c)),
                    _ => Feed::Pending, // cancelled; swallow the key like Vim does
                };
            }
            Pending::Quote => {
                return match ch {
                    Some('\'') | Some('`') => Feed::Cmd(Cmd::JumpBack),
                    Some(c) if c.is_ascii_alphabetic() => Feed::Cmd(Cmd::JumpMark(c)),
                    _ => Feed::Pending,
                };
            }
            Pending::None => {}
        }

        if ctrl {
            return match ch {
                Some('d') => Feed::Cmd(Cmd::HalfScreen(1)),
                Some('u') => Feed::Cmd(Cmd::HalfScreen(-1)),
                Some('f') => Feed::Cmd(Cmd::Screen(1)),
                Some('b') => Feed::Cmd(Cmd::Screen(-1)),
                Some('e') => Feed::Cmd(Cmd::Lines(1)),
                Some('y') => Feed::Cmd(Cmd::Lines(-1)),
                _ => Feed::Unhandled,
            };
        }
        if key == "space" {
            return Feed::Cmd(Cmd::Screen(if shift { -1 } else { 1 }));
        }
        match ch {
            Some('j') => Feed::Cmd(Cmd::Lines(1)),
            Some('k') => Feed::Cmd(Cmd::Lines(-1)),
            Some('J') => Feed::Cmd(Cmd::Page(1)),
            Some('K') => Feed::Cmd(Cmd::Page(-1)),
            Some('G') => Feed::Cmd(Cmd::LastPage),
            Some('g') => {
                self.pending = Pending::G(String::new());
                Feed::Pending
            }
            Some('m') => {
                self.pending = Pending::Mark;
                Feed::Pending
            }
            Some('\'') | Some('`') => {
                self.pending = Pending::Quote;
                Feed::Pending
            }
            Some('/') => Feed::Cmd(Cmd::SearchStart),
            Some('n') => Feed::Cmd(Cmd::SearchNext),
            Some('N') => Feed::Cmd(Cmd::SearchPrev),
            _ => Feed::Unhandled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(keys: &[&str]) -> Vec<Feed> {
        let mut v = Vim::default();
        keys.iter()
            .map(|k| {
                let shift = k.len() == 1 && k.chars().next().unwrap().is_ascii_uppercase();
                v.feed(&k.to_lowercase(), shift, false)
            })
            .collect()
    }

    #[test]
    fn page_jumps() {
        assert_eq!(run(&["g", "g"]), vec![Feed::Pending, Feed::Cmd(Cmd::FirstPage)]);
        assert_eq!(run(&["g", "1"]), vec![Feed::Pending, Feed::Cmd(Cmd::GotoPage(1))]);
        assert_eq!(run(&["g", "1", "2"]), vec![Feed::Pending, Feed::Cmd(Cmd::GotoPage(1)), Feed::Cmd(Cmd::GotoPage(12))]);
        assert_eq!(run(&["G"]), vec![Feed::Cmd(Cmd::LastPage)]);
        // a non-digit ends g12 and runs as its own key
        assert_eq!(run(&["g", "3", "j"]), vec![Feed::Pending, Feed::Cmd(Cmd::GotoPage(3)), Feed::Cmd(Cmd::Lines(1))]);
        // g then an unrelated tool key falls through to normal handling
        assert_eq!(run(&["g", "h"]), vec![Feed::Pending, Feed::Unhandled]);
    }

    #[test]
    fn marks() {
        assert_eq!(run(&["m", "a"]), vec![Feed::Pending, Feed::Cmd(Cmd::SetMark('a'))]);
        assert_eq!(run(&["'", "a"]), vec![Feed::Pending, Feed::Cmd(Cmd::JumpMark('a'))]);
        assert_eq!(run(&["`", "B"]), vec![Feed::Pending, Feed::Cmd(Cmd::JumpMark('B'))]);
        assert_eq!(run(&["'", "'"]), vec![Feed::Pending, Feed::Cmd(Cmd::JumpBack)]);
        // m then a non-letter cancels without side effects
        assert_eq!(run(&["m", "1", "j"]), vec![Feed::Pending, Feed::Pending, Feed::Cmd(Cmd::Lines(1))]);
    }

    #[test]
    fn motions_and_search() {
        assert_eq!(run(&["j", "k", "J", "K"]), vec![
            Feed::Cmd(Cmd::Lines(1)),
            Feed::Cmd(Cmd::Lines(-1)),
            Feed::Cmd(Cmd::Page(1)),
            Feed::Cmd(Cmd::Page(-1)),
        ]);
        assert_eq!(run(&["/", "n", "N"]), vec![Feed::Cmd(Cmd::SearchStart), Feed::Cmd(Cmd::SearchNext), Feed::Cmd(Cmd::SearchPrev)]);
        let mut v = Vim::default();
        assert_eq!(v.feed("d", false, true), Feed::Cmd(Cmd::HalfScreen(1)));
        assert_eq!(v.feed("space", true, false), Feed::Cmd(Cmd::Screen(-1)));
        assert_eq!(v.feed("h", false, false), Feed::Unhandled);
    }
}
