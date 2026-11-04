use chrono::{DateTime, Utc};

/// A zone in the picker, labelled with its offset from UTC right now.
pub struct ZoneChoice {
    pub name: String,
    pub label: String,
}

/// The picker's zones: `current` first when the list leaves it out, since the
/// API takes any IANA name, then [`timezone_names`], each with its offset at
/// `now`.
pub fn zone_choices(current: &str, now: DateTime<Utc>) -> Vec<ZoneChoice> {
    let names = timezone_names();
    let stored = (!names.contains(&current)).then_some(current);
    stored
        .into_iter()
        .chain(names.iter().copied())
        .map(|name| {
            let tz: chrono_tz::Tz = name.parse().unwrap_or(chrono_tz::UTC);
            let offset = now.with_timezone(&tz).format("%:z").to_string();
            ZoneChoice {
                label: if name == "UTC" {
                    name.to_owned()
                } else {
                    format!("{name} (UTC{offset})")
                },
                name: name.to_owned(),
            }
        })
        .collect()
}

/// Region-named zones and UTC, the ones a person picks from. Leaves out
/// country and legacy names such as `US/Eastern` and `GB`, and the
/// sign-inverted `Etc/GMT+N`; the API still accepts any IANA name.
fn timezone_names() -> &'static [&'static str] {
    const REGIONS: [&str; 10] = [
        "Africa",
        "America",
        "Antarctica",
        "Arctic",
        "Asia",
        "Atlantic",
        "Australia",
        "Europe",
        "Indian",
        "Pacific",
    ];
    static NAMES: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
        std::iter::once("UTC")
            .chain(
                chrono_tz::TZ_VARIANTS
                    .iter()
                    .map(|tz| tz.name())
                    .filter(|name| {
                        name.split_once('/')
                            .is_some_and(|(r, _)| REGIONS.contains(&r))
                    }),
            )
            .collect()
    });
    &NAMES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_zone_the_list_leaves_out_is_still_offered() {
        let now = "2026-01-15T12:00:00Z".parse().unwrap();
        let zones = zone_choices("US/Eastern", now);
        assert_eq!(zones[0].name, "US/Eastern");
        assert_eq!(zones[0].label, "US/Eastern (UTC-05:00)");
        assert_eq!(zones[1].label, "UTC");
        assert_eq!(zone_choices("UTC", now)[0].name, "UTC");
    }
}
