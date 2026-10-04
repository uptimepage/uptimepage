//! Shared HTML shell for transactional mail: one card, one palette, one set of
//! blocks. Every template composes these instead of carrying its own inline CSS.
//!
//! Mail clients get table layout, inline styles and hex colours only — Outlook's
//! Word engine has no flex/grid and no client parses the app's `oklch()` tokens,
//! so the palette below is those tokens converted once.

use crate::email::templates::{attr_escape, html_escape};

const INK: &str = "#0c1318";
const INK_MUTED: &str = "#33393e";
const INK_QUIET: &str = "#50565b";
/// Quietest tone that still clears 4.5:1 on the card. The app's faint grey is
/// 2:1 here, which a screen in daylight loses outright.
const INK_SOFT: &str = "#6b7479";
const LINE: &str = "#ccd2d6";
const LINE_SOFT: &str = "#e8edf0";
const SURFACE: &str = "#f8fafc";
const CARD: &str = "#ffffff";
/// Two steps darker than the app's action green: on a screen the button label
/// and the eyebrow are the only things carrying it, with no hover to confirm.
const BRAND: &str = "#007a45";
const BAND_BG: &str = "#13161c";
const BAND_INK: &str = "#e8ecee";
const BAND_MUTED: &str = "#a1a5ab";
const WARN: &str = "#e6ac3d";
const WARN_SOFT: &str = "#fdf2dd";

const SANS: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
const MONO: &str = "ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace";

/// Signal colour of a status band, read against the dark band background.
#[derive(Debug, Clone, Copy)]
pub enum Tone {
    Bad,
    Warn,
    Good,
    Info,
}

impl Tone {
    fn signal(self) -> &'static str {
        match self {
            Tone::Bad => "#ff6f69",
            Tone::Warn => "#f3b94c",
            Tone::Good => "#43d58f",
            Tone::Info => "#a1c1e4",
        }
    }
}

pub enum ButtonStyle {
    /// Brand fill. The one thing the recipient is meant to do.
    Solid,
    /// Outline. For mail that reports rather than asks.
    Outline,
}

pub struct Page<'a> {
    pub title: &'a str,
    /// Inbox preview line. Clients show it next to the subject, so it carries
    /// the detail the subject had to drop rather than repeating it.
    pub preheader: &'a str,
    /// Signs the card off with the sender. `None` for mail sent on a customer's
    /// behalf, where naming us under their status page would attribute our
    /// product to them.
    pub signature: Option<&'a str>,
    /// Output of [`band`] or [`wordmark`].
    pub header: String,
    pub body: String,
    /// Attribution and opt-out, above the signature.
    pub footnote: Option<String>,
}

/// Dark status band: signal dot, uppercase state, then the subject of the alert.
pub fn band(tone: Tone, kicker: &str, headline: &str, sub: Option<&str>) -> String {
    let sub_html = sub
        .map(|s| {
            format!(
                "\n<div style=\"margin-top:8px;font-family:{MONO};font-size:13px;\
                 line-height:1.5;color:{BAND_MUTED};word-break:break-word;\">{}</div>",
                html_escape(s)
            )
        })
        .unwrap_or_default();

    // The band's own top corners, or its fill squares off the card's.
    format!(
        "<tr><td style=\"background-color:{BAND_BG};padding:20px 24px;\
         border-radius:9px 9px 0 0;\">\n\
         <div style=\"font-family:{MONO};font-size:11px;font-weight:600;line-height:1.4;\
         letter-spacing:0.12em;text-transform:uppercase;color:{signal};\">\
         &#9679;&nbsp; {kicker}</div>\n\
         <div style=\"margin-top:10px;font-family:{SANS};font-size:19px;font-weight:600;\
         line-height:1.35;color:{BAND_INK};word-break:break-word;text-wrap:balance;\">\
         {headline}</div>\
         {sub_html}\n\
         </td></tr>\n",
        signal = tone.signal(),
        kicker = html_escape(kicker),
        headline = html_escape(headline),
    )
}

/// Light header for mail that is a message, not a status change.
pub fn wordmark(site_name: &str, heading: &str) -> String {
    format!(
        "<tr><td style=\"padding:22px 24px 0;\">\n\
         <div style=\"font-family:{MONO};font-size:11px;font-weight:600;line-height:1.4;\
         letter-spacing:0.12em;color:{BRAND};\">{site}</div>\n\
         <div style=\"margin-top:10px;font-family:{SANS};font-size:21px;font-weight:600;\
         line-height:1.3;color:{INK};text-wrap:balance;\">{heading}</div>\n\
         </td></tr>\n",
        site = html_escape(site_name),
        heading = html_escape(heading),
    )
}

/// Body paragraph. `html` is caller-escaped so it can carry links and `<strong>`.
pub fn paragraph(html: &str) -> String {
    format!(
        "<p style=\"margin:0 0 14px;font-family:{SANS};font-size:15px;line-height:1.6;\
         color:{INK_MUTED};text-wrap:pretty;\">{html}</p>\n"
    )
}

/// Customer-written prose: escaped here, with its own line breaks kept.
pub fn prose(text: &str) -> String {
    paragraph(&html_escape(text).replace('\n', "<br>"))
}

/// Label/value rows. Values are escaped here, so no row can carry markup.
/// Labels are upper-cased in Rust, not by CSS: Outlook's Word engine ignores
/// `text-transform`, and they would then disagree with the band above them.
pub fn facts(rows: &[(&str, String)]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let body = rows
        .iter()
        .map(|(label, value)| {
            format!(
                "<tr>\
                 <td style=\"padding:9px 12px 9px 0;border-bottom:1px solid {LINE_SOFT};\
                 font-family:{MONO};font-size:11px;line-height:1.5;letter-spacing:0.06em;\
                 color:{INK_QUIET};vertical-align:top;width:104px;\">\
                 {label}</td>\
                 <td style=\"padding:9px 0;border-bottom:1px solid {LINE_SOFT};\
                 font-family:{SANS};font-size:14px;line-height:1.5;color:{INK};\
                 vertical-align:top;word-break:break-word;\
                 font-variant-numeric:tabular-nums;\">{value}</td>\
                 </tr>",
                label = html_escape(&label.to_uppercase()),
                value = html_escape(value),
            )
        })
        .collect::<String>();

    format!(
        "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" \
         border=\"0\" style=\"width:100%;margin:2px 0 20px;border-collapse:collapse;\">\n\
         {body}\n</table>\n"
    )
}

/// Verbatim machine output — an error sample, a customer's own message.
pub fn code_block(text: &str) -> String {
    format!(
        "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" \
         border=\"0\" style=\"width:100%;margin:0 0 18px;\">\n\
         <tr><td style=\"background-color:{SURFACE};border:1px solid {LINE_SOFT};\
         border-radius:6px;padding:12px 14px;font-family:{MONO};font-size:12.5px;\
         line-height:1.55;color:{INK_MUTED};white-space:pre-wrap;word-break:break-word;\">\
         {text}</td></tr>\n</table>\n",
        text = html_escape(text),
    )
}

/// Aside the recipient needs to read the alert stream itself, such as a held
/// alert from a flapping monitor.
pub fn callout(text: &str) -> String {
    format!(
        "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" \
         border=\"0\" style=\"width:100%;margin:0 0 18px;\">\n\
         <tr><td style=\"background-color:{WARN_SOFT};border-left:3px solid {WARN};\
         border-radius:0 4px 4px 0;padding:11px 14px;font-family:{SANS};font-size:13px;\
         line-height:1.55;color:{INK_MUTED};\">{text}</td></tr>\n</table>\n",
        text = html_escape(text),
    )
}

/// Table-wrapped so Outlook honours the padding. Padding and line height are
/// sized to a 44px tap target, since most alerts are read on a phone.
pub fn button(url: &str, label: &str, style: ButtonStyle) -> String {
    let (cell, text) = match style {
        ButtonStyle::Solid => (
            format!("background-color:{BRAND};border:1px solid {BRAND};"),
            CARD.to_string(),
        ),
        ButtonStyle::Outline => (format!("border:1px solid {INK};"), INK.to_string()),
    };
    format!(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" \
         style=\"margin:0 0 20px;\">\n\
         <tr><td style=\"{cell}border-radius:8px;\">\
         <a href=\"{url}\" style=\"display:inline-block;padding:12px 22px;font-family:{SANS};\
         font-size:14px;font-weight:600;line-height:20px;color:{text};text-decoration:none;\">\
         {label}</a></td></tr>\n</table>\n",
        url = attr_escape(url),
        label = html_escape(label),
    )
}

/// Secondary link under a button, for the action nobody clicks first.
pub fn fine_print(html: &str) -> String {
    format!(
        "<p style=\"margin:0 0 14px;font-family:{SANS};font-size:13px;line-height:1.6;\
         color:{INK_QUIET};text-wrap:pretty;\">{html}</p>\n"
    )
}

/// Inline link, on brand in body copy.
pub fn link(url: &str, label: &str) -> String {
    format!(
        "<a href=\"{url}\" style=\"color:{BRAND};text-decoration:underline;\">{label}</a>",
        url = attr_escape(url),
        label = html_escape(label),
    )
}

/// Muted link, for opt-outs that should not compete with the alert.
pub fn quiet_link(url: &str, label: &str) -> String {
    format!(
        "<a href=\"{url}\" style=\"color:{INK_QUIET};text-decoration:underline;\">{label}</a>",
        url = attr_escape(url),
        label = html_escape(label),
    )
}

pub fn render(page: Page<'_>) -> String {
    render_in("en", page)
}

pub fn render_in(lang: &str, page: Page<'_>) -> String {
    // The hairline separates the footnote from the signature, so it only earns
    // its place when both are there.
    let rule = match page.signature {
        Some(_) => format!(
            "<div style=\"height:1px;background-color:{LINE_SOFT};margin:0 0 10px;\"></div>\n"
        ),
        None => String::new(),
    };
    let footnote = page
        .footnote
        .map(|html| format!("<div style=\"margin:0 0 10px;\">{html}</div>\n{rule}"))
        .unwrap_or_default();

    let signature = page
        .signature
        .map(|site| {
            format!(
                "<div style=\"color:{INK_SOFT};\">{site} &middot; uptime monitoring</div>\n",
                site = html_escape(site),
            )
        })
        .unwrap_or_default();

    format!(
        "<!doctype html>\n\
         <html lang=\"{lang}\"><head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"only light\">\n\
         <meta name=\"supported-color-schemes\" content=\"only light\">\n\
         <title>{title}</title>\n\
         </head>\n\
         <body style=\"margin:0;padding:0;background-color:{SURFACE};\
         -webkit-font-smoothing:antialiased;\">\n\
         <div style=\"display:none;max-height:0;overflow:hidden;font-size:1px;\
         line-height:1px;color:{SURFACE};\">{preheader}</div>\n\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" \
         border=\"0\" style=\"width:100%;background-color:{SURFACE};\">\n\
         <tr><td align=\"center\" style=\"padding:28px 12px;\">\n\
         <table role=\"presentation\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" \
         border=\"0\" style=\"width:100%;max-width:600px;background-color:{CARD};\
         border:1px solid {LINE};border-radius:10px;\">\n\
         {header}\
         <tr><td style=\"padding:22px 24px 4px;\">\n{body}</td></tr>\n\
         <tr><td style=\"padding:16px 24px 20px;border-top:1px solid {LINE_SOFT};\
         font-family:{SANS};font-size:12px;line-height:1.55;color:{INK_QUIET};\">\n\
         {footnote}\
         {signature}\
         </td></tr>\n\
         </table>\n\
         </td></tr>\n</table>\n</body></html>\n",
        lang = attr_escape(lang),
        title = html_escape(page.title),
        preheader = html_escape(page.preheader),
        header = page.header,
        body = page.body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of a `#rrggbb` literal.
    fn luminance(hex: &str) -> f64 {
        let channel = |i: usize| {
            let v = u8::from_str_radix(&hex[i..i + 2], 16).expect("hex pair") as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// Mail has no theme toggle and no hover to fall back on, so every pair the
    /// shell paints has to clear AA on its own.
    #[test]
    fn every_text_colour_clears_aa_on_the_surface_behind_it() {
        let pairs = [
            (CARD, BRAND, "button label on brand fill"),
            (BRAND, CARD, "eyebrow and links on the card"),
            (INK, CARD, "fact values on the card"),
            (INK_MUTED, CARD, "body copy on the card"),
            (INK_QUIET, CARD, "footer copy on the card"),
            (INK_SOFT, CARD, "signature on the card"),
            (INK_MUTED, SURFACE, "code block on its own fill"),
            (INK_MUTED, WARN_SOFT, "callout on its own fill"),
            (BAND_INK, BAND_BG, "headline on the band"),
            (BAND_MUTED, BAND_BG, "strapline on the band"),
        ];
        for (fg, bg, what) in pairs {
            let ratio = contrast(fg, bg);
            assert!(ratio >= 4.5, "{what}: {fg} on {bg} is {ratio:.2}:1");
        }
        for tone in [Tone::Bad, Tone::Warn, Tone::Good, Tone::Info] {
            let ratio = contrast(tone.signal(), BAND_BG);
            assert!(ratio >= 4.5, "{tone:?} signal is {ratio:.2}:1 on the band");
        }
    }

    #[test]
    fn shell_keeps_client_hostile_constructs_out() {
        let html = render(Page {
            title: "t",
            preheader: "p",
            signature: Some("Uptimepage"),
            header: wordmark("Uptimepage", "Heading"),
            body: paragraph("copy") + &facts(&[("Started", "now".into())]),
            footnote: Some(fine_print("why")),
        });
        assert!(html.starts_with("<!doctype html>"));
        for banned in ["oklch(", "display:flex", "display:grid", "var(--"] {
            assert!(
                !html.contains(banned),
                "{banned} does not render in Outlook"
            );
        }
    }

    #[test]
    fn every_block_escapes_the_values_it_is_handed() {
        let hostile = "<img src=x onerror=alert(1)>";
        let blocks = [
            band(Tone::Bad, hostile, hostile, Some(hostile)),
            wordmark(hostile, hostile),
            facts(&[(hostile, hostile.into())]),
            code_block(hostile),
            callout(hostile),
            button("https://x.test", hostile, ButtonStyle::Solid),
            link("https://x.test", hostile),
            quiet_link("https://x.test", hostile),
        ];
        for block in blocks {
            assert!(!block.contains("<img src=x"), "unescaped: {block}");
            assert!(
                block.contains("&lt;img src=x"),
                "not escaped at all: {block}"
            );
        }
    }

    #[test]
    fn a_hostile_url_cannot_break_out_of_the_href() {
        let html = button("https://x.test/?a=\"><script>", "Go", ButtonStyle::Outline);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&quot;&gt;&lt;script&gt;"));
    }

    #[test]
    fn preheader_stays_hidden_from_the_rendered_card() {
        let html = render(Page {
            title: "t",
            preheader: "down in 3 regions",
            signature: Some("Uptimepage"),
            header: String::new(),
            body: String::new(),
            footnote: None,
        });
        let hidden = html
            .split_once("down in 3 regions")
            .expect("preheader present")
            .0;
        assert!(
            hidden.ends_with("\">"),
            "preheader must sit inside the hidden div"
        );
        assert!(hidden.contains("display:none;max-height:0"));
    }
}
