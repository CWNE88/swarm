//! Web addresses in what's said: found, so the window can show them as
//! links, and opened in the system's own browser when one's clicked.
//! Only http and https: nothing else said in a chat opens anything.

use std::ops::Range;
use std::sync::OnceLock;

/// A character that shows as nothing: no part of a link shown.
fn is_unseen(c: char) -> bool {
    matches!(c, '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}' | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}')
}

/// Where each web address in `text` is, in order: from `http://` or
/// `https://` (any case, not inside a word) to the next space, less the
/// punctuation a sentence puts after one (`see https://x.org/a.`) and a
/// closing bracket it didn't open (`(https://x.org/a)`).
pub fn find(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("http").map(|i| from + i) {
        from = at + 4;
        let rest = &lower[at..];
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            continue;
        };
        if text[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        // Ended by anything unseen too (a zero-width space, a soft hyphen,
        // a direction mark), so what's shown is all of where it goes.
        let mut end = text[at..].find(|c: char| c.is_whitespace() || c.is_control() || is_unseen(c) || matches!(c, '<' | '>' | '"' | '`')).map_or(text.len(), |i| at + i);
        while let Some(last) = text[..end].chars().next_back() {
            let unopened = |open: char, close: char| last == close && text[at..end].matches(close).count() > text[at..end].matches(open).count();
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '_') || unopened('(', ')') || unopened('[', ']') || unopened('{', '}') {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        if end > at + scheme {
            found.push(at..end);
            from = end;
        }
    }
    found
}

/// `text` as CommonMark for Slint's styled text, its web addresses as
/// links and everything else exactly as written (every ASCII punctuation
/// mark escaped, so nothing's taken as markup); italic if `italic`.
/// `None` if there are no addresses in it. Each line is a paragraph of
/// its own, a blank one a no-break space, and the spaces a line starts
/// with no-break spaces, so it's laid out as the plain text would be.
pub fn markdown(text: &str, italic: bool) -> Option<String> {
    if find(text).is_empty() {
        return None;
    }
    let mut out = String::new();
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let line = line.trim_end();
        let words = line.trim_start_matches([' ', '\t']);
        for _ in 0..line.len() - words.len() {
            out.push('\u{a0}');
        }
        if words.is_empty() {
            if line.is_empty() {
                out.push('\u{a0}');
            }
            continue;
        }
        if italic {
            out.push('*');
        }
        let mut done = 0;
        for link in find(words) {
            escape(&words[done..link.start], &mut out);
            out.push('[');
            escape(&words[link.clone()], &mut out);
            out.push_str("](<");
            for c in words[link.clone()].chars() {
                if c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push_str(">)");
            done = link.end;
        }
        escape(&words[done..], &mut out);
        if italic {
            out.push('*');
        }
    }
    Some(out)
}

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Whether the whole of `text` is one web address, as `find` finds them.
fn is_web_address(text: &str) -> bool {
    matches!(find(text).as_slice(), [only] if *only == (0..text.len()))
}

/// How the phone's app opens one (its activity's `openLink`).
static PHONE: OnceLock<fn(&str)> = OnceLock::new();

/// The phone's app: how it opens a web address.
pub fn set_phone(open: fn(&str)) {
    let _ = PHONE.set(open);
}

/// Opens `url` in the system's own browser, if it's a web address (the
/// whole of it, as `find` finds them); anything else is left alone.
pub fn open(url: &str) {
    if !is_web_address(url) {
        eprintln!("swarm: links: not opening {url:?}: not a web address");
        return;
    }
    if let Some(open) = PHONE.get() {
        open(url);
        return;
    }
    if let Err(e) = open_here(url) {
        eprintln!("swarm: links: couldn't open {url}: {e}");
    }
}

#[cfg(windows)]
fn open_here(url: &str) -> anyhow::Result<()> {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let done = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL) };
    anyhow::ensure!(done.0 as isize > 32, "ShellExecute gave {}", done.0 as isize);
    Ok(())
}

#[cfg(all(unix, not(target_os = "android")))]
fn open_here(url: &str) -> anyhow::Result<()> {
    let mut child = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(target_os = "android")]
fn open_here(_url: &str) -> anyhow::Result<()> {
    anyhow::bail!("no browser hook set")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(text: &str) -> Vec<&str> {
        find(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn web_addresses_are_found_without_what_follows_them() {
        assert_eq!(links("see https://example.org/a."), ["https://example.org/a"]);
        assert_eq!(links("(https://en.wikipedia.org/wiki/Swarm_(disambiguation))"), ["https://en.wikipedia.org/wiki/Swarm_(disambiguation)"]);
        assert_eq!(links("(see http://x.org/b)"), ["http://x.org/b"]);
        assert_eq!(links("HTTPS://X.ORG, and http://y.org?q=1&r=2!"), ["HTTPS://X.ORG", "http://y.org?q=1&r=2"]);
        assert_eq!(links("one\nhttps://a.org/\ntwo <https://b.org>"), ["https://a.org/", "https://b.org"]);
        assert_eq!(links("https://café.fr/ü"), ["https://café.fr/ü"]);
        // Something unseen ends it: what's shown is all of where it goes.
        assert_eq!(links("https://bank.example\u{200B}.evil.org/login"), ["https://bank.example"]);
        assert_eq!(links("https://a.org/x\u{00AD}y"), ["https://a.org/x"]);
        assert_eq!(links("https://a.org/\u{2066}b"), ["https://a.org/"]);
    }

    #[test]
    fn nothing_else_is_a_link() {
        assert!(links("http:// or https:// alone, xhttp://a.org, ftp://a.org, file:///etc/passwd, javascript:alert(1)").is_empty());
        assert!(links("no links here").is_empty());
    }

    #[test]
    fn only_web_addresses_open() {
        for bad in ["file:///etc/passwd", "https://a.org b", "javascript:x", "https://a.org."] {
            assert!(!is_web_address(bad), "{bad}");
        }
        assert!(is_web_address("https://a.org/x?y=1"));
    }

    #[test]
    fn markup_in_what_is_said_stays_as_written() {
        let md = markdown("# *hi* <b>x</b> & `c` https://a.org/x_(y)", false).unwrap();
        assert_eq!(md, "\\# \\*hi\\* \\<b\\>x\\<\\/b\\> \\& \\`c\\` [https\\:\\/\\/a\\.org\\/x\\_\\(y\\)](<https://a.org/x_(y)>)");
        assert_eq!(markdown("no link", false), None);
    }

    #[test]
    fn lines_keep_their_shape() {
        let md = markdown("first https://a.org\n\n    indented\n1. not a list  ", true).unwrap();
        assert_eq!(md, "*first [https\\:\\/\\/a\\.org](<https://a.org>)*\n\u{a0}\n\u{a0}\u{a0}\u{a0}\u{a0}*indented*\n*1\\. not a list*");
    }
}
