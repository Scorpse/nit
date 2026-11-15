//! Plain display text from Bitbucket's rendered comment HTML (used only when `raw` is empty).

/// Tags whose end starts a new line.
const BLOCK: [&str; 14] = [
    "p",
    "div",
    "li",
    "ul",
    "ol",
    "tr",
    "pre",
    "blockquote",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
];

/// Strips tags, drops `<script>`/`<style>` content, turns `<br>` and block ends (`p`, `li`, …)
/// into newlines and decodes common entities. Display text only, never sent anywhere.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    let mut pre = 0usize;
    while let Some(lt) = rest.find('<') {
        push_text(&mut out, &rest[..lt], pre > 0);
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            // an unclosed `<` is text
            push_text(&mut out, &rest[lt..], pre > 0);
            rest = "";
            break;
        };
        let tag = &after[..gt];
        rest = &after[gt + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if !closing && (name == "script" || name == "style") {
            let end = format!("</{name}");
            rest = match rest.to_ascii_lowercase().find(&end) {
                Some(i) => rest[i..].find('>').map_or("", |j| &rest[i + j + 1..]),
                None => "",
            };
            continue;
        }
        if name == "pre" {
            pre = if closing {
                pre.saturating_sub(1)
            } else {
                pre + 1
            };
        }
        if name == "br" {
            newline(&mut out, true);
        } else if BLOCK.contains(&name.as_str()) && (closing || name == "li") {
            newline(&mut out, false);
        }
    }
    push_text(&mut out, rest, pre > 0);
    tidy(&out)
}

/// `<br>` always breaks; a block edge only ends a line that has text.
fn newline(out: &mut String, always: bool) {
    while out.ends_with(' ') {
        out.pop();
    }
    if always || !(out.is_empty() || out.ends_with('\n')) {
        out.push('\n');
    }
}

/// Appends decoded text; outside `<pre>` source whitespace collapses to single spaces.
fn push_text(out: &mut String, text: &str, pre: bool) {
    let text = decode_entities(text);
    if pre {
        out.push_str(&text);
        return;
    }
    for c in text.chars() {
        if c.is_whitespace() && c != '\u{a0}' {
            if !(out.is_empty() || out.ends_with(' ') || out.ends_with('\n')) {
                out.push(' ');
            }
        } else if c == '\u{a0}' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
}

/// Trims each line's end, drops blank lines at both ends and keeps at most one blank line.
fn tidy(s: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in s.lines().map(str::trim_end) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&i| i <= 10).and_then(|i| {
            let c = match &rest[1..i] {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                e => e
                    .strip_prefix("#x")
                    .or_else(|| e.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            c.map(|c| (c, i))
        });
        match decoded {
            Some((c, i)) => {
                out.push(c);
                rest = &rest[i + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::html_to_text;

    #[test]
    fn entities_tags_and_blocks() {
        assert_eq!(
            html_to_text("a &amp;&lt;&gt;&quot;&#39;&#x41;&nbsp;b"),
            "a &<>\"'A b"
        );
        assert_eq!(html_to_text("<p>one</p><p>two</p>"), "one\ntwo");
        assert_eq!(html_to_text("<ul><li>a</li><li>b</li></ul>"), "a\nb");
        assert_eq!(
            html_to_text("x<SCRIPT>bad()</SCRIPT>y<style>s</style>z"),
            "xyz"
        );
        assert_eq!(html_to_text("<pre>a\n  b</pre>c"), "a\n  b\nc");
        assert_eq!(html_to_text("1 < 2 & 3"), "1 < 2 & 3");
        assert_eq!(html_to_text(""), "");
    }
}
