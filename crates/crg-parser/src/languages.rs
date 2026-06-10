/// Detect the tree-sitter language name from a file path's extension.
///
/// Returns `Some(language_name)` where the language name matches what
/// `tree_sitter_language_pack::get_language` expects.  Returns `None` for
/// extensions we don't recognise.
pub fn detect_language(file_path: &str) -> Option<&'static str> {
    // Extract extension from the last path component, lowercased.
    let ext = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())?;

    match ext {
        // Python (also used for Jupyter notebooks — caller handles .ipynb specially)
        "py" | "pyw" | "ipynb" => Some("python"),

        // JavaScript / CommonJS / ES modules
        "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),

        // TypeScript
        "ts" | "mts" | "cts" => Some("typescript"),

        // TSX (React + TypeScript)
        "tsx" => Some("tsx"),

        // Rust
        "rs" => Some("rust"),

        // Go
        "go" => Some("go"),

        // Java
        "java" => Some("java"),

        // Kotlin
        "kt" | "kts" => Some("kotlin"),

        // C#
        "cs" => Some("c_sharp"),

        // C++
        "cpp" | "cc" | "cxx" | "c++" | "hh" | "hpp" => Some("cpp"),

        // C / headers (default to C; .hpp already matched above)
        "c" | "h" => Some("c"),

        // Ruby
        "rb" => Some("ruby"),

        // PHP
        "php" => Some("php"),

        // Swift
        "swift" => Some("swift"),

        // Lua
        "lua" => Some("lua"),

        // Luau (Roblox Lua)
        "luau" => Some("luau"),

        // R
        "r" | "R" => Some("r"),

        // Scala
        "scala" => Some("scala"),

        // Elixir
        "ex" | "exs" => Some("elixir"),

        // Dart
        "dart" => Some("dart"),

        // Vue single-file component
        "vue" => Some("vue"),

        // Svelte
        "svelte" => Some("svelte"),

        // Haskell
        "hs" => Some("haskell"),

        // OCaml
        "ml" | "mli" => Some("ocaml"),

        // Julia
        "jl" => Some("julia"),

        // Perl
        "pl" | "pm" | "t" => Some("perl"),

        // Shell / Bash
        "sh" | "bash" | "zsh" | "ksh" => Some("bash"),

        // Nix
        "nix" => Some("nix"),

        // Solidity
        "sol" => Some("solidity"),

        // GDScript (Godot)
        "gd" => Some("gdscript"),

        // ReScript
        "res" | "resi" => Some("rescript"),

        // Verilog / SystemVerilog
        "v" | "sv" | "svh" | "vh" => Some("verilog"),

        // Objective-C implementation
        "m" => Some("objc"),

        // SQL (generic)
        "sql" => Some("sql"),

        // Terraform / HCL
        "tf" | "hcl" => Some("hcl"),

        // TOML
        "toml" => Some("toml"),

        // JSON
        "json" => Some("json"),

        // YAML
        "yaml" | "yml" => Some("yaml"),

        // Zig
        "zig" => Some("zig"),

        // PowerShell
        "ps1" | "psm1" | "psd1" => Some("powershell"),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_maps_to_python() {
        assert_eq!(detect_language("foo.py"), Some("python"));
    }

    #[test]
    fn ts_maps_to_typescript() {
        assert_eq!(detect_language("bar.ts"), Some("typescript"));
    }

    #[test]
    fn tsx_maps_to_tsx() {
        assert_eq!(detect_language("App.tsx"), Some("tsx"));
    }

    #[test]
    fn rs_maps_to_rust() {
        assert_eq!(detect_language("main.rs"), Some("rust"));
    }

    #[test]
    fn cs_maps_to_c_sharp() {
        assert_eq!(detect_language("Foo.cs"), Some("c_sharp"));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(detect_language("binary.exe"), None);
        assert_eq!(detect_language("no_extension"), None);
    }

    #[test]
    fn ipynb_maps_to_python() {
        assert_eq!(detect_language("notebook.ipynb"), Some("python"));
    }
}
