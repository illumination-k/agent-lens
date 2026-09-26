//! Experimental `.astro` support: expose an Astro component's script
//! to the TypeScript analyzers.
//!
//! An Astro component is TypeScript frontmatter between `---` fences,
//! followed by an HTML-like template that can carry `<script>` elements.
//! [tree-sitter-astro-next](https://docs.rs/tree-sitter-astro-next)
//! locates those script regions; [`mask`] keeps them and blanks every
//! other byte, so the result parses as one TypeScript module whose byte
//! offsets — and so line numbers — are the `.astro` file's own.
//!
//! The template is not analyzed: `{expr}` interpolations and
//! `<Component />` usages are blanked like the rest of the markup.

use std::ops::Range;

/// `type` values under which a `<script>` body is still JavaScript.
/// Anything else (`application/ld+json`, `text/partytown`, ...) is data.
const SCRIPT_TYPES: &[&str] = &["module", "text/javascript", "text/typescript"];

/// `source` with every byte outside the frontmatter and the `<script>`
/// bodies replaced by a space, line breaks kept.
///
/// The first blanked byte after each kept region becomes `;`, so
/// automatic semicolon insertion cannot join the last statement of one
/// region with the first statement of the next.
pub(crate) fn mask(source: &str) -> String {
    let regions = script_regions(source);
    let mut out = source.as_bytes().to_vec();
    let mut kept = regions.iter().peekable();
    let mut terminate = false;
    for (i, byte) in out.iter_mut().enumerate() {
        while kept.next_if(|r| r.end <= i).is_some() {
            terminate = true;
        }
        if kept.peek().is_some_and(|r| r.start <= i) {
            continue;
        }
        match *byte {
            b'\n' | b'\r' => {}
            _ if terminate => {
                *byte = b';';
                terminate = false;
            }
            _ => *byte = b' ',
        }
    }
    // Every non-ASCII byte lies either in a kept region, where it is
    // untouched, or outside, where it became ASCII; the result stays
    // valid UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// Byte ranges of the frontmatter body and every JavaScript `<script>`
/// body, in source order.
fn script_regions(source: &str) -> Vec<Range<usize>> {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_astro_next::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut regions = Vec::new();
    collect(tree.root_node(), source, &mut regions);
    regions
}

fn collect(node: tree_sitter::Node<'_>, source: &str, out: &mut Vec<Range<usize>>) {
    match node.kind() {
        "frontmatter_js_block" => out.push(node.byte_range()),
        "script_element" if is_javascript(node, source) => {
            let mut cursor = node.walk();
            out.extend(
                node.named_children(&mut cursor)
                    .filter(|child| child.kind() == "raw_text")
                    .map(|child| child.byte_range()),
            );
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect(child, source, out);
            }
        }
    }
}

/// Whether a `<script>` element's body is JavaScript: it has no `type`
/// attribute, or one naming a JavaScript type.
fn is_javascript(script: tree_sitter::Node<'_>, source: &str) -> bool {
    let mut cursor = script.walk();
    let Some(start_tag) = script
        .named_children(&mut cursor)
        .find(|child| child.kind() == "start_tag")
    else {
        return true;
    };
    let mut cursor = start_tag.walk();
    let type_value = start_tag
        .named_children(&mut cursor)
        .filter(|attr| attr.kind() == "attribute")
        .find(|attr| {
            attr.named_child(0)
                .is_some_and(|name| &source[name.byte_range()] == "type")
        })
        .and_then(|attr| attr.named_child(1))
        .map(|value| source[value.byte_range()].trim_matches(['"', '\'']));
    type_value.is_none_or(|ty| SCRIPT_TYPES.contains(&ty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const COMPONENT: &str = "---
import Layout from '../layouts/Layout.astro';
const title = \"Hello ---\"
---
<Layout title={title}>
  <h1>{title}</h1>
  <script>
    (() => {})()
  </script>
  <script type=\"application/ld+json\">{\"a\": 1}</script>
</Layout>
";

    #[test]
    fn mask_keeps_frontmatter_and_script_bodies_on_their_lines() {
        let masked = mask(COMPONENT);
        assert_eq!(masked.len(), COMPONENT.len());
        let lines: Vec<&str> = masked.lines().collect();
        assert_eq!(lines.len(), COMPONENT.lines().count());
        assert_eq!(lines[1], "import Layout from '../layouts/Layout.astro';");
        assert_eq!(lines[2], "const title = \"Hello ---\"");
        assert_eq!(lines[7], "    (() => {})()");
    }

    #[test]
    fn mask_blanks_markup_and_terminates_each_region() {
        let masked = mask(COMPONENT);
        let lines: Vec<&str> = masked.lines().collect();
        // The closing fence becomes the statement terminator.
        assert_eq!(lines[3].trim(), ";");
        assert!(
            lines[4].trim().is_empty(),
            "template tag kept: {:?}",
            lines[4]
        );
        assert!(!masked.contains("h1"));
        assert!(!masked.contains("ld+json"));
        assert!(!masked.contains("\"a\": 1"));
    }

    #[rstest]
    #[case("<script>let a = 1</script>", true)]
    #[case("<script type=\"module\">let a = 1</script>", true)]
    #[case("<script is:inline>let a = 1</script>", true)]
    #[case("<script type=\"application/json\">{}</script>", false)]
    #[case("<script type='text/partytown'>let a = 1</script>", false)]
    fn script_body_is_kept_only_for_javascript(#[case] source: &str, #[case] kept: bool) {
        assert_eq!(!script_regions(source).is_empty(), kept, "{source}");
    }

    #[test]
    fn mask_blanks_non_ascii_markup_as_valid_utf8() {
        let source = "---\nconst a = \"日本\";\n---\n<p>日本語</p>\n";
        let masked = mask(source);
        assert_eq!(masked.len(), source.len());
        assert!(masked.contains("const a = \"日本\";"));
        assert!(!masked.contains("日本語"));
    }
}
