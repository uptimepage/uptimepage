use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Type;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::reserved_slugs::is_reserved;

/// Strongly-typed account id. An account is the quota subject: it holds the
/// plan and pools every cap across the orgs it owns, so an extra org buys a
/// workspace, never extra capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type, ToSchema)]
#[serde(transparent)]
#[sqlx(transparent)]
#[schema(value_type = String, format = "uuid")]
pub struct AccountId(pub Uuid);

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Strongly-typed organisation id. Wrapping `Uuid` blocks the easy mistake of
/// passing a `UserId` where an `OrgId` is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type, ToSchema)]
#[serde(transparent)]
#[sqlx(transparent)]
#[schema(value_type = String, format = "uuid")]
pub struct OrgId(pub Uuid);

impl std::fmt::Display for OrgId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Organization {
    pub id: OrgId,
    pub slug: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// A status page's display branding (the page's `enabled` flag lives on
/// `StatusPage`, not here). Optional fields fall back to `PublicStatusConfig`
/// defaults when `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct PublicOrgBranding {
    pub public_display_name: Option<String>,
    pub public_about: Option<String>,
    pub public_brand_color: Option<String>,
    /// Content hash of the page's `logo` asset (cache-buster for the logo URL),
    /// or `None` when no logo is set. Sourced from `page_assets`, read-only here
    /// — logo writes go through `PageAssetStore`, never a branding update.
    pub logo_hash: Option<String>,
    pub public_show_powered_by: Option<bool>,
    #[serde(default)]
    pub public_style: PublicStyle,
    #[serde(default)]
    pub public_hide_from_search: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PublicStyle {
    #[default]
    Default,
    Classic,
    Terminal,
    Winter,
    Dark,
    Night,
    Dim,
    Nord,
    Dracula,
    Corporate,
    Light,
    Cupcake,
    Cyberpunk,
    Synthwave,
}

impl PublicStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Classic => "classic",
            Self::Terminal => "terminal",
            Self::Winter => "winter",
            Self::Dark => "dark",
            Self::Night => "night",
            Self::Dim => "dim",
            Self::Nord => "nord",
            Self::Dracula => "dracula",
            Self::Corporate => "corporate",
            Self::Light => "light",
            Self::Cupcake => "cupcake",
            Self::Cyberpunk => "cyberpunk",
            Self::Synthwave => "synthwave",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "default" => Self::Default,
            "classic" => Self::Classic,
            "terminal" => Self::Terminal,
            "winter" => Self::Winter,
            "dark" => Self::Dark,
            "night" => Self::Night,
            "dim" => Self::Dim,
            "nord" => Self::Nord,
            "dracula" => Self::Dracula,
            "corporate" => Self::Corporate,
            "light" => Self::Light,
            "cupcake" => Self::Cupcake,
            "cyberpunk" => Self::Cyberpunk,
            "synthwave" => Self::Synthwave,
            other => {
                tracing::warn!(
                    value = other,
                    "unknown public_style in DB, falling back to default"
                );
                Self::Default
            }
        }
    }

    pub const ALL: &'static [&'static str] = &[
        "default",
        "classic",
        "terminal",
        "winter",
        "dark",
        "night",
        "dim",
        "nord",
        "dracula",
        "corporate",
        "light",
        "cupcake",
        "cyberpunk",
        "synthwave",
    ];
}

/// Domain-layer mirror of the column CHECK constraints. Lets handlers map to
/// a 400 that names the offending field, instead of a generic constraint
/// violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrandingError {
    BrandColorFormat,
    AboutTooLong,
    DisplayNameLength,
}

impl std::fmt::Display for BrandingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::BrandColorFormat => "brand color must be a 6-digit hex like #3b82f6",
            Self::AboutTooLong => "public about may be at most 500 characters",
            Self::DisplayNameLength => "display name must be 1-80 characters",
        };
        f.write_str(s)
    }
}

impl BrandingError {
    /// The request/form field this error is about. Matches the JSON request
    /// key and the settings-form input `name`, so the API can point the
    /// browser straight at the offending control.
    pub fn field(&self) -> &'static str {
        match self {
            Self::BrandColorFormat => "public_brand_color",
            Self::AboutTooLong => "public_about",
            Self::DisplayNameLength => "public_display_name",
        }
    }
}

impl std::error::Error for BrandingError {}

impl PublicOrgBranding {
    /// Resolve the footer toggle against the configured default. One place so
    /// the operator preview and the live page can't disagree.
    pub fn show_powered_by(&self, default: bool) -> bool {
        self.public_show_powered_by.unwrap_or(default)
    }

    pub fn validate(&self) -> Result<(), BrandingError> {
        if let Some(c) = &self.public_brand_color
            && !is_hex_color(c)
        {
            return Err(BrandingError::BrandColorFormat);
        }
        if let Some(a) = &self.public_about
            && a.chars().count() > 500
        {
            return Err(BrandingError::AboutTooLong);
        }
        if let Some(n) = &self.public_display_name {
            let len = n.chars().count();
            if !(1..=80).contains(&len) {
                return Err(BrandingError::DisplayNameLength);
            }
        }
        Ok(())
    }
}

fn is_hex_color(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlugError {
    TooShort,
    TooLong,
    InvalidChar,
    LeadingDigitOrHyphen,
    TrailingHyphen,
    ConsecutiveHyphens,
    Reserved,
}

impl std::fmt::Display for SlugError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::TooShort => "slug must be at least 3 characters",
            Self::TooLong => "slug must be at most 30 characters",
            Self::InvalidChar => "slug may only contain lowercase letters, digits, and hyphens",
            Self::LeadingDigitOrHyphen => "slug must start with a letter",
            Self::TrailingHyphen => "slug must not end with a hyphen",
            Self::ConsecutiveHyphens => "slug must not contain consecutive hyphens",
            Self::Reserved => "slug is reserved",
        };
        f.write_str(s)
    }
}

impl std::error::Error for SlugError {}

/// Validate a slug: 3-30 chars, [a-z0-9-], leading letter, no trailing
/// hyphen, no consecutive hyphens, not in reserved list (exact match).
pub fn validate_slug(slug: &str) -> Result<(), SlugError> {
    let len = slug.len();
    if len < 3 {
        return Err(SlugError::TooShort);
    }
    if len > 30 {
        return Err(SlugError::TooLong);
    }
    let bytes = slug.as_bytes();
    if !bytes[0].is_ascii_lowercase() {
        return Err(SlugError::LeadingDigitOrHyphen);
    }
    let last = bytes[len - 1];
    if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return Err(SlugError::TrailingHyphen);
    }
    for &b in bytes {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if !ok {
            return Err(SlugError::InvalidChar);
        }
    }
    if slug.contains("--") {
        return Err(SlugError::ConsecutiveHyphens);
    }
    // Exact-match reserved check — auto-generated `personal-*` slugs pass
    // because only the bare word `personal` is in the list.
    if is_reserved(slug) {
        return Err(SlugError::Reserved);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_basic_slug() {
        assert!(validate_slug("acme-corp").is_ok());
        assert!(validate_slug("widgets").is_ok());
        assert!(validate_slug("a1b").is_ok());
    }

    #[test]
    fn rejects_too_short() {
        assert_eq!(validate_slug("ab"), Err(SlugError::TooShort));
    }

    #[test]
    fn rejects_too_long() {
        let s = "a".repeat(31);
        assert_eq!(validate_slug(&s), Err(SlugError::TooLong));
    }

    #[test]
    fn rejects_leading_digit() {
        assert_eq!(validate_slug("1abc"), Err(SlugError::LeadingDigitOrHyphen));
    }

    #[test]
    fn rejects_leading_hyphen() {
        assert_eq!(validate_slug("-abc"), Err(SlugError::LeadingDigitOrHyphen));
    }

    #[test]
    fn rejects_trailing_hyphen() {
        assert_eq!(validate_slug("abc-"), Err(SlugError::TrailingHyphen));
    }

    #[test]
    fn rejects_uppercase() {
        assert_eq!(validate_slug("Abc"), Err(SlugError::LeadingDigitOrHyphen));
        assert_eq!(validate_slug("aBc"), Err(SlugError::InvalidChar));
    }

    #[test]
    fn rejects_consecutive_hyphens() {
        assert_eq!(validate_slug("ab--cd"), Err(SlugError::ConsecutiveHyphens));
    }

    #[test]
    fn rejects_reserved_exact() {
        assert_eq!(validate_slug("admin"), Err(SlugError::Reserved));
        assert_eq!(validate_slug("personal"), Err(SlugError::Reserved));
    }

    #[test]
    fn accepts_personal_prefixed_slug() {
        // Auto-generated personal slugs must pass — reserved check is exact-match.
        assert!(validate_slug("personal-happy-fox-3a9k7m").is_ok());
    }

    #[test]
    fn branding_validate_rejects_bad_color() {
        let mut b = PublicOrgBranding {
            public_brand_color: Some("blue".into()),
            ..PublicOrgBranding::default()
        };
        assert_eq!(b.validate(), Err(BrandingError::BrandColorFormat));
        b.public_brand_color = Some("#zzzzzz".into());
        assert_eq!(b.validate(), Err(BrandingError::BrandColorFormat));
        b.public_brand_color = Some("#3b82f".into());
        assert_eq!(b.validate(), Err(BrandingError::BrandColorFormat));
    }

    #[test]
    fn branding_validate_rejects_long_about_and_name() {
        let mut b = PublicOrgBranding {
            public_about: Some("x".repeat(501)),
            ..PublicOrgBranding::default()
        };
        assert_eq!(b.validate(), Err(BrandingError::AboutTooLong));
        b.public_about = None;
        b.public_display_name = Some(String::new());
        assert_eq!(b.validate(), Err(BrandingError::DisplayNameLength));
        b.public_display_name = Some("y".repeat(81));
        assert_eq!(b.validate(), Err(BrandingError::DisplayNameLength));
    }

    #[test]
    fn public_style_from_db_known_values_round_trip() {
        for s in PublicStyle::ALL {
            assert_eq!(PublicStyle::from_db(s).as_str(), *s);
        }
        assert_eq!(PublicStyle::from_db("garbage").as_str(), "default");
    }
}
