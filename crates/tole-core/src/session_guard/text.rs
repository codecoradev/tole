//! Text readings of the session guard (#1307): shell words, quotes, globs. Each
//! only ever adds refusals to what `session_guard.rs` reads.

/// Does the glob `p` match `s`? (`*`, `?`, `[..]`). A class is read as any one
/// character: more refusals, never fewer. Linear in `p`: no backtracking.
pub(super) fn glob(p: &[char], s: &[char]) -> bool {
    let (mut toks, mut i) = (Vec::new(), 0);
    while i < p.len() {
        let class = (p[i] == '[')
            .then(|| p[i..].iter().position(|&c| c == ']'))
            .flatten();
        toks.push(if let Some(e) = class {
            i += e;
            '?'
        } else {
            p[i]
        });
        i += 1;
    }
    let n = toks.len();
    let mut ok = vec![false; n + 1];
    ok[0] = true;
    for step in 0..=s.len() {
        for k in 0..n {
            ok[k + 1] |= toks[k] == '*' && ok[k];
        }
        let Some(&c) = s.get(step) else { break };
        let mut next = vec![false; n + 1];
        for k in (0..n).filter(|&k| ok[k]) {
            match toks[k] {
                '*' => next[k] = true,
                t => next[k + 1] |= t == '?' || t == c,
            }
        }
        ok = next;
    }
    ok[n]
}

/// `w` without its variable references (`$E`, `${E}`, `$@`): what the word is
/// when they expand to nothing.
pub(super) fn novars(w: &str) -> String {
    let (mut out, mut it) = (String::new(), w.chars().peekable());
    while let Some(c) = it.next() {
        match (c, it.peek().copied()) {
            ('$', Some('{')) => it.by_ref().take_while(|&c| c != '}').for_each(drop),
            ('$', Some(n)) if n.is_ascii_alphanumeric() || "_@*#?!".contains(n) => {
                while it
                    .next_if(|n| n.is_ascii_alphanumeric() || "_@*#?!".contains(*n))
                    .is_some()
                {}
            }
            _ => out.push(c),
        }
    }
    out
}

/// What the shell a payload is handed to reads: a backslash keeps the next
/// character (and drops a newline after it), and with `quotes` the quotes go.
/// A payload that is a real argument word keeps its quotes (`grep "a|tole"`).
pub(super) fn unquote_shell(t: &str, quotes: bool) -> String {
    let (mut out, mut q, mut it) = (String::new(), None, t.chars());
    while let Some(c) = it.next() {
        match (q, c) {
            (Some(k), c) if c == k => q = None,
            (None, '\'' | '"') if quotes => q = Some(c),
            (Some('\''), c) => out.push(c),
            (_, '\\') => out.extend(it.next().filter(|&n| n != '\n')),
            (_, c) => out.push(c),
        }
    }
    out
}

/// A second reading of shell text, in addition to the cuts above and only when
/// they were naive (#1307): where a cut splits a word the shell keeps whole, the
/// program behind it was never seen. A backslash keeps the next character in
/// its word (`a\ b`, `a\;b`), `{` and `}` are word characters, and the `&` of
/// `>&`, `<&`, `&>` belongs to a redirection. A substitution that opens a word
/// (`$(..)`, a backtick) becomes the word `$(`: an unknown program. Quotes are
/// not followed in the first pass, and kept whole (with their quotes) in the
/// second, so this only ever adds refusals.
pub(super) fn shell_words(s: &str, quotes: bool) -> Vec<Vec<String>> {
    let (mut cmds, mut cmd, mut word) = (Vec::new(), Vec::new(), String::new());
    let (mut esc, mut prev, mut q, mut ansi) = (false, ' ', None, false);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if let Some(k) = q {
            // Inside quotes everything is part of the word (`"` and `$'` take escapes).
            word.push(c);
            q = (c != k || esc).then_some(k);
            (esc, prev) = ((k == '"' || ansi) && !esc && c == '\\', c);
            continue;
        }
        let amp = c == '&' && !matches!(prev, '<' | '>') && it.peek() != Some(&'>');
        let pipe = c == '|' && prev != '>';
        let sep = !esc && (matches!(c, ';' | '(' | ')' | '`' | '\n') || pipe || amp);
        let at = word.trim_start_matches(['\'', '"', '\\']);
        if (c == '`' || c == '(') && !esc && at == if c == '`' { "" } else { "$" } {
            word = "$(".into();
        }
        if sep || (!esc && matches!(c, ' ' | '\t')) {
            if !word.is_empty() {
                cmd.push(std::mem::take(&mut word));
            }
            if sep {
                cmds.push(std::mem::take(&mut cmd));
            }
        } else if esc && c == '\n' {
            word.pop(); // a backslash-newline is no character
        } else {
            word.push(c);
            if quotes && !esc && matches!(c, '\'' | '"') {
                ansi = c == '\'' && word.ends_with("$'");
                q = Some(c);
            }
        }
        (esc, prev) = (!esc && c == '\\', c);
    }
    if !word.is_empty() {
        cmd.push(word);
    }
    cmds.push(cmd);
    cmds
}
