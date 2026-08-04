//! C/C++ parser plugin - full-parse mode.
//!
//! Handles `.c`, `.h` (C grammar) and `.cpp`, `.cxx`, `.cc`, `.hpp`, `.hxx`
//! (C++ grammar) files. The plugin selects the appropriate Rust tree-sitter
//! grammar itself; no Python CST serializer or Python grammar package is used.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

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
    "number_literal",
    "true",
    "false",
    "null",
    "nullptr",
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
    let root = node_to_cst(tree.root_node(), source.as_bytes());
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
    use crate::exports::intentumdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

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
