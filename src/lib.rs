//! C/C++ parser plugin - full-parse mode.
//!
//! Handles `.c`, `.h` (C grammar) and `.cpp`, `.cxx`, `.cc`, `.hpp`, `.hxx`
//! (C++ grammar) files. The plugin selects the appropriate Rust tree-sitter
//! grammar itself; no Python CST serializer or Python grammar package is used.

use intentumdiff_plugin_sdk::{cst::CstNode, tree::SemanticNode};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct CppParser;

const TRIVIA: &[&str] = &["comment", "line_comment", "block_comment", "whitespace"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "translation_unit",
    // Preprocessor
    "preproc_include",
    "preproc_def",
    "preproc_ifdef",
    "preproc_if",
    // Declarations
    "function_definition",
    "declaration",
    "field_declaration",
    // C++ specific
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
    "namespace_definition",
    "template_declaration",
    "template_instantiation",
    "access_specifier",
    // Constructors / destructors
    "function_declarator",
    // Statements
    "expression_statement",
    "compound_statement",
    "return_statement",
    "throw_statement",
    "if_statement",
    "for_statement",
    "for_range_loop",
    "while_statement",
    "do_statement",
    "try_statement",
    "catch_clause",
    "switch_statement",
    "case_statement",
    "break_statement",
    "continue_statement",
    "goto_statement",
    "labeled_statement",
    // Expressions
    "assignment_expression",
    "call_expression",
    "new_expression",
    "delete_expression",
    "lambda_expression",
    // Identifiers / literals
    "identifier",
    "type_identifier",
    "field_identifier",
    "namespace_identifier",
    "string_literal",
    "char_literal",
    "number_literal",
    "true",
    "false",
    "null",
    "nullptr",
    // Declaration qualifiers / specifiers (issue #4). Each is a leaf whose text is
    // the keyword itself, so linkage, cv-qualification and C++ member/override
    // contract edits are carried by an explicit token with its exact span instead
    // of disappearing from the projection.
    "storage_class_specifier",
    "type_qualifier",
    "explicit_function_specifier",
    "virtual",
    "virtual_specifier",
    "ref_qualifier",
    "noexcept",
    // Attributes (C++11)
    "attribute_declaration",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn compact_text_for(node: &CstNode) -> String {
    if let Some(text) = &node.text {
        return text.trim().to_string();
    }

    node.children
        .iter()
        .map(compact_text_for)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentumdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        "function_definition" => {
            // Walk into the declarator to find the function name
            for child in &node.children {
                if child.node_type == "function_declarator" {
                    for inner in &child.children {
                        if inner.node_type == "identifier"
                            || inner.node_type == "field_identifier"
                            || inner.node_type == "qualified_identifier"
                        {
                            return inner.text_or_empty().to_string();
                        }
                    }
                }
                if child.node_type == "identifier" || child.node_type == "field_identifier" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier" => {
            for child in &node.children {
                if child.node_type == "type_identifier" || child.node_type == "identifier" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "namespace_definition" => {
            for child in &node.children {
                if child.node_type == "identifier" || child.node_type == "namespace_identifier" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "preproc_include" => {
            for child in &node.children {
                if child.node_type == "string_literal" || child.node_type == "system_lib_string" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "preproc_def" => {
            let mut parts: Vec<String> = Vec::new();
            for child in &node.children {
                if child.node_type == "identifier" || child.node_type == "preproc_arg" {
                    let text = compact_text_for(child);
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
            }
            if !parts.is_empty() {
                return parts.join(" ");
            }
        }
        _ => {}
    }
    for child in &node.children {
        if child.node_type == "identifier"
            || child.node_type == "type_identifier"
            || child.node_type == "field_identifier"
        {
            return child.text_or_empty().to_string();
        }
    }
    node.node_type.clone()
}

fn is_class_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "class_specifier"
            | "struct_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "namespace_definition"
    )
}

fn is_method_like(node_type: &str) -> bool {
    matches!(node_type, "function_definition" | "lambda_expression")
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    convert_semantic_classed(
        node,
        id_prefix,
        parent_class,
        memo,
        &|_| false,
        &is_semantic,
        &is_class_like,
        &is_method_like,
        &label_for,
    )
}

use intentumdiff_plugin_sdk::ts_convert::{convert_semantic_classed, node_to_cst};

/// Anonymous grammar keywords that carry declaration semantics. tree-sitter-cpp
/// emits `virtual` as an unnamed token, so the shared named-only CST conversion
/// would drop it entirely (issue #4); it is re-inserted as a keyword leaf.
const ANONYMOUS_MODIFIERS: &[&str] = &["virtual"];

fn keyword_leaf(node: tree_sitter::Node<'_>, source: &[u8]) -> CstNode {
    CstNode {
        node_type: node.kind().to_string(),
        named: true,
        text: Some(node.utf8_text(source).unwrap_or("").to_string()),
        start_line: node.start_position().row as u32,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32,
        end_col: node.end_position().column as u32,
        children: Vec::new(),
    }
}

// Walk the shared named-only CST alongside the original tree:
// - C/C++ character prefixes are anonymous grammar tokens. Make the literal an
//   atomic value so shared leaf hashing includes every source byte (including
//   prefixes and escapes), rather than only the named children. Read the
//   original span to bypass the SDK's bounded display-text capture (issue #5).
// - Re-insert semantic anonymous keywords (ANONYMOUS_MODIFIERS) in source order.
fn normalize_cst(node: &mut CstNode, original: tree_sitter::Node<'_>, source: &[u8]) {
    if original.kind() == "char_literal" {
        node.text = Some(original.utf8_text(source).unwrap_or("").to_string());
        node.children.clear();
        return;
    }
    let mut named = std::mem::take(&mut node.children).into_iter();
    let mut children = Vec::new();
    let mut cursor = original.walk();
    for child in original.children(&mut cursor) {
        if child.is_named() {
            if let Some(mut converted) = named.next() {
                normalize_cst(&mut converted, child, source);
                children.push(converted);
            }
        } else if ANONYMOUS_MODIFIERS.contains(&child.kind()) {
            children.push(keyword_leaf(child, source));
        }
    }
    children.extend(named);
    node.children = children;
}

fn is_c_language(language: &str, filename: &str) -> bool {
    let language = language.to_lowercase();
    let filename = filename.to_lowercase();
    language == "c" || filename.ends_with(".c") || filename.ends_with(".h")
}

fn process_impl(source: &str, language: &str, filename: &str) -> String {
    let mut parser = tree_sitter::Parser::new();
    let set_result = if is_c_language(language, filename) {
        let lang = tree_sitter_c::LANGUAGE.into();
        parser.set_language(&lang)
    } else {
        let lang = tree_sitter_cpp::LANGUAGE.into();
        parser.set_language(&lang)
    };
    if set_result.is_err() {
        return r#"{"error":"Failed to load C/C++ grammar"}"#.to_string();
    }

    let tree = match parser.parse(source, None) {
        Some(tree) => tree,
        None => return r#"{"error":"Parse failed"}"#.to_string(),
    };
    let mut root = node_to_cst(tree.root_node(), source.as_bytes());
    normalize_cst(&mut root, tree.root_node(), source.as_bytes());
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for CppParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "cpp".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".cpp")
            || lower.ends_with(".cxx")
            || lower.ends_with(".cc")
            || lower.ends_with(".hpp")
            || lower.ends_with(".hxx")
        {
            "cpp".to_string()
        } else if lower.ends_with(".c") || lower.ends_with(".h") {
            "c".to_string()
        } else {
            String::new()
        }
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(language: String) -> ExamplePair {
        if language.eq_ignore_ascii_case("c") {
            return ExamplePair {
                old: "#include <stdio.h>\n\nvoid greet(const char *name) {\n    printf(\"Hello, %s\\n\", name);\n}\n\nint main(void) {\n    greet(\"World\");\n    return 0;\n}\n".to_string(),
                new: "#include <stdbool.h>\n#include <stdio.h>\n\nvoid greet(const char *name, bool excited) {\n    printf(\"Hello, %s%s\\n\", name, excited ? \"!\" : \"\");\n}\n\nint main(void) {\n    greet(\"World\", true);\n    return 0;\n}\n".to_string(),
            };
        }

        ExamplePair {
            old: "#include <iostream>\n#include <string>\n\nvoid greet(std::string name) {\n    std::cout << \"Hello, \" + name << std::endl;\n}\n\nint main() {\n    greet(\"World\");\n    return 0;\n}\n".to_string(),
            new: "#include <iostream>\n#include <string>\n#include <vector>\n\nvoid greet(const std::string& name) {\n    std::cout << \"Hello, \" << name << \"!\\n\";\n}\n\nvoid greetMany(const std::vector<std::string>& names) {\n    for (const auto& name : names) greet(name);\n}\n\nint main() {\n    greet(\"World\");\n    return 0;\n}\n".to_string(),
        }
    }
    fn process(input: String, language: String, filename: String) -> String {
        process_impl(&input, &language, &filename)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["c".to_string(), "cpp".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(CppParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    fn parse(source: &str, language: &str) -> SemanticNode {
        serde_json::from_str(&process_impl(
            source,
            language,
            if language == "c" {
                "test.c"
            } else {
                "test.cpp"
            },
        ))
        .unwrap()
    }

    fn literals(node: &SemanticNode) -> Vec<&SemanticNode> {
        let mut found = Vec::new();
        if node.node_type == "char_literal" {
            found.push(node);
        }
        for child in &node.children {
            found.extend(literals(child));
        }
        found
    }

    #[test]
    fn character_literals_are_atomic_complete_and_byte_positioned() {
        for language in ["c", "cpp"] {
            for literal in [
                "'a'", r"'\n'", r"'\''", r"'\x41'", "L'a'", "u'a'", "U'a'", "u8'a'", "U'λ'",
            ] {
                let source = format!("// λ\nint f() {{ return {literal}; }}\n");
                let tree = parse(&source, language);
                let found = literals(&tree);
                assert_eq!(found.len(), 1, "{language}: {literal}");
                let node = found[0];
                assert_eq!(node.label, literal);
                assert!(node.children.is_empty());
                assert_eq!(node.position.start_line, 1);
                assert_eq!(node.position.end_line, 1);
                let line = source.lines().nth(1).unwrap();
                assert_eq!(
                    &line[node.position.start_col as usize..node.position.end_col as usize],
                    literal
                );
                assert_eq!(
                    serde_json::to_string(&tree).unwrap(),
                    serde_json::to_string(&parse(&source, language)).unwrap()
                );
            }
        }
    }

    #[test]
    fn character_literal_value_and_prefix_changes_change_hashes() {
        for language in ["c", "cpp"] {
            for (old, new) in [
                ("'a'", "'b'"),
                (r"'\n'", r"'\t'"),
                ("'a'", "L'a'"),
                ("u'a'", "U'a'"),
                ("'a'", "u8'a'"),
                ("U'λ'", "U'μ'"),
            ] {
                let old = parse(&format!("int f() {{ return {old}; }}"), language);
                let new = parse(&format!("int f() {{ return {new}; }}"), language);
                assert_ne!(old.structural_hash, new.structural_hash);
                assert_ne!(
                    literals(&old)[0].structural_hash,
                    literals(&new)[0].structural_hash
                );
            }
        }
    }

    #[test]
    fn character_literal_identity_and_formatting_preserve_hashes() {
        for language in ["c", "cpp"] {
            let old = parse("int f() { return L'λ'; }", language);
            let same = parse("int f() { return L'λ'; }", language);
            let formatted = parse("int f()\n{\n    return L'λ';\n}\n", language);
            assert_eq!(old.structural_hash, same.structural_hash);
            assert_eq!(old.structural_hash, formatted.structural_hash);
            assert_eq!(
                literals(&old)[0].structural_hash,
                literals(&formatted)[0].structural_hash
            );
        }
    }

    #[test]
    fn character_literal_payload_exceeds_sdk_capture_limit() {
        let literal = format!("'{}b'", "a".repeat(4096));
        let other = format!("'{}c'", "a".repeat(4096));
        for language in ["c", "cpp"] {
            let old = parse(&format!("int f() {{ return {literal}; }}"), language);
            let new = parse(&format!("int f() {{ return {other}; }}"), language);
            assert_eq!(literals(&old)[0].label, literal);
            assert_ne!(old.structural_hash, new.structural_hash);
        }
    }

    fn nodes_of<'a>(node: &'a SemanticNode, kind: &str, out: &mut Vec<&'a SemanticNode>) {
        if node.node_type == kind {
            out.push(node);
        }
        for child in &node.children {
            nodes_of(child, kind, out);
        }
    }

    fn find<'a>(node: &'a SemanticNode, kind: &str) -> Vec<&'a SemanticNode> {
        let mut out = Vec::new();
        nodes_of(node, kind, &mut out);
        out
    }

    /// Assert `source` projects exactly one `kind` token labelled `keyword` whose
    /// 0-based row / UTF-8 byte-column span selects `keyword` in the source.
    fn assert_keyword(source: &str, language: &str, kind: &str, keyword: &str) {
        let tree = parse(source, language);
        let found = find(&tree, kind);
        assert_eq!(found.len(), 1, "{language} {source:?}: {kind}");
        let node = found[0];
        assert_eq!(node.label, keyword, "{language} {source:?}");
        assert!(node.children.is_empty());
        let pos = &node.position;
        assert_eq!(pos.start_line, pos.end_line);
        let line = source.lines().nth(pos.start_line as usize).unwrap();
        assert_eq!(
            &line[pos.start_col as usize..pos.end_col as usize],
            keyword,
            "{language} {source:?}"
        );
    }

    // Exact pairs from issue #4.
    const ISSUE_4_PAIRS: &[(&str, &str, &str, &str, &str)] = &[
        (
            "c",
            "static int x=1;\n",
            "int x=1;\n",
            "storage_class_specifier",
            "static",
        ),
        (
            "c",
            "volatile int x=1;\n",
            "int x=1;\n",
            "type_qualifier",
            "volatile",
        ),
        (
            "cpp",
            "const int x=1;\n",
            "int x=1;\n",
            "type_qualifier",
            "const",
        ),
    ];

    #[test]
    fn issue_4_qualifier_tokens_are_projected_with_exact_spans() {
        for (language, old, new, kind, keyword) in ISSUE_4_PAIRS {
            assert_keyword(old, language, kind, keyword);
            let new_tree = parse(new, language);
            assert!(find(&new_tree, kind).is_empty(), "{language} {new:?}");
            let old_tree = parse(old, language);
            assert_ne!(old_tree.structural_hash, new_tree.structural_hash);
            assert_ne!(
                serde_json::to_string(&old_tree.children).unwrap(),
                serde_json::to_string(&new_tree.children)
                    .unwrap()
                    .replace("\"0.0.1", "\"0.0.2"),
                "{language}: projections must differ beyond ids"
            );
            // Declaration identity is unchanged by the qualifier edit.
            assert_eq!(old_tree.children[0].label, new_tree.children[0].label);
        }
    }

    #[test]
    fn issue_4_pairs_hold_in_both_dialects() {
        for (_, old, new, kind, keyword) in ISSUE_4_PAIRS {
            for language in ["c", "cpp"] {
                assert_keyword(old, language, kind, keyword);
                assert_ne!(
                    parse(old, language).structural_hash,
                    parse(new, language).structural_hash
                );
            }
        }
    }

    #[test]
    fn qualifier_identity_comment_and_formatting_controls_preserve_hashes() {
        for (language, old, _, kind, keyword) in ISSUE_4_PAIRS {
            let base = parse(old, language);
            assert_eq!(
                serde_json::to_string(&base).unwrap(),
                serde_json::to_string(&parse(old, language)).unwrap()
            );
            let formatted = format!("  {keyword}   int\n x = 1 ;\n");
            let commented = format!("/* note */ {keyword} int x=1; // trailing\n");
            for variant in [formatted, commented] {
                let other = parse(&variant, language);
                // Comments are trivia: never projected. (The root hash covers the
                // raw CST, so compare the declaration and the qualifier token.)
                assert_eq!(other.children.len(), 1, "{variant:?}");
                assert_eq!(
                    base.children[0].structural_hash, other.children[0].structural_hash,
                    "{variant:?}"
                );
                assert_eq!(
                    find(&base, kind)[0].structural_hash,
                    find(&other, kind)[0].structural_hash
                );
            }
        }
    }

    #[test]
    fn more_storage_and_qualifier_keywords_are_projected() {
        for language in ["c", "cpp"] {
            for (source, kind, keyword) in [
                ("extern int x;\n", "storage_class_specifier", "extern"),
                (
                    "inline int f(void) { return 1; }\n",
                    "storage_class_specifier",
                    "inline",
                ),
                (
                    "int f(void) { register int r = 1; return r; }\n",
                    "storage_class_specifier",
                    "register",
                ),
                ("int * restrict p;\n", "type_qualifier", "restrict"),
                ("void f(const char *s);\n", "type_qualifier", "const"),
            ] {
                assert_keyword(source, language, kind, keyword);
            }
        }
        for (source, kind, keyword) in [
            ("constexpr int x = 1;\n", "type_qualifier", "constexpr"),
            (
                "thread_local int x = 1;\n",
                "storage_class_specifier",
                "thread_local",
            ),
            (
                "struct P { mutable int x; };\n",
                "type_qualifier",
                "mutable",
            ),
        ] {
            assert_keyword(source, "cpp", kind, keyword);
        }
    }

    #[test]
    fn cpp_member_contract_keywords_are_projected_and_change_hashes() {
        for (old, new, kind, keyword) in [
            (
                "struct P { int f() { return 1; } };\n",
                "struct P { int f() const { return 1; } };\n",
                "type_qualifier",
                "const",
            ),
            (
                "struct B { void f(); };\n",
                "struct B { virtual void f(); };\n",
                "virtual",
                "virtual",
            ),
            (
                "struct D : B { void f(); };\n",
                "struct D : B { void f() override; };\n",
                "virtual_specifier",
                "override",
            ),
            (
                "struct P { P(int); };\n",
                "struct P { explicit P(int); };\n",
                "explicit_function_specifier",
                "explicit",
            ),
            (
                "struct P { void f(); };\n",
                "struct P { void f() &&; };\n",
                "ref_qualifier",
                "&&",
            ),
            (
                "void f();\n",
                "void f() noexcept;\n",
                "noexcept",
                "noexcept",
            ),
        ] {
            assert_keyword(new, "cpp", kind, keyword);
            assert!(find(&parse(old, "cpp"), kind).is_empty(), "{old:?}");
            assert_ne!(
                parse(old, "cpp").structural_hash,
                parse(new, "cpp").structural_hash,
                "{new:?}"
            );
        }
    }

    #[test]
    fn virtual_keyword_keeps_member_identity_and_formatting_invariance() {
        let base = parse("struct B { virtual void f(); };\n", "cpp");
        let formatted = parse("struct B {\n    virtual\n    void f();\n};\n", "cpp");
        assert_eq!(base.structural_hash, formatted.structural_hash);
        let class = &base.children[0];
        assert_eq!(class.label, "B");
    }

    #[test]
    fn grammar_id_nonempty() {
        assert!(!CppParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = CppParser::grammar_id();
        let ids = CppParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = CppParser::detect_language("test.cpp".to_string(), "".to_string());
        assert_eq!(r.as_str(), "cpp");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            CppParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("", "cpp", "empty.cpp");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ", "cpp", "whitespace.cpp");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    #[test]
    fn preprocessor_define_label_includes_value() {
        let out = process_impl("#define LIMIT 4\n", "c", "limits.h");
        let tree: SemanticNode =
            serde_json::from_str(&out).expect("valid semantic tree for preprocessor define");

        assert_eq!(tree.children[0].node_type, "preproc_def");
        assert_eq!(tree.children[0].label, "LIMIT 4");
    }

    #[test]
    fn cpp_preprocessor_define_label_includes_value() {
        let out = process_impl("#define LIMIT 4\n", "cpp", "limits.hpp");
        let tree: SemanticNode =
            serde_json::from_str(&out).expect("valid semantic tree for C++ preprocessor define");

        assert_eq!(tree.children[0].node_type, "preproc_def");
        assert_eq!(tree.children[0].label, "LIMIT 4");
    }

    #[test]
    fn c_playground_example_parses_with_c_grammar() {
        let example = CppParser::example("c".to_string());
        assert!(!example.old.contains("std::"));
        assert!(!example.new.contains("std::"));

        let old_out = process_impl(&example.old, "c", "code.c");
        let new_out = process_impl(&example.new, "c", "code.c");

        t::assert_valid_json(&old_out, "process(c playground old)");
        t::assert_valid_json(&new_out, "process(c playground new)");
        let old_tree: SemanticNode =
            serde_json::from_str(&old_out).expect("valid semantic tree for old C example");
        let new_tree: SemanticNode =
            serde_json::from_str(&new_out).expect("valid semantic tree for new C example");

        assert_eq!(old_tree.node_type, "translation_unit");
        assert_eq!(new_tree.node_type, "translation_unit");
    }
}
