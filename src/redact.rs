//! Keeping credentials and terminal control sequences out of everything we
//! print or persist.
//!
//! Two different hazards, both of which reached the same places -- the terminal
//! and the evidence log -- through the same channel, which is restic's error
//! text.
//!
//! **Credentials.** A peer's repository URL carries HTTP basic-auth
//! credentials, and restic embeds the repository URL in its own error prose:
//! `Fatal: create repository at rest:http://me:pw@127.0.0.1:8023/me/ failed:
//! config file already exists`. That text was copied verbatim into
//! `EngineError::message`, printed to the terminal, and appended to
//! `evidence.jsonl` forever.
//!
//! **Control sequences.** The same text is partly written by the peer. restic
//! prints a server's HTTP status line verbatim, and restic's warnings during a
//! backup embed source paths, which an attacker-writable directory under
//! `sources` lets someone choose. `\x1b[1A\x1b[2K` moves the cursor up and
//! erases a line, so a peer could rewrite the `FAILED` row an operator was
//! looking at. The exit code was never at risk; the human reading the table was.

/// Hide the password in a repository URL before printing it.
///
/// The separator is the *last* `@` in the authority segment, not the first.
/// Using the first leaked any password containing an `@`: given
/// `rest:https://me:p@ssw0rd@host/`, the first `@` sits inside the password, so
/// everything from there on was treated as the host and printed verbatim,
/// producing `me:***@ssw0rd@host/`. `peer list` is the command people paste
/// into bug reports, which is the exact thing the config and secret split
/// exists to make safe.
///
/// Bounded to the authority segment so an `@` later in the path cannot be
/// mistaken for the credential separator.
///
/// The slicing is safe despite the lint: every index below comes from `find`
/// or `rfind`, which only ever return character boundaries.
#[allow(clippy::string_slice)]
pub fn url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find('/')
        .map_or(url.len(), |i| authority_start + i);

    let authority = &url[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return url.to_owned();
    };
    let credentials = &authority[..at];
    let user = credentials.split(':').next().unwrap_or("");
    if credentials.len() == user.len() {
        // A userinfo with no colon carries no password to hide.
        return url.to_owned();
    }
    format!(
        "{}{user}:***{}",
        &url[..authority_start],
        &url[authority_start + at..]
    )
}

/// Redact every credentialed URL anywhere in a block of free text.
///
/// [`url`] handles one URL that is known to be a URL. This handles restic's
/// prose, where a URL turns up mid-sentence among words that are not URLs.
/// Tokens are split on whitespace and each is redacted on its own; [`url`] stops
/// at the first `/` after the scheme, so trailing punctuation is left alone.
#[must_use]
pub fn urls_in(text: &str) -> String {
    if !text.contains("://") {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for (i, token) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if token.contains("://") {
            out.push_str(&url(token));
        } else {
            out.push_str(token);
        }
    }
    out
}

/// The longest `detail` worth keeping on a record.
///
/// `EngineError::message` is restic's entire combined stdout and stderr, which
/// for a backup that failed part-way over a large tree is megabytes of per-file
/// JSON. All of it went onto one line of the evidence log. The useful part is
/// the first line or two; the rest is a liability in a file that is meant to be
/// read by a human and walked backwards by a machine.
const MAX_DETAIL: usize = 2000;

/// Make a string from restic, or from a peer, safe to print and to store.
///
/// Redacts credentials, folds every control character to a space so nothing can
/// move the cursor or forge a row, and truncates.
///
/// Control characters are folded rather than dropped so that the text does not
/// silently close up into a different message, and `\n` goes with them because
/// `detail` is displayed as one line under a table -- a newline in it can paint
/// a row that looks like the program's own output.
#[must_use]
pub fn detail(text: &str) -> String {
    let mut s = fold_controls(&urls_in(text), false);
    let trimmed = s.trim();
    if trimmed.len() != s.len() {
        s = trimmed.to_owned();
    }
    if s.chars().count() > MAX_DETAIL {
        let cut = s.char_indices().nth(MAX_DETAIL).map_or(s.len(), |(i, _)| i);
        s.truncate(cut);
        s.push_str(" [...]");
    }
    s
}

/// The same, for text that is printed on its own rather than inside a table.
///
/// restic's errors are legitimately several lines and the operator wants them
/// that way, so newlines survive here. Everything that can move the cursor does
/// not.
#[must_use]
pub fn message(text: &str) -> String {
    fold_controls(&urls_in(text), true)
}

fn fold_controls(text: &str, keep_newlines: bool) -> String {
    text.chars()
        .map(|c| {
            if keep_newlines && c == '\n' {
                c
            } else if c.is_control() || ('\u{80}'..='\u{9f}').contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_hidden_when_urls_are_printed() {
        let out = url("rest:https://me:hunter2@alice.example.org:8000/me/");
        assert!(!out.contains("hunter2"), "password leaked: {out}");
        assert!(out.contains("alice.example.org"));
        assert!(out.contains("me"));
    }

    #[test]
    fn redacting_leaves_urls_without_credentials_alone() {
        for plain in [
            "rest:https://alice.example.org:8000/me/",
            "rest:https://alice.example.org/path/with@sign/",
            "rest:https://user@alice.example.org/me/",
        ] {
            assert_eq!(url(plain), plain);
        }
    }

    #[test]
    fn redacting_hides_the_whole_password_even_when_it_contains_an_at_sign() {
        // The version this replaced used the FIRST '@' in the URL. With a
        // password containing one, everything after it was treated as the host
        // and printed as-is, so `peer list` published most of the password to
        // whatever bug report it was pasted into.
        for (raw, want) in [
            (
                "rest:https://me:hunter2@alice.example.org:8000/me/",
                "rest:https://me:***@alice.example.org:8000/me/",
            ),
            (
                "rest:https://me:p@ssw0rd@alice.example.org:8000/me/",
                "rest:https://me:***@alice.example.org:8000/me/",
            ),
            (
                "rest:https://me:@@@@@alice.example.org/me/",
                "rest:https://me:***@alice.example.org/me/",
            ),
        ] {
            let got = url(raw);
            assert_eq!(got, want, "redacting {raw}");
            assert!(
                !got.contains("ssw0rd") && !got.contains("hunter2"),
                "password survived redaction: {got}"
            );
        }
    }

    #[test]
    fn a_credentialed_url_inside_restic_prose_is_redacted() {
        // The fixture in restic_error.rs proves restic does exactly this.
        let got = urls_in(
            "Fatal: create repository at rest:http://me:hunter2@127.0.0.1:8023/me/ \
             failed: config file already exists",
        );
        assert!(!got.contains("hunter2"), "password leaked: {got}");
        assert!(
            got.contains("127.0.0.1:8023"),
            "must stay diagnosable: {got}"
        );
        assert!(got.contains("config file already exists"), "{got}");
    }

    #[test]
    fn text_with_no_url_in_it_is_left_alone() {
        let s = "Fatal: unable to open config file: Stat: dial tcp: i/o timeout";
        assert_eq!(urls_in(s), s);
    }

    #[test]
    fn control_characters_cannot_repaint_the_status_table() {
        // A peer controls the HTTP status line restic prints verbatim, and can
        // control filenames under a writable source directory. `\x1b[1A\x1b[2K`
        // moves the cursor up and erases the line, which is the FAILED row the
        // operator is reading.
        let hostile = "server said: \u{1b}[2K\rALL GOOD\u{1b}[1A\u{1b}[2K";
        let got = detail(hostile);
        assert!(
            !got.chars().any(char::is_control),
            "control characters survived: {got:?}"
        );
        assert!(
            got.contains("ALL GOOD"),
            "the text itself must survive: {got}"
        );
    }

    #[test]
    fn a_newline_cannot_forge_a_table_row() {
        let forged = "timed out\nalice        ok          just now";
        assert!(!detail(forged).contains('\n'));
    }

    #[test]
    fn a_message_keeps_its_line_breaks_but_not_its_escapes() {
        // restic's errors are legitimately several lines and the operator wants
        // them that way; only the cursor-moving part has to go.
        let got = message("Fatal: one\ntwo\u{1b}[1Athree");
        assert!(got.contains("one\ntwo"), "{got:?}");
        assert!(!got.contains('\u{1b}'), "{got:?}");
    }

    #[test]
    fn an_enormous_detail_is_truncated() {
        // EngineError::message is restic's entire combined output, which for a
        // backup that failed part-way over a large tree is megabytes of per-file
        // JSON, all of it on one line of the evidence log.
        let got = detail(&"x".repeat(500_000));
        assert!(got.len() < 3000, "still {} bytes", got.len());
        assert!(got.ends_with("[...]"), "must say it was cut");
    }

    #[test]
    fn redaction_survives_non_ascii() {
        // `detail` slices by byte offset when it truncates.
        let got = detail(&"é".repeat(5000));
        assert!(got.ends_with("[...]"));
    }
}
