//! XML file parsing.
//!
//! Extracts named XML elements, XSD type definitions, and XSLT templates
//! as individual code units using a simple line-based parser.

use super::types::{CodeUnit, Language, UnitType};
use std::path::Path;

/// Attributes that identify a named element, checked in priority order.
const NAME_ATTRS: &[&str] = &["id", "name", "x:Name", "x:Key", "match"];

/// Extract code units from an XML file.
///
/// Scans for elements with identifying attributes and extracts each as a
/// separate `CodeUnit`. Falls back to a single `Document` unit when no
/// named elements are found or the file is trivially small.
pub fn extract_xml_units(path: &Path, source: &str) -> Vec<CodeUnit> {
    let lines: Vec<&str> = source.lines().collect();

    // Trivially small files -> single Document unit
    if lines.len() <= 3 {
        return make_document_unit(path, source, &lines);
    }

    let mut units = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Skip non-element lines
        if !trimmed.starts_with('<') || trimmed.starts_with("<?") || trimmed.starts_with("<!") || trimmed.starts_with("</") {
            i += 1;
            continue;
        }

        // Try to parse opening tag info from this line
        if let Some(element) = parse_opening_tag(trimmed) {
            if let Some((attr_name, attr_value)) = find_name_attr(&element.tag_full, &element.tag_local) {
                let start_line = i + 1; // 1-indexed

                // Determine end line: self-closing or find matching close
                let end_line = if element.self_closing {
                    start_line
                } else {
                    find_closing_line(&lines, i, &element.tag_name) + 1 // 1-indexed
                };

                let code = lines[i..end_line].join("\n");
                let signature = build_signature(&element.tag_name, &element.tag_local, &attr_name, &attr_value);
                let name = attr_value.clone();
                let unit_type = pick_unit_type(&element.tag_local);

                let mut unit = CodeUnit::new(
                    name,
                    path.to_path_buf(),
                    start_line,
                    end_line,
                    Language::Xml,
                    unit_type,
                    None,
                );
                unit.signature = signature;
                unit.code = code;

                units.push(unit);

                // Jump past the element
                i = end_line; // end_line is 1-indexed, so this is next 0-indexed line
                continue;
            }
        }

        i += 1;
    }

    if units.is_empty() {
        return make_document_unit(path, source, &lines);
    }

    units
}

/// Info extracted from an opening tag line.
struct TagInfo {
    /// Full tag name including prefix, e.g. "xs:complexType"
    tag_name: String,
    /// Full tag text for attribute scanning (may span the opening line only)
    tag_full: String,
    /// Local name without namespace prefix, e.g. "complexType"
    tag_local: String,
    /// Whether the tag is self-closing (`/>`)
    self_closing: bool,
}

/// Parse an opening tag from a trimmed line.
fn parse_opening_tag(trimmed: &str) -> Option<TagInfo> {
    // Must start with '<' and contain a tag name
    let rest = trimmed.strip_prefix('<')?;

    // Extract tag name (up to whitespace, '/', or '>')
    let tag_end = rest.find(|c: char| c.is_whitespace() || c == '/' || c == '>')?;
    let tag_name = rest[..tag_end].to_string();

    if tag_name.is_empty() {
        return None;
    }

    let local = tag_name
        .split(':')
        .last()
        .unwrap_or(&tag_name)
        .to_string();

    let self_closing = trimmed.ends_with("/>");

    Some(TagInfo {
        tag_name,
        tag_full: trimmed.to_string(),
        tag_local: local,
        self_closing,
    })
}

/// Look for a naming attribute in the tag text. Returns (attr_name, attr_value).
fn find_name_attr(tag_text: &str, tag_local: &str) -> Option<(String, String)> {
    // For XSD types (complexType, simpleType, element, group, attributeGroup),
    // prioritise "name" attribute.
    // For XSLT templates, check "match" and "name".
    // For everything else, check the standard list.

    for &attr in NAME_ATTRS {
        if let Some(value) = extract_attr_value(tag_text, attr) {
            // Skip "name" on child property-like elements that are nested
            // (we still extract them; the caller decides what to keep)
            // For "match" attr, only accept on template-like elements
            if attr == "match" && tag_local != "template" {
                continue;
            }
            return Some((attr.to_string(), value));
        }
    }
    None
}

/// Extract the value of an attribute from a tag string.
fn extract_attr_value(tag: &str, attr: &str) -> Option<String> {
    // Search for `attr="value"` or `attr='value'`
    let patterns = [
        format!("{}=\"", attr),
        format!("{}='", attr),
    ];

    for pat in &patterns {
        if let Some(start) = tag.find(pat.as_str()) {
            let value_start = start + pat.len();
            let quote = pat.chars().last().unwrap(); // '"' or '\''
            if let Some(end) = tag[value_start..].find(quote) {
                return Some(tag[value_start..value_start + end].to_string());
            }
        }
    }
    None
}

/// Find the 0-indexed line of the closing tag for a given element.
fn find_closing_line(lines: &[&str], open_line: usize, tag_name: &str) -> usize {
    let close_tag = format!("</{}", tag_name);
    let mut depth = 1usize;
    let open_tag_prefix = format!("<{}", tag_name);

    for j in (open_line + 1)..lines.len() {
        let trimmed = lines[j].trim();

        // Check for self-closing instances of same tag (don't change depth)
        // Check for opening tags of same name
        if trimmed.starts_with(&open_tag_prefix) {
            let after = &trimmed[open_tag_prefix.len()..];
            if after.starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>') {
                if !trimmed.ends_with("/>") {
                    depth += 1;
                }
            }
        }

        if trimmed.contains(&close_tag) {
            depth -= 1;
            if depth == 0 {
                return j;
            }
        }
    }

    // If no closing tag found, return last line
    lines.len().saturating_sub(1)
}

/// Build a signature string based on element type.
fn build_signature(tag_name: &str, tag_local: &str, attr_name: &str, attr_value: &str) -> String {
    // XSD type definitions: "localName value"
    let is_xsd_type = matches!(
        tag_local,
        "complexType" | "simpleType" | "group" | "attributeGroup"
    );
    if is_xsd_type {
        return format!("{} {}", tag_local, attr_value);
    }

    // XSLT templates: "tag attr=value"
    if tag_local == "template" && (attr_name == "match" || attr_name == "name") {
        return format!("{} {}={}", tag_name, attr_name, attr_value);
    }

    // Named elements: "tagName#value"
    format!("{}#{}", tag_name, attr_value)
}

/// Choose a UnitType based on the element's local name.
fn pick_unit_type(tag_local: &str) -> UnitType {
    match tag_local {
        "complexType" | "simpleType" | "group" | "attributeGroup" => UnitType::Class,
        _ => UnitType::Function,
    }
}

/// Create a single Document unit for the whole file.
fn make_document_unit(path: &Path, source: &str, lines: &[&str]) -> Vec<CodeUnit> {
    if lines.is_empty() || lines.iter().all(|l| l.trim().is_empty()) {
        return Vec::new();
    }

    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document")
        .to_string();

    let end_line = lines.len();
    let mut unit = CodeUnit::new(
        title,
        path.to_path_buf(),
        1,
        end_line,
        Language::Xml,
        UnitType::Document,
        None,
    );
    unit.signature = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    unit.code = source.to_string();

    vec![unit]
}
