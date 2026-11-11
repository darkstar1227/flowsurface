//! A system notification when an entry's protection failed (M5 d4, condition 65), on macOS only.
//!
//! The text is fixed and the only thing that varies is the account's alias (typed by the user).
//! No symbol, quantity, price, client id, exchange message or credential can reach it: the
//! function takes a [`Notice`] (a fixed table of texts) and the alias, nothing else.
//!
//! The AppleScript is a constant. The message is handed to it as an argument of `osascript`
//! (`item 1 of argv`), never spliced into the script text, so nothing in an alias can become
//! script. The message always starts with `Account `, so no alias can look like an option of
//! `osascript` either. The program is started by its absolute path, with no shell and no input
//! or output.
//!
//! Other platforms do nothing: the banner in the account panel is the notice there.

/// What the notification says. A fixed table: add a kind here, never a free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// An entry's protection failed (a protective order was not placed, or an order could not
    /// be cancelled).
    ProtectionFailed,
}

impl Notice {
    #[cfg(any(target_os = "macos", test))]
    fn text(self) -> &'static str {
        match self {
            Notice::ProtectionFailed => {
                "protection failed. Open the account panel in Flowsurface to see what to do."
            }
        }
    }
}

/// The AppleScript, one line per `-e`. It does not change and holds no outside text.
#[cfg(any(target_os = "macos", test))]
const SCRIPT: [&str; 3] = [
    "on run argv",
    "display notification (item 1 of argv) with title \"Flowsurface\"",
    "end run",
];

#[cfg(any(target_os = "macos", test))]
const ALIAS_MAX_CHARS: usize = 32;

/// Unicode category Cf (format characters): bidi controls and overrides, zero-width marks and
/// joiners, the BOM. `char::is_control` is Cc only, so these are listed by range.
#[cfg(any(target_os = "macos", test))]
fn is_format_char(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{0600}'..='\u{0605}'
        | '\u{061C}'
        | '\u{06DD}'
        | '\u{070F}'
        | '\u{0890}'..='\u{0891}'
        | '\u{08E2}'
        | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{110BD}'
        | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0001}'
        | '\u{E0020}'..='\u{E007F}')
}

/// The accounts to notify, each once and in the order they came.
pub fn unique_accounts(ids: Vec<uuid::Uuid>) -> Vec<uuid::Uuid> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

/// The alias as it is shown: printable characters only, at most 32 of them.
#[cfg(any(target_os = "macos", test))]
fn clean_alias(alias: &str) -> String {
    let cleaned: String = alias
        .chars()
        .filter(|c| !c.is_control() && !is_format_char(*c))
        .take(ALIAS_MAX_CHARS)
        .collect();
    if cleaned.trim().is_empty() {
        "(no alias)".to_string()
    } else {
        cleaned
    }
}

/// The arguments of `osascript`: the fixed script, then the message as the one argument.
#[cfg(any(target_os = "macos", test))]
fn osascript_args(notice: Notice, alias: &str) -> Vec<String> {
    let mut args = Vec::with_capacity(SCRIPT.len() * 2 + 1);
    for line in SCRIPT {
        args.push("-e".to_string());
        args.push(line.to_string());
    }
    args.push(format!("Account {}: {}", clean_alias(alias), notice.text()));
    args
}

/// Make the arguments and hand them to `run`. The sender is a parameter so a test never starts
/// a process.
#[cfg(any(target_os = "macos", test))]
fn notify_with(notice: Notice, alias: &str, run: &mut dyn FnMut(&[String])) {
    run(&osascript_args(notice, alias));
}

#[cfg(target_os = "macos")]
fn spawn_osascript(args: &[String]) {
    use std::process::{Command, Stdio};
    let child = Command::new("/usr/bin/osascript")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        // The child is waited for off the UI thread, so it does not stay a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => log::warn!("The system notification could not be shown: {e}"),
    }
}

/// Show the notification. A failure is logged and never blocks anything.
#[cfg(target_os = "macos")]
pub fn send(notice: Notice, alias: &str) {
    notify_with(notice, alias, &mut |args| spawn_osascript(args));
}

#[cfg(not(target_os = "macos"))]
pub fn send(_notice: Notice, _alias: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_is_fixed_and_the_alias_is_only_the_last_argument() {
        let hostile = [
            "main",
            "\" & (do shell script \"touch /tmp/x\") & \"",
            "a\\\"b",
            "-e",
            "-e display dialog \"x\"",
            "line one\nline two\r\u{0007}",
            "end run\non run argv",
        ];
        for alias in hostile {
            let args = osascript_args(Notice::ProtectionFailed, alias);
            assert_eq!(args.len(), 7, "{alias:?}");
            // Three fixed `-e` lines, identical for every alias.
            let fixed: Vec<&str> = args[..6].iter().map(String::as_str).collect();
            assert_eq!(fixed, ["-e", SCRIPT[0], "-e", SCRIPT[1], "-e", SCRIPT[2]]);
            // The message is one argument that cannot be an option, with no control character.
            let msg = &args[6];
            assert!(msg.starts_with("Account "), "{msg:?}");
            assert!(!msg.chars().any(char::is_control), "{msg:?}");
            // Nothing of the alias is in the script lines.
            for line in &args[..6] {
                assert!(!line.contains("shell"), "{line:?}");
                assert!(!line.contains("touch"), "{line:?}");
            }
        }
    }

    #[test]
    fn format_characters_are_dropped_from_the_alias() {
        // Bidi override and isolates, zero-width space and joiner, word joiner, BOM, soft hyphen.
        let hostile = "ab\u{202E}cd\u{2066}e\u{2069}\u{200B}f\u{200D}\u{2060}\u{FEFF}\u{00AD}g";
        assert_eq!(clean_alias(hostile), "abcdefg");
        assert_eq!(clean_alias("\u{202E}\u{200B}"), "(no alias)");
        let args = osascript_args(Notice::ProtectionFailed, hostile);
        assert!(args[6].starts_with("Account abcdefg: "), "{:?}", args[6]);
        // Ordinary non-ASCII text stays.
        assert_eq!(clean_alias("帳戶 é"), "帳戶 é");
    }

    #[test]
    fn an_account_is_notified_once_per_round() {
        let (a, b) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
        assert_eq!(unique_accounts(vec![a, b, a, a, b]), vec![a, b]);
        assert!(unique_accounts(Vec::new()).is_empty());
    }

    #[test]
    fn the_message_is_the_fixed_text_with_the_alias() {
        let args = osascript_args(Notice::ProtectionFailed, "Main account");
        assert_eq!(
            args[6],
            "Account Main account: protection failed. Open the account panel in Flowsurface to \
             see what to do."
        );
        // Long aliases are cut, empty ones are named.
        let long = "x".repeat(100);
        let args = osascript_args(Notice::ProtectionFailed, &long);
        assert!(args[6].starts_with(&format!("Account {}: ", "x".repeat(32))));
        let args = osascript_args(Notice::ProtectionFailed, "  \n ");
        assert!(args[6].starts_with("Account (no alias): "));
    }

    #[test]
    fn every_notice_has_a_fixed_text_without_a_placeholder() {
        // One kind today; an exhaustive match makes a new kind add its row here.
        let all = |n: Notice| match n {
            Notice::ProtectionFailed => n,
        };
        let text = all(Notice::ProtectionFailed).text();
        assert!(!text.is_empty());
        assert!(text.is_ascii());
        assert!(!text.contains('{') && !text.contains('%'));
    }

    #[test]
    fn the_sender_is_called_once_with_exactly_the_arguments() {
        let mut seen: Vec<Vec<String>> = Vec::new();
        notify_with(Notice::ProtectionFailed, "main", &mut |args| {
            seen.push(args.to_vec());
        });
        assert_eq!(seen, vec![osascript_args(Notice::ProtectionFailed, "main")]);
    }
}
