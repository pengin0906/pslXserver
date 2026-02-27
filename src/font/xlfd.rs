// XLFD (X Logical Font Description) name parser
//
// Format: -foundry-family-weight-slant-setwidth-addstyle-pixel-point-resx-resy-spacing-avgwidth-registry-encoding
// Example: -adobe-courier-bold-o-normal--12-120-75-75-m-70-iso8859-1

/// Parsed XLFD name.
#[derive(Debug, Clone, Default)]
pub struct XlfdName {
    pub foundry: String,
    pub family: String,
    pub weight: String,       // "bold", "medium", "demibold", etc.
    pub slant: String,        // "r" (roman), "i" (italic), "o" (oblique)
    pub set_width: String,    // "normal", "condensed", etc.
    pub add_style: String,
    pub pixel_size: Option<u16>,
    pub point_size: Option<u16>,
    pub resolution_x: Option<u16>,
    pub resolution_y: Option<u16>,
    pub spacing: String,      // "m" (monospace), "p" (proportional), "c" (cell)
    pub average_width: Option<u16>,
    pub charset_registry: String,
    pub charset_encoding: String,
}

impl XlfdName {
    /// Parse an XLFD name string.
    pub fn parse(name: &str) -> Option<Self> {
        if !name.starts_with('-') {
            return None;
        }

        let parts: Vec<&str> = name[1..].splitn(14, '-').collect();
        if parts.len() < 14 {
            return None;
        }

        Some(Self {
            foundry: parts[0].to_string(),
            family: parts[1].to_string(),
            weight: parts[2].to_string(),
            slant: parts[3].to_string(),
            set_width: parts[4].to_string(),
            add_style: parts[5].to_string(),
            pixel_size: parse_opt_u16(parts[6]),
            point_size: parse_opt_u16(parts[7]),
            resolution_x: parse_opt_u16(parts[8]),
            resolution_y: parse_opt_u16(parts[9]),
            spacing: parts[10].to_string(),
            average_width: parse_opt_u16(parts[11]),
            charset_registry: parts[12].to_string(),
            charset_encoding: parts[13].to_string(),
        })
    }

    /// Check if this XLFD matches a pattern (with * and ? wildcards).
    pub fn matches_pattern(name: &str, pattern: &str) -> bool {
        // Simple wildcard matching
        if pattern == "*" {
            return true;
        }

        let name_lower = name.to_lowercase();
        let pattern_lower = pattern.to_lowercase();

        glob_match(&name_lower, &pattern_lower)
    }

    /// Check if this is a CJK font (Japanese, Chinese, Korean).
    pub fn is_cjk(&self) -> bool {
        let reg = self.charset_registry.to_lowercase();
        reg == "jisx0208.1983"
            || reg == "jisx0208"
            || reg == "jisx0212.1990"
            || reg == "jisx0201.1976"
            || reg == "gb2312.1980"
            || reg == "ksc5601.1987"
            || reg == "big5"
    }

    /// Map common XLFD family names to macOS font names.
    pub fn to_macos_family(&self) -> &str {
        let family = self.family.to_lowercase();
        match family.as_str() {
            "courier" | "courier new" => "Courier",
            "helvetica" => "Helvetica",
            "times" | "times new roman" => "Times New Roman",
            "fixed" | "misc" => "Menlo",
            "gothic" | "mincho" => {
                if self.is_cjk() {
                    "Hiragino Kaku Gothic ProN"
                } else {
                    "Menlo"
                }
            }
            _ => "Menlo", // fallback
        }
    }
}

fn parse_opt_u16(s: &str) -> Option<u16> {
    if s == "*" || s.is_empty() {
        None
    } else {
        s.parse().ok()
    }
}

fn glob_match(name: &str, pattern: &str) -> bool {
    let mut ni = 0;
    let mut pi = 0;
    let name = name.as_bytes();
    let pattern = pattern.as_bytes();
    let mut star_pi = usize::MAX;
    let mut star_ni = 0;

    while ni < name.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == name[ni]) {
            ni += 1;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_ni = ni;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_xlfd() {
        let xlfd = XlfdName::parse(
            "-adobe-courier-bold-o-normal--12-120-75-75-m-70-iso8859-1"
        );
        assert!(xlfd.is_some());
        let xlfd = xlfd.unwrap();
        assert_eq!(xlfd.foundry, "adobe");
        assert_eq!(xlfd.family, "courier");
        assert_eq!(xlfd.weight, "bold");
        assert_eq!(xlfd.slant, "o");
        assert_eq!(xlfd.pixel_size, Some(12));
        assert_eq!(xlfd.point_size, Some(120));
        assert_eq!(xlfd.spacing, "m");
        assert_eq!(xlfd.charset_registry, "iso8859");
        assert_eq!(xlfd.charset_encoding, "1");
    }

    #[test]
    fn test_parse_wildcard_xlfd() {
        let xlfd = XlfdName::parse(
            "-*-fixed-medium-r-*-*-14-*-*-*-*-*-iso8859-1"
        );
        assert!(xlfd.is_some());
        let xlfd = xlfd.unwrap();
        assert_eq!(xlfd.family, "fixed");
        assert_eq!(xlfd.pixel_size, Some(14));
    }

    #[test]
    fn test_glob_match() {
        assert!(XlfdName::matches_pattern("hello", "*"));
        assert!(XlfdName::matches_pattern("hello", "he*"));
        assert!(XlfdName::matches_pattern("hello", "h?llo"));
        assert!(!XlfdName::matches_pattern("hello", "world"));
    }
}
