/// Dispatch tables mapping a language name to the set of tree-sitter node
/// type strings that represent classes, functions, imports, and call sites.
///
/// Each function returns a `&'static [&'static str]` backed by a `match`
/// expression — no `HashMap` allocation needed.

/// Tree-sitter node type strings that represent class/struct/trait
/// declarations in `lang`.
pub fn class_types(lang: &str) -> &'static [&'static str] {
    match lang {
        "python" => &["class_definition"],
        "javascript" => &["class_declaration", "class"],
        "typescript" => &["class_declaration", "class"],
        "tsx" => &["class_declaration", "class"],
        "rust" => &["struct_item", "enum_item", "trait_item", "impl_item"],
        "go" => &["type_declaration"],
        "java" => &["class_declaration", "interface_declaration", "enum_declaration"],
        "kotlin" => &["class_declaration", "object_declaration"],
        "c_sharp" => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "struct_declaration",
        ],
        "cpp" => &["class_specifier", "struct_specifier"],
        "c" => &["struct_specifier", "type_definition"],
        "ruby" => &["class", "module"],
        "php" => &["class_declaration", "interface_declaration", "trait_declaration"],
        "swift" => &[
            "class_declaration",
            "struct_declaration",
            "protocol_declaration",
            "enum_declaration",
        ],
        "lua" => &[],   // table-based OOP — no dedicated class node
        "luau" => &["type_definition"],
        "r" => &[],     // class detection via call patterns, not AST nodes
        "scala" => &[
            "class_definition",
            "object_definition",
            "trait_definition",
            "enum_definition",
        ],
        "elixir" => &[], // defmodule is a `call` node; handled in walk_tree
        "dart" => &["class_definition", "mixin_declaration", "enum_declaration"],
        "solidity" => &[
            "contract_declaration",
            "interface_declaration",
            "library_declaration",
            "struct_declaration",
            "enum_declaration",
        ],
        "haskell" => &["data_declaration", "newtype_declaration", "class_declaration"],
        "julia" => &["struct_definition", "abstract_definition", "module_definition"],
        "zig" => &["container_declaration"],
        "powershell" => &["class_statement"],
        "perl" => &["package_statement", "class_statement"],
        "objc" => &[
            "class_interface",
            "class_implementation",
            "category_interface",
            "protocol_declaration",
        ],
        "verilog" => &["module_declaration", "interface_declaration", "class_declaration"],
        "gdscript" => &["class_definition", "class_name_statement"],
        _ => &[],
    }
}

/// Tree-sitter node type strings that represent function / method definitions
/// in `lang`.
pub fn function_types(lang: &str) -> &'static [&'static str] {
    match lang {
        "python" => &["function_definition"],
        "javascript" => &[
            "function_declaration",
            "arrow_function",
            "method_definition",
            "function_expression",
            "generator_function_declaration",
        ],
        "typescript" => &[
            "function_declaration",
            "arrow_function",
            "method_definition",
            "function_expression",
            "method_signature",
        ],
        "tsx" => &[
            "function_declaration",
            "arrow_function",
            "method_definition",
            "function_expression",
            "method_signature",
        ],
        "rust" => &["function_item"],
        "go" => &["function_declaration", "method_declaration"],
        "java" => &["method_declaration", "constructor_declaration"],
        "kotlin" => &["function_declaration"],
        "c_sharp" => &["method_declaration", "constructor_declaration"],
        "cpp" | "c" => &["function_definition"],
        "ruby" => &["method", "singleton_method"],
        "php" => &["function_definition", "method_declaration"],
        "swift" => &["function_declaration", "initializer_declaration"],
        "lua" => &["function_declaration", "local_function_statement", "function_statement"],
        "luau" => &["function_declaration", "local_function_statement"],
        "r" => &["function_definition"],
        "scala" => &["function_definition", "function_declaration"],
        "elixir" => &[], // def/defp/defmacro are `call` nodes; handled in walk_tree
        "dart" => &["function_signature"],
        "haskell" => &["function", "function_declaration"],
        "julia" => &["function_definition", "macro_definition"],
        "perl" => &["subroutine_declaration_statement", "method_declaration_statement"],
        "bash" => &["function_definition"],
        "objc" => &["method_definition", "function_definition"],
        "zig" => &["fn_proto", "fn_decl"],
        "powershell" => &["function_statement"],
        "verilog" => &["task_declaration", "function_declaration", "always_construct"],
        "gdscript" => &["function_definition"],
        "solidity" => &[
            "function_definition",
            "constructor_definition",
            "modifier_definition",
            "event_definition",
            "fallback_receive_definition",
        ],
        _ => &[],
    }
}

/// Tree-sitter node type strings that represent import / use declarations in
/// `lang`.
pub fn import_types(lang: &str) -> &'static [&'static str] {
    match lang {
        "python" => &["import_statement", "import_from_statement"],
        "javascript" | "typescript" | "tsx" => &["import_statement", "import_declaration"],
        "rust" => &["use_declaration"],
        "go" => &["import_declaration"],
        "java" | "kotlin" => &["import_declaration"],
        "c_sharp" => &["using_directive"],
        "cpp" | "c" => &["preproc_include"],
        "ruby" => &[], // require() is a call — handled generically via call_types
        "php" => &["namespace_use_declaration"],
        "swift" => &["import_declaration"],
        "scala" => &["import_declaration"],
        "elixir" => &[], // alias/import/use/require are `call` nodes
        "dart" => &["import_or_export"],
        "objc" => &["preproc_include"],
        "julia" => &["import_statement", "using_statement"],
        "perl" => &["use_statement", "require_expression"],
        "verilog" => &["package_import_declaration"],
        "gdscript" => &["extends_statement"],
        "solidity" => &["import_directive"],
        _ => &[],
    }
}

/// Tree-sitter node type strings that represent call sites in `lang`.
pub fn call_types(lang: &str) -> &'static [&'static str] {
    match lang {
        "python" => &["call"],
        "javascript" | "typescript" | "tsx" => &["call_expression", "new_expression"],
        "rust" => &["call_expression", "macro_invocation"],
        "go" => &["call_expression"],
        "java" => &["method_invocation", "object_creation_expression"],
        "kotlin" => &["call_expression"],
        "c_sharp" => &["invocation_expression", "object_creation_expression"],
        "cpp" | "c" => &["call_expression"],
        "ruby" => &["call", "method_call"],
        "php" => &[
            "function_call_expression",
            "member_call_expression",
            "scoped_call_expression",
            "nullsafe_member_call_expression",
        ],
        "swift" => &["call_expression", "function_call_expression"],
        "lua" | "luau" => &["function_call"],
        "r" => &["call"],
        "scala" => &["call_expression", "instance_expression", "generic_function"],
        "elixir" => &[], // handled in walk_tree with special filtering
        "dart" => &["function_expression_invocation"],
        "objc" => &["message_expression", "call_expression"],
        "bash" => &["command"],
        "zig" => &["call_expression", "builtin_call_expr"],
        "powershell" => &["command_expression"],
        "julia" => &["call_expression", "broadcast_call_expression", "macrocall_expression"],
        "perl" => &[
            "function_call_expression",
            "method_call_expression",
            "ambiguous_function_call_expression",
        ],
        "verilog" => &[
            "module_instantiation",
            "function_subroutine_call",
            "subroutine_call",
            "system_tf_call",
        ],
        "gdscript" => &["call", "attribute_call"],
        "solidity" => &["call_expression"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_class_types() {
        assert!(class_types("python").contains(&"class_definition"));
    }

    #[test]
    fn rust_function_types() {
        assert!(function_types("rust").contains(&"function_item"));
    }

    #[test]
    fn python_import_types() {
        assert!(import_types("python").contains(&"import_statement"));
        assert!(import_types("python").contains(&"import_from_statement"));
    }

    #[test]
    fn javascript_call_types() {
        assert!(call_types("javascript").contains(&"call_expression"));
    }

    #[test]
    fn unknown_lang_returns_empty() {
        assert!(class_types("cobol").is_empty());
        assert!(function_types("cobol").is_empty());
        assert!(import_types("cobol").is_empty());
        assert!(call_types("cobol").is_empty());
    }
}
