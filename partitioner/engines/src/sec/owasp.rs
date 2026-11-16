//! CWE → OWASP Top 10 2021 mapping (S35).
//!
//! A small built-in table covering the CWE Top 25 (2023) plus common web CWEs.
//! It is intentionally partial: a CWE with no entry yields `None`, which a
//! report renders as "unmapped". The category ids follow the OWASP 2021 list.

/// The ten OWASP Top 10 2021 category ids and human titles.
pub const CATEGORIES: &[(&str, &str)] = &[
    ("A01:2021", "Broken Access Control"),
    ("A02:2021", "Cryptographic Failures"),
    ("A03:2021", "Injection"),
    ("A04:2021", "Insecure Design"),
    ("A05:2021", "Security Misconfiguration"),
    ("A06:2021", "Vulnerable and Outdated Components"),
    ("A07:2021", "Identification and Authentication Failures"),
    ("A08:2021", "Software and Data Integrity Failures"),
    ("A09:2021", "Security Logging and Monitoring Failures"),
    ("A10:2021", "Server-Side Request Forgery (SSRF)"),
];

/// Returns the human title for a category id (e.g. `A03:2021`), if known.
pub fn category_title(id: &str) -> Option<&'static str> {
    CATEGORIES.iter().find(|(cid, _)| *cid == id).map(|(_, t)| *t)
}

/// Maps a single CWE number to its OWASP 2021 category id.
pub fn owasp_for_cwe(cwe_num: u32) -> Option<&'static str> {
    let cat = match cwe_num {
        // A01 Broken Access Control
        22 | 23 | 35 | 59 | 200 | 201 | 219 | 264 | 275 | 276 | 284 | 285 | 352 | 359 | 377 | 402
        | 425 | 441 | 497 | 538 | 540 | 548 | 552 | 566 | 601 | 639 | 651 | 668 | 706 | 862 | 863
        | 913 | 922 | 1275 => "A01:2021",
        // A02 Cryptographic Failures
        261 | 296 | 310 | 319 | 321 | 322 | 323 | 324 | 325 | 326 | 327 | 328 | 329 | 330 | 331 | 335
        | 336 | 337 | 338 | 340 | 347 | 523 | 720 | 757 | 759 | 760 | 780 | 818 => "A02:2021",
        // A03 Injection
        20 | 74 | 75 | 77 | 78 | 79 | 80 | 83 | 87 | 88 | 89 | 90 | 91 | 93 | 94 | 95 | 96 | 97 | 98
        | 99 | 100 | 113 | 116 | 138 | 184 | 470 | 471 | 564 | 610 | 643 | 644 | 652 | 917 => "A03:2021",
        // A04 Insecure Design
        73 | 183 | 209 | 213 | 235 | 256 | 257 | 266 | 269 | 280 | 311 | 312 | 313 | 316 | 419 | 430
        | 434 | 444 | 451 | 472 | 501 | 522 | 525 | 539 | 579 | 598 | 602 | 642 | 646 | 650 | 653 | 656
        | 657 | 799 | 807 | 840 | 841 | 927 | 1021 | 1173 => "A04:2021",
        // A05 Security Misconfiguration
        2 | 11 | 13 | 15 | 16 | 260 | 315 | 520 | 526 | 537 | 541 | 547 | 611 | 614 | 756 | 776 | 942
        | 1004 | 1032 | 1174 => "A05:2021",
        // A06 Vulnerable and Outdated Components
        937 | 1035 | 1104 => "A06:2021",
        // A07 Identification and Authentication Failures
        255 | 259 | 287 | 288 | 290 | 294 | 295 | 297 | 300 | 302 | 304 | 306 | 307 | 346 | 384 | 521
        | 613 | 620 | 640 | 798 | 940 | 1216 => "A07:2021",
        // A08 Software and Data Integrity Failures
        345 | 353 | 426 | 494 | 502 | 565 | 784 | 829 | 830 | 915 => "A08:2021",
        // A09 Security Logging and Monitoring Failures
        117 | 223 | 532 | 778 => "A09:2021",
        // A10 Server-Side Request Forgery
        918 => "A10:2021",
        _ => return None,
    };
    Some(cat)
}

/// Parses a `CWE-NNN` / `cwe-nnn` / bare-number token into its number.
pub fn parse_cwe(token: &str) -> Option<u32> {
    let t = token.trim();
    let num = t.strip_prefix("CWE-").or_else(|| t.strip_prefix("cwe-")).unwrap_or(t);
    num.trim().parse::<u32>().ok()
}

/// Returns the best OWASP category for a list of `CWE-NNN` strings: the first
/// CWE that maps wins (findings usually carry a single CWE).
pub fn owasp_for_cwes(cwes: &[String]) -> Option<String> {
    for c in cwes {
        if let Some(num) = parse_cwe(c) {
            if let Some(cat) = owasp_for_cwe(num) {
                return Some(cat.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_cwe_top_25_examples() {
        assert_eq!(owasp_for_cwe(79), Some("A03:2021")); // XSS
        assert_eq!(owasp_for_cwe(89), Some("A03:2021")); // SQLi
        assert_eq!(owasp_for_cwe(22), Some("A01:2021")); // path traversal
        assert_eq!(owasp_for_cwe(787), None); // out-of-bounds write (memory, unmapped)
        assert_eq!(owasp_for_cwe(798), Some("A07:2021")); // hardcoded creds
        assert_eq!(owasp_for_cwe(327), Some("A02:2021")); // weak crypto
        assert_eq!(owasp_for_cwe(502), Some("A08:2021")); // deserialization
        assert_eq!(owasp_for_cwe(918), Some("A10:2021")); // SSRF
    }

    #[test]
    fn parses_cwe_tokens() {
        assert_eq!(parse_cwe("CWE-79"), Some(79));
        assert_eq!(parse_cwe("cwe-089"), Some(89));
        assert_eq!(parse_cwe("22"), Some(22));
        assert_eq!(parse_cwe("not-a-cwe"), None);
    }

    #[test]
    fn owasp_for_cwes_takes_first_mappable() {
        assert_eq!(owasp_for_cwes(&["CWE-787".into(), "CWE-79".into()]), Some("A03:2021".into()));
        assert_eq!(owasp_for_cwes(&["CWE-787".into()]), None);
    }

    #[test]
    fn category_titles_resolve() {
        assert_eq!(category_title("A03:2021"), Some("Injection"));
        assert_eq!(category_title("A99:2021"), None);
    }
}
