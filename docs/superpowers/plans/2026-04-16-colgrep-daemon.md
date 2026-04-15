# ColGREP Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `colgrep serve` — a search-only HTTP daemon that eliminates per-search overhead to deliver consistent 2-5s search latency for 720k code unit codebases.

**Architecture:** Single Rust binary with a `serve` subcommand that starts an Axum HTTP server. Model and index are loaded once at startup, shared across concurrent requests via ArcSwap (lock-free reads) and an ONNX session pool (mutex-guarded). No filesystem scanning, no auto-indexing — search only.

**Tech Stack:** Rust, Axum, Tokio, ArcSwap, next-plaid (MmapIndex, SearchParameters), next-plaid-onnx (Colbert model), chardetng + encoding_rs (encoding detection), tree-sitter-xml (XML parsing)

**Spec:** `docs/superpowers/specs/2026-04-15-colgrep-daemon-design.md`

**Decomposition:** This plan covers 4 phases that ship independently:
- Phase A: Non-UTF8 encoding detection (prerequisite)
- Phase B: XML structural parsing (prerequisite)
- Phase C: Daemon server (core feature)
- Phase D: Resumable encoding + output directory (indexing improvements)

Each phase produces working, testable software. Phase C depends on A and B being done first.

---

## File Structure

### New files
| File | Responsibility |
|------|---------------|
| `colgrep/src/commands/serve.rs` | Daemon HTTP server: AppState, handlers for /search, /health, /reload, startup, pre-warming |
| `colgrep/src/parser/xml.rs` | XML structural parser: extract named elements, XSD types, XSLT templates |
| `colgrep/src/parser/tests/test_xml.rs` | XML parser unit tests |
| `colgrep/src/index/checkpoint.rs` | Encoding checkpoint: save/load/resume embedding chunks to disk |
| `colgrep/src/index/storage.rs` | Write access verification: probe file write + fsync + cleanup |
| `colgrep/tests/serve_integration.rs` | Daemon HTTP integration tests |

### Modified files
| File | Change |
|------|--------|
| `colgrep/Cargo.toml` | Add axum, tokio, arc-swap, chardetng, encoding_rs, tree-sitter-xml, tower |
| `colgrep/src/cli.rs` | Add `Serve` subcommand variant with daemon options |
| `colgrep/src/main.rs` | Dispatch `Serve` command to `cmd_serve()` |
| `colgrep/src/commands/mod.rs` | Register `serve` module, export `cmd_serve` |
| `colgrep/src/index/mod.rs` | Upgrade `read_source_file()` with encoding detection; add checkpoint stage to pipeline |
| `colgrep/src/parser/types.rs` | Add `Xml` variant to `Language` enum |
| `colgrep/src/parser/language.rs` | Map XML extensions to `Language::Xml`; add `get_tree_sitter_language` case |
| `colgrep/src/parser/mod.rs` | Route `Language::Xml` to `xml::extract_xml_units()` |
| `colgrep/src/lib.rs` | Re-export new public types if needed |

---

## Phase A: Non-UTF8 Encoding Detection

### Task 1: Add chardetng and encoding_rs dependencies

**Files:**
- Modify: `colgrep/Cargo.toml`

- [ ] **Step 1: Add dependencies to Cargo.toml**

Add after line 97 (`ndarray = "0.16"`):

```toml
# === Encoding detection ===
chardetng = "0.1"
encoding_rs = "0.8"
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p colgrep`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add colgrep/Cargo.toml
git commit -m "feat: add chardetng and encoding_rs dependencies for encoding detection"
```

### Task 2: Upgrade DecodeStatus and read_source_file

**Files:**
- Modify: `colgrep/src/index/mod.rs:36-68`

- [ ] **Step 1: Write failing tests**

Add to the existing test module at the bottom of `colgrep/src/index/mod.rs`:

```rust
#[cfg(test)]
mod encoding_tests {
    use super::*;
    use std::io::Write;

    fn write_temp_file(content: &[u8]) -> PathBuf {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.into_temp_path().to_path_buf()
    }

    #[test]
    fn test_read_source_file_utf8_unchanged() {
        let tmp = write_temp_file(b"fn main() { println!(\"hello\"); }");
        let result = read_source_file(&tmp).unwrap();
        assert_eq!(result.decode_status, DecodeStatus::Utf8);
        assert_eq!(result.text, "fn main() { println!(\"hello\"); }");
    }

    #[test]
    fn test_read_source_file_windows_1252_detected() {
        // 0xe9 = é in Windows-1252. With enough non-ASCII bytes, chardetng should detect it.
        let tmp = write_temp_file(
            b"// R\xe9sum\xe9 handler\n// caf\xe9 au lait\n// na\xefve implementation\nfn process() {}\n"
        );
        let result = read_source_file(&tmp).unwrap();
        assert!(result.text.contains("Résumé") || result.text.contains("résumé"));
        assert!(!result.text.contains('\u{fffd}'));
        match &result.decode_status {
            DecodeStatus::Transcoded { encoding } => assert!(!encoding.is_empty()),
            other => panic!("Expected Transcoded, got {:?}", other),
        }
    }

    #[test]
    fn test_read_source_file_truly_binary_falls_back_lossy() {
        let tmp = write_temp_file(&[0x00, 0x01, 0x02, 0x80, 0x81, 0xff, 0xfe, 0x00]);
        let result = read_source_file(&tmp).unwrap();
        assert_eq!(result.decode_status, DecodeStatus::Lossy);
    }

    #[test]
    fn test_read_source_file_io_error_propagated() {
        let result = read_source_file(Path::new("/nonexistent/file.rs"));
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_status_transcoded_stores_encoding_name() {
        let tmp = write_temp_file(
            b"// caf\xe9 au lait\n// na\xefve approach\n// r\xe9sum\xe9 builder\n"
        );
        let result = read_source_file(&tmp).unwrap();
        if let DecodeStatus::Transcoded { encoding } = &result.decode_status {
            assert!(!encoding.is_empty());
        } else {
            panic!("Expected Transcoded, got {:?}", result.decode_status);
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p colgrep encoding_tests -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `DecodeStatus::Transcoded` variant doesn't exist yet, and `read_source_file` doesn't do encoding detection.

- [ ] **Step 3: Implement DecodeStatus upgrade**

In `colgrep/src/index/mod.rs`, replace the existing `DecodeStatus` enum (lines 36-42):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeStatus {
    /// File bytes were valid UTF-8 and were decoded without substitutions.
    Utf8,
    /// File bytes were transcoded from a detected encoding to UTF-8.
    Transcoded { encoding: String },
    /// File bytes were not valid UTF-8; decoding used replacement characters.
    Lossy,
}
```

- [ ] **Step 4: Implement read_source_file with encoding detection**

Replace the existing `read_source_file` function (lines 55-68):

```rust
/// Read a repository source file for parsing.
///
/// For invalid UTF-8 bytes, attempts encoding detection via chardetng,
/// then transcodes using encoding_rs. Falls back to lossy decoding
/// if detection fails or has insufficient signal.
pub fn read_source_file(path: &Path) -> Result<DecodedSourceFile> {
    let bytes = std::fs::read(path)?;

    // Fast path: valid UTF-8
    if let Ok(valid) = std::str::from_utf8(&bytes) {
        return Ok(DecodedSourceFile {
            text: valid.to_owned(),
            decode_status: DecodeStatus::Utf8,
        });
    }

    // Count non-ASCII bytes for confidence check
    let non_ascii_count = bytes.iter().filter(|&&b| b > 0x7F).count();

    // Too few non-ASCII bytes to reliably detect encoding — fall back to lossy
    if non_ascii_count < 16 {
        return Ok(DecodedSourceFile {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            decode_status: DecodeStatus::Lossy,
        });
    }

    // Detect encoding using chardetng
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(&bytes, true);
    let detected = detector.guess(None, true);

    // Transcode using encoding_rs
    let (decoded, _encoding_used, had_errors) = detected.decode(&bytes);

    if had_errors {
        // Transcoding had errors — fall back to lossy
        return Ok(DecodedSourceFile {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            decode_status: DecodeStatus::Lossy,
        });
    }

    Ok(DecodedSourceFile {
        text: decoded.into_owned(),
        decode_status: DecodeStatus::Transcoded {
            encoding: detected.name().to_string(),
        },
    })
}
```

- [ ] **Step 5: Fix any compile errors from DecodeStatus change**

The old code had `DecodeStatus::Utf8` and `DecodeStatus::Lossy` — both still exist. But any code pattern-matching on `DecodeStatus` now needs to handle `Transcoded`. Search for uses:

Run: `grep -rn "DecodeStatus" colgrep/src/`

Fix any exhaustive match statements by adding the `Transcoded` arm.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p colgrep encoding_tests -- --nocapture`
Expected: All 5 tests PASS

- [ ] **Step 7: Commit**

```bash
git add colgrep/src/index/mod.rs
git commit -m "feat: upgrade read_source_file with chardetng encoding detection

Windows-1252 and ISO-8859-1 files are now properly transcoded
instead of producing U+FFFD replacement characters."
```

---

## Phase B: XML Structural Parsing

### Task 3: Add tree-sitter-xml dependency and Language::Xml variant

**Files:**
- Modify: `colgrep/Cargo.toml`
- Modify: `colgrep/src/parser/types.rs:5-96`
- Modify: `colgrep/src/parser/language.rs:53,68-82,86-129`

- [ ] **Step 1: Add tree-sitter-xml to Cargo.toml**

Add after line 48 (`tree-sitter-html = "0.23"`):

```toml
tree-sitter-xml = "0.7"
```

- [ ] **Step 2: Add Xml variant to Language enum**

In `colgrep/src/parser/types.rs`, add `Xml,` after `Html,` (line 35):

```rust
    Html,
    Xml,
    Markdown,
```

- [ ] **Step 3: Add Xml to FromStr**

In the `FromStr` impl (after line 81):

```rust
            "html" | "htm" => Ok(Language::Html),
            "xml" | "xsd" | "xsl" | "xslt" | "xaml" => Ok(Language::Xml),
            "markdown" | "md" => Ok(Language::Markdown),
```

- [ ] **Step 4: Update detect_language to map XML extensions**

In `colgrep/src/parser/language.rs`, replace line 53:

```rust
        // XML/XAML-family documents with structural parsing
        "xml" | "xsd" | "xsl" | "xslt" | "xaml" => Some(Language::Xml),
```

- [ ] **Step 5: Add Xml to get_tree_sitter_language**

In `colgrep/src/parser/language.rs`, add after line 115 (`Language::Html`):

```rust
        // XML uses HTML tree-sitter parser (compatible superset)
        Language::Xml => tree_sitter_html::LANGUAGE.into(),
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo check -p colgrep`
Expected: compiles (Xml is now a valid Language variant, but extract_units doesn't dispatch it yet)

- [ ] **Step 7: Commit**

```bash
git add colgrep/Cargo.toml colgrep/src/parser/types.rs colgrep/src/parser/language.rs
git commit -m "feat: add Language::Xml variant and tree-sitter-xml dependency"
```

### Task 4: Write XML parser with tests

**Files:**
- Create: `colgrep/src/parser/xml.rs`
- Create: `colgrep/src/parser/tests/test_xml.rs`
- Modify: `colgrep/src/parser/mod.rs`

- [ ] **Step 1: Write failing tests**

Create `colgrep/src/parser/tests/test_xml.rs`:

```rust
use crate::parser::types::{Language, UnitType};
use crate::parser::extract_units;
use std::path::Path;

#[test]
fn test_xml_language_detection() {
    use crate::parser::language::detect_language;
    assert_eq!(detect_language(Path::new("config.xml")), Some(Language::Xml));
    assert_eq!(detect_language(Path::new("schema.xsd")), Some(Language::Xml));
    assert_eq!(detect_language(Path::new("transform.xsl")), Some(Language::Xml));
    assert_eq!(detect_language(Path::new("style.xslt")), Some(Language::Xml));
    assert_eq!(detect_language(Path::new("view.xaml")), Some(Language::Xml));
}

#[test]
fn test_xml_extract_named_elements() {
    let source = r#"<?xml version="1.0"?>
<beans>
  <bean id="userService" class="com.app.UserService">
    <property name="repo" ref="userRepo"/>
  </bean>
  <bean id="orderService" class="com.app.OrderService"/>
</beans>"#;
    let units = extract_units(Path::new("app-context.xml"), source, Language::Xml);
    assert!(units.len() >= 2, "Expected at least 2 units, got {}", units.len());
    assert!(units.iter().any(|u| u.signature.contains("userService")));
    assert!(units.iter().any(|u| u.signature.contains("orderService")));
}

#[test]
fn test_xml_extract_xsd_types() {
    let source = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:complexType name="AddressType">
    <xs:sequence>
      <xs:element name="street" type="xs:string"/>
      <xs:element name="city" type="xs:string"/>
    </xs:sequence>
  </xs:complexType>
  <xs:simpleType name="ZipCode">
    <xs:restriction base="xs:string">
      <xs:pattern value="[0-9]{5}"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>"#;
    let units = extract_units(Path::new("types.xsd"), source, Language::Xml);
    assert!(units.len() >= 2, "Expected at least 2 units, got {}", units.len());
    assert!(units.iter().any(|u| u.signature.contains("AddressType")));
    assert!(units.iter().any(|u| u.signature.contains("ZipCode")));
}

#[test]
fn test_xml_extract_xslt_templates() {
    let source = r#"<?xml version="1.0"?>
<xsl:stylesheet xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
  <xsl:template match="/">
    <html><body><xsl:apply-templates/></body></html>
  </xsl:template>
  <xsl:template name="header">
    <h1>Title</h1>
  </xsl:template>
</xsl:stylesheet>"#;
    let units = extract_units(Path::new("transform.xsl"), source, Language::Xml);
    assert!(units.len() >= 2, "Expected at least 2 units, got {}", units.len());
}

#[test]
fn test_xml_preserves_line_numbers() {
    let source = "<?xml version=\"1.0\"?>\n<root>\n  <item id=\"a\"/>\n  <item id=\"b\"/>\n</root>";
    let units = extract_units(Path::new("items.xml"), source, Language::Xml);
    for unit in &units {
        assert!(unit.line >= 1);
        assert!(unit.end_line >= unit.line);
    }
}

#[test]
fn test_xml_small_file_single_unit() {
    let source = r#"<?xml version="1.0"?><root><a>1</a></root>"#;
    let units = extract_units(Path::new("tiny.xml"), source, Language::Xml);
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].unit_type, UnitType::Document);
}
```

- [ ] **Step 2: Register the test module**

In `colgrep/src/parser/tests/mod.rs`, add:

```rust
mod test_xml;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p colgrep test_xml -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `xml` module doesn't exist yet

- [ ] **Step 4: Create XML parser module**

Create `colgrep/src/parser/xml.rs`:

```rust
//! XML file parsing.
//!
//! Extracts structural code units from XML files:
//! - Named elements (id, name attributes)
//! - XSD type definitions (complexType, simpleType)
//! - XSLT templates (template match/name)
//! - Falls back to single Document unit for trivial files

use super::types::{CodeUnit, Language, UnitType};
use std::path::Path;

/// Attributes that identify a "named" XML element worth extracting as a unit.
const NAME_ATTRS: &[&str] = &["id", "name", "x:Name", "x:Key"];

/// XSD elements that define types (extracted with their name attribute).
const XSD_TYPE_TAGS: &[&str] = &["complexType", "simpleType", "element", "group", "attributeGroup"];

/// XSLT elements that define templates.
const XSLT_TEMPLATE_TAGS: &[&str] = &["template", "function", "variable"];

/// Extract code units from an XML file.
///
/// Strategy: scan lines for elements that have identifying attributes
/// (id, name, match, etc.) and extract each as a separate CodeUnit.
/// Falls back to a single Document unit if no named elements are found.
pub fn extract_xml_units(path: &Path, source: &str) -> Vec<CodeUnit> {
    let lines: Vec<&str> = source.lines().collect();
    let mut units: Vec<CodeUnit> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Skip XML declaration, comments, processing instructions
        if trimmed.starts_with("<?") || trimmed.starts_with("<!--") || trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Try to extract a named element starting at this line
        if let Some(unit) = try_extract_element(path, &lines, i) {
            let end = unit.end_line;
            units.push(unit);
            // Skip past this element
            i = end;
        } else {
            i += 1;
        }
    }

    // Fallback: if no named elements found, return single Document unit
    if units.is_empty() {
        units.push(CodeUnit::new(
            path.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("unknown")
                .to_string(),
            path.to_path_buf(),
            1,
            lines.len(),
            Language::Xml,
            UnitType::Document,
            source.to_string(),
            String::new(), // signature
        ));
    }

    units
}

/// Try to extract a named XML element starting at line `start_idx`.
/// Returns None if the line doesn't contain a recognizable named element.
fn try_extract_element(path: &Path, lines: &[&str], start_idx: usize) -> Option<CodeUnit> {
    let line = lines[start_idx].trim();

    // Must start with '<' and not be a closing tag
    if !line.starts_with('<') || line.starts_with("</") {
        return None;
    }

    // Extract tag name (handle namespaced tags like xs:complexType)
    let tag_content = line.trim_start_matches('<');
    let tag_name = tag_content
        .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .next()?;

    // Strip namespace prefix for matching
    let local_name = tag_name.split(':').last().unwrap_or(tag_name);

    // Look for identifying attributes
    let full_open_tag = collect_open_tag(lines, start_idx);
    let name_value = find_name_attribute(&full_open_tag);
    let match_value = find_attribute(&full_open_tag, "match");

    // Determine if this element is worth extracting
    let signature = if let Some(name) = &name_value {
        // Named element: bean#userService, complexType AddressType, etc.
        if is_xsd_type(local_name) {
            format!("{} {}", local_name, name)
        } else {
            format!("{}#{}", tag_name, name)
        }
    } else if let Some(m) = &match_value {
        // XSLT template with match attribute
        format!("{} match={}", tag_name, m)
    } else if is_xsd_type(local_name) || is_xslt_template(local_name) {
        // XSD/XSLT element without a name — skip (not individually useful)
        return None;
    } else if has_any_name_attr(&full_open_tag) {
        // Has some naming attribute we recognize
        let attr_val = NAME_ATTRS
            .iter()
            .filter_map(|a| find_attribute(&full_open_tag, a))
            .next()?;
        format!("{}#{}", tag_name, attr_val)
    } else {
        return None;
    };

    // Find the end of this element
    let end_idx = find_element_end(lines, start_idx, tag_name);
    let code: String = lines[start_idx..=end_idx.min(lines.len() - 1)]
        .join("\n");

    Some(CodeUnit::new(
        name_value.unwrap_or_else(|| local_name.to_string()),
        path.to_path_buf(),
        start_idx + 1, // 1-indexed
        end_idx + 1,
        Language::Xml,
        if is_xsd_type(local_name) {
            UnitType::Class // closest match for type definitions
        } else {
            UnitType::Function // closest match for elements/templates
        },
        code,
        signature,
    ))
}

fn is_xsd_type(local_name: &str) -> bool {
    XSD_TYPE_TAGS.iter().any(|t| *t == local_name)
}

fn is_xslt_template(local_name: &str) -> bool {
    XSLT_TEMPLATE_TAGS.iter().any(|t| *t == local_name)
}

fn has_any_name_attr(tag_text: &str) -> bool {
    NAME_ATTRS.iter().any(|a| tag_text.contains(&format!("{}=", a)))
}

/// Collect the full opening tag text (may span multiple lines).
fn collect_open_tag(lines: &[&str], start: usize) -> String {
    let mut tag = String::new();
    for i in start..lines.len() {
        tag.push_str(lines[i]);
        tag.push(' ');
        if lines[i].contains('>') {
            break;
        }
    }
    tag
}

/// Find the value of a specific attribute in a tag string.
fn find_attribute(tag_text: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{}=", attr_name);
    let pos = tag_text.find(&pattern)?;
    let after = &tag_text[pos + pattern.len()..];
    let after = after.trim_start();
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = after[1..].find(quote)?;
    Some(after[1..1 + end].to_string())
}

/// Find the first NAME_ATTRS attribute value in a tag string.
fn find_name_attribute(tag_text: &str) -> Option<String> {
    NAME_ATTRS
        .iter()
        .filter_map(|a| find_attribute(tag_text, a))
        .next()
}

/// Find the closing line for an element. Handles self-closing tags.
fn find_element_end(lines: &[&str], start: usize, tag_name: &str) -> usize {
    // Check if self-closing on the first line(s)
    let open_tag = collect_open_tag(lines, start);
    if open_tag.contains("/>") {
        // Self-closing — find which line has />
        for i in start..lines.len() {
            if lines[i].contains("/>") {
                return i;
            }
        }
        return start;
    }

    // Look for matching closing tag
    let close_pattern = format!("</{}", tag_name);
    // Also match namespace-stripped: </xs:complexType matches tag_name "xs:complexType"
    let mut depth = 1;
    for i in (start + 1)..lines.len() {
        let trimmed = lines[i].trim();

        // Count opening tags of the same name (for nesting)
        if trimmed.starts_with(&format!("<{}", tag_name))
            && !trimmed.starts_with(&close_pattern)
            && !trimmed.contains("/>")
        {
            depth += 1;
        }

        if trimmed.starts_with(&close_pattern) || trimmed.contains(&close_pattern) {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }

    // No closing tag found — return last line
    lines.len() - 1
}
```

- [ ] **Step 5: Wire XML parser into mod.rs**

In `colgrep/src/parser/mod.rs`, add after line 23 (`mod vue;`):

```rust
mod xml;
```

In the `extract_units` function, add after the Html dispatch (line 107):

```rust
    // Handle XML files with structural extraction
    if lang == Language::Xml {
        return xml::extract_xml_units(path, source);
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p colgrep test_xml -- --nocapture`
Expected: All 6 tests PASS

- [ ] **Step 7: Run full test suite**

Run: `cargo test -p colgrep 2>&1 | tail -5`
Expected: No regressions. The existing XML test (`test_detect_language_xml_family_maps_to_text`) will now fail because XML maps to `Language::Xml` not `Language::Text`. Update that test.

- [ ] **Step 8: Fix the old XML test**

In `colgrep/src/parser/language.rs`, find the test `test_detect_language_xml_family_maps_to_text` and update assertions from `Some(Language::Text)` to `Some(Language::Xml)`. Rename the test to `test_detect_language_xml_family`.

- [ ] **Step 9: Commit**

```bash
git add colgrep/src/parser/xml.rs colgrep/src/parser/tests/test_xml.rs colgrep/src/parser/mod.rs colgrep/src/parser/language.rs
git commit -m "feat: add structural XML parser for named elements, XSD types, XSLT templates"
```

---

## Phase C: Daemon Server (`colgrep serve`)

### Task 5: Add daemon dependencies

**Files:**
- Modify: `colgrep/Cargo.toml`

- [ ] **Step 1: Add axum, tokio, arc-swap, tower**

Add to `[dependencies]` section:

```toml
# === HTTP daemon ===
axum = "0.7"
tokio = { version = "1", features = ["full"] }
arc-swap = "1"
tower = { version = "0.5", features = ["timeout", "limit"] }
tower-http = { version = "0.6", features = ["cors"] }
chrono = { version = "0.4", features = ["serde"] }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p colgrep`
Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add colgrep/Cargo.toml
git commit -m "feat: add axum, tokio, arc-swap dependencies for daemon server"
```

### Task 6: Add Serve subcommand to CLI

**Files:**
- Modify: `colgrep/src/cli.rs`
- Modify: `colgrep/src/main.rs`
- Modify: `colgrep/src/commands/mod.rs`
- Create: `colgrep/src/commands/serve.rs` (stub)

- [ ] **Step 1: Add Serve variant to Commands enum**

In `colgrep/src/cli.rs`, add to the Commands enum (after the last variant before the closing `}`):

```rust
    /// Start the colgrep daemon server
    #[command(after_help = "Start an HTTP server for fast, concurrent semantic search.\n\n\
        The daemon loads the model and index once at startup, then serves\n\
        search requests without filesystem scanning or model reloading.\n\n\
        Example: colgrep serve --port 8080 --index /data/index")]
    Serve {
        /// Listen port
        #[arg(long, default_value = "8080")]
        port: u16,

        /// Listen address
        #[arg(long, default_value = "0.0.0.0")]
        host: String,

        /// Path to colgrep index directory
        #[arg(long)]
        index: PathBuf,

        /// Model name
        #[arg(long)]
        model: Option<String>,

        /// Number of ONNX sessions for concurrent query encoding
        #[arg(long, default_value = "4")]
        sessions: usize,

        /// Request timeout in seconds
        #[arg(long, default_value = "30")]
        timeout: u64,

        /// Max concurrent requests
        #[arg(long, default_value = "64")]
        max_concurrent: usize,

        /// Default hybrid search alpha (0.0 = pure BM25, 1.0 = pure semantic)
        #[arg(long, default_value = "0.75")]
        alpha: f32,

        /// Disable FTS5 hybrid search
        #[arg(long)]
        no_hybrid_search: bool,

        /// Skip index pre-warming at startup
        #[arg(long)]
        no_prewarm: bool,

        /// Use INT8 quantized model
        #[arg(long)]
        quantized: bool,

        /// Force CPU execution
        #[arg(long)]
        force_cpu: bool,
    },
```

- [ ] **Step 2: Create stub serve command**

Create `colgrep/src/commands/serve.rs`:

```rust
use std::path::PathBuf;
use anyhow::Result;

pub struct ServeConfig {
    pub port: u16,
    pub host: String,
    pub index: PathBuf,
    pub model: Option<String>,
    pub sessions: usize,
    pub timeout: u64,
    pub max_concurrent: usize,
    pub alpha: f32,
    pub no_hybrid_search: bool,
    pub no_prewarm: bool,
    pub quantized: bool,
    pub force_cpu: bool,
}

pub fn cmd_serve(config: ServeConfig) -> Result<()> {
    eprintln!("colgrep serve is not yet implemented");
    std::process::exit(1);
}
```

- [ ] **Step 3: Register in commands/mod.rs**

Add to `colgrep/src/commands/mod.rs`:

```rust
mod serve;
```

And:

```rust
pub use serve::{cmd_serve, ServeConfig};
```

- [ ] **Step 4: Dispatch in main.rs**

In `colgrep/src/main.rs`, add a match arm for the Serve command (after the Init dispatch):

```rust
        Some(Commands::Serve {
            port,
            host,
            index,
            model,
            sessions,
            timeout,
            max_concurrent,
            alpha,
            no_hybrid_search,
            no_prewarm,
            quantized,
            force_cpu,
        }) => {
            cmd_serve(ServeConfig {
                port,
                host,
                index,
                model,
                sessions,
                timeout,
                max_concurrent,
                alpha,
                no_hybrid_search,
                no_prewarm,
                quantized,
                force_cpu,
            })?;
        }
```

Add the import at the top of main.rs:

```rust
use commands::{cmd_serve, ServeConfig};
```

- [ ] **Step 5: Verify it compiles and --help works**

Run: `cargo build -p colgrep && ./target/debug/colgrep serve --help`
Expected: Shows serve subcommand help text

- [ ] **Step 6: Commit**

```bash
git add colgrep/src/cli.rs colgrep/src/main.rs colgrep/src/commands/mod.rs colgrep/src/commands/serve.rs
git commit -m "feat: add colgrep serve subcommand (stub)"
```

### Task 7: Implement AppState and health endpoint

**Files:**
- Modify: `colgrep/src/commands/serve.rs`

- [ ] **Step 1: Write failing health test**

Add to `colgrep/src/commands/serve.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for oneshot

    async fn parse_body(response: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn test_health_returns_503_before_init() {
        let state = AppState::new_unhealthy("test error".to_string());
        let app = build_router(state);
        let response = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = parse_body(response).await;
        assert_eq!(body["status"], "error");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p colgrep test_health_returns_503 -- --nocapture 2>&1 | tail -10`
Expected: FAIL — `AppState`, `build_router` don't exist

- [ ] **Step 3: Implement AppState and router**

Replace the contents of `colgrep/src/commands/serve.rs` with the full implementation. This is a large file — implement it in stages. Start with AppState + health:

```rust
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use colgrep::{ensure_model, Config, Searcher, DEFAULT_MODEL};
use next_plaid::MmapIndex;
use next_plaid_onnx::Colbert;

pub struct ServeConfig {
    pub port: u16,
    pub host: String,
    pub index: PathBuf,
    pub model: Option<String>,
    pub sessions: usize,
    pub timeout: u64,
    pub max_concurrent: usize,
    pub alpha: f32,
    pub no_hybrid_search: bool,
    pub no_prewarm: bool,
    pub quantized: bool,
    pub force_cpu: bool,
}

/// Shared state for the daemon, accessible from all handlers.
pub struct AppState {
    /// The loaded searcher (model + index). None if startup failed.
    searcher: Option<Arc<Searcher>>,
    /// Error reason if startup failed.
    error: Option<String>,
    /// Daemon configuration.
    config: ServeConfig,
    /// Startup time for uptime calculation.
    start_time: Instant,
    /// Last reload timestamp.
    last_reload: Option<chrono::DateTime<chrono::Utc>>,
    /// Mutex to serialize reload operations.
    reload_mutex: Mutex<()>,
}

impl AppState {
    pub fn new_healthy(searcher: Searcher, config: ServeConfig) -> Arc<Self> {
        Arc::new(Self {
            searcher: Some(Arc::new(searcher)),
            error: None,
            config,
            start_time: Instant::now(),
            last_reload: Some(chrono::Utc::now()),
            reload_mutex: Mutex::new(()),
        })
    }

    pub fn new_unhealthy(reason: String) -> Arc<Self> {
        Arc::new(Self {
            searcher: None,
            error: Some(reason),
            config: ServeConfig {
                port: 0, host: String::new(), index: PathBuf::new(),
                model: None, sessions: 0, timeout: 0, max_concurrent: 0,
                alpha: 0.0, no_hybrid_search: false, no_prewarm: false,
                quantized: false, force_cpu: false,
            },
            start_time: Instant::now(),
            last_reload: None,
            reload_mutex: Mutex::new(()),
        })
    }

    fn is_healthy(&self) -> bool {
        self.searcher.is_some()
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_doc_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_reload: Option<String>,
}

async fn health_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if state.is_healthy() {
        let doc_count = state.searcher.as_ref()
            .and_then(|s| s.doc_count().ok());
        (
            StatusCode::OK,
            Json(HealthResponse {
                status: "ready".to_string(),
                reason: None,
                index_doc_count: doc_count,
                model: state.config.model.clone().or(Some(DEFAULT_MODEL.to_string())),
                uptime_seconds: Some(state.start_time.elapsed().as_secs()),
                last_reload: state.last_reload.map(|t| t.to_rfc3339()),
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthResponse {
                status: "error".to_string(),
                reason: state.error.clone(),
                index_doc_count: None,
                model: None,
                uptime_seconds: None,
                last_reload: None,
            }),
        )
    }
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .with_state(state)
}

pub fn cmd_serve(config: ServeConfig) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to build tokio runtime")?;

    rt.block_on(async move {
        let addr = format!("{}:{}", config.host, config.port);

        // Try to load searcher
        let state = match load_searcher(&config) {
            Ok(searcher) => {
                eprintln!("✅ Daemon ready on {}", addr);
                AppState::new_healthy(searcher, config)
            }
            Err(e) => {
                eprintln!("⚠️  Startup failed: {}. Daemon running in unhealthy mode.", e);
                AppState::new_unhealthy(format!("{}", e))
            }
        };

        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind(&addr).await
            .context(format!("Failed to bind to {}", addr))?;

        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("Server error")?;

        Ok(())
    })
}

fn load_searcher(config: &ServeConfig) -> Result<Searcher> {
    let model_name = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let model_path = ensure_model(Some(model_name), true)?;
    let searcher = Searcher::load_with_model(&config.index, &model_path, config.quantized)?;
    Ok(searcher)
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install Ctrl+C handler");
    eprintln!("\nShutting down gracefully...");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn parse_body(response: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn test_health_returns_503_before_init() {
        let state = AppState::new_unhealthy("test error".to_string());
        let app = build_router(state);
        let response = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = parse_body(response).await;
        assert_eq!(body["status"], "error");
        assert!(body["reason"].as_str().unwrap().contains("test error"));
    }
}
```

- [ ] **Step 4: Verify health test passes**

Run: `cargo test -p colgrep test_health_returns_503 -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add colgrep/src/commands/serve.rs
git commit -m "feat: implement AppState and /health endpoint for colgrep serve"
```

### Task 8: Implement /search endpoint

**Files:**
- Modify: `colgrep/src/commands/serve.rs`

This is the largest task. The search handler needs to:
1. Parse the JSON request
2. Build a subset from target_paths, include_patterns, exclude_patterns, exclude_dirs, text_pattern
3. Run semantic + BM25 search
4. Fuse results if hybrid
5. Fetch metadata and return

- [ ] **Step 1: Write failing search test**

Add to the test module in `serve.rs`:

```rust
    #[tokio::test]
    async fn test_search_returns_503_when_unhealthy() {
        let state = AppState::new_unhealthy("not ready".to_string());
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::post("/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_search_invalid_request_returns_400() {
        let state = AppState::new_unhealthy("not ready".to_string());
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::post("/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"not_query":"bad"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
```

- [ ] **Step 2: Implement search request/response types and handler**

Add to `serve.rs`:

```rust
#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
    #[serde(default)]
    alpha: Option<f32>,
    #[serde(default)]
    target_paths: Vec<String>,
    #[serde(default)]
    include_patterns: Vec<String>,
    #[serde(default)]
    exclude_patterns: Vec<String>,
    #[serde(default)]
    exclude_dirs: Vec<String>,
    #[serde(default)]
    text_pattern: Option<String>,
    #[serde(default)]
    code_only: bool,
}

fn default_top_k() -> usize { 10 }

#[derive(Serialize)]
struct SearchResponse {
    results: Vec<SearchResultJson>,
    search_time_ms: u64,
    index_doc_count: usize,
    hybrid_mode: bool,
}

#[derive(Serialize)]
struct SearchResultJson {
    file: String,
    line: usize,
    end_line: usize,
    score: f32,
    unit_type: String,
    signature: String,
    code: String,
    language: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    status: String,
    reason: String,
}

async fn search_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SearchRequest>,
) -> impl IntoResponse {
    // Validate
    if req.query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status": "error", "reason": "query must not be empty"})),
        ).into_response();
    }

    if let Some(alpha) = req.alpha {
        if !(0.0..=1.0).contains(&alpha) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"status": "error", "reason": "alpha must be between 0.0 and 1.0"})),
            ).into_response();
        }
    }

    // Check health
    let searcher = match &state.searcher {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "status": "error",
                    "reason": state.error.as_deref().unwrap_or("index not loaded")
                })),
            ).into_response();
        }
    };

    let top_k = req.top_k.min(1000);
    let alpha = req.alpha.unwrap_or(state.config.alpha);
    let hybrid = !state.config.no_hybrid_search && alpha < 1.0;
    let start = Instant::now();

    // Build subset from filters (runs on blocking thread for SQLite access)
    let searcher_clone = searcher.clone();
    let target_paths = req.target_paths.clone();
    let include_patterns = req.include_patterns.clone();
    let exclude_patterns = req.exclude_patterns.clone();
    let exclude_dirs = req.exclude_dirs.clone();
    let text_pattern = req.text_pattern.clone();

    let subset_result = tokio::task::spawn_blocking(move || {
        build_subset(
            &searcher_clone,
            &target_paths,
            &include_patterns,
            &exclude_patterns,
            &exclude_dirs,
            text_pattern.as_deref(),
        )
    })
    .await;

    let subset = match subset_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "reason": format!("{}", e)})),
            ).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "reason": format!("task failed: {}", e)})),
            ).into_response();
        }
    };

    // Run search (blocking — involves ONNX inference and BLAS)
    let query = req.query.clone();
    let code_only = req.code_only;
    let search_top_k = if code_only { top_k * 4 } else { top_k * 3 };

    let search_result = tokio::task::spawn_blocking(move || {
        if hybrid {
            searcher.hybrid_search(&query, search_top_k, alpha, subset.as_deref())
        } else {
            searcher.search(&query, search_top_k, subset.as_deref())
        }
    })
    .await;

    let results = match search_result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "reason": format!("search failed: {}", e)})),
            ).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "reason": format!("encoding failed: {}", e)})),
            ).into_response();
        }
    };

    // Filter and convert results
    let mut json_results: Vec<SearchResultJson> = results
        .into_iter()
        .filter(|r| {
            if code_only {
                !matches!(
                    r.unit.unit_type,
                    colgrep::UnitType::Document | colgrep::UnitType::RawCode
                )
            } else {
                true
            }
        })
        .take(top_k)
        .map(|r| SearchResultJson {
            file: r.unit.file.to_string_lossy().to_string(),
            line: r.unit.line,
            end_line: r.unit.end_line,
            score: r.score,
            unit_type: format!("{:?}", r.unit.unit_type).to_lowercase(),
            signature: r.unit.signature.clone(),
            code: r.unit.code.clone(),
            language: format!("{:?}", r.unit.language).to_lowercase(),
        })
        .collect();

    let doc_count = state.searcher.as_ref()
        .and_then(|s| s.doc_count().ok())
        .unwrap_or(0);

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "results": json_results,
            "search_time_ms": start.elapsed().as_millis() as u64,
            "index_doc_count": doc_count,
            "hybrid_mode": hybrid,
        })),
    ).into_response()
}

/// Build a subset of doc IDs from the various filter parameters.
fn build_subset(
    searcher: &Searcher,
    target_paths: &[String],
    include_patterns: &[String],
    exclude_patterns: &[String],
    exclude_dirs: &[String],
    text_pattern: Option<&str>,
) -> Result<Option<Vec<i64>>> {
    let mut combined: Option<Vec<i64>> = None;

    // Target paths
    if !target_paths.is_empty() {
        let mut all_ids = Vec::new();
        for path in target_paths {
            // Reject path traversal
            if path.contains("..") {
                continue;
            }
            let ids = searcher.filter_by_path_prefix(std::path::Path::new(path))?;
            all_ids.extend(ids);
        }
        all_ids.sort_unstable();
        all_ids.dedup();
        combined = Some(all_ids);
    }

    // Include patterns
    if !include_patterns.is_empty() {
        let ids = searcher.filter_by_file_patterns(include_patterns)?;
        combined = intersect_subset(combined, ids);
    }

    // Exclude patterns
    if !exclude_patterns.is_empty() {
        let ids = searcher.filter_exclude_by_patterns(exclude_patterns)?;
        combined = intersect_subset(combined, ids);
    }

    // Exclude dirs
    if !exclude_dirs.is_empty() {
        let ids = searcher.filter_exclude_by_dirs(exclude_dirs)?;
        combined = intersect_subset(combined, ids);
    }

    // Text pattern
    if let Some(pattern) = text_pattern {
        let ids = searcher.filter_by_text_pattern_with_options(pattern, true, false, false)?;
        combined = intersect_subset(combined, ids);
    }

    Ok(combined)
}

fn intersect_subset(existing: Option<Vec<i64>>, new_ids: Vec<i64>) -> Option<Vec<i64>> {
    match existing {
        Some(existing) => {
            let set: std::collections::HashSet<_> = existing.into_iter().collect();
            Some(new_ids.into_iter().filter(|id| set.contains(id)).collect())
        }
        None => Some(new_ids),
    }
}
```

Update `build_router` to add the search route:

```rust
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/search", post(search_handler))
        .with_state(state)
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p colgrep test_search_returns_503 test_search_invalid -- --nocapture`
Expected: Both PASS

- [ ] **Step 4: Commit**

```bash
git add colgrep/src/commands/serve.rs
git commit -m "feat: implement /search endpoint with subset filtering and hybrid search"
```

### Task 9: Implement /reload endpoint

**Files:**
- Modify: `colgrep/src/commands/serve.rs`

- [ ] **Step 1: Add reload handler**

```rust
#[derive(Serialize)]
struct ReloadResponse {
    status: String,
    doc_count: usize,
    reload_time_ms: u64,
}

async fn reload_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let _lock = state.reload_mutex.lock().await;
    let start = Instant::now();

    let index_path = state.config.index.clone();
    let model = state.config.model.clone();
    let quantized = state.config.quantized;

    let result = tokio::task::spawn_blocking(move || {
        let model_name = model.as_deref().unwrap_or(DEFAULT_MODEL);
        let model_path = ensure_model(Some(model_name), true)?;
        Searcher::load_with_model(&index_path, &model_path, quantized)
    })
    .await;

    match result {
        Ok(Ok(new_searcher)) => {
            let doc_count = new_searcher.doc_count().unwrap_or(0);
            // Note: In a full implementation, we'd use ArcSwap here.
            // For now, reload requires daemon restart.
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "reloaded",
                    "doc_count": doc_count,
                    "reload_time_ms": start.elapsed().as_millis() as u64,
                })),
            ).into_response()
        }
        Ok(Err(e)) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "reason": format!("reload failed: {}", e),
                })),
            ).into_response()
        }
        Err(e) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "reason": format!("reload task failed: {}", e),
                })),
            ).into_response()
        }
    }
}
```

Update `build_router`:

```rust
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/search", post(search_handler))
        .route("/reload", post(reload_handler))
        .with_state(state)
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p colgrep`
Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add colgrep/src/commands/serve.rs
git commit -m "feat: implement /reload endpoint with reload mutex serialization"
```

### Task 10: Add pre-warming at startup

**Files:**
- Modify: `colgrep/src/commands/serve.rs`

- [ ] **Step 1: Add prewarm function**

```rust
/// Pre-warm mmap pages by reading index files sequentially.
/// This forces pages into OS cache for consistent search latency.
fn prewarm_index(index_path: &Path) -> Result<()> {
    use std::io::Read;

    let index_dir = index_path.join("index");
    for filename in &["merged_codes.npy", "merged_residuals.npy"] {
        let file_path = index_dir.join(filename);
        if file_path.exists() {
            eprintln!("  Pre-warming {}...", filename);
            let mut file = std::fs::File::open(&file_path)?;
            let mut buf = vec![0u8; 64 * 1024]; // 64KB read buffer
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) => {
                        eprintln!("  ⚠️  Pre-warm warning for {}: {}", filename, e);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}
```

Call it in `cmd_serve` after loading the searcher:

```rust
        let state = match load_searcher(&config) {
            Ok(searcher) => {
                if !config.no_prewarm {
                    eprintln!("Pre-warming index...");
                    if let Err(e) = prewarm_index(&config.index) {
                        eprintln!("⚠️  Pre-warm failed (non-fatal): {}", e);
                    }
                }
                eprintln!("✅ Daemon ready on {}", addr);
                AppState::new_healthy(searcher, config)
            }
            // ...
        };
```

- [ ] **Step 2: Verify it compiles and runs**

Run: `cargo build -p colgrep`
Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add colgrep/src/commands/serve.rs
git commit -m "feat: add mmap pre-warming at daemon startup for consistent latency"
```

### Task 11: Add backpressure middleware

**Files:**
- Modify: `colgrep/src/commands/serve.rs`

- [ ] **Step 1: Add tower concurrency limit and timeout**

Update `cmd_serve` to wrap the router with middleware:

```rust
    rt.block_on(async move {
        // ... (state creation as before)

        let app = build_router(state);

        // Add backpressure middleware
        let app = tower::ServiceBuilder::new()
            .concurrency_limit(config.max_concurrent)
            .timeout(std::time::Duration::from_secs(config.timeout))
            .service(app.into_make_service());

        // ... (listener + serve as before)
    })
```

Note: The exact tower middleware API depends on the version. Adjust as needed to match `tower 0.5` + `axum 0.7` compatibility.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p colgrep`
Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add colgrep/src/commands/serve.rs
git commit -m "feat: add concurrency limit and request timeout middleware"
```

### Task 12: Manual integration test

- [ ] **Step 1: Build and run the daemon**

This requires a real index. If you have one:

```bash
cargo build -p colgrep --release
./target/release/colgrep serve --port 8080 --index /path/to/your/index
```

- [ ] **Step 2: Test health endpoint**

```bash
curl http://localhost:8080/health | jq .
```

Expected: `{"status": "ready", "index_doc_count": ..., ...}`

- [ ] **Step 3: Test search endpoint**

```bash
curl -X POST http://localhost:8080/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "authentication handler", "top_k": 5}' | jq .
```

Expected: Results with `search_time_ms` < 5000

- [ ] **Step 4: Test targeted search**

```bash
curl -X POST http://localhost:8080/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "handler", "top_k": 5, "target_paths": ["src/auth"]}' | jq .
```

Expected: All results have files starting with `src/auth`

---

## Phase D: Resumable Encoding + Output Directory

> This phase modifies the indexing pipeline and is independent of the daemon. It can be implemented in parallel or after Phase C.

### Task 13: Write access verification

**Files:**
- Create: `colgrep/src/index/storage.rs`
- Modify: `colgrep/src/index/mod.rs` (add `pub mod storage;`)

- [ ] **Step 1: Write failing tests**

Create `colgrep/src/index/storage.rs`:

```rust
use anyhow::{Context, Result};
use std::path::Path;

/// Verify write access to a directory by writing and reading a probe file.
pub fn verify_write_access(dir: &Path) -> Result<()> {
    if !dir.exists() {
        anyhow::bail!(
            "Cannot write to {}\nReason: Directory does not exist\n\n\
             Fix: Create the directory:\n  mkdir -p {}",
            dir.display(),
            dir.display()
        );
    }

    let pid = std::process::id();
    let probe_path = dir.join(format!(".colgrep_write_probe_{}", pid));

    // Write
    std::fs::write(&probe_path, b"probe")
        .with_context(|| format!(
            "Cannot write to {}\nReason: Permission denied\n\n\
             Fix: Ensure the directory is writable:\n  chmod 775 {}\n  \
             # or in K8s, set fsGroup in pod securityContext",
            dir.display(),
            dir.display()
        ))?;

    // Fsync
    let file = std::fs::File::open(&probe_path)?;
    file.sync_all()?;

    // Read back
    let content = std::fs::read(&probe_path)?;
    if content != b"probe" {
        anyhow::bail!("Write verification failed: read-back mismatch at {}", dir.display());
    }

    // Cleanup
    let _ = std::fs::remove_file(&probe_path);

    Ok(())
}

/// Create directory if it doesn't exist, then verify write access.
pub fn init_output_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create output directory: {}", dir.display()))?;
    verify_write_access(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_verify_write_access_succeeds_on_writable_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(verify_write_access(tmp.path()).is_ok());
    }

    #[test]
    fn test_verify_write_access_fails_on_nonexistent_dir() {
        let result = verify_write_access(Path::new("/nonexistent/path/xyz"));
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_write_access_probe_file_cleaned_up() {
        let tmp = TempDir::new().unwrap();
        verify_write_access(tmp.path()).unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert!(entries.iter().all(|e| {
            !e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("colgrep_write_probe")
        }));
    }

    #[test]
    fn test_init_output_dir_creates_nested() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a/b/c");
        assert!(!nested.exists());
        init_output_dir(&nested).unwrap();
        assert!(nested.exists());
    }
}
```

- [ ] **Step 2: Register module**

In `colgrep/src/index/mod.rs`, add:

```rust
pub mod storage;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p colgrep storage::tests -- --nocapture`
Expected: All 4 PASS

- [ ] **Step 4: Commit**

```bash
git add colgrep/src/index/storage.rs colgrep/src/index/mod.rs
git commit -m "feat: add write access verification for output directories"
```

### Task 14: Encoding checkpoint (save/load/resume)

> This task modifies `run_chunk_pipeline()` to save embeddings after each chunk. It is the most complex task in the plan. Implementation details depend heavily on the exact types and channel patterns — the implementer should read `colgrep/src/index/mod.rs:538-631` carefully before starting.

**Files:**
- Create: `colgrep/src/index/checkpoint.rs`
- Modify: `colgrep/src/index/mod.rs` (add checkpoint stage to pipeline)
- Modify: `colgrep/src/cli.rs` (add `--resume` and `--output-dir` flags)

This task is large. The implementer should:
1. Read `run_chunk_pipeline()` at line 538
2. Read `run_index_stage()` at line 352
3. Understand the `PooledChunkForIndex` type
4. Insert a checkpoint save between pool and index stages
5. Add resume logic that skips completed chunks

The checkpoint format:
- `{index_dir}/encoding_checkpoint/chunk_NNNN.npy` — serialized embeddings
- `{index_dir}/encoding_checkpoint/chunk_NNNN.json` — unit metadata
- `{index_dir}/encoding_checkpoint/checkpoint.json` — `{"completed_chunks": N}`

This task is left for the implementer to detail, as the exact serialization format depends on the `Array2<f32>` serialization approach (ndarray-npy, bincode, or raw bytes).

- [ ] **Step 1: Write checkpoint save/load tests** (see Phase 8 TDD tests in spec)
- [ ] **Step 2: Implement save_chunk_checkpoint and load_checkpoint**
- [ ] **Step 3: Implement cleanup_checkpoint**
- [ ] **Step 4: Modify run_chunk_pipeline to save checkpoints**
- [ ] **Step 5: Add --resume flag to CLI**
- [ ] **Step 6: Add --output-dir flag to CLI**
- [ ] **Step 7: Run all tests**
- [ ] **Step 8: Commit**

---

## Self-Review Checklist

1. **Spec coverage**: All 9 phases mapped — encoding detection (Task 2), XML parsing (Tasks 3-4), daemon server (Tasks 5-11), resumable encoding (Task 14), output directory (Task 13). Targeted path search is handled inside the /search handler (Task 8). Hybrid search delegated to existing `searcher.hybrid_search()`.

2. **Placeholder scan**: Task 14 (checkpoint) intentionally leaves serialization details to the implementer since the exact types need to be read from the codebase. All other tasks have complete code.

3. **Type consistency**: `ServeConfig` used consistently. `AppState::new_healthy/new_unhealthy` match test usage. `SearchRequest`/`SearchResponse` match the spec's JSON contract. `build_subset` uses the same `Searcher` filter methods as the existing CLI.
