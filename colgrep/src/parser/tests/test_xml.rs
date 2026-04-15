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
    assert!(units.len() >= 2, "Expected at least 2 units, got {}: {:?}", units.len(), units.iter().map(|u| &u.signature).collect::<Vec<_>>());
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
    assert!(units.len() >= 2, "Expected at least 2 units, got {}: {:?}", units.len(), units.iter().map(|u| &u.signature).collect::<Vec<_>>());
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
    assert!(units.len() >= 2, "Expected at least 2 units, got {}: {:?}", units.len(), units.iter().map(|u| &u.signature).collect::<Vec<_>>());
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
