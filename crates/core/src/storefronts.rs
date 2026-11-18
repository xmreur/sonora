//! Apple Music storefronts offered by the catalog API.
//!
//! The codes are the iTunes Store territory identifiers used as
//! `/v1/catalog/{storefront}/...` path segments. Kept as static data so the
//! country pickers work offline and need no token or network round-trip.

/// `(storefront code, display name)`, sorted by display name.
pub const STOREFRONTS: &[(&str, &str)] = &[
    ("al", "Albania"),
    ("dz", "Algeria"),
    ("ao", "Angola"),
    ("ai", "Anguilla"),
    ("ag", "Antigua and Barbuda"),
    ("ar", "Argentina"),
    ("am", "Armenia"),
    ("au", "Australia"),
    ("at", "Austria"),
    ("az", "Azerbaijan"),
    ("bs", "Bahamas"),
    ("bh", "Bahrain"),
    ("bb", "Barbados"),
    ("by", "Belarus"),
    ("be", "Belgium"),
    ("bz", "Belize"),
    ("bj", "Benin"),
    ("bm", "Bermuda"),
    ("bt", "Bhutan"),
    ("bo", "Bolivia"),
    ("ba", "Bosnia and Herzegovina"),
    ("bw", "Botswana"),
    ("br", "Brazil"),
    ("vg", "British Virgin Islands"),
    ("bn", "Brunei Darussalam"),
    ("bg", "Bulgaria"),
    ("bf", "Burkina Faso"),
    ("kh", "Cambodia"),
    ("cm", "Cameroon"),
    ("ca", "Canada"),
    ("cv", "Cape Verde"),
    ("ky", "Cayman Islands"),
    ("td", "Chad"),
    ("cl", "Chile"),
    ("cn", "China mainland"),
    ("co", "Colombia"),
    ("cr", "Costa Rica"),
    ("hr", "Croatia"),
    ("cy", "Cyprus"),
    ("cz", "Czech Republic"),
    ("ci", "Côte d'Ivoire"),
    ("cd", "Democratic Republic of the Congo"),
    ("dk", "Denmark"),
    ("dm", "Dominica"),
    ("do", "Dominican Republic"),
    ("ec", "Ecuador"),
    ("eg", "Egypt"),
    ("sv", "El Salvador"),
    ("ee", "Estonia"),
    ("sz", "Eswatini"),
    ("fj", "Fiji"),
    ("fi", "Finland"),
    ("fr", "France"),
    ("ga", "Gabon"),
    ("gm", "Gambia"),
    ("ge", "Georgia"),
    ("de", "Germany"),
    ("gh", "Ghana"),
    ("gr", "Greece"),
    ("gd", "Grenada"),
    ("gt", "Guatemala"),
    ("gw", "Guinea-Bissau"),
    ("gy", "Guyana"),
    ("hn", "Honduras"),
    ("hk", "Hong Kong"),
    ("hu", "Hungary"),
    ("is", "Iceland"),
    ("in", "India"),
    ("id", "Indonesia"),
    ("iq", "Iraq"),
    ("ie", "Ireland"),
    ("il", "Israel"),
    ("it", "Italy"),
    ("jm", "Jamaica"),
    ("jp", "Japan"),
    ("jo", "Jordan"),
    ("kz", "Kazakhstan"),
    ("ke", "Kenya"),
    ("kr", "Korea, Republic of"),
    ("xk", "Kosovo"),
    ("kw", "Kuwait"),
    ("kg", "Kyrgyzstan"),
    ("la", "Lao People's Democratic Republic"),
    ("lv", "Latvia"),
    ("lb", "Lebanon"),
    ("lr", "Liberia"),
    ("ly", "Libya"),
    ("lt", "Lithuania"),
    ("lu", "Luxembourg"),
    ("mo", "Macao"),
    ("mg", "Madagascar"),
    ("mw", "Malawi"),
    ("my", "Malaysia"),
    ("mv", "Maldives"),
    ("ml", "Mali"),
    ("mt", "Malta"),
    ("mr", "Mauritania"),
    ("mu", "Mauritius"),
    ("mx", "Mexico"),
    ("fm", "Micronesia, Federated States of"),
    ("md", "Moldova"),
    ("mn", "Mongolia"),
    ("me", "Montenegro"),
    ("ms", "Montserrat"),
    ("ma", "Morocco"),
    ("mz", "Mozambique"),
    ("mm", "Myanmar"),
    ("na", "Namibia"),
    ("np", "Nepal"),
    ("nl", "Netherlands"),
    ("nz", "New Zealand"),
    ("ni", "Nicaragua"),
    ("ne", "Niger"),
    ("ng", "Nigeria"),
    ("mk", "North Macedonia"),
    ("no", "Norway"),
    ("om", "Oman"),
    ("pk", "Pakistan"),
    ("pa", "Panama"),
    ("pg", "Papua New Guinea"),
    ("py", "Paraguay"),
    ("pe", "Peru"),
    ("ph", "Philippines"),
    ("pl", "Poland"),
    ("pt", "Portugal"),
    ("qa", "Qatar"),
    ("cg", "Republic of the Congo"),
    ("ro", "Romania"),
    ("ru", "Russia"),
    ("rw", "Rwanda"),
    ("sa", "Saudi Arabia"),
    ("sn", "Senegal"),
    ("rs", "Serbia"),
    ("sc", "Seychelles"),
    ("sl", "Sierra Leone"),
    ("sg", "Singapore"),
    ("sk", "Slovakia"),
    ("si", "Slovenia"),
    ("sb", "Solomon Islands"),
    ("za", "South Africa"),
    ("es", "Spain"),
    ("lk", "Sri Lanka"),
    ("kn", "St. Kitts and Nevis"),
    ("lc", "St. Lucia"),
    ("vc", "St. Vincent and The Grenadines"),
    ("sr", "Suriname"),
    ("se", "Sweden"),
    ("ch", "Switzerland"),
    ("st", "São Tomé and Príncipe"),
    ("tw", "Taiwan"),
    ("tj", "Tajikistan"),
    ("tz", "Tanzania"),
    ("th", "Thailand"),
    ("to", "Tonga"),
    ("tt", "Trinidad and Tobago"),
    ("tn", "Tunisia"),
    ("tm", "Turkmenistan"),
    ("tc", "Turks and Caicos"),
    ("tr", "Türkiye"),
    ("ae", "UAE"),
    ("ug", "Uganda"),
    ("ua", "Ukraine"),
    ("gb", "United Kingdom"),
    ("us", "United States"),
    ("uy", "Uruguay"),
    ("uz", "Uzbekistan"),
    ("vu", "Vanuatu"),
    ("ve", "Venezuela"),
    ("vn", "Vietnam"),
    ("ye", "Yemen"),
    ("zm", "Zambia"),
    ("zw", "Zimbabwe"),
];

/// Canonical code for a user-supplied value, or `None` if it is not a known
/// storefront. Trims whitespace and accepts upper-case input (e.g. `" US "`).
pub fn canonical(code: &str) -> Option<&'static str> {
    let code = code.trim().to_ascii_lowercase();
    STOREFRONTS
        .iter()
        .find(|(id, _)| *id == code)
        .map(|(id, _)| *id)
}

/// Human-readable name for a code; falls back to the code itself so an
/// unrecognised value copied into settings is still visible in the UI.
pub fn label(code: &str) -> &str {
    let code = code.trim();
    STOREFRONTS
        .iter()
        .find(|(id, _)| *id == code)
        .map(|(_, name)| *name)
        .unwrap_or(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_populated_and_has_unique_codes() {
        assert!(STOREFRONTS.len() > 100);
        let mut codes: Vec<&str> = STOREFRONTS.iter().map(|(id, _)| *id).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "duplicate storefront code");
        assert!(STOREFRONTS
            .iter()
            .all(|(id, name)| !id.is_empty() && !name.is_empty()));
    }

    #[test]
    fn canonical_normalises_and_rejects() {
        assert_eq!(canonical(" us "), Some("us"));
        assert_eq!(canonical("JP"), Some("jp"));
        assert_eq!(canonical(""), None);
        assert_eq!(canonical("zz"), None);
    }

    #[test]
    fn label_uses_name_or_falls_back_to_code() {
        assert_eq!(label("us"), "United States");
        assert_eq!(label("xx"), "xx");
    }
}
